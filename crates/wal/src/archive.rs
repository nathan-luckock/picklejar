//! Point-in-time WAL truncation, the mechanism behind restoring a base backup
//! forward to an exact log position.
//!
//! Recovery replays the WAL forward over a base image, so trimming the WAL to the
//! records at or before a target LSN, while leaving those records byte-for-byte
//! intact (and therefore keeping their original LSNs), makes the next recovery
//! stop exactly at that point. This is how a point-in-time restore lands on a
//! chosen moment instead of the end of the log.

use std::fs;
use std::io;
use std::path::Path;

use crate::lsn::Lsn;
use crate::record::{read_length, LogRecord, MIN_RECORD_BYTES};

/// Truncate the WAL at `path` to keep only records at or before `target`.
///
/// The kept records' bytes are preserved (and so their original LSNs), so a later
/// recovery replays forward to that exact point. Returns the number of records
/// kept.
///
/// Records are walked from the start; the first record with an LSN past `target`,
/// or the first torn or unparseable record, ends the kept prefix, and the file is
/// truncated to the byte boundary after the last kept record.
///
/// # Errors
///
/// Returns an I/O error if the WAL cannot be read or truncated.
pub fn truncate_to_lsn(path: &Path, target: Lsn) -> io::Result<usize> {
    let bytes = fs::read(path)?;
    let mut cursor = 0usize;
    let mut keep_bytes = 0usize;
    let mut kept = 0usize;
    while cursor + 4 <= bytes.len() {
        let Some(length) = read_length(&bytes[cursor..]) else {
            break;
        };
        let length = length as usize;
        if length < MIN_RECORD_BYTES || cursor + length > bytes.len() {
            break;
        }
        let Ok((header, _record)) = LogRecord::read(&bytes[cursor..cursor + length]) else {
            break;
        };
        if header.lsn > target {
            break;
        }
        keep_bytes = cursor + length;
        kept += 1;
        cursor += length;
    }
    let file = fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(keep_bytes as u64)?;
    Ok(kept)
}

/// The highest LSN present in the WAL at `path`, or `None` if it has no readable
/// records. Useful for naming the end of the log a restore could target.
///
/// # Errors
///
/// Returns an I/O error if the WAL cannot be read.
pub fn max_lsn(path: &Path) -> io::Result<Option<Lsn>> {
    let bytes = fs::read(path)?;
    let mut cursor = 0usize;
    let mut max = None;
    while cursor + 4 <= bytes.len() {
        let Some(length) = read_length(&bytes[cursor..]) else {
            break;
        };
        let length = length as usize;
        if length < MIN_RECORD_BYTES || cursor + length > bytes.len() {
            break;
        }
        let Ok((header, _)) = LogRecord::read(&bytes[cursor..cursor + length]) else {
            break;
        };
        max = Some(max.map_or(header.lsn, |m: Lsn| m.max(header.lsn)));
        cursor += length;
    }
    Ok(max)
}

#[cfg(test)]
mod tests {
    use super::{max_lsn, truncate_to_lsn};
    use crate::lsn::{Lsn, TxnId};
    use crate::record::LogRecord;
    use crate::writer::WalWriter;

    #[test]
    fn truncates_to_a_target_lsn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("w.wal");
        let lsns: Vec<Lsn> = {
            let mut w = WalWriter::open(&path).expect("open");
            let mut out = Vec::new();
            for _ in 0..10 {
                out.push(
                    w.append(&LogRecord::Begin, TxnId::new(1), Lsn::INVALID)
                        .expect("append"),
                );
            }
            w.fsync_all().expect("fsync");
            out
        };
        assert_eq!(max_lsn(&path).expect("max").expect("some"), lsns[9]);

        // Keep through the 5th record; the 6th and later are dropped.
        let kept = truncate_to_lsn(&path, lsns[4]).expect("truncate");
        assert_eq!(kept, 5);
        assert_eq!(max_lsn(&path).expect("max").expect("some"), lsns[4]);

        // A target before the first record keeps nothing.
        let kept = truncate_to_lsn(&path, Lsn(0)).expect("truncate");
        assert_eq!(kept, 0);
        assert_eq!(max_lsn(&path).expect("max"), None);
    }
}
