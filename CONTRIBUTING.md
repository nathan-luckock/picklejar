# Contributing

Thanks for your interest. This is a relational database engine written in
Rust; the architecture and the rationale behind each decision live in
[docs/design.md](docs/design.md), which is the best place to start.

## Ground rules

These are enforced in CI, so check them locally before opening a pull request:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- The graded engine (storage, WAL and recovery, MVCC, the SQL parser, the
  planner, the executor) is implemented in this workspace. External crates are
  limited to plumbing (error handling, logging, CLI parsing); no embedded
  database, SQL parser crate, or checksum crate.
- Each change should carry its reasoning. Commits include a short note on what
  was chosen and why, and larger decisions are written up in `docs/design.md`
  with the alternatives that were rejected.
- Add tests with the code. Unit tests live beside the module; property tests
  and integration tests live under each crate's `tests/` directory.

## Workflow

Work on a feature branch, open a pull request against `main`, and let CI run.
Squash-merge once it is green.

## Layout

| Crate | Responsibility |
|---|---|
| `rustdb-storage` | Pages, buffer pool, B+ tree |
| `rustdb-wal` | Write-ahead log and ARIES recovery |
| `rustdb-txn` | Transactions and MVCC |
| `rustdb-sql` | SQL lexer and parser |
| `rustdb-planner` | Logical plan, cost model, physical plan, EXPLAIN |
| `rustdb-executor` | Volcano operators and row codec |
| `rustdb` | The embedded engine that wires it together |
| `rustdb-cli` | The interactive shell |
