<div align="center">

# picklejar features

The complete SQL and engine surface.

[Overview](../README.md) &nbsp;·&nbsp; [Design](design.md) &nbsp;·&nbsp; [Build log](sprints.md)

</div>

---

For the reasoning behind each decision, see [design.md](design.md); for a quick
tour, see the [README](../README.md). For where the project is headed, the
durable, isolated memory layer for AI in unreachable environments, see
[Mission and direction](design.md#mission-and-direction).

This page is the *shipped* surface. The AI memory layer is built on it: a native
`VECTOR(n)` type, the four distance operators and their function forms,
brute-force nearest-neighbor search, row-level-security-filtered similarity
(isolation enforced by the engine, not application code), an HNSW index, and a
fault simulator (`vecsim`) that proves durability and isolation together under
simulated crash. The one remaining step is wiring the HNSW index into the planner
so nearest-neighbor queries use it automatically.

## SQL

- **DDL** - `CREATE TABLE` (with `PRIMARY KEY` / `UNIQUE` / `NOT NULL` /
  `DEFAULT` / `SERIAL`, plus `CHECK` and single-column `FOREIGN KEY`
  constraints), `DROP TABLE`, `TRUNCATE TABLE`, `CREATE INDEX`,
  `CREATE VIEW` / `DROP VIEW`, and `CREATE TABLE name AS <query>` (build and
  populate a table from a query result, inferring its columns).
  `CREATE TABLE IF NOT EXISTS` and `DROP TABLE` / `DROP VIEW ... IF EXISTS`
  make a schema script re-runnable.
- **`ALTER TABLE`** - `ADD COLUMN` (existing rows take the column's `DEFAULT`
  or NULL), `DROP COLUMN [IF EXISTS]`, `RENAME COLUMN a TO b`, and
  `RENAME TO new_name`. Add and drop rewrite the table into fresh storage; a
  drop or rename is refused when a `CHECK` or `FOREIGN KEY` constraint names
  the column, so no constraint is left pointing at a stale name.
- **Auto-increment** - a `SERIAL` column fills in the next id (running max plus
  one) when an `INSERT` omits it; the set of serial columns survives a restart
  in a `.seq` sidecar.
- **Constraints** - `CHECK` predicates and `FOREIGN KEY` referential integrity,
  enforced on write and persisted across restarts. A foreign key takes the
  usual referential actions, `ON DELETE` / `ON UPDATE` `NO ACTION` / `RESTRICT`
  / `CASCADE` / `SET NULL`, and cascades run through the normal delete/update
  path so they recurse and stay transactional.
- **DML** - `INSERT` (with or without a column list, from `VALUES` or from a
  query: `INSERT INTO t [(cols)] SELECT ...`), `UPDATE`, `DELETE`, each with an
  optional `RETURNING` projection over the affected rows.
- **Bulk CSV** - `COPY table FROM 'file.csv' [HEADER]` loads rows (through the
  normal insert path, so all constraints apply), and `COPY table TO 'file.csv'
  [HEADER]` writes the table out as RFC-4180 CSV (quoting fields with commas or
  quotes, NULL as an empty field).
- **Upserts** - `INSERT ... ON CONFLICT [(cols)] DO NOTHING | DO UPDATE SET ...
  [WHERE ...]`, with `excluded.col` referring to the rejected row's proposed
  value.
- **Queries** - projection and `*`, `WHERE` with SQL three-valued logic,
  `INNER` / `LEFT` / `RIGHT` / `FULL` / `CROSS JOIN` (the `OUTER` keyword
  optional), `NATURAL` joins and `JOIN ... USING (cols)` (each resolved to the
  equivalent `ON` predicate over the shared columns), `GROUP BY` with `COUNT` / `SUM` / `MIN` /
  `MAX` / `AVG` (and `DISTINCT` aggregates), `HAVING`, `DISTINCT`, `ORDER BY`
  (with `ASC` / `DESC` and `NULLS FIRST` / `NULLS LAST`), `LIMIT` / `OFFSET`.
- **Set operations** - `UNION`, `INTERSECT`, and `EXCEPT`, each with optional
  `ALL` (multiset) semantics.
- **Window functions** - `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG` / `LEAD`, and
  the aggregates `OVER (PARTITION BY ... ORDER BY ...)`.
- **Subqueries** - scalar, `IN`, and `EXISTS`, both uncorrelated and
  correlated; derived tables (`FROM (SELECT ...)`); views expand to the same
  machinery.
- **CTEs** - `WITH name AS (query), ... SELECT ...`, inlined as derived tables;
  `WITH RECURSIVE` evaluated to a fixpoint (e.g. a transitive closure).
- **Introspection** - queryable `information_schema.tables` and
  `information_schema.columns` views, so a client can discover the schema.
- **Dump / restore** - the CLI's `\dump [file]` writes a self-contained SQL
  script (tables in foreign-key-safe order with their constraints, then
  explicit indexes, views, and an `INSERT` per table) that recreates the whole
  database when run on an empty one. This is picklejar's `pg_dump`.
- **Temporal types** - `DATE` and `TIMESTAMP` columns, with `DATE '2024-01-15'`
  / `TIMESTAMP '2024-01-15 10:30:00'` typed literals (a bare string is coerced
  into a temporal column). Stored as an epoch offset (days / microseconds) so
  they compare, `ORDER BY`, and index as time, not text. The date math is
  in-tree (no external crate); a column named `date` still works, since the
  type words are not reserved. `EXTRACT(field FROM ts)` / `DATE_PART('field',
  ts)` pull out a component (year, month, day, hour, minute, second, dow, doy)
  and `DATE_TRUNC('field', ts)` floors to the start of one.
- **JSON** - a `JSON` column (validated on write, stored as text) with the
  `->` (returns JSON) and `->>` (returns text) access operators, navigating by
  a text member name or an integer array index, and chainable
  (`body -> 'a' ->> 0`). The JSON parser is in-tree (no external crate).
- **Vector** - a `VECTOR(n)` / `EMBEDDING(n)` column for AI embeddings, with
  pgvector-style literals (`VECTOR '[0.1, 0.2, 0.9]'`, or a bare `'[...]'` string
  coerced into the column). Stored as native `f32` components (a `u32` count then
  the little-endian floats), so an embedding round-trips a crash and reopen like
  any other value. The optional dimension declares the embedding width and is
  enforced on every write; a bare `VECTOR` is width-agnostic. Distance operators
  `<->` (L2), `<=>` (cosine), `<#>` (negative inner product), and `<+>` (L1 /
  Manhattan) evaluate to a FLOAT, so `ORDER BY embedding <-> :query LIMIT k` is
  brute-force nearest-neighbor search and `WHERE embedding <-> :query < r` is a
  radius filter; the same distances are available as the functions `l2_distance`,
  `l1_distance`, `cosine_distance`, and `inner_product`, alongside `vector_dims`
  and `l2_norm`. This is the storage and query foundation of the AI memory layer.
- **Decimal** - a `DECIMAL` / `NUMERIC` column with exact base-10 arithmetic
  (`0.1 + 0.2` is `0.3`, not a binary-float approximation), `DECIMAL '12.34'`
  literals, and exact `SUM` / `AVG`. Stored as an `i128` mantissa plus a scale,
  so it compares and `ORDER BY`s as a number; the `(precision, scale)` in a
  type is accepted and each value keeps its own scale. The arithmetic is
  in-tree (no external bignum).
- **Casts** - `CAST(expr AS type)` and the `expr::type` shorthand, converting
  between `INT` / `FLOAT` / `BOOL` / `TEXT` / `DATE` / `TIMESTAMP` / `JSON`
  (text is parsed, a float rounds to an int, any value renders to text, a
  timestamp truncates to a date, text validates as JSON, text parses to a
  `VECTOR`). A cast over a constant folds at insert time.
- **Expressions** - `INT` / `FLOAT` / `BOOL` / `TEXT` / `DATE` / `TIMESTAMP`,
  arithmetic with int-to-float promotion, `IN` / `BETWEEN` / `LIKE` / `IS NULL`,
  `CASE`, string `||`, and a library of scalar functions: string (`LENGTH`,
  `UPPER` / `LOWER`,
  `INITCAP`, `TRIM` / `LTRIM` / `RTRIM`, `SUBSTR`, `RIGHT`, `REPEAT`, `REVERSE`,
  `REPLACE`, `STRPOS` / `POSITION`, `CONCAT`), numeric (`ABS`, `SIGN`, `MOD`,
  `ROUND`, `TRUNC`, `FLOOR`, `CEIL`, `POWER`, `SQRT`, `EXP`, `LN`, `LOG`), and
  conditional (`COALESCE`, `NULLIF`, `GREATEST`, `LEAST`).
- **Transactions** - `BEGIN` / `COMMIT` / `ROLLBACK` over MVCC snapshots;
  auto-commit otherwise.
- **`EXPLAIN`** - the cost-annotated physical plan, showing the planner's scan
  and join choices. `EXPLAIN ANALYZE` also runs the query and appends the
  actual row count and wall-clock time.
- **`ANALYZE [table]`** - scan the live rows and record real per-column
  statistics (distinct count and integer min/max) so the cost model estimates
  selectivity from data instead of defaults; a range bound now uses the
  column's observed `[min, max]` span.
- **`VACUUM [table]`** - compact a table by rewriting only its currently
  visible rows into fresh MVCC storage with rebuilt indexes, reclaiming the
  space held by dead row versions (from updates and deletes) and stale index
  entries. Refused inside a transaction block, since the rewrite would
  invalidate an older snapshot.

## Engine

- **Durability** - write-ahead logging, ARIES crash recovery, and schema plus
  data that survive a restart.
- **Storage** - 8 KiB slotted pages, an LRU-K buffer pool, a B+ tree primary
  index, secondary indexes, and CRC32 page checksums.
- **Indexes** - a unique column of an order-preserving fixed type (`INT`,
  `DATE`, `TIMESTAMP`, `BOOL`) gets a physical secondary B+ tree automatically,
  and `CREATE [UNIQUE] INDEX name ON t (col, ...)` builds one over any indexable
  column(s), including `TEXT`, **non-unique**, and **composite** (multi-column)
  keys, through a second, variable-length-key B+ tree. The key is the column
  values encoded order-preservingly plus the row id, so repeated values produce
  distinct keys and a value lookup is a prefix range scan that returns every
  matching row; equality on a leading subset of a composite index works the same
  way. `UNIQUE` rejects a write (or an index build) that would duplicate the
  indexed value tuple (`NULL`s never conflict). Because the key map is order-preserving, the
  planner drives both a point get (`col = x`) and a range scan (`col > x`,
  `col BETWEEN a AND b`) off either index, and the cost model chooses it whenever
  it beats a full scan. Every index hit is a candidate the executor re-checks
  against the predicate, so a stale or out-of-snapshot entry is filtered, never
  returned. (`FLOAT` / `DECIMAL` / `VECTOR` are not keyed by a B+ tree; such a column
  falls back to a sequential scan. A vector has no meaningful total order and is
  searched by distance, so it gets an approximate-nearest-neighbor index of its
  own instead: an in-memory, seeded HNSW graph (`crates/picklejar/src/hnsw.rs`)
  with build and top-k search, recall measured against the brute-force baseline.
  The structure is in place; wiring it into the planner so `ORDER BY col <-> q
  LIMIT k` uses it, with the row-level-security filter applied before the top-k,
  is the next step.)
- **Concurrency control** - MVCC with snapshot isolation and version chains.
- **Interfaces** - an embeddable library, a `psql`-style CLI, and a
  PostgreSQL-wire-protocol server (simple + extended, with `$N` parameters).
- **Authentication** - the wire server defaults to trust (any user) but accepts
  `--user` / `--password` to require SCRAM-SHA-256 (the modern PostgreSQL
  mechanism): the password is never sent or stored, only a salted one-way
  verifier, and the client proves knowledge of it through a challenge-response
  exchange. The SHA-256, HMAC, and PBKDF2 primitives are in-tree (no external
  crypto crate).
- **Roles and privileges** - `CREATE ROLE` / `CREATE USER` (with `SUPERUSER`,
  `LOGIN`, `CREATEROLE`, `BYPASSRLS`, `PASSWORD` attributes), `ALTER ROLE`,
  `DROP ROLE`, role membership (`GRANT role TO role`), and table privileges
  (`GRANT` / `REVOKE` `SELECT` / `INSERT` / `UPDATE` / `DELETE` / `TRUNCATE` /
  `ALL` `ON t TO role | PUBLIC`). Every statement is authorized against the
  session's current role: a superuser bypasses all checks, the table owner (the
  role that created it) holds every privilege, and other roles need a grant made
  directly, to `PUBLIC`, or to a group they belong to. `SET ROLE` / `RESET ROLE`
  switch the active role; `current_user` / `current_role` / `session_user`
  report it. A fresh database has a single bootstrap superuser and the default
  session runs as it, so an unconfigured database stays fully open; enforcement
  begins once roles exist and a session runs as a non-superuser. The wire server
  runs each connection as the role it authenticated. Roles, grants, memberships,
  and ownership persist across a restart.
- **Row-level security** - `ALTER TABLE t ENABLE` / `DISABLE` / `FORCE` /
  `NO FORCE ROW LEVEL SECURITY` and `CREATE POLICY name ON t [FOR cmd]
  [TO roles] [USING (expr)] [WITH CHECK (expr)]` / `DROP POLICY`. When RLS is on
  and the role is not exempt (superuser / `BYPASSRLS`, or the owner unless the
  table forces RLS), the engine folds the applicable policies' `USING`
  predicates into every read (`SELECT`, and the affected-row scope of `UPDATE` /
  `DELETE`) and enforces their `WITH CHECK` on every written row, so a policy
  like `USING (owner = current_user)` gives per-tenant isolation. With RLS
  enabled and no matching policy the default is deny. Policies persist across a
  restart.
- **Concurrency** - the wire server handles many client connections at once: the
  single-threaded engine runs as an actor on its own thread, and each connection
  gets its own thread and session handle. Transaction exclusivity keeps explicit
  transactions isolated; auto-commit statements from different connections
  interleave freely.

## Correctness and crash safety

The graded requirement is "forced crash, no committed data loss." picklejar proves
it three ways:

1. **In-process recovery tests** - drive a workload, drop the buffer pool
   without flushing (losing dirty pages, exactly as a kill does), recover from
   the WAL, and assert committed rows survive while uncommitted rows roll back.
2. **Forced process kill** - a child process commits rows forever and is
   hard-killed (`SIGKILL` / `TerminateProcess`); recovery must reproduce every
   row recorded as durable.
3. **Deterministic simulation testing (DST)** - every run is one `u64` seed, so
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

For query correctness, a separate **differential tester checks picklejar against
SQLite**. Each seed generates random SQL (joins, `GROUP BY` / `HAVING`,
`DISTINCT`, set operations, three-valued NULL logic) in a dialect-shared subset
and runs the identical query through both engines, comparing results as a sorted
multiset. SQLite is the independent oracle, so any divergence is a picklejar bug.

```bash
cargo run --release --bin difftest -- 100000   # 100k queries vs SQLite
cargo run --bin difftest -- --seed 42           # replay one exactly
```

For the AI memory layer, a third simulator proves durability and isolation
together. The **`vecsim`** binary runs the real engine through a random
multi-tenant embedding workload (some transactions committed, a quarter rolled
back), crashes by dropping and reopening the engine (WAL recovery), and checks an
oracle that every committed embedding survives byte-for-byte *and* that each
tenant, after recovery, sees exactly its own embeddings and never another's, on
both ordinary reads and nearest-neighbor ranking. It sits one level above the
storage `dst` simulator: `dst` proves row durability against a strict fault disk,
`vecsim` proves embedding durability plus engine-enforced tenant isolation through
the full SQL / RLS / MVCC stack. Every run is one seed, so any failure replays
exactly.

```bash
cargo run --release --bin vecsim -- 100000     # 100k durability+isolation sims
cargo run --bin vecsim -- --seed 42             # replay one exactly
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
| [`picklejar`](../crates/picklejar/) | The embedded engine that wires every layer together |
| [`picklejar-cli`](../crates/picklejar-cli/) | The interactive shell |
| [`picklejar-server`](../crates/picklejar-server/) | The PostgreSQL-wire-protocol server (`picklejar-pg`) and an HTTP/JSON API |
| [`picklejar-difftest`](../crates/picklejar-difftest/) | Differential testing of the engine against SQLite |

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo run --bin picklejar         # the psql-style CLI
cargo run --bin picklejar-pg      # the PostgreSQL-wire-protocol server
cargo run --release --bin dst  # deterministic crash-recovery simulations
```

The project has no external database, SQL-parser, wire-protocol, or checksum
dependencies; the engine and its interfaces are implemented in-tree.
