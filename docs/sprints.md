<div align="center">

# picklejar build log

How the engine was sequenced, sprint by sprint.

[Overview](../README.md) &nbsp;·&nbsp; [Design](design.md) &nbsp;·&nbsp; [Features](FEATURES.md)

</div>

---

Each sprint is roughly one week. Every sprint maps to a GitHub milestone, and
every task to a pull request that is squash-merged once the checks
(`cargo fmt`, `cargo clippy -D warnings`, `cargo test`) pass.

## Plan

| Sprint | Theme | Status |
|---|---|---|
| 0 | Bootstrap: workspace, design doc, sprint plan | Shipped |
| 1 | Storage I: pages, file manager, slotted layout, CRC32 | Shipped |
| 2 | Storage II: buffer pool (LRU-K), B+ tree | Shipped |
| 3 | WAL: record format, writer, reader, write path | Shipped |
| 4 | ARIES recovery: analysis, redo, undo, forced-kill torture test | Shipped |
| 5 | Transactions and MVCC: snapshots, visibility, versions, isolation levels | Shipped |
| 6 | MVCC polish: write-write conflict detection, version GC | Deferred |
| 7 | SQL parser: lexer, Pratt expressions, DDL, DML, SELECT | Shipped |
| 8 | Cost-based planner: catalog, logical and physical plan, EXPLAIN | Shipped |
| 9 | Executor and CLI: row codec, Volcano operators, joins, aggregates, persistence | Shipped |
| 10 | Deepen the engine: full DML, constraints, the everyday type system, sequences, views, subqueries, window functions, set operations, CTEs | Shipped |
| 11 | Make it real: PostgreSQL wire protocol, deterministic and differential testing, multi-connection concurrency, VACUUM | Shipped |
| 12 | Security: roles, GRANT/REVOKE, ownership, SCRAM auth, row-level security | Shipped |
| 13 | AI memory layer: VECTOR type, distance operators + KNN, RLS-filtered similarity, the vecsim simulator, HNSW index | Shipped |
| 14 | Reliability for unreachable hardware: HNSW wired into SQL with a cached, RLS-safe index; corruption detection and self-healing; the metamorphic oracle; the `vecert` certificate; the orbital radiation fault model in the live simulator | Shipped |
| 15 | Mass-efficient self-healing: from-scratch Reed-Solomon erasure coding; a self-healing block store; whole-footprint radiation (heap, WAL, sidecars); the live heap reconstructing corrupt pages from parity on `open_resilient` | Shipped |
| 16 | Operability of self-healing (PROTECT statement, pjscrub, pg_fault_log); snapshot backup and replication (Database::backup, pjbackup) | Shipped |
| 17 | Model-checking the WAL-ordering and snapshot-isolation invariants from scratch (`walmodel`, the `txn` model, both certified in `vecert`) | Shipped |
| 18 | WAL-logging the catalog and row-level-security state so the log is authoritative for schema and isolation, with resilient fallback to the sidecar, all certified in `vecert` | Shipped |
| 19 | Model-checking, through the approximate index, both tenant isolation (a tenant's query never returns another tenant's row) and cache freshness (a query never returns a deleted row) from scratch (`rlsmodel`) | Shipped |

## What shipped, by sprint

### Sprint 0 - Bootstrap

Cargo workspace with eight crates, the design document, this plan, and the
working agreement. A CI workflow (`.github/workflows/ci.yml`) runs fmt, clippy,
and the test suite on push and pull request.

### Sprint 1 - Storage I

8 KiB pages with a 24-byte header and a hand-checked CRC32, a slotted-page heap
layout, and the file manager. Property tests cover the page format.

### Sprint 2 - Storage II

An LRU-K (K=2) buffer pool with RAII pin guards and a WAL-ordered flush path,
and a B+ tree (fanout 509 internal, 453 leaf) with insert, search, upsert,
delete, and range scan over the pool. Property tests against an oracle.

### Sprint 3 - WAL

An append-only write-ahead log: checksummed records, a writer with
fsync-through, and a forward reader, integrated with the buffer pool so pages
never reach disk ahead of their log records.

### Sprint 4 - ARIES recovery

Three-phase recovery (analysis, redo gated on the page LSN, undo with
compensation log records) plus a forced-kill torture test: a child process is
hard-killed mid-write, then recovery proves no committed row is lost. This is
the headline crash-safety evidence.

### Sprint 5 - Transactions and MVCC

A transaction manager with txid-based snapshots (Postgres-style
xmin/xmax/active set), the visibility rule, version chains, the `MvccTable`
key/value store, and RepeatableRead and ReadCommitted isolation. Oracle
property tests, including a stable snapshot under concurrent commits.

### Sprint 6 - MVCC polish (deferred)

Write-write conflict detection and version garbage collection. Not observable
through the single-connection interface yet, so deferred until concurrent
connections land.

### Sprint 7 - SQL parser

A lexer with spans, a Pratt expression parser, and recursive-descent statement
parsers for DDL, DML, and SELECT (joins, GROUP BY, ORDER BY, LIMIT). Every AST
node prints back to canonical SQL; a round-trip property test proves
`parse(print(ast)) == ast`.

### Sprint 8 - Cost-based planner

An in-memory catalog with statistics, a binder that emits a logical plan, a
cost model (selectivity, scan and join costs), and a physical plan that makes
the seq-vs-index and hash-vs-loop choices. `EXPLAIN` renders the cost-annotated
tree. A property test pins that the chosen scan never costs more than a full
scan.

### Sprint 9 - Executor and CLI

The row codec, a snapshot-consistent table scan, the engine that wires every
layer together, and Volcano operators (seq scan, filter, project, sort, limit,
nested-loop join, group-by aggregate) with a three-valued-logic expression
evaluator. The psql-style CLI runs CREATE / INSERT / SELECT / JOIN / GROUP BY /
EXPLAIN, and the schema and data persist across a restart.

### Sprint 10 - Deepen the engine

Making it behave like a real database: full DML (`INSERT` / `UPDATE` /
`DELETE`), explicit transactions (`BEGIN` / `COMMIT` / `ROLLBACK`), the
constraint set (`PRIMARY KEY`, `UNIQUE`, `NOT NULL`, `CHECK`, `FOREIGN KEY`),
sequences and `SERIAL`, `RETURNING`, `ON CONFLICT` upserts, views, derived
tables and correlated subqueries, window functions, `UNION` / `INTERSECT` /
`EXCEPT`, CTEs (including `WITH RECURSIVE`), the `information_schema`, and the
full everyday type system (`BOOL`, `FLOAT`, `DATE`, `TIMESTAMP`, `JSON`,
`DECIMAL`) with casts and the supporting functions.

### Sprint 11 - Make it real

Proving and exposing the engine: the PostgreSQL v3 wire protocol (simple and
extended) so real clients connect over TCP, deterministic simulation testing
and differential testing against SQLite, multi-connection concurrency via an
engine actor with transaction exclusivity, and `VACUUM` for MVCC space
reclamation.

### Sprint 12 - Security

Roles and privileges (`CREATE ROLE` / `CREATE USER` with attributes, `GRANT` /
`REVOKE`, ownership, role membership, `SET ROLE`), SCRAM-SHA-256 authentication
on the wire (the SHA-256, HMAC, and PBKDF2 primitives in-tree), and row-level
security (`CREATE POLICY` with `USING` and `WITH CHECK`, enforced in the engine).
All of it persists across a restart.

### Sprint 13 - AI memory layer

The pivot, in code (see [Mission and direction](design.md#mission-and-direction)
and [The vector memory layer](design.md#the-vector-memory-layer)): a native
`VECTOR(n)` type, the four distance operators (`<->`, `<=>`, `<#>`, `<+>`) and
their function forms, brute-force nearest-neighbor search, row-level-security-
filtered similarity (a tenant's KNN can only ever rank its own vectors), an HNSW
index (four metrics, insert/search/delete, durable), and the `vecsim` simulator
that proves durability and isolation of the memory layer together under crash.
Durability is verified at 100,000 deterministic crash simulations.

### Sprint 14 - Reliability for unreachable hardware

The HNSW index reachable from SQL through a cached, write-invalidated, RLS-safe
path (about 150x on a warm query, and structurally unable to widen what a tenant
sees); end-to-end corruption detection, where every page and serialized index
carries a CRC32 refused on read; a self-healing redundant index; the metamorphic
oracle for approximate search; the `vecert` certificate; and the orbital radiation
fault model injected into the live simulator, irradiating a committed multi-tenant
workload at a named orbit's single-event-upset rate and proving it is never served
silently corrupted.

### Sprint 15 - Mass-efficient self-healing

A from-scratch Reed-Solomon erasure code over GF(2^8), a self-healing block store
(detect, log, repair, heal), and whole-footprint radiation across the heap, WAL,
and the now-checksummed metadata sidecars. The live heap reconstructs its corrupt
pages from parity on `open_resilient`, at `m/k` overhead instead of the `+m*100%`
of redundant copies.

### Sprint 16 - Operability, backup, and replication

The `PROTECT` statement, a durable fault log surfaced as `pg_fault_log`, and the
`pjscrub` cron scrubber that heals and refreshes parity on a cadence. Snapshot
backup (`Database::backup`, `pjbackup`) writes a consistent copy, healing first,
and a physical standby replica streams the WAL and stays caught up.

### Sprint 17 - Model-checking the core invariants

From-scratch bounded model checkers for the write-ahead-logging ordering invariant
(`walmodel`, the `wal` model) and the MVCC snapshot read-stability invariant (the
`txn` model). Each enumerates every reachable interleaving of an abstract machine,
and each ships a deliberately buggy variant that yields a concrete counterexample,
so the proofs are not vacuous. Both are certified in `vecert`.

### Sprint 18 - WAL-logged catalog and isolation for log-streamed recovery

The catalog and the row-level-security state are now each written to the WAL as a
snapshot record after every schema or policy change, and replayed on open, so the
log is authoritative for both: a change that reached the log is recovered even if
its sidecar write was lost in a crash, and forward replay reconstructs later
schema and policy state rather than only the base state. For isolation this is a
security property, not just durability: a crash can never silently drop a tenant
fence and leak one tenant's rows to another. The records carry a sentinel
transaction and sit outside the redo/undo chain (analysis skips them), so they can
never be mistaken for an uncommitted loser. Bounding the replay to a chosen LSN
reconstructs the schema and policy state as of that point.

### Sprint 19 - Model-checking RLS-filtered retrieval

The memory layer's central promise, proved exhaustively: a tenant's query,
accelerated by the cached approximate index or served exactly, can never return
another tenant's row. A from-scratch bounded model checker (`rlsmodel`, the
`isolation_model`) enumerates every reachable interleaving of inserts, cache
invalidations, role switches, policy changes, index builds, and queries, and
proves no query returns a row the caller's policy forbids; a deliberately buggy
dispatch that serves the index under an active policy is caught with a concrete
cross-tenant counterexample, so the proof is not vacuous. This is the sharpest
piece of open ground in the research: no vector or AI-memory database is known to
model-check its filtered-retrieval isolation. Certified in `vecert`.

### Sprint 20 - Valid-time travel

A memory of record should answer not only what an agent knows now but what it
knew at a past instant. A session as-of instant, `SET valid_time = TIMESTAMP
'...'`, rewinds every read in the session: a table that carries `valid_from` and
`valid_to` columns is treated as temporal, and the binder folds the half-open
validity predicate `valid_from <= t AND (valid_to IS NULL OR t < valid_to)` into
its reads, so a query returns exactly the rows valid then, a `NULL` upper bound
being the still-current row. The instant rides the same parser-safe `SET`
mechanism as the index toggle, which is what sidesteps the `AS OF` collision with
table-alias parsing; `SET valid_time = off` / `RESET valid_time` returns to the
present. Travel is a read concept, so writes still act on the latest state, and
the fold applies only to temporal tables, leaving ordinary tables read in full.
It composes with row-level security: the validity predicate is `AND`-ed after the
tenant fence, so time travel can never widen what a tenant sees.

The filter is then model-checked, a fifth proved invariant alongside the WAL,
snapshot, isolation, and freshness ones. Valid-time travel is a pure, row-local
predicate, so `valid_time_model` exhaustively sweeps every interval and every
instant over a bounded time domain and proves the binder's predicate returns a
row exactly when the half-open rule says it is valid. A deliberately buggy closed
upper bound (`t <= valid_to`), which would serve a row at the very instant it is
superseded, is caught with a concrete counterexample, so the boundary the rule
exists to get right is the boundary the proof pins. Certified in `vecert` and
swept by `rlsmodel`.

## Direction

A from-scratch engine that speaks PostgreSQL over the wire, turned toward a
specific mission: the durable, isolated memory layer for AI in environments that
cannot be physically serviced (orbital and edge data centers).

1. The relational engine is deep, crash-proven, and speaks Postgres (sprints
   0-11).
2. The memory layer is built on it: vectors, similarity search, engine-enforced
   isolation, and a fault simulator (sprints 12-13).
3. Reliability for unreachable hardware is built (sprint 14): the HNSW index is
   reachable from SQL through a cached, RLS-safe path (fast at scale while the
   row-level-security filter is preserved); corruption is detected and self-healed;
   a metamorphic oracle and the `vecert` certificate make the proof concrete; and
   the live simulator irradiates a committed workload at an orbit's upset rate and
   proves it is never served silently corrupted.
4. Mass-efficient self-healing, operability, recovery, and proof are built
   (sprints 15-17): Reed-Solomon erasure coding heals corrupt pages from parity on
   open; `PROTECT`, `pg_fault_log`, and `pjscrub` operate it; backup, point-in-time
   restore, and a physical standby replica round out recovery; and the core WAL and
   isolation invariants are model-checked exhaustively, not just sampled.
5. The catalog is now WAL-logged and replayed on open (sprint 18), so the log is
   authoritative for the schema and forward replay reconstructs later schema
   changes rather than only the base state. The recovery story is complete.

## Out of scope

- Multi-node distributed operation beyond a single physical standby: consensus,
  sharding, and partition reconciliation are company-scale work, not capstone work.
- Hardware-in-the-loop and radiation-beam testing: the fault model is simulated and
  parameterized by published upset rates, not measured on a flight part.
