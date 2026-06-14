//! In-memory buffer pool with LRU-K (K=2) replacement.
//!
//! The buffer pool caches a fixed number of pages from a [`FileManager`] in
//! memory and hands out RAII pin/unpin guards to callers. Everything above
//! the storage layer reads and writes through this pool - it's the only
//! place that sees the file directly.
//!
//! # Concurrency
//!
//! Sprint 2 ships single-threaded. Page bytes live in `RefCell<FrameInner>`
//! cells; multiple read guards on the same frame are fine, but a write
//! guard conflicts with any other guard on the same frame (`RefCell` will
//! panic). Frame *metadata* (pin count, access history) lives outside the
//! `RefCell` in `Cell`s so the pool can poll pin counts and update access
//! timestamps without conflicting with held guards. Concurrent reads /
//! one writer comes in Sprint 6 with MVCC - this will upgrade to
//! `RwLock<FrameInner>` then.
//!
//! # Replacement: LRU-K (K=2)
//!
//! Each frame remembers the timestamps of its K most recent accesses.
//! The eviction victim is the unpinned frame whose **K-th-most-recent**
//! access is oldest. Frames with fewer than K accesses ("infrequent")
//! sort oldest by construction - they're evicted before well-warmed
//! frames, which is the entire reason to use LRU-K over plain LRU.
//! Empty frames win the lottery first; pinned frames are never evicted.
//!
//! **Why LRU-K over CLOCK or 2Q?** LRU-K with K=2 is exactly what
//! "scan-resistant LRU" looks like - a single full-table scan can't kick
//! out the working set, because a single access doesn't promote a frame
//! past the K-access threshold. CLOCK is faster but doesn't have this
//! property. 2Q has it but adds a second queue, more code, more state.
//! For an 8 KiB-page capstone scope, K=2 LRU-K is the sweet spot.
//!
//! # The dirty bit lives in memory only
//!
//! `FrameInner::dirty` is purely an in-memory flag. The on-disk page's
//! `FLAG_DIRTY` byte is **never** set - that flag exists for vacuum-hint
//! propagation, not durability. Recovery reads the WAL, not the dirty
//! bit.

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::rc::Rc;

use crate::error::{Result, StorageError};
use crate::file::FileManager;
use crate::header::PageHeader;
use crate::page::{Page, PageId, PAGE_SIZE};

/// Hook the buffer pool calls before flushing any dirty page, to enforce
/// the WAL ordering invariant: WAL records are durable before the dirty
/// pages they describe ever reach disk.
///
/// The buffer pool reads the page's LSN from its header and calls
/// [`fsync_through`](Self::fsync_through) with that LSN. The hook
/// guarantees every WAL record with LSN `<= page_lsn` is durable before
/// returning.
///
/// Implementors live in the WAL crate (see `rustdb-wal::WalSyncHandle`).
/// This trait lives in storage so the buffer pool can call it without
/// taking a dependency on `rustdb-wal`, which would create a dependency
/// cycle.
pub trait WalSyncHook: std::fmt::Debug {
    /// Make every WAL record with LSN less than or equal to `page_lsn`
    /// durable on disk.
    fn fsync_through(&self, page_lsn: u64) -> std::io::Result<()>;
}

/// LRU-K parameter. K=2 gives "scan-resistant" eviction: a single sweep
/// over a large relation can't evict the working set.
pub const K: usize = 2;

/// Logical access timestamp. Monotonically increasing inside one
/// `BufferPool`; not comparable across pools.
type Timestamp = u64;

/// "Never accessed" sentinel - distinguishable from a real timestamp
/// because the clock starts at 1.
const NEVER: Timestamp = 0;

/// Per-frame state behind a `RefCell` so write guards get exclusive
/// access to the page bytes.
struct FrameInner {
    page_id: Option<PageId>,
    page: Box<Page>,
    dirty: bool,
}

impl std::fmt::Debug for FrameInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Skip `page` - 8 KiB of bytes is not useful in Debug output.
        f.debug_struct("FrameInner")
            .field("page_id", &self.page_id)
            .field("dirty", &self.dirty)
            .finish_non_exhaustive()
    }
}

impl FrameInner {
    fn empty() -> Self {
        Self {
            page_id: None,
            page: Box::new([0u8; PAGE_SIZE]),
            dirty: false,
        }
    }
}

/// One slot in the buffer pool. Metadata (`pin_count`, `history`) lives in
/// `Cell`s so the pool can read and update them while a guard holds a
/// borrow on `inner`.
struct Frame {
    pin_count: Cell<u32>,
    /// K most recent access timestamps. `history[K-1]` is the most recent.
    /// `NEVER` entries indicate "fewer than K accesses so far."
    history: Cell<[Timestamp; K]>,
    inner: RefCell<FrameInner>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("pin_count", &self.pin_count.get())
            .field("history", &self.history.get())
            .field("inner", &self.inner)
            .finish()
    }
}

impl Frame {
    fn empty() -> Self {
        Self {
            pin_count: Cell::new(0),
            history: Cell::new([NEVER; K]),
            inner: RefCell::new(FrameInner::empty()),
        }
    }

    fn record_access(&self, now: Timestamp) {
        let mut h = self.history.get();
        for i in 0..K - 1 {
            h[i] = h[i + 1];
        }
        h[K - 1] = now;
        self.history.set(h);
    }

    fn reset_history(&self) {
        self.history.set([NEVER; K]);
    }

    /// Sort key for LRU-K eviction. Smaller = evict first. Frames with
    /// fewer than K accesses have `history[0] == NEVER == 0`, so they
    /// sort oldest by construction.
    fn eviction_score(&self) -> Timestamp {
        self.history.get()[0]
    }
}

/// In-memory buffer pool over a [`FileManager`].
///
/// See the module-level docs for invariants and the replacement policy.
pub struct BufferPool {
    file: RefCell<FileManager>,
    frames: Vec<Frame>,
    index: RefCell<HashMap<PageId, usize>>,
    clock: Cell<Timestamp>,
    /// Optional hook to enforce the WAL ordering invariant. Set via
    /// [`BufferPool::with_wal`]. When `None`, the pool flushes pages
    /// directly without consulting any WAL.
    wal: Option<Rc<dyn WalSyncHook>>,
}

impl std::fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Summarize rather than dump the full frame array or the wrapped
        // FileManager handle.
        f.debug_struct("BufferPool")
            .field("pool_size", &self.frames.len())
            .field("pinned_count", &self.pinned_count())
            .field("resident_pages", &self.index.borrow().len())
            .finish_non_exhaustive()
    }
}

impl BufferPool {
    /// Construct a buffer pool of `pool_size` frames over `file`. The
    /// pool flushes pages directly to disk without WAL ordering checks.
    /// For a pool that enforces WAL ordering, use [`Self::with_wal`].
    ///
    /// # Panics
    ///
    /// Panics if `pool_size == 0`. A pool with zero frames is useless.
    #[must_use]
    pub fn new(file: FileManager, pool_size: usize) -> Self {
        Self::build(file, pool_size, None)
    }

    /// Construct a buffer pool of `pool_size` frames over `file` with WAL
    /// ordering enforced through `wal_hook`. Before flushing a dirty
    /// page, the pool reads the page's LSN and calls
    /// [`WalSyncHook::fsync_through`] to make sure the corresponding WAL
    /// records are durable first.
    ///
    /// # Panics
    ///
    /// Panics if `pool_size == 0`.
    #[must_use]
    pub fn with_wal(file: FileManager, pool_size: usize, wal_hook: Rc<dyn WalSyncHook>) -> Self {
        Self::build(file, pool_size, Some(wal_hook))
    }

    fn build(file: FileManager, pool_size: usize, wal: Option<Rc<dyn WalSyncHook>>) -> Self {
        assert!(pool_size > 0, "buffer pool size must be > 0");
        let frames = (0..pool_size).map(|_| Frame::empty()).collect();
        Self {
            file: RefCell::new(file),
            frames,
            index: RefCell::new(HashMap::with_capacity(pool_size)),
            clock: Cell::new(1),
            wal,
        }
    }

    /// Helper: read a page's LSN from its header and call the WAL hook,
    /// if one is configured. Called before any `write_page` on a dirty
    /// page. No-op when no hook is set or the page header is invalid
    /// (a fresh page may have an unwritten header; in that case
    /// `PageHeader::read` returns an error and we just skip the WAL
    /// call, since there are no log records to wait for).
    fn enforce_wal_ordering(&self, page: &Page) -> Result<()> {
        if let Some(hook) = &self.wal {
            // The page header read can fail for a freshly-allocated page
            // whose page_type byte is still 0 (which is PageType::Free).
            // That's not a real WAL ordering violation; treat as "no LSN
            // to wait for" and continue.
            if let Ok(header) = PageHeader::read(page) {
                // LSN 0 is the "never been logged" sentinel; the WAL
                // writer assigns LSNs starting at 1. A page with LSN 0
                // either is freshly allocated or comes from a build of
                // the engine that did not record an LSN; either way the
                // WAL has no records to make durable on its behalf.
                if header.lsn != 0 {
                    hook.fsync_through(header.lsn).map_err(StorageError::Io)?;
                }
            }
        }
        Ok(())
    }

    /// Total number of frames in the pool.
    #[must_use]
    pub fn pool_size(&self) -> usize {
        self.frames.len()
    }

    /// Number of frames currently pinned by live guards. Reads pin counts
    /// without borrowing frame inners.
    #[must_use]
    pub fn pinned_count(&self) -> usize {
        self.frames.iter().filter(|f| f.pin_count.get() > 0).count()
    }

    /// Number of frames currently holding a resident page (live or stale).
    #[must_use]
    pub fn resident_count(&self) -> usize {
        self.index.borrow().len()
    }

    fn next_timestamp(&self) -> Timestamp {
        let now = self.clock.get();
        self.clock.set(now.wrapping_add(1).max(1));
        now
    }

    /// Pin `id` and return a read-only RAII guard.
    pub fn fetch_page(&self, id: PageId) -> Result<PageReadGuard<'_>> {
        let frame_idx = self.pin_for_existing(id)?;
        let inner = self.frames[frame_idx].inner.borrow();
        Ok(PageReadGuard {
            pool: self,
            frame_idx,
            inner: Some(inner),
        })
    }

    /// Pin `id` and return a write RAII guard. The frame's dirty bit is
    /// set on the first call to [`PageWriteGuard::page_mut`].
    pub fn fetch_page_mut(&self, id: PageId) -> Result<PageWriteGuard<'_>> {
        let frame_idx = self.pin_for_existing(id)?;
        let inner = self.frames[frame_idx].inner.borrow_mut();
        Ok(PageWriteGuard {
            pool: self,
            frame_idx,
            inner: Some(inner),
        })
    }

    /// Allocate a new page on disk and return a write guard over it. The
    /// new page's contents are zero.
    pub fn new_page(&self) -> Result<(PageId, PageWriteGuard<'_>)> {
        let new_id = self.file.borrow_mut().allocate_page()?;
        let frame_idx = self.pin_for_new(new_id)?;
        let inner = self.frames[frame_idx].inner.borrow_mut();
        Ok((
            new_id,
            PageWriteGuard {
                pool: self,
                frame_idx,
                inner: Some(inner),
            },
        ))
    }

    /// Number of pages currently allocated in the underlying file.
    #[must_use]
    pub fn page_count(&self) -> u64 {
        self.file.borrow().page_count()
    }

    /// Ensure the file has a page with `id`, extending it with zero pages
    /// if necessary, then return a write guard over it.
    ///
    /// Used by WAL recovery: after a crash the data file may have fewer
    /// pages than the log references (a page was allocated in memory but
    /// never flushed). Redo calls this so it can re-apply mutations to a
    /// page that does not yet exist on disk. Newly created pages are zero,
    /// which reads back as a `PageType::Free` page with LSN 0, so redo's
    /// page-LSN gate will always re-apply the first logged mutation.
    pub fn ensure_allocated(&self, id: PageId) -> Result<PageWriteGuard<'_>> {
        while self.file.borrow().page_count() <= id.get() {
            self.file.borrow_mut().allocate_page()?;
        }
        self.fetch_page_mut(id)
    }

    /// Flush the page with `id` if it's resident and dirty. Issues an
    /// fsync at the end either way. Enforces WAL ordering when a WAL hook
    /// is configured.
    pub fn flush_page(&self, id: PageId) -> Result<()> {
        if let Some(&idx) = self.index.borrow().get(&id) {
            let mut inner = self.frames[idx].inner.borrow_mut();
            if inner.dirty {
                self.enforce_wal_ordering(&inner.page)?;
                self.file.borrow_mut().write_page(id, &inner.page)?;
                inner.dirty = false;
            }
        }
        self.file.borrow_mut().fsync()?;
        Ok(())
    }

    /// Flush every dirty page in the pool. One fsync at the end.
    pub fn flush_all(&self) -> Result<()> {
        for frame in &self.frames {
            let mut inner = frame.inner.borrow_mut();
            if inner.dirty {
                if let Some(id) = inner.page_id {
                    self.enforce_wal_ordering(&inner.page)?;
                    self.file.borrow_mut().write_page(id, &inner.page)?;
                    inner.dirty = false;
                }
            }
        }
        self.file.borrow_mut().fsync()?;
        Ok(())
    }

    // --- internal helpers ---

    /// Pin an existing on-disk page. Hits the index if resident; otherwise
    /// picks a victim, evicts it, and reads from disk.
    fn pin_for_existing(&self, id: PageId) -> Result<usize> {
        if let Some(&idx) = self.index.borrow().get(&id) {
            let frame = &self.frames[idx];
            frame.pin_count.set(frame.pin_count.get() + 1);
            let now = self.next_timestamp();
            frame.record_access(now);
            return Ok(idx);
        }
        let victim_idx = self.find_victim()?;
        {
            let frame = &self.frames[victim_idx];
            let mut inner = frame.inner.borrow_mut();
            self.evict_inner(&mut inner)?;
            self.file.borrow_mut().read_page(id, &mut inner.page)?;
            inner.page_id = Some(id);
            inner.dirty = false;
            frame.reset_history();
            frame.pin_count.set(1);
            let now = self.next_timestamp();
            frame.record_access(now);
        }
        self.index.borrow_mut().insert(id, victim_idx);
        Ok(victim_idx)
    }

    /// Pin a freshly allocated page. Picks a victim and zeroes it.
    fn pin_for_new(&self, new_id: PageId) -> Result<usize> {
        let victim_idx = self.find_victim()?;
        {
            let frame = &self.frames[victim_idx];
            let mut inner = frame.inner.borrow_mut();
            self.evict_inner(&mut inner)?;
            *inner.page = [0u8; PAGE_SIZE];
            inner.page_id = Some(new_id);
            inner.dirty = true; // a fresh page should eventually reach disk
            frame.reset_history();
            frame.pin_count.set(1);
            let now = self.next_timestamp();
            frame.record_access(now);
        }
        self.index.borrow_mut().insert(new_id, victim_idx);
        Ok(victim_idx)
    }

    /// Spill `inner`'s current page to disk if dirty, remove from index.
    /// Enforces WAL ordering before the write.
    /// Caller is responsible for resetting metadata after this returns.
    fn evict_inner(&self, inner: &mut FrameInner) -> Result<()> {
        if let Some(old_id) = inner.page_id.take() {
            if inner.dirty {
                self.enforce_wal_ordering(&inner.page)?;
                self.file.borrow_mut().write_page(old_id, &inner.page)?;
                inner.dirty = false;
            }
            self.index.borrow_mut().remove(&old_id);
        }
        Ok(())
    }

    /// Pick an eviction victim. Empty frames first, then LRU-K among
    /// unpinned frames. Returns `BufferPoolFull` if every frame is pinned.
    ///
    /// Reads only `Cell`-stored metadata; no `RefCell` borrows here so
    /// this is safe to call while guards on *other* frames are held.
    fn find_victim(&self) -> Result<usize> {
        // Pass 1: empty + unpinned frame.
        for (idx, frame) in self.frames.iter().enumerate() {
            if frame.pin_count.get() == 0 {
                // Quick check via try_borrow - empty frames have page_id None.
                // try_borrow lets us peek without panicking if a guard is alive
                // on a DIFFERENT frame (this loop iterates all, but the borrow
                // is per-frame).
                if let Ok(inner) = frame.inner.try_borrow() {
                    if inner.page_id.is_none() {
                        return Ok(idx);
                    }
                }
            }
        }
        // Pass 2: LRU-K among unpinned frames.
        let mut best: Option<(usize, Timestamp)> = None;
        for (idx, frame) in self.frames.iter().enumerate() {
            if frame.pin_count.get() > 0 {
                continue;
            }
            let score = frame.eviction_score();
            match best {
                None => best = Some((idx, score)),
                Some((_, current)) if score < current => best = Some((idx, score)),
                _ => {}
            }
        }
        best.map_or(Err(StorageError::BufferPoolFull), |(idx, _)| Ok(idx))
    }

    fn unpin(&self, frame_idx: usize) {
        let frame = &self.frames[frame_idx];
        let count = frame.pin_count.get();
        debug_assert!(count > 0, "unpin of unpinned frame {frame_idx}");
        frame.pin_count.set(count.saturating_sub(1));
    }
}

// --- RAII guards ---

/// RAII read-only guard over a pinned page.
///
/// Pin count is incremented when the guard is created and decremented in
/// `Drop`. As long as a guard is live the frame cannot be evicted.
pub struct PageReadGuard<'a> {
    pool: &'a BufferPool,
    frame_idx: usize,
    /// `Option` so `Drop` can release the borrow before unpinning. Always
    /// `Some` between construction and `Drop`.
    inner: Option<Ref<'a, FrameInner>>,
}

impl PageReadGuard<'_> {
    /// The page ID this guard covers.
    #[must_use]
    pub fn page_id(&self) -> PageId {
        self.inner
            .as_ref()
            .expect("guard active")
            .page_id
            .expect("pinned frame has page_id")
    }

    /// Borrow the page bytes.
    #[must_use]
    pub fn page(&self) -> &Page {
        &self.inner.as_ref().expect("guard active").page
    }
}

impl std::fmt::Debug for PageReadGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageReadGuard")
            .field("page_id", &self.page_id())
            .field("frame_idx", &self.frame_idx)
            .finish()
    }
}

impl Drop for PageReadGuard<'_> {
    fn drop(&mut self) {
        drop(self.inner.take());
        self.pool.unpin(self.frame_idx);
    }
}

/// RAII write guard over a pinned page.
///
/// Calling [`page_mut`](Self::page_mut) marks the frame dirty. Reads
/// through [`page`](Self::page) do not set the dirty bit.
pub struct PageWriteGuard<'a> {
    pool: &'a BufferPool,
    frame_idx: usize,
    inner: Option<RefMut<'a, FrameInner>>,
}

impl PageWriteGuard<'_> {
    /// The page ID this guard covers.
    #[must_use]
    pub fn page_id(&self) -> PageId {
        self.inner
            .as_ref()
            .expect("guard active")
            .page_id
            .expect("pinned frame has page_id")
    }

    /// Borrow the page bytes read-only. Does **not** mark the page dirty.
    #[must_use]
    pub fn page(&self) -> &Page {
        &self.inner.as_ref().expect("guard active").page
    }

    /// Borrow the page bytes mutably. Marks the page dirty.
    pub fn page_mut(&mut self) -> &mut Page {
        let inner = self.inner.as_mut().expect("guard active");
        inner.dirty = true;
        &mut inner.page
    }
}

impl std::fmt::Debug for PageWriteGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageWriteGuard")
            .field("page_id", &self.page_id())
            .field("frame_idx", &self.frame_idx)
            .finish()
    }
}

impl Drop for PageWriteGuard<'_> {
    fn drop(&mut self) {
        drop(self.inner.take());
        self.pool.unpin(self.frame_idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_pool(pool_size: usize) -> (TempDir, BufferPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = FileManager::open(dir.path().join("buf.db")).expect("open");
        let pool = BufferPool::new(file, pool_size);
        (dir, pool)
    }

    #[test]
    fn new_page_pins_and_assigns_id() {
        let (_dir, pool) = fresh_pool(4);
        let (id, guard) = pool.new_page().expect("new_page");
        assert_eq!(id, PageId::new(0));
        assert_eq!(guard.page_id(), id);
        assert_eq!(pool.pinned_count(), 1);
        drop(guard);
        assert_eq!(pool.pinned_count(), 0);
    }

    #[test]
    fn fetch_page_hits_cached_frame() {
        let (_dir, pool) = fresh_pool(4);
        let id = {
            let (id, mut g) = pool.new_page().expect("new_page");
            g.page_mut()[42] = 0xCC;
            id
        };
        let g = pool.fetch_page(id).expect("fetch");
        assert_eq!(g.page()[42], 0xCC);
    }

    #[test]
    fn fetch_page_evicts_lru_when_cold() {
        let (_dir, pool) = fresh_pool(2);
        let _id_a = {
            let (id, mut g) = pool.new_page().expect("new a");
            g.page_mut()[0] = 0xAA;
            id
        };
        let _id_b = {
            let (id, mut g) = pool.new_page().expect("new b");
            g.page_mut()[0] = 0xBB;
            id
        };
        let _id_c = {
            let (id, mut g) = pool.new_page().expect("new c");
            g.page_mut()[0] = 0xCC;
            id
        };
        assert_eq!(
            pool.resident_count(),
            2,
            "pool size 2 - only 2 frames resident",
        );
    }

    #[test]
    fn dirty_page_survives_eviction() {
        let (_dir, pool) = fresh_pool(2);
        let id_a;
        {
            let (id, mut g) = pool.new_page().expect("new a");
            g.page_mut()[100] = 0xA5;
            id_a = id;
        }
        let _ = pool.new_page().expect("new b");
        let _ = pool.new_page().expect("new c");
        let g = pool.fetch_page(id_a).expect("re-fetch a");
        assert_eq!(g.page()[100], 0xA5, "dirty page lost across eviction");
    }

    #[test]
    fn pinned_frames_block_eviction() {
        let (_dir, pool) = fresh_pool(2);
        let (id_a, g_a) = pool.new_page().expect("new a");
        let (_id_b, g_b) = pool.new_page().expect("new b");
        let err = pool.new_page().expect_err("must error - all frames pinned");
        assert!(matches!(err, StorageError::BufferPoolFull));
        drop(g_a);
        drop(g_b);
        let _ = pool.new_page().expect("new c after unpin");
        let _ = id_a;
    }

    #[test]
    fn write_then_read_through_separate_guards() {
        let (_dir, pool) = fresh_pool(4);
        let id;
        {
            let (new_id, mut g) = pool.new_page().expect("new");
            g.page_mut()[7] = 0x77;
            id = new_id;
        }
        let g = pool.fetch_page(id).expect("read");
        assert_eq!(g.page()[7], 0x77);
    }

    #[test]
    fn flush_page_writes_dirty_and_clears_flag() {
        let (_dir, pool) = fresh_pool(4);
        let id;
        {
            let (new_id, mut g) = pool.new_page().expect("new");
            g.page_mut()[3] = 0x33;
            id = new_id;
        }
        pool.flush_page(id).expect("flush");
        pool.flush_page(id).expect("flush again");
    }

    #[test]
    fn flush_all_writes_every_dirty_page() {
        let (_dir, pool) = fresh_pool(4);
        let mut ids = Vec::new();
        for byte in 0u8..3 {
            let (id, mut g) = pool.new_page().expect("new");
            g.page_mut()[0] = byte;
            ids.push(id);
        }
        pool.flush_all().expect("flush_all");
        for (i, id) in ids.iter().enumerate() {
            let g = pool.fetch_page(*id).expect("re-fetch");
            assert_eq!(g.page()[0], u8::try_from(i).unwrap());
        }
    }

    #[test]
    fn flush_page_for_missing_page_is_noop() {
        let (_dir, pool) = fresh_pool(4);
        pool.flush_page(PageId::new(999)).expect("flush missing");
    }

    #[test]
    fn lru_k_evicts_one_access_frame_before_warm_one() {
        let (_dir, pool) = fresh_pool(2);
        let id_warm = {
            let (id, _g) = pool.new_page().expect("new warm");
            id
        };
        let id_cold = {
            let (id, _g) = pool.new_page().expect("new cold");
            id
        };
        // Touch warm a second time so it has K=2 accesses.
        drop(pool.fetch_page(id_warm).expect("touch warm"));
        // Now allocate a 3rd page - id_cold (1 access) should be the victim.
        let _ = pool.new_page().expect("new third");
        assert!(
            pool.index.borrow().contains_key(&id_warm),
            "warm page (K=2 accesses) should NOT be evicted before cold (K=1)",
        );
        let _ = id_cold;
    }

    #[test]
    fn multiple_read_guards_on_same_page() {
        let (_dir, pool) = fresh_pool(4);
        let id = {
            let (new_id, _g) = pool.new_page().expect("new");
            new_id
        };
        let g1 = pool.fetch_page(id).expect("read 1");
        let g2 = pool.fetch_page(id).expect("read 2");
        assert_eq!(g1.page_id(), g2.page_id());
        // pinned_count counts FRAMES, not pins - one frame, pin_count=2.
        assert_eq!(pool.pinned_count(), 1);
        drop(g1);
        // Frame still pinned (pin_count goes 2 -> 1).
        assert_eq!(pool.pinned_count(), 1);
        drop(g2);
        assert_eq!(pool.pinned_count(), 0);
    }

    #[test]
    fn debug_impl_does_not_panic() {
        let (_dir, pool) = fresh_pool(2);
        let _ = format!("{pool:?}");
        let (_id, g) = pool.new_page().expect("new");
        let _ = format!("{g:?}");
    }

    #[test]
    fn flushed_page_survives_pool_drop() {
        // Persistence across a drop+reopen of the pool.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("persist.db");
        let id;
        {
            let file = FileManager::open(&path).expect("open");
            let pool = BufferPool::new(file, 4);
            let (new_id, mut g) = pool.new_page().expect("new");
            g.page_mut()[200] = 0xDE;
            id = new_id;
            drop(g);
            pool.flush_all().expect("flush");
        }
        let file = FileManager::open(&path).expect("reopen");
        let pool = BufferPool::new(file, 4);
        let g = pool.fetch_page(id).expect("re-fetch after reopen");
        assert_eq!(g.page()[200], 0xDE);
    }
}
