//! Append-only WAL writer.
//!
//! Owns the on-disk WAL file and allocates monotonically increasing LSNs.
//! Records are buffered in memory until [`WalWriter::fsync_through`] hits
//! or exceeds the LSN of interest; then the buffered bytes are flushed and
//! `File::sync_all` is called.
//!
//! # LSN allocation
//!
//! The writer keeps a `next_lsn` counter, initialized to 1. Every
//! [`append`](WalWriter::append) returns the assigned LSN and increments
//! the counter. The counter only ever goes up. On reopen the writer reads
//! the highest LSN already present on disk and resumes from `last + 1`.
//!
//! # `fsync_through` semantics
//!
//! `fsync_through(L)`:
//! 1. If `L` is already durable (i.e., already flushed and fsynced),
//!    returns `Ok(())` immediately.
//! 2. Otherwise flushes the internal buffer to the file, calls
//!    `sync_all`, and advances `durable_through` to the new high water
//!    mark.
//!
//! Group commit, batched fsync, and concurrent writers all come in later
//! sprints. Sprint 3 is single-threaded and serialized.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use tracing::debug;

use crate::error::Result;
use crate::lsn::{Lsn, TxnId};
use crate::record::{read_length, LogRecord, MIN_RECORD_BYTES};

/// In-memory append + on-disk persistence for a WAL file.
#[derive(Debug)]
pub struct WalWriter {
    file: File,
    /// Next LSN to assign. Starts at 1 on a fresh file.
    next_lsn: u64,
    /// Highest LSN that has been flushed AND fsync'd. Records with LSN
    /// strictly greater than this may still be in the buffer.
    durable_through: u64,
    /// Bytes queued for append but not yet flushed.
    buffer: Vec<u8>,
    /// Highest LSN currently in the buffer (or already on disk). Used to
    /// short-circuit `fsync_through` for LSNs we never assigned.
    pending_high_lsn: u64,
    /// Reusable scratch buffer for serializing a record before copying
    /// into `buffer`.
    scratch: Vec<u8>,
}

impl WalWriter {
    /// Open the WAL at `path`. If the file exists, scans it to recover the
    /// highest assigned LSN. If it does not exist, creates an empty file
    /// and starts LSN allocation at 1.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        file.seek(SeekFrom::Start(0))?;
        let mut full = Vec::new();
        file.read_to_end(&mut full)?;
        let last_lsn = scan_for_last_lsn(&full);
        let next_lsn = last_lsn.saturating_add(1).max(1);
        // Seek to end for append (the OS handles this with O_APPEND but
        // we also want our internal cursor accurate for diagnostics).
        file.seek(SeekFrom::End(0))?;
        debug!(
            path = %path.display(),
            next_lsn,
            durable_through = last_lsn,
            "opened WAL writer"
        );
        Ok(Self {
            file,
            next_lsn,
            durable_through: last_lsn,
            buffer: Vec::with_capacity(64 * 1024),
            pending_high_lsn: last_lsn,
            scratch: Vec::with_capacity(256),
        })
    }

    /// LSN that will be assigned to the next [`append`](Self::append).
    #[must_use]
    pub const fn current_lsn(&self) -> Lsn {
        Lsn::new(self.next_lsn)
    }

    /// Highest LSN that is durable on disk. Useful for tests and for the
    /// buffer pool's WAL-ordering check.
    #[must_use]
    pub const fn durable_through(&self) -> Lsn {
        Lsn::new(self.durable_through)
    }

    /// Append `record` to the WAL. Returns the LSN assigned to it.
    /// The bytes are buffered in memory; call [`fsync_through`] to make
    /// them durable.
    ///
    /// [`fsync_through`]: Self::fsync_through
    pub fn append(&mut self, record: &LogRecord, txn: TxnId, prev_lsn: Lsn) -> Result<Lsn> {
        let lsn = Lsn::new(self.next_lsn);
        self.next_lsn = self
            .next_lsn
            .checked_add(1)
            .expect("LSN counter overflow (would need 2^64 records)");
        record.write(lsn, prev_lsn, txn, &mut self.scratch);
        self.buffer.extend_from_slice(&self.scratch);
        self.pending_high_lsn = self.pending_high_lsn.max(lsn.get());
        Ok(lsn)
    }

    /// Make every record with LSN `<= lsn` durable on disk.
    ///
    /// Cheap no-op when `lsn <= durable_through()`.
    pub fn fsync_through(&mut self, lsn: Lsn) -> Result<()> {
        if lsn.is_invalid() || lsn.get() <= self.durable_through {
            return Ok(());
        }
        if !self.buffer.is_empty() {
            self.file.write_all(&self.buffer)?;
            self.buffer.clear();
        }
        self.file.sync_all()?;
        self.durable_through = self.durable_through.max(self.pending_high_lsn);
        Ok(())
    }

    /// Make every record currently appended durable on disk. Equivalent to
    /// `fsync_through(self.current_lsn() - 1)` but does not require the
    /// caller to track that value.
    pub fn fsync_all(&mut self) -> Result<()> {
        if self.pending_high_lsn <= self.durable_through {
            return Ok(());
        }
        if !self.buffer.is_empty() {
            self.file.write_all(&self.buffer)?;
            self.buffer.clear();
        }
        self.file.sync_all()?;
        self.durable_through = self.pending_high_lsn;
        Ok(())
    }
}

/// Scan an entire WAL file buffer and return the highest LSN of a complete
/// record. Returns 0 if the file is empty or every record is corrupt.
/// Stops cleanly at the first short read at EOF (torn tail).
fn scan_for_last_lsn(buf: &[u8]) -> u64 {
    let mut cursor = 0usize;
    let mut last = 0u64;
    while cursor < buf.len() {
        let remaining = &buf[cursor..];
        let length = match read_length(remaining) {
            Some(l) => l as usize,
            None => break, // torn tail before length prefix
        };
        if length < MIN_RECORD_BYTES || remaining.len() < length {
            break; // torn tail mid-record
        }
        // Try to read the record. If checksum fails treat it as torn tail.
        match LogRecord::read(&remaining[..length]) {
            Ok((hdr, _)) => {
                last = last.max(hdr.lsn.get());
                cursor += length;
            }
            Err(_) => break,
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::LogRecord;
    use tempfile::TempDir;

    fn fresh_writer() -> (TempDir, WalWriter) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wal.log");
        let w = WalWriter::open(&path).expect("open");
        (dir, w)
    }

    fn make_update(slot: u16) -> LogRecord {
        LogRecord::Update {
            page_id: 1,
            slot_id: slot,
            before: vec![],
            after: vec![0xAA; 32],
        }
    }

    #[test]
    fn open_fresh_starts_at_lsn_1() {
        let (_dir, w) = fresh_writer();
        assert_eq!(w.current_lsn(), Lsn::new(1));
        assert_eq!(w.durable_through(), Lsn::new(0));
    }

    #[test]
    fn append_assigns_monotonic_lsns() {
        let (_dir, mut w) = fresh_writer();
        let l1 = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .expect("a");
        let l2 = w.append(&make_update(0), TxnId::new(1), l1).expect("b");
        let l3 = w.append(&LogRecord::Commit, TxnId::new(1), l2).expect("c");
        assert_eq!(l1, Lsn::new(1));
        assert_eq!(l2, Lsn::new(2));
        assert_eq!(l3, Lsn::new(3));
        assert_eq!(w.current_lsn(), Lsn::new(4));
    }

    #[test]
    fn append_does_not_durably_persist_until_fsync() {
        let (dir, mut w) = fresh_writer();
        let path = dir.path().join("wal.log");
        let _l1 = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .expect("a");
        // File should be empty (or at least missing our record) before fsync.
        let on_disk = std::fs::read(&path).expect("read");
        assert!(
            on_disk.is_empty(),
            "WAL file has bytes before fsync: {on_disk:?}"
        );
        assert_eq!(w.durable_through(), Lsn::new(0));
    }

    #[test]
    fn fsync_through_makes_records_durable() {
        let (dir, mut w) = fresh_writer();
        let path = dir.path().join("wal.log");
        let l1 = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .expect("a");
        w.fsync_through(l1).expect("fsync");
        let on_disk = std::fs::read(&path).expect("read");
        assert!(!on_disk.is_empty(), "WAL should have bytes after fsync");
        assert_eq!(w.durable_through(), l1);
    }

    #[test]
    fn fsync_through_below_durable_is_noop() {
        let (_dir, mut w) = fresh_writer();
        let l1 = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .expect("a");
        w.fsync_through(l1).expect("fsync");
        // Calling again with the same or lower LSN must be a no-op.
        w.fsync_through(l1).expect("noop");
        w.fsync_through(Lsn::new(0)).expect("noop zero");
        w.fsync_through(Lsn::INVALID).expect("noop invalid");
    }

    #[test]
    fn fsync_all_flushes_everything_buffered() {
        let (dir, mut w) = fresh_writer();
        let path = dir.path().join("wal.log");
        let _l1 = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .expect("a");
        let l2 = w
            .append(&LogRecord::Commit, TxnId::new(1), Lsn::INVALID)
            .expect("b");
        w.fsync_all().expect("fsync_all");
        assert_eq!(w.durable_through(), l2);
        let on_disk = std::fs::read(&path).expect("read");
        assert!(on_disk.len() >= 2 * MIN_RECORD_BYTES);
    }

    #[test]
    fn reopen_resumes_from_last_lsn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wal.log");
        {
            let mut w = WalWriter::open(&path).expect("open");
            let _ = w
                .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
                .unwrap();
            let _ = w
                .append(&LogRecord::Commit, TxnId::new(1), Lsn::INVALID)
                .unwrap();
            let _ = w
                .append(&LogRecord::Begin, TxnId::new(2), Lsn::INVALID)
                .unwrap();
            w.fsync_all().expect("fsync");
        }
        let w = WalWriter::open(&path).expect("reopen");
        assert_eq!(w.current_lsn(), Lsn::new(4));
        assert_eq!(w.durable_through(), Lsn::new(3));
    }

    #[test]
    fn torn_tail_is_skipped_on_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wal.log");
        {
            let mut w = WalWriter::open(&path).expect("open");
            for txn in 1u64..=3 {
                let _ = w
                    .append(&LogRecord::Begin, TxnId::new(txn), Lsn::INVALID)
                    .unwrap();
                let _ = w
                    .append(&LogRecord::Commit, TxnId::new(txn), Lsn::INVALID)
                    .unwrap();
            }
            w.fsync_all().expect("fsync");
        }
        // Truncate the last 5 bytes to simulate a torn write at the tail.
        let mut bytes = std::fs::read(&path).expect("read");
        let original_len = bytes.len();
        bytes.truncate(original_len - 5);
        std::fs::write(&path, &bytes).expect("write");
        // Reopen: scan should stop cleanly at the last complete record.
        let w = WalWriter::open(&path).expect("reopen");
        // We wrote 6 records (LSNs 1..=6). Truncation drops the 6th record;
        // last good LSN is 5, next is 6.
        assert_eq!(w.current_lsn(), Lsn::new(6));
        assert_eq!(w.durable_through(), Lsn::new(5));
    }

    #[test]
    fn many_appends_then_single_fsync() {
        let (_dir, mut w) = fresh_writer();
        let n = 1000;
        for i in 0..n {
            let _ = w
                .append(
                    &make_update(u16::try_from(i).expect("fits in u16")),
                    TxnId::new(1),
                    Lsn::INVALID,
                )
                .expect("a");
        }
        assert_eq!(w.current_lsn(), Lsn::new(1 + n));
        // Nothing durable yet.
        assert_eq!(w.durable_through(), Lsn::new(0));
        w.fsync_all().expect("fsync");
        assert_eq!(w.durable_through(), Lsn::new(n));
    }

    #[test]
    fn scan_for_last_lsn_handles_empty_file() {
        assert_eq!(scan_for_last_lsn(&[]), 0);
    }

    #[test]
    fn current_lsn_advances_even_without_fsync() {
        let (_dir, mut w) = fresh_writer();
        let _ = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .unwrap();
        let _ = w
            .append(&LogRecord::Begin, TxnId::new(2), Lsn::INVALID)
            .unwrap();
        // No fsync called; current_lsn should still be 3.
        assert_eq!(w.current_lsn(), Lsn::new(3));
    }
}
