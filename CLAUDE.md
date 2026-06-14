# Capstone — Relational Database from Scratch (Rust)

CSE 499 senior project. Build a real disk-based relational database engine in Rust with ACID guarantees, WAL+ARIES recovery, MVCC, SQL parser, cost-based planner, and query executor.

**Owner:** Nathan Luckock, `nathanluckock@gmail.com`
**Repo:** https://github.com/nathan-luckock/capstone
**Budget:** ~120 hrs over 13 weeks (W1-13)

---

## How to work in this repo (rules for Claude)

1. **Feature branch per task → PR → merge.** Never push directly to `main`. Branch names: `feat/<sprint>-<short-slug>`, `fix/<short-slug>`, `docs/<short-slug>`.
2. **Every commit ships with a `Design notes:` section.** Brief, but real. What we picked, what we rejected, why. This is Nathan's live-defense ammo when his prof asks "why a fanout of 128?"
3. **Checkpoint before architectural decisions.** Don't silently pick between e.g. B+ tree vs LSM, MVCC snapshot vs lock-based, ARIES vs simpler shadow-paging. Surface the tradeoff, get Nathan to call it, then implement.
4. **Tests first or alongside, not after.** Every module gets unit tests. The WAL + recovery layer gets a crash-recovery torture test that's run in CI.
5. **No silent scope creep.** If a task grows beyond its issue description, comment on the issue, don't just expand the diff.
6. **No dependencies for the graded must-haves.** No `sqlparser-rs`, no `sled`, no `rocksdb`. We're building the engine from scratch — that's the entire point. Standard library + `tracing` + `clap` + `serde` are fine for plumbing. Anything storage/parser/planner-related is hand-written.

---

## Architecture (target)

```
   ┌──────────────────────────────────────────────────────┐
   │                       CLI (psql-like)                │
   │                  crate: rustdb-cli                   │
   └───────────────────────────┬──────────────────────────┘
                               │
   ┌───────────────────────────▼──────────────────────────┐
   │          SQL parser  →  planner  →  executor         │
   │         crates: sql, planner, executor               │
   └───────────────────────────┬──────────────────────────┘
                               │
   ┌───────────────────────────▼──────────────────────────┐
   │     transaction manager + MVCC + lock manager        │
   │              crate: txn                              │
   └───────────────────────────┬──────────────────────────┘
                               │
   ┌───────────────────────────▼──────────────────────────┐
   │           WAL  +  ARIES recovery manager             │
   │              crate: wal                              │
   └───────────────────────────┬──────────────────────────┘
                               │
   ┌───────────────────────────▼──────────────────────────┐
   │    buffer pool  +  page manager  +  B+ tree          │
   │              crate: storage                          │
   └──────────────────────────────────────────────────────┘
```

All crates live in a single Cargo workspace.

---

## State log (running progress)

> **Update this section as a running heartbeat.** Top entry = most recent. Keep terse — one line per shipped task.

- _(Sprint 8, 2026-06-14)_ **Sprint 8 done. Cost-based planner shipped (M6, issues #68-72, PRs #73-77).** `rustdb-planner`, no external deps. Catalog (schema + row/distinct stats), binder (name resolution + single-table predicate pushdown) emitting a logical relational-algebra tree, cost model (equality sel = 1/distinct floored at 1e-6, range = 0.3, AND/OR/NOT composition; seq cost = rows, index cost = log2(rows+1) + sel*rows), and a physical plan that makes the two M6 choices: `choose_scan` (SeqScan vs IndexScan, fusing a Filter-over-Scan into the access path) and `choose_join` (NestedLoopJoin `L*R` vs HashJoin `L+R` for equi-joins). `EXPLAIN <stmt>` is a new parser keyword (`Statement::Explain`); `explain.rs` renders the cost-annotated tree (e.g. `IndexScan parts USING idx_id  (rows=1 cost=11.0)`). End-to-end tests (point lookup uses index, full scan, hash join, grouped/ordered/limited stack) with full EXPLAIN snapshots, plus a proptest pinning the invariant `chosen_scan.cost <= seq_scan_cost(rows)` for any catalog+predicate (400 cases). 46 planner tests. Next: Sprint 9 executor (Volcano iterators) + CLI wiring (M1). NOTE: GitHub username is now `nathan-luckock` (was slatefile); capstone remote updated. C:\Users\natha is a SEPARATE repo (Nathan7108/Java) - run capstone git from /c/Users/natha/capstone only.
- _(Sprint 5, 2026-06-07)_ **Sprint 5 done.** MVCC proptests + design lockdown (issue #48). Two oracle-checked property tests (table matches a committed-state `HashMap` after arbitrary commit/abort workloads; RepeatableRead view is byte-stable under concurrent commits). The proptests caught two real bugs, now fixed: (1) update/delete must stamp the version *visible to the writer*, not the index head (the head can be a dead aborted version); (2) insert must chain onto an existing head, not orphan the chain with prev=None. design.md MVCC section locked; isolation levels (#47) also shipped. ~210 tests. Next: Sprint 6 (MVCC polish: write-write conflict detection, version GC) or Sprint 7 (SQL parser).
- _(Sprint 5, 2026-06-07)_ **MvccTable landed (issue #46) — M5 demo works.** `rustdb-txn::mvcc::MvccTable` with `create`/`insert`/`update`/`delete`/`get`. B+ tree index (key→newest version ref), versions chained newest-to-oldest in heap pages via `Version` codec, reads walk the chain honoring the snapshot. Writes WAL-logged. The headline test: reader A keeps seeing "v1" while B commits "v2" concurrently, new txn C sees "v2", A never blocks B. Added `BTree::upsert` + `LeafPage::update_value` (in-place value overwrite) to storage. Also landed #44 visibility (PR #50) and #45 version codec (PR #51). 203 tests. Remaining Sprint 5: isolation levels (#47), proptests + design lockdown (#48).
- _(Sprint 7, 2026-06-07)_ **Sprint 7 done. SQL parser shipped (issues #56-61, PRs #62-67).** Hand-written `rustdb-sql`: lexer (spans, case-insensitive keywords, `''` string escape, line comments), Pratt expression parser (OR<AND<NOT<cmp<+-<*/<unary, NOT handled correctly), statement parsers for CREATE TABLE/DROP TABLE/CREATE INDEX, INSERT/UPDATE/DELETE, and SELECT with projections/aliases/FROM/INNER+LEFT JOIN/WHERE/GROUP BY/ORDER BY/LIMIT. Every AST node has a canonical `Display`; the round-trip proptest proves `parse(print(ast)) == ast` for generated ASTs. Parser is schema-free (semantic checks belong to the planner). 60 sql tests (58 unit + 2 proptests), ~262 total. Next: Sprint 8 cost-based planner (M6), then Sprint 9 executor + CLI (M1).
- _(Sprint 5, 2026-06-07)_ TransactionManager landed (issue #43). `rustdb-txn::manager`: monotonic xid allocation (from 1), status table (Active/Committed/Aborted), `begin`/`begin_with`/`commit`/`abort`/`state`/`current_snapshot`. Txid-based `Snapshot {xmin, xmax, active}` (Postgres-style) captured at begin; `committed_in_past` helper. `Transaction` holds its snapshot in a RefCell (for ReadCommitted refresh later) + `IsolationLevel` (RepeatableRead default). Interior-mutable `&self` like BufferPool. 9 tests, 176 total. Next: MVCC visibility rules (#44).
- _(Sprint 4, 2026-06-07)_ **Sprint 4 done.** Forced-kill torture test landed (issue #36). `crash_harness` binary commits rows forever and logs each durable commit to a ground-truth file; `tests/torture.rs` spawns it, hard-kills it (`Child::kill` = TerminateProcess/SIGKILL) over 4 rounds, recovers, and asserts every committed row survives + recovery is idempotent. design.md recovery section locked to the implemented analysis/redo/undo + CLR scheme; Sprint 4 open questions resolved. 167 tests total. **This is the graded live-demo requirement, working.** Next: Sprint 5, transactions + MVCC.
- _(Sprint 4, 2026-06-07)_ RecoveryManager + MiniHeap workload landed (issue #35). `recover(pool, wal_path) -> RecoveryStats` runs analyze→redo→undo→flush_all. `MiniHeap` (in `wal::workload`) is the recoverable workload harness: begin/insert/update/delete/commit/abort, logs WAL-before-page, fsyncs every record, stamps page LSN. Uniformly `&self` via `Cell` like `BufferPool`. Integration test (`recovery_integration.rs`): drive workload, drop pool without flush (simulated crash), recover, committed survive + uncommitted rolled back, incl. a 500-row multi-page run. 3 integration tests, 165 total.
- _(Sprint 4, 2026-06-06)_ Recovery undo landed (issue #34). `undo` walks each loser's `prev_lsn` chain backward, reverts every Update via its before-image, and appends a CLR (`undo_next` chained) per revert plus a terminating Abort, all fsync'd. CLRs encountered from a prior crashed undo are skipped to their `undo_next`; a loser already ending in Abort is skipped entirely. Full recovery (redo+undo) is idempotent across repeated runs. 3 new tests, 162 total.
- _(Sprint 4, 2026-06-06)_ Recovery analysis + redo landed (issue #33). `wal::recovery` with `analyze` (rebuilds txn table, winners vs losers) and `redo` (replays Update + CLR after/undo images, gated on page LSN for idempotency, materializes missing pages via the new `BufferPool::ensure_allocated`). Added `HeapPage::recover_slot` (apply image at a chosen slot id; empty image = tombstone) so replaying logged inserts in LSN order reproduces the original slot assignment. 10 new tests (5 storage recover_slot, 5 recovery), 159 total.
- _(Sprint 4, 2026-06-06)_ Checkpoint + CLR record payloads landed (issue #32). `Checkpoint{active_txns}` and `Clr{page_id,slot_id,undo_image,undo_next}` now serialize and parse. CLRs are redo-only; `undo_next` chains the undo for idempotent rollback across repeated crashes. 11 new tests.
- _(Sprint 3, 2026-06-06)_ WAL writer landed (issue #23). `WalWriter` with `open`/`append`/`fsync_through`/`fsync_all`/`current_lsn`/`durable_through`. Buffered append (records sit in memory until fsync), monotonic LSN allocation starting at 1, scan-on-reopen recovers `next_lsn` from the last complete record on disk, torn tail at EOF is silently skipped. 11 new tests, 122 total (112 unit + 10 proptests).
- _(Sprint 3, 2026-06-06)_ WAL record format landed (issue #22). `LogRecord` enum (Begin, Update, Commit, Abort, Checkpoint+Clr stubbed for Sprint 4). 29-byte header (length, type, lsn, txn_id, prev_lsn) + per-type payload + CRC32 trailer. `Lsn` and `TxnId` newtypes with `Lsn::INVALID = u64::MAX` sentinel. `WalError` variants for short records, checksum mismatch, unknown types, truncated payloads, tail truncation. 12 new tests, 111 total (101 unit + 10 proptests).
- _(Sprint 2, 2026-06-06)_ **Sprint 2 done.** B+ tree proptests landed (issue #16). Buffer pool ops sequences preserve pin invariants and content. B+ tree sibling chain is sorted after arbitrary inserts. Search matches a `BTreeMap` oracle for arbitrary queries. Range scan matches the oracle's range filter. 99 tests total (89 unit + 10 proptests). Sprint 3 next: WAL.
- _(Sprint 2, 2026-06-06)_ Storage II, B+ tree ops landed (issue #15). `BTree<'pool>` with `create`/`open`/`search`/`insert`/`range_scan`/`root_page`. Insert walks root to leaf, splits leaves and internal nodes with promote-middle-key semantics, allocates a new root when the existing root splits. Range scan walks sibling pointers, supports `Bound::Included`/`Bound::Excluded`/`Bound::Unbounded` on both ends. Adds private `InternalView`/`LeafView` types for read-only access from `&Page`. 12 new tests, 95 total (89 unit + 6 proptests).
- _(Sprint 2, 2026-06-06)_ Storage II, B+ tree leaf node landed (issue #14). `LeafPage<'a>` with `init`/`from_bytes`/`find_key`/`insert`/`delete`/`next_leaf`/`set_next_leaf`. `TupleRef { page_id, slot_id }` carries the heap reference. `PageId::INVALID` sentinel for the right-most leaf. Packed 18-byte entries give 453 keys per leaf. 14 new tests, 83 total (77 unit + 6 proptests).
- _(Sprint 2, 2026-06-06)_ Storage II, B+ tree internal node landed (issue #13). `InternalPage<'a>` over `&mut Page`, BTreeInternal page type, layout is `key_count u16 + first_child PageId + entries[(key u64, right_child PageId)]`. Sorted entries, binary-search `find_child`, `insert` keeps order and rejects duplicates. Fanout 509 keys (510 children) for 8 KiB pages, locked in `MAX_INTERNAL_KEYS`. 11 new tests, 69 total (63 unit + 6 proptests).
- _(Sprint 2, 2026-05-23)_ Storage II, buffer pool landed (issue #12). `BufferPool` over `FileManager` with LRU-K (K=2) replacement, RAII `PageReadGuard`/`PageWriteGuard` (pin on construct, unpin on Drop, write guard marks dirty). Frame metadata (`pin_count`, `history`) in `Cell`s so the pool can poll without conflicting with held guards; page bytes in `RefCell<FrameInner>`. Dirty pages flushed before eviction. `flush_page`/`flush_all` with single fsync. 13 new tests; 58 total (52 unit + 6 proptests).
- _(Sprint 1, 2026-05-23)_ **Sprint 1 done.** Doc lockdown (issue #6) — `docs/design.md` reflects implemented decisions (file manager API, slot ID stability, CRC32 polynomial + scope, tombstone encoding). Resolved questions moved out of Open Questions; new ones added for Sprint 2 (buffer pool replacement, free-space tracking, B+ tree fanout). `docs/sprints.md` records final Sprint 1 status. Next: Sprint 2 — buffer pool + B+ tree.
- _(Sprint 1, 2026-05-23)_ Storage I — property tests landed (issue #5): `proptest` dev-dep, `crates/storage/tests/proptests.rs` covers header round-trip, full checksum bit-flip sweep over all 8 KiB, insert error-or-roundtrip, arbitrary insert/delete/compact op sequences against an oracle, file manager durability across reopen. 45 tests total (39 unit + 6 proptests).
- _(Sprint 1, 2026-05-23)_ Storage I — slotted-page layout landed (issue #4): `HeapPage` with `init`/`from_bytes`/`insert`/`get`/`delete`/`compact`/`free_space`/`tuple_count`. `SlotId(u16)`, stable across deletes (never recycled). Tombstone = slot length 0; `compact` reclaims the bytes. `FLAG_NEEDS_VACUUM` hint at 1 KiB of tombstones. 39/39 storage tests green.
- _(Sprint 1, 2026-05-22)_ Storage I — page header + CRC32 landed (issue #3): 24-byte header (`lsn`, `checksum`, `page_type`, `slot_count`, `free_space_ptr`, `flags`, `reserved`), hand-written CRC32 (IEEE polynomial, const-table-generated, matches the standard 0xCBF43926 test vector), checksum scope = `[12..PAGE_SIZE]` so LSN updates don't invalidate it. 22/22 storage tests green.
- _(Sprint 1, 2026-05-22)_ Storage I — file manager landed (issue #2): `PageId`, `PAGE_SIZE = 8192`, `FileManager` with `open` / `allocate_page` / `read_page` / `write_page` / `fsync`. 8/8 unit tests green. CI gates clean.
- _(pre-Sprint-1, 2026-05-22)_ Bootstrap (PR #1, merged): Rust 1.95.0 installed, `.claude/skills/db-debug.md` added, `.mcp.json` registers GitHub MCP server, CLAUDE.md + design.md + sprints.md drafted, 8-crate workspace, Sprint 1 milestone + 5 issues filed (#2–#6).

---

## Page format (canonical, update as it changes)

> Single source of truth for the on-disk layout.

- Page size: **8192 bytes (8 KiB).** Defined as `rustdb_storage::PAGE_SIZE`.
- Page ID: **`u64`, 0-indexed**, byte offset = `page_id * PAGE_SIZE`. See `rustdb_storage::PageId`.
- Page header layout: **24 bytes** — see [docs/design.md](docs/design.md#slotted-page-format-heap-tables). Implementation lands in Sprint 1 issue #3.

---

## Invariants

- **WAL ordering:** No dirty page is flushed to disk before its log record is fsync'd.
- **Pin/unpin balance:** Every `PageGuard` drop unpins exactly once. No bare `pin`/`unpin` calls in user code.
- **No torn writes:** All page writes are full-page or recoverable from WAL.
- **MVCC snapshot stability:** A transaction's read snapshot doesn't shift mid-transaction.

---

## Reference material (read these, don't just cite them)

- ARIES paper (Mohan et al., 1992) — for WAL + recovery
- CMU 15-445 lectures (Pavlo) — for storage, buffer pool, indexes, joins
- CMU 15-721 lectures — for MVCC, query optimization
- "Database Internals" (Petrov) — pragmatic reference for layout choices

---

## Local scripts / Make targets

> Fill in as we add them. Goal: `cargo test`, `cargo bench`, `cargo run --bin rustdb` should all just work.
