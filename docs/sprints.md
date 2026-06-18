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
| 15 | Whole-footprint radiation (heap, WAL, and checksummed metadata sidecars); replication and point-in-time recovery, model-checking the recovery and isolation invariants | In progress |

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
4. Next: the radiation model now corrupts every persistent file (heap, WAL, and
   the checksummed metadata sidecars), so what remains is replication and
   point-in-time recovery, and model-checking the recovery and isolation
   invariants; then the deployment story for unreachable infrastructure.

## Out of scope

- Distributed replication and point-in-time recovery (candidate future work,
  built on the existing WAL).
- TLS on the wire (SCRAM authentication is in; transport encryption is the next
  hardening step before exposing the server publicly).
