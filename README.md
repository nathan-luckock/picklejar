<div align="center">

# picklejar

**Durable, isolated AI memory for hardware you cannot reach.**
Proven by 100,000 deterministic crash simulations. A from-scratch, Postgres-wire database engine in Rust.

[![CI](https://img.shields.io/github/actions/workflow/status/nathan-luckock/capstone/ci.yml?style=flat-square&label=CI&logo=github)](https://github.com/nathan-luckock/capstone/actions/workflows/ci.yml)
[![Crash sims](https://img.shields.io/badge/crash%20sims-100%2C000%20passed-3FB950?style=flat-square&logo=checkmarx&logoColor=white)](#proof-not-vibes)
[![Rust](https://img.shields.io/badge/Rust-from%20scratch-CE422B?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Postgres wire](https://img.shields.io/badge/wire-PostgreSQL%20v3-336791?style=flat-square&logo=postgresql&logoColor=white)](#it-speaks-postgres)
[![License](https://img.shields.io/badge/license-MIT%20or%20Apache--2.0-2F81F7?style=flat-square)](#license)

</div>

---

## What this is

picklejar is a relational database engine written from scratch in Rust, evolving into a specific thing: **the memory layer for AI in environments that cannot be physically serviced**, such as orbital and edge data centers, where a failed disk is never swapped and a partitioned link is never fixed by hand.

The data layer for those places has one hard requirement: it must keep committed data intact and isolated through faults that nobody is around to repair, and it must be able to *prove* it. That proof, durability and tenant isolation established by deterministic crash simulation rather than asserted, is what this project is built around.

## Why now

Compute is moving to places people cannot reach. [Starcloud](https://www.starcloud.com) is already running GPUs in orbit and training models in space (a $1.1B valuation and an 88,000-satellite data-center filing as of early 2026), and the edge keeps pushing further out. AI workloads in those places need durable, isolated memory: embeddings, retrieval, and per-tenant context that survive failure and never leak across tenants.

Honest scoping of what is and is not new:

- **Not novel on its own.** Vector search and engine-enforced row-level isolation already ship together (Postgres with `pgvector` and RLS, Oracle 23ai). Deterministic simulation testing is a respected technique (FoundationDB, TigerBeetle, Antithesis).
- **The uncontested combination.** A single from-scratch engine that is an AI memory layer with engine-enforced isolation, whose durability and isolation are *proven by deterministic crash simulation*, aimed at unreachable infrastructure. Reliability testing of vector databases is still posed as an open problem in the literature ([*Towards Reliable Vector Database Management Systems: A Software Testing Roadmap for 2030*](https://arxiv.org/abs/2502.20812)).

## Where it stands

| Layer | State |
|---|---|
| Crash-proven SQL engine (storage, WAL + ARIES recovery, MVCC, cost-based planner, roles, RLS, Postgres wire) | **done**, cross-checked against SQLite and a fault-injecting simulator |
| Native vector type (`VECTOR(n)`, pgvector-style literals, durable storage) | **done** |
| Distance operators and brute-force KNN (`<->`, `<=>`, `<#>`, `<+>`, plus function forms) | **done** |
| RLS-filtered similarity search (isolation enforced by the engine, not application code) | **done** |
| Fault simulator for the memory layer (`vecsim`: durability and isolation under simulated crash) | **done** |
| HNSW index (4 metrics, insert/search/delete, durable, recall > 0.98 on hard data) | **done** |
| HNSW wired into SQL: `ORDER BY col <-> :q LIMIT k` served from a write-invalidated, RLS-safe cached index (~150x warm) | **done** |
| Orbital radiation fault model injected into the live simulator (`vecsim --irradiate`, dose set by orbit) | **done** |
| Corruption detection and self-healing (page and index CRC32 enforced, redundant self-healing index, metamorphic oracle) | **done** |
| Regenerable reliability certificate (`vecert`, framed in orbital upset rates) | **done** |

**Where it is headed.** The core is in place: the HNSW index is reachable from SQL through a cached, opt-in, row-level-security-safe path that turns a repeated nearest-neighbor query into roughly a 150x speedup over the exact scan while still fencing each tenant to its own rows; the corruption story is enforced end to end (page and index checksums refuse a flipped bit, the index self-heals from a redundant copy, a metamorphic oracle tests approximate search without a ground-truth answer); and the orbital radiation fault model is injected into the live simulator, so a multi-tenant workload can be irradiated at a chosen orbit's upset rate for a chosen dwell time and proven to never serve a silently corrupted answer. From here: corrupting the WAL stream as well as the heap, replication and point-in-time recovery, and model-checking the core recovery and isolation invariants. The full plan and its honest scoping are in [docs/ROADMAP.md](docs/ROADMAP.md).

## Quickstart

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

### The memory layer

```sql
-- A memory store for AI agents: each tenant is fenced off by the engine.
CREATE TABLE memories (id SERIAL PRIMARY KEY, tenant TEXT, embedding VECTOR(3));
CREATE POLICY tenant ON memories USING (tenant = current_user);
ALTER TABLE memories ENABLE ROW LEVEL SECURITY;

INSERT INTO memories (tenant, embedding) VALUES ('acme', '[0.1, 0.2, 0.9]');

-- Nearest-neighbor recall, fenced to the calling tenant's own rows.
SELECT id FROM memories ORDER BY embedding <-> '[0.1, 0.2, 0.8]' LIMIT 5;
```

A `VECTOR(n)` column stores an embedding as native `f32`, validates its width on write, and survives a crash and reopen like any other value. Similarity search runs through the same row-level-security fence as every other read, so a tenant's nearest-neighbor query can only ever rank that tenant's own vectors, enforced by the engine.

That last query can be served two ways. By default it is an exact scan. Turn on the index path and the same SQL is answered from a cached HNSW index instead, roughly 150x faster on a warm query, and only when row-level security does not apply to the query, so the acceleration can never widen what a tenant can see. An RLS-fenced query always falls back to the exact, fenced path.

## It speaks Postgres

No shim. The server implements the PostgreSQL v3 wire protocol, so the real `psql` binary connects straight to the engine:

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

That correlated subquery, the aggregate, and `EXPLAIN` all run through the engine. Both the simple and the extended query protocol (server-side prepared statements with `$N` parameters) are implemented and verified against `psql` 18.

## What's inside

A toy "build a database" project stops at a key-value store or wraps an existing engine. picklejar implements the parts that make a database a database:

| | |
|---|---|
| **Storage** | 8 KiB slotted pages, an LRU-K buffer pool, a B+ tree, CRC32 checksums verified on every read, all behind a `Disk` trait |
| **Crash safety** | a write-ahead log and full ARIES recovery (analysis, redo, undo with compensation records) |
| **Transactions** | MVCC snapshot isolation; `BEGIN` / `COMMIT` / `ROLLBACK`; a reader never blocks a writer |
| **Query engine** | hand-written lexer and Pratt parser, a cost-based planner, and a Volcano executor |
| **Security** | roles, `GRANT` / `REVOKE`, ownership, and row-level security enforced in the engine |
| **Vector memory** | `VECTOR(n)` type, four distance metrics, KNN, and an HNSW index (build, search, delete, persist) wired into SQL through a cached, RLS-safe path |
| **Reliability under fault** | page and index checksums refuse corrupt data, a self-healing redundant index, a metamorphic oracle, and a regenerable certificate |
| **Postgres wire** | real clients and drivers connect over TCP, no shim |
| **Deep SQL** | joins, window functions, set operations, correlated subqueries, CTEs, upserts, `information_schema` |

The complete feature list is in [docs/FEATURES.md](docs/FEATURES.md).

## Proof, not vibes

Correctness is not asserted, it is tested several independent ways, all under `clippy -D warnings` and `rustfmt` in CI:

- **Deterministic simulation testing.** Every crash scenario is one `u64` seed against a fault-injecting disk, so any failure replays exactly. **100,000 seeded crash-and-recover runs pass** (4.1M committed rows verified), and the harness found and fixed a real recovery bug.
- **Differential testing against SQLite.** Random SQL run through both engines, compared as a sorted multiset, with SQLite as the independent oracle.
- **Vector durability and isolation simulation (`vecsim`).** The same seeded, replayable model applied to the memory layer: a multi-tenant embedding workload writing through the RLS fence, a crash, then a check that every committed embedding survives intact and each tenant sees only its own after recovery, on reads and on nearest-neighbor ranking.
- **A metamorphic oracle for approximate search.** Relations that must always hold (self-retrieval, monotonic insertion, deletion consistency, recall monotonicity) test correctness without a ground-truth answer, the accepted answer to the oracle problem for approximate search.
- **Corruption detection and self-healing.** Every page and every serialized index carries a CRC32 that is verified on read, so a flipped bit is refused rather than served; the index keeps a redundant copy and reconstructs itself from it with no intervention.
- **An orbital radiation fault model in the live simulator.** A committed multi-tenant workload is irradiated on disk at a chosen orbit's single-event-upset rate for a chosen dwell time, then reopened: the engine either detects the corruption or it changed no committed answer, but it never serves a tenant a silently wrong embedding and never leaks another tenant's row. The injected dose is the orbit model, not an arbitrary fault count.

```bash
cargo run --release --bin dst -- 100000              # 100k reproducible crash scenarios
cargo run --release --bin difftest -- 100000         # 100k queries checked against SQLite
cargo run --release --bin vecsim -- 100000           # 100k durability + isolation sims
cargo run --release --bin vecsim -- --irradiate 10000 365 geo   # irradiate a year in GEO
cargo run --release --bin vecbench                   # HNSW vs brute-force speedup and recall
cargo run --release --bin vecsqlbench                # cached SQL index path vs exact scan
cargo run --release --bin vecert                     # the regenerable reliability certificate
```

A database for hardware you cannot service is only as good as its proof that it survives failure. `vecert` turns that proof into a single regenerable, content-hashed artifact:

```text
[PASS] recall L2 (clustered): recall@10 = 1.0000 over 3000 clustered vectors (oracle: brute force)
[PASS] corruption detection: 7271/7271 single-bit faults detected on load
[PASS] self-healing: 6/6 corrupted copies recovered exactly from redundancy
[PASS] radiation survivability (LEO): modeled low Earth orbit dose ~1.07 upsets/day for a 261 KB index; all detected
result: ALL INVARIANTS HELD
certificate hash: ...  (regenerate from this commit to verify)
```

## Architecture

```text
   psql / drivers ──TCP──▶ picklejar-pg  (PostgreSQL v3 wire)
                                │
   picklejar-cli (REPL) ────────┤   Database::execute(sql)
                                ▼
        sql ▸ parser, AST   →   planner ▸ logical/physical plan, cost model, EXPLAIN
                                ▼
        executor ▸ Volcano operators, expression eval, vector distance
                                ▼
        txn ▸ MVCC, snapshots   │   wal ▸ write-ahead log, ARIES recovery
                                ▼
        storage ▸ pages, buffer pool, B+ tree   →   data file + WAL on disk
```

## Docs

| | |
|---|---|
| [docs/ROADMAP.md](docs/ROADMAP.md) | where this is headed: from a crash-proven vector engine to flight-certifiable AI memory |
| [docs/design.md](docs/design.md) | every design decision, with the alternatives considered and rejected |
| [docs/FEATURES.md](docs/FEATURES.md) | the complete SQL surface and engine features |
| [docs/sprints.md](docs/sprints.md) | how the build was sequenced |
| [CONTRIBUTING.md](CONTRIBUTING.md) | `fmt` + `clippy -D warnings` + `test` before every PR |

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
