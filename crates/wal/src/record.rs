//! WAL record format and (de)serialization.
//!
//! Records have a fixed 29-byte header followed by a variable-length
//! per-type payload and a 4-byte CRC32 trailer.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │ length: u32 (total record size, including this length field) │
//! │ type:   u8  (Begin / Update / Commit / Abort / ...)          │
//! │ lsn:    u64                                                  │
//! │ txn_id: u64                                                  │
//! │ prev_lsn: u64 (Lsn::INVALID for first record in a txn)       │
//! │ payload: [u8] (per-type, see LogRecord)                      │
//! │ checksum: u32 (CRC32 of [0 .. length-4])                     │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! Length-prefixed framing lets the reader skip a record without
//! understanding its payload, and lets the recovery torture test handle
//! tail truncation cleanly.

use picklejar_storage::crc32::crc32;

use crate::error::{Result, WalError};
use crate::lsn::{Lsn, TxnId};

/// Bytes occupied by the fixed header (`length` + `type` + `lsn` + `txn_id` + `prev_lsn`).
pub const HEADER_BYTES: usize = 4 + 1 + 8 + 8 + 8;

/// Bytes occupied by the trailing checksum.
pub const TRAILER_BYTES: usize = 4;

/// Minimum complete record size (header + empty payload + trailer).
pub const MIN_RECORD_BYTES: usize = HEADER_BYTES + TRAILER_BYTES;

// Field offsets within the record buffer.
const LENGTH_OFFSET: usize = 0;
const TYPE_OFFSET: usize = 4;
const LSN_OFFSET: usize = 5;
const TXN_ID_OFFSET: usize = 13;
const PREV_LSN_OFFSET: usize = 21;
pub(crate) const PAYLOAD_OFFSET: usize = 29;

/// On-disk discriminant for [`LogRecord`].
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RecordKind {
    /// Marks the start of a transaction.
    Begin = 1,
    /// Records a page mutation. Carries before- and after-images.
    Update = 2,
    /// Marks a successful transaction commit.
    Commit = 3,
    /// Marks a transaction abort.
    Abort = 4,
    /// Checkpoint marker. Sprint 4.
    Checkpoint = 5,
    /// Compensation log record (written during undo). Sprint 4.
    Clr = 6,
    /// Full catalog snapshot, written after a schema change so forward
    /// replay can reconstruct the catalog as of any LSN. Carries the same
    /// serialized body the `.meta` sidecar holds, as an opaque payload.
    Catalog = 7,
    /// Full row-level-security snapshot, written after a policy change so the
    /// log is authoritative for tenant isolation the same way `Catalog` is for
    /// schema. Carries the serialized `.pol` body as an opaque payload.
    RlsPolicies = 8,
}

impl RecordKind {
    /// Decode the on-disk byte. Returns an error for unknown values.
    pub const fn from_u8(v: u8) -> Result<Self> {
        match v {
            1 => Ok(Self::Begin),
            2 => Ok(Self::Update),
            3 => Ok(Self::Commit),
            4 => Ok(Self::Abort),
            5 => Ok(Self::Checkpoint),
            6 => Ok(Self::Clr),
            7 => Ok(Self::Catalog),
            8 => Ok(Self::RlsPolicies),
            other => Err(WalError::UnknownRecordType(other)),
        }
    }
}

/// Per-type payload of a WAL record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogRecord {
    /// `BEGIN T` marker.
    Begin,
    /// `UPDATE` capturing both before- and after-images.
    Update {
        /// Page id mutated.
        page_id: u64,
        /// Slot within that page.
        slot_id: u16,
        /// Tuple bytes before the change. Empty for inserts.
        before: Vec<u8>,
        /// Tuple bytes after the change. Empty for deletes.
        after: Vec<u8>,
    },
    /// `COMMIT T` marker.
    Commit,
    /// `ABORT T` marker.
    Abort,
    /// Fuzzy checkpoint. Records the set of transactions active at
    /// checkpoint time so analysis can start mid-log instead of from byte
    /// zero. Sprint 4 stores only the active txn table (no dirty page
    /// table yet); recovery falls back to scanning from the start when no
    /// checkpoint is present.
    Checkpoint {
        /// `(txn_id, last_lsn)` for each transaction active at checkpoint.
        active_txns: Vec<(u64, u64)>,
    },
    /// Compensation log record, written during undo. Redo-only: a CLR is
    /// replayed by redo but is never itself undone. `undo_next` chains to
    /// the next record to undo for this transaction, which makes undo
    /// idempotent across repeated crashes.
    Clr {
        /// Page the undo touched.
        page_id: u64,
        /// Slot within that page.
        slot_id: u16,
        /// Bytes to restore (the before-image of the undone update). Empty
        /// means tombstone the slot (undo of an insert).
        undo_image: Vec<u8>,
        /// LSN to undo next for this txn (the undone record's `prev_lsn`),
        /// or [`Lsn::INVALID`](crate::Lsn::INVALID) when nothing remains.
        undo_next: u64,
    },
    /// Full catalog snapshot written after a schema change. The payload is
    /// the serialized catalog body (the same bytes the `.meta` sidecar
    /// holds), stored opaquely. Replay applies the latest snapshot at or
    /// before the recovery point, which makes the WAL authoritative for the
    /// schema and lets point-in-time recovery reconstruct schema changes
    /// rather than only the base state.
    Catalog {
        /// Serialized catalog body.
        snapshot: Vec<u8>,
    },
    /// Full row-level-security snapshot written after a policy change. The
    /// payload is the serialized `.pol` body, stored opaquely, so forward
    /// replay can reconstruct tenant isolation as of any LSN.
    RlsPolicies {
        /// Serialized row-level-security body.
        snapshot: Vec<u8>,
    },
}

impl LogRecord {
    /// The on-disk kind discriminant for this record.
    #[must_use]
    pub const fn kind(&self) -> RecordKind {
        match self {
            Self::Begin => RecordKind::Begin,
            Self::Update { .. } => RecordKind::Update,
            Self::Commit => RecordKind::Commit,
            Self::Abort => RecordKind::Abort,
            Self::Checkpoint { .. } => RecordKind::Checkpoint,
            Self::Clr { .. } => RecordKind::Clr,
            Self::Catalog { .. } => RecordKind::Catalog,
            Self::RlsPolicies { .. } => RecordKind::RlsPolicies,
        }
    }

    /// Serialize this record into `out` with the given header fields.
    /// Truncates `out` first. After this returns, `out.len()` matches the
    /// header's `length` field.
    pub fn write(&self, lsn: Lsn, prev_lsn: Lsn, txn: TxnId, out: &mut Vec<u8>) {
        out.clear();
        // Reserve the 4-byte length, fill at the end.
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(self.kind() as u8);
        out.extend_from_slice(&lsn.get().to_le_bytes());
        out.extend_from_slice(&txn.get().to_le_bytes());
        out.extend_from_slice(&prev_lsn.get().to_le_bytes());

        match self {
            Self::Begin | Self::Commit | Self::Abort => {
                // No payload.
            }
            Self::Update {
                page_id,
                slot_id,
                before,
                after,
            } => {
                out.extend_from_slice(&page_id.to_le_bytes());
                out.extend_from_slice(&slot_id.to_le_bytes());
                let before_len = u16::try_from(before.len())
                    .expect("Update before-image fits in u16 (max page = 8 KiB)");
                out.extend_from_slice(&before_len.to_le_bytes());
                out.extend_from_slice(before);
                let after_len = u16::try_from(after.len()).expect("Update after-image fits in u16");
                out.extend_from_slice(&after_len.to_le_bytes());
                out.extend_from_slice(after);
            }
            Self::Checkpoint { active_txns } => {
                let count = u32::try_from(active_txns.len()).expect("active txn count fits in u32");
                out.extend_from_slice(&count.to_le_bytes());
                for (txn_id, last_lsn) in active_txns {
                    out.extend_from_slice(&txn_id.to_le_bytes());
                    out.extend_from_slice(&last_lsn.to_le_bytes());
                }
            }
            Self::Clr {
                page_id,
                slot_id,
                undo_image,
                undo_next,
            } => {
                out.extend_from_slice(&page_id.to_le_bytes());
                out.extend_from_slice(&slot_id.to_le_bytes());
                out.extend_from_slice(&undo_next.to_le_bytes());
                let img_len = u16::try_from(undo_image.len()).expect("CLR undo image fits in u16");
                out.extend_from_slice(&img_len.to_le_bytes());
                out.extend_from_slice(undo_image);
            }
            Self::Catalog { snapshot } | Self::RlsPolicies { snapshot } => {
                // The whole payload is the snapshot; its length is implied by
                // the record's length field, so no inner length prefix is
                // needed (and the snapshot can exceed the 64 KiB a u16 prefix
                // would allow).
                out.extend_from_slice(snapshot);
            }
        }

        // Reserve trailer space, compute checksum over bytes [0..len-4],
        // write the checksum.
        out.extend_from_slice(&0u32.to_le_bytes()); // placeholder for checksum
        let total_len = u32::try_from(out.len()).expect("WAL record fits in u32");
        out[LENGTH_OFFSET..LENGTH_OFFSET + 4].copy_from_slice(&total_len.to_le_bytes());

        let trailer_offset = out.len() - TRAILER_BYTES;
        let checksum = crc32(&out[..trailer_offset]);
        out[trailer_offset..].copy_from_slice(&checksum.to_le_bytes());
    }

    /// Parse one complete record from `buf`. `buf.len()` must equal the
    /// record's `length` field (use [`read_length`] to peek).
    pub fn read(buf: &[u8]) -> Result<(RecordHeader, Self)> {
        if buf.len() < MIN_RECORD_BYTES {
            return Err(WalError::RecordTooShort {
                length: u32::try_from(buf.len()).unwrap_or(u32::MAX),
                minimum: u32::try_from(MIN_RECORD_BYTES).expect("MIN_RECORD_BYTES fits in u32"),
            });
        }

        let stored_len = u32::from_le_bytes(
            buf[LENGTH_OFFSET..LENGTH_OFFSET + 4]
                .try_into()
                .expect("4 bytes"),
        );
        if stored_len as usize != buf.len() {
            return Err(WalError::RecordTooShort {
                length: stored_len,
                minimum: u32::try_from(buf.len()).unwrap_or(u32::MAX),
            });
        }

        // Verify checksum first; if it fails everything below is suspect.
        let trailer_offset = buf.len() - TRAILER_BYTES;
        let stored_crc = u32::from_le_bytes(buf[trailer_offset..].try_into().expect("4 bytes"));
        if stored_crc != crc32(&buf[..trailer_offset]) {
            return Err(WalError::ChecksumMismatch);
        }

        let kind = RecordKind::from_u8(buf[TYPE_OFFSET])?;
        let lsn = Lsn::new(u64::from_le_bytes(
            buf[LSN_OFFSET..LSN_OFFSET + 8].try_into().expect("8 bytes"),
        ));
        let txn = TxnId::new(u64::from_le_bytes(
            buf[TXN_ID_OFFSET..TXN_ID_OFFSET + 8]
                .try_into()
                .expect("8 bytes"),
        ));
        let prev_lsn = Lsn::new(u64::from_le_bytes(
            buf[PREV_LSN_OFFSET..PREV_LSN_OFFSET + 8]
                .try_into()
                .expect("8 bytes"),
        ));

        let header = RecordHeader {
            length: stored_len,
            kind,
            lsn,
            txn,
            prev_lsn,
        };

        let payload = &buf[PAYLOAD_OFFSET..trailer_offset];
        let record = Self::decode_payload(kind, payload)?;
        Ok((header, record))
    }

    /// Decode the per-kind payload bytes (everything between the fixed
    /// header and the checksum trailer) into a `LogRecord`. The header
    /// fields and checksum are validated by [`read`](Self::read) before
    /// this is called.
    fn decode_payload(kind: RecordKind, payload: &[u8]) -> Result<Self> {
        let record = match kind {
            RecordKind::Begin => Self::Begin,
            RecordKind::Commit => Self::Commit,
            RecordKind::Abort => Self::Abort,
            RecordKind::Update => {
                if payload.len() < 10 {
                    // page_id (8) + slot_id (2)
                    return Err(WalError::PayloadTruncated {
                        expected: 10,
                        available: payload.len(),
                    });
                }
                let page_id = u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
                let slot_id = u16::from_le_bytes(payload[8..10].try_into().expect("2 bytes"));
                let mut cursor = 10usize;
                let (before, advanced) = read_length_prefixed(&payload[cursor..])?;
                cursor += advanced;
                let (after, _) = read_length_prefixed(&payload[cursor..])?;
                Self::Update {
                    page_id,
                    slot_id,
                    before,
                    after,
                }
            }
            RecordKind::Checkpoint => Self::decode_checkpoint(payload)?,
            RecordKind::Clr => Self::decode_clr(payload)?,
            RecordKind::Catalog => Self::Catalog {
                snapshot: payload.to_vec(),
            },
            RecordKind::RlsPolicies => Self::RlsPolicies {
                snapshot: payload.to_vec(),
            },
        };
        Ok(record)
    }

    fn decode_checkpoint(payload: &[u8]) -> Result<Self> {
        if payload.len() < 4 {
            return Err(WalError::PayloadTruncated {
                expected: 4,
                available: payload.len(),
            });
        }
        let count = u32::from_le_bytes(payload[0..4].try_into().expect("4 bytes")) as usize;
        let needed = 4 + count * 16;
        if payload.len() < needed {
            return Err(WalError::PayloadTruncated {
                expected: needed,
                available: payload.len(),
            });
        }
        let mut active_txns = Vec::with_capacity(count);
        let mut cursor = 4;
        for _ in 0..count {
            let txn_id =
                u64::from_le_bytes(payload[cursor..cursor + 8].try_into().expect("8 bytes"));
            let last_lsn = u64::from_le_bytes(
                payload[cursor + 8..cursor + 16]
                    .try_into()
                    .expect("8 bytes"),
            );
            active_txns.push((txn_id, last_lsn));
            cursor += 16;
        }
        Ok(Self::Checkpoint { active_txns })
    }

    fn decode_clr(payload: &[u8]) -> Result<Self> {
        // page_id (8) + slot_id (2) + undo_next (8) = 18 fixed.
        if payload.len() < 18 {
            return Err(WalError::PayloadTruncated {
                expected: 18,
                available: payload.len(),
            });
        }
        let page_id = u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
        let slot_id = u16::from_le_bytes(payload[8..10].try_into().expect("2 bytes"));
        let undo_next = u64::from_le_bytes(payload[10..18].try_into().expect("8 bytes"));
        let (undo_image, _) = read_length_prefixed(&payload[18..])?;
        Ok(Self::Clr {
            page_id,
            slot_id,
            undo_image,
            undo_next,
        })
    }
}

/// Read a `u16`-length-prefixed byte slice from the start of `payload`.
/// Returns the bytes and the number of bytes consumed (length prefix + data).
fn read_length_prefixed(payload: &[u8]) -> Result<(Vec<u8>, usize)> {
    if payload.len() < 2 {
        return Err(WalError::PayloadTruncated {
            expected: 2,
            available: payload.len(),
        });
    }
    let len = u16::from_le_bytes(payload[0..2].try_into().expect("2 bytes")) as usize;
    if payload.len() < 2 + len {
        return Err(WalError::PayloadTruncated {
            expected: 2 + len,
            available: payload.len(),
        });
    }
    let bytes = payload[2..2 + len].to_vec();
    Ok((bytes, 2 + len))
}

/// Peek at the `length` field of the next record in `buf` without parsing
/// the rest. Returns `None` if `buf` is shorter than 4 bytes (clean EOF).
#[must_use]
pub fn read_length(buf: &[u8]) -> Option<u32> {
    if buf.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes(
        buf[LENGTH_OFFSET..LENGTH_OFFSET + 4]
            .try_into()
            .expect("4 bytes"),
    ))
}

/// Fixed-header fields of a WAL record. Returned alongside the variant by
/// [`LogRecord::read`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RecordHeader {
    /// Total record size in bytes, including length field and checksum.
    pub length: u32,
    /// Record discriminant.
    pub kind: RecordKind,
    /// Monotonic record LSN.
    pub lsn: Lsn,
    /// Owning transaction id.
    pub txn: TxnId,
    /// Previous record in this txn's undo chain. [`Lsn::INVALID`] for the
    /// first record in a transaction.
    pub prev_lsn: Lsn,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lsn(v: u64) -> Lsn {
        Lsn::new(v)
    }
    fn txn(v: u64) -> TxnId {
        TxnId::new(v)
    }

    fn round_trip(
        rec: &LogRecord,
        lsn_v: u64,
        prev_v: u64,
        txn_v: u64,
    ) -> (RecordHeader, LogRecord) {
        let mut buf = Vec::new();
        rec.write(lsn(lsn_v), lsn(prev_v), txn(txn_v), &mut buf);
        let read_len = read_length(&buf).expect("len");
        assert_eq!(
            read_len as usize,
            buf.len(),
            "length field mismatches buf len"
        );
        LogRecord::read(&buf).expect("read")
    }

    #[test]
    fn begin_round_trips() {
        let (hdr, rec) = round_trip(&LogRecord::Begin, 1, u64::MAX, 7);
        assert_eq!(hdr.kind, RecordKind::Begin);
        assert_eq!(hdr.lsn, lsn(1));
        assert_eq!(hdr.txn, txn(7));
        assert!(hdr.prev_lsn.is_invalid());
        assert_eq!(rec, LogRecord::Begin);
    }

    #[test]
    fn commit_round_trips() {
        let (hdr, rec) = round_trip(&LogRecord::Commit, 42, 41, 7);
        assert_eq!(hdr.kind, RecordKind::Commit);
        assert_eq!(rec, LogRecord::Commit);
    }

    #[test]
    fn abort_round_trips() {
        let (hdr, rec) = round_trip(&LogRecord::Abort, 9, 8, 3);
        assert_eq!(hdr.kind, RecordKind::Abort);
        assert_eq!(rec, LogRecord::Abort);
    }

    #[test]
    fn update_round_trips_with_payloads() {
        let before: Vec<u8> = b"old data".to_vec();
        let after: Vec<u8> = b"new data goes here".to_vec();
        let r = LogRecord::Update {
            page_id: 0xDEAD_BEEF,
            slot_id: 42,
            before: before.clone(),
            after: after.clone(),
        };
        let (hdr, rec) = round_trip(&r, 100, 99, 5);
        assert_eq!(hdr.kind, RecordKind::Update);
        assert_eq!(
            rec,
            LogRecord::Update {
                page_id: 0xDEAD_BEEF,
                slot_id: 42,
                before,
                after,
            }
        );
    }

    #[test]
    fn update_empty_payloads_round_trip() {
        let r = LogRecord::Update {
            page_id: 1,
            slot_id: 0,
            before: vec![],
            after: vec![],
        };
        let (_hdr, rec) = round_trip(&r, 1, u64::MAX, 1);
        assert_eq!(rec, r);
    }

    #[test]
    fn catalog_round_trips() {
        let snapshot = b"users 3 4 5 ...serialized catalog body...".to_vec();
        let r = LogRecord::Catalog {
            snapshot: snapshot.clone(),
        };
        let (hdr, rec) = round_trip(&r, 200, 0, 0);
        assert_eq!(hdr.kind, RecordKind::Catalog);
        assert_eq!(rec, LogRecord::Catalog { snapshot });
    }

    #[test]
    fn catalog_snapshot_exceeding_u16_round_trips() {
        // A real catalog can exceed the 64 KiB a u16 length prefix allows, so
        // the payload carries no inner length prefix and the record length
        // (u32) bounds it.
        let snapshot = vec![0xABu8; 100_000];
        let r = LogRecord::Catalog {
            snapshot: snapshot.clone(),
        };
        let (hdr, rec) = round_trip(&r, 1, 0, 0);
        assert_eq!(hdr.kind, RecordKind::Catalog);
        assert_eq!(rec, LogRecord::Catalog { snapshot });
    }

    #[test]
    fn rls_policies_round_trips() {
        let snapshot = b"flags memories 1 0\npolicy CREATE POLICY tenant ON memories ...".to_vec();
        let r = LogRecord::RlsPolicies {
            snapshot: snapshot.clone(),
        };
        let (hdr, rec) = round_trip(&r, 7, 0, 0);
        assert_eq!(hdr.kind, RecordKind::RlsPolicies);
        assert_eq!(rec, LogRecord::RlsPolicies { snapshot });
    }

    #[test]
    fn checksum_corruption_detected() {
        let mut buf = Vec::new();
        LogRecord::Begin.write(lsn(1), Lsn::INVALID, txn(1), &mut buf);
        // Flip a bit somewhere in the middle.
        buf[15] ^= 0x01;
        let err = LogRecord::read(&buf).expect_err("must error");
        assert!(matches!(err, WalError::ChecksumMismatch));
    }

    #[test]
    fn unknown_type_byte_errors() {
        let mut buf = Vec::new();
        LogRecord::Begin.write(lsn(1), Lsn::INVALID, txn(1), &mut buf);
        // Stomp on the type byte with an unknown value, then re-compute the
        // checksum so the type-byte error is reached (not the checksum).
        buf[TYPE_OFFSET] = 99;
        let trailer_offset = buf.len() - TRAILER_BYTES;
        let new_crc = crc32(&buf[..trailer_offset]);
        buf[trailer_offset..].copy_from_slice(&new_crc.to_le_bytes());
        let err = LogRecord::read(&buf).expect_err("must error");
        assert!(matches!(err, WalError::UnknownRecordType(99)));
    }

    #[test]
    fn too_short_buf_rejected() {
        let buf = vec![0u8; MIN_RECORD_BYTES - 1];
        let err = LogRecord::read(&buf).expect_err("must error");
        assert!(matches!(err, WalError::RecordTooShort { .. }));
    }

    #[test]
    fn truncated_update_payload_rejected() {
        let r = LogRecord::Update {
            page_id: 1,
            slot_id: 0,
            before: vec![1, 2, 3, 4, 5],
            after: vec![6, 7, 8, 9],
        };
        let mut buf = Vec::new();
        r.write(lsn(1), Lsn::INVALID, txn(1), &mut buf);
        // Drop the last 3 bytes of the after-image (corrupting the
        // checksum will also fail, but the length-mismatch check fires
        // first).
        buf.truncate(buf.len() - 3);
        // Patch the length so it matches the new size to make the
        // length check pass; we want to exercise the inner truncation
        // detection.
        let new_len = u32::try_from(buf.len()).unwrap();
        buf[LENGTH_OFFSET..LENGTH_OFFSET + 4].copy_from_slice(&new_len.to_le_bytes());
        // Re-checksum.
        let trailer_offset = buf.len() - TRAILER_BYTES;
        let new_crc = crc32(&buf[..trailer_offset]);
        buf[trailer_offset..].copy_from_slice(&new_crc.to_le_bytes());
        let err = LogRecord::read(&buf).expect_err("must error");
        assert!(matches!(err, WalError::PayloadTruncated { .. }));
    }

    #[test]
    fn read_length_peek_returns_none_for_short_buf() {
        assert_eq!(read_length(&[]), None);
        assert_eq!(read_length(&[1, 2, 3]), None);
        assert_eq!(read_length(&[10, 0, 0, 0]), Some(10));
    }

    #[test]
    fn record_header_fields_match_written() {
        let r = LogRecord::Update {
            page_id: 17,
            slot_id: 3,
            before: vec![0xAA, 0xBB],
            after: vec![0xCC],
        };
        let mut buf = Vec::new();
        r.write(lsn(42), lsn(41), txn(99), &mut buf);
        let (hdr, rec) = LogRecord::read(&buf).expect("read");
        assert_eq!(hdr.lsn, lsn(42));
        assert_eq!(hdr.prev_lsn, lsn(41));
        assert_eq!(hdr.txn, txn(99));
        assert_eq!(hdr.length as usize, buf.len());
        assert_eq!(rec, r);
    }

    #[test]
    fn checkpoint_round_trips_with_active_txns() {
        let r = LogRecord::Checkpoint {
            active_txns: vec![(7, 100), (9, 142), (12, 7)],
        };
        let (hdr, rec) = round_trip(&r, 200, 199, 0);
        assert_eq!(hdr.kind, RecordKind::Checkpoint);
        assert_eq!(rec, r);
    }

    #[test]
    fn checkpoint_empty_active_list_round_trips() {
        let r = LogRecord::Checkpoint {
            active_txns: vec![],
        };
        let (_hdr, rec) = round_trip(&r, 1, Lsn::INVALID.get(), 0);
        assert_eq!(rec, r);
    }

    #[test]
    fn clr_round_trips() {
        let r = LogRecord::Clr {
            page_id: 0x1234_5678,
            slot_id: 9,
            undo_image: b"restore me".to_vec(),
            undo_next: 41,
        };
        let (hdr, rec) = round_trip(&r, 50, 49, 6);
        assert_eq!(hdr.kind, RecordKind::Clr);
        assert_eq!(rec, r);
    }

    #[test]
    fn clr_empty_undo_image_round_trips() {
        // Empty undo image = undo of an insert (tombstone the slot).
        let r = LogRecord::Clr {
            page_id: 3,
            slot_id: 0,
            undo_image: vec![],
            undo_next: Lsn::INVALID.get(),
        };
        let (_hdr, rec) = round_trip(&r, 5, 4, 2);
        assert_eq!(rec, r);
    }

    #[test]
    fn truncated_checkpoint_payload_rejected() {
        let r = LogRecord::Checkpoint {
            active_txns: vec![(1, 2), (3, 4)],
        };
        let mut buf = Vec::new();
        r.write(lsn(1), Lsn::INVALID, txn(0), &mut buf);
        // Drop the last 8 bytes (half of the second pair), patch length +
        // checksum so the inner truncation check fires.
        buf.truncate(buf.len() - 8);
        let new_len = u32::try_from(buf.len()).unwrap();
        buf[LENGTH_OFFSET..LENGTH_OFFSET + 4].copy_from_slice(&new_len.to_le_bytes());
        let trailer_offset = buf.len() - TRAILER_BYTES;
        let new_crc = crc32(&buf[..trailer_offset]);
        buf[trailer_offset..].copy_from_slice(&new_crc.to_le_bytes());
        let err = LogRecord::read(&buf).expect_err("must error");
        assert!(matches!(err, WalError::PayloadTruncated { .. }));
    }

    #[test]
    fn clr_checksum_corruption_detected() {
        let r = LogRecord::Clr {
            page_id: 1,
            slot_id: 2,
            undo_image: vec![9, 9, 9],
            undo_next: 0,
        };
        let mut buf = Vec::new();
        r.write(lsn(1), Lsn::INVALID, txn(1), &mut buf);
        buf[PAYLOAD_OFFSET] ^= 0xFF;
        let err = LogRecord::read(&buf).expect_err("must error");
        assert!(matches!(err, WalError::ChecksumMismatch));
    }
}
