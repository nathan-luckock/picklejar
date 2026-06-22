<div align="center">

# picklejar design

The architecture, and the reasoning behind every decision.

[Overview](../README.md) &nbsp;·&nbsp; [Features](FEATURES.md) &nbsp;·&nbsp; [Build log](sprints.md)

</div>

---

This document records each major design decision in the engine together with the
alternatives that were considered and rejected, so a reader can reconstruct why
every layer is shaped the way it is. For the order in which the pieces were
built, see [sprints.md](sprints.md); for the full SQL and engine surface, see
[FEATURES.md](FEATURES.md).

## Mission and direction

The engine began as a from-scratch relational database. Its direction is now
specific: **a proof-driven database for infrastructure humans cannot physically
service** - orbital and edge nodes, remote sensors, anywhere a failed disk is
never swapped and a partitioned link is never fixed by hand. Its first
application is reliability infrastructure for AI memory: durable, isolated
embeddings and per-tenant context for agents running in those places.

This is a deliberate pivot toward a real, open problem rather than a better copy
of an existing one. The reasoning:

- **Compute is already moving to those environments.** Orbital data centers are
  no longer speculative (GPUs and model training in space, multi-billion-dollar
  valuations, large satellite-constellation filings as of early 2026), and the
  edge keeps pushing further out, but the durable, queryable *data layer* for them
  does not exist. Space storage products so far are archival, not databases.
- **The hard requirement there is provable durability, not features.** When no
  human can intervene, "it usually recovers" is unacceptable. The system must be
  able to *prove* that committed data survives arbitrary crashes and faults.
- **That proof is exactly what this engine already has.** Recovery correctness is
  established by deterministic simulation testing: a fault-injecting in-memory
  disk, every run a single seed, every failure replayable byte-for-byte. The
  current bar is **1,000,000 seeded crash-and-recover runs, all passing** (see
  [Crash model and the torture test](#crash-model-and-the-torture-test)).

### What is and isn't novel (an honest scoping)

Stated plainly so the claim survives scrutiny:

- **Not novel:** vector similarity search; *engine-enforced* row-level isolation
  on vector queries (Postgres + `pgvector` + RLS, and Oracle 23ai, already ship
  this); deterministic simulation testing as a technique (FoundationDB,
  TigerBeetle, Antithesis).
- **The open ground:** reliability infrastructure for AI memory whose durability
  is *proven* by deterministic simulation and exhaustive model-checking, built for
  unreachable infrastructure. Rigorous reliability testing of vector databases is
  still posed as a future problem in the literature, and no system fuses durable +
  isolated + vector + fault-proven for the unreachable-node target.

So vector search and row-level isolation are treated here as table stakes the
engine must have; the differentiator, and the thing the roadmap drives toward, is
the *proof* that the memory survives an environment no one can reach.

The memory layer is built on top of the proven engine and is complete (see
[The vector memory layer](#the-vector-memory-layer)). A native `VECTOR(n)` type
stores `f32` embeddings durably, width-enforced on write; the four distance
operators and an HNSW index serve nearest-neighbor search; row-level security is
folded into similarity queries so the engine itself guarantees one tenant can
never read another's memory; and the deterministic simulator proves isolation and
durability hold together under fault, now extended with a space radiation model,
self-healing erasure coding, and model-checked core invariants, all regenerated
into the `vecert` certificate.

## Goals

A relational database engine with:
1. SQL interface (CREATE, INSERT, SELECT/WHERE, plus UPDATE, DELETE, JOIN, GROUP BY).
2. ACID transactions: durability via WAL, atomicity via ARIES-style undo, isolation via MVCC.
3. A cost-based query planner that picks between scan and join strategies using table statistics.
4. A live demo proving crash safety: forced kill mid-write, restart, no data loss.

Non-goals: distributed replication, network protocol compatibility with Postgres, performance parity with mature engines.

## Ground rules

1. **Every core component is implemented in this workspace, not delegated to a third-party crate.** The storage engine, WAL and recovery, MVCC, the SQL parser, and the planner are all part of the codebase. External crates are restricted to plumbing outside the engine: error handling (`thiserror`), logging (`tracing`), CLI argument parsing (`clap`). No embedded database (SQLite, sled, RocksDB), no SQL parser crate (`sqlparser`), no checksum crate (`crc32fast`).
2. **Every change carries its reasoning.** Each commit ships with a `Design notes:` section recording what was chosen and why, and the larger decisions land in this document with the alternatives that were rejected.
3. **Test before commit.** `cargo build`, `fmt`, `clippy -D warnings`, and the full test suite pass on every change.

---

## Status

Every layer below the SQL surface is implemented and tested: storage, the
write-ahead log and ARIES recovery, MVCC, the parser, the cost-based planner,
the executor, and the PostgreSQL wire protocol. The SQL surface is deep
(joins, window functions, set operations, CTEs, subqueries, a full everyday
type system, roles and row-level security) and still growing. Durability is
proven by 1,000,000 deterministic crash-and-recover simulations. The AI memory
layer is built on this foundation: a native `VECTOR(n)` type, four distance
metrics with brute-force KNN, row-level-security-filtered similarity search, an
HNSW index wired into SQL through a cached, RLS-safe path, and a fault simulator
that proves durability and isolation together, now extended with a space
radiation model, self-healing erasure coding, and model-checked core invariants.
[sprints.md](sprints.md) tracks what shipped in what order.

## Architecture

```
    +--------------------------+
    |    picklejar-cli (REPL)     |
    +--------------------------+
                 |  Database::execute(sql)
                 v
    +--------------------------+
    |    executor (Volcano)    |
    +--------------------------+
                 |  physical plan
                 v
    +--------------------------+
    |   planner (cost-based)   |
    +--------------------------+
                 |  logical plan
                 v
    +--------------------------+
    |       sql (parser)       |
    +--------------------------+

    The query path above runs over the storage stack below.

    +--------------------------+
    |    txn manager + MVCC    |
    +--------------------------+
                 |  pin / read / write / log
                 v
    +--------------------------+
    |       buffer pool        |
    +--------------------------+
                 |
                 v
    +--------------------------+
    |  page manager + B+ tree  |   (data file on disk)
    +--------------------------+
    +--------------------------+
    |  WAL + recovery manager  |   (write-ahead log, separate file)
    +--------------------------+
```

---

## Storage layer

### Page size

**Decision: 8 KiB.** Matches Postgres default. Big enough to amortize per-page overhead, small enough that buffer-pool memory ratio is reasonable.

Exposed as `picklejar_storage::PAGE_SIZE` (`usize`) and `PAGE_SIZE_U16` (typed mirror for `u16` arithmetic in the slot directory). A compile-time assertion in `page.rs` keeps them in sync.

### File manager

`picklejar_storage::FileManager` owns the database file and exposes page-granular I/O. Single source of truth for raw page reads and writes; higher layers (buffer pool, WAL flush path) go through it.

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

- `&mut self` on read/write. Concurrent reads are the buffer pool's job. Pushing positional pread/pwrite into the file layer would be premature optimization for the capstone's single-node scope.
- `allocate_page` uses `set_len` to extend the file. OS guarantees zero-fill for the new region; faster than a `seek + write_zeros` loop and identical semantics.
- File opened with `truncate(false)` AND `create(true)`. Explicit so a refactor doesn't accidentally start truncating live databases.
- `MisalignedFile` is a real error, not a panic. A wrong-page-size build or a half-written torture-test artifact needs to surface to the caller so they can decide to abort, repair, or ignore.
- **Rejected**: `O_DIRECT`. We want the OS page cache for the demo - cache effects are part of what the buffer pool buys.
- **Rejected**: positional I/O for now. Sprint 1 surface is small enough that seeking is fine; revisit if benchmarks show seek overhead matters.

### Page header

Every page starts with a 24-byte header, little-endian:

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 8 | `lsn: u64` | Last LSN that touched this page. WAL ordering anchor. |
| 8 | 4 | `checksum: u32` | CRC32 of `[12..PAGE_SIZE]`. |
| 12 | 2 | `page_type: u16` | `Free` / `Heap` / `BTreeInternal` / `BTreeLeaf` / `Overflow`. |
| 14 | 2 | `slot_count: u16` | Live + tombstoned slots. |
| 16 | 2 | `free_space_ptr: u16` | Offset where the tuple region begins (tuples grow up toward lower offsets from here). |
| 18 | 2 | `flags: u16` | Bit 0 = `FLAG_DIRTY` (in-memory only), Bit 1 = `FLAG_NEEDS_VACUUM`. |
| 20 | 4 | `reserved: u32` | Zero on disk. Reserved for MVCC version-chain pointer. |

**Decisions:**

- `page_type` is a `u16` even though we have <8 variants. Room for visibility map / free-space map page types later without breaking the binary format.
- `reserved: u32` exists specifically for the MVCC chain pointer. Reserving the space now means Sprint 6 doesn't force a layout migration.
- `FLAG_DIRTY` is in-memory only - the flush path clears it before write-back.

### Checksum

**Polynomial:** IEEE 802.3, reflected (`0xEDB88320`). The standard CRC32 used by gzip, Ethernet, and Postgres.

**Implementation:** `crates/storage/src/crc32.rs`, ~30 lines with a compile-time `[u32; 256]` lookup table. Passes the IEEE test vector `crc32("123456789") == 0xCBF43926`.

**Decisions:**

- **Scope: `[12..PAGE_SIZE]`** - the page **excluding the LSN and the checksum field itself**. The LSN is updated on every WAL-acknowledged page write; including it in the checksum would force a recompute on every update. Excluding it still catches what the checksum is for: torn writes and silent bit-rot in the payload. Postgres makes the same tradeoff.
- **Implemented directly rather than via `crc32fast`.** Per ground rule 1 (above), storage-related code stays in-tree. CRC32 is small enough that a direct implementation is justified by clarity alone, and no dependency wins back enough complexity to matter.
- **Rejected**: SIMD-accelerated CRC32 (e.g. `crc32` intrinsic). The page checksum runs over 8 KiB at a time - not the hot path that benefits from intrinsics.
- **Rejected**: blake3 / xxHash. CRC32 catches accidental corruption (the documented threat model). Cryptographic strength isn't needed for a single-node DB.

### Slotted-page heap layout

```
 0       24                            free_space_ptr        PAGE_SIZE
 +-------+-----------------+---------+------------------------------+
 |header | slot directory  |  free   |          tuple data         |
 |       | (grows right)   |  space  |        (grows left)         |
 +-------+-----------------+---------+------------------------------+
```

Slot directory entry (4 bytes, little-endian): `(offset: u16, length: u16)`. Length 0 = tombstoned.

`picklejar_storage::HeapPage<'a>` wraps a `&'a mut Page` and exposes `init` / `from_bytes` / `insert` / `get` / `delete` / `compact` / `free_space` / `slot_count` / `tuple_count`.

**Decisions:**

- **Slot IDs are stable for the page's lifetime and never recycled.** Once a `SlotId` is assigned by an `insert`, it always refers to the same logical slot, even after `delete`. This is the foundation that secondary B+ tree indexes rely on. The alternative - reusing freed slot IDs on the next insert - saves 4 bytes per delete but silently breaks any external `(page_id, slot_id)` reference. Hard no.
- **Tombstone marker = slot length 0**, not a separate flag bit. Smaller (no extra bit per slot), simpler (one comparison in `get`, not two). Cost: empty tuples can't be stored. Schemas with zero-length columns will use NULL bitmaps; not a real loss.
- **`compact` is explicit, never implicit.** Inserts and reads never trigger compaction on their own. The buffer pool will decide when to schedule it based on the `FLAG_NEEDS_VACUUM` hint. Keeps the hot path predictable - no "this insert took 50ms because it compacted" surprises.
- **`FLAG_NEEDS_VACUUM` threshold: 1024 bytes** of tombstoned space. Arbitrary for the capstone - real systems use adaptive thresholds based on page utilization and access patterns.
- **`compact` allocates a small temp `Vec<(u16, Vec<u8>)>`.** Page-local, ~30 tuples max at typical sizes, not on the hot path. A zero-alloc in-place compaction is doable via overlapping copies but adds complexity unjustified for Sprint 1's budget.
- **Rejected**: storing tuples in slot order. Insertion order makes `compact` a simple "walk live slots, copy to end" pass. Keeping tuples sorted by slot ID would force a re-sort after every `compact`.

### B+ tree

Fanout 509 keys per internal node and 453 entries per leaf, for 8 KiB pages with `u64` keys. An internal node holds sorted keys plus child page IDs; a leaf holds sorted keys plus tuple references `(PageId, SlotId)` and a sibling pointer for range scans. `BTree` exposes `insert`, `search`, `upsert`, `delete`, and `range_scan` over the buffer pool, splitting and propagating to a new root as needed.

### Buffer pool

LRU-K (K=2) replacement. Pin and unpin are handled by RAII read and write guards; pinned pages are evict-immune, and the dirty bit is set on the first write through a write guard. The pool is interior-mutable (`&self`) so multiple guards can coexist, and it routes flushes through the WAL hook so write-ahead ordering is preserved.

---

## WAL

### Log record layout (29-byte header + payload + 4-byte trailer)

| Offset | Size | Field |
|---|---|---|
| 0..4 | 4 | `length: u32` (total record size including length and checksum) |
| 4..5 | 1 | `type: u8` (Begin / Update / Commit / Abort / Checkpoint / Clr) |
| 5..13 | 8 | `lsn: u64` (assigned by writer, starts at 1) |
| 13..21 | 8 | `txn_id: u64` |
| 21..29 | 8 | `prev_lsn: u64` (`Lsn::INVALID = u64::MAX` for first record in txn) |
| 29..N-4 | N | payload (per-type) |
| N-4..N | 4 | `checksum: u32` (CRC32 of bytes `[0..N-4]`) |

Update payload: `page_id u64 + slot_id u16 + before (u16-prefixed bytes) + after (u16-prefixed bytes)`. Insert has empty before; delete has empty after.

### Writer (`WalWriter`)

- `open(path)`: creates the file if missing. Scans existing files to recover the highest assigned LSN; resumes allocation at `last_lsn + 1`.
- `append(record, txn, prev_lsn) -> Lsn`: serializes into an in-memory buffer. NOT durable until `fsync_through` or `fsync_all`.
- `fsync_through(lsn)`: flushes buffer and calls `File::sync_all` if `lsn > durable_through`. No-op otherwise.
- `fsync_all()`: makes everything currently buffered durable.

### Reader (`WalReader`)

Forward `Iterator<Item = Result<(RecordHeader, LogRecord)>>`. Clean `None` on EOF or torn tail (before length prefix or mid-record). Single `Some(Err)` then `None` on checksum mismatch / unknown type byte (poisoned).

### Buffer pool integration

`BufferPool::with_wal(file, pool_size, hook: Rc<dyn WalSyncHook>)` constructor. Before any dirty-page write (eviction, `flush_page`, `flush_all`), the pool reads the page header LSN and calls `hook.fsync_through(lsn)`. Page LSN = 0 is a sentinel for "never logged about" and skips the hook.

### Decisions

- **Checksum after the length** so the reader can size the buffer before verifying.
- **Length-prefixed framing** for variable-length Update records and torn-tail tolerance.
- **Lsn::INVALID = u64::MAX** sentinel (not 0); zero-init bugs would otherwise look like valid LSNs.
- **Update carries both before and after images.** Before for undo, after for redo.
- **Tail-truncation tolerance** in both the writer (on reopen) and reader (during iteration). A crash during fsync can leave a partial record at EOF; recovery treats everything after the last complete record as if it was never appended.
- **WAL ordering hook trait lives in `picklejar-storage`** to avoid a circular dependency on `picklejar-wal`.

### Rejected

- Variable-length length prefix (varint). Fixed `u32` is plenty and keeps fast-forward seeking trivial.
- 4-byte length prefixes for before/after images. `u16` is enough (tuples are at most ~8 KiB).
- WAL segment rotation. Single-file fits demo scope.
- `O_DIRECT` for the WAL. OS page cache hides write latency without changing semantics, since we explicitly `sync_all` for durability.

## Recovery

The log record layout:

| Field | Type | Notes |
|---|---|---|
| `length` | `u32` | Total record length. |
| `type` | `u8` | One of BEGIN, UPDATE, COMMIT, ABORT, CHECKPOINT, CLR. |
| `lsn` | `u64` | Log sequence number. |
| `txn_id` | `u64` | Owning transaction. |
| `prev_lsn` | `u64` | This transaction's previous record, for undo-chain traversal. |
| `payload` | `[u8]` | Per-type body. |
| `checksum` | `u32` | Over the preceding fields. |

### Three-phase recovery (ARIES)

Implemented in `crates/wal/src/recovery.rs`. `recover(pool, wal_path)` runs all three phases and returns a `RecoveryStats { winners, losers, redone, undone }`.

1. **Analysis** (`analyze`). Scan the WAL forward and rebuild the transaction table: each transaction's last LSN and whether it committed. Committed transactions are *winners*; everything else is a *loser*. Sprint 4 scans from the start of the log (the dirty page table optimization is deferred - see below).
2. **Redo** (`redo`). Replay history: re-apply every `Update` after-image and every `Clr` undo-image to its page, but only when the page's stored LSN is strictly less than the record's LSN (the page-LSN gate). This makes redo idempotent. Redo runs for winners and losers alike so undo starts from a known state. Pages the crashed data file never persisted are materialized via `BufferPool::ensure_allocated`.
3. **Undo** (`undo`). For each loser, walk its `prev_lsn` chain backward from its last LSN. Revert each `Update` by applying its before-image, and append a fsync'd CLR whose `undo_next` points at the next record to undo. CLRs left by a prior crashed undo are skipped straight to their `undo_next`; a loser already ending in `Abort` is skipped entirely.

### Why CLRs

A CLR (compensation log record) is **redo-only**: redo replays it, undo never undoes it. `undo_next` chains the rollback so that a crash *during* undo is safe - re-recovery replays the CLRs already on disk (idempotent via the page-LSN gate) and resumes undo from the last CLR's `undo_next` instead of re-reverting compensated work. This is what makes the whole recovery process idempotent: running it twice, or crashing partway through and rerunning, converges to the same state.

### Crash model and the torture test

The graded requirement is a forced crash with no committed data loss. Three layers prove it:
- **In-process** (`crates/wal/tests/recovery_integration.rs`): drive a `MiniHeap` workload, drop the buffer pool *without* flushing (dirty pages lost, only the fsync'd WAL survives - exactly what a kill does to unflushed pages), recover, assert committed rows survive and uncommitted rows are rolled back.
- **Forced process kill** (`crates/wal/tests/torture.rs` + the `crash_harness` binary): spawn a child process that commits rows forever and records each durably-committed row to a ground-truth file *after* its commit is on disk, then hard-kill it (`TerminateProcess` on Windows / `SIGKILL`), recover, and assert every ground-truth row is present. Runs several rounds so the kill lands at different points.
- **Deterministic simulation testing (DST)** (`crates/wal/src/sim.rs`, `crates/wal/tests/dst.rs`, and the `dst` binary): every run is driven by a single `u64` seed, so any failure replays exactly (`cargo run --bin dst -- --seed <n>`). The data file is a `FaultDisk`, an in-memory block device (over the new `Disk` trait the buffer pool writes through) that models durability explicitly: a `write_page` only stages bytes, and a crash keeps the last-`fsync`'d image and discards everything else. This is stricter than the in-process layer, where the OS page cache would still hand back un-fsynced writes and hide durability bugs. Each seed builds a random workload (committed / aborted / in-flight transactions) with a randomized durable-vs-lost split, crashes, recovers, and checks every committed row survives and every rolled-back row is gone. The `dst` binary routinely sweeps tens of thousands of seeds.

**A bug DST found.** The simulator immediately surfaced a real recovery defect: `MiniHeap::abort` reverted pages in process *without logging CLRs*, while recovery's undo skips any loser that already logged `Abort` (it assumes the rollback is durable). When a crash lost the in-memory revert but redo replayed the original insert, the aborted row resurrected. The fix makes `abort` write a fsync'd CLR per revert, exactly like recovery's undo, so the rollback is part of the replayable log; the skip-on-`Abort` optimization is sound only because aborts are now logged. This is the value of DST: a class of crash-timing bug that a few hand-written tests will not reliably hit, found and fixed reproducibly.

`MiniHeap` (`crates/wal/src/workload.rs`) is the recoverable workload harness standing in for the SQL executor in the recovery tests: begin / insert / update / delete / commit / abort, logging WAL-before-page and stamping each page's LSN exactly the way recovery expects to replay it.

### Invariant (WAL ordering)

A dirty page cannot be flushed before its corresponding log records are fsync'd. Enforced by the buffer pool's flush path: before write-back, it calls the WAL hook to fsync through the page's stored LSN.

---

## Transactions + MVCC

Implemented in the `picklejar-txn` crate. Snapshot isolation is the default; Read Committed is also available.

### Transaction manager (`manager.rs`)

Assigns monotonic xids (from 1; `0` is the "no transaction" sentinel), tracks each xid's state (Active / Committed / Aborted), and captures a **txid-based snapshot** at begin (Postgres model):

- `xmax`: first xid not yet started at snapshot time (xids `>= xmax` are invisible).
- `active`: xids in progress at snapshot time (their writes are invisible).
- `xmin`: lowest active xid (a fast-path floor).

A creator xid `v` is "committed in the past" for a snapshot iff `v < xmax`, `v` not in `active`, and `state(v) == Committed`.

### Visibility (`visibility.rs`)

A version carries `xmin` (creator) and `xmax` (deleter; `0` = live). `Snapshot::is_visible(xmin, xmax, mgr, reader)` returns true iff **both**:
1. the creator is visible (`xmin == reader`, i.e. own write, or `xmin` committed in the past), and
2. the deletion is not visible (`xmax == 0`, or the deleter aborted, or the deleter is still in progress / committed after the snapshot).

Snapshot stability falls out for free: `is_visible` reads only state frozen at begin time, so a later commit can never flip a version's visibility for an existing reader.

### Versioned values (`version.rs`)

Each row is a chain of versions, each stored in a heap slot as:
`[ xmin u64 | xmax u64 | prev_page u64 | prev_slot u16 | payload ]` (26-byte header). `prev` (a `TupleRef`, `INVALID` = oldest) links a version to the previous one. Deleting or updating stamps the old version's `xmax` in place - a fixed 8-byte write, never a payload rewrite.

### MVCC table (`mvcc.rs`)

A B+ tree index maps `key -> newest version ref`; versions live in heap pages.
- **insert**: new version at the head, chaining onto any existing head.
- **update**: stamp the version *visible to the writer* with `xmax = txn`, then chain a new version at the head.
- **delete**: stamp the version visible to the writer with `xmax = txn`.
- **get**: walk the chain newest-to-oldest, return the first version visible to the reader's snapshot.

Critically, update/delete operate on the **version visible to the writer**, not the index head - the head can be a dead version from an aborted transaction. (This was caught by the property tests.)

Every write logs an `Update` WAL record (WAL-before-page) so versions are durable.

### Isolation levels

`RepeatableRead` (default) reuses the begin-time snapshot for the whole transaction. `ReadCommitted` refreshes the snapshot before each `get`, so each statement sees commits that landed since the transaction began. The only difference is *when* the snapshot is taken; the visibility rule is shared.

### Scope and deferrals

- **Write-write conflict detection** (first-committer-wins / Serializable, SSI) is a Sprint 6 stretch. Sprint 5 delivers snapshot-stable concurrent **reads**.
- **MVCC-aware crash recovery** (rebuilding the index, undoing versions) integrates with the executor in a later sprint. Writes are WAL-logged today.
- **Version GC / vacuum.** `VACUUM [table]` reclaims the space held by dead
  versions and stale index entries by rewriting a table's currently visible
  rows into a fresh MVCC store with rebuilt secondary indexes (a compacting,
  `VACUUM FULL`-style rewrite). This is safe and simple because the engine is
  single-threaded, so a vacuum runs with no other live snapshot to invalidate;
  it is refused inside a transaction block for the same reason. Incremental,
  opportunistic GC (pruning chains in place behind a global visibility horizon)
  is the natural follow-up once the engine is multi-threaded.

The page header's `reserved: u32` field remains available for an on-page version-chain optimization; the current implementation stores the chain pointer inside the version payload instead.

---

## SQL parser

No `sqlparser-rs`; implemented in `picklejar-sql`.

### Lexer (`lexer.rs`, `token.rs`)

A single forward pass turns SQL text into `Vec<Token>` ending in `Eof`. Each token carries a byte `Span` so every downstream error can point at exact source text. Keywords are matched case-insensitively (`INT`/`INTEGER`, `TEXT`/`VARCHAR` aliased); identifiers keep their case. String literals are single-quoted with `''` as the quote escape. Whitespace and `-- line comments` are skipped. `!=` and `<>` both lex to `NotEq`.

### Expression parser (`parser.rs`)

Precedence-climbing (Pratt): a single `parse_bp` plus a binding-power table. Precedence (loosest first): `OR < AND < NOT < comparison < + - < * / < unary -`, binary operators left-associative. `NOT` is a prefix operator whose operand is parsed at comparison binding power, so `NOT a = b` is `NOT (a = b)` and `NOT a AND b` is `(NOT a) AND b`. The `Parser` cursor (peek / advance / eat / expect / expect_keyword / parse_ident) is shared by every statement parser.

### Statement parsers (`statement.rs`)

- **DDL**: `CREATE TABLE name (col type [PRIMARY KEY], ...)`, `DROP TABLE name`, `CREATE INDEX name ON table (column)`.
- **DML**: `INSERT INTO t (cols) VALUES (...), (...)`, `UPDATE t SET c = e, ... [WHERE]`, `DELETE FROM t [WHERE]`.
- **SELECT**: projection list (`*`, expressions with optional `[AS] alias`), `FROM` with alias, `INNER`/`LEFT JOIN ... ON`, `WHERE`, `GROUP BY`, `ORDER BY [ASC|DESC]`, `LIMIT n`.

### Correctness: Display round-trip

Every AST node has a `Display` that prints canonical SQL, fully parenthesizing expressions. The property test (`tests/proptests.rs`) generates arbitrary ASTs, prints them, re-parses, and asserts equality: `parse(print(ast)) == ast`. The printer and parser are exact inverses by construction, which is the parser's correctness oracle.

### Supported SQL surface

DDL is `CREATE TABLE` (with `PRIMARY KEY` / `UNIQUE` / `NOT NULL` / `DEFAULT` /
`SERIAL` per column, plus table-level `CHECK (predicate)` and single-column
`FOREIGN KEY (col) REFERENCES parent (col)`, both also accepted inline on a
column), `CREATE INDEX`, `DROP TABLE`, `TRUNCATE TABLE`,
`ALTER TABLE ... ADD COLUMN`, and `CREATE VIEW` / `DROP VIEW`. A `SERIAL`
column is an integer that auto-assigns the next value (the column's running
maximum plus one) when an `INSERT` omits it, so an explicit value above the
current maximum simply advances the sequence. The set of serial columns per
table is persisted in a `.seq` sidecar and reloaded on open, so the counter
continues across a restart without ever resurrecting a reused id.
DML is `INSERT` (multi-row, omitted columns take their default), `UPDATE`, and
`DELETE`, each accepting a `RETURNING <projection>` that turns the write into a
result set over the affected rows. An `INSERT` may carry an
`ON CONFLICT [(cols)] DO {NOTHING | UPDATE SET ... [WHERE ...]}` clause: a
proposed row that would collide on a unique or primary-key column is either
skipped (`DO NOTHING`) or upserted onto the existing row (`DO UPDATE`), where
the `SET` and `WHERE` expressions may read the existing row by bare name and
the rejected row through the `excluded.` qualifier (Postgres `EXCLUDED`).
Transaction control is `BEGIN` / `COMMIT` / `ROLLBACK`. `SELECT` covers:

- Projections with `AS` aliases, `*`, and arbitrary expressions.
- `WHERE` over the full expression grammar.
- `INNER JOIN`, `LEFT JOIN`, and `CROSS JOIN` (and comma joins) with `ON`.
- `GROUP BY` with `COUNT` / `SUM` / `MIN` / `MAX` / `AVG` (including
  `COUNT(DISTINCT ...)`), and `HAVING`.
- Window functions `func(args) OVER ([PARTITION BY ...] [ORDER BY ...])`:
  `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG` / `LEAD` (with offset and default),
  and the aggregates over a partition. A blocking Window operator computes them
  after `GROUP BY` / `HAVING` and before `ORDER BY` and the projection,
  appending one column per distinct window expression (resolved by its printed
  name, the same scheme aggregates use). An aggregate window is computed over
  the whole partition; running-frame semantics are not implemented.
- `DISTINCT`, `ORDER BY` (multi-key, `ASC` / `DESC`, and by output ordinal or
  alias), `LIMIT`, and `OFFSET`.
- Set operations `UNION`, `INTERSECT`, and `EXCEPT` (each with optional `ALL`),
  chained left-associatively at equal precedence (matching SQLite), with a
  trailing `ORDER BY` / `LIMIT` over the whole result. `UNION` streams both
  sides with optional dedup; `INTERSECT` / `EXCEPT` buffer the right side to
  test membership, then stream the left, preserving left-side order. The `ALL`
  forms use multiset arithmetic (`INTERSECT ALL` keeps `min` multiplicity,
  `EXCEPT ALL` the difference).
- Scalar subqueries `(SELECT ...)`, `expr [NOT] IN (SELECT ...)`, and
  `EXISTS (SELECT ...)`, both uncorrelated and correlated. An uncorrelated one
  is folded to a literal before planning; a correlated one (it references an
  outer column) is evaluated per outer row by a subquery runner over a
  consistent snapshot of the base tables.
- Derived tables: a subquery as a `FROM` / `JOIN` relation,
  `(SELECT ...) AS x`, with its columns re-qualified under the alias. A view
  reference expands to the same machinery over its stored query.
- Common table expressions: `WITH [RECURSIVE] name AS (query), ... body`. A
  non-recursive CTE is inlined: each reference in the body (and in later CTEs)
  is rewritten into a derived table over its query before planning, so a CTE is
  just a named subquery and reuses the derived-table machinery. A `RECURSIVE`
  CTE is evaluated at run time instead: its `anchor UNION [ALL] recursive`
  shape is iterated to a fixpoint (run the anchor once, then repeatedly run the
  recursive term with the CTE bound to the rows found so far, until a round
  adds nothing new), materializing the result into an in-memory relation that
  the body then reads. Each CTE relation is registered in a scratch catalog and
  served through the same in-memory `MaterializedSource` the correlated-subquery
  path uses. A safety cap aborts a recursion that would exceed a million rows. A
  self-reference without `RECURSIVE`, a recursive CTE that is not a `UNION`, and
  a `WITH` column-rename list are each rejected.
- Schema introspection: the read-only views `information_schema.tables`
  (`table_name`, `table_type`) and `information_schema.columns` (`table_name`,
  `column_name`, `ordinal_position`, `data_type`, `is_nullable`) are queryable
  like any table, so a client can discover the schema. They are registered in
  the catalog (so queries bind) but carry no physical store; the engine builds
  their rows from the live catalog on each scan. A schema-qualified table name
  (`information_schema.tables`) is parsed as a single dotted name, which also
  keeps the system views out of the `FROM`-able space for ordinary DDL/DML
  (those parse a single bare identifier).
- `EXPLAIN` of any of the above, and `EXPLAIN ANALYZE`, which also runs the
  query and appends the actual row count and wall-clock time below the
  estimated plan.

The expression grammar has four column types (`INT`, `FLOAT`, `BOOL`, `TEXT`),
arithmetic with int-to-float promotion, comparison and boolean logic with
three-valued NULL handling, the predicates `IN` / `BETWEEN` / `LIKE` /
`IS NULL` (each negatable), `CASE` (searched and simple), string concatenation
(`||`), and the scalar functions `LENGTH`, `UPPER`, `LOWER`, `TRIM` / `LTRIM` /
`RTRIM`, `SUBSTR`, `REPLACE`, `ABS`, `ROUND`, `FLOOR`, `CEIL`, `MOD`, `POWER`,
`SQRT`, `CONCAT`, `COALESCE`, and `NULLIF`.

**Constraint enforcement.** `NOT NULL` and `UNIQUE` are enforced by the storage
glue on each write. `CHECK` and `FOREIGN KEY` are enforced by the engine: a
column-level `CHECK` / `REFERENCES` is normalized to a table constraint at parse
time, so a table carries one uniform constraint list. On `INSERT` / `UPDATE`,
each row is built and validated (NOT NULL, then `CHECK`, then foreign-key
existence) in a pass that precedes any write, so a violation rejects the
statement cleanly. A `CHECK` rejects a row only when its predicate is definitely
false (NULL is unknown and passes), matching SQL. A foreign key requires its
(non-NULL) referencing value to exist in the parent, and is `RESTRICT` on the
parent side: deleting a referenced row, changing a referenced key, or dropping a
referenced table is rejected while a child still points at it. Constraints are
validated when the table is created (the parent table and column must exist) and
persisted to a `<base>.cons` sidecar, so they survive a reopen. `ON DELETE` /
`ON UPDATE CASCADE` and multi-column foreign keys are deferred.

### Scope and deferrals

- The parser is **schema-free**: it enforces grammar only. Semantic checks (INSERT column/value arity, unknown columns, type errors) are the planner's job, since they need the catalog.
- `INTERSECT` / `EXCEPT`, window functions, CTEs, and right/full outer joins are deferred. They are additive: each is a new node or expression form on the same parse/bind/plan/execute pipeline the features above already use. Uncorrelated subqueries and views are constant-folded or expanded to derived tables before planning; a correlated subquery is left in the plan and evaluated per outer row, where its `FROM` must be plain base tables (a correlated subquery whose own `FROM` is a view, derived table, or further subquery is not yet handled).

---

## Planner

The planner is the cost-based optimizer. It turns a parsed
`SELECT` into a logical plan, then into a cost-annotated physical plan,
making two cost-driven choices: sequential scan vs index scan per table, and
nested-loop vs hash join per join. `crates/planner`, no external dependencies.

### Pipeline

1. **Catalog** (`catalog.rs`). In-memory schema + statistics: tables,
   columns, indexes, per-table `row_count`, and per-column statistics
   (`ColumnStats`: distinct-value count plus an integer min/max). DDL is
   applied through `Catalog::apply`; stats are set via `set_row_count` /
   `set_column_stats`. A column with no recorded stats defaults to
   `distinct = 1` and no min/max (pessimistic: it makes equality look
   non-selective, so the planner does not reach for an index on a column it
   knows nothing about). The `ANALYZE [table]` statement scans the live rows
   and records the real distinct count and integer min/max per column, so the
   estimates below come from data rather than defaults. Stats are in-memory, so
   a reopen needs a fresh `ANALYZE` (writes keep a rough distinct count current
   in the meantime).
2. **Logical plan** (`logical.rs`, `binder.rs`). The binder resolves table and
   column names against the catalog and emits a relational-algebra tree
   bottom-up in SQL's evaluation order: `Scan -> Join* -> Filter (WHERE) ->
   Aggregate (GROUP BY) -> Project (SELECT) -> Sort (ORDER BY) -> Limit`. A
   single-table WHERE is placed directly above its Scan (predicate pushdown)
   so the physical planner can fuse it into the access path.
3. **Cost model** (`cost.rs`). Selectivity is estimated from catalog stats:
   - `col = const` -> `1 / distinct(col)` (uniform-distribution guess),
     floored at `1e-6` so a huge cardinality never estimates zero rows.
   - a range comparison (`<`, `<=`, `>`, `>=`) -> the fraction of the column's
     `[min, max]` span the bound admits when `ANALYZE` has recorded it, else
     `0.3` (textbook default). A bound outside the observed range estimates
     all or almost-no rows.
   - `a AND b` -> `sel(a) * sel(b)` (independence); `a OR b` ->
     `sel(a) + sel(b) - sel(a)*sel(b)` (inclusion-exclusion); `NOT a` ->
     `1 - sel(a)`.
   - Scan costs: `seq_scan_cost(rows) = rows`; `index_scan_cost(rows, sel) =
     log2(rows+1) + sel*rows` (a logarithmic B+ tree descent plus one unit
     per matched row).
4. **Physical plan** (`physical.rs`). Every node carries `est_rows` and
   `est_cost`. `choose_scan` fuses a Filter-over-Scan into an `IndexScan`
   when the predicate is sargable on an indexed column *and* the index cost
   beats the seq cost, otherwise a `SeqScan`. `choose_join` picks a
   `HashJoin` for an equi-join (every AND conjunct an equality) when its
   linear `left + right` cost beats the nested loop's `left * right`,
   otherwise a `NestedLoopJoin`.
5. **`EXPLAIN`** (`explain.rs`). `EXPLAIN <statement>` is a parser keyword
   (`Statement::Explain`). The renderer prints the physical plan as an
   indented tree; each node shows its operator and `(rows=.. cost=..)`, with
   scan/filter predicates on an indented line. Example:

   ```
   Project name  (rows=1 cost=11.0)
     IndexScan parts USING idx_id  (rows=1 cost=11.0)
       predicate: (id = 5)
   ```

### Design decisions

- **Monotone, not maximal.** The model targets a *defensible ordering*, not
  perfect cardinalities. A more selective predicate on an indexed column
  always lowers the index cost relative to seq, so the planner flips to the
  index exactly when it pays off. The property test
  (`tests/proptests.rs`) pins the invariant that the chosen scan never costs
  more than a full seq scan, for any catalog and predicate.
- **Cost is chosen, never assumed.** Both the index scan and the hash join
  are taken only when cost says so. A low-cardinality equality keeps the seq
  scan even with an index present; a one-row-per-side join keeps the nested
  loop rather than building a hash table. The crossover falls out of the
  formulas, not a magic constant.
- **Join output rows are the cross-product bound for both algorithms.**
  Without join-key histograms the match ratio is unknown, so the row
  estimate is identical for hash and loop and cannot bias the choice; only
  the access cost (linear vs quadratic) decides.
- **Abstract cost units (rows touched), not milliseconds.** The optimizer
  only needs a consistent ordering between plans. `EXPLAIN` prints these
  absolute units so the seq-vs-index and hash-vs-loop crossovers are
  inspectable side by side. This is the artifact shown at defense.

What is deliberately *not* here yet: logical rewrites beyond single-table
predicate pushdown (no projection pushdown or constant folding), multi-index
intersection, and join-order enumeration (joins are planned in written
order). These are optimizer refinements, not correctness gaps, and are out of
scope for the planner.

---

## Executor and engine

The executor runs a physical plan against stored data, and the `picklejar`
engine ties every layer together so a SQL string produces rows.

### Row codec

A stored row is the bytes the engine writes as an `MvccTable` value. The
format is schema-driven (`crates/executor/src/row.rs`): a null bitmap
(`ceil(n/8)` bytes, LSB first) followed by each non-null column. The four
column types encode as: `INT` 8 little-endian bytes (`i64`), `FLOAT` 8
little-endian bytes (IEEE-754 `f64`), `BOOL` one byte (`0`/`1`), `TEXT` a
4-byte length prefix then UTF-8. The catalog supplies the types, so the bytes
carry only data. A 500-case property test pins `decode(encode(row)) == row`.

`Value` hand-writes `Eq`/`Hash`-style equality (floats compare by bit pattern)
because `f64` is not `Eq`; that keeps grouping and storage keys total while the
SQL `=` operator's three-valued, type-promoting semantics (an `INT` compares
with a `FLOAT`; either float operand promotes arithmetic to float) live in the
executor's evaluator.

### Operator model

Operators are pull-based (`crates/executor/src/operator.rs`): construction is
`open`, `next` yields one row, and `drop` is `close`. `build` lowers a
`PhysicalPlan` into a tree and `run` drains it. Implemented: `SeqScan` (over a
materialized snapshot scan), `Filter`, `Project` (expanding `*`), `Sort` (a
blocking sort, NULLs last), `Limit`, a nested-loop join (`INNER` and `LEFT`,
materializing the right side so it can be rescanned per left row), and a
group-by aggregate (`COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, with or without a
`GROUP BY`, emitting one row per group). Base-table rows arrive through a
`TableSource` trait, so the executor never
depends on the storage stack: the engine materializes a table's visible rows
once via the MVCC scan and hands them over. Expression evaluation (`eval.rs`)
follows SQL three-valued logic, where anything involving NULL yields NULL and a
WHERE row passes only when its predicate is literally true.

Scan output columns are qualified by the table's alias (or name), so a join's
combined row distinguishes `o.id` from `c.id`; a bare reference resolves by the
column suffix and errors if it is ambiguous. The final projection presents
columns under their bare names.

### Engine glue

`picklejar::Database` owns the storage stack (file manager, buffer pool, WAL,
transaction manager), an in-memory catalog, and a descriptor per table.
`execute` parses and routes: DDL updates the catalog and creates or drops the
backing `MvccTable`; `INSERT` encodes each row and stores it under an
auto-increment rowid; `UPDATE` and `DELETE` scan and rewrite or tombstone the
matching rows; `SELECT` binds, plans, and runs over the transaction's
snapshot; `EXPLAIN` prints the cost-annotated plan. `INSERT` validates and
builds every row first, then (when an `ON CONFLICT` clause is present) plans
each one against a single snapshot of the live rows as an insert, a skip, or
an update of the conflicting rowid before any write, so the decision is made
with `&self` and the writes happen together under the mutable table borrow.

### Transactions

By default each statement runs in its own transaction, committed and persisted
on success or aborted on error (auto-commit). `BEGIN` opens an explicit
transaction: subsequent DML and SELECT run inside it and see its own writes,
and the changes become durable only on `COMMIT`. `ROLLBACK` aborts it, after
which the rows it wrote are invisible (their version's `xmin` belongs to an
aborted transaction, so the visibility rule walks past them). This exposes the
MVCC machinery directly: an explicit transaction is the same `Transaction` the
manager hands out, held open across statements instead of one per statement.
DDL auto-commits regardless of an open transaction.

### Constraints

A column may declare `PRIMARY KEY`, `NOT NULL`, or `UNIQUE` (in any order); a
primary key implies the other two. `INSERT` enforces them: a `NOT NULL` column
rejects a NULL (including a column left unnamed in the insert), and a `UNIQUE`
or primary-key column rejects a value already present (the engine scans the
live rows once to gather existing values, and also catches duplicates within
the same statement). `UPDATE` enforces `NOT NULL`. NULLs do not conflict under
`UNIQUE`, matching SQL. Enforcement is by scan today; once the secondary-index
runtime lands, a unique index can answer the check directly.

After each statement that changes the schema or a table's data, the engine
flushes every dirty page to the data file and rewrites a catalog sidecar
(`<base>.meta`) recording, per table, the columns, indexes, the index B+ tree
root page, the current version heap page, and the next rowid. On open the
engine reads the sidecar to rebuild the catalog and the per-table descriptors,
so the existing on-disk pages are reachable again. The sidecar is written
atomically (temp file, then rename), so an interrupted write never leaves a
half-written catalog. This is what makes a table and its rows survive closing
and reopening the database. Three companion sidecars carry the schema that does
not fit the fixed catalog record: `<base>.view` (view definitions), `<base>.cons`
(`CHECK` and `FOREIGN KEY` constraints), and `<base>.seq` (the `(table, column)`
pairs that are `SERIAL`). Each is written the same atomic way and reloaded on
open.

### Secondary indexes

A secondary index is a B+ tree, one per indexed column, mapping the column
value to the rowid that holds it, so an equality lookup becomes a point get
instead of a full scan. The engine builds one automatically for every unique
INT column (a `PRIMARY KEY` or `UNIQUE` column), registers it in the catalog so
the planner can cost an `IndexScan`, and persists its root page in the sidecar.

- **Read path.** `IndexScan` resolves the predicate's equality to a candidate
  rowid through the index, fetches that rowid through the MVCC primary index
  (`MvccTable::get`, which enforces the snapshot), and the executor re-applies
  the full predicate as a residual filter. The residual is what verifies a
  candidate, so an over-broad or stale index result can never return a wrong
  row, only an extra candidate that is filtered.
- **Maintenance is upsert only, never delete.** On an insert, or an update that
  changes the value, the engine upserts `key(value) -> rowid`. Deletes and the
  old values left by updates are not removed. This keeps the index correct
  under MVCC with no rollback logic: a lookup is always verified against the
  visible row, so a leftover entry from an aborted or superseded write is
  filtered, and because nothing is removed, no entry a concurrent reader still
  needs is ever deleted. The cost is index bloat that a periodic rebuild would
  reclaim.
- **Why unique INT only.** Uniqueness guarantees the index keys never collide,
  so the existing unique-keyed B+ tree serves directly with no duplicate-key
  support. INT maps to an order-preserving `u64` key with no hashing. Non-unique
  columns and TEXT (which would need duplicate keys and a hash with collision
  handling) fall back to a sequential scan, which is still correct.

### Design decisions

- **Tables are reopened per operation, not stored.** An `MvccTable` borrows
  the pool and the transaction manager, so storing both the pool and a table
  that borrows it would be a self-referential struct, which Rust forbids.
  Each table's two anchor pages (index B+ tree root, current version heap
  page) live in its descriptor, and a transient `MvccTable` is rebuilt per
  call; after a write the anchors are read back and persisted (an insert can
  split the root or advance the version page).
- **Hidden rowid keying.** Every table uses an auto-increment rowid as its
  storage key; user columns live entirely in the encoded row. Uniform storage,
  and secondary indexes map a column value to that rowid (see Secondary
  indexes), with the MVCC primary index resolving rowid to the visible version.
- **Materialized scans.** The engine reads a table's visible rows into a `Vec`
  via the snapshot scan and hands them to the executor, keeping the executor
  free of storage lifetimes and pin bookkeeping. Streaming is a later
  optimization.

### Known limitations

- `IndexScan` uses a real index lookup for unique INT columns (`PRIMARY KEY` /
  `UNIQUE`); see Secondary indexes above. Non-unique and TEXT predicates fall
  back to a sequential scan with a residual filter (correct, not yet faster),
  pending duplicate-key support and a TEXT hash.
- An equi-join the planner costs as a hash join runs through a real build/probe
  hash join: the right (build) side is hashed by its join-key columns, each left
  row finds matches in O(1), and the full `ON` predicate confirms each candidate
  (so extra non-equi conditions still apply). This turns an O(n*m) join into
  O(n+m). A join with no usable equality key (a theta join), or a node the
  planner costs as a nested-loop, uses the nested-loop executor. NULL join keys
  never match, and `LEFT` joins keep unmatched left rows padded with NULL.
- Schema and data survive a clean restart (flush plus catalog sidecar). A
  reopen also restores MVCC visibility: the transaction watermark (the next
  xid) and the aborted-xid set are persisted, so on reopen every xid below the
  watermark reads as committed except the recorded aborts. Without this the
  manager's xid counter resets and previously committed rows read as aborted,
  losing all data committed across more than one transaction; with it, data
  committed across many transactions survives and rolled-back data stays
  hidden. Full crash-consistency at the SQL level (rebuilding index pages from
  the WAL after a mid-statement kill) is the remaining durability step: the WAL
  logs heap version writes but not B+ tree index pages, so the raw forced-kill
  recovery (proven in Sprint 4) covers the heap, while the index relies on the
  per-statement flush.

### CLI

`picklejar-cli` is a psql-style REPL: SQL terminated by `;` prints an aligned
table, `EXPLAIN <select>` prints the plan, and backslash meta-commands
(`\dt`, `\d <table>`, `\q`) introspect and exit. This is the core CLI surface.

### HTTP API server

`picklejar-server` exposes the engine over HTTP/JSON, an alternative to the wire
protocol for clients that prefer a simple request/response API (a browser, a
service, a notebook). Because the engine is `!Send` (the buffer pool holds
`Rc`), the server is
single-threaded: one accept loop owns one `Database` and processes requests in
order. The HTTP layer and the JSON writer are hand-written, so the database and
its API have no external dependencies beyond CLI and logging plumbing.

- `POST /api/query` runs the request body as SQL and returns the outcome as
  JSON, tagged by `type`: `rows` (with `columns` and `rows`), `mutation` (with
  `affected`), `ok`, `explain` (with `plan`), `message` (transaction control),
  or `error`.
- `GET /api/tables` returns the schema.
- Responses carry permissive CORS headers so a frontend on a different origin
  (a Vite dev server) can call the API.

Any client can drive the engine this way: post SQL to `/api/query` and render
the tagged result.

### PostgreSQL wire protocol

`picklejar-pg` (in the `picklejar-server` crate, module `pgwire`) serves the engine
over the real PostgreSQL v3 frontend/backend protocol, so the actual `psql`
client, GUI tools, and language drivers connect to it directly. It serves
**many connections concurrently** (see "Concurrency: the engine actor" below):
the accept loop hands each connection its own thread and an `Engine` handle. The
framing is exact: each backend message is a one-byte type tag, a big-endian
length that counts itself but not the tag, then the payload.

- **Startup**: SSL/GSS negotiation is declined with a single byte so clients
  fall back to cleartext; protocol 3.0 is accepted with trust authentication,
  a few `ParameterStatus` values, `BackendKeyData`, and `ReadyForQuery`.
- **Simple query** (`Query`): a statement string (split on `;`, respecting
  quoted semicolons) is run through `Database::execute`. Rows become
  `RowDescription` + `DataRow` per row + `CommandComplete` (with the right tag:
  `SELECT n`, `INSERT 0 n`, `CREATE TABLE`, ...); `EXPLAIN` renders as a
  `QUERY PLAN` text column; an error becomes `ErrorResponse` and abandons the
  rest of the batch.
- **Extended query** (`Parse` / `Bind` / `Describe` / `Execute` / `Close` /
  `Sync` / `Flush`): positional parameters `$N` are a parser-level expression
  (`Expr::Parameter`). `Bind` decodes each value (text format typed by its OID,
  with an unspecified type inferred, plus the common binary scalar formats) and
  substitutes it into the statement (`Statement::substitute_params`), turning a
  prepared statement into an ordinary one. `Describe` of a portal runs a
  row-returning statement to learn its columns and caches the result for the
  following `Execute`; a non-row statement answers `NoData` and runs once on
  `Execute`. This is what lets drivers that use server-side prepared statements
  (and `psql`'s `\bind`) work, verified end to end against `psql` 18.
- **Types**: `INT` / `FLOAT` / `BOOL` / `TEXT` map to the int8 / float8 / bool /
  text OIDs; values are sent in text format (bool as `t` / `f`). Result column
  types are inferred from the first non-null value in the result.

The wire protocol reuses the same `Database::execute` entry point as the CLI and
the HTTP API, so every interface exercises one engine.

### Concurrency: the engine actor

The engine is `!Send` (the buffer pool holds `Rc`), so it cannot be wrapped in a
lock and shared across threads. Instead it runs as an **actor**: a dedicated
thread opens and owns the one `Database` (the database is created *on* that
thread, never moved across one), and client connections, each on their own
thread, send SQL to it over a channel and wait for the reply. Because the engine
thread processes one statement at a time, there is never a data race, and the
`Rc` interior-mutability that makes the storage layer fast stays valid.

The wire layer talks to a small `Engine` trait (run a statement, ask whether a
transaction is open), implemented both by the live `Database` (for in-process
and tests) and by a per-connection `SessionHandle` (which round-trips through
the actor). Isolation across connections is enforced by **transaction
exclusivity**: an open explicit transaction owns the engine, so other
connections' statements queue until it commits or rolls back, while auto-commit
statements from any connection interleave freely whenever no transaction is
open; MVCC then gives each statement a consistent snapshot. Dropping a
connection's handle tells the actor to roll back any transaction it still held.
Per-row write-write conflict detection for *overlapping* explicit transactions
(first-updater-wins) is the natural next step; exclusivity makes that case
serial, hence correct, just not yet concurrent.

---

## The vector memory layer

This is the first slice of the AI memory direction (see
[Mission and direction](#mission-and-direction)). It begins with a native vector
type and builds toward isolated, fault-proven similarity search.

### The `VECTOR(n)` type

A `VECTOR(n)` column holds an embedding: a fixed-width list of `f32` components.
The type lives in the same `Value` / `DataType` vocabulary as every other column
type, so it rides the existing row codec, catalog, persistence, wire, and CLI
paths rather than a bolted-on side channel.

- **`f32`, not `f64`.** Real-model embeddings are `f32`, and at a million vectors
  of a few hundred dimensions the halved width is the difference between fitting
  in memory and not. Storing `f64` would double the cost to represent precision
  the source data never had.
- **Equality stays reflexive.** `Value::Vector` compares component *bit patterns*,
  not float values, so a vector containing a `NaN` still equals itself. That keeps
  `Eq` lawful, which the engine relies on for grouping, deduplication, and hash
  keys. Vectors are never the thing being compared for similarity through `=`;
  that is what the distance operators are for.
- **Width is declared and enforced.** The optional `(n)` is the embedding
  dimension. Every write checks the vector's length against it, so a 384-dim model
  cannot quietly insert a 768-dim row that would corrupt a later distance
  computation. A bare `VECTOR` (dimension `0`) is width-agnostic, useful before the
  embedding model is pinned down.
- **On-disk form.** A `u32` component count followed by the little-endian `f32`s.
  The count makes a vector self-describing within the row, and the format
  round-trips a crash and reopen exactly like any other value. The declared
  dimension is persisted in the catalog sidecar (a `VECTOR(n)` type tag), so a
  reopened column rebuilds its width and keeps enforcing it.
- **Literals.** A pgvector-style `VECTOR '[0.1, 0.2, 0.9]'` typed literal, and a
  bare `'[...]'` string coerced into a vector column on write, so embeddings load
  through the ordinary `INSERT` and `COPY` paths.

### Why a vector column declines the B+ tree index

The order-preserving key encoding behind the secondary indexes deliberately
refuses `VECTOR` (alongside `FLOAT` and `DECIMAL`). A vector has no meaningful
total order: nearest-neighbor search ranks by *distance to a query*, not by a
fixed `<` over the values, and that ranking changes with every query. Forcing a
vector into a B+ tree would index something nobody searches by. A vector column
therefore falls back to a sequential scan today, which is correct, and will get an
approximate-nearest-neighbor index (HNSW) of its own later, which is fast.

### The build order, and where it stands

1. **The `VECTOR(n)` type.** Shipped (above): durable `f32` embedding storage,
   width-enforced on write.
2. **Distance operators and brute-force KNN.** Shipped. `<->` (L2), `<=>`
   (cosine), `<#>` (negative inner product), and `<+>` (L1) evaluate to a scalar,
   so `ORDER BY embedding <-> :q LIMIT k` is nearest-neighbor search over a
   sequential scan. Brute force first because it is the *correct* baseline every
   faster index must be checked against.
3. **RLS-filtered similarity.** Shipped. Row-level security already folds a
   `USING` predicate into every read, so similarity search inherits it: the
   engine itself guarantees a tenant's query can only ever rank that tenant's own
   vectors. This is the isolation half of the mission, enforced in the engine, not
   the application. Because the brute-force path filters before it sorts before it
   limits, a `LIMIT` can never leak a forbidden row.
4. **A fault simulator for the memory layer.** Shipped: the `vecsim` binary (and
   `crates/picklejar/src/vecsim.rs`). It runs the real engine through a random
   multi-tenant embedding workload, crashes by drop-and-reopen (WAL recovery),
   and checks an oracle that every committed embedding survives intact *and* that
   each tenant, after recovery, sees exactly its own embeddings and never
   another's, on both reads and nearest-neighbor ranking. Every run is one seed,
   so a failure replays exactly. This sits one level above the storage `dst`
   simulator (row durability against a strict fault disk); together they cover
   durability at the page level and durability-plus-isolation at the memory-layer
   level. That proof, for AI memory in an unreachable environment, is the
   contribution.
5. **An ANN index (HNSW).** The index structure is in
   (`crates/picklejar/src/hnsw.rs`): a seeded, deterministic Hierarchical
   Navigable Small World graph. It is feature-complete as a standalone index:
   build and top-k search; all three metrics (L2, cosine, inner product) matching
   the SQL operators; tombstone-based delete (a removed vector keeps routing the
   graph but never returns from a search); and a versioned serialization so the
   index itself survives a restart. Recall is measured against the brute-force
   baseline from step 2 (recall@10 above 0.90 on random data, exact on the single
   nearest), and `vecbench` reports its speedup over a linear scan. The index is
   now wired into SQL: an opt-in path serves `ORDER BY embedding <-> :q LIMIT k`
   from a cached HNSW index instead of an exact scan. It is keyed by
   `(role, table, column, metric)` and cleared on any write, and it only engages
   when row-level security does not apply, because RLS folds a predicate into a
   WHERE before dispatch and the index shape requires no WHERE. An RLS-fenced query
   therefore always falls back to the exact, fenced path, so the acceleration can
   never breach the isolation guarantee that step 3 establishes; `vecsim` and the
   `vector_index_path` tests are the regression net for exactly that, and
   `vecsqlbench` measures the warm speedup end to end through the SQL engine.

## Self-healing storage (mass-efficient redundancy)

Detecting corruption is not enough where nobody can replace the hardware: the
store has to reconstruct what radiation damaged. The redundancy is done in
software so a deployment can launch light, dense commodity storage instead of
heavy radiation-hardened, triple-redundant parts.

- **The code.** `crates/storage/src/erasure.rs` is a from-scratch systematic
  Reed-Solomon code over GF(2^8) with the standard `0x11D` polynomial: `k` data
  shards plus `m` parity shards, reconstructing the data from any `k` of the
  `k + m`. Surviving `m` failures costs `m / k` storage instead of the `+m*100%`
  of `m` redundant copies; for `k = 10, m = 2` that is `+20%` versus `+200%`. The
  field tables, Gauss-Jordan inversion, and matrix multiply are all in-tree, like
  the rest of the storage layer.

- **The block store.** `resilient.rs` frames each shard with its own CRC32; on
  read it verifies every shard, treats a mismatch as an erasure, reconstructs from
  the survivors when at most `m` are bad, logs the fault, and rewrites the repaired
  shards so the next read is clean. `scrub()` is the periodic pass that heals
  latent corruption before a second fault on the same blob makes it unrecoverable.

- **The live heap.** `resilience.rs` plus `Database::protect` / `open_resilient`
  bring this to the engine. `protect(k, m)` writes a parity sidecar over stripes of
  `k` heap pages; `open_resilient` reconstructs any heap page whose checksum fails
  from that parity *before the buffer pool is built*, so the heal never fights the
  checksum-enforcing read path and the crash-proven hot path is untouched (the
  2000-seed DST sweep still recovers every seed). The page CRC localizes the fault
  to an erasure; the erasure code rebuilds it.

- **Honest scope.** The parity is a point-in-time snapshot. A page changed after
  the last `protect` is crash-covered by the WAL and gains parity at the next
  `protect`; `open_resilient` heals to the snapshot and WAL recovery replays
  anything committed since, which is exactly right for a write-once-read-many
  memory store with a periodic protect. More than `m` bad pages in a stripe is
  genuine data loss: it is reported and the pages stay detectably corrupt, never
  served as a silently wrong answer. `resilientsim` explores the envelope (orbital
  dose versus scrub cadence), and `vecert` certifies both the block store and the
  live-heap heal.

## Backup, replication, and the point-in-time-recovery boundary

`Database::backup` takes a consistent physical snapshot: it flushes the buffer
pool and the WAL, then copies the heap, the WAL, and every sidecar to a
destination base path. Because the engine is single-threaded, between statements
is a consistent point, so the copy is a valid database and `open(dest)` restores
it. The `pjbackup` binary runs this from cron (healing from parity first), which
is how committed data leaves a node that could be lost: ship snapshots to a ground
station or a peer. Restore is just opening the snapshot, and a standby is a peer
kept warm by periodic snapshots. This is snapshot-granularity durability across
whole-node loss, with the recovery-point objective set by the backup cadence.

What is deliberately *not* claimed is log-streaming, LSN-precise point-in-time
recovery, and this is an honest architectural boundary rather than missing polish.
The WAL is an ARIES log of *heap page* changes, but the catalog metadata (table
anchors, `next_rowid`, sequences, policies) lives in separate sidecar files that
are written directly, not logged. A probe confirms the consequence: take a base
backup at 100 rows, append 100 more to the primary, replace the base's WAL with
the primary's longer one, and reopen. The result is 100 rows, not 200: recovery
replays the heap pages, but the base's `meta` sidecar still anchors the table at
its 100-row state, so the later rows are unreachable. Replaying the WAL forward
over a base image therefore cannot land on an arbitrary log position. True PITR
and physical streaming replication require WAL-logging the catalog metadata too,
so a follower can be driven entirely by the log. The WAL-truncation primitives
that such a restore would build on (`picklejar_wal::archive::truncate_to_lsn` and
`max_lsn`) exist and are tested; the metadata logging is the identified next step,
and naming the reason is more useful than shipping a subtly wrong restore.

## Testing strategy

- **Unit tests** in each module. Fast (`cargo test --lib` runs in <50ms today).
- **Property tests** via `proptest` in `crates/storage/tests/proptests.rs`. Covers header round-trip, a full checksum bit-flip sweep (8 KiB x 8 bits = 65K flips per case), insert/delete/compact op-sequence invariants against an oracle, and file-manager durability across reopen.
- **Crash-recovery torture test**: `crates/wal/tests/torture.rs` spawns the `crash_harness` binary, force-kills it mid-write, recovers, and asserts no committed row is lost. Runs several rounds. A polish pass in Sprint 10 will extend the run length and add a long-soak variant.
- **Deterministic simulation testing** (`crates/wal/src/sim.rs`, the `dst` binary): seeded, reproducible crash-recovery exploration over a durability-modeling fault disk. Found and fixed a real undo bug. See [Crash model and the torture test](#crash-model-and-the-torture-test).
- **Exhaustive model checking** (`crates/wal/src/model.rs`, the `walmodel` binary): a from-scratch bounded model checker that enumerates *every* reachable interleaving of an abstract log-and-page state machine (write, fsync log, flush page, crash) and proves the write-ahead-logging ordering invariant, no page change is durable ahead of its log record, holds in all of them. A deliberately buggy variant (flush without the rule) produces a concrete counterexample, so the proof is not vacuous. This complements the random crash sims: simulation finds bugs, exhaustive checking over the bounded model proves their absence.
- **Differential testing against SQLite** (`crates/picklejar-difftest`, the `difftest` binary): for each seed, a generator emits random SQL in a dialect-shared subset (INT/TEXT columns, type-correct predicates, integer aggregates, no `ORDER BY` reliance) and runs the identical SQL through both picklejar and SQLite, comparing results as a sorted multiset. SQLite is the independent oracle: any divergence is a picklejar bug. Thousands of seeds covering joins, `GROUP BY` / `HAVING`, `DISTINCT`, and three-valued NULL logic agree with SQLite. The generator is deliberately type-correct, since picklejar (like Postgres) rejects cross-type comparisons that SQLite's dynamic typing would coerce; that difference is by design, not a bug. The generated subset will widen over time (more operators and types) to push the comparison further.
- CI bumps `PROPTEST_CASES=512` (local default 256).

---

## Future work

The relational engine is complete and proven. The active direction is the AI
memory layer (see [The vector memory layer](#the-vector-memory-layer)); the items
below are the deliberate next steps, each tracked as an issue.

- **Distance operators and KNN.** `<->` / `<=>` / `<#>` and brute-force
  nearest-neighbor search over the shipped `VECTOR` type. The correct baseline
  before any approximate index.
- **RLS-filtered similarity.** Fold row-level security into similarity queries so
  per-tenant isolation on vector search is enforced by the engine.
- **A fault simulator for the memory layer.** Extend deterministic simulation to
  prove durability *and* isolation of AI memory under crash and fault.
- **An ANN index (HNSW).** Speed at scale, verified against the brute-force
  baseline.
- **Concurrency depth.** Dead versions are reclaimed by `VACUUM`; epoch-based
  background garbage collection and first-updater-wins write-write conflict
  detection for overlapping explicit transactions are the next steps.
- **Checkpointing and group commit.** The `Checkpoint` record type exists and
  carries the active-transaction table; bounding recovery to the last
  checkpoint, and batching `fsync` across committers, are additive performance
  work.
- **Replication and point-in-time recovery**, built on the existing WAL.

## References

- Mohan et al., *ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging* (1992).
- CMU 15-445 / 15-721 lectures (Pavlo).
- Petrov, *Database Internals*.
- Postgres source - `src/backend/storage/buffer/` and `src/backend/access/transam/xlog.c` as a sanity check on real-world layouts.
