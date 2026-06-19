//! Forward iterator over a WAL file.
//!
//! Used by Sprint 4 recovery (the analysis and redo phases). Reads records
//! sequentially from byte 0, verifies the trailing CRC32, and yields
//! `Result<(RecordHeader, LogRecord)>` items.
//!
//! # End-of-stream handling
//!
//! Three distinct ways a WAL read can end:
//!
//! 1. **Clean EOF.** Bytes ran out exactly between records. The reader
//!    yields `None` forever. No error.
//! 2. **Torn tail.** The file ended mid-record: either before a complete
//!    length prefix or after the length prefix but before the full
//!    record. Treated the same as clean EOF: yields `None` forever, no
//!    error. A crash during fsync can leave a partial record at the
//!    tail; recovery is supposed to treat everything after the last
//!    complete record as if it was never appended.
//! 3. **Record decode error.** Length prefix decoded, full record bytes
//!    read, but parsing failed (checksum mismatch, unknown type byte,
//!    truncated payload). Yields `Some(Err(...))` once, then `None`
//!    forever. Recovery should log the error and either truncate the
//!    WAL at the last good record or abort startup, depending on the
//!    operator policy.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::error::{Result, WalError};
use crate::lsn::Lsn;
use crate::record::{LogRecord, RecordHeader, MIN_RECORD_BYTES};

/// Forward iterator over a WAL file.
#[derive(Debug)]
pub struct WalReader {
    inner: BufReader<File>,
    /// Set after the first error so we yield `None` forever after.
    poisoned: bool,
    /// Reusable per-record buffer; capacity grows to the largest record
    /// we've seen.
    buf: Vec<u8>,
}

impl WalReader {
    /// Open the WAL at `path` for reading from byte 0.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path.as_ref())?;
        Ok(Self {
            inner: BufReader::with_capacity(64 * 1024, file),
            poisoned: false,
            buf: Vec::with_capacity(256),
        })
    }
}

impl Iterator for WalReader {
    type Item = Result<(RecordHeader, LogRecord)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.poisoned {
            return None;
        }
        // Read the 4-byte length prefix.
        let mut len_buf = [0u8; 4];
        match read_exact_or_eof(&mut self.inner, &mut len_buf) {
            Ok(Some(())) => {}
            Ok(None) => return None, // clean EOF
            Err(e) => {
                self.poisoned = true;
                return Some(Err(WalError::Io(e)));
            }
        }
        let length = u32::from_le_bytes(len_buf) as usize;
        if length < MIN_RECORD_BYTES {
            // Bogus length. Treat as torn tail and stop cleanly. We avoid
            // returning an error here because a torn write at the tail can
            // produce a partial length prefix that looks like a tiny
            // record; reporting that as an error would cause spurious
            // recovery failures.
            self.poisoned = true;
            return None;
        }
        // Read the rest of the record.
        self.buf.clear();
        self.buf.extend_from_slice(&len_buf);
        self.buf.resize(length, 0);
        match read_exact_or_eof(&mut self.inner, &mut self.buf[4..]) {
            Ok(Some(())) => {}
            Ok(None) => {
                // Torn tail mid-record. Treat as clean stop.
                self.poisoned = true;
                return None;
            }
            Err(e) => {
                self.poisoned = true;
                return Some(Err(WalError::Io(e)));
            }
        }
        // Parse.
        match LogRecord::read(&self.buf) {
            Ok((hdr, rec)) => Some(Ok((hdr, rec))),
            Err(e) => {
                self.poisoned = true;
                Some(Err(e))
            }
        }
    }
}

/// Like `Read::read_exact` but distinguishes clean EOF from partial reads.
/// Returns `Ok(Some(()))` if `buf` was filled, `Ok(None)` if the reader
/// returned 0 bytes (clean EOF), and `Err` if a real I/O error occurred or
/// only part of `buf` was filled (torn tail mid-buffer).
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<Option<()>> {
    let mut total = 0;
    while total < buf.len() {
        match r.read(&mut buf[total..]) {
            // Both clean EOF (total == 0) and partial-fill (total > 0) map to
            // Ok(None); the caller (WalReader) treats partial fills as torn
            // tails by yielding None on the next iteration.
            Ok(0) => return Ok(None),
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(()))
}

/// Scan the WAL at `path` for the most recent [`LogRecord::Catalog`] snapshot
/// at or before `up_to` (or the most recent overall when `up_to` is `None`).
///
/// This makes the WAL authoritative for the catalog. On open, the engine
/// applies the latest snapshot, so a schema change that reached the log is
/// recovered even if its sidecar write was lost; bounding `up_to` lets a
/// point-in-time restore reconstruct the schema as of a chosen LSN rather than
/// only the base state.
///
/// A torn or corrupt tail record stops the scan cleanly (the reader treats it
/// as end-of-log), so a snapshot is returned only when a complete,
/// checksum-valid catalog record was read. An absent WAL yields `Ok(None)`.
///
/// # Errors
///
/// Returns an error if the file cannot be read, or if a record before the
/// stopping point fails to decode (a checksum mismatch mid-log).
pub fn latest_catalog_snapshot<P: AsRef<Path>>(
    path: P,
    up_to: Option<Lsn>,
) -> Result<Option<Vec<u8>>> {
    latest_snapshot(path, up_to, |rec| match rec {
        LogRecord::Catalog { snapshot } => Some(snapshot),
        _ => None,
    })
}

/// Most recent [`LogRecord::RlsPolicies`] snapshot at or before `up_to`.
///
/// The row-level-security analogue of [`latest_catalog_snapshot`] (or the most
/// recent overall when `up_to` is `None`).
/// Makes the WAL authoritative for tenant isolation: a policy change that
/// reached the log is recovered on open even if its `.pol` sidecar write was
/// lost in a crash, which closes a security-relevant durability gap (a missing
/// isolation policy after a crash).
///
/// # Errors
///
/// Returns an error if the file cannot be read, or if a record before the
/// stopping point fails to decode.
pub fn latest_rls_snapshot<P: AsRef<Path>>(path: P, up_to: Option<Lsn>) -> Result<Option<Vec<u8>>> {
    latest_snapshot(path, up_to, |rec| match rec {
        LogRecord::RlsPolicies { snapshot } => Some(snapshot),
        _ => None,
    })
}

/// Shared scan behind [`latest_catalog_snapshot`] and [`latest_rls_snapshot`]:
/// return the payload of the last record at or before `up_to` for which
/// `extract` yields `Some`. An absent WAL yields `Ok(None)`; a torn tail stops
/// the scan cleanly.
fn latest_snapshot<P: AsRef<Path>>(
    path: P,
    up_to: Option<Lsn>,
    extract: impl Fn(LogRecord) -> Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    let reader = match WalReader::open(path) {
        Ok(r) => r,
        Err(WalError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut latest: Option<Vec<u8>> = None;
    for item in reader {
        let (hdr, rec) = item?;
        if up_to.is_some_and(|limit| hdr.lsn.get() > limit.get()) {
            break;
        }
        if let Some(snapshot) = extract(rec) {
            latest = Some(snapshot);
        }
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsn::{Lsn, TxnId};
    use crate::record::TRAILER_BYTES;
    use crate::writer::WalWriter;
    use picklejar_storage::crc32::crc32;
    use std::io::Write;
    use tempfile::TempDir;

    fn fresh_writer() -> (TempDir, std::path::PathBuf, WalWriter) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wal.log");
        let w = WalWriter::open(&path).expect("open");
        (dir, path, w)
    }

    #[test]
    fn read_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wal.log");
        std::fs::write(&path, []).expect("touch");
        let mut r = WalReader::open(&path).expect("open");
        assert!(r.next().is_none());
        // And forever after.
        assert!(r.next().is_none());
    }

    #[test]
    fn read_single_record() {
        let (_dir, path, mut w) = fresh_writer();
        let l1 = w
            .append(&LogRecord::Begin, TxnId::new(7), Lsn::INVALID)
            .expect("append");
        w.fsync_all().expect("fsync");
        drop(w);
        let mut r = WalReader::open(&path).expect("open");
        let (hdr, rec) = r.next().expect("some").expect("ok");
        assert_eq!(hdr.lsn, l1);
        assert_eq!(rec, LogRecord::Begin);
        assert!(r.next().is_none());
    }

    #[test]
    fn read_many_records_in_order() {
        let (_dir, path, mut w) = fresh_writer();
        let mut expected = Vec::new();
        for i in 1u64..=20 {
            let txn = TxnId::new(i);
            let l1 = w
                .append(&LogRecord::Begin, txn, Lsn::INVALID)
                .expect("begin");
            let upd = LogRecord::Update {
                page_id: i,
                slot_id: 0,
                before: vec![],
                after: vec![0xAB; 16],
            };
            let l2 = w.append(&upd, txn, l1).expect("update");
            let l3 = w.append(&LogRecord::Commit, txn, l2).expect("commit");
            expected.push((l1, LogRecord::Begin));
            expected.push((l2, upd));
            expected.push((l3, LogRecord::Commit));
        }
        w.fsync_all().expect("fsync");
        drop(w);

        let r = WalReader::open(&path).expect("open");
        let got: Vec<(Lsn, LogRecord)> = r
            .map(|item| {
                let (hdr, rec) = item.expect("ok");
                (hdr.lsn, rec)
            })
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn corrupt_record_yields_error_then_none() {
        let (_dir, path, mut w) = fresh_writer();
        let _ = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .expect("a");
        let _ = w
            .append(&LogRecord::Commit, TxnId::new(1), Lsn::INVALID)
            .expect("b");
        w.fsync_all().expect("fsync");
        drop(w);

        // Flip a bit in the middle of the file (inside the first record).
        let mut bytes = std::fs::read(&path).expect("read");
        bytes[20] ^= 0x01;
        std::fs::write(&path, &bytes).expect("write");

        let mut r = WalReader::open(&path).expect("open");
        let first = r.next().expect("some");
        assert!(matches!(first, Err(WalError::ChecksumMismatch)));
        // And `None` forever after.
        assert!(r.next().is_none());
        assert!(r.next().is_none());
    }

    #[test]
    fn torn_tail_before_length_prefix_returns_none_cleanly() {
        let (_dir, path, mut w) = fresh_writer();
        let _ = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .expect("a");
        w.fsync_all().expect("fsync");
        drop(w);
        // Append 2 garbage bytes (less than a length prefix).
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open append");
        f.write_all(&[0xFF, 0xFF]).expect("garbage");
        drop(f);
        let mut r = WalReader::open(&path).expect("open");
        let _first = r.next().expect("some").expect("ok"); // the original record
        assert!(
            r.next().is_none(),
            "torn tail before length must stop cleanly"
        );
    }

    #[test]
    fn torn_tail_mid_record_returns_none_cleanly() {
        let (_dir, path, mut w) = fresh_writer();
        let _ = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .expect("a");
        let _ = w
            .append(&LogRecord::Commit, TxnId::new(1), Lsn::INVALID)
            .expect("b");
        w.fsync_all().expect("fsync");
        drop(w);
        // Drop the last 5 bytes of the file.
        let mut bytes = std::fs::read(&path).expect("read");
        let len = bytes.len();
        bytes.truncate(len - 5);
        std::fs::write(&path, &bytes).expect("write");
        let mut r = WalReader::open(&path).expect("open");
        let _first = r.next().expect("some").expect("ok"); // first record
        assert!(r.next().is_none(), "torn tail mid-record must stop cleanly");
    }

    #[test]
    fn bogus_length_prefix_is_treated_as_torn_tail() {
        let (_dir, path, mut w) = fresh_writer();
        let _ = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .expect("a");
        w.fsync_all().expect("fsync");
        drop(w);
        // Append a length prefix below MIN_RECORD_BYTES.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append");
        f.write_all(&3u32.to_le_bytes()).expect("len");
        drop(f);
        let mut r = WalReader::open(&path).expect("open");
        let _first = r.next().expect("some").expect("ok");
        assert!(r.next().is_none(), "bogus length must stop cleanly");
    }

    #[test]
    fn unknown_type_byte_yields_error() {
        let (_dir, path, mut w) = fresh_writer();
        let _ = w
            .append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
            .expect("a");
        w.fsync_all().expect("fsync");
        drop(w);
        // Stomp the type byte (offset 4) of the first record and patch
        // the checksum so the type-byte error fires.
        let mut bytes = std::fs::read(&path).expect("read");
        bytes[4] = 99;
        let trailer_offset = bytes.len() - TRAILER_BYTES;
        let new_crc = crc32(&bytes[..trailer_offset]);
        bytes[trailer_offset..].copy_from_slice(&new_crc.to_le_bytes());
        std::fs::write(&path, &bytes).expect("write");

        let mut r = WalReader::open(&path).expect("open");
        let first = r.next().expect("some");
        assert!(matches!(first, Err(WalError::UnknownRecordType(99))));
        assert!(r.next().is_none());
    }

    #[test]
    fn reader_after_pure_writer_round_trip() {
        // Sanity: 100 mixed records written, all read back in order.
        let (_dir, path, mut w) = fresh_writer();
        let mut expected = Vec::new();
        for i in 1u64..=100 {
            let txn = TxnId::new(i);
            let l1 = w.append(&LogRecord::Begin, txn, Lsn::INVALID).expect("a");
            expected.push((l1, LogRecord::Begin));
            for _ in 0..3 {
                let upd = LogRecord::Update {
                    page_id: i,
                    slot_id: 0,
                    before: vec![1, 2, 3],
                    after: vec![4, 5, 6, 7],
                };
                let lu = w.append(&upd, txn, Lsn::INVALID).expect("u");
                expected.push((lu, upd));
            }
            let lc = w.append(&LogRecord::Commit, txn, Lsn::INVALID).expect("c");
            expected.push((lc, LogRecord::Commit));
        }
        w.fsync_all().expect("fsync");
        drop(w);

        let r = WalReader::open(&path).expect("open");
        let got: Vec<(Lsn, LogRecord)> = r
            .map(|item| {
                let (hdr, rec) = item.expect("ok");
                (hdr.lsn, rec)
            })
            .collect();
        assert_eq!(got.len(), expected.len());
        assert_eq!(got, expected);
    }
}
