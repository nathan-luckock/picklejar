//! ARIES-style crash recovery: analysis, redo, and undo.
//!
//! Recovery runs in three phases after a crash:
//!
//! 1. **Analysis** ([`analyze`]) scans the WAL and rebuilds the transaction
//!    table: which transactions existed, their last LSN, and whether they
//!    committed. Committed transactions are *winners*; everything else is a
//!    *loser* to be rolled back.
//! 2. **Redo** ([`redo`]) replays *history*: every `Update` and `Clr` is
//!    re-applied to the page it touched, gated on the page's stored LSN so
//!    the replay is idempotent. Redo runs for winners and losers alike, so
//!    that undo starts from a known state.
//! 3. **Undo** ([`undo`]) rolls back losers, walking each one's `prev_lsn`
//!    chain backward and writing a CLR per reverted update.
//!
//! # Why redo replays losers too
//!
//! ARIES "repeats history": it brings the database to the exact state it
//! was in at the moment of the crash, including the effects of transactions
//! that had not yet committed, and *then* undoes the losers. This is
//! simpler and more robust than trying to selectively redo only winners,
//! because undo can assume every logged change is present before it starts
//! walking backward.
//!
//! # Idempotency
//!
//! Each data page stores the LSN of the last log record applied to it (in
//! its header). Redo applies a record only when the page's stored LSN is
//! strictly less than the record's LSN. Replaying the same log twice, or
//! crashing partway through redo and rerunning, produces the same result.

use std::collections::HashMap;
use std::path::Path;

use picklejar_storage::{BufferPool, HeapPage, PageHeader, PageId, PageType, SlotId};

use crate::error::Result;
use crate::lsn::{Lsn, TxnId};
use crate::reader::WalReader;
use crate::record::{LogRecord, RecordHeader, RecordKind};
use crate::writer::WalWriter;

/// Per-transaction state rebuilt during analysis.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TxnStatus {
    /// Highest LSN seen for this transaction. Undo starts here and walks
    /// back via each record's `prev_lsn`.
    pub last_lsn: Lsn,
    /// True once a `Commit` record was seen. Committed transactions are
    /// winners; everything else is a loser to roll back.
    pub committed: bool,
}

/// Output of the analysis phase.
#[derive(Clone, Debug)]
pub struct Analysis {
    /// Transaction table keyed by transaction id.
    pub txns: HashMap<u64, TxnStatus>,
    /// Highest LSN observed anywhere in the log, or [`Lsn::INVALID`] for an
    /// empty log.
    pub max_lsn: Lsn,
}

impl Default for Analysis {
    fn default() -> Self {
        Self {
            txns: HashMap::new(),
            max_lsn: Lsn::INVALID,
        }
    }
}

impl Analysis {
    /// Transaction ids that committed (winners).
    #[must_use]
    pub fn winners(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .txns
            .iter()
            .filter(|(_, s)| s.committed)
            .map(|(id, _)| *id)
            .collect();
        v.sort_unstable();
        v
    }

    /// Transaction ids that did not commit (losers, to be undone).
    #[must_use]
    pub fn losers(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .txns
            .iter()
            .filter(|(_, s)| !s.committed)
            .map(|(id, _)| *id)
            .collect();
        v.sort_unstable();
        v
    }
}

/// Scan the WAL and rebuild the transaction table.
///
/// Records are read in LSN order, so each transaction's `last_lsn` is just
/// the LSN of the most recent record seen for it. `Checkpoint` records are
/// metadata and do not belong to a real transaction, so they are skipped.
pub fn analyze<P: AsRef<Path>>(wal_path: P) -> Result<Analysis> {
    let reader = WalReader::open(wal_path)?;
    let mut analysis = Analysis::default();

    for item in reader {
        let (hdr, rec) = item?;
        if hdr.lsn.get() > analysis.max_lsn.get() || analysis.max_lsn.is_invalid() {
            analysis.max_lsn = hdr.lsn;
        }
        // Checkpoint, Catalog, and RlsPolicies records are metadata, not part
        // of any transaction, so they never create a transaction-table entry (a
        // metadata record carries a sentinel txn and must not be mistaken for an
        // uncommitted loser and rolled back).
        if matches!(
            rec,
            LogRecord::Checkpoint { .. }
                | LogRecord::Catalog { .. }
                | LogRecord::RlsPolicies { .. }
        ) {
            continue;
        }
        let entry = analysis.txns.entry(hdr.txn.get()).or_insert(TxnStatus {
            last_lsn: hdr.lsn,
            committed: false,
        });
        entry.last_lsn = hdr.lsn;
        if matches!(rec, LogRecord::Commit) {
            entry.committed = true;
        }
    }
    Ok(analysis)
}

/// Replay history: re-apply every `Update` and `Clr` to its page, gated on
/// the page's stored LSN. Returns the number of records actually applied
/// (records skipped by the LSN gate are not counted).
pub fn redo(pool: &BufferPool, wal_path: impl AsRef<Path>) -> Result<usize> {
    let reader = WalReader::open(wal_path)?;
    let mut applied = 0usize;
    for item in reader {
        let (hdr, rec) = item?;
        // Both Update (after-image) and CLR (undo-image) apply an image to
        // a page slot. Begin / Commit / Abort / Checkpoint have no page
        // effect.
        let target = match rec {
            LogRecord::Update {
                page_id,
                slot_id,
                after,
                ..
            } => Some((page_id, slot_id, after)),
            LogRecord::Clr {
                page_id,
                slot_id,
                undo_image,
                ..
            } => Some((page_id, slot_id, undo_image)),
            _ => None,
        };
        if let Some((page_id, slot_id, image)) = target {
            applied += usize::from(apply_image(pool, page_id, slot_id, &image, hdr.lsn)?);
        }
    }
    Ok(applied)
}

/// Apply one image (after-image for an Update, undo-image for a CLR) to a
/// page slot, gated on the page LSN. Returns `true` if the image was
/// applied, `false` if the gate skipped it (page already at or past this
/// record).
///
/// The page is materialized via [`BufferPool::ensure_allocated`] so redo
/// works even when the crashed data file never persisted the page.
pub(crate) fn apply_image(
    pool: &BufferPool,
    page_id: u64,
    slot_id: u16,
    image: &[u8],
    rec_lsn: Lsn,
) -> Result<bool> {
    let mut guard = pool.ensure_allocated(PageId::new(page_id)).map_err(to_io)?;

    // Read the page's current LSN. A freshly-allocated (all-zero) page is
    // PageType::Free with LSN 0, which always loses the gate, so its first
    // logged mutation is applied.
    let page_lsn = PageHeader::read(guard.page()).map_or(0, |h| h.lsn);
    if page_lsn >= rec_lsn.get() {
        return Ok(false);
    }

    {
        let buf = guard.page_mut();
        // Ensure the page is a heap page before applying. A never-written
        // page is Free and must be initialized; a partially-flushed page is
        // already Heap and must be preserved.
        let is_heap = matches!(PageHeader::read(buf), Ok(h) if h.page_type == PageType::Heap);
        if !is_heap {
            HeapPage::init(buf);
        }
        let mut heap = HeapPage::from_bytes(buf).map_err(to_io)?;
        heap.recover_slot(SlotId::new(slot_id), image)
            .map_err(to_io)?;
    }

    // Stamp the page LSN so re-running redo skips this record next time.
    {
        let buf = guard.page_mut();
        let mut h = PageHeader::read(buf).expect("heap header present after recover_slot");
        h.lsn = rec_lsn.get();
        h.write(buf);
    }
    Ok(true)
}

/// Summary of a recovery run, returned by [`recover`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RecoveryStats {
    /// Committed transactions found during analysis.
    pub winners: usize,
    /// Uncommitted transactions rolled back.
    pub losers: usize,
    /// Records re-applied during redo.
    pub redone: usize,
    /// Updates reverted during undo (CLRs written).
    pub undone: usize,
}

/// Run all three recovery phases against `pool` and `wal_path`, then flush
/// the recovered pages so the result is durable. Returns a summary.
///
/// `pool` may be a plain pool with no WAL hook: everything the recovered
/// pages reference is already fsync'd (winner records were durable before
/// the crash; undo fsyncs its CLRs), so the final `flush_all` is safe.
///
/// Recovery is idempotent: running it twice over the same WAL converges to
/// the same state and the second run redoes/undoes nothing new.
pub fn recover(pool: &BufferPool, wal_path: impl AsRef<Path>) -> Result<RecoveryStats> {
    let wal_path = wal_path.as_ref();
    let analysis = analyze(wal_path)?;
    let redone = redo(pool, wal_path)?;
    let mut writer = WalWriter::open(wal_path)?;
    let undone = undo(pool, &mut writer, &analysis, wal_path)?;
    pool.flush_all().map_err(to_io)?;
    Ok(RecoveryStats {
        winners: analysis.winners().len(),
        losers: analysis.losers().len(),
        redone,
        undone,
    })
}

/// Roll back every loser transaction, writing CLRs as it goes.
///
/// For each loser, walk its log records backward from `last_lsn` via the
/// `prev_lsn` chain. Each `Update` is reverted by applying its before-image
/// and appending a CLR whose `undo_next` points at the next record to undo.
/// `Clr` records encountered (left by a previous crashed undo) are skipped
/// straight to their `undo_next`, so undo never re-does work already
/// compensated. A loser whose last record is already an `Abort` is fully
/// rolled back and is skipped entirely.
///
/// Returns the number of CLRs written (i.e. the number of updates undone).
///
/// CLRs and the terminating `Abort` are fsync'd, so a crash mid-undo is
/// recoverable: re-running redo replays the CLRs already on disk, and undo
/// resumes from the last CLR's `undo_next`.
pub fn undo(
    pool: &BufferPool,
    writer: &mut WalWriter,
    analysis: &Analysis,
    wal_path: impl AsRef<Path>,
) -> Result<usize> {
    let records = read_all(wal_path)?;
    let mut clrs_written = 0usize;

    for txn_id in analysis.losers() {
        let status = analysis.txns[&txn_id];

        // Already fully rolled back? (last record is an Abort.)
        if let Some((hdr, _)) = records.get(&status.last_lsn.get()) {
            if hdr.kind == RecordKind::Abort {
                continue;
            }
        }

        let txn = TxnId::new(txn_id);
        // The undo chain's log tail: each CLR we append links to the prior
        // record so the transaction's on-disk chain stays connected.
        let mut chain_tail = status.last_lsn;
        let mut next = status.last_lsn;

        while !next.is_invalid() {
            let Some((hdr, rec)) = records.get(&next.get()) else {
                break; // chain points outside the log; stop defensively
            };
            match rec {
                LogRecord::Update {
                    page_id,
                    slot_id,
                    before,
                    ..
                } => {
                    let clr = LogRecord::Clr {
                        page_id: *page_id,
                        slot_id: *slot_id,
                        undo_image: before.clone(),
                        undo_next: hdr.prev_lsn.get(),
                    };
                    let clr_lsn = writer.append(&clr, txn, chain_tail)?;
                    writer.fsync_through(clr_lsn)?;
                    // Apply the before-image, stamping the page with the
                    // CLR's LSN so redo of this CLR is idempotent.
                    apply_image(pool, *page_id, *slot_id, before, clr_lsn)?;
                    clrs_written += 1;
                    chain_tail = clr_lsn;
                    next = hdr.prev_lsn;
                }
                LogRecord::Clr { undo_next, .. } => {
                    // Region already compensated by a prior undo; skip it.
                    next = Lsn::new(*undo_next);
                }
                LogRecord::Begin => break,
                // Commit/Abort/Checkpoint should not appear mid-chain for a
                // loser, but follow the link defensively.
                _ => next = hdr.prev_lsn,
            }
        }

        // Mark the transaction fully rolled back.
        let abort_lsn = writer.append(&LogRecord::Abort, txn, chain_tail)?;
        writer.fsync_through(abort_lsn)?;
    }

    Ok(clrs_written)
}

/// Read every complete record in the WAL into a map keyed by LSN. Used by
/// undo for random access while walking `prev_lsn` chains backward.
fn read_all(wal_path: impl AsRef<Path>) -> Result<HashMap<u64, (RecordHeader, LogRecord)>> {
    let reader = WalReader::open(wal_path)?;
    let mut map = HashMap::new();
    for item in reader {
        let (hdr, rec) = item?;
        map.insert(hdr.lsn.get(), (hdr, rec));
    }
    Ok(map)
}

/// Map a storage error into a WAL error. Recovery surfaces storage failures
/// as I/O errors since they all mean "the page layer could not complete a
/// recovery operation".
fn to_io(e: picklejar_storage::StorageError) -> crate::error::WalError {
    crate::error::WalError::Io(std::io::Error::other(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsn::TxnId;
    use crate::writer::WalWriter;
    use picklejar_storage::{FileManager, SlotId, PAGE_SIZE};
    use tempfile::TempDir;

    /// Build a WAL on disk from a list of `(record, txn, prev_lsn)`
    /// triples, returning the directory (kept alive) and the wal path.
    fn build_wal(records: &[(LogRecord, u64, u64)]) -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal_path = dir.path().join("wal.log");
        let mut w = WalWriter::open(&wal_path).expect("open wal");
        let mut last = Lsn::INVALID;
        for (rec, txn, prev) in records {
            let prev_lsn = if *prev == u64::MAX {
                Lsn::INVALID
            } else {
                Lsn::new(*prev)
            };
            last = w.append(rec, TxnId::new(*txn), prev_lsn).expect("append");
        }
        w.fsync_through(last).expect("fsync");
        (dir, wal_path)
    }

    fn fresh_pool(dir: &TempDir, name: &str, frames: usize) -> BufferPool {
        let file = FileManager::open(dir.path().join(name)).expect("open data");
        BufferPool::new(file, frames)
    }

    fn upd(page_id: u64, slot_id: u16, before: &[u8], after: &[u8]) -> LogRecord {
        LogRecord::Update {
            page_id,
            slot_id,
            before: before.to_vec(),
            after: after.to_vec(),
        }
    }

    #[test]
    fn analyze_separates_winners_and_losers() {
        // txn 1 commits, txn 2 does not.
        let (_dir, wal) = build_wal(&[
            (LogRecord::Begin, 1, u64::MAX),
            (upd(0, 0, b"", b"alpha"), 1, 1),
            (LogRecord::Commit, 1, 2),
            (LogRecord::Begin, 2, u64::MAX),
            (upd(0, 1, b"", b"bravo"), 2, 4),
        ]);
        let a = analyze(&wal).expect("analyze");
        assert_eq!(a.winners(), vec![1]);
        assert_eq!(a.losers(), vec![2]);
        assert_eq!(a.max_lsn, Lsn::new(5));
        assert_eq!(a.txns[&1].last_lsn, Lsn::new(3));
        assert_eq!(a.txns[&2].last_lsn, Lsn::new(5));
    }

    #[test]
    fn redo_reconstructs_committed_insert() {
        let (dir, wal) = build_wal(&[
            (LogRecord::Begin, 1, u64::MAX),
            (upd(0, 0, b"", b"hello"), 1, 1),
            (LogRecord::Commit, 1, 2),
        ]);
        let pool = fresh_pool(&dir, "data.db", 16);
        let applied = redo(&pool, &wal).expect("redo");
        assert_eq!(applied, 1, "one Update should be applied");
        let guard = pool.fetch_page(PageId::new(0)).expect("fetch");
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        buf.copy_from_slice(guard.page());
        let heap = HeapPage::from_bytes(&mut buf).expect("heap");
        assert_eq!(heap.get(SlotId::new(0)), Some(&b"hello"[..]));
    }

    #[test]
    fn redo_is_idempotent() {
        let (dir, wal) = build_wal(&[
            (LogRecord::Begin, 1, u64::MAX),
            (upd(0, 0, b"", b"one"), 1, 1),
            (upd(0, 1, b"", b"two"), 1, 2),
            (LogRecord::Commit, 1, 3),
        ]);
        let pool = fresh_pool(&dir, "data.db", 16);
        let first = redo(&pool, &wal).expect("redo 1");
        assert_eq!(first, 2);
        // Second pass: page LSN now equals the last update's LSN, so the
        // gate skips both updates.
        let second = redo(&pool, &wal).expect("redo 2");
        assert_eq!(second, 0, "second redo applies nothing (idempotent)");
        let guard = pool.fetch_page(PageId::new(0)).expect("fetch");
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        buf.copy_from_slice(guard.page());
        let heap = HeapPage::from_bytes(&mut buf).expect("heap");
        assert_eq!(heap.get(SlotId::new(0)), Some(&b"one"[..]));
        assert_eq!(heap.get(SlotId::new(1)), Some(&b"two"[..]));
    }

    #[test]
    fn redo_gate_skips_already_durable_page() {
        // Pre-stamp the page with a high LSN, as if it had been flushed
        // after the update. Redo must not re-apply.
        let (dir, wal) = build_wal(&[
            (LogRecord::Begin, 1, u64::MAX),
            (upd(0, 0, b"", b"skipme"), 1, 1),
            (LogRecord::Commit, 1, 2),
        ]);
        let pool = fresh_pool(&dir, "data.db", 16);
        {
            // Materialize page 0 as an empty heap page with LSN 999.
            let mut guard = pool.ensure_allocated(PageId::new(0)).expect("alloc");
            let buf = guard.page_mut();
            HeapPage::init(buf);
            let mut h = PageHeader::read(buf).unwrap();
            h.lsn = 999;
            h.write(buf);
        }
        let applied = redo(&pool, &wal).expect("redo");
        assert_eq!(applied, 0, "update LSN 2 < page LSN 999, must skip");
        let guard = pool.fetch_page(PageId::new(0)).expect("fetch");
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        buf.copy_from_slice(guard.page());
        let heap = HeapPage::from_bytes(&mut buf).expect("heap");
        assert_eq!(heap.get(SlotId::new(0)), None, "no tuple should be present");
    }

    /// Read a slot's bytes off a page through the pool (test helper).
    fn read_slot(pool: &BufferPool, page_id: u64, slot: u16) -> Option<Vec<u8>> {
        let guard = pool.fetch_page(PageId::new(page_id)).expect("fetch");
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        buf.copy_from_slice(guard.page());
        let heap = HeapPage::from_bytes(&mut buf).expect("heap");
        heap.get(SlotId::new(slot)).map(<[u8]>::to_vec)
    }

    #[test]
    fn undo_rolls_back_uncommitted_insert() {
        // txn 1 inserts but never commits. After redo+undo the row is gone.
        let (dir, wal) = build_wal(&[
            (LogRecord::Begin, 1, u64::MAX),
            (upd(0, 0, b"", b"ghost"), 1, 1),
        ]);
        let pool = fresh_pool(&dir, "data.db", 16);
        redo(&pool, &wal).expect("redo");
        // After redo, the (uncommitted) insert is present (repeat history).
        assert_eq!(read_slot(&pool, 0, 0).as_deref(), Some(&b"ghost"[..]));

        let analysis = analyze(&wal).expect("analyze");
        assert_eq!(analysis.losers(), vec![1]);
        let mut writer = WalWriter::open(&wal).expect("writer");
        let undone = undo(&pool, &mut writer, &analysis, &wal).expect("undo");
        assert_eq!(undone, 1);
        // The insert has been rolled back to a tombstone.
        assert_eq!(read_slot(&pool, 0, 0), None);
    }

    #[test]
    fn undo_reverts_uncommitted_update_to_before_image() {
        // txn 1 commits an insert; txn 2 updates it but does not commit.
        let (dir, wal) = build_wal(&[
            (LogRecord::Begin, 1, u64::MAX),
            (upd(0, 0, b"", b"committed-value"), 1, 1),
            (LogRecord::Commit, 1, 2),
            (LogRecord::Begin, 2, u64::MAX),
            (upd(0, 0, b"committed-value", b"dirty-value"), 2, 4),
        ]);
        let pool = fresh_pool(&dir, "data.db", 16);
        redo(&pool, &wal).expect("redo");
        assert_eq!(read_slot(&pool, 0, 0).as_deref(), Some(&b"dirty-value"[..]));

        let analysis = analyze(&wal).expect("analyze");
        assert_eq!(analysis.winners(), vec![1]);
        assert_eq!(analysis.losers(), vec![2]);
        let mut writer = WalWriter::open(&wal).expect("writer");
        undo(&pool, &mut writer, &analysis, &wal).expect("undo");
        // Loser's update reverted; winner's value restored.
        assert_eq!(
            read_slot(&pool, 0, 0).as_deref(),
            Some(&b"committed-value"[..])
        );
    }

    #[test]
    fn full_recovery_is_idempotent_across_repeated_runs() {
        // Simulate a crash during undo by running the whole recovery twice
        // against fresh buffer pools over the SAME wal. The second run sees
        // the CLRs + Abort the first run wrote and must converge to the
        // same state without re-undoing.
        let (dir, wal) = build_wal(&[
            (LogRecord::Begin, 1, u64::MAX),
            (upd(0, 0, b"", b"keep"), 1, 1),
            (LogRecord::Commit, 1, 2),
            (LogRecord::Begin, 2, u64::MAX),
            (upd(0, 1, b"", b"rollme"), 2, 4),
        ]);

        // First recovery.
        {
            let pool = fresh_pool(&dir, "data.db", 16);
            redo(&pool, &wal).expect("redo 1");
            let a = analyze(&wal).expect("analyze 1");
            let mut w = WalWriter::open(&wal).expect("writer 1");
            undo(&pool, &mut w, &a, &wal).expect("undo 1");
            assert_eq!(read_slot(&pool, 0, 0).as_deref(), Some(&b"keep"[..]));
            assert_eq!(read_slot(&pool, 0, 1), None, "loser rolled back");
        }

        // Second recovery against a fresh data file but the grown WAL.
        {
            let pool = fresh_pool(&dir, "data2.db", 16);
            redo(&pool, &wal).expect("redo 2");
            let a = analyze(&wal).expect("analyze 2");
            // Loser 2 is already aborted; undo should write no new CLRs.
            let mut w = WalWriter::open(&wal).expect("writer 2");
            let undone = undo(&pool, &mut w, &a, &wal).expect("undo 2");
            assert_eq!(undone, 0, "already-aborted loser is not undone again");
            assert_eq!(read_slot(&pool, 0, 0).as_deref(), Some(&b"keep"[..]));
            assert_eq!(read_slot(&pool, 0, 1), None, "still rolled back");
        }
    }

    #[test]
    fn redo_replays_clr() {
        // A CLR's undo image is applied by redo just like an Update.
        let (dir, wal) = build_wal(&[
            (LogRecord::Begin, 1, u64::MAX),
            (upd(0, 0, b"", b"original"), 1, 1),
            (
                LogRecord::Clr {
                    page_id: 0,
                    slot_id: 0,
                    undo_image: b"".to_vec(),
                    undo_next: u64::MAX,
                },
                1,
                2,
            ),
        ]);
        let pool = fresh_pool(&dir, "data.db", 16);
        redo(&pool, &wal).expect("redo");
        let guard = pool.fetch_page(PageId::new(0)).expect("fetch");
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        buf.copy_from_slice(guard.page());
        let heap = HeapPage::from_bytes(&mut buf).expect("heap");
        // The CLR (empty undo image) tombstoned slot 0 during redo.
        assert_eq!(heap.get(SlotId::new(0)), None);
    }
}
