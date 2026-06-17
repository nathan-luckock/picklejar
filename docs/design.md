# rustdb - Design Document

> Living design document. It records the architecture and the rationale behind each major decision, including the alternatives that were considered and rejected, so that a reviewer can reconstruct why the engine is shaped the way it is from this single file.

## Goals

A relational database engine with:
1. SQL interface (CREATE, INSERT, SELECT/WHERE, plus UPDATE, DELETE, JOIN, GROUP BY).
2. ACID transactions: durability via WAL, atomicity via ARIES-style undo, isolation via MVCC.
3. A cost-based query planner that picks between scan and join strategies using table statistics.
4. A live demo proving crash safety: forced kill mid-write, restart, no data loss.

Non-goals: distributed replication, network protocol compatibility with Postgres, performance parity with mature engines.

## Ground rules

1. **Everything graded is implemented in this workspace, not delegated to a third-party crate.** The storage engine, WAL and recovery, MVCC, the SQL parser, and the planner are all part of the codebase. External crates are restricted to plumbing that is not part of the graded engine: error handling (`thiserror`), logging (`tracing`), CLI argument parsing (`clap`). No embedded database (SQLite, sled, RocksDB), no SQL parser crate (`sqlparser`), no checksum crate (`crc32fast`).
2. **Every change carries its reasoning.** Each commit ships with a `Design notes:` section recording what was chosen and why, and the larger decisions land in this document with the alternatives that were rejected.
3. **Test before commit.** `cargo build`, `fmt`, `clippy -D warnings`, and the full test suite pass on every change.

---

## Implementation status (running)

| Sprint | Scope | Status |
|---|---|---|
| 0 | Bootstrap: workspace, CI, design doc, working agreement | Shipped (PR #1) |
| 1 | Storage I: file manager, page header and CRC32, slotted page, property tests | Shipped (PRs #7-#10) |
| 2 | Storage II: buffer pool and B+ tree, property tests | Shipped (PRs #17-#21) |
| 3 | WAL: record format, writer, reader, buffer-pool integration, property tests | Shipped (PRs #27-#31) |
| 4 | ARIES recovery: analysis, redo, undo, forced-kill torture test | Shipped (PRs #37-#42) |
| 5 | Transactions and MVCC: manager, visibility, versions, `MvccTable`, isolation levels | Shipped (PRs #49-#54) |
| 6 | MVCC polish: write-write conflict detection, version garbage collection | Deferred |
| 7 | SQL parser: lexer, Pratt expressions, DDL, DML, SELECT with JOIN/GROUP/ORDER/LIMIT | Shipped (PRs #62-#67) |
| 8 | Cost-based planner (M6): catalog, logical plan, cost model, join selection, EXPLAIN | Shipped (PRs #73-#77) |
| 9 | Executor and CLI (M1): row codec, MVCC scan, engine, Volcano operators, joins, aggregates, catalog persistence, CLI | Shipped (PRs #85-#99) |
| 10 | Deepen the engine: full DML, explicit transactions, constraints, types, real indexes | In progress (DML, BEGIN/COMMIT/ROLLBACK, PRIMARY KEY/UNIQUE/NOT NULL shipped; more types, real index-scan runtime, and concurrency remain) |
| 11 | Studio: HTTP API and web UI | Planned |
| 12 | Demo, write-up, and presentation | Planned |

---

## High-level architecture

```
    +--------------------------+
    |    rustdb-cli (REPL)     |
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

Exposed as `rustdb_storage::PAGE_SIZE` (`usize`) and `PAGE_SIZE_U16` (typed mirror for `u16` arithmetic in the slot directory). A compile-time assertion in `page.rs` keeps them in sync.

### File manager (Sprint 1 - shipped)

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
- **Rejected**: `O_DIRECT`. We want the OS page cache for the demo - cache effects are part of what the buffer pool buys.
- **Rejected**: positional I/O for now. Sprint 1 surface is small enough that seeking is fine; revisit if benchmarks show seek overhead matters.

### Page header (Sprint 1 - shipped)

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
- `FLAG_DIRTY` is in-memory only - the flush path clears it before write-back.

### Checksum

**Polynomial:** IEEE 802.3, reflected (`0xEDB88320`). The standard CRC32 used by gzip, Ethernet, and Postgres.

**Implementation:** `crates/storage/src/crc32.rs`, ~30 lines with a compile-time `[u32; 256]` lookup table. Passes the IEEE test vector `crc32("123456789") == 0xCBF43926`.

**Decisions:**

- **Scope: `[12..PAGE_SIZE]`** - the page **excluding the LSN and the checksum field itself**. The LSN is updated on every WAL-acknowledged page write; including it in the checksum would force a recompute on every update. Excluding it still catches what the checksum is for: torn writes and silent bit-rot in the payload. Postgres makes the same tradeoff.
- **Implemented directly rather than via `crc32fast`.** Per ground rule 1 (above), storage-related code stays in-tree. CRC32 is small enough that a direct implementation is justified by clarity alone, and no dependency wins back enough complexity to matter.
- **Rejected**: SIMD-accelerated CRC32 (e.g. `crc32` intrinsic). The page checksum runs over 8 KiB at a time - not the hot path that benefits from intrinsics.
- **Rejected**: blake3 / xxHash. CRC32 catches accidental corruption (the documented threat model). Cryptographic strength isn't needed for a single-node DB.

### Slotted-page heap layout (Sprint 1 - shipped)

```
 0       24                            free_space_ptr        PAGE_SIZE
 +-------+-----------------+---------+------------------------------+
 |header | slot directory  |  free   |          tuple data         |
 |       | (grows right)   |  space  |        (grows left)         |
 +-------+-----------------+---------+------------------------------+
```

Slot directory entry (4 bytes, little-endian): `(offset: u16, length: u16)`. Length 0 = tombstoned.

`rustdb_storage::HeapPage<'a>` wraps a `&'a mut Page` and exposes `init` / `from_bytes` / `insert` / `get` / `delete` / `compact` / `free_space` / `slot_count` / `tuple_count`.

**Decisions:**

- **Slot IDs are stable for the page's lifetime and never recycled.** Once a `SlotId` is assigned by an `insert`, it always refers to the same logical slot, even after `delete`. This is the foundation that secondary B+ tree indexes (Sprint 2) rely on. The alternative - reusing freed slot IDs on the next insert - saves 4 bytes per delete but silently breaks any external `(page_id, slot_id)` reference. Hard no.
- **Tombstone marker = slot length 0**, not a separate flag bit. Smaller (no extra bit per slot), simpler (one comparison in `get`, not two). Cost: empty tuples can't be stored. Schemas with zero-length columns will use NULL bitmaps; not a real loss.
- **`compact` is explicit, never implicit.** Inserts and reads never trigger compaction on their own. The buffer pool will decide when to schedule it based on the `FLAG_NEEDS_VACUUM` hint. Keeps the hot path predictable - no "this insert took 50ms because it compacted" surprises.
- **`FLAG_NEEDS_VACUUM` threshold: 1024 bytes** of tombstoned space. Arbitrary for the capstone - real systems use adaptive thresholds based on page utilization and access patterns.
- **`compact` allocates a small temp `Vec<(u16, Vec<u8>)>`.** Page-local, ~30 tuples max at typical sizes, not on the hot path. A zero-alloc in-place compaction is doable via overlapping copies but adds complexity unjustified for Sprint 1's budget.
- **Rejected**: storing tuples in slot order. Insertion order makes `compact` a simple "walk live slots, copy to end" pass. Keeping tuples sorted by slot ID would force a re-sort after every `compact`.

### B+ tree (Sprint 2 - shipped)

Fanout 509 keys per internal node and 453 entries per leaf, for 8 KiB pages with `u64` keys. An internal node holds sorted keys plus child page IDs; a leaf holds sorted keys plus tuple references `(PageId, SlotId)` and a sibling pointer for range scans. `BTree` exposes `insert`, `search`, `upsert`, `delete`, and `range_scan` over the buffer pool, splitting and propagating to a new root as needed.

### Buffer pool (Sprint 2 - shipped)

LRU-K (K=2) replacement. Pin and unpin are handled by RAII read and write guards; pinned pages are evict-immune, and the dirty bit is set on the first write through a write guard. The pool is interior-mutable (`&self`) so multiple guards can coexist, and it routes flushes through the WAL hook so write-ahead ordering is preserved.

---

## WAL (Sprint 3 - shipped)

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
- **WAL ordering hook trait lives in `rustdb-storage`** to avoid a circular dependency on `rustdb-wal`.

### Rejected

- Variable-length length prefix (varint). Fixed `u32` is plenty and keeps fast-forward seeking trivial.
- 4-byte length prefixes for before/after images. `u16` is enough (tuples are at most ~8 KiB).
- WAL segment rotation. Single-file fits demo scope.
- `O_DIRECT` for the WAL. OS page cache hides write latency without changing semantics, since we explicitly `sync_all` for durability.

## Recovery (Sprint 4 - shipped)

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

### Three-phase recovery (ARIES) - Sprint 4, shipped

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

## Transactions + MVCC (Sprint 5 - shipped)

Implemented in the `rustdb-txn` crate. Snapshot isolation is the default; Read Committed is also available.

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

- **Write-write conflict detection** (first-committer-wins / Serializable, SSI) is a Sprint 6 stretch. Sprint 5 delivers snapshot-stable concurrent **reads** (requirement M5).
- **MVCC-aware crash recovery** (rebuilding the index, undoing versions) integrates with the executor in a later sprint. Writes are WAL-logged today.
- **Version GC / vacuum** (reclaiming dead versions) is deferred; dead versions accumulate in the chain for now.

The page header's `reserved: u32` field remains available for an on-page version-chain optimization; the current implementation stores the chain pointer inside the version payload instead.

---

## SQL parser (Sprint 7 - shipped)

No `sqlparser-rs`; implemented in `rustdb-sql`.

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
- `DISTINCT`, `ORDER BY` (multi-key, `ASC` / `DESC`, and by output ordinal or
  alias), `LIMIT`, and `OFFSET`.
- `UNION` and `UNION ALL`, with a trailing `ORDER BY` / `LIMIT` over the union.
- Scalar subqueries `(SELECT ...)`, `expr [NOT] IN (SELECT ...)`, and
  `EXISTS (SELECT ...)`, both uncorrelated and correlated. An uncorrelated one
  is folded to a literal before planning; a correlated one (it references an
  outer column) is evaluated per outer row by a subquery runner over a
  consistent snapshot of the base tables.
- Derived tables: a subquery as a `FROM` / `JOIN` relation,
  `(SELECT ...) AS x`, with its columns re-qualified under the alias. A view
  reference expands to the same machinery over its stored query.
- `EXPLAIN` of any of the above.

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

## Planner (Sprint 8 - shipped, M6)

The planner is the cost-based optimizer (requirement M6). It turns a parsed
`SELECT` into a logical plan, then into a cost-annotated physical plan,
making two cost-driven choices: sequential scan vs index scan per table, and
nested-loop vs hash join per join. `crates/planner`, no external dependencies.

### Pipeline

1. **Catalog** (`catalog.rs`). In-memory schema + statistics: tables,
   columns, indexes, per-table `row_count`, and per-column distinct-value
   counts (`ColumnStats`). DDL is applied through `Catalog::apply`; stats are
   set via `set_row_count` / `set_column_stats`. A column with no recorded
   stats defaults to `distinct = 1` (pessimistic: it makes equality look
   non-selective, so the planner does not reach for an index on a column it
   knows nothing about).
2. **Logical plan** (`logical.rs`, `binder.rs`). The binder resolves table and
   column names against the catalog and emits a relational-algebra tree
   bottom-up in SQL's evaluation order: `Scan -> Join* -> Filter (WHERE) ->
   Aggregate (GROUP BY) -> Project (SELECT) -> Sort (ORDER BY) -> Limit`. A
   single-table WHERE is placed directly above its Scan (predicate pushdown)
   so the physical planner can fuse it into the access path.
3. **Cost model** (`cost.rs`). Selectivity is estimated from catalog stats:
   - `col = const` -> `1 / distinct(col)` (uniform-distribution guess),
     floored at `1e-6` so a huge cardinality never estimates zero rows.
   - a range comparison (`<`, `<=`, `>`, `>=`) -> `0.3` (textbook default).
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
scope for the must-have M6.

---

## Executor and engine (Sprint 9 - M1)

The executor runs a physical plan against stored data, and the `rustdb`
engine ties every layer together so a SQL string produces rows. This is
requirement M1 (CREATE / INSERT / SELECT with WHERE).

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

`rustdb::Database` owns the storage stack (file manager, buffer pool, WAL,
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

### Known limitations (tracked, not gaps in M1)

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

`rustdb-cli` is a psql-style REPL: SQL terminated by `;` prints an aligned
table, `EXPLAIN <select>` prints the plan, and backslash meta-commands
(`\dt`, `\d <table>`, `\q`) introspect and exit. This is the M1 demo surface.

### HTTP API server

`rustdb-server` exposes the engine over HTTP/JSON so a browser studio can use
it. Because the engine is `!Send` (the buffer pool holds `Rc`), the server is
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

This is the boundary the studio UI is built on: a SQL editor posts to
`/api/query` and renders the tagged result.

### PostgreSQL wire protocol

`rustdb-pg` (in the `rustdb-server` crate, module `pgwire`) serves the engine
over the real PostgreSQL v3 frontend/backend protocol, so the actual `psql`
client, GUI tools, and language drivers connect to it directly. Like the HTTP
server it is single-threaded and owns one `Database`, serving one connection at
a time. The framing is exact: each backend message is a one-byte type tag, a
big-endian length that counts itself but not the tag, then the payload.

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

---

## Testing strategy

- **Unit tests** in each module. Fast (`cargo test --lib` runs in <50ms today).
- **Property tests** via `proptest` in `crates/storage/tests/proptests.rs`. Covers header round-trip, a full checksum bit-flip sweep (8 KiB x 8 bits = 65K flips per case), insert/delete/compact op-sequence invariants against an oracle, and file-manager durability across reopen.
- **Crash-recovery torture test** (Sprint 4, shipped): `crates/wal/tests/torture.rs` spawns the `crash_harness` binary, force-kills it mid-write, recovers, and asserts no committed row is lost. Runs several rounds. A polish pass in Sprint 10 will extend the run length and add a long-soak variant.
- **Deterministic simulation testing** (`crates/wal/src/sim.rs`, the `dst` binary): seeded, reproducible crash-recovery exploration over a durability-modeling fault disk. Found and fixed a real undo bug. See [Crash model and the torture test](#crash-model-and-the-torture-test).
- **Differential testing against SQLite** (`crates/rustdb-difftest`, the `difftest` binary): for each seed, a generator emits random SQL in a dialect-shared subset (INT/TEXT columns, type-correct predicates, integer aggregates, no `ORDER BY` reliance) and runs the identical SQL through both rustdb and SQLite, comparing results as a sorted multiset. SQLite is the independent oracle: any divergence is a rustdb bug. Thousands of seeds covering joins, `GROUP BY` / `HAVING`, `DISTINCT`, and three-valued NULL logic agree with SQLite. The generator is deliberately type-correct, since rustdb (like Postgres) rejects cross-type comparisons that SQLite's dynamic typing would coerce; that difference is by design, not a bug. The generated subset will widen over time (more operators and types) to push the comparison further.
- CI bumps `PROPTEST_CASES=512` (local default 256).

---

## Open questions

Resolved during Sprint 1 (moved to the relevant sections above):
- ~~Page size~~ -> 8 KiB.
- ~~Checksum algorithm~~ -> CRC32 IEEE, scope `[12..PAGE_SIZE]`.
- ~~Slot ID recycling policy~~ -> no recycling, IDs stable for page lifetime.
- ~~Tombstone encoding~~ -> slot length 0.

Resolved during Sprint 2/3 (moved to relevant sections above):
- ~~B+ tree fanout~~: 509 keys per internal node, 453 per leaf, locked.
- ~~Buffer pool replacement~~: LRU-K with K=2.
- ~~WAL record field ordering~~: locked (length, type, lsn, txn, prev_lsn, payload, checksum).

Resolved during Sprint 4 (moved to relevant sections above):
- ~~CLR granularity~~: one CLR per undo step, with `undo_next` chaining for crash-safe, idempotent rollback.
- ~~Recovery start point~~: scan from the start of the WAL. The dirty-page-table optimization (start redo at the earliest recovery LSN) is deferred - with no long-running server, a full scan is fast and far simpler to reason about. The `Checkpoint` record type exists and carries the active txn table so adding a checkpoint-bounded analysis later is additive.
- ~~Crash model for testing~~: forced process kill of a child harness, plus an in-process "drop the pool without flushing" simulation.

Still open (resolve before the relevant sprint):

| Question | When to resolve |
|---|---|
| Free-space tracking: per-page free-space map, or scan-on-demand? | Sprint 5 (executor needs it) |
| MVCC garbage collection: epoch-based or vacuum scan? | Sprint 6 |
| Write-write conflict detection: first-committer-wins or SSI? | Sprint 6 |
| Checkpoint strategy: fuzzy vs sharp, and frequency (time / txn-count / WAL-size)? | Sprint 6 (when there is a long-running server to checkpoint) |
| Page checksum maintenance on the write path (recompute on flush)? | Sprint 6 |
| Group commit: batch fsync across multiple committers? | Sprint 6 |

Resolved during Sprint 5 (moved to the Transactions + MVCC section above):
- ~~Snapshot model~~: txid-based (Postgres-style xmin/xmax/active set).
- ~~Isolation levels~~: RepeatableRead (default) + ReadCommitted shipped; SI is the baseline.
- ~~Version chain storage~~: chain pointer (prev `TupleRef`) inside the version payload, not the page header.

---

## Reference reading (load when relevant)

- Mohan et al., *ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging* (1992).
- CMU 15-445 / 15-721 lectures (Pavlo).
- Petrov, *Database Internals*.
- Postgres source - `src/backend/storage/buffer/` and `src/backend/access/transam/xlog.c` as a sanity check on real-world layouts.
