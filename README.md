<div align="center">

<img src="https://readme-typing-svg.demolab.com?font=JetBrains+Mono&weight=800&size=58&duration=2200&pause=1200&color=CE422B&center=true&vCenter=true&repeat=false&width=520&height=95&lines=picklejar" alt="picklejar" />

<img src="https://readme-typing-svg.demolab.com?font=Fira+Code&size=18&duration=3200&pause=900&color=768390&center=true&vCenter=true&width=720&height=40&lines=durable+AI+memory+for+hardware+you+can't+reach;proven+by+100%2C000+deterministic+crash+simulations;a+Postgres-wire+engine%2C+built+from+the+bytes+up+in+Rust" alt="picklejar: durable AI memory, proven by deterministic simulation" />

<br/>

[![CI](https://img.shields.io/github/actions/workflow/status/nathan-luckock/capstone/ci.yml?style=flat-square&label=CI&logo=github)](https://github.com/nathan-luckock/capstone/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-~30k%20LOC-CE422B?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Crash sims](https://img.shields.io/badge/crash%20sims-100%2C000%20passed-3FB950?style=flat-square&logo=checkmarx&logoColor=white)](#-proof-not-vibes)
[![Tests](https://img.shields.io/badge/tests-500%2B%20%2B%20fuzzing-3FB950?style=flat-square&logo=checkmarx&logoColor=white)](#-proof-not-vibes)
[![Postgres wire](https://img.shields.io/badge/wire-PostgreSQL%20v3-336791?style=flat-square&logo=postgresql&logoColor=white)](#-it-speaks-postgres)
[![From scratch](https://img.shields.io/badge/built-from%20scratch-8957E5?style=flat-square)](#-whats-inside)
[![License](https://img.shields.io/badge/license-MIT%20or%20Apache--2.0-2F81F7?style=flat-square)](#-license)

</div>

---

## 🛰️ The one-sentence pitch

**`picklejar` is durable, isolated AI memory for environments you cannot physically reach, and it is the first database to *prove* that durability with deterministic crash simulation instead of just asserting it.**

When a server fails in a datacenter, someone walks over and swaps the drive. When a server fails in orbit, or on a remote rig, or at the far edge of a network, *no one is coming*. The data layer for those places has to assume the machine will be hit by faults nobody is around to fix, and it has to be able to **show** it survives them. That is the gap this project is built for.

## 🌌 Why this is the moment

Compute is moving to places people cannot service.

- [Starcloud](https://www.starcloud.com) is already running GPUs in orbit and training models in space. As of early 2026 the orbital-datacenter category carries a roughly **$1.1B** valuation and an **88,000-satellite** datacenter filing.
- The edge keeps pushing further out: remote sensors, autonomous vehicles, offshore and battlefield deployments, anywhere a hand cannot reach the hardware.
- AI workloads in those places need **memory**: embeddings, retrieval, agent state, per-tenant context. That memory has to be durable through faults and isolated between tenants, enforced by the engine and not by hope.

Vector search exists. Engine-enforced tenant isolation exists. Postgres with `pgvector` and row-level security does both today. **What does not exist is a memory layer that proves it keeps that data intact and isolated through the chaos of an environment no one can repair.** That proof is the contribution.

## 🧪 What makes this different (and honest about it)

The individual ingredients are not new, and this project does not pretend they are:

- **Vector + row-level security** is shipped (Supabase's `pgvector` + RLS, Oracle 23ai).
- **Deterministic simulation testing** is a respected technique (FoundationDB pioneered it, TigerBeetle and Antithesis productized it).

The **fusion** is what is uncontested:

> a from-scratch engine that is an **AI / embedding memory layer** with **engine-enforced isolation**, whose **durability and recovery are proven by deterministic crash simulation**, aimed at **unreachable infrastructure**.

Rigorous reliability testing of vector databases is still framed as an open problem in the literature ([*"...a Software Testing Roadmap for 2030"*](https://arxiv.org/pdf/2502.20812)). Being first to actually do it, and to prove it, is the point.

## 📍 Where it stands right now

| Layer | State |
|---|---|
| **Crash-proven SQL engine** (storage, WAL + ARIES recovery, MVCC, cost-based planner, roles, row-level security, Postgres wire) | **done**, built from scratch in Rust, cross-checked against SQLite and a fault-injecting simulator |
| **Native vector type** (`VECTOR(n)`, pgvector-style literals, durable storage) | **shipped** |
| **Distance operators + brute-force KNN** (`<->`, `<=>`, `<#>`, nearest-neighbor search) | **shipped** |
| **RLS-filtered similarity search** (isolation enforced by the engine on vector queries, not app code) | **shipped** |
| **Fault simulator for the memory layer** (`vecsim`: proves *zero data lost, zero data leaked* under simulated crash) | **shipped** |
| **ANN index** (HNSW, for speed at scale) | **index complete** (3 metrics, delete, durable, recall > 0.90), planner wiring next |

The durability is not asserted, it is **proven**: **100,000 seeded crash scenarios, every failure replayable byte-for-byte.** That is the foundation everything else is built on top of.

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

### 🧠 The memory layer, taking shape

```sql
picklejar> CREATE TABLE memories (id SERIAL PRIMARY KEY, tenant TEXT, embedding VECTOR(3));
picklejar> INSERT INTO memories (tenant, embedding) VALUES ('acme', '[0.1, 0.2, 0.9]');
picklejar> INSERT INTO memories (tenant, embedding) VALUES ('acme', VECTOR '[0.0, 0.1, 1.0]');
```

A `VECTOR(n)` column stores an embedding as native `f32` components, validates its width on write, and survives a crash and reopen like any other column. Distance operators and nearest-neighbor search are landing on top of it next, and after that the engine itself enforces that one tenant can never retrieve another tenant's memory.

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
| 🛟 **Crash safety** | a write-ahead log and full ARIES recovery (analysis, redo, undo with compensation records) |
| 🔀 **Transactions** | MVCC snapshot isolation; `BEGIN` / `COMMIT` / `ROLLBACK`; a reader never blocks a writer |
| 🧮 **Query engine** | hand-written lexer + Pratt parser, a cost-based planner, and a Volcano executor |
| 🔐 **Security** | roles, `GRANT` / `REVOKE`, ownership, and row-level security policies enforced in the engine |
| 🧠 **Vector memory** | a native `VECTOR(n)` type with durable storage, the foundation of the AI memory layer |
| 🐘 **Postgres wire** | real clients and drivers connect over TCP, no shim |
| 🪟 **Deep SQL** | joins, window functions, `UNION` / `INTERSECT` / `EXCEPT`, correlated subqueries, `WITH` / `WITH RECURSIVE`, upserts, `information_schema` |

→ the complete feature list lives in **[docs/FEATURES.md](docs/FEATURES.md)**.

## 📊 Progress

```text
Storage · pages, buffer pool, B+ tree     ██████████  done
WAL + ARIES crash recovery                ██████████  done   (100k sims passed)
MVCC transactions                         ██████████  done
SQL surface · joins, windows, CTEs, sets  █████████▒  deep
Types · date, timestamp, json, decimal    ██████████  done
Security · roles, grants, row-level RLS   ██████████  done
Cost-based planner · ANALYZE, EXPLAIN     █████████▒  solid
PostgreSQL wire · simple + extended       █████████▒  solid
Vector memory · VECTOR type + distance KNN ██████████  done
RLS-filtered similarity · isolation proof ██████████  done
Fault simulator · vecsim durab + isolation ██████████  done
ANN index · HNSW (3 metrics, CRUD, durable) ███████▒▒▒  index done, planner wiring next
```

The full roadmap and the reasoning behind every decision are in [docs/design.md](docs/design.md).

## 🔬 Proof, not vibes

Correctness isn't asserted, it's tested three independent ways, all under `clippy -D warnings` and `rustfmt` in CI:

- ✅ **500+ unit tests + parser property tests** across the workspace crates.
- 🎲 **Deterministic simulation testing.** Every crash scenario is one `u64` seed against a fault-injecting disk, so any failure replays exactly. **100,000 seeded crash-and-recover runs pass**, and the harness found and fixed a real recovery bug.
- 🆚 **Differential testing vs SQLite.** Random SQL run through both engines, results compared as a sorted multiset, with SQLite as the independent oracle.
- 🧠 **Vector durability + isolation simulation (`vecsim`).** The same seeded, replayable model applied to the AI memory layer: a random multi-tenant embedding workload, a crash, then a check that every committed embedding survives intact *and* that each tenant sees only its own after recovery, on reads and on nearest-neighbor ranking.

```bash
cargo run --release --bin dst -- 100000        # 100k reproducible crash scenarios
cargo run --release --bin difftest -- 100000   # 100k queries checked against SQLite
cargo run --release --bin vecsim -- 100000     # 100k durability + isolation sims
```

This is the part that matters most for the mission: a database for hardware you cannot service is only as good as its proof that it survives failure. That proof is reproducible, on demand, from a single integer seed.

## 🗺️ Architecture

```text
   psql / drivers ──TCP──▶ picklejar-pg  (PostgreSQL v3 wire)
                                │
   picklejar-cli (REPL) ──────────┤   Database::execute(sql)
                                ▼
        sql ▸ parser, AST   →   planner ▸ logical/physical plan, cost model, EXPLAIN
                                ▼
        executor ▸ Volcano operators, expression eval, vector distance
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
