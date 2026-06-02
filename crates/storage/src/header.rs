//! 24-byte page header: LSN, checksum, type, slot directory, flags.
//!
//! Every page on disk starts with this struct, serialized little-endian.
//! Layout is frozen by [docs/design.md](../../../docs/design.md#slotted-page-format-heap-tables):
//!
//! | Offset | Size | Field            | Notes                                              |
//! |--------|------|------------------|----------------------------------------------------|
//! | 0      | 8    | `lsn`            | Last LSN that touched this page. WAL ordering.     |
//! | 8      | 4    | `checksum`       | CRC32 of bytes `[12..PAGE_SIZE]`.                  |
//! | 12     | 2    | `page_type`      | Free / Heap / `BTreeInternal` / `BTreeLeaf` / Overflow. |
//! | 14     | 2    | `slot_count`     | Live + tombstoned slots.                           |
//! | 16     | 2    | `free_space_ptr` | Offset where free region ends (tuples grow up).    |
//! | 18     | 2    | `flags`          | Bit 0 = dirty (in-memory), 1 = needs vacuum.       |
//! | 20     | 4    | `reserved`       | Zero. Reserved for MVCC chain pointer.             |
//!
//! # Checksum scope
//!
//! The checksum covers bytes `[12..PAGE_SIZE]` — the page **excluding the LSN
//! and the checksum field itself**. Excluding the LSN means a WAL flush that
//! updates only the LSN doesn't require recomputing the checksum (a
//! micro-optimization that adds up at high write rates). The checksum is
//! enough to catch torn writes and silent bit-rot in the payload, which is
//! what it's there for.

use crate::crc32::crc32;
use crate::error::{Result, StorageError};
use crate::page::{Page, PAGE_SIZE, PAGE_SIZE_U16};

/// Size of the page header in bytes.
pub const HEADER_SIZE: usize = 24;

/// [`HEADER_SIZE`] re-typed as a `u16`. Same trick as
/// [`crate::page::PAGE_SIZE_U16`] — lets `u16` arithmetic stay clean in
/// the slotted-page layout without per-call `try_from` ceremony.
pub const HEADER_SIZE_U16: u16 = 24;

const _: () = assert!(
    HEADER_SIZE == HEADER_SIZE_U16 as usize,
    "HEADER_SIZE and HEADER_SIZE_U16 must agree",
);

/// Byte range covered by the checksum: everything after the LSN and the
/// checksum field itself.
pub const CHECKSUM_RANGE: std::ops::Range<usize> = 12..PAGE_SIZE;

/// Byte offset of the checksum field within the page.
const CHECKSUM_OFFSET: usize = 8;

// --- flag bits ---

/// Set if the page has been modified in memory and not yet written back.
/// In-memory only; never persists to disk (flush path clears it).
pub const FLAG_DIRTY: u16 = 0b0000_0001;

/// Set if the page has accumulated enough tombstones that vacuum would
/// reclaim meaningful space.
pub const FLAG_NEEDS_VACUUM: u16 = 0b0000_0010;

/// On-disk page type. Encoded as a `u16` in the header.
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PageType {
    /// Allocated but uninitialized. Used briefly between `allocate_page`
    /// and the first `init` call.
    Free = 0,
    /// Heap table page using the slotted-page layout.
    Heap = 1,
    /// B+ tree internal (non-leaf) node.
    BTreeInternal = 2,
    /// B+ tree leaf node.
    BTreeLeaf = 3,
    /// Overflow page (for tuples larger than ~`PAGE_SIZE`/4).
    Overflow = 4,
}

impl PageType {
    const fn from_u16(v: u16) -> Result<Self> {
        match v {
            0 => Ok(Self::Free),
            1 => Ok(Self::Heap),
            2 => Ok(Self::BTreeInternal),
            3 => Ok(Self::BTreeLeaf),
            4 => Ok(Self::Overflow),
            other => Err(StorageError::InvalidPageType(other)),
        }
    }
}

/// Page header. Each field maps to a fixed offset in the first 24 bytes of
/// the page; see the module docs for the byte layout.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PageHeader {
    /// Last LSN that touched this page.
    pub lsn: u64,
    /// CRC32 of `[12..PAGE_SIZE]`.
    pub checksum: u32,
    /// Page category.
    pub page_type: PageType,
    /// Number of slots (live + tombstoned) in the slot directory.
    pub slot_count: u16,
    /// Offset where the free region of the page ends. Tuples grow upward
    /// from here.
    pub free_space_ptr: u16,
    /// Bit field; see `FLAG_*` constants.
    pub flags: u16,
    /// Zero on disk. Reserved for an MVCC version-chain pointer.
    pub reserved: u32,
}

impl PageHeader {
    /// A header for a freshly allocated `Heap` page with an empty slot
    /// directory and the entire post-header region free.
    #[must_use]
    pub const fn new_heap() -> Self {
        Self {
            lsn: 0,
            checksum: 0,
            page_type: PageType::Heap,
            slot_count: 0,
            free_space_ptr: PAGE_SIZE_U16,
            flags: 0,
            reserved: 0,
        }
    }

    /// Read a header from the first 24 bytes of `page`. Validates the
    /// page-type discriminant; does NOT verify the checksum (callers verify
    /// when they care about data integrity).
    pub fn read(page: &Page) -> Result<Self> {
        let lsn = u64::from_le_bytes(page[0..8].try_into().expect("8 bytes"));
        let checksum = u32::from_le_bytes(page[8..12].try_into().expect("4 bytes"));
        let page_type_raw = u16::from_le_bytes(page[12..14].try_into().expect("2 bytes"));
        let page_type = PageType::from_u16(page_type_raw)?;
        let slot_count = u16::from_le_bytes(page[14..16].try_into().expect("2 bytes"));
        let free_space_ptr = u16::from_le_bytes(page[16..18].try_into().expect("2 bytes"));
        let flags = u16::from_le_bytes(page[18..20].try_into().expect("2 bytes"));
        let reserved = u32::from_le_bytes(page[20..24].try_into().expect("4 bytes"));
        Ok(Self {
            lsn,
            checksum,
            page_type,
            slot_count,
            free_space_ptr,
            flags,
            reserved,
        })
    }

    /// Serialize this header into the first 24 bytes of `page`.
    pub fn write(&self, page: &mut Page) {
        page[0..8].copy_from_slice(&self.lsn.to_le_bytes());
        page[8..12].copy_from_slice(&self.checksum.to_le_bytes());
        page[12..14].copy_from_slice(&(self.page_type as u16).to_le_bytes());
        page[14..16].copy_from_slice(&self.slot_count.to_le_bytes());
        page[16..18].copy_from_slice(&self.free_space_ptr.to_le_bytes());
        page[18..20].copy_from_slice(&self.flags.to_le_bytes());
        page[20..24].copy_from_slice(&self.reserved.to_le_bytes());
    }
}

/// Compute the checksum that *should* be in the header given the current
/// payload bytes. Does NOT read or modify the page header itself.
#[must_use]
pub fn compute_checksum(page: &Page) -> u32 {
    crc32(&page[CHECKSUM_RANGE])
}

/// Return true iff the checksum stored in the header matches the payload.
#[must_use]
pub fn verify_checksum(page: &Page) -> bool {
    let stored = u32::from_le_bytes(
        page[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4]
            .try_into()
            .expect("4 bytes"),
    );
    stored == compute_checksum(page)
}

/// Recompute the checksum over the payload and write it back into the
/// header in place. Use after modifying any byte in `[12..PAGE_SIZE]`.
pub fn recompute_checksum(page: &mut Page) {
    let new = compute_checksum(page);
    page[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&new.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_page() -> Box<Page> {
        Box::new([0u8; PAGE_SIZE])
    }

    #[test]
    fn header_round_trip() {
        let header = PageHeader {
            lsn: 0xdead_beef_cafe_babe,
            checksum: 0x1234_5678,
            page_type: PageType::BTreeLeaf,
            slot_count: 42,
            free_space_ptr: 4096,
            flags: FLAG_DIRTY | FLAG_NEEDS_VACUUM,
            reserved: 0xaabb_ccdd,
        };
        let mut page = make_page();
        header.write(&mut page);
        let read_back = PageHeader::read(&page).expect("read");
        assert_eq!(read_back, header);
    }

    #[test]
    fn new_heap_has_full_free_space() {
        let h = PageHeader::new_heap();
        assert_eq!(h.page_type, PageType::Heap);
        assert_eq!(h.slot_count, 0);
        assert_eq!(h.free_space_ptr, PAGE_SIZE_U16);
        assert_eq!(h.lsn, 0);
    }

    #[test]
    fn write_lays_out_fields_little_endian() {
        let header = PageHeader {
            lsn: 0x0102_0304_0506_0708,
            checksum: 0x0a0b_0c0d,
            page_type: PageType::Heap, // = 1
            slot_count: 0x1112,
            free_space_ptr: 0x2122,
            flags: 0x3132,
            reserved: 0x4142_4344,
        };
        let mut page = make_page();
        header.write(&mut page);
        // LSN, little-endian
        assert_eq!(
            &page[0..8],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
        // Checksum
        assert_eq!(&page[8..12], &[0x0d, 0x0c, 0x0b, 0x0a]);
        // page_type = 1u16
        assert_eq!(&page[12..14], &[0x01, 0x00]);
        // slot_count
        assert_eq!(&page[14..16], &[0x12, 0x11]);
        // free_space_ptr
        assert_eq!(&page[16..18], &[0x22, 0x21]);
        // flags
        assert_eq!(&page[18..20], &[0x32, 0x31]);
        // reserved
        assert_eq!(&page[20..24], &[0x44, 0x43, 0x42, 0x41]);
    }

    #[test]
    fn invalid_page_type_rejected() {
        let mut page = make_page();
        // Write a valid-ish header then corrupt page_type to an unknown value.
        PageHeader::new_heap().write(&mut page);
        page[12..14].copy_from_slice(&99u16.to_le_bytes());
        let err = PageHeader::read(&page).expect_err("must reject");
        assert!(matches!(err, StorageError::InvalidPageType(99)));
    }

    #[test]
    fn checksum_round_trip() {
        let mut page = make_page();
        PageHeader::new_heap().write(&mut page);
        // Stuff some payload.
        for (i, b) in page[HEADER_SIZE..].iter_mut().enumerate() {
            *b = u8::try_from(i % 251).unwrap();
        }
        recompute_checksum(&mut page);
        assert!(verify_checksum(&page));
    }

    #[test]
    fn checksum_catches_single_bit_flip_in_payload() {
        let mut page = make_page();
        PageHeader::new_heap().write(&mut page);
        for (i, b) in page[HEADER_SIZE..].iter_mut().enumerate() {
            *b = u8::try_from((i * 7) % 251).unwrap();
        }
        recompute_checksum(&mut page);
        assert!(verify_checksum(&page));

        // Flip one bit somewhere in the payload.
        page[1000] ^= 0x01;
        assert!(!verify_checksum(&page));
    }

    #[test]
    fn checksum_catches_single_bit_flip_at_every_payload_offset() {
        let mut page = make_page();
        PageHeader::new_heap().write(&mut page);
        for (i, b) in page[HEADER_SIZE..].iter_mut().enumerate() {
            *b = u8::try_from((i * 13) % 251).unwrap();
        }
        recompute_checksum(&mut page);
        let original = page.clone();

        // Sample 64 offsets across the payload (full sweep would be slow in
        // debug). For each, flip every bit and confirm the checksum
        // detects the corruption.
        for offset in (CHECKSUM_RANGE.start..PAGE_SIZE).step_by((PAGE_SIZE - HEADER_SIZE) / 64) {
            for bit in 0..8 {
                page[offset] ^= 1 << bit;
                assert!(
                    !verify_checksum(&page),
                    "checksum failed to detect flip at byte {offset} bit {bit}",
                );
                page.copy_from_slice(&*original);
            }
        }
    }

    #[test]
    fn lsn_change_does_not_invalidate_checksum() {
        // The checksum range starts at byte 12 — LSN changes (bytes 0..8)
        // must NOT require recomputing.
        let mut page = make_page();
        PageHeader::new_heap().write(&mut page);
        recompute_checksum(&mut page);
        assert!(verify_checksum(&page));

        // Now bump the LSN (in place) without touching the checksum.
        let new_lsn = 0xabcd_ef01_2345_6789u64;
        page[0..8].copy_from_slice(&new_lsn.to_le_bytes());
        assert!(
            verify_checksum(&page),
            "LSN update outside checksum range must not invalidate the checksum",
        );
    }

    #[test]
    fn checksum_change_is_caught_too() {
        // Modifying the checksum bytes themselves is fine (they're outside
        // the checksum range), but modifying the payload without
        // recomputing must fail.
        let mut page = make_page();
        PageHeader::new_heap().write(&mut page);
        recompute_checksum(&mut page);
        page[2000] = 0xff;
        assert!(!verify_checksum(&page));
    }
}
