# Sprint plan - rustdb

Each sprint is roughly one week. Every sprint maps to a GitHub milestone, and
every task to a pull request that is squash-merged once CI-equivalent checks
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
| 10 | Deepen the engine: transactions, constraints, more types, real indexes | In progress |
| 11 | Studio: HTTP API and web UI | Planned |
| 12 | Demo, write-up, presentation | Planned |

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

### Sprint 10 - Deepen the engine (in progress)

Making it behave like a real database:

- [x] Full DML: `INSERT` (with or without a column list), `UPDATE`, `DELETE`.
- [x] Explicit transactions: `BEGIN` / `COMMIT` / `ROLLBACK`, exposing MVCC.
- [x] Constraints: `PRIMARY KEY`, `UNIQUE`, `NOT NULL`, enforced.
- [ ] More column types (`BOOL`, `FLOAT`).
- [ ] Real secondary-index lookups (B+ tree duplicate keys).
- [ ] Concurrent connections with write-write conflict detection.

## Direction

The goal is a from-scratch engine that behaves like PostgreSQL, with a
Supabase-style studio on top:

1. Finish deepening the engine (Sprint 10).
2. Build the studio: an HTTP API over the engine and a web UI with a SQL
   editor, a results grid, a schema browser, a live cost-based plan
   visualizer, and a crash-recovery panel (Sprint 11).

## Out of scope

- Distributed replication.
- Right and full outer joins (inner and left are supported).
- Network protocol compatibility with PostgreSQL on the wire (the studio talks
  to the engine through the embedded API).
