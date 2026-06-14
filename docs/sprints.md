# Sprint Plan - rustdb capstone

Each sprint = ~1 calendar week, ~9-10 hours of work. Sprint label aligns with the proposal timeline. Each sprint maps to a milestone on GitHub.

| Sprint | Weeks | Theme | Definition of done |
|---|---|---|---|
| 0 | (current) | Bootstrap | Workspace, CI, design doc, this file, first issues filed. |
| 1 | W3 | Storage I: pages & file manager | Read/write 8 KiB pages with header + checksum, slotted-page tuple layout, unit tests. |
| 2 | W4 | Storage II: buffer pool & B+ tree | LRU-K buffer pool with RAII pins, B+ tree insert/search/range-scan over the page manager. |
| 3 | W5 | WAL I: log records & write path | Append-only WAL with checksummed records, fsync-on-commit, LSN ordering. |
| 4 | W6 | WAL II: ARIES recovery | Analysis + redo + undo phases. Crash-restart torture test green. |
| 5 | W7 | Transactions & lock manager | Begin/commit/abort, lock manager for DDL + unique indexes. |
| 6 | W8 | MVCC + isolation | Snapshot isolation, xmin/xmax visibility, version chains, isolation level enum. |
| 7 | W9 | SQL parser | Lexer + recursive-descent parser for the target subset. AST + display impl. |
| 8 | W10 | Planner + cost model | Logical plan, predicate pushdown, physical plan, EXPLAIN output. |
| 9 | W11 | Executor | Volcano iterators: SeqScan, IndexScan, Filter, Project, NestedLoopJoin, HashJoin, Aggregate. CLI wired end-to-end. |
| 10 | W12 | Torture test + polish | Multi-hour crash-restart torture test, EXPLAIN polish, isolation-level wiring, bug-fix pass. |
| 11 | W13 | Demo + write-up + SPED talk | Demo script, recorded video, README polish, presentation slides. |

## Sprint 0 - Bootstrap ✅ shipped

- [x] PR #1: Cargo workspace + 8 crate stubs + design doc + sprint plan
- [x] `cargo build / fmt / clippy / test` pass locally on all PRs
- [x] Sprint 1 milestone + 5 issues filed (#2-#6)
- [ ] CI workflow file pushed to repo - **pending**, blocked on `workflow` OAuth scope; content stashed at `C:\Users\natha\AppData\Local\Temp\ci.yml.pending`

## Sprint 1 - Storage I ✅ shipped (5/5 issues)

- [x] [#2](https://github.com/Nathan7108/capstone/issues/2) - file manager (PR #7)
- [x] [#3](https://github.com/Nathan7108/capstone/issues/3) - page header + CRC32 (PR #8)
- [x] [#4](https://github.com/Nathan7108/capstone/issues/4) - slotted page (PR #9)
- [x] [#5](https://github.com/Nathan7108/capstone/issues/5) - proptests (PR #10)
- [x] [#6](https://github.com/Nathan7108/capstone/issues/6) - design.md doc lockdown (this PR)

**Counts**: 45 tests (39 unit + 6 proptests), ~1500 LOC, all gates clean locally.

## Sprint 2 - Storage II ✅ shipped (5/5 issues)

- [x] [#12](https://github.com/Nathan7108/capstone/issues/12) buffer pool with LRU-K (PR #17)
- [x] [#13](https://github.com/Nathan7108/capstone/issues/13) B+ tree internal node (PR #18)
- [x] [#14](https://github.com/Nathan7108/capstone/issues/14) B+ tree leaf node (PR #19)
- [x] [#15](https://github.com/Nathan7108/capstone/issues/15) B+ tree insert/search/range_scan (PR #20)
- [x] [#16](https://github.com/Nathan7108/capstone/issues/16) B+ tree proptests (PR #21)

## Sprint 3 - WAL ✅ shipped (5/5 issues)

- [x] [#22](https://github.com/Nathan7108/capstone/issues/22) record format + serialization (PR #27)
- [x] [#23](https://github.com/Nathan7108/capstone/issues/23) writer (PR #28)
- [x] [#24](https://github.com/Nathan7108/capstone/issues/24) forward reader (PR #29)
- [x] [#25](https://github.com/Nathan7108/capstone/issues/25) buffer pool integration (PR #30)
- [x] [#26](https://github.com/Nathan7108/capstone/issues/26) proptests + doc lockdown (this PR)

## Sprint 4 - ARIES recovery (next)

Theme: crash recovery. Take the WAL, replay it on startup, restore the database to a state consistent with everything committed before the crash.

Issues to file at sprint start:
- Analysis pass: scan from last checkpoint, build transaction table and dirty page table
- Redo pass: replay every log record forward, conditional on page LSN
- Undo pass: walk per-transaction prev_lsn chains backward, write compensation log records
- Checkpoint emission and use
- Torture test: kill the process at random WAL offsets, restart, verify consistency
- Sprint 4 doc lockdown

## Out of scope until decided

- **Web UI / "supabase-like" admin interface.** Live decision: build the core engine + CLI first (all must-haves), then add an HTTP API + web UI as Phase 2 polish for demo wow factor. Logged as future scope; not blocking Sprint 0-10.
- **JOIN beyond inner + left.** Right/full outer joins deferred unless time permits.
- **Distributed replication.** Out of scope for capstone.
