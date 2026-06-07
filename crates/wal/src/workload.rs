//! A minimal recoverable workload: `MiniHeap`.
//!
//! There is no SQL executor yet (that arrives in Sprint 9), but recovery
//! needs *something* that produces a realistic WAL and mutates pages
//! through the buffer pool so it can be tested and demonstrated. `MiniHeap`
//! is that something: a tiny append-mostly table that supports
//! begin / insert / update / delete / commit / abort, logging each
//! mutation exactly the way recovery expects to replay it.
//!
//! `MiniHeap` is deliberately simple and synchronous. It is the *contract*
//! that [`crate::recovery`] inverts: every mutation writes its WAL record
//! before (or together with) the page change, stamps the page LSN, and
//! fsyncs the record so a crash leaves a recoverable log.
//!
//! # Why fsync on every record
//!
//! A production engine batches fsyncs (group commit). The harness fsyncs
//! every record so that after a crash the log deterministically contains
//! every mutation issued, which makes the recovery tests and the torture
//! test reproducible. Throughput is not a goal here.
//!
//! # WAL ordering
//!
//! Each mutation appends its log record and learns the slot id *before*
//! touching the page, so the log entry always precedes the page change.
//! The page is mutated only in the buffer pool; the pool's WAL hook fsyncs
//! the log through the page's LSN before the page is ever written to disk.

use std::cell::Cell;

use rustdb_storage::{BufferPool, HeapPage, PageHeader, PageId, SlotId};

use crate::error::Result;
use crate::hook::WalSyncHandle;
use crate::lsn::{Lsn, TxnId};
use crate::record::LogRecord;

/// A handle to an in-flight transaction on a [`MiniHeap`].
///
/// Tracks the undo chain (`last_lsn`) and the before-images needed for an
/// in-process [`abort`](MiniHeap::abort).
#[derive(Debug)]
pub struct Txn {
    id: TxnId,
    last_lsn: Lsn,
    /// `(page, slot, before_image)` for each mutation, in apply order. Used
    /// by in-process abort to revert in reverse.
    mutations: Vec<(PageId, SlotId, Vec<u8>)>,
    finished: bool,
}

impl Txn {
    /// The transaction id.
    #[must_use]
    pub const fn id(&self) -> TxnId {
        self.id
    }
}

/// A minimal recoverable heap table over a [`BufferPool`] and a WAL.
///
/// Like [`BufferPool`], `MiniHeap` uses interior mutability so every method
/// takes `&self`: the WAL is shared through a [`WalSyncHandle`], and the two
/// mutable bookkeeping fields live in `Cell`s.
#[derive(Debug)]
pub struct MiniHeap<'p> {
    pool: &'p BufferPool,
    wal: WalSyncHandle,
    next_txn: Cell<u64>,
    /// Page currently receiving inserts. Grows by allocating a new page
    /// when this one is full.
    cur_page: Cell<PageId>,
}

impl<'p> MiniHeap<'p> {
    /// Create a new table: allocate one empty heap page as the insert
    /// target. The `pool` must already have `wal`'s hook installed (via
    /// [`BufferPool::with_wal`](rustdb_storage::BufferPool::with_wal)) so
    /// page flushes respect WAL ordering.
    pub fn create(pool: &'p BufferPool, wal: WalSyncHandle) -> Result<Self> {
        let (cur_page, mut guard) = pool.new_page().map_err(to_io)?;
        HeapPage::init(guard.page_mut());
        drop(guard);
        Ok(Self {
            pool,
            wal,
            next_txn: Cell::new(1),
            cur_page: Cell::new(cur_page),
        })
    }

    /// Open an existing table whose insert page is `cur_page` (e.g. after
    /// recovery). `next_txn` should be one past the highest txn id seen.
    #[must_use]
    pub const fn open(
        pool: &'p BufferPool,
        wal: WalSyncHandle,
        cur_page: PageId,
        next_txn: u64,
    ) -> Self {
        Self {
            pool,
            wal,
            next_txn: Cell::new(next_txn),
            cur_page: Cell::new(cur_page),
        }
    }

    /// The page currently receiving inserts.
    #[must_use]
    pub fn current_page(&self) -> PageId {
        self.cur_page.get()
    }

    /// Begin a transaction. Logs a `Begin` record.
    pub fn begin(&self) -> Result<Txn> {
        let id = TxnId::new(self.next_txn.get());
        self.next_txn.set(self.next_txn.get() + 1);
        let lsn = self
            .wal
            .writer()
            .append(&LogRecord::Begin, id, Lsn::INVALID)?;
        self.wal.writer().fsync_through(lsn)?;
        Ok(Txn {
            id,
            last_lsn: lsn,
            mutations: Vec::new(),
            finished: false,
        })
    }

    /// Insert `tuple`, returning where it landed. Allocates a new page if
    /// the current one is full.
    pub fn insert(&self, txn: &mut Txn, tuple: &[u8]) -> Result<(PageId, SlotId)> {
        assert!(!txn.finished, "insert on a finished transaction");
        // Find a page with room, allocating a fresh one if needed.
        let page_id = self.page_with_room(tuple.len())?;
        // The slot id is the page's current slot_count; learn it before we
        // mutate so the WAL record (which carries slot_id) is written first.
        let slot_id = self.slot_count(page_id)?;

        let lsn = {
            let rec = LogRecord::Update {
                page_id: page_id.get(),
                slot_id,
                before: Vec::new(),
                after: tuple.to_vec(),
            };
            self.wal.writer().append(&rec, txn.id, txn.last_lsn)?
        };

        // Apply to the page and stamp its LSN.
        {
            let mut guard = self.pool.fetch_page_mut(page_id).map_err(to_io)?;
            let mut heap = HeapPage::from_bytes(guard.page_mut()).map_err(to_io)?;
            let assigned = heap.insert(tuple).map_err(to_io)?;
            debug_assert_eq!(assigned, SlotId::new(slot_id));
            stamp_lsn(guard.page_mut(), lsn);
        }

        self.wal.writer().fsync_through(lsn)?;
        txn.last_lsn = lsn;
        txn.mutations
            .push((page_id, SlotId::new(slot_id), Vec::new()));
        Ok((page_id, SlotId::new(slot_id)))
    }

    /// Overwrite an existing tuple, logging its before-image.
    pub fn update(&self, txn: &mut Txn, page: PageId, slot: SlotId, new: &[u8]) -> Result<()> {
        assert!(!txn.finished, "update on a finished transaction");
        let before = self.read_slot(page, slot)?.unwrap_or_default();
        self.apply_logged(txn, page, slot, &before, new)
    }

    /// Delete a tuple (tombstone), logging its before-image.
    pub fn delete(&self, txn: &mut Txn, page: PageId, slot: SlotId) -> Result<()> {
        assert!(!txn.finished, "delete on a finished transaction");
        let before = self.read_slot(page, slot)?.unwrap_or_default();
        self.apply_logged(txn, page, slot, &before, &[])
    }

    /// Commit: log a durable `Commit`. After this returns, the
    /// transaction's effects survive a crash.
    pub fn commit(&self, txn: &mut Txn) -> Result<()> {
        assert!(!txn.finished, "double commit");
        let lsn = self
            .wal
            .writer()
            .append(&LogRecord::Commit, txn.id, txn.last_lsn)?;
        self.wal.writer().fsync_through(lsn)?;
        txn.last_lsn = lsn;
        txn.finished = true;
        Ok(())
    }

    /// Abort: revert this transaction's mutations in reverse (restoring
    /// before-images) and log a durable `Abort`. If the process crashes
    /// mid-abort, recovery treats the txn as a loser and finishes the
    /// rollback, so this is safe without writing CLRs in-process.
    pub fn abort(&self, txn: &mut Txn) -> Result<()> {
        assert!(!txn.finished, "double finish");
        let mutations: Vec<(PageId, SlotId, Vec<u8>)> =
            txn.mutations.iter().rev().cloned().collect();
        for (page, slot, before) in mutations {
            let lsn = self.wal.writer().current_lsn();
            {
                let mut guard = self.pool.fetch_page_mut(page).map_err(to_io)?;
                let mut heap = HeapPage::from_bytes(guard.page_mut()).map_err(to_io)?;
                heap.recover_slot(slot, &before).map_err(to_io)?;
                stamp_lsn(guard.page_mut(), lsn);
            }
        }
        let lsn = self
            .wal
            .writer()
            .append(&LogRecord::Abort, txn.id, txn.last_lsn)?;
        self.wal.writer().fsync_through(lsn)?;
        txn.last_lsn = lsn;
        txn.finished = true;
        Ok(())
    }

    /// Read a slot's current bytes through the pool.
    pub fn read_slot(&self, page: PageId, slot: SlotId) -> Result<Option<Vec<u8>>> {
        let guard = self.pool.fetch_page(page).map_err(to_io)?;
        let mut buf = Box::new([0u8; rustdb_storage::PAGE_SIZE]);
        buf.copy_from_slice(guard.page());
        let heap = HeapPage::from_bytes(&mut buf).map_err(to_io)?;
        Ok(heap.get(slot).map(<[u8]>::to_vec))
    }

    // --- internals ---

    fn apply_logged(
        &self,
        txn: &mut Txn,
        page: PageId,
        slot: SlotId,
        before: &[u8],
        after: &[u8],
    ) -> Result<()> {
        let rec = LogRecord::Update {
            page_id: page.get(),
            slot_id: slot.get(),
            before: before.to_vec(),
            after: after.to_vec(),
        };
        let lsn = self.wal.writer().append(&rec, txn.id, txn.last_lsn)?;
        {
            let mut guard = self.pool.fetch_page_mut(page).map_err(to_io)?;
            let mut heap = HeapPage::from_bytes(guard.page_mut()).map_err(to_io)?;
            heap.recover_slot(slot, after).map_err(to_io)?;
            stamp_lsn(guard.page_mut(), lsn);
        }
        self.wal.writer().fsync_through(lsn)?;
        txn.last_lsn = lsn;
        txn.mutations.push((page, slot, before.to_vec()));
        Ok(())
    }

    /// Slot count of a page (number of slots, live + tombstoned).
    fn slot_count(&self, page: PageId) -> Result<u16> {
        let guard = self.pool.fetch_page(page).map_err(to_io)?;
        let mut buf = Box::new([0u8; rustdb_storage::PAGE_SIZE]);
        buf.copy_from_slice(guard.page());
        let heap = HeapPage::from_bytes(&mut buf).map_err(to_io)?;
        Ok(heap.slot_count())
    }

    /// Return a page that has room for a `tuple_len`-byte tuple, allocating
    /// and initializing a fresh page if the current one is full.
    fn page_with_room(&self, tuple_len: usize) -> Result<PageId> {
        let needed = u16::try_from(tuple_len + rustdb_storage::SLOT_SIZE).unwrap_or(u16::MAX);
        let cur = self.cur_page.get();
        let have = {
            let guard = self.pool.fetch_page(cur).map_err(to_io)?;
            let mut buf = Box::new([0u8; rustdb_storage::PAGE_SIZE]);
            buf.copy_from_slice(guard.page());
            let heap = HeapPage::from_bytes(&mut buf).map_err(to_io)?;
            heap.free_space()
        };
        if have >= needed {
            return Ok(cur);
        }
        // Allocate a fresh insert page.
        let (new_page, mut guard) = self.pool.new_page().map_err(to_io)?;
        HeapPage::init(guard.page_mut());
        drop(guard);
        self.cur_page.set(new_page);
        Ok(new_page)
    }
}

/// Stamp a page header's LSN in place.
fn stamp_lsn(buf: &mut rustdb_storage::Page, lsn: Lsn) {
    let mut h = PageHeader::read(buf).expect("heap header present");
    h.lsn = lsn.get();
    h.write(buf);
}

fn to_io(e: rustdb_storage::StorageError) -> crate::error::WalError {
    crate::error::WalError::Io(std::io::Error::other(e))
}
