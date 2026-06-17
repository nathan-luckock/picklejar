<div align="center">

<img src="https://readme-typing-svg.demolab.com?font=JetBrains+Mono&weight=800&size=58&duration=2200&pause=1200&color=CE422B&center=true&vCenter=true&repeat=false&width=520&height=95&lines=picklejar" alt="picklejar" />

<img src="https://readme-typing-svg.demolab.com?font=Fira+Code&size=18&duration=3200&pause=900&color=768390&center=true&vCenter=true&width=660&height=40&lines=a+relational+database%2C+from+the+bytes+up;disk+%E2%86%92+WAL+%E2%86%92+MVCC+%E2%86%92+planner+%E2%86%92+Postgres+wire;no+SQLite.+no+parser+crate.+every+layer+lives+here." alt="a relational database, built from the bytes up, in Rust" />

<br/>

[![CI](https://img.shields.io/github/actions/workflow/status/nathan-luckock/capstone/ci.yml?style=flat-square&label=CI&logo=github)](https://github.com/nathan-luckock/capstone/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-~29k%20LOC-CE422B?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-500%2B%20%2B%20fuzzing-3FB950?style=flat-square&logo=checkmarx&logoColor=white)](#-proof-not-vibes)
[![Postgres wire](https://img.shields.io/badge/wire-PostgreSQL%20v3-336791?style=flat-square&logo=postgresql&logoColor=white)](#-it-speaks-postgres)
[![From scratch](https://img.shields.io/badge/built-from%20scratch-8957E5?style=flat-square)](#-whats-inside)
[![License](https://img.shields.io/badge/license-MIT%20or%20Apache--2.0-2F81F7?style=flat-square)](#-license)

</div>

---

> [!NOTE]
> `psql`, JDBC, and `psycopg` connect to this engine over TCP and never notice it isn't Postgres. Behind the socket: a B+ tree on 8 KiB pages, ARIES crash recovery proven by a deterministic simulator, snapshot-isolation MVCC, and a planner that picks hash vs. nested-loop joins from statistics.

## ⚡ Quickstart

```bash
cargo run --bin picklejar -- --database mydb.db
```

```sql
picklejar> CREATE TABLE customers (id SERIAL PRIMARY KEY, name TEXT NOT NULL);
picklejar> INSERT INTO customers (name) VALUES ('alice'), ('bob') RETURNING id, name;
picklejar> SELECT c.name, SUM(o.total) FROM orders o JOIN customers c ON o.cid = c.id GROUP BY c.name;
picklejar> EXPLAIN SELECT name FROM customers WHERE id = 1;   -- the cost-annotated plan
```

Close the file, reopen it, and your schema and rows are still there.

## 🐘 It speaks Postgres

No shim. The server implements the PostgreSQL v3 wire protocol, so the actual `psql` binary connects straight to the from-scratch engine:

```bash
cargo run --release --bin picklejar-pg -- --database mydb.db --port 5433
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

## 🧩 What's inside

A toy "build a database" project stops at a key-value store or wraps an existing engine. picklejar implements the parts that actually make a database a database:

| | |
|---|---|
| 🗄️ **Storage** | 8 KiB slotted pages, an LRU-K buffer pool, a B+ tree, CRC32 checksums, all behind a `Disk` trait |
| 🛟 **Crash safety** | a write-ahead log and full ARIES recovery (analysis → redo → undo with compensation records) |
| 🔀 **Transactions** | MVCC snapshot isolation; `BEGIN` / `COMMIT` / `ROLLBACK`; a reader never blocks a writer |
| 🧮 **Query engine** | hand-written lexer + Pratt parser, a cost-based planner, and a Volcano executor |
| 🐘 **Postgres wire** | real clients and drivers connect over TCP, no shim |
| 🪟 **Deep SQL** | joins, window functions, `UNION` / `INTERSECT` / `EXCEPT`, correlated subqueries, `WITH` / `WITH RECURSIVE`, upserts, `information_schema` |

→ the complete feature list lives in **[docs/FEATURES.md](docs/FEATURES.md)**.

## 📊 Progress

```text
Storage · pages, buffer pool, B+ tree     ██████████  done
WAL + ARIES crash recovery                ██████████  done
MVCC transactions                         ██████████  done
SQL surface · joins, windows, CTEs, sets  █████████▒  deep
Types · date, timestamp, json, decimal    ██████████  done
Cost-based planner · ANALYZE, EXPLAIN     █████████▒  solid
PostgreSQL wire · simple + extended       █████████▒  solid
Concurrency · many connections (actor)    ████████▒▒  works
Indexing, auth, replication               ███▒▒▒▒▒▒▒  next
```

The full roadmap and the reasoning behind every decision are in [docs/design.md](docs/design.md).

## 🔬 Proof, not vibes

Correctness isn't asserted, it's tested three independent ways - all under `clippy -D warnings` and `rustfmt` in CI:

- ✅ **500+ unit tests + parser property tests** across 10 crates.
- 🎲 **Deterministic simulation testing** - every crash scenario is one `u64` seed against a fault-injecting disk, so any failure replays exactly. It found and fixed a real recovery bug.
- 🆚 **Differential testing vs SQLite** - random SQL run through both engines, results compared as a sorted multiset, with SQLite as the independent oracle.

```bash
cargo run --release --bin dst -- 100000        # 100k reproducible crash scenarios
cargo run --release --bin difftest -- 100000   # 100k queries checked against SQLite
```

## 🗺️ Architecture

```text
   psql / drivers ──TCP──▶ picklejar-pg  (PostgreSQL v3 wire)
                                │
   picklejar-cli (REPL) ──────────┤   Database::execute(sql)
                                ▼
        sql ▸ parser, AST   →   planner ▸ logical/physical plan, cost model, EXPLAIN
                                ▼
        executor ▸ Volcano operators, expression eval
                                ▼
        txn ▸ MVCC, snapshots   │   wal ▸ write-ahead log, ARIES recovery
                                ▼
        storage ▸ pages, buffer pool, B+ tree   →   data file + WAL on disk
```

## 📚 Docs

| | |
|---|---|
| [docs/design.md](docs/design.md) | every design decision, with alternatives considered and rejected |
| [docs/FEATURES.md](docs/FEATURES.md) | the complete SQL surface and engine features |
| [docs/sprints.md](docs/sprints.md) | how the build was sequenced |
| [CONTRIBUTING.md](CONTRIBUTING.md) | `fmt` + `clippy -D warnings` + `test` before every PR |

## 📄 License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
