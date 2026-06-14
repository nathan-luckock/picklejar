# capstone - a relational database from scratch (Rust)

> CSE 499 senior project. A real disk-based relational database engine with ACID guarantees, written from scratch in Rust.

Not a SQLite/Postgres wrapper. Not a key-value store with SQL on top. A real engine: page manager, buffer pool, B+ tree indexes, WAL + ARIES-style recovery, MVCC for concurrent reads, a hand-written SQL parser, a cost-based query planner, and a query executor.

## Status

Storage, durability (WAL + ARIES recovery), transactions (MVCC), the SQL
parser, and the cost-based planner are built and tested. The query executor
and CLI wiring come next, which will let typed SQL run end to end.

| Layer | Crate | What it does | Status |
|---|---|---|---|
| Storage | [`storage`](crates/storage/) | Pages, buffer pool, B+ tree | built |
| Durability | [`wal`](crates/wal/) | Write-ahead log + ARIES recovery | built |
| Concurrency | [`txn`](crates/txn/) | Transaction manager + MVCC | built |
| Parsing | [`sql`](crates/sql/) | Hand-written SQL parser (lexer + recursive-descent) | built |
| Optimization | [`planner`](crates/planner/) | Cost-based query planner + EXPLAIN | built |
| Execution | [`executor`](crates/executor/) | Seq scan, index scan, hash join, nested-loop join | in progress |
| Library entry | [`rustdb`](crates/rustdb/) | Top-level DB handle, embeds all layers | in progress |
| CLI | [`rustdb-cli`](crates/rustdb-cli/) | `psql`-style interactive shell | in progress |

## Build

```bash
cargo build --workspace
cargo test --workspace
cargo run --bin rustdb        # CLI
```

## Architecture

Every design decision, with the alternatives considered and rejected, is
written up in [docs/design.md](docs/design.md). Each commit also carries a
`Design notes:` section explaining what was chosen and why.

## License

MIT OR Apache-2.0
