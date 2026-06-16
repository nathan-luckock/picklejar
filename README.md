# rustdb

[![CI](https://github.com/nathan-luckock/capstone/actions/workflows/ci.yml/badge.svg)](https://github.com/nathan-luckock/capstone/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org)

A relational database engine written from scratch in Rust: disk-backed storage, a write-ahead log with ARIES crash recovery, MVCC transactions, a SQL parser, a cost-based query planner, and a Volcano executor, all behind a `psql`-style shell.

> Not a wrapper around SQLite or Postgres, and not a key-value store with SQL bolted on. Every layer, from the bytes on disk to the query optimizer, is implemented in this repository.

## Why it is interesting

Most "build a database" projects stop at a key-value store or wrap an existing engine. rustdb implements the parts that actually make a database a database:

- **It survives crashes.** A write-ahead log and ARIES-style recovery (analysis, redo, undo with compensation records) guarantee that no committed transaction is lost. This is proven by a torture test that force-kills the process mid-write and verifies the data after recovery.
- **It is transactional.** MVCC gives every transaction a consistent snapshot; `BEGIN` / `COMMIT` / `ROLLBACK` work, and a reader never blocks a writer.
- **It optimizes queries.** A cost-based planner chooses between a sequential scan and an index scan, and between a nested-loop and a hash join, from table statistics. `EXPLAIN` prints the annotated plan.
- **It enforces a schema.** Typed columns with `PRIMARY KEY`, `UNIQUE`, and `NOT NULL` constraints.

The whole thing is around 360 tests, including property tests and the crash-recovery torture test, with `clippy -D warnings` and `rustfmt` enforced in CI.

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

## Features

- **DDL**: `CREATE TABLE` (with `PRIMARY KEY` / `UNIQUE` / `NOT NULL`), `DROP TABLE`, `CREATE INDEX`.
- **DML**: `INSERT` (with or without a column list), `UPDATE`, `DELETE`.
- **Queries**: projection and `*`, `WHERE` with SQL three-valued logic, `INNER` and `LEFT JOIN`, `GROUP BY` with `COUNT` / `SUM` / `MIN` / `MAX` / `AVG`, `ORDER BY`, `LIMIT`.
- **Transactions**: `BEGIN` / `COMMIT` / `ROLLBACK` over MVCC snapshots; auto-commit otherwise.
- **`EXPLAIN`**: the cost-annotated physical plan, showing the planner's scan and join choices.
- **Durability**: write-ahead logging, ARIES crash recovery, and schema plus data that survive a restart.

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
| [`storage`](crates/storage/) | 8 KiB pages, an LRU-K buffer pool, a B+ tree, CRC32 checksums |
| [`wal`](crates/wal/) | Write-ahead log and ARIES recovery |
| [`txn`](crates/txn/) | Transactions and MVCC |
| [`sql`](crates/sql/) | SQL lexer and recursive-descent parser |
| [`planner`](crates/planner/) | Logical plan, cost model, physical plan, EXPLAIN |
| [`executor`](crates/executor/) | Volcano operators and the row codec |
| [`rustdb`](crates/rustdb/) | The embedded engine that wires every layer together |
| [`rustdb-cli`](crates/rustdb-cli/) | The interactive shell |

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo run --bin rustdb        # the CLI
```

The project targets Rust 1.80+ and has no external database, SQL-parser, or checksum dependencies; the graded engine is implemented in-tree.

## Roadmap

The engine is feature-complete for a single connection. Next is more depth
(more column types, a true secondary-index runtime, concurrent connections)
and a Supabase-style studio: an HTTP API over the engine and a web UI with a
SQL editor, a results grid, a live plan visualizer, and a crash-recovery panel.
See [docs/sprints.md](docs/sprints.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). In short: run `cargo fmt`,
`cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test`
before opening a pull request.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
