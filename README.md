<div align="center">

# picklejar

### A database for when no one is coming to fix it.

picklejar is a from-scratch, Postgres-wire database engine built in Rust for places humans cannot physically reach - satellites, seabed nodes, and remote field robotics.
When a disk corrupts in orbit, there is no technician to swap it. picklejar expects the fault, repairs it from parity, and mathematically proves it never served a wrong answer. No vibes, no dependencies, just absolute durability.

[![CI](https://img.shields.io/github/actions/workflow/status/nathan-luckock/picklejar/ci.yml?style=flat-square&label=CI&logo=github)](https://github.com/nathan-luckock/picklejar/actions/workflows/ci.yml)
[![Crash sims](https://img.shields.io/badge/crash%20sims-1%2C000%2C000%20passed-3FB950?style=flat-square&logo=checkmarx&logoColor=white)](#proof-not-vibes)
[![Model-checked](https://img.shields.io/badge/invariants-5%20exhaustively%20proven-8957e5?style=flat-square&logo=ghostery&logoColor=white)](#proof-not-vibes)
[![No unsafe](https://img.shields.io/badge/unsafe-forbidden-CE422B?style=flat-square&logo=rust&logoColor=white)](#)
[![Postgres wire](https://img.shields.io/badge/wire-PostgreSQL%20v3-336791?style=flat-square&logo=postgresql&logoColor=white)](#it-speaks-postgres)
[![License](https://img.shields.io/badge/license-MIT%20or%20Apache--2.0-2F81F7?style=flat-square)](#license)

<br/>

<img src="docs/img/attest.svg" alt="picklejar attest: one command re-verifies every guarantee, 34/34 checks passed" width="820"/>

</div>

> Built for hardware you can't reach: a satellite, a forward-deployed sensor, a seabed node, a robot in the field. When a disk corrupts and there is no technician, picklejar detects it, repairs it from parity, and proves it never served a wrong answer or leaked one tenant's data into another's.

<table align="center">
<tr>
<td align="center"><strong>1,000,000</strong><br/>crash sims survived</td>
<td align="center"><strong>40,947,775</strong><br/>rows verified</td>
<td align="center"><strong>5</strong><br/>invariants <em>exhaustively</em><br/>model-checked</td>
<td align="center"><strong>0</strong><br/>lines of <code>unsafe</code></td>
<td align="center"><strong>0</strong><br/>crypto dependencies</td>
</tr>
</table>

---

## See it prove itself

The terminal above is real output. `cargo run --release --bin attest` re-verifies every guarantee live and emits a single content hash over all of them; re-run it from this commit and the hash matches. `cargo run --release --bin scorecard` does the same for live throughput and the 20 proven invariants. The verification results are deterministic and regenerable. Nothing here is a static claim.

## Run something impossible

Each row is a from-scratch primitive with a live demo. No libraries are pulled in; even the SHA-256 and the finite-field math are hand-written.

| Watch it happen | One command |
|---|---|
| A nearest-neighbor answer you can **verify without trusting the server** | `cargo run --release --bin authknn` |
| A memory that **proves it forgot**, unrecoverable even after a crash and a parity rebuild | `cargo run --release --bin forgetsim` |
| A history that **catches a forger** who rewrote and re-signed the whole log | `cargo run --release --bin ledgersim` |
| Years of **orbital radiation** survived with zero data loss | `cargo run --release --bin resilientsim` |

There are ~45 more, every one a primitive built by hand: HyperLogLog, Count-Min, product quantization, Shamir secret sharing, private information retrieval, CRDTs, consistent hashing, and the rest. The full catalog is in [docs/gallery.md](docs/gallery.md).

## What it is

picklejar is a relational database engine, built from the page layout up, for **infrastructure humans cannot physically service**: orbital and edge nodes, remote sensors, anywhere a failed disk is never swapped and a partitioned link is never fixed by hand. ([Starcloud](https://www.starcloud.com) is already running GPUs in orbit; the edge keeps pushing further out.)

The data layer for those places has one hard job: keep committed data intact and tenant-isolated through faults that nobody is around to repair, and *prove* it. That proof is the product. Everything else, including the AI-memory layer, the cryptography, and the distributed pieces, is what owning the whole I/O path makes possible.

**Honest scoping.** No single part is novel: vector search with engine-enforced isolation ships in Postgres + pgvector and Oracle 23ai; deterministic simulation testing is FoundationDB and TigerBeetle; erasure coding is in every object store. The uncontested combination is a single from-scratch engine that is reliability infrastructure for AI memory, self-healing under a modeled fault environment, whose durability and isolation are *proven* by deterministic replay and exhaustive model-checking. The full argument is in [docs/why-from-scratch.md](docs/why-from-scratch.md).

## What's inside

A toy "build a database" project stops at a key-value store or wraps an existing engine. picklejar implements the parts that make a database a database:

| | |
|---|---|
| **Storage** | 8 KiB slotted pages, an LRU-K buffer pool, a B+ tree, CRC32 checksums verified on every read, all behind a `Disk` trait |
| **Crash safety** | a write-ahead log and full ARIES recovery (analysis, redo, undo with compensation records) |
| **Transactions** | MVCC snapshot isolation; `BEGIN` / `COMMIT` / `ROLLBACK`; a reader never blocks a writer |
| **Query engine** | hand-written lexer and Pratt parser, a cost-based planner, and a Volcano executor |
| **Security** | roles, `GRANT` / `REVOKE`, ownership, and row-level security enforced in the engine |
| **Vector memory** | `VECTOR(n)` type, four distance metrics, KNN, and an HNSW index wired into SQL through a cached, RLS-safe path |
| **Reliability under fault** | page, index, and metadata checksums; a self-healing redundant index; Reed-Solomon erasure coding; a regenerable certificate |
| **Postgres wire** | real clients and drivers connect over TCP, no shim |
| **Multi-node (AP)** | Dynamo-style replication: consistent-hash placement, quorum writes, CRDT merge, Merkle anti-entropy, tenant-fenced distributed vector KNN, proven to converge under partition |

The complete SQL surface is in [docs/FEATURES.md](docs/FEATURES.md).

## Proof, not vibes

Correctness is not asserted, it is tested several independent ways, all under `clippy -D warnings` and `rustfmt` in CI:

- **Deterministic simulation.** Every crash scenario is one `u64` seed against a fault-injecting disk, so any failure replays exactly. 1,000,000 seeded crash-and-recover runs pass (40,947,775 committed rows verified), and the harness found and fixed a real recovery bug.
- **Exhaustive model-checking.** From-scratch bounded model checkers enumerate *every* reachable interleaving of an abstract model and prove an invariant holds in all of them: WAL ordering, snapshot isolation, RLS-filtered retrieval, cache freshness, and valid-time travel. Each has a deliberately buggy variant that produces a counterexample, so the check has teeth.
- **Differential testing against SQLite.** Random SQL run through both engines, compared as a sorted multiset, with SQLite as the independent oracle.
- **Corruption detection and self-healing.** All four storage-write fault classes are caught: payload checksums refuse a bit flip or torn write, a self-identifying page id catches a misdirected write (another page's still-valid image landed at the wrong place), and the LSN-versus-log guard catches a lost write. Reed-Solomon parity then reconstructs the bad heap page on open, at `+m/k` storage overhead instead of the `+m*100%` of redundant copies.
- **A modeled fault environment.** A committed multi-tenant workload is irradiated at a chosen orbit's single-event-upset rate across every persistent file, then reopened: the engine either detects the corruption or it changed no committed answer, and it never leaks a tenant.
- **Convergence under partition.** A multi-node cluster takes writes on both sides of a network split and reconciles itself on heal with no coordinator; a seeded simulator sweeps thousands of random partition-and-crash schedules and every one converges, the cluster-level counterpart of the crash sims.

```bash
cargo run --release --bin dst -- 1000000      # 1,000,000 reproducible crash scenarios
cargo run --release --bin walmodel             # exhaustively model-check the WAL ordering invariant
cargo run --release --bin rlsmodel             # model-check tenant isolation through the index
cargo run --release --bin difftest -- 100000   # 100k queries checked against SQLite
cargo run --release --bin vecsim -- 100000     # 100k durability + isolation sims
cargo run --release --bin vecert               # the regenerable reliability certificate
cargo run --release --bin repsim -- 2000        # thousands of partition schedules, all converge
cargo run --release --bin repdemo              # the partition money-shot, narrated
```

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

```sql
-- A memory store for AI agents: each tenant is fenced off by the engine.
CREATE TABLE memories (id SERIAL PRIMARY KEY, tenant TEXT, embedding VECTOR(3));
CREATE POLICY tenant ON memories USING (tenant = current_user);
ALTER TABLE memories ENABLE ROW LEVEL SECURITY;

INSERT INTO memories (tenant, embedding) VALUES ('acme', '[0.1, 0.2, 0.9]');

-- Nearest-neighbor recall, fenced to the calling tenant's own rows.
SELECT id FROM memories ORDER BY embedding <-> '[0.1, 0.2, 0.8]' LIMIT 5;
```

`SET vector_index = on` answers that query from a cached HNSW index (~150x faster on a warm query), and only when row-level security does not apply, so the acceleration can never widen what a tenant sees. The memory layer also travels in time (bitemporal `AS OF`), detects contradictions on write, and holds recall flat under drift; the details are in [docs/FEATURES.md](docs/FEATURES.md).

## It speaks Postgres

No shim. The server implements the PostgreSQL v3 wire protocol, so the real `psql` binary connects straight to the engine:

```bash
cargo run --release --bin picklejar-pg -- --database mydb.db --port 5433
psql -h 127.0.0.1 -p 5433 -U postgres
```

Both the simple and the extended query protocol (server-side prepared statements with `$N` parameters) are implemented and verified against `psql` 18.

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
| [docs/quickstart.md](docs/quickstart.md) | run the server and store/recall memories in five minutes (psql, any driver, or the Python client) |
| [docs/why-from-scratch.md](docs/why-from-scratch.md) | why this is not just Postgres + pgvector, the objection answered honestly |
| [docs/gallery.md](docs/gallery.md) | every runnable demo and hand-built primitive, grouped |
| [docs/ROADMAP.md](docs/ROADMAP.md) | what is built, and where the frontier is |
| [docs/design.md](docs/design.md) | every design decision, with the alternatives considered and rejected |
| [docs/FEATURES.md](docs/FEATURES.md) | the complete SQL surface and engine features |
| [docs/sprints.md](docs/sprints.md) | how the build was sequenced |

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
