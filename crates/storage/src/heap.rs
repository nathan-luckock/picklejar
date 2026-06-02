//! Slotted-page heap layout.
//!
//! A heap page stores variable-length tuples. The 24-byte [`PageHeader`]
//! occupies the start of the page; the slot directory grows upward (toward
//! higher offsets) from the end of the header; tuple data grows downward
//! (toward lower offsets) from the end of the page. Free space lives in
//! the middle.
//!
//! ```text
//!  0       24      ...                  free_space_ptr        PAGE_SIZE
//!  ┌───────┬─────────────────┬─────────┬──────────────────────────────┐
//!  │header │ slot directory →│  free   │ ← tuple data                 │
//!  └───────┴─────────────────┴─────────┴──────────────────────────────┘
//! ```
//!
//! # Slot directory entry (4 bytes, little-endian)
//!
//! | Offset | Size | Field   |
//! |--------|------|---------|
//! | 0      | 2    | `offset` (byte offset of tuple within the page) |
//! | 2      | 2    | `length` (tuple length in bytes; **0 = tombstoned**) |
//!
//! # Tombstones
//!
//! `delete` sets a slot's length to 0; the slot ID stays valid and is never
//! recycled. This keeps index entries pointing at slot IDs stable across
//! deletes — important once secondary B+ tree indexes exist (Sprint 2). The
//! tuple bytes are not reclaimed until [`HeapPage::compact`] runs.
//!
//! # Invariants
//!
//! - `header.free_space_ptr` ≥ end of slot directory at all times.
//! - Live tuples occupy `[header.free_space_ptr..PAGE_SIZE)` with no
//!   overlap; gaps between them are tombstoned regions awaiting `compact`.
//! - Empty tuples are not allowed — length 0 is the tombstone marker.
//!
//! [`PageHeader`]: crate::header::PageHeader

use crate::error::{Result, StorageError};
use crate::header::{PageHeader, PageType, FLAG_NEEDS_VACUUM, HEADER_SIZE, HEADER_SIZE_U16};
use crate::page::{Page, PAGE_SIZE, PAGE_SIZE_U16};

/// Bytes per slot directory entry.
pub const SLOT_SIZE: usize = 4;

/// [`SLOT_SIZE`] re-typed as a `u16`. Same pattern as [`HEADER_SIZE_U16`].
pub const SLOT_SIZE_U16: u16 = 4;

const _: () = assert!(
    SLOT_SIZE == SLOT_SIZE_U16 as usize,
    "SLOT_SIZE and SLOT_SIZE_U16 must agree",
);

/// Maximum tuple size in bytes — anything larger needs an overflow page
/// (Sprint 2). Currently `PAGE_SIZE - HEADER_SIZE - SLOT_SIZE = 8164`.
pub const MAX_TUPLE_SIZE: usize = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;

/// Threshold of tombstoned bytes that triggers the
/// [`FLAG_NEEDS_VACUUM`] hint. Tuned for the capstone demo;
/// real systems use adaptive thresholds.
const VACUUM_HINT_THRESHOLD: u16 = 1024;

/// Identifier for a slot within a heap page. Stable for the page's lifetime.
///
/// Slot IDs are assigned sequentially on insert and never recycled — even
/// after a `delete`, the same `SlotId` will continue to refer to the now-
/// tombstoned slot. This stability is what lets external structures
/// (indexes, MVCC version chains) hold direct references.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct SlotId(pub u16);

impl SlotId {
    /// Construct a `SlotId` from a raw `u16`.
    #[must_use]
    pub const fn new(id: u16) -> Self {
        Self(id)
    }

    /// The raw `u16` identifier.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl std::fmt::Display for SlotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "slot#{}", self.0)
    }
}

/// View over a heap page backed by a mutable page buffer.
///
/// `HeapPage` itself owns no allocation; it simply provides typed access to
/// the underlying [`Page`]. All mutations happen in place on the borrowed
/// buffer.
#[derive(Debug)]
pub struct HeapPage<'a> {
    buf: &'a mut Page,
}

impl<'a> HeapPage<'a> {
    /// Initialize `buf` as a fresh heap page, overwriting any previous
    /// contents.
    pub fn init(buf: &'a mut Page) -> Self {
        buf.fill(0);
        PageHeader::new_heap().write(buf);
        Self { buf }
    }

    /// Open an existing heap page. The header is validated;
    /// [`StorageError::WrongPageType`] is returned if the page is not a
    /// heap page.
    pub fn from_bytes(buf: &'a mut Page) -> Result<Self> {
        let header = PageHeader::read(buf)?;
        if header.page_type != PageType::Heap {
            return Err(StorageError::WrongPageType {
                expected: PageType::Heap,
                actual: header.page_type,
            });
        }
        Ok(Self { buf })
    }

    /// Borrow the underlying page buffer. Useful when handing the page back
    /// to the storage layer for persistence.
    #[must_use]
    pub fn as_bytes(&self) -> &Page {
        self.buf
    }

    fn header(&self) -> PageHeader {
        PageHeader::read(self.buf).expect("HeapPage always holds a valid heap header")
    }

    fn write_header(&mut self, h: PageHeader) {
        h.write(self.buf);
    }

    /// Number of slots in the directory, live + tombstoned.
    #[must_use]
    pub fn slot_count(&self) -> u16 {
        self.header().slot_count
    }

    /// Number of live (non-tombstoned) tuples on the page.
    #[must_use]
    pub fn tuple_count(&self) -> u16 {
        let count = self.header().slot_count;
        let live = (0..count).filter(|&i| self.slot_length_at(i) > 0).count();
        u16::try_from(live).expect("slot_count is u16, live ≤ slot_count")
    }

    /// Bytes available to a new insert. Accounts for the 4-byte slot
    /// directory entry the new tuple would require.
    #[must_use]
    pub fn free_space(&self) -> u16 {
        let h = self.header();
        let dir_end = HEADER_SIZE_U16 + h.slot_count.saturating_mul(SLOT_SIZE_U16);
        h.free_space_ptr.saturating_sub(dir_end)
    }

    /// Insert `tuple` into the page. Returns the assigned [`SlotId`].
    pub fn insert(&mut self, tuple: &[u8]) -> Result<SlotId> {
        if tuple.is_empty() {
            return Err(StorageError::EmptyTuple);
        }
        if tuple.len() > MAX_TUPLE_SIZE {
            return Err(StorageError::TupleTooLarge { size: tuple.len() });
        }
        // tuple.len() ≤ MAX_TUPLE_SIZE ≤ u16::MAX, so this never truncates.
        let tuple_len = u16::try_from(tuple.len()).expect("checked against MAX_TUPLE_SIZE");
        let needed = tuple_len
            .checked_add(SLOT_SIZE_U16)
            .ok_or(StorageError::TupleTooLarge { size: tuple.len() })?;
        if self.free_space() < needed {
            return Err(StorageError::PageFull {
                needed,
                available: self.free_space(),
            });
        }

        let mut h = self.header();
        let new_offset = h.free_space_ptr - tuple_len;
        let slot_id = SlotId::new(h.slot_count);

        self.buf[new_offset as usize..(new_offset as usize + tuple.len())].copy_from_slice(tuple);
        self.write_slot_at(h.slot_count, new_offset, tuple_len);

        h.slot_count += 1;
        h.free_space_ptr = new_offset;
        self.write_header(h);

        Ok(slot_id)
    }

    /// Read a tuple by slot ID. Returns `None` if the slot ID is out of
    /// range or the slot has been tombstoned.
    #[must_use]
    pub fn get(&self, slot: SlotId) -> Option<&[u8]> {
        if slot.0 >= self.slot_count() {
            return None;
        }
        let len = self.slot_length_at(slot.0);
        if len == 0 {
            return None;
        }
        let off = self.slot_offset_at(slot.0) as usize;
        Some(&self.buf[off..off + len as usize])
    }

    /// Tombstone a slot. The slot ID stays valid; the tuple bytes are
    /// retained until the next [`compact`](Self::compact). Tombstoning the
    /// same slot twice returns [`StorageError::SlotAlreadyDeleted`].
    pub fn delete(&mut self, slot: SlotId) -> Result<()> {
        let slot_count = self.slot_count();
        if slot.0 >= slot_count {
            return Err(StorageError::InvalidSlot {
                slot: slot.0,
                slot_count,
            });
        }
        let len = self.slot_length_at(slot.0);
        if len == 0 {
            return Err(StorageError::SlotAlreadyDeleted(slot.0));
        }
        let off = self.slot_offset_at(slot.0);
        // Keep `offset` for debugging — only length=0 marks tombstone.
        self.write_slot_at(slot.0, off, 0);

        // Maintain the FLAG_NEEDS_VACUUM hint so the buffer pool / future
        // vacuum process can prioritize this page.
        if self.tombstoned_bytes() >= VACUUM_HINT_THRESHOLD {
            let mut h = self.header();
            h.flags |= FLAG_NEEDS_VACUUM;
            self.write_header(h);
        }
        Ok(())
    }

    /// Reclaim space from tombstoned slots by packing live tuples toward
    /// the end of the page. Slot IDs of live tuples are preserved.
    ///
    /// Idempotent: a page with no tombstones is unchanged.
    pub fn compact(&mut self) {
        let count = self.slot_count();
        if count == 0 {
            return;
        }

        // Collect (slot_id, current_offset, length) for live tuples.
        let mut live: Vec<(u16, u16, u16)> = (0..count)
            .filter_map(|i| {
                let len = self.slot_length_at(i);
                if len > 0 {
                    Some((i, self.slot_offset_at(i), len))
                } else {
                    None
                }
            })
            .collect();

        if live.is_empty() {
            // Reset free_space_ptr to PAGE_SIZE — no live data.
            let mut h = self.header();
            h.free_space_ptr = PAGE_SIZE_U16;
            h.flags &= !FLAG_NEEDS_VACUUM;
            self.write_header(h);
            return;
        }

        // Sort live tuples by descending current offset so we can copy them
        // toward the end of the page without overlapping subsequent reads.
        live.sort_by_key(|entry| std::cmp::Reverse(entry.1));

        // Copy tuple bytes into a temp buffer (allocation here is fine —
        // page-local compaction is not in the hot path).
        let payloads: Vec<(u16, Vec<u8>)> = live
            .iter()
            .map(|&(slot_id, off, len)| {
                let bytes = self.buf[off as usize..(off + len) as usize].to_vec();
                (slot_id, bytes)
            })
            .collect();

        let mut write_end: u16 = PAGE_SIZE_U16;
        for (slot_id, bytes) in &payloads {
            let len = u16::try_from(bytes.len()).expect("bytes was sourced from a u16 length");
            let new_off = write_end - len;
            self.buf[new_off as usize..(new_off + len) as usize].copy_from_slice(bytes);
            self.write_slot_at(*slot_id, new_off, len);
            write_end = new_off;
        }

        let mut h = self.header();
        h.free_space_ptr = write_end;
        h.flags &= !FLAG_NEEDS_VACUUM;
        self.write_header(h);
    }

    // --- internal helpers ---

    fn slot_offset_at(&self, index: u16) -> u16 {
        let off = HEADER_SIZE + (index as usize) * SLOT_SIZE;
        u16::from_le_bytes(self.buf[off..off + 2].try_into().expect("2 bytes"))
    }

    fn slot_length_at(&self, index: u16) -> u16 {
        let off = HEADER_SIZE + (index as usize) * SLOT_SIZE + 2;
        u16::from_le_bytes(self.buf[off..off + 2].try_into().expect("2 bytes"))
    }

    fn write_slot_at(&mut self, index: u16, offset: u16, length: u16) {
        let off = HEADER_SIZE + (index as usize) * SLOT_SIZE;
        self.buf[off..off + 2].copy_from_slice(&offset.to_le_bytes());
        self.buf[off + 2..off + 4].copy_from_slice(&length.to_le_bytes());
    }

    /// Bytes locked up in tombstoned slots — the would-be space reclaimed
    /// by a `compact`. Internal accounting for the vacuum-hint flag.
    fn tombstoned_bytes(&self) -> u16 {
        let count = self.slot_count();
        // Walk the slot directory and sum (live tuple bytes) vs (occupied
        // region size). The difference is tombstoned bytes.
        //
        // Occupied region = PAGE_SIZE - free_space_ptr.
        // Live bytes      = Σ slot_length for live slots.
        let occupied = PAGE_SIZE_U16 - self.header().free_space_ptr;
        let live: u16 = (0..count).map(|i| self.slot_length_at(i)).sum();
        occupied.saturating_sub(live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::PageHeader;

    fn fresh_page() -> Box<Page> {
        Box::new([0u8; PAGE_SIZE])
    }

    #[test]
    fn init_produces_empty_heap_page() {
        let mut buf = fresh_page();
        let page = HeapPage::init(&mut buf);
        assert_eq!(page.slot_count(), 0);
        assert_eq!(page.tuple_count(), 0);
        // Slot directory is empty, all bytes after header are free.
        assert_eq!(page.free_space(), PAGE_SIZE_U16 - HEADER_SIZE_U16,);
    }

    #[test]
    fn from_bytes_accepts_initialized_heap_page() {
        let mut buf = fresh_page();
        HeapPage::init(&mut buf);
        assert!(HeapPage::from_bytes(&mut buf).is_ok());
    }

    #[test]
    fn from_bytes_rejects_non_heap_page() {
        let mut buf = fresh_page();
        // Write a non-Heap header into the buffer.
        let mut h = PageHeader::new_heap();
        h.page_type = PageType::BTreeLeaf;
        h.write(&mut buf);
        let err = HeapPage::from_bytes(&mut buf).expect_err("must reject");
        assert!(matches!(
            err,
            StorageError::WrongPageType {
                expected: PageType::Heap,
                actual: PageType::BTreeLeaf
            }
        ));
    }

    #[test]
    fn insert_then_get_round_trips() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        let id = page.insert(b"hello, world").expect("insert");
        assert_eq!(id, SlotId::new(0));
        assert_eq!(page.tuple_count(), 1);
        assert_eq!(page.get(id), Some(&b"hello, world"[..]));
    }

    #[test]
    fn empty_tuple_rejected() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        let err = page.insert(b"").expect_err("must reject");
        assert!(matches!(err, StorageError::EmptyTuple));
    }

    #[test]
    fn oversized_tuple_rejected() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        let too_big = vec![0u8; MAX_TUPLE_SIZE + 1];
        let err = page.insert(&too_big).expect_err("must reject");
        assert!(matches!(err, StorageError::TupleTooLarge { .. }));
    }

    #[test]
    fn multiple_inserts_assign_sequential_slot_ids() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        for expected in 0u16..16 {
            let id = page
                .insert(format!("tuple-{expected}").as_bytes())
                .expect("insert");
            assert_eq!(id, SlotId::new(expected));
        }
        assert_eq!(page.slot_count(), 16);
        assert_eq!(page.tuple_count(), 16);
        for i in 0..16u16 {
            let want = format!("tuple-{i}");
            assert_eq!(page.get(SlotId::new(i)), Some(want.as_bytes()));
        }
    }

    #[test]
    fn fill_until_page_full_then_error() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        let payload = [0xAAu8; 256];
        let mut inserted = 0u32;
        loop {
            match page.insert(&payload) {
                Ok(_) => inserted += 1,
                Err(StorageError::PageFull { .. }) => break,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        // ~30 tuples of 256 bytes + slots in an 8 KiB page after the
        // 24-byte header — exact value is implementation-detail, but it
        // should be at least 25.
        assert!(inserted >= 25, "fit only {inserted} tuples — too few");
        assert_eq!(page.tuple_count(), u16::try_from(inserted).unwrap());
    }

    #[test]
    fn delete_then_get_returns_none() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        let id = page.insert(b"ephemeral").expect("insert");
        assert_eq!(page.get(id), Some(&b"ephemeral"[..]));
        page.delete(id).expect("delete");
        assert_eq!(page.get(id), None);
        assert_eq!(page.tuple_count(), 0);
        // Slot directory entry still exists — slot_count unchanged.
        assert_eq!(page.slot_count(), 1);
    }

    #[test]
    fn double_delete_errors() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        let id = page.insert(b"x").expect("insert");
        page.delete(id).expect("first delete");
        let err = page.delete(id).expect_err("second delete must fail");
        assert!(matches!(err, StorageError::SlotAlreadyDeleted(0)));
    }

    #[test]
    fn invalid_slot_id_errors_on_delete() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        page.insert(b"x").expect("insert");
        let err = page.delete(SlotId::new(99)).expect_err("must error");
        assert!(matches!(
            err,
            StorageError::InvalidSlot {
                slot: 99,
                slot_count: 1
            }
        ));
    }

    #[test]
    fn get_with_invalid_slot_id_returns_none() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        page.insert(b"x").expect("insert");
        assert_eq!(page.get(SlotId::new(42)), None);
    }

    #[test]
    fn delete_does_not_recycle_slot_ids() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        let a = page.insert(b"alpha").expect("insert a");
        let b = page.insert(b"bravo").expect("insert b");
        page.delete(a).expect("delete a");
        let c = page.insert(b"charlie").expect("insert c");
        // Slot ID 0 (a) is tombstoned, b stays valid, c gets a fresh ID 2.
        assert_eq!(a, SlotId::new(0));
        assert_eq!(b, SlotId::new(1));
        assert_eq!(c, SlotId::new(2));
        assert_eq!(page.get(a), None);
        assert_eq!(page.get(b), Some(&b"bravo"[..]));
        assert_eq!(page.get(c), Some(&b"charlie"[..]));
    }

    #[test]
    fn compact_reclaims_tombstoned_space() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        // Insert 4 fat tuples, delete two, then compact.
        let payload = [0xCDu8; 1024];
        let ids: Vec<SlotId> = (0..4)
            .map(|_| page.insert(&payload).expect("insert"))
            .collect();
        let before_free = page.free_space();
        page.delete(ids[1]).expect("delete 1");
        page.delete(ids[2]).expect("delete 2");
        // Tombstones don't change free_space (slot directory entries still
        // there + tuple bytes still resident).
        assert_eq!(page.free_space(), before_free);
        page.compact();
        // After compact, ~2048 bytes are back.
        assert!(page.free_space() > before_free + 2000);
        // Live tuples are still readable via their original slot IDs.
        assert_eq!(page.get(ids[0]), Some(&payload[..]));
        assert_eq!(page.get(ids[1]), None);
        assert_eq!(page.get(ids[2]), None);
        assert_eq!(page.get(ids[3]), Some(&payload[..]));
        assert_eq!(page.tuple_count(), 2);
        // slot_count is preserved — IDs stay stable.
        assert_eq!(page.slot_count(), 4);
    }

    #[test]
    fn compact_is_idempotent() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        let id = page.insert(b"keep me").expect("insert");
        let free_before = page.free_space();
        page.compact();
        assert_eq!(page.free_space(), free_before);
        page.compact();
        assert_eq!(page.get(id), Some(&b"keep me"[..]));
    }

    #[test]
    fn compact_on_all_tombstoned_resets_to_full_free_minus_directory() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        let ids: Vec<SlotId> = (0..8)
            .map(|i| page.insert(format!("t-{i}").as_bytes()).expect("insert"))
            .collect();
        for id in &ids {
            page.delete(*id).expect("delete");
        }
        page.compact();
        // After compact: no live data; free_space_ptr should be PAGE_SIZE.
        // Slot directory still has 8 tombstoned entries (32 bytes), so free
        // space = PAGE_SIZE - HEADER_SIZE - 32.
        let expected = PAGE_SIZE_U16 - HEADER_SIZE_U16 - 8 * SLOT_SIZE_U16;
        assert_eq!(page.free_space(), expected);
        assert_eq!(page.tuple_count(), 0);
    }

    #[test]
    fn many_deletes_set_needs_vacuum_flag() {
        let mut buf = fresh_page();
        let mut page = HeapPage::init(&mut buf);
        let payload = [0xEFu8; 256];
        let ids: Vec<SlotId> = (0..8)
            .map(|_| page.insert(&payload).expect("insert"))
            .collect();
        // Delete enough to exceed VACUUM_HINT_THRESHOLD (1024 bytes).
        for id in &ids[..5] {
            page.delete(*id).expect("delete");
        }
        let h = PageHeader::read(page.as_bytes()).unwrap();
        assert!(
            h.flags & FLAG_NEEDS_VACUUM != 0,
            "vacuum hint should be set after >1024 bytes tombstoned",
        );
        page.compact();
        let h2 = PageHeader::read(page.as_bytes()).unwrap();
        assert_eq!(
            h2.flags & FLAG_NEEDS_VACUUM,
            0,
            "compact must clear the vacuum hint",
        );
    }
}
