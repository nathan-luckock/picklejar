# rustdb - a relational database engine (Rust)

> CSE 499 senior project. A real disk-based relational database engine with ACID guarantees, in Rust.

Not a SQLite or Postgres wrapper, and not a key-value store with SQL bolted on. A real engine, built layer by layer: a page manager and buffer pool, B+ tree indexes, a write-ahead log with ARIES-style crash recovery, MVCC for snapshot isolation, a SQL parser, a cost-based query planner with EXPLAIN, and a Volcano executor, all behind a `psql`-style shell.

## Quickstart

```bash
cargo run --bin rustdb -- --database mydb.db
```

```sql
rustdb> CREATE TABLE customers (id INT, name TEXT);
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

rustdb> EXPLAIN SELECT name FROM customers WHERE id = 1;
Project name  (rows=1 cost=...)
  SeqScan customers  (rows=1 cost=...)
    predicate: (id = 1)

rustdb> \q
```

Reopen the same file and the schema and rows are still there.

## What works

- **DDL**: `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`.
- **DML**: `INSERT` (with or without a column list), `UPDATE`, `DELETE`.
- **Queries**: projection and `*`, `WHERE` with three-valued logic, `INNER` and `LEFT JOIN`, `GROUP BY` with `COUNT`/`SUM`/`MIN`/`MAX`/`AVG`, `ORDER BY`, `LIMIT`.
- **`EXPLAIN`**: the cost-annotated physical plan, showing the planner's scan and join choices.
- **Durability**: write-ahead logging, ARIES crash recovery (analysis, redo, undo), and schema plus data that survive a restart.
- **Isolation**: MVCC snapshots, so a reader sees a stable view while writers proceed.

## Crates

| Layer | Crate | Responsibility |
|---|---|---|
| Storage | [`storage`](crates/storage/) | Pages, buffer pool, B+ tree |
| Durability | [`wal`](crates/wal/) | Write-ahead log and ARIES recovery |
| Concurrency | [`txn`](crates/txn/) | Transaction manager and MVCC |
| Parsing | [`sql`](crates/sql/) | SQL lexer and recursive-descent parser |
| Optimization | [`planner`](crates/planner/) | Logical plan, cost model, physical plan, EXPLAIN |
| Execution | [`executor`](crates/executor/) | Volcano operators and the row codec |
| Engine | [`rustdb`](crates/rustdb/) | The embedded database that wires every layer together |
| CLI | [`rustdb-cli`](crates/rustdb-cli/) | The interactive shell |

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo run --bin rustdb        # the CLI
```

## Architecture

Every design decision, with the alternatives considered and rejected, is written up in [docs/design.md](docs/design.md). Contribution guidelines are in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
