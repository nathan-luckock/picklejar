<div align="center">

```
██████╗ ██╗   ██╗███████╗████████╗██████╗ ██████╗
██╔══██╗██║   ██║██╔════╝╚══██╔══╝██╔══██╗██╔══██╗
██████╔╝██║   ██║███████╗   ██║   ██║  ██║██████╔╝
██╔══██╗██║   ██║╚════██║   ██║   ██║  ██║██╔══██╗
██║  ██║╚██████╔╝███████║   ██║   ██████╔╝██████╔╝
╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝   ╚═════╝ ╚═════╝
```

### a relational database, built from the bytes up — in Rust

*Disk pages, a write-ahead log with crash recovery, MVCC transactions, a cost-based SQL planner, and the real PostgreSQL wire protocol. No SQLite under the hood, no parser crate, no ORM. Every layer lives in this repo.*

<br/>

[![CI](https://github.com/nathan-luckock/capstone/actions/workflows/ci.yml/badge.svg)](https://github.com/nathan-luckock/capstone/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-~29k%20LOC-CE422B?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-500%2B%20%2B%20fuzzing-3FB950)](#proof-not-vibes)
[![Wire](https://img.shields.io/badge/wire-PostgreSQL%20v3-336791?logo=postgresql&logoColor=white)](#-it-speaks-postgres)
[![From scratch](https://img.shields.io/badge/built-from%20scratch-8957E5)](#whats-inside)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-2F81F7)](#license)

</div>

---

> **TL;DR** — `psql`, JDBC, and `psycopg` connect to this engine over TCP and have no idea it isn't Postgres. Behind the socket: a B+ tree on 8 KiB pages, ARIES recovery proven by a deterministic crash simulator, snapshot-isolation MVCC, and a planner that picks hash vs. nested-loop joins from statistics.

## ⚡ Quickstart

```bash
cargo run --bin rustdb -- --database mydb.db
```

```sql
rustdb> CREATE TABLE customers (id SERIAL PRIMARY KEY, name TEXT NOT NULL);
rustdb> INSERT INTO customers (name) VALUES ('alice'), ('bob') RETURNING id, name;
rustdb> SELECT c.name, SUM(o.total) FROM orders o JOIN customers c ON o.cid = c.id GROUP BY c.name;
rustdb> EXPLAIN SELECT name FROM customers WHERE id = 1;   -- the cost-annotated plan
```

Close the file, reopen it, and your schema and rows are still there.

## 🐘 It speaks Postgres

There is no shim. The server implements the PostgreSQL v3 wire protocol, so the actual `psql` binary connects to the from-scratch engine:

```bash
cargo run --release --bin rustdb-pg -- --database mydb.db --port 5433
psql -h 127.0.0.1 -p 5433 -U postgres
```

```text
postgres=> SELECT name FROM engineers AS e
postgres->  WHERE rust_years > (SELECT AVG(rust_years) FROM engineers WHERE active = e.active);
 name
------
 Ada
(1 row)
```

That correlated subquery, the aggregate, and `EXPLAIN` all run through the engine and come back as ordinary psql tables. Both the **simple** and the **extended** query protocol (server-side prepared statements with `$N` parameters) are implemented and verified against `psql` 18's `\bind`.

## What's inside

A toy "build a database" project stops at a key-value store or wraps an existing engine. rustdb implements the parts that actually make a database a database:

- 🗄️ **Storage** — 8 KiB slotted pages, an LRU-K buffer pool, a B+ tree, CRC32 checksums, all behind a `Disk` trait.
- 🛟 **Crash safety** — a write-ahead log and full ARIES recovery (analysis → redo → undo with compensation records). No committed transaction is lost.
- 🔀 **Transactions** — MVCC snapshot isolation; `BEGIN` / `COMMIT` / `ROLLBACK`; a reader never blocks a writer.
- 🧮 **A real query engine** — hand-written lexer + Pratt parser, a cost-based planner (seq vs. index scan, nested-loop vs. hash join), and a Volcano executor.
- 🐘 **PostgreSQL wire protocol** — real clients and drivers connect over TCP.
- 🪟 **A deep slice of SQL** — joins, `GROUP BY` / `HAVING`, window functions, `UNION` / `INTERSECT` / `EXCEPT`, correlated subqueries, `WITH` and `WITH RECURSIVE`, upserts, and `information_schema`. → [full feature list](docs/FEATURES.md)

## 📊 Progress

```text
Storage · pages, buffer pool, B+ tree     ██████████  done
WAL + ARIES crash recovery                ██████████  done
MVCC transactions                         ██████████  done
SQL surface · joins, windows, CTEs, sets  █████████▒  deep
Cost-based planner · EXPLAIN              ████████▒▒  solid
PostgreSQL wire · simple + extended       █████████▒  solid
Concurrency · multiple connections        ███▒▒▒▒▒▒▒  next
Types · DATE / TIMESTAMP / DECIMAL        ██▒▒▒▒▒▒▒▒  next
```

Full roadmap and the reasoning behind each decision live in [docs/design.md](docs/design.md).

## Proof, not vibes

Correctness is not asserted, it is tested three independent ways — all under `clippy -D warnings` and `rustfmt` in CI:

- **500+ unit tests + parser property tests** across 10 crates.
- **Deterministic simulation testing** — every crash scenario is one `u64` seed against a fault-injecting disk, so any failure replays exactly. It found and fixed a real recovery bug. `cargo run --release --bin dst -- 100000`
- **Differential testing vs SQLite** — random SQL run through both engines, results compared as a sorted multiset, with SQLite as the independent oracle. `cargo run --release --bin difftest -- 100000`

```text
crates/  storage · wal · txn · sql · planner · executor · rustdb · rustdb-cli · rustdb-server · rustdb-difftest
```

## 🗺️ Architecture

```
   psql / drivers ──TCP──▶ rustdb-pg (PostgreSQL v3 wire)
                                │
   rustdb-cli (REPL) ──────────┤  Database::execute(sql)
                                ▼
        sql ▸ parser, AST  →  planner ▸ logical/physical plan, cost model, EXPLAIN
                                ▼
        executor ▸ Volcano operators, expression eval
                                ▼
        txn ▸ MVCC, snapshots   │   wal ▸ write-ahead log, ARIES recovery
                                ▼
        storage ▸ pages, buffer pool, B+ tree  →  data file + WAL on disk
```

## 📚 Docs

| | |
|---|---|
| [docs/design.md](docs/design.md) | Every design decision, with alternatives considered and rejected |
| [docs/FEATURES.md](docs/FEATURES.md) | The complete SQL surface and engine features |
| [docs/sprints.md](docs/sprints.md) | How the build was sequenced |
| [CONTRIBUTING.md](CONTRIBUTING.md) | `fmt` + `clippy -D warnings` + `test` before every PR |

## 👋 About

I'm Nathan — 20, and I've been getting paid to write software since I was 16. rustdb is my senior capstone (CSE 499). I wanted to know if I could build a *real* database rather than a toy, so I wrote every layer myself: the bytes on disk, crash recovery, the optimizer, the Postgres wire protocol. It's open source because I think the best way to learn how databases work is to read one that someone built on purpose, end to end. Still shipping.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
