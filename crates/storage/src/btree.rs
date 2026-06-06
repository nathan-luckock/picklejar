//! B+ tree node layouts (internal nodes for this commit; leaves and tree
//! operations land in follow-up commits).
//!
//! Each B+ tree node lives on its own page and starts with the standard
//! 24-byte [`PageHeader`]. The page's `page_type` field tells the storage
//! layer which on-disk format follows the header.
//!
//! # Internal node layout
//!
//! ```text
//!  0       24                                                  PAGE_SIZE
//!  ┌───────┬─────────┬──────────────┬───────────────────────────────────┐
//!  │header │key_count│ first_child  │ entries[]                         │
//!  │ (24)  │  (u16)  │ PageId (u64) │ (key u64, right_child PageId u64) │
//!  └───────┴─────────┴──────────────┴───────────────────────────────────┘
//!         24        26             34                            8192
//! ```
//!
//! Keys are sorted ascending. For an internal node with N keys the node
//! has N+1 children: `first_child` (left of all keys) plus N `right_child`
//! pointers (one per key, right of that key).
//!
//! ## Find-child semantics
//!
//! Given a query key Q:
//! 1. If Q is less than every key, follow `first_child`.
//! 2. Otherwise find the largest key index `i` such that `key[i] <= Q`
//!    and follow `entries[i].right_child`.
//!
//! ## Fanout
//!
//! Capacity is computed from page size: `(PAGE_SIZE - HEADER_SIZE - 10) / 16`.
//! For 8 KiB pages and 16-byte entries that yields 509 keys (so 510
//! children). Way above the original target of 128. Locked in
//! [`MAX_INTERNAL_KEYS`].
//!
//! [`PageHeader`]: crate::header::PageHeader

use crate::error::{Result, StorageError};
use crate::header::{PageHeader, PageType, HEADER_SIZE};
use crate::heap::SlotId;
use crate::page::{Page, PageId, PAGE_SIZE};

/// Bytes per entry in an internal node (`key u64` + `right_child PageId u64`).
pub const INTERNAL_ENTRY_SIZE: usize = 16;

/// Bytes between the page header and the entry array on an internal node:
/// `key_count u16` + `first_child PageId u64`.
const INTERNAL_FIXED_FIELDS: usize = 2 + 8;

/// Maximum number of keys an internal node can hold.
pub const MAX_INTERNAL_KEYS: usize =
    (PAGE_SIZE - HEADER_SIZE - INTERNAL_FIXED_FIELDS) / INTERNAL_ENTRY_SIZE;

/// `MAX_INTERNAL_KEYS` typed as `u16` for arithmetic that produces
/// `key_count` values. The compile-time assertion below guarantees the
/// cast can never truncate.
#[allow(clippy::cast_possible_truncation)]
pub const MAX_INTERNAL_KEYS_U16: u16 = MAX_INTERNAL_KEYS as u16;

const _: () = assert!(
    MAX_INTERNAL_KEYS <= u16::MAX as usize,
    "MAX_INTERNAL_KEYS must fit in u16",
);

// Field offsets.
const KEY_COUNT_OFFSET: usize = HEADER_SIZE; // 24
const FIRST_CHILD_OFFSET: usize = KEY_COUNT_OFFSET + 2; // 26
const ENTRIES_OFFSET: usize = FIRST_CHILD_OFFSET + 8; // 34

/// View over a B+ tree internal node page.
///
/// Wraps a `&mut Page` and provides typed access to the sorted (key,
/// child) directory. Owns no allocation.
#[derive(Debug)]
pub struct InternalPage<'a> {
    buf: &'a mut Page,
}

impl<'a> InternalPage<'a> {
    /// Initialize `buf` as a fresh internal node with `first_child` as its
    /// only child. Overwrites the buffer.
    pub fn init(buf: &'a mut Page, first_child: PageId) -> Self {
        buf.fill(0);
        let mut header = PageHeader::new_heap();
        header.page_type = PageType::BTreeInternal;
        header.write(buf);
        // key_count starts at 0.
        buf[KEY_COUNT_OFFSET..KEY_COUNT_OFFSET + 2].copy_from_slice(&0u16.to_le_bytes());
        buf[FIRST_CHILD_OFFSET..FIRST_CHILD_OFFSET + 8]
            .copy_from_slice(&first_child.get().to_le_bytes());
        Self { buf }
    }

    /// Open an existing internal node. Validates `page_type`.
    pub fn from_bytes(buf: &'a mut Page) -> Result<Self> {
        let header = PageHeader::read(buf)?;
        if header.page_type != PageType::BTreeInternal {
            return Err(StorageError::WrongPageType {
                expected: PageType::BTreeInternal,
                actual: header.page_type,
            });
        }
        Ok(Self { buf })
    }

    /// Borrow the underlying page buffer.
    #[must_use]
    pub fn as_bytes(&self) -> &Page {
        self.buf
    }

    /// Current number of keys in this node.
    #[must_use]
    pub fn key_count(&self) -> u16 {
        u16::from_le_bytes(
            self.buf[KEY_COUNT_OFFSET..KEY_COUNT_OFFSET + 2]
                .try_into()
                .expect("2 bytes"),
        )
    }

    /// The left-most child pointer (followed when the query key is less
    /// than every key in the node).
    #[must_use]
    pub fn first_child(&self) -> PageId {
        let raw = u64::from_le_bytes(
            self.buf[FIRST_CHILD_OFFSET..FIRST_CHILD_OFFSET + 8]
                .try_into()
                .expect("8 bytes"),
        );
        PageId::new(raw)
    }

    /// True when the node has reached `MAX_INTERNAL_KEYS`. Caller should
    /// split before inserting another key.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.key_count() >= MAX_INTERNAL_KEYS_U16
    }

    /// The `(key, right_child)` pair at the given index. Panics if `index`
    /// is out of range; callers should bounds-check via [`key_count`].
    ///
    /// [`key_count`]: Self::key_count
    #[must_use]
    pub fn entry_at(&self, index: u16) -> (u64, PageId) {
        debug_assert!(
            index < self.key_count(),
            "entry_at({}) out of bounds (key_count={})",
            index,
            self.key_count(),
        );
        let off = ENTRIES_OFFSET + (index as usize) * INTERNAL_ENTRY_SIZE;
        let key = u64::from_le_bytes(self.buf[off..off + 8].try_into().expect("8 bytes"));
        let child = u64::from_le_bytes(self.buf[off + 8..off + 16].try_into().expect("8 bytes"));
        (key, PageId::new(child))
    }

    /// Iterator over the keys in ascending order.
    pub fn iter_keys(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.key_count()).map(|i| self.entry_at(i).0)
    }

    /// Iterator over all `(key, right_child)` entries in ascending key order.
    pub fn iter_entries(&self) -> impl Iterator<Item = (u64, PageId)> + '_ {
        (0..self.key_count()).map(|i| self.entry_at(i))
    }

    /// Find the child page that should be followed for `query_key`.
    ///
    /// Returns `first_child` when `query_key` is less than every key in
    /// the node; otherwise returns `entries[i].right_child` where `i` is
    /// the largest index with `key[i] <= query_key`.
    #[must_use]
    pub fn find_child(&self, query_key: u64) -> PageId {
        let count = self.key_count();
        if count == 0 {
            return self.first_child();
        }
        // Binary search for the largest i such that key[i] <= query_key.
        // Standard "upper_bound minus 1" pattern.
        let mut lo: u16 = 0;
        let mut hi: u16 = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (k, _) = self.entry_at(mid);
            if k <= query_key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // lo is now the upper_bound, i.e., first index with key > query_key.
        // The largest i with key[i] <= query_key is lo - 1.
        if lo == 0 {
            // Every key in the node is strictly greater than query_key.
            self.first_child()
        } else {
            self.entry_at(lo - 1).1
        }
    }

    /// Set the `key_count` field. Internal helper used during splits.
    fn set_key_count(&mut self, count: u16) {
        self.buf[KEY_COUNT_OFFSET..KEY_COUNT_OFFSET + 2].copy_from_slice(&count.to_le_bytes());
    }

    /// Insert a new `(key, right_child)` entry. Keeps entries sorted
    /// ascending by key.
    ///
    /// Returns [`StorageError::BTreeNodeFull`] when the node has reached
    /// [`MAX_INTERNAL_KEYS`]. Caller is expected to split. Duplicate keys
    /// are rejected (B+ tree internal nodes carry separator keys, and
    /// duplicates would break the `find_child` invariant).
    pub fn insert(&mut self, key: u64, right_child: PageId) -> Result<()> {
        if self.is_full() {
            return Err(StorageError::BTreeNodeFull {
                key_count: self.key_count(),
                capacity: MAX_INTERNAL_KEYS_U16,
            });
        }
        let count = self.key_count();
        // Find insertion index: lowest i with key[i] >= key. Binary search.
        let mut lo: u16 = 0;
        let mut hi: u16 = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (k, _) = self.entry_at(mid);
            if k < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // Reject duplicate.
        if lo < count {
            let (existing, _) = self.entry_at(lo);
            if existing == key {
                return Err(StorageError::DuplicateBTreeKey(key));
            }
        }
        // Shift entries [lo, count) right by one slot to make room.
        let insert_off = ENTRIES_OFFSET + (lo as usize) * INTERNAL_ENTRY_SIZE;
        let last_used_off = ENTRIES_OFFSET + (count as usize) * INTERNAL_ENTRY_SIZE;
        if lo < count {
            self.buf
                .copy_within(insert_off..last_used_off, insert_off + INTERNAL_ENTRY_SIZE);
        }
        // Write the new entry.
        self.buf[insert_off..insert_off + 8].copy_from_slice(&key.to_le_bytes());
        self.buf[insert_off + 8..insert_off + 16].copy_from_slice(&right_child.get().to_le_bytes());
        // Bump key_count.
        self.set_key_count(count + 1);
        Ok(())
    }
}

// ============================================================================
// Leaf node
// ============================================================================

/// Reference to a tuple stored on a heap page. B+ tree leaves carry these
/// instead of the tuple bytes; the actual data lives in the heap.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct TupleRef {
    /// Heap page that holds the tuple.
    pub page_id: PageId,
    /// Slot within that page.
    pub slot_id: SlotId,
}

impl TupleRef {
    /// Construct a `TupleRef` from raw parts.
    #[must_use]
    pub const fn new(page_id: PageId, slot_id: SlotId) -> Self {
        Self { page_id, slot_id }
    }
}

/// Bytes per entry in a leaf node.
///
/// Layout: 8 (key) + 8 (`page_id`) + 2 (`slot_id`). Packed, not padded.
/// Reads go through `from_le_bytes` and there is no alignment requirement
/// on byte slices, so the padding would just waste fanout.
pub const LEAF_ENTRY_SIZE: usize = 18;

/// Bytes between the page header and the entry array on a leaf node:
/// `key_count u16` + `next_leaf PageId u64`.
const LEAF_FIXED_FIELDS: usize = 2 + 8;

/// Maximum number of `(key, tuple_ref)` pairs a leaf can hold.
pub const MAX_LEAF_KEYS: usize = (PAGE_SIZE - HEADER_SIZE - LEAF_FIXED_FIELDS) / LEAF_ENTRY_SIZE;

/// `MAX_LEAF_KEYS` typed as `u16`. The compile-time assertion below
/// guarantees the cast can never truncate.
#[allow(clippy::cast_possible_truncation)]
pub const MAX_LEAF_KEYS_U16: u16 = MAX_LEAF_KEYS as u16;

const _: () = assert!(
    MAX_LEAF_KEYS <= u16::MAX as usize,
    "MAX_LEAF_KEYS must fit in u16",
);

// Leaf field offsets.
const LEAF_KEY_COUNT_OFFSET: usize = HEADER_SIZE; // 24
const LEAF_NEXT_LEAF_OFFSET: usize = LEAF_KEY_COUNT_OFFSET + 2; // 26
const LEAF_ENTRIES_OFFSET: usize = LEAF_NEXT_LEAF_OFFSET + 8; // 34

/// View over a B+ tree leaf node page.
#[derive(Debug)]
pub struct LeafPage<'a> {
    buf: &'a mut Page,
}

impl<'a> LeafPage<'a> {
    /// Initialize `buf` as a fresh leaf with the given `next_leaf` sibling
    /// pointer. Use [`PageId::INVALID`] for the right-most leaf.
    pub fn init(buf: &'a mut Page, next_leaf: PageId) -> Self {
        buf.fill(0);
        let mut header = PageHeader::new_heap();
        header.page_type = PageType::BTreeLeaf;
        header.write(buf);
        buf[LEAF_KEY_COUNT_OFFSET..LEAF_KEY_COUNT_OFFSET + 2].copy_from_slice(&0u16.to_le_bytes());
        buf[LEAF_NEXT_LEAF_OFFSET..LEAF_NEXT_LEAF_OFFSET + 8]
            .copy_from_slice(&next_leaf.get().to_le_bytes());
        Self { buf }
    }

    /// Open an existing leaf. Validates `page_type`.
    pub fn from_bytes(buf: &'a mut Page) -> Result<Self> {
        let header = PageHeader::read(buf)?;
        if header.page_type != PageType::BTreeLeaf {
            return Err(StorageError::WrongPageType {
                expected: PageType::BTreeLeaf,
                actual: header.page_type,
            });
        }
        Ok(Self { buf })
    }

    /// Borrow the underlying page buffer.
    #[must_use]
    pub fn as_bytes(&self) -> &Page {
        self.buf
    }

    /// Current number of live keys in this leaf.
    #[must_use]
    pub fn key_count(&self) -> u16 {
        u16::from_le_bytes(
            self.buf[LEAF_KEY_COUNT_OFFSET..LEAF_KEY_COUNT_OFFSET + 2]
                .try_into()
                .expect("2 bytes"),
        )
    }

    /// The next leaf in the sibling chain, or [`PageId::INVALID`] if this
    /// is the right-most leaf.
    #[must_use]
    pub fn next_leaf(&self) -> PageId {
        let raw = u64::from_le_bytes(
            self.buf[LEAF_NEXT_LEAF_OFFSET..LEAF_NEXT_LEAF_OFFSET + 8]
                .try_into()
                .expect("8 bytes"),
        );
        PageId::new(raw)
    }

    /// Update the sibling pointer. Pass [`PageId::INVALID`] when this leaf
    /// becomes the right-most.
    pub fn set_next_leaf(&mut self, next: PageId) {
        self.buf[LEAF_NEXT_LEAF_OFFSET..LEAF_NEXT_LEAF_OFFSET + 8]
            .copy_from_slice(&next.get().to_le_bytes());
    }

    /// True when the leaf has reached [`MAX_LEAF_KEYS`]. Caller should split.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.key_count() >= MAX_LEAF_KEYS_U16
    }

    /// The `(key, tuple_ref)` pair at the given index. Panics if `index`
    /// is out of range; callers should bounds-check via [`key_count`].
    ///
    /// [`key_count`]: Self::key_count
    #[must_use]
    pub fn entry_at(&self, index: u16) -> (u64, TupleRef) {
        debug_assert!(
            index < self.key_count(),
            "entry_at({}) out of bounds (key_count={})",
            index,
            self.key_count(),
        );
        let off = LEAF_ENTRIES_OFFSET + (index as usize) * LEAF_ENTRY_SIZE;
        let key = u64::from_le_bytes(self.buf[off..off + 8].try_into().expect("8 bytes"));
        let page = u64::from_le_bytes(self.buf[off + 8..off + 16].try_into().expect("8 bytes"));
        let slot = u16::from_le_bytes(self.buf[off + 16..off + 18].try_into().expect("2 bytes"));
        (key, TupleRef::new(PageId::new(page), SlotId::new(slot)))
    }

    /// Iterator over the keys in ascending order.
    pub fn iter_keys(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.key_count()).map(|i| self.entry_at(i).0)
    }

    /// Iterator over all `(key, tuple_ref)` entries in ascending key order.
    pub fn iter_entries(&self) -> impl Iterator<Item = (u64, TupleRef)> + '_ {
        (0..self.key_count()).map(|i| self.entry_at(i))
    }

    /// Binary search for `key`. Returns the tuple reference if present.
    #[must_use]
    pub fn find_key(&self, key: u64) -> Option<TupleRef> {
        let count = self.key_count();
        let mut lo: u16 = 0;
        let mut hi: u16 = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (k, t) = self.entry_at(mid);
            match k.cmp(&key) {
                std::cmp::Ordering::Equal => return Some(t),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    /// Set `key_count`. Internal helper for split / merge paths.
    fn set_key_count(&mut self, count: u16) {
        self.buf[LEAF_KEY_COUNT_OFFSET..LEAF_KEY_COUNT_OFFSET + 2]
            .copy_from_slice(&count.to_le_bytes());
    }

    /// Insert a new `(key, tuple_ref)` entry. Keeps entries sorted by key.
    /// Rejects duplicates with [`StorageError::DuplicateBTreeKey`].
    pub fn insert(&mut self, key: u64, tuple_ref: TupleRef) -> Result<()> {
        if self.is_full() {
            return Err(StorageError::BTreeNodeFull {
                key_count: self.key_count(),
                capacity: MAX_LEAF_KEYS_U16,
            });
        }
        let count = self.key_count();
        let mut lo: u16 = 0;
        let mut hi: u16 = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (k, _) = self.entry_at(mid);
            if k < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < count {
            let (existing, _) = self.entry_at(lo);
            if existing == key {
                return Err(StorageError::DuplicateBTreeKey(key));
            }
        }
        let insert_off = LEAF_ENTRIES_OFFSET + (lo as usize) * LEAF_ENTRY_SIZE;
        let last_used_off = LEAF_ENTRIES_OFFSET + (count as usize) * LEAF_ENTRY_SIZE;
        if lo < count {
            self.buf
                .copy_within(insert_off..last_used_off, insert_off + LEAF_ENTRY_SIZE);
        }
        self.buf[insert_off..insert_off + 8].copy_from_slice(&key.to_le_bytes());
        self.buf[insert_off + 8..insert_off + 16]
            .copy_from_slice(&tuple_ref.page_id.get().to_le_bytes());
        self.buf[insert_off + 16..insert_off + 18]
            .copy_from_slice(&tuple_ref.slot_id.get().to_le_bytes());
        self.set_key_count(count + 1);
        Ok(())
    }

    /// Delete the entry for `key`. Shifts later entries left by one slot.
    /// Returns the removed `TupleRef`.
    pub fn delete(&mut self, key: u64) -> Result<TupleRef> {
        let count = self.key_count();
        let mut lo: u16 = 0;
        let mut hi: u16 = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (k, _) = self.entry_at(mid);
            match k.cmp(&key) {
                std::cmp::Ordering::Equal => {
                    let (_, removed) = self.entry_at(mid);
                    let entry_off = LEAF_ENTRIES_OFFSET + (mid as usize) * LEAF_ENTRY_SIZE;
                    let last_off = LEAF_ENTRIES_OFFSET + (count as usize) * LEAF_ENTRY_SIZE;
                    if mid + 1 < count {
                        self.buf
                            .copy_within(entry_off + LEAF_ENTRY_SIZE..last_off, entry_off);
                    }
                    // Zero the now-unused tail so debug dumps stay clean.
                    let new_last_off = last_off - LEAF_ENTRY_SIZE;
                    for b in &mut self.buf[new_last_off..last_off] {
                        *b = 0;
                    }
                    self.set_key_count(count - 1);
                    return Ok(removed);
                }
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Err(StorageError::BTreeKeyNotFound(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_page() -> Box<Page> {
        Box::new([0u8; PAGE_SIZE])
    }

    #[test]
    fn fanout_matches_documented_capacity() {
        // Sanity check on the layout math. If this assertion changes the
        // design doc needs updating.
        assert_eq!(MAX_INTERNAL_KEYS, 509);
    }

    #[test]
    fn init_produces_empty_internal_node() {
        let mut buf = fresh_page();
        let first = PageId::new(42);
        let node = InternalPage::init(&mut buf, first);
        assert_eq!(node.key_count(), 0);
        assert_eq!(node.first_child(), first);
        assert!(!node.is_full());
    }

    #[test]
    fn from_bytes_accepts_internal_page() {
        let mut buf = fresh_page();
        InternalPage::init(&mut buf, PageId::new(7));
        assert!(InternalPage::from_bytes(&mut buf).is_ok());
    }

    #[test]
    fn from_bytes_rejects_non_internal_page() {
        let mut buf = fresh_page();
        // Default new_heap header has page_type = Heap, not BTreeInternal.
        PageHeader::new_heap().write(&mut buf);
        let err = InternalPage::from_bytes(&mut buf).expect_err("must reject");
        assert!(matches!(
            err,
            StorageError::WrongPageType {
                expected: PageType::BTreeInternal,
                actual: PageType::Heap,
            }
        ));
    }

    #[test]
    fn insert_then_find_child_round_trips() {
        let mut buf = fresh_page();
        let mut node = InternalPage::init(&mut buf, PageId::new(100));
        // Three separators: 10 -> child 200, 20 -> child 300, 30 -> child 400.
        node.insert(10, PageId::new(200)).expect("insert 10");
        node.insert(20, PageId::new(300)).expect("insert 20");
        node.insert(30, PageId::new(400)).expect("insert 30");
        assert_eq!(node.key_count(), 3);
        // Query < first key falls through to first_child.
        assert_eq!(node.find_child(0), PageId::new(100));
        assert_eq!(node.find_child(9), PageId::new(100));
        // Exact-match keys go to the right child of that key.
        assert_eq!(node.find_child(10), PageId::new(200));
        assert_eq!(node.find_child(20), PageId::new(300));
        assert_eq!(node.find_child(30), PageId::new(400));
        // Between separators.
        assert_eq!(node.find_child(15), PageId::new(200));
        assert_eq!(node.find_child(25), PageId::new(300));
        // Above all keys.
        assert_eq!(node.find_child(999), PageId::new(400));
    }

    #[test]
    fn insert_maintains_sorted_order_under_random_order() {
        // Insert in scrambled order, verify iter_keys yields sorted.
        let mut buf = fresh_page();
        let mut node = InternalPage::init(&mut buf, PageId::new(0));
        let order = [50u64, 10, 30, 20, 40, 5, 60];
        for (i, &k) in order.iter().enumerate() {
            node.insert(k, PageId::new(1000 + i as u64))
                .expect("insert");
        }
        let keys: Vec<u64> = node.iter_keys().collect();
        let mut expected: Vec<u64> = order.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    #[test]
    fn insert_rejects_duplicate_key() {
        let mut buf = fresh_page();
        let mut node = InternalPage::init(&mut buf, PageId::new(0));
        node.insert(42, PageId::new(1)).expect("first");
        let err = node.insert(42, PageId::new(2)).expect_err("duplicate");
        assert!(matches!(err, StorageError::DuplicateBTreeKey(42)));
        // First insert still intact.
        assert_eq!(node.key_count(), 1);
    }

    #[test]
    fn is_full_at_max_keys() {
        let mut buf = fresh_page();
        let mut node = InternalPage::init(&mut buf, PageId::new(0));
        for i in 0..u64::from(MAX_INTERNAL_KEYS_U16) {
            node.insert(i, PageId::new(i + 1)).expect("insert");
        }
        assert!(node.is_full());
        let err = node.insert(9999, PageId::new(99)).expect_err("must reject");
        assert!(matches!(
            err,
            StorageError::BTreeNodeFull {
                key_count: MAX_INTERNAL_KEYS_U16,
                capacity: MAX_INTERNAL_KEYS_U16,
            }
        ));
    }

    #[test]
    fn find_child_on_empty_node_returns_first_child() {
        let mut buf = fresh_page();
        let node = InternalPage::init(&mut buf, PageId::new(7));
        assert_eq!(node.find_child(0), PageId::new(7));
        assert_eq!(node.find_child(u64::MAX), PageId::new(7));
    }

    #[test]
    fn entry_at_round_trips() {
        let mut buf = fresh_page();
        let mut node = InternalPage::init(&mut buf, PageId::new(0));
        node.insert(100, PageId::new(11)).expect("insert");
        node.insert(200, PageId::new(22)).expect("insert");
        node.insert(300, PageId::new(33)).expect("insert");
        assert_eq!(node.entry_at(0), (100, PageId::new(11)));
        assert_eq!(node.entry_at(1), (200, PageId::new(22)));
        assert_eq!(node.entry_at(2), (300, PageId::new(33)));
    }

    #[test]
    fn iter_entries_yields_all_in_order() {
        let mut buf = fresh_page();
        let mut node = InternalPage::init(&mut buf, PageId::new(0));
        let pairs = [(5u64, 50u64), (1, 10), (3, 30), (4, 40), (2, 20)];
        for &(k, c) in &pairs {
            node.insert(k, PageId::new(c)).expect("insert");
        }
        let got: Vec<(u64, PageId)> = node.iter_entries().collect();
        let mut want: Vec<(u64, PageId)> =
            pairs.iter().map(|&(k, c)| (k, PageId::new(c))).collect();
        want.sort_by_key(|&(k, _)| k);
        assert_eq!(got, want);
    }

    // ------------------------------------------------------------------------
    // Leaf node tests
    // ------------------------------------------------------------------------

    fn tref(page: u64, slot: u16) -> TupleRef {
        TupleRef::new(PageId::new(page), SlotId::new(slot))
    }

    #[test]
    fn leaf_fanout_matches_documented_capacity() {
        assert_eq!(MAX_LEAF_KEYS, 453);
    }

    #[test]
    fn leaf_init_produces_empty() {
        let mut buf = fresh_page();
        let leaf = LeafPage::init(&mut buf, PageId::INVALID);
        assert_eq!(leaf.key_count(), 0);
        assert!(leaf.next_leaf().is_invalid());
        assert!(!leaf.is_full());
    }

    #[test]
    fn leaf_from_bytes_accepts_btree_leaf() {
        let mut buf = fresh_page();
        LeafPage::init(&mut buf, PageId::INVALID);
        assert!(LeafPage::from_bytes(&mut buf).is_ok());
    }

    #[test]
    fn leaf_from_bytes_rejects_non_leaf_page() {
        let mut buf = fresh_page();
        PageHeader::new_heap().write(&mut buf);
        let err = LeafPage::from_bytes(&mut buf).expect_err("must reject");
        assert!(matches!(
            err,
            StorageError::WrongPageType {
                expected: PageType::BTreeLeaf,
                actual: PageType::Heap,
            }
        ));
    }

    #[test]
    fn leaf_insert_then_find_round_trips() {
        let mut buf = fresh_page();
        let mut leaf = LeafPage::init(&mut buf, PageId::INVALID);
        leaf.insert(10, tref(100, 0)).expect("insert 10");
        leaf.insert(20, tref(100, 1)).expect("insert 20");
        leaf.insert(30, tref(100, 2)).expect("insert 30");
        assert_eq!(leaf.find_key(10), Some(tref(100, 0)));
        assert_eq!(leaf.find_key(20), Some(tref(100, 1)));
        assert_eq!(leaf.find_key(30), Some(tref(100, 2)));
        assert_eq!(leaf.find_key(15), None);
        assert_eq!(leaf.find_key(0), None);
        assert_eq!(leaf.find_key(999), None);
    }

    #[test]
    fn leaf_insert_maintains_sorted_order() {
        let mut buf = fresh_page();
        let mut leaf = LeafPage::init(&mut buf, PageId::INVALID);
        let order = [50u64, 10, 30, 20, 40, 5, 60];
        for (i, &k) in order.iter().enumerate() {
            leaf.insert(k, tref(100, u16::try_from(i).unwrap()))
                .expect("insert");
        }
        let keys: Vec<u64> = leaf.iter_keys().collect();
        let mut expected = order.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    #[test]
    fn leaf_rejects_duplicate_key() {
        let mut buf = fresh_page();
        let mut leaf = LeafPage::init(&mut buf, PageId::INVALID);
        leaf.insert(42, tref(100, 0)).expect("first");
        let err = leaf.insert(42, tref(200, 5)).expect_err("duplicate");
        assert!(matches!(err, StorageError::DuplicateBTreeKey(42)));
        assert_eq!(leaf.find_key(42), Some(tref(100, 0)));
    }

    #[test]
    fn leaf_is_full_at_max_keys() {
        let mut buf = fresh_page();
        let mut leaf = LeafPage::init(&mut buf, PageId::INVALID);
        for i in 0..u64::from(MAX_LEAF_KEYS_U16) {
            leaf.insert(i, tref(100, 0)).expect("insert");
        }
        assert!(leaf.is_full());
        let err = leaf.insert(9999, tref(100, 0)).expect_err("must reject");
        assert!(matches!(err, StorageError::BTreeNodeFull { .. }));
    }

    #[test]
    fn leaf_delete_removes_and_returns_tuple_ref() {
        let mut buf = fresh_page();
        let mut leaf = LeafPage::init(&mut buf, PageId::INVALID);
        leaf.insert(10, tref(100, 0)).expect("insert");
        leaf.insert(20, tref(100, 1)).expect("insert");
        leaf.insert(30, tref(100, 2)).expect("insert");
        let removed = leaf.delete(20).expect("delete");
        assert_eq!(removed, tref(100, 1));
        assert_eq!(leaf.key_count(), 2);
        assert_eq!(leaf.find_key(20), None);
        assert_eq!(leaf.find_key(10), Some(tref(100, 0)));
        assert_eq!(leaf.find_key(30), Some(tref(100, 2)));
        let keys: Vec<u64> = leaf.iter_keys().collect();
        assert_eq!(keys, vec![10, 30]);
    }

    #[test]
    fn leaf_delete_missing_key_errors() {
        let mut buf = fresh_page();
        let mut leaf = LeafPage::init(&mut buf, PageId::INVALID);
        leaf.insert(10, tref(100, 0)).expect("insert");
        let err = leaf.delete(99).expect_err("missing");
        assert!(matches!(err, StorageError::BTreeKeyNotFound(99)));
    }

    #[test]
    fn leaf_sibling_pointer_round_trips() {
        let mut buf = fresh_page();
        let mut leaf = LeafPage::init(&mut buf, PageId::INVALID);
        assert!(leaf.next_leaf().is_invalid());
        leaf.set_next_leaf(PageId::new(77));
        assert_eq!(leaf.next_leaf(), PageId::new(77));
        leaf.set_next_leaf(PageId::INVALID);
        assert!(leaf.next_leaf().is_invalid());
    }

    #[test]
    fn leaf_entry_at_round_trips() {
        let mut buf = fresh_page();
        let mut leaf = LeafPage::init(&mut buf, PageId::INVALID);
        leaf.insert(100, tref(11, 1)).expect("insert");
        leaf.insert(200, tref(22, 2)).expect("insert");
        leaf.insert(300, tref(33, 3)).expect("insert");
        assert_eq!(leaf.entry_at(0), (100, tref(11, 1)));
        assert_eq!(leaf.entry_at(1), (200, tref(22, 2)));
        assert_eq!(leaf.entry_at(2), (300, tref(33, 3)));
    }

    #[test]
    fn leaf_init_with_explicit_next() {
        let mut buf = fresh_page();
        let leaf = LeafPage::init(&mut buf, PageId::new(99));
        assert_eq!(leaf.next_leaf(), PageId::new(99));
    }

    #[test]
    fn page_id_invalid_helpers() {
        assert!(PageId::INVALID.is_invalid());
        assert!(!PageId::new(0).is_invalid());
        assert!(!PageId::new(42).is_invalid());
    }
}
