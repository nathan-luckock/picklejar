# Features

The complete engine and SQL surface. For the *why* behind each decision, see
[design.md](design.md); for a quick tour, see the [README](../README.md).

## SQL

- **DDL** — `CREATE TABLE` (with `PRIMARY KEY` / `UNIQUE` / `NOT NULL` /
  `DEFAULT` / `SERIAL`, plus `CHECK` and single-column `FOREIGN KEY`
  constraints), `DROP TABLE`, `TRUNCATE TABLE`, `CREATE INDEX`,
  `CREATE VIEW` / `DROP VIEW`, and `CREATE TABLE name AS <query>` (build and
  populate a table from a query result, inferring its columns).
  `CREATE TABLE IF NOT EXISTS` and `DROP TABLE` / `DROP VIEW ... IF EXISTS`
  make a schema script re-runnable.
- **`ALTER TABLE`** — `ADD COLUMN` (existing rows take the column's `DEFAULT`
  or NULL), `DROP COLUMN [IF EXISTS]`, `RENAME COLUMN a TO b`, and
  `RENAME TO new_name`. Add and drop rewrite the table into fresh storage; a
  drop or rename is refused when a `CHECK` or `FOREIGN KEY` constraint names
  the column, so no constraint is left pointing at a stale name.
- **Auto-increment** — a `SERIAL` column fills in the next id (running max plus
  one) when an `INSERT` omits it; the set of serial columns survives a restart
  in a `.seq` sidecar.
- **Constraints** — `CHECK` predicates and `FOREIGN KEY` referential integrity,
  enforced on write (a foreign key is `RESTRICT` on the parent side) and
  persisted across restarts.
- **DML** — `INSERT` (with or without a column list, from `VALUES` or from a
  query: `INSERT INTO t [(cols)] SELECT ...`), `UPDATE`, `DELETE`, each with an
  optional `RETURNING` projection over the affected rows.
- **Bulk CSV** — `COPY table FROM 'file.csv' [HEADER]` loads rows (through the
  normal insert path, so all constraints apply), and `COPY table TO 'file.csv'
  [HEADER]` writes the table out as RFC-4180 CSV (quoting fields with commas or
  quotes, NULL as an empty field).
- **Upserts** — `INSERT ... ON CONFLICT [(cols)] DO NOTHING | DO UPDATE SET ...
  [WHERE ...]`, with `excluded.col` referring to the rejected row's proposed
  value.
- **Queries** — projection and `*`, `WHERE` with SQL three-valued logic,
  `INNER` / `LEFT` / `CROSS JOIN`, `GROUP BY` with `COUNT` / `SUM` / `MIN` /
  `MAX` / `AVG` (and `DISTINCT` aggregates), `HAVING`, `DISTINCT`, `ORDER BY`,
  `LIMIT` / `OFFSET`.
- **Set operations** — `UNION`, `INTERSECT`, and `EXCEPT`, each with optional
  `ALL` (multiset) semantics.
- **Window functions** — `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG` / `LEAD`, and
  the aggregates `OVER (PARTITION BY ... ORDER BY ...)`.
- **Subqueries** — scalar, `IN`, and `EXISTS`, both uncorrelated and
  correlated; derived tables (`FROM (SELECT ...)`); views expand to the same
  machinery.
- **CTEs** — `WITH name AS (query), ... SELECT ...`, inlined as derived tables;
  `WITH RECURSIVE` evaluated to a fixpoint (e.g. a transitive closure).
- **Introspection** — queryable `information_schema.tables` and
  `information_schema.columns` views, so a client can discover the schema.
- **Dump / restore** — the CLI's `\dump [file]` writes a self-contained SQL
  script (tables in foreign-key-safe order with their constraints, then
  explicit indexes, views, and an `INSERT` per table) that recreates the whole
  database when run on an empty one. This is rustdb's `pg_dump`.
- **Temporal types** — `DATE` and `TIMESTAMP` columns, with `DATE '2024-01-15'`
  / `TIMESTAMP '2024-01-15 10:30:00'` typed literals (a bare string is coerced
  into a temporal column). Stored as an epoch offset (days / microseconds) so
  they compare, `ORDER BY`, and index as time, not text. The date math is
  in-tree (no external crate); a column named `date` still works, since the
  type words are not reserved. `EXTRACT(field FROM ts)` / `DATE_PART('field',
  ts)` pull out a component (year, month, day, hour, minute, second, dow, doy)
  and `DATE_TRUNC('field', ts)` floors to the start of one.
- **Casts** — `CAST(expr AS type)` and the `expr::type` shorthand, converting
  between `INT` / `FLOAT` / `BOOL` / `TEXT` / `DATE` / `TIMESTAMP` (text is
  parsed, a float rounds to an int, any value renders to text, a timestamp
  truncates to a date). A cast over a constant folds at insert time.
- **Expressions** — `INT` / `FLOAT` / `BOOL` / `TEXT` / `DATE` / `TIMESTAMP`,
  arithmetic with int-to-float promotion, `IN` / `BETWEEN` / `LIKE` / `IS NULL`,
  `CASE`, string `||`, and a library of scalar functions: string (`LENGTH`,
  `UPPER` / `LOWER`,
  `INITCAP`, `TRIM` / `LTRIM` / `RTRIM`, `SUBSTR`, `RIGHT`, `REPEAT`, `REVERSE`,
  `REPLACE`, `STRPOS` / `POSITION`, `CONCAT`), numeric (`ABS`, `SIGN`, `MOD`,
  `ROUND`, `TRUNC`, `FLOOR`, `CEIL`, `POWER`, `SQRT`, `EXP`, `LN`, `LOG`), and
  conditional (`COALESCE`, `NULLIF`, `GREATEST`, `LEAST`).
- **Transactions** — `BEGIN` / `COMMIT` / `ROLLBACK` over MVCC snapshots;
  auto-commit otherwise.
- **`EXPLAIN`** — the cost-annotated physical plan, showing the planner's scan
  and join choices. `EXPLAIN ANALYZE` also runs the query and appends the
  actual row count and wall-clock time.
- **`ANALYZE [table]`** — scan the live rows and record real per-column
  statistics (distinct count and integer min/max) so the cost model estimates
  selectivity from data instead of defaults; a range bound now uses the
  column's observed `[min, max]` span.
- **`VACUUM [table]`** — compact a table by rewriting only its currently
  visible rows into fresh MVCC storage with rebuilt indexes, reclaiming the
  space held by dead row versions (from updates and deletes) and stale index
  entries. Refused inside a transaction block, since the rewrite would
  invalidate an older snapshot.

## Engine

- **Durability** — write-ahead logging, ARIES crash recovery, and schema plus
  data that survive a restart.
- **Storage** — 8 KiB slotted pages, an LRU-K buffer pool, a B+ tree primary
  index, secondary indexes, and CRC32 page checksums.
- **Concurrency control** — MVCC with snapshot isolation and version chains.
- **Interfaces** — an embeddable library, a `psql`-style CLI, and a
  PostgreSQL-wire-protocol server (simple + extended, with `$N` parameters).
- **Concurrency** — the wire server handles many client connections at once: the
  single-threaded engine runs as an actor on its own thread, and each connection
  gets its own thread and session handle. Transaction exclusivity keeps explicit
  transactions isolated; auto-commit statements from different connections
  interleave freely.

## Correctness and crash safety

The graded requirement is "forced crash, no committed data loss." rustdb proves
it three ways:

1. **In-process recovery tests** — drive a workload, drop the buffer pool
   without flushing (losing dirty pages, exactly as a kill does), recover from
   the WAL, and assert committed rows survive while uncommitted rows roll back.
2. **Forced process kill** — a child process commits rows forever and is
   hard-killed (`SIGKILL` / `TerminateProcess`); recovery must reproduce every
   row recorded as durable.
3. **Deterministic simulation testing (DST)** — every run is one `u64` seed, so
   any failure replays exactly. The data file is a fault-injecting in-memory
   disk that models durability honestly (only `fsync`-ed writes survive a
   crash). Each seed builds a random workload, crashes at a random durable/lost
   split, recovers, and checks an oracle of exactly which rows must survive.

```bash
cargo run --release --bin dst -- 100000      # 100k reproducible crash scenarios
cargo run --bin dst -- --seed 42             # replay one exactly
```

DST is not decoration: it **found a real recovery bug**. An aborted transaction
whose in-memory rollback was lost in the crash could resurrect its row, because
recovery skips undo for a transaction that already logged `Abort`. The fix makes
rollback log compensation records so redo reproduces it. The write-up is in
[design.md](design.md#crash-model-and-the-torture-test).

For query correctness, a separate **differential tester checks rustdb against
SQLite**. Each seed generates random SQL (joins, `GROUP BY` / `HAVING`,
`DISTINCT`, set operations, three-valued NULL logic) in a dialect-shared subset
and runs the identical query through both engines, comparing results as a sorted
multiset. SQLite is the independent oracle, so any divergence is a rustdb bug.

```bash
cargo run --release --bin difftest -- 100000   # 100k queries vs SQLite
cargo run --bin difftest -- --seed 42           # replay one exactly
```

## Crates

| Crate | Responsibility |
|---|---|
| [`storage`](../crates/storage/) | 8 KiB pages, an LRU-K buffer pool, a B+ tree, CRC32 checksums, the `Disk` trait |
| [`wal`](../crates/wal/) | Write-ahead log, ARIES recovery, and the deterministic crash simulator |
| [`txn`](../crates/txn/) | Transactions and MVCC |
| [`sql`](../crates/sql/) | SQL lexer and recursive-descent parser |
| [`planner`](../crates/planner/) | Logical plan, cost model, physical plan, EXPLAIN |
| [`executor`](../crates/executor/) | Volcano operators and the row codec |
| [`rustdb`](../crates/rustdb/) | The embedded engine that wires every layer together |
| [`rustdb-cli`](../crates/rustdb-cli/) | The interactive shell |
| [`rustdb-server`](../crates/rustdb-server/) | The PostgreSQL-wire-protocol server (`rustdb-pg`) and an HTTP/JSON API |
| [`rustdb-difftest`](../crates/rustdb-difftest/) | Differential testing of the engine against SQLite |

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo run --bin rustdb         # the psql-style CLI
cargo run --bin rustdb-pg      # the PostgreSQL-wire-protocol server
cargo run --release --bin dst  # deterministic crash-recovery simulations
```

The project has no external database, SQL-parser, wire-protocol, or checksum
dependencies; the engine and its interfaces are implemented in-tree.
