//! End-to-end recovery: drive a `MiniHeap` workload, simulate a crash by
//! dropping the buffer pool without flushing, then recover from the WAL and
//! the surviving data file and check the result.
//!
//! The crash model: dropping the pool without `flush_all` loses every dirty
//! page still in memory. That is exactly what a process kill does to pages
//! that never reached disk. Recovery must rebuild committed state from the
//! WAL and roll back anything uncommitted.

use rustdb_storage::{BufferPool, FileManager, HeapPage, PageId, SlotId, PAGE_SIZE};
use rustdb_wal::{recover, MiniHeap, WalSyncHandle, WalWriter};

/// Open a buffer pool over `data` with `wal`'s hook installed.
fn pool_with_wal(data: &std::path::Path, wal: &WalSyncHandle, frames: usize) -> BufferPool {
    let file = FileManager::open(data).expect("open data");
    BufferPool::with_wal(file, frames, wal.as_hook())
}

/// Read a slot through a fresh pool (post-recovery verification).
fn read_slot(data: &std::path::Path, page: u64, slot: u16) -> Option<Vec<u8>> {
    let file = FileManager::open(data).expect("reopen");
    let pool = BufferPool::new(file, 8);
    let guard = pool.fetch_page(PageId::new(page)).expect("fetch");
    let mut buf = Box::new([0u8; PAGE_SIZE]);
    buf.copy_from_slice(guard.page());
    let heap = HeapPage::from_bytes(&mut buf).expect("heap");
    heap.get(SlotId::new(slot)).map(<[u8]>::to_vec)
}

#[test]
fn committed_survives_uncommitted_rolled_back_after_crash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("data.db");
    let wal_path = dir.path().join("wal.log");

    // Where the rows land, captured during the workload for verification.
    let committed_loc;
    let loser_loc;

    // --- Workload phase ---
    {
        let writer = WalWriter::open(&wal_path).expect("wal");
        let wal = WalSyncHandle::new(writer);
        let pool = pool_with_wal(&data, &wal, 16);
        let heap = MiniHeap::create(&pool, wal.clone()).expect("create");

        // Committed transaction.
        let mut t1 = heap.begin().expect("begin t1");
        committed_loc = heap.insert(&mut t1, b"durable-row").expect("insert t1");
        heap.commit(&mut t1).expect("commit t1");

        // Uncommitted transaction: insert but never commit. Its update is
        // fsync'd (MiniHeap fsyncs every record), so recovery WILL see it
        // and must roll it back.
        let mut t2 = heap.begin().expect("begin t2");
        loser_loc = heap.insert(&mut t2, b"ghost-row").expect("insert t2");
        // Drop t2 without commit. Simulate crash next.

        // CRASH: drop the pool WITHOUT flush_all. Dirty pages in memory are
        // lost; only the WAL (fsync'd) survives. We also drop `wal`/`heap`.
        drop(heap);
        drop(pool);
        drop(wal);
        let _ = (t1, t2);
    }

    // At this point data.db may be missing the rows entirely (pages never
    // flushed). Prove recovery rebuilds the committed row and drops the
    // loser.

    // --- Recovery phase ---
    {
        let file = FileManager::open(&data).expect("open data for recovery");
        let pool = BufferPool::new(file, 16);
        let stats = recover(&pool, &wal_path).expect("recover");
        assert_eq!(stats.winners, 1, "one committed txn");
        assert_eq!(stats.losers, 1, "one uncommitted txn");
        assert!(stats.redone >= 2, "both inserts redone (repeat history)");
        assert_eq!(stats.undone, 1, "loser insert undone");
    }

    // --- Verification phase ---
    let (cp, cs) = committed_loc;
    assert_eq!(
        read_slot(&data, cp.get(), cs.get()).as_deref(),
        Some(&b"durable-row"[..]),
        "committed row must survive the crash",
    );
    let (lp, ls) = loser_loc;
    assert_eq!(
        read_slot(&data, lp.get(), ls.get()),
        None,
        "uncommitted row must be rolled back",
    );
}

#[test]
fn recovery_is_idempotent_when_run_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("data.db");
    let wal_path = dir.path().join("wal.log");

    let loc;
    {
        let writer = WalWriter::open(&wal_path).expect("wal");
        let wal = WalSyncHandle::new(writer);
        let pool = pool_with_wal(&data, &wal, 16);
        let heap = MiniHeap::create(&pool, wal.clone()).expect("create");
        let mut t = heap.begin().expect("begin");
        loc = heap.insert(&mut t, b"value").expect("insert");
        heap.commit(&mut t).expect("commit");
        drop(heap);
        drop(pool);
        drop(wal);
        let _ = t;
    }

    // First recovery.
    {
        let file = FileManager::open(&data).expect("open");
        let pool = BufferPool::new(file, 16);
        let s = recover(&pool, &wal_path).expect("recover 1");
        assert_eq!(s.winners, 1);
        assert_eq!(s.losers, 0);
    }
    // Second recovery: pages already durable + LSN-stamped, so redo applies
    // nothing and there are no losers to undo.
    {
        let file = FileManager::open(&data).expect("open");
        let pool = BufferPool::new(file, 16);
        let s = recover(&pool, &wal_path).expect("recover 2");
        assert_eq!(s.redone, 0, "nothing to redo on a recovered db");
        assert_eq!(s.undone, 0, "nothing to undo");
    }

    let (p, sl) = loc;
    assert_eq!(
        read_slot(&data, p.get(), sl.get()).as_deref(),
        Some(&b"value"[..]),
    );
}

#[test]
fn many_committed_rows_across_pages_survive_crash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("data.db");
    let wal_path = dir.path().join("wal.log");

    // Insert enough rows to span multiple pages, committing each.
    let mut locs: Vec<(PageId, SlotId, Vec<u8>)> = Vec::new();
    {
        let writer = WalWriter::open(&wal_path).expect("wal");
        let wal = WalSyncHandle::new(writer);
        // Small pool so eviction happens and exercises the WAL hook.
        let pool = pool_with_wal(&data, &wal, 4);
        let heap = MiniHeap::create(&pool, wal.clone()).expect("create");
        for i in 0..500u32 {
            let mut t = heap.begin().expect("begin");
            let tuple = format!("row-{i:04}-{}", "x".repeat(40)).into_bytes();
            let (p, s) = heap.insert(&mut t, &tuple).expect("insert");
            heap.commit(&mut t).expect("commit");
            locs.push((p, s, tuple));
        }
        // Crash without flushing.
        drop(heap);
        drop(pool);
        drop(wal);
    }

    {
        let file = FileManager::open(&data).expect("open");
        let pool = BufferPool::new(file, 16);
        let s = recover(&pool, &wal_path).expect("recover");
        assert_eq!(s.winners, 500);
        assert_eq!(s.losers, 0);
    }

    // Every committed row must be readable.
    for (p, s, tuple) in &locs {
        assert_eq!(
            read_slot(&data, p.get(), s.get()).as_deref(),
            Some(tuple.as_slice()),
            "row at {p:?}/{s:?} missing after recovery",
        );
    }
}
