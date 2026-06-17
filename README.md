# rustdb

[![CI](https://github.com/nathan-luckock/capstone/actions/workflows/ci.yml/badge.svg)](https://github.com/nathan-luckock/capstone/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org)

A relational database engine written from scratch in Rust: disk-backed storage, a write-ahead log with ARIES crash recovery, MVCC transactions, a SQL parser, a cost-based query planner, and a Volcano executor. It runs as an embeddable library, a `psql`-style shell, and a **server that speaks the real PostgreSQL wire protocol** so `psql` and standard drivers connect to it directly.

> Not a wrapper around SQLite or Postgres, and not a key-value store with SQL bolted on. Every layer, from the bytes on disk to the query optimizer to the wire protocol, is implemented in this repository.

## Why it is interesting

Most "build a database" projects stop at a key-value store or wrap an existing engine. rustdb implements the parts that actually make a database a database:

- **Real PostgreSQL clients connect to it.** It implements the PostgreSQL v3 wire protocol, so the actual `psql` client (and GUI tools and drivers) talk to the from-scratch engine over TCP, with no shim. See [Connect with psql](#connect-with-psql).
- **It survives crashes, and that is proven reproducibly.** A write-ahead log and ARIES recovery (analysis, redo, undo with compensation records) guarantee no committed transaction is lost. Beyond a force-kill torture test, a **deterministic simulation tester** explores thousands of seeded, reproducible crash scenarios against a fault-injecting disk. It found and fixed a real recovery bug. See [Correctness and crash safety](#correctness-and-crash-safety).
- **It is transactional.** MVCC gives every transaction a consistent snapshot; `BEGIN` / `COMMIT` / `ROLLBACK` work, and a reader never blocks a writer.
- **It optimizes queries.** A cost-based planner chooses between a sequential scan and an index scan, and between a nested-loop and a hash join, from table statistics. `EXPLAIN` prints the annotated plan.
- **It speaks a deep slice of SQL.** Joins, aggregates with `GROUP BY` / `HAVING`, `DISTINCT`, `UNION`, subqueries (scalar, `IN`, `EXISTS`, both uncorrelated and **correlated**), **derived tables**, and **views**, over typed columns with `PRIMARY KEY` / `UNIQUE` / `NOT NULL` / `DEFAULT` / `SERIAL`.

Correctness is enforced by hundreds of unit tests, parser property tests, a forced-kill crash torture test, and thousands of deterministic crash-simulation seeds, all under `clippy -D warnings` and `rustfmt` in CI.

## Quickstart

```bash
cargo run --bin rustdb -- --database mydb.db
```

```sql
rustdb> CREATE TABLE customers (id INT PRIMARY KEY, name TEXT NOT NULL);
OK
rustdb> CREATE TABLE orders (id INT, cid INT, total INT);
OK
rustdb> INSERT INTO customers VALUES (1, 'alice'), (2, 'bob');
2 rows
rustdb> INSERT INTO orders VALUES (10, 1, 50), (11, 1, 30), (12, 2, 90);
3 rows

rustdb> SELECT c.name, SUM(o.total)
   ...> FROM orders AS o INNER JOIN customers AS c ON o.cid = c.id
   ...> GROUP BY c.name;
name  | SUM(o.total)
------+-------------
alice | 80
bob   | 90
(2 rows)

rustdb> BEGIN;
BEGIN
rustdb> DELETE FROM orders WHERE total < 40;
1 row
rustdb> ROLLBACK;          -- the delete is undone
ROLLBACK

rustdb> EXPLAIN SELECT name FROM customers WHERE id = 1;
Project name  (rows=1 cost=...)
  SeqScan customers  (rows=1 cost=...)
    predicate: (id = 1)

rustdb> \q
```

Reopen the same file and the schema and rows are still there.

## Connect with psql

rustdb ships a server that speaks the PostgreSQL v3 wire protocol, so the real `psql` client connects to the from-scratch engine:

```bash
cargo run --release --bin rustdb-pg -- --database mydb.db --port 5433
psql -h 127.0.0.1 -p 5433 -U postgres
```

```text
psql (18.0)
postgres=> CREATE TABLE engineers (id INT, name TEXT, rust_years FLOAT, active BOOL);
CREATE TABLE
postgres=> INSERT INTO engineers VALUES (1,'Nathan',3.5,TRUE),(2,'Ada',7.0,TRUE),(3,'Linus',1.0,FALSE);
INSERT 0 3
postgres=> SELECT name, rust_years FROM engineers WHERE active = TRUE ORDER BY rust_years DESC;
  name  | rust_years
--------+------------
 Ada    |          7
 Nathan |        3.5
(2 rows)

postgres=> SELECT name FROM engineers AS e
postgres->  WHERE rust_years > (SELECT AVG(rust_years) FROM engineers WHERE active = e.active);
 name
------
 Ada
(1 row)
```

The correlated subquery, the aggregate, and `EXPLAIN` all run through the engine and render as ordinary psql tables. Both the simple and the **extended query protocol** (parse/bind/execute with `$N` parameters) are implemented, so server-side prepared statements and the drivers that use them work. Verified against `psql` 18's `\bind`:

```text
postgres=> SELECT name, age FROM users WHERE age > $1 ORDER BY age \bind 28 \g
 name  | age
-------+-----
 alice |  30
 carol |  40
(2 rows)
```

## Correctness and crash safety

The graded requirement is "forced crash, no committed data loss." rustdb proves it three ways:

1. **In-process recovery tests**: drive a workload, drop the buffer pool without flushing (losing dirty pages, exactly as a kill does), recover from the WAL, and assert committed rows survive while uncommitted rows roll back.
2. **Forced process kill**: a child process commits rows forever and is hard-killed (`SIGKILL` / `TerminateProcess`); recovery must reproduce every row recorded as durable.
3. **Deterministic simulation testing (DST)**: every run is one `u64` seed, so any failure replays exactly. The data file is a fault-injecting in-memory disk that models durability honestly (only `fsync`-ed writes survive a crash, unlike a real test where the OS page cache hides un-fsynced writes). Each seed builds a random workload, crashes at a random durable/lost split, recovers, and checks an oracle of exactly which rows must survive.

```bash
cargo run --release --bin dst -- 100000      # 100k reproducible crash scenarios
cargo run --bin dst -- --seed 42             # replay one exactly
```

DST is not decoration: it **found a real recovery bug**. An aborted transaction whose in-memory rollback was lost in the crash could resurrect its row, because recovery skips undo for a transaction that already logged `Abort`. The fix makes rollback log compensation records so redo reproduces it. A 5000-seed sweep now verifies 206,007 committed rows recover correctly. The write-up is in [docs/design.md](docs/design.md#crash-model-and-the-torture-test).

For query correctness, a separate **differential tester checks rustdb against SQLite**. Each seed generates random SQL (joins, `GROUP BY` / `HAVING`, `DISTINCT`, three-valued NULL logic) in a dialect-shared subset and runs the identical query through both engines, comparing results as a sorted multiset. SQLite is the independent oracle, so any divergence is a rustdb bug.

```bash
cargo run --release --bin difftest -- 100000   # 100k queries vs SQLite
cargo run --bin difftest -- --seed 42           # replay one exactly
```

## Features

- **DDL**: `CREATE TABLE` (with `PRIMARY KEY` / `UNIQUE` / `NOT NULL` / `DEFAULT` / `SERIAL`, plus `CHECK` and single-column `FOREIGN KEY` constraints), `DROP TABLE`, `TRUNCATE TABLE`, `ALTER TABLE ... ADD COLUMN`, `CREATE INDEX`, and `CREATE VIEW` / `DROP VIEW`.
- **Auto-increment**: a `SERIAL` column fills in the next id (running max plus one) when an `INSERT` omits it; the set of serial columns survives a restart in a `.seq` sidecar.
- **Constraints**: `CHECK` predicates and `FOREIGN KEY` referential integrity, enforced on write (a foreign key is `RESTRICT` on the parent side) and persisted across restarts.
- **DML**: `INSERT` (with or without a column list), `UPDATE`, `DELETE`, each with an optional `RETURNING` projection over the affected rows.
- **Upserts**: `INSERT ... ON CONFLICT [(cols)] DO NOTHING | DO UPDATE SET ... [WHERE ...]`, with `excluded.col` referring to the rejected row's proposed value.
- **Queries**: projection and `*`, `WHERE` with SQL three-valued logic, `INNER` / `LEFT` / `CROSS JOIN`, `GROUP BY` with `COUNT` / `SUM` / `MIN` / `MAX` / `AVG` (and `DISTINCT` aggregates), `HAVING`, `DISTINCT`, `ORDER BY`, `LIMIT` / `OFFSET`.
- **Set operations**: `UNION`, `INTERSECT`, and `EXCEPT`, each with optional `ALL` (multiset) semantics.
- **Window functions**: `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG` / `LEAD`, and aggregates `OVER (PARTITION BY ... ORDER BY ...)`.
- **Subqueries**: scalar, `IN`, and `EXISTS`, both uncorrelated and correlated; derived tables (`FROM (SELECT ...)`); views expand to the same machinery.
- **CTEs**: `WITH name AS (query), ... SELECT ...`, inlined as derived tables (non-recursive).
- **Expressions**: `INT` / `FLOAT` / `BOOL` / `TEXT`, arithmetic with int-to-float promotion, `IN` / `BETWEEN` / `LIKE` / `IS NULL`, `CASE`, string `||`, and a library of scalar functions.
- **Transactions**: `BEGIN` / `COMMIT` / `ROLLBACK` over MVCC snapshots; auto-commit otherwise.
- **`EXPLAIN`**: the cost-annotated physical plan, showing the planner's scan and join choices.
- **Durability**: write-ahead logging, ARIES crash recovery, and schema plus data that survive a restart.
- **Interfaces**: an embeddable library, a `psql`-style CLI, and a PostgreSQL-wire-protocol server.

## Architecture

```
    +--------------------------+
    |    rustdb-cli (REPL)     |
    +--------------------------+
                 |  Database::execute(sql)
                 v
    +--------------------------+      sql      ->  parser, AST
    |    executor (Volcano)    |      planner  ->  logical/physical plan, cost model, EXPLAIN
    +--------------------------+      executor ->  Volcano operators, expression eval
                 | reads/writes
                 v
    +--------------------------+      txn      ->  transactions, MVCC, versions
    |    txn manager + MVCC    |      wal      ->  write-ahead log, ARIES recovery
    +--------------------------+      storage  ->  pages, buffer pool, B+ tree
                 |
                 v
    +--------------------------+
    |  buffer pool + storage   |   <- data file and write-ahead log on disk
    +--------------------------+
```

Every design decision, with the alternatives considered and rejected, is written up in [docs/design.md](docs/design.md).

| Crate | Responsibility |
|---|---|
| [`storage`](crates/storage/) | 8 KiB pages, an LRU-K buffer pool, a B+ tree, CRC32 checksums, the `Disk` trait |
| [`wal`](crates/wal/) | Write-ahead log, ARIES recovery, and the deterministic crash simulator |
| [`txn`](crates/txn/) | Transactions and MVCC |
| [`sql`](crates/sql/) | SQL lexer and recursive-descent parser |
| [`planner`](crates/planner/) | Logical plan, cost model, physical plan, EXPLAIN |
| [`executor`](crates/executor/) | Volcano operators and the row codec |
| [`rustdb`](crates/rustdb/) | The embedded engine that wires every layer together |
| [`rustdb-cli`](crates/rustdb-cli/) | The interactive shell |
| [`rustdb-server`](crates/rustdb-server/) | The PostgreSQL-wire-protocol server (`rustdb-pg`) and an HTTP/JSON API |
| [`rustdb-difftest`](crates/rustdb-difftest/) | Differential testing of the engine against SQLite |

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo run --bin rustdb         # the psql-style CLI
cargo run --bin rustdb-pg      # the PostgreSQL-wire-protocol server
cargo run --release --bin dst  # deterministic crash-recovery simulations
```

The project targets Rust 1.80+ and has no external database, SQL-parser, wire-protocol, or checksum dependencies; the graded engine and its interfaces are implemented in-tree.

## Roadmap

Done: the storage / WAL+ARIES / MVCC / planner / executor core, a deep SQL
surface, `CHECK` and `FOREIGN KEY` constraints, the PostgreSQL wire protocol
(simple and extended, with parameters), deterministic simulation testing, and
differential testing against SQLite. Next: more column types
(`DATE` / `TIMESTAMP` / `DECIMAL`) and concurrent connections (the engine is
single-threaded today). See [docs/sprints.md](docs/sprints.md) and
[docs/design.md](docs/design.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). In short: run `cargo fmt`,
`cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test`
before opening a pull request.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
