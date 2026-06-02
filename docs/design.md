# rustdb — Design Document

> Working document. Updated as decisions are made. The goal is for any reviewer (and future-you) to be able to reconstruct **why** the engine is shaped the way it is from this single file.

## Goals

A relational database engine built from scratch with:
1. SQL interface (CREATE, INSERT, SELECT/WHERE, plus UPDATE, DELETE, JOIN, GROUP BY).
2. ACID transactions: durability via WAL, atomicity via ARIES-style undo, isolation via MVCC.
3. A cost-based query planner that picks between scan and join strategies using table statistics.
4. A live demo proving crash safety: forced kill mid-write, restart, no data loss.

Non-goals: distributed replication, network protocol compatibility with Postgres, performance parity with mature engines.

---

## Implementation status (running)

| Sprint | What | Status |
|---|---|---|
| 0 | Bootstrap: workspace, CI, design doc, working agreement | ✅ shipped (PR #1) |
| 1 | Storage I: file manager, page header + CRC32, slotted page, proptests | ✅ shipped (PRs #7–#10) |
| 2 | Storage II: buffer pool + B+ tree | ⏳ next |
| 3–4 | WAL + ARIES recovery | ⏳ |
| 5–6 | Transactions + MVCC | ⏳ |
| 7–9 | SQL parser → planner → executor | ⏳ |
| 10 | Torture test + polish | ⏳ |
| 11 | Demo + write-up + SPED talk | ⏳ |

---

## High-level architecture

```
        ┌──────────────────────────┐
        │     rustdb-cli (REPL)    │
        └────────────┬─────────────┘
                     │ rustdb::Database::query()
        ┌────────────▼─────────────┐
        │     executor (Volcano)   │
        └────────────┬─────────────┘
                     │ physical plan
        ┌────────────▼─────────────┐
        │    planner (cost-based)  │
        └────────────┬─────────────┘
                     │ logical plan
        ┌────────────▼─────────────┐
        │       sql (parser)       │
        └──────────────────────────┘

           ───── all of the above flow through ─────

        ┌──────────────────────────┐
        │   txn manager + MVCC     │
        └────────────┬─────────────┘
                     │ pin / read / write / log
        ┌────────────▼─────────────┐
        │       buffer pool        │
        └────────────┬─────────────┘
                     │
        ┌────────────▼─────────────┐
        │  page manager + B+ tree  │   ←── disk
        └──────────────────────────┘
        ┌──────────────────────────┐
        │  WAL  +  recovery mgr    │   ←── disk (separate file)
        └──────────────────────────┘
```

---

## Storage layer

### Page size

**Decision: 8 KiB.** Matches Postgres default. Big enough to amortize per-page overhead, small enough that buffer-pool memory ratio is reasonable.

Exposed as `rustdb_storage::PAGE_SIZE` (`usize`) and `PAGE_SIZE_U16` (typed mirror for `u16` arithmetic in the slot directory). A compile-time assertion in `page.rs` keeps them in sync.

### File manager (Sprint 1 — shipped)

`rustdb_storage::FileManager` owns the database file and exposes page-granular I/O. Single source of truth for raw page reads and writes; higher layers (buffer pool, WAL flush path) go through it.

```rust
impl FileManager {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self>;
    pub fn allocate_page(&mut self) -> Result<PageId>;
    pub fn read_page(&mut self, id: PageId, buf: &mut Page) -> Result<()>;
    pub fn write_page(&mut self, id: PageId, buf: &Page) -> Result<()>;
    pub fn fsync(&mut self) -> Result<()>;
    pub fn page_count(&self) -> u64;
}
```

**Decisions:**

- `&mut self` on read/write. Concurrent reads are the buffer pool's job (Sprint 2). Pushing positional pread/pwrite into the file layer would be premature optimization for the capstone's single-node scope.
- `allocate_page` uses `set_len` to extend the file. OS guarantees zero-fill for the new region; faster than a `seek + write_zeros` loop and identical semantics.
- File opened with `truncate(false)` AND `create(true)`. Explicit so a refactor doesn't accidentally start truncating live databases.
- `MisalignedFile` is a real error, not a panic. A wrong-page-size build or a half-written torture-test artifact needs to surface to the caller so they can decide to abort, repair, or ignore.
- **Rejected**: `O_DIRECT`. We want the OS page cache for the demo — cache effects are part of what the buffer pool buys.
- **Rejected**: positional I/O for now. Sprint 1 surface is small enough that seeking is fine; revisit if benchmarks show seek overhead matters.

### Page header (Sprint 1 — shipped)

Every page starts with a 24-byte header, little-endian:

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 8 | `lsn: u64` | Last LSN that touched this page. WAL ordering anchor. |
| 8 | 4 | `checksum: u32` | CRC32 of `[12..PAGE_SIZE]`. |
| 12 | 2 | `page_type: u16` | `Free` / `Heap` / `BTreeInternal` / `BTreeLeaf` / `Overflow`. |
| 14 | 2 | `slot_count: u16` | Live + tombstoned slots. |
| 16 | 2 | `free_space_ptr: u16` | Offset where the tuple region begins (tuples grow up toward lower offsets from here). |
| 18 | 2 | `flags: u16` | Bit 0 = `FLAG_DIRTY` (in-memory only), Bit 1 = `FLAG_NEEDS_VACUUM`. |
| 20 | 4 | `reserved: u32` | Zero on disk. Reserved for MVCC version-chain pointer (Sprint 6). |

**Decisions:**

- `page_type` is a `u16` even though we have <8 variants. Room for visibility map / free-space map page types later without breaking the binary format.
- `reserved: u32` exists specifically for the MVCC chain pointer. Reserving the space now means Sprint 6 doesn't force a layout migration.
- `FLAG_DIRTY` is in-memory only — the flush path clears it before write-back.

### Checksum

**Polynomial:** IEEE 802.3, reflected (`0xEDB88320`). The standard CRC32 used by gzip, Ethernet, and Postgres.

**Implementation:** hand-written in `crates/storage/src/crc32.rs`, ~30 lines with a compile-time `[u32; 256]` lookup table. Passes the IEEE test vector `crc32("123456789") == 0xCBF43926`.

**Decisions:**

- **Scope: `[12..PAGE_SIZE]`** — the page **excluding the LSN and the checksum field itself**. The LSN is updated on every WAL-acknowledged page write; including it in the checksum would force a recompute on every update. Excluding it still catches what the checksum is for: torn writes and silent bit-rot in the payload. Postgres makes the same tradeoff.
- **Hand-written over `crc32fast`.** Per [CLAUDE.md rule 6](../CLAUDE.md), storage-related code is from scratch. CRC32 is small enough that a from-scratch impl is justified by clarity alone — no dependency wins back enough complexity to matter.
- **Rejected**: SIMD-accelerated CRC32 (e.g. `crc32` intrinsic). The page checksum runs over 8 KiB at a time — not the hot path that benefits from intrinsics.
- **Rejected**: blake3 / xxHash. CRC32 catches accidental corruption (the documented threat model). Cryptographic strength isn't needed for a single-node DB.

### Slotted-page heap layout (Sprint 1 — shipped)

```
 0       24      ...                  free_space_ptr        PAGE_SIZE
 ┌───────┬─────────────────┬─────────┬──────────────────────────────┐
 │header │ slot directory →│  free   │ ← tuple data                 │
 └───────┴─────────────────┴─────────┴──────────────────────────────┘
```

Slot directory entry (4 bytes, little-endian): `(offset: u16, length: u16)`. Length 0 = tombstoned.

`rustdb_storage::HeapPage<'a>` wraps a `&'a mut Page` and exposes `init` / `from_bytes` / `insert` / `get` / `delete` / `compact` / `free_space` / `slot_count` / `tuple_count`.

**Decisions:**

- **Slot IDs are stable for the page's lifetime and never recycled.** Once a `SlotId` is assigned by an `insert`, it always refers to the same logical slot, even after `delete`. This is the foundation that secondary B+ tree indexes (Sprint 2) rely on. The alternative — reusing freed slot IDs on the next insert — saves 4 bytes per delete but silently breaks any external `(page_id, slot_id)` reference. Hard no.
- **Tombstone marker = slot length 0**, not a separate flag bit. Smaller (no extra bit per slot), simpler (one comparison in `get`, not two). Cost: empty tuples can't be stored. Schemas with zero-length columns will use NULL bitmaps; not a real loss.
- **`compact` is explicit, never implicit.** Inserts and reads never trigger compaction on their own. The buffer pool will decide when to schedule it based on the `FLAG_NEEDS_VACUUM` hint. Keeps the hot path predictable — no "this insert took 50ms because it compacted" surprises.
- **`FLAG_NEEDS_VACUUM` threshold: 1024 bytes** of tombstoned space. Arbitrary for the capstone — real systems use adaptive thresholds based on page utilization and access patterns.
- **`compact` allocates a small temp `Vec<(u16, Vec<u8>)>`.** Page-local, ~30 tuples max at typical sizes, not on the hot path. A zero-alloc in-place compaction is doable via overlapping copies but adds complexity unjustified for Sprint 1's budget.
- **Rejected**: storing tuples in slot order. Insertion order makes `compact` a simple "walk live slots, copy to end" pass. Keeping tuples sorted by slot ID would force a re-sort after every `compact`.

### B+ tree (Sprint 2 — planned)

Branching factor TBD (target ~128 for 8 KiB pages with `u64` keys). Internal node = sorted keys + child page IDs. Leaf node = sorted keys + tuple references `(PageId, SlotId)`. Sibling pointer in leaves for range scans.

### Buffer pool (Sprint 2 — planned)

LRU-K (K=2) replacement. Pin/unpin via RAII `PageGuard`. Pinned pages are evict-immune. Dirty bit set on first write through a guard.

---

## WAL & recovery (Sprints 3–4 — planned)

### Log record layout

Variable-length records, prefixed with length + type:

```
┌────────────────────────────────────────────────┐
│ length: u32                                    │
│ type: u8         (BEGIN, UPDATE, COMMIT, ABORT,│
│                   CHECKPOINT, CLR)             │
│ lsn: u64                                       │
│ txn_id: u64                                    │
│ prev_lsn: u64    (txn's previous record, for   │
│                   undo chain traversal)        │
│ payload: [u8]    (per-type)                    │
│ checksum: u32                                  │
└────────────────────────────────────────────────┘
```

### Three-phase recovery (ARIES)

1. **Analysis.** Scan from last checkpoint, rebuild the active transaction table + dirty page table.
2. **Redo.** Replay every log record from the earliest dirty-page recovery LSN forward, applying any update whose page LSN < record LSN.
3. **Undo.** For every transaction still active at crash time, walk back via `prev_lsn` and write compensation log records (CLRs).

### Invariant (WAL ordering)

A dirty page cannot be flushed before its corresponding log records are fsync'd. Enforced by the buffer pool's flush path: before write-back, look up the page's LSN, ensure WAL has fsync'd through that LSN.

---

## Transactions + MVCC (Sprints 5–6 — planned)

Snapshot isolation as the default. Each tuple carries `xmin` (creating txn) and `xmax` (deleting txn). A reader at snapshot S sees tuple T iff `xmin(T) ≤ S` and (`xmax(T)` is null or `xmax(T) > S`).

The page header's `reserved: u32` field is the version-chain pointer for older tuple versions; the current row lives at the regular slot.

Lock manager exists primarily for DDL and unique-index enforcement; reads under SI don't take row locks.

---

## SQL parser (Sprint 7 — planned)

Hand-written. Lexer produces a flat token stream; recursive-descent parser produces an AST. Pratt-style precedence for expressions.

Target subset:
- DDL: `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`.
- DML: `INSERT`, `UPDATE`, `DELETE`.
- Query: `SELECT` with `WHERE`, `GROUP BY`, `ORDER BY`, `LIMIT`, `JOIN` (inner + left).

---

## Planner (Sprint 8 — planned)

1. **Logical plan.** AST → relational algebra tree (Scan, Filter, Project, Join, Aggregate, Sort).
2. **Logical rewrites.** Predicate pushdown, projection pushdown, constant folding.
3. **Physical plan.** Choose between SeqScan vs IndexScan per relation; choose between NestedLoopJoin vs HashJoin per join. Costs from per-table stats (row count, NDV, min/max per column).
4. **`EXPLAIN`** output: pretty-printed plan tree with per-node estimated cost.

---

## Testing strategy

- **Unit tests** in each module. Fast (`cargo test --lib` runs in <50ms today).
- **Property tests** via `proptest` in `crates/storage/tests/proptests.rs`. Covers header round-trip, full checksum bit-flip sweep (8 KiB × 8 bits = 65K flips per case), insert/delete/compact op-sequence invariants against an oracle, file manager durability across reopen.
- **Crash-recovery torture test** (Sprint 10): kill the process at random points during a WAL-heavy write workload, restart, verify the database is consistent with the committed transactions.
- CI bumps `PROPTEST_CASES=512` (local default 256).

---

## Open questions

Resolved during Sprint 1 (moved to the relevant sections above):
- ~~Page size~~ → 8 KiB.
- ~~Checksum algorithm~~ → CRC32 IEEE, hand-written, scope `[12..PAGE_SIZE]`.
- ~~Slot ID recycling policy~~ → no recycling, IDs stable for page lifetime.
- ~~Tombstone encoding~~ → slot length 0.

Still open (resolve before the relevant sprint):

| Question | When to resolve |
|---|---|
| B+ tree fanout: empirical (benchmark) or analytic (target 128)? | Sprint 2 |
| Buffer pool replacement: LRU-K, CLOCK, or 2Q? | Sprint 2 |
| Free-space tracking: per-page free-space map page, or scan-on-demand? | Sprint 2 |
| MVCC garbage collection: epoch-based or vacuum scan? | Sprint 6 |
| Checkpoint strategy: fuzzy vs sharp? | Sprint 4 |
| Isolation levels above SI: ship Serializable (SSI) or stop at SI? | Sprint 6 |
| WAL format: do `prev_lsn` and `txn_id` go before or after the per-type payload? | Sprint 3 |

---

## Reference reading (load when relevant)

- Mohan et al., *ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging* (1992).
- CMU 15-445 / 15-721 lectures (Pavlo).
- Petrov, *Database Internals*.
- Postgres source — `src/backend/storage/buffer/` and `src/backend/access/transam/xlog.c` as a sanity check on real-world layouts.
