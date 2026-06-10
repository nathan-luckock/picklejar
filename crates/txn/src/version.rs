//! On-heap encoding of a tuple version.
//!
//! MVCC stores each row as a chain of *versions*. A version is the unit the
//! heap page actually holds (as opaque bytes); this module gives it
//! structure:
//!
//! ```text
//! ┌──────────┬──────────┬─────────────┬────────────┬──────────────┐
//! │ xmin u64 │ xmax u64 │ prev_page u64│ prev_slot u16│ payload [u8] │
//! └──────────┴──────────┴─────────────┴────────────┴──────────────┘
//!  0          8          16            24           26
//! ```
//!
//! - `xmin`: the transaction that created this version.
//! - `xmax`: the transaction that deleted it, or `0` if live.
//! - `prev_page` / `prev_slot`: a [`TupleRef`] to the previous (older)
//!   version of the same row, forming the version chain. `prev_page ==
//!   PageId::INVALID` means this is the oldest version.
//! - `payload`: the row's actual bytes.
//!
//! # Why `xmax` is mutated in place
//!
//! Deleting or updating a row does not rewrite the old version's payload; it
//! only stamps the old version's `xmax` with the deleting transaction. That
//! is a fixed-offset 8-byte write ([`set_xmax`]), so it never changes the
//! version's size and never needs the slot to be relocated.

use rustdb_storage::{PageId, SlotId, TupleRef};

use crate::error::{Result, TxnError};

/// Byte offsets within an encoded version.
const XMIN_OFFSET: usize = 0;
const XMAX_OFFSET: usize = 8;
const PREV_PAGE_OFFSET: usize = 16;
const PREV_SLOT_OFFSET: usize = 24;

/// Size of the fixed version header (everything before the payload).
pub const VERSION_HEADER_SIZE: usize = 26;

/// A decoded view of a tuple version. Borrows its payload from the
/// underlying page bytes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Version<'a> {
    /// Creating transaction id.
    pub xmin: u64,
    /// Deleting transaction id, or `0` if the version is live.
    pub xmax: u64,
    /// Previous (older) version of this row, or `None` if this is the oldest.
    pub prev: Option<TupleRef>,
    /// The row's bytes.
    pub payload: &'a [u8],
}

impl<'a> Version<'a> {
    /// Encode a version into a fresh byte buffer.
    #[must_use]
    pub fn encode(xmin: u64, xmax: u64, prev: Option<TupleRef>, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(VERSION_HEADER_SIZE + payload.len());
        out.extend_from_slice(&xmin.to_le_bytes());
        out.extend_from_slice(&xmax.to_le_bytes());
        let (prev_page, prev_slot) = prev.map_or((PageId::INVALID.get(), 0), |t| {
            (t.page_id.get(), t.slot_id.get())
        });
        out.extend_from_slice(&prev_page.to_le_bytes());
        out.extend_from_slice(&prev_slot.to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Decode a version from `bytes`. The payload borrows from `bytes`.
    pub fn decode(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < VERSION_HEADER_SIZE {
            return Err(TxnError::VersionTruncated {
                len: bytes.len(),
                min: VERSION_HEADER_SIZE,
            });
        }
        let xmin = u64::from_le_bytes(
            bytes[XMIN_OFFSET..XMIN_OFFSET + 8]
                .try_into()
                .expect("8 bytes"),
        );
        let xmax = u64::from_le_bytes(
            bytes[XMAX_OFFSET..XMAX_OFFSET + 8]
                .try_into()
                .expect("8 bytes"),
        );
        let prev_page = u64::from_le_bytes(
            bytes[PREV_PAGE_OFFSET..PREV_PAGE_OFFSET + 8]
                .try_into()
                .expect("8 bytes"),
        );
        let prev_slot = u16::from_le_bytes(
            bytes[PREV_SLOT_OFFSET..PREV_SLOT_OFFSET + 2]
                .try_into()
                .expect("2 bytes"),
        );
        let prev = if prev_page == PageId::INVALID.get() {
            None
        } else {
            Some(TupleRef::new(
                PageId::new(prev_page),
                SlotId::new(prev_slot),
            ))
        };
        let payload = &bytes[VERSION_HEADER_SIZE..];
        Ok(Self {
            xmin,
            xmax,
            prev,
            payload,
        })
    }

    /// Read just the `xmin` / `xmax` markers without decoding the rest.
    /// Cheap visibility precheck.
    pub fn read_markers(bytes: &[u8]) -> Result<(u64, u64)> {
        if bytes.len() < VERSION_HEADER_SIZE {
            return Err(TxnError::VersionTruncated {
                len: bytes.len(),
                min: VERSION_HEADER_SIZE,
            });
        }
        let xmin = u64::from_le_bytes(bytes[XMIN_OFFSET..XMIN_OFFSET + 8].try_into().unwrap());
        let xmax = u64::from_le_bytes(bytes[XMAX_OFFSET..XMAX_OFFSET + 8].try_into().unwrap());
        Ok((xmin, xmax))
    }
}

/// Stamp a version's `xmax` in place. Used to mark a version deleted by an
/// update or delete without rewriting its payload.
///
/// # Errors
///
/// Returns [`TxnError::VersionTruncated`] if `bytes` is too short to hold
/// the header.
pub fn set_xmax(bytes: &mut [u8], xmax: u64) -> Result<()> {
    if bytes.len() < VERSION_HEADER_SIZE {
        return Err(TxnError::VersionTruncated {
            len: bytes.len(),
            min: VERSION_HEADER_SIZE,
        });
    }
    bytes[XMAX_OFFSET..XMAX_OFFSET + 8].copy_from_slice(&xmax.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tref(page: u64, slot: u16) -> TupleRef {
        TupleRef::new(PageId::new(page), SlotId::new(slot))
    }

    #[test]
    fn round_trips_with_prev_and_payload() {
        let enc = Version::encode(7, 9, Some(tref(42, 3)), b"hello world");
        let v = Version::decode(&enc).expect("decode");
        assert_eq!(v.xmin, 7);
        assert_eq!(v.xmax, 9);
        assert_eq!(v.prev, Some(tref(42, 3)));
        assert_eq!(v.payload, b"hello world");
    }

    #[test]
    fn none_prev_round_trips() {
        let enc = Version::encode(1, 0, None, b"oldest");
        let v = Version::decode(&enc).expect("decode");
        assert_eq!(v.prev, None);
        assert_eq!(v.xmax, 0);
        assert_eq!(v.payload, b"oldest");
    }

    #[test]
    fn empty_payload_round_trips() {
        let enc = Version::encode(3, 0, None, b"");
        assert_eq!(enc.len(), VERSION_HEADER_SIZE);
        let v = Version::decode(&enc).expect("decode");
        assert_eq!(v.payload, b"");
    }

    #[test]
    fn set_xmax_flips_in_place_without_touching_payload() {
        let mut enc = Version::encode(5, 0, Some(tref(1, 1)), b"payload-bytes");
        set_xmax(&mut enc, 99).expect("set");
        let v = Version::decode(&enc).expect("decode");
        assert_eq!(v.xmax, 99);
        assert_eq!(v.xmin, 5, "xmin untouched");
        assert_eq!(v.prev, Some(tref(1, 1)), "prev untouched");
        assert_eq!(v.payload, b"payload-bytes", "payload untouched");
    }

    #[test]
    fn truncated_buffer_rejected_on_decode() {
        let buf = vec![0u8; VERSION_HEADER_SIZE - 1];
        let err = Version::decode(&buf).expect_err("must reject");
        assert!(matches!(err, TxnError::VersionTruncated { .. }));
    }

    #[test]
    fn truncated_buffer_rejected_on_set_xmax() {
        let mut buf = vec![0u8; 10];
        let err = set_xmax(&mut buf, 1).expect_err("must reject");
        assert!(matches!(err, TxnError::VersionTruncated { .. }));
    }

    #[test]
    fn read_markers_matches_decode() {
        let enc = Version::encode(11, 22, None, b"data");
        let (xmin, xmax) = Version::read_markers(&enc).expect("markers");
        assert_eq!((xmin, xmax), (11, 22));
    }

    #[test]
    fn invalid_prev_page_decodes_to_none() {
        // Manually craft a version whose prev_page is INVALID.
        let enc = Version::encode(1, 0, None, b"x");
        let v = Version::decode(&enc).expect("decode");
        assert!(v.prev.is_none());
    }
}
