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
use std::ops::Bound;

use rustdb_storage::{BTree, BufferPool, HeapPage, Page, PageHeader, PageId, SlotId, TupleRef};
use rustdb_wal::{LogRecord, Lsn, TxnId, WalSyncHandle};

use crate::error::{Result, TxnError};
use crate::manager::{IsolationLevel, Snapshot, Transaction, TransactionManager};
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

    /// Open an existing table: rebuild the handle from the index B+ tree's
    /// root page and the current version heap page. The engine stores those
    /// two ids per table and reconstructs a transient `MvccTable` for each
    /// operation, which is how it avoids holding a self-referential borrow of
    /// the buffer pool.
    #[must_use]
    pub const fn open(
        pool: &'env BufferPool,
        wal: WalSyncHandle,
        mgr: &'env TransactionManager,
        index_root: PageId,
        version_page: PageId,
    ) -> Self {
        Self {
            pool,
            wal,
            mgr,
            index: BTree::open(pool, index_root),
            version_page: Cell::new(version_page),
        }
    }

    /// The index B+ tree's current root page. Changes when the root splits,
    /// so the engine must read it back after a write and persist it.
    #[must_use]
    pub fn index_root(&self) -> PageId {
        self.index.root_page()
    }

    /// The heap page currently receiving new versions. Advances as pages
    /// fill, so the engine persists it back after a write.
    #[must_use]
    pub fn version_page(&self) -> PageId {
        self.version_page.get()
    }

    /// Insert a row. Creates a new version of `key` at the head of the row's
    /// chain.
    ///
    /// If the key already has versions (for example a leftover version from
    /// an aborted transaction), the new version chains onto the current head
    /// rather than starting a fresh chain, so no older version is ever
    /// orphaned and snapshots that should still reach it can.
    pub fn insert(&self, txn: &Transaction, key: u64, value: &[u8]) -> Result<()> {
        let prev = self.index.search(key)?;
        let bytes = Version::encode(txn.xid(), 0, prev, value);
        let new_ref = self.store_version(txn.xid(), &bytes)?;
        self.index.upsert(key, new_ref)?;
        Ok(())
    }

    /// Read the value of `key` visible to `txn`, or `None`.
    pub fn get(&self, txn: &Transaction, key: u64) -> Result<Option<Vec<u8>>> {
        match self.visible_ref(txn, key)? {
            Some(at) => {
                let bytes = self.read_version_bytes(at)?;
                Ok(Some(Version::decode(&bytes)?.payload.to_vec()))
            }
            None => Ok(None),
        }
    }

    /// Update `key` to `value`. Marks the version currently visible to `txn`
    /// as deleted by `txn`, then chains a new version at the head of the row.
    pub fn update(&self, txn: &Transaction, key: u64, value: &[u8]) -> Result<()> {
        let Some(visible) = self.visible_ref(txn, key)? else {
            return Err(TxnError::KeyNotVisible(key));
        };
        // Mark the version this txn could see as deleted by this txn.
        self.stamp_xmax(txn.xid(), visible)?;
        // Chain the new version in front of the current head so no version is
        // dropped from the chain (older snapshots still walk down to theirs).
        let head = self.index.search(key)?.unwrap_or(visible);
        let bytes = Version::encode(txn.xid(), 0, Some(head), value);
        let new_ref = self.store_version(txn.xid(), &bytes)?;
        self.index.upsert(key, new_ref)?;
        Ok(())
    }

    /// Delete `key`. Marks the version currently visible to `txn` as deleted.
    pub fn delete(&self, txn: &Transaction, key: u64) -> Result<()> {
        let Some(visible) = self.visible_ref(txn, key)? else {
            return Err(TxnError::KeyNotVisible(key));
        };
        self.stamp_xmax(txn.xid(), visible)?;
        Ok(())
    }

    /// Return every key and its visible value, in ascending key order: a
    /// snapshot-consistent full scan, which is what `SeqScan` reads.
    ///
    /// Like [`get`](Self::get) applied to every key, but the snapshot is
    /// resolved once up front so the whole scan reflects a single point in
    /// time even under `ReadCommitted`. A key whose visible version is a
    /// delete (no live version under the snapshot) is omitted.
    pub fn scan(&self, txn: &Transaction) -> Result<Vec<(u64, Vec<u8>)>> {
        let snapshot = self.snapshot_for(txn);
        let mut out = Vec::new();
        for entry in self.index.range_scan(Bound::Unbounded, Bound::Unbounded)? {
            let (key, _head) = entry?;
            if let Some(at) = self.chain_visible(&snapshot, txn.xid(), key)? {
                let bytes = self.read_version_bytes(at)?;
                out.push((key, Version::decode(&bytes)?.payload.to_vec()));
            }
        }
        Ok(out)
    }

    /// Walk the version chain for `key` newest-to-oldest and return the
    /// reference of the first version visible to `txn`, or `None`.
    ///
    /// Under `ReadCommitted` the snapshot is refreshed for this statement;
    /// under `RepeatableRead` the begin-time snapshot is reused.
    fn visible_ref(&self, txn: &Transaction, key: u64) -> Result<Option<TupleRef>> {
        let snapshot = self.snapshot_for(txn);
        self.chain_visible(&snapshot, txn.xid(), key)
    }

    /// Resolve the snapshot a read should use: refreshed per statement under
    /// `ReadCommitted`, frozen at begin under `RepeatableRead`.
    fn snapshot_for(&self, txn: &Transaction) -> Snapshot {
        if txn.level() == IsolationLevel::ReadCommitted {
            txn.set_snapshot(self.mgr.current_snapshot());
        }
        txn.snapshot()
    }

    /// Walk `key`'s version chain from the index head, newest-to-oldest, and
    /// return the first version visible under `snapshot`, or `None`.
    fn chain_visible(&self, snapshot: &Snapshot, xid: u64, key: u64) -> Result<Option<TupleRef>> {
        let Some(mut current) = self.index.search(key)? else {
            return Ok(None);
        };
        loop {
            let bytes = self.read_version_bytes(current)?;
            let v = Version::decode(&bytes)?;
            if snapshot.is_visible(v.xmin, v.xmax, self.mgr, xid) {
                return Ok(Some(current));
            }
            match v.prev {
                Some(prev) => current = prev,
                None => return Ok(None),
            }
        }
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
    fn scan_returns_visible_rows_in_key_order() {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let t = e.mgr.begin();
        // Insert out of order; scan must still come back sorted by key.
        table.insert(&t, 3, b"three").expect("insert");
        table.insert(&t, 1, b"one").expect("insert");
        table.insert(&t, 2, b"two").expect("insert");
        e.mgr.commit(&t);

        let reader = e.mgr.begin();
        let rows = table.scan(&reader).expect("scan");
        let got: Vec<(u64, &[u8])> = rows.iter().map(|(k, v)| (*k, v.as_slice())).collect();
        assert_eq!(
            got,
            vec![(1, &b"one"[..]), (2, &b"two"[..]), (3, &b"three"[..])]
        );
    }

    #[test]
    fn scan_skips_deleted_keys_and_honors_snapshot() {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let setup = e.mgr.begin();
        table.insert(&setup, 1, b"a").expect("insert");
        table.insert(&setup, 2, b"b").expect("insert");
        e.mgr.commit(&setup);

        // A's snapshot predates the delete and the new insert.
        let a = e.mgr.begin();

        let w = e.mgr.begin();
        table.delete(&w, 1).expect("delete");
        table.insert(&w, 3, b"c").expect("insert");
        e.mgr.commit(&w);

        // A still sees the original two rows, not the delete or the insert.
        let a_keys: Vec<u64> = table
            .scan(&a)
            .expect("scan")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(a_keys, vec![1, 2], "A's RepeatableRead scan is frozen");

        // A fresh reader sees key 1 gone and key 3 present.
        let c = e.mgr.begin();
        let c_keys: Vec<u64> = table
            .scan(&c)
            .expect("scan")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(c_keys, vec![2, 3], "deleted key absent, new key present");
    }

    #[test]
    fn scan_of_empty_table_is_empty() {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let t = e.mgr.begin();
        assert!(table.scan(&t).expect("scan").is_empty());
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
