//! `MvccTable`: a key/value table with multi-version concurrency control.
//!
//! Every write creates a new *version* rather than overwriting in place, and
//! every read returns the newest version visible to the reader's snapshot.
//! Concurrent readers never block writers and never see uncommitted or
//! future changes. This is the evidence for must-have requirement M5.
//!
//! # Structure
//!
//! - A **B+ tree index** maps each `key` to a [`TupleRef`] pointing at the
//!   *newest* version of that row.
//! - Versions live in **heap pages** as [`Version`]-encoded bytes, chained
//!   newest-to-oldest via each version's `prev` pointer.
//! - The [`TransactionManager`] supplies xids and snapshots; the
//!   [`visibility`](crate::visibility) rule decides which version a reader
//!   sees.
//!
//! # Operation summary
//!
//! - `insert`: write a new version `(xmin = txn, xmax = 0, prev = None)`;
//!   point the index at it.
//! - `update`: stamp the current newest version's `xmax = txn` (marking it
//!   deleted by this txn), write a new version whose `prev` is the old one,
//!   and re-point the index.
//! - `delete`: stamp the newest version's `xmax = txn`.
//! - `get`: from the index, walk the version chain newest-to-oldest and
//!   return the first version visible to the txn's snapshot.
//!
//! Every write is logged to the WAL (an `Update` record) before the page is
//! mutated, so versions are durable.
//!
//! # Scope (Sprint 5)
//!
//! This delivers snapshot-stable concurrent reads (M5). Write-write conflict
//! detection between concurrent writers to the same key (first-committer-
//! wins / serializable) is a Sprint 6 concern, and MVCC-aware crash recovery
//! (rebuilding the index, undoing versions) integrates with the executor in
//! a later sprint.

use std::cell::Cell;

use rustdb_storage::{BTree, BufferPool, HeapPage, Page, PageHeader, PageId, SlotId, TupleRef};
use rustdb_wal::{LogRecord, Lsn, TxnId, WalSyncHandle};

use crate::error::{Result, TxnError};
use crate::manager::{IsolationLevel, Transaction, TransactionManager};
use crate::version::{set_xmax, Version};

/// A multi-version key/value table. See the module docs.
#[derive(Debug)]
pub struct MvccTable<'env> {
    pool: &'env BufferPool,
    wal: WalSyncHandle,
    mgr: &'env TransactionManager,
    /// key -> newest version ref.
    index: BTree<'env>,
    /// Heap page currently receiving new versions.
    version_page: Cell<PageId>,
}

impl<'env> MvccTable<'env> {
    /// Create a fresh table: a B+ tree index plus one heap page for
    /// versions. `pool` should have `wal`'s hook installed so page flushes
    /// respect WAL ordering.
    pub fn create(
        pool: &'env BufferPool,
        wal: WalSyncHandle,
        mgr: &'env TransactionManager,
    ) -> Result<Self> {
        let index = BTree::create(pool)?;
        let (version_page, mut guard) = pool.new_page()?;
        HeapPage::init(guard.page_mut());
        drop(guard);
        Ok(Self {
            pool,
            wal,
            mgr,
            index,
            version_page: Cell::new(version_page),
        })
    }

    /// Insert a new row. Creates the first version of `key`.
    pub fn insert(&self, txn: &Transaction, key: u64, value: &[u8]) -> Result<()> {
        let bytes = Version::encode(txn.xid(), 0, None, value);
        let new_ref = self.store_version(txn.xid(), &bytes)?;
        self.index.upsert(key, new_ref)?;
        Ok(())
    }

    /// Read the value of `key` visible to `txn`, or `None`.
    pub fn get(&self, txn: &Transaction, key: u64) -> Result<Option<Vec<u8>>> {
        // Under ReadCommitted each statement sees a fresh snapshot, so it
        // observes commits that landed after the transaction began. Under
        // RepeatableRead the begin-time snapshot is reused unchanged.
        if txn.level() == IsolationLevel::ReadCommitted {
            txn.set_snapshot(self.mgr.current_snapshot());
        }
        let snapshot = txn.snapshot();
        let Some(mut current) = self.index.search(key)? else {
            return Ok(None);
        };
        loop {
            let bytes = self.read_version_bytes(current)?;
            let v = Version::decode(&bytes)?;
            if snapshot.is_visible(v.xmin, v.xmax, self.mgr, txn.xid()) {
                return Ok(Some(v.payload.to_vec()));
            }
            match v.prev {
                Some(prev) => current = prev,
                None => return Ok(None),
            }
        }
    }

    /// Update `key` to `value`. Stamps the current newest version as deleted
    /// by `txn` and chains a new version in front of it.
    pub fn update(&self, txn: &Transaction, key: u64, value: &[u8]) -> Result<()> {
        let Some(head) = self.index.search(key)? else {
            return Err(TxnError::KeyNotVisible(key));
        };
        self.stamp_xmax(txn.xid(), head)?;
        let bytes = Version::encode(txn.xid(), 0, Some(head), value);
        let new_ref = self.store_version(txn.xid(), &bytes)?;
        self.index.upsert(key, new_ref)?;
        Ok(())
    }

    /// Delete `key`. Stamps the newest version's `xmax` with `txn`.
    pub fn delete(&self, txn: &Transaction, key: u64) -> Result<()> {
        let Some(head) = self.index.search(key)? else {
            return Err(TxnError::KeyNotVisible(key));
        };
        self.stamp_xmax(txn.xid(), head)?;
        Ok(())
    }

    // --- internals ---

    /// Store a version's bytes in a heap page, logging the write first.
    /// Returns where it landed.
    fn store_version(&self, xid: u64, bytes: &[u8]) -> Result<TupleRef> {
        let page = self.page_with_room(bytes.len())?;
        let slot = self.slot_count(page)?;

        // WAL-before-page: log the insert (with the slot we are about to use)
        // before touching the page.
        let rec = LogRecord::Update {
            page_id: page.get(),
            slot_id: slot,
            before: Vec::new(),
            after: bytes.to_vec(),
        };
        let lsn = self
            .wal
            .writer()
            .append(&rec, TxnId::new(xid), Lsn::INVALID)?;

        {
            let mut guard = self.pool.fetch_page_mut(page)?;
            let mut heap = HeapPage::from_bytes(guard.page_mut())?;
            let assigned = heap.insert(bytes)?;
            debug_assert_eq!(assigned, SlotId::new(slot));
            stamp_page_lsn(guard.page_mut(), lsn);
        }
        self.wal.writer().fsync_through(lsn)?;
        Ok(TupleRef::new(page, SlotId::new(slot)))
    }

    /// Stamp the `xmax` of an existing version in place, logging the change.
    fn stamp_xmax(&self, xid: u64, target: TupleRef) -> Result<()> {
        let before = self.read_version_bytes(target)?;
        let mut after = before.clone();
        set_xmax(&mut after, xid)?;

        let rec = LogRecord::Update {
            page_id: target.page_id.get(),
            slot_id: target.slot_id.get(),
            before,
            after: after.clone(),
        };
        let lsn = self
            .wal
            .writer()
            .append(&rec, TxnId::new(xid), Lsn::INVALID)?;
        {
            let mut guard = self.pool.fetch_page_mut(target.page_id)?;
            let mut heap = HeapPage::from_bytes(guard.page_mut())?;
            heap.recover_slot(target.slot_id, &after)?;
            stamp_page_lsn(guard.page_mut(), lsn);
        }
        self.wal.writer().fsync_through(lsn)?;
        Ok(())
    }

    /// Copy a version's raw bytes out of its heap slot.
    fn read_version_bytes(&self, at: TupleRef) -> Result<Vec<u8>> {
        let guard = self.pool.fetch_page(at.page_id)?;
        let mut buf: Box<Page> = Box::new([0u8; rustdb_storage::PAGE_SIZE]);
        buf.copy_from_slice(guard.page());
        let heap = HeapPage::from_bytes(&mut buf)?;
        heap.get(at.slot_id).map(<[u8]>::to_vec).ok_or(
            // A version slot is never tombstoned, so a missing slot means
            // a dangling chain pointer (corruption).
            TxnError::VersionTruncated {
                len: 0,
                min: crate::version::VERSION_HEADER_SIZE,
            },
        )
    }

    fn slot_count(&self, page: PageId) -> Result<u16> {
        let guard = self.pool.fetch_page(page)?;
        let mut buf: Box<Page> = Box::new([0u8; rustdb_storage::PAGE_SIZE]);
        buf.copy_from_slice(guard.page());
        let heap = HeapPage::from_bytes(&mut buf)?;
        Ok(heap.slot_count())
    }

    fn page_with_room(&self, len: usize) -> Result<PageId> {
        let needed = u16::try_from(len + rustdb_storage::SLOT_SIZE).unwrap_or(u16::MAX);
        let cur = self.version_page.get();
        let have = {
            let guard = self.pool.fetch_page(cur)?;
            let mut buf: Box<Page> = Box::new([0u8; rustdb_storage::PAGE_SIZE]);
            buf.copy_from_slice(guard.page());
            HeapPage::from_bytes(&mut buf)?.free_space()
        };
        if have >= needed {
            return Ok(cur);
        }
        let (new_page, mut guard) = self.pool.new_page()?;
        HeapPage::init(guard.page_mut());
        drop(guard);
        self.version_page.set(new_page);
        Ok(new_page)
    }
}

/// Stamp a page header's LSN in place so recovery's gate is satisfied.
fn stamp_page_lsn(buf: &mut Page, lsn: Lsn) {
    let mut h = PageHeader::read(buf).expect("heap header present");
    h.lsn = lsn.get();
    h.write(buf);
}

/// The default isolation level for tables created without an explicit one.
/// Re-exported here for callers building MVCC tables.
pub const DEFAULT_ISOLATION: IsolationLevel = IsolationLevel::RepeatableRead;

#[cfg(test)]
mod tests {
    use super::*;
    use rustdb_storage::FileManager;
    use rustdb_wal::WalWriter;
    use tempfile::TempDir;

    struct Env {
        _dir: TempDir,
        pool: BufferPool,
        wal: WalSyncHandle,
        mgr: TransactionManager,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = WalWriter::open(dir.path().join("wal.log")).expect("wal");
        let wal = WalSyncHandle::new(writer);
        let file = FileManager::open(dir.path().join("data.db")).expect("data");
        let pool = BufferPool::with_wal(file, 64, wal.as_hook());
        Env {
            _dir: dir,
            pool,
            wal,
            mgr: TransactionManager::new(),
        }
    }

    #[test]
    fn insert_then_get_in_same_txn() {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let t = e.mgr.begin();
        table.insert(&t, 1, b"hello").expect("insert");
        assert_eq!(
            table.get(&t, 1).expect("get").as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(table.get(&t, 2).expect("get missing"), None);
    }

    #[test]
    fn m5_demo_reader_keeps_stable_snapshot_across_concurrent_commit() {
        // The headline M5 evidence.
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");

        // Setup: commit key 1 = "v1".
        let setup = e.mgr.begin();
        table.insert(&setup, 1, b"v1").expect("insert v1");
        e.mgr.commit(&setup);

        // A begins and reads "v1".
        let a = e.mgr.begin();
        assert_eq!(
            table.get(&a, 1).expect("a get").as_deref(),
            Some(&b"v1"[..])
        );

        // B updates 1 -> "v2" and commits, concurrently with A.
        let b = e.mgr.begin();
        table.update(&b, 1, b"v2").expect("update");
        e.mgr.commit(&b);

        // A re-reads and STILL sees "v1": its snapshot is stable, and B is
        // not in A's snapshot. A never blocked B.
        assert_eq!(
            table.get(&a, 1).expect("a re-get").as_deref(),
            Some(&b"v1"[..]),
            "A's repeatable-read snapshot must not see B's commit",
        );

        // A new transaction C, beginning after B committed, sees "v2".
        let c = e.mgr.begin();
        assert_eq!(
            table.get(&c, 1).expect("c get").as_deref(),
            Some(&b"v2"[..])
        );
    }

    #[test]
    fn update_builds_a_version_chain() {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let t1 = e.mgr.begin();
        table.insert(&t1, 1, b"one").expect("insert");
        e.mgr.commit(&t1);

        let t2 = e.mgr.begin();
        table.update(&t2, 1, b"two").expect("update");
        table.update(&t2, 1, b"three").expect("update again");
        e.mgr.commit(&t2);

        let reader = e.mgr.begin();
        assert_eq!(
            table.get(&reader, 1).expect("get").as_deref(),
            Some(&b"three"[..]),
            "newest committed version wins",
        );
    }

    #[test]
    fn delete_hides_from_later_snapshots() {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let t1 = e.mgr.begin();
        table.insert(&t1, 1, b"doomed").expect("insert");
        e.mgr.commit(&t1);

        // A begins BEFORE the delete, so it still sees the row.
        let a = e.mgr.begin();

        let t2 = e.mgr.begin();
        table.delete(&t2, 1).expect("delete");
        e.mgr.commit(&t2);

        // A (snapshot before delete) still sees it; C (after) does not.
        assert_eq!(
            table.get(&a, 1).expect("a get").as_deref(),
            Some(&b"doomed"[..])
        );
        let c = e.mgr.begin();
        assert_eq!(table.get(&c, 1).expect("c get"), None);
    }

    #[test]
    fn aborted_writers_version_is_never_visible() {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let doomed = e.mgr.begin();
        table.insert(&doomed, 1, b"never").expect("insert");
        e.mgr.abort(&doomed);

        let reader = e.mgr.begin();
        assert_eq!(
            table.get(&reader, 1).expect("get"),
            None,
            "an aborted insert must be invisible",
        );
    }

    #[test]
    fn update_missing_key_errors() {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let t = e.mgr.begin();
        let err = table.update(&t, 99, b"x").expect_err("must error");
        assert!(matches!(err, TxnError::KeyNotVisible(99)));
    }

    #[test]
    fn repeatable_read_does_not_see_concurrent_commit() {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let setup = e.mgr.begin();
        table.insert(&setup, 1, b"v1").expect("insert");
        e.mgr.commit(&setup);

        let a = e.mgr.begin_with(IsolationLevel::RepeatableRead);
        assert_eq!(table.get(&a, 1).expect("get").as_deref(), Some(&b"v1"[..]));

        let b = e.mgr.begin();
        table.update(&b, 1, b"v2").expect("update");
        e.mgr.commit(&b);

        // RepeatableRead: A's snapshot is frozen, still "v1".
        assert_eq!(
            table.get(&a, 1).expect("re-get").as_deref(),
            Some(&b"v1"[..]),
            "RepeatableRead must not see B's concurrent commit",
        );
    }

    #[test]
    fn read_committed_sees_concurrent_commit_after_it_lands() {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let setup = e.mgr.begin();
        table.insert(&setup, 1, b"v1").expect("insert");
        e.mgr.commit(&setup);

        let a = e.mgr.begin_with(IsolationLevel::ReadCommitted);
        assert_eq!(table.get(&a, 1).expect("get").as_deref(), Some(&b"v1"[..]));

        let b = e.mgr.begin();
        table.update(&b, 1, b"v2").expect("update");
        e.mgr.commit(&b);

        // ReadCommitted: A refreshes its snapshot per statement, so the next
        // read sees B's committed value.
        assert_eq!(
            table.get(&a, 1).expect("re-get").as_deref(),
            Some(&b"v2"[..]),
            "ReadCommitted must see B's commit on the next statement",
        );
    }

    #[test]
    fn read_committed_still_hides_uncommitted_writes() {
        // ReadCommitted refreshes the snapshot, but an in-progress writer's
        // changes are still invisible until that writer commits.
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let setup = e.mgr.begin();
        table.insert(&setup, 1, b"v1").expect("insert");
        e.mgr.commit(&setup);

        let a = e.mgr.begin_with(IsolationLevel::ReadCommitted);
        let b = e.mgr.begin();
        table.update(&b, 1, b"v2").expect("update"); // NOT committed
        assert_eq!(
            table.get(&a, 1).expect("get").as_deref(),
            Some(&b"v1"[..]),
            "uncommitted writes stay invisible even under ReadCommitted",
        );
    }
}
