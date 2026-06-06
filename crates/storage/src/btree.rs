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

use std::cell::Cell;
use std::ops::Bound;

use crate::buffer::BufferPool;
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

// ============================================================================
// Read-only views (operate on `&Page`)
// ============================================================================

/// Read-only view over an internal node. Used by traversal paths that hold
/// a buffer pool read guard and only need to inspect the page.
#[derive(Debug)]
struct InternalView<'a> {
    buf: &'a Page,
}

impl<'a> InternalView<'a> {
    fn new(buf: &'a Page) -> Result<Self> {
        let header = PageHeader::read(buf)?;
        if header.page_type != PageType::BTreeInternal {
            return Err(StorageError::WrongPageType {
                expected: PageType::BTreeInternal,
                actual: header.page_type,
            });
        }
        Ok(Self { buf })
    }

    fn key_count(&self) -> u16 {
        u16::from_le_bytes(
            self.buf[KEY_COUNT_OFFSET..KEY_COUNT_OFFSET + 2]
                .try_into()
                .expect("2 bytes"),
        )
    }

    fn first_child(&self) -> PageId {
        let raw = u64::from_le_bytes(
            self.buf[FIRST_CHILD_OFFSET..FIRST_CHILD_OFFSET + 8]
                .try_into()
                .expect("8 bytes"),
        );
        PageId::new(raw)
    }

    fn entry_at(&self, index: u16) -> (u64, PageId) {
        let off = ENTRIES_OFFSET + (index as usize) * INTERNAL_ENTRY_SIZE;
        let key = u64::from_le_bytes(self.buf[off..off + 8].try_into().expect("8 bytes"));
        let child = u64::from_le_bytes(self.buf[off + 8..off + 16].try_into().expect("8 bytes"));
        (key, PageId::new(child))
    }

    fn find_child(&self, query_key: u64) -> PageId {
        let count = self.key_count();
        if count == 0 {
            return self.first_child();
        }
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
        if lo == 0 {
            self.first_child()
        } else {
            self.entry_at(lo - 1).1
        }
    }
}

/// Read-only view over a leaf node.
#[derive(Debug)]
struct LeafView<'a> {
    buf: &'a Page,
}

impl<'a> LeafView<'a> {
    fn new(buf: &'a Page) -> Result<Self> {
        let header = PageHeader::read(buf)?;
        if header.page_type != PageType::BTreeLeaf {
            return Err(StorageError::WrongPageType {
                expected: PageType::BTreeLeaf,
                actual: header.page_type,
            });
        }
        Ok(Self { buf })
    }

    fn key_count(&self) -> u16 {
        u16::from_le_bytes(
            self.buf[LEAF_KEY_COUNT_OFFSET..LEAF_KEY_COUNT_OFFSET + 2]
                .try_into()
                .expect("2 bytes"),
        )
    }

    fn next_leaf(&self) -> PageId {
        let raw = u64::from_le_bytes(
            self.buf[LEAF_NEXT_LEAF_OFFSET..LEAF_NEXT_LEAF_OFFSET + 8]
                .try_into()
                .expect("8 bytes"),
        );
        PageId::new(raw)
    }

    fn entry_at(&self, index: u16) -> (u64, TupleRef) {
        let off = LEAF_ENTRIES_OFFSET + (index as usize) * LEAF_ENTRY_SIZE;
        let key = u64::from_le_bytes(self.buf[off..off + 8].try_into().expect("8 bytes"));
        let page = u64::from_le_bytes(self.buf[off + 8..off + 16].try_into().expect("8 bytes"));
        let slot = u16::from_le_bytes(self.buf[off + 16..off + 18].try_into().expect("2 bytes"));
        (key, TupleRef::new(PageId::new(page), SlotId::new(slot)))
    }

    fn find_key(&self, key: u64) -> Option<TupleRef> {
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

    /// Position of the first entry whose key is >= `lo_key`. Returns
    /// `key_count` when every key is strictly less than `lo_key`.
    fn lower_bound(&self, lo_key: u64) -> u16 {
        let count = self.key_count();
        let mut lo: u16 = 0;
        let mut hi: u16 = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (k, _) = self.entry_at(mid);
            if k < lo_key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

// ============================================================================
// BTree: insert / search / range_scan
// ============================================================================

/// A B+ tree index over a [`BufferPool`].
///
/// The tree's root page may change across `insert` calls when the current
/// root splits, so the root id lives in a `Cell` and `BTree` borrows the
/// pool immutably.
pub struct BTree<'pool> {
    pool: &'pool BufferPool,
    root: Cell<PageId>,
}

impl<'pool> BTree<'pool> {
    /// Create a new empty B+ tree. Allocates one leaf page as the initial
    /// root and returns a handle.
    pub fn create(pool: &'pool BufferPool) -> Result<Self> {
        let (root_id, mut guard) = pool.new_page()?;
        LeafPage::init(guard.page_mut(), PageId::INVALID);
        Ok(Self {
            pool,
            root: Cell::new(root_id),
        })
    }

    /// Open an existing tree whose root is `root`.
    #[must_use]
    pub const fn open(pool: &'pool BufferPool, root: PageId) -> Self {
        Self {
            pool,
            root: Cell::new(root),
        }
    }

    /// Current root page id. Changes when the root splits.
    #[must_use]
    pub fn root_page(&self) -> PageId {
        self.root.get()
    }

    /// Look up `key`. Returns `None` if the key is not in the tree.
    pub fn search(&self, key: u64) -> Result<Option<TupleRef>> {
        let mut current = self.root.get();
        loop {
            let guard = self.pool.fetch_page(current)?;
            let header = PageHeader::read(guard.page())?;
            match header.page_type {
                PageType::BTreeLeaf => {
                    let leaf = LeafView::new(guard.page())?;
                    return Ok(leaf.find_key(key));
                }
                PageType::BTreeInternal => {
                    let internal = InternalView::new(guard.page())?;
                    current = internal.find_child(key);
                }
                other => {
                    return Err(StorageError::WrongPageType {
                        expected: PageType::BTreeLeaf,
                        actual: other,
                    });
                }
            }
        }
    }

    /// Insert `(key, tuple)`. Splits and propagates as needed; allocates a
    /// new root when the existing root splits.
    pub fn insert(&self, key: u64, tuple: TupleRef) -> Result<()> {
        let root = self.root.get();
        let split = self.insert_recursive(root, key, tuple)?;
        if let Some((promoted_key, new_right_child)) = split {
            // The root split. Allocate a new root that points at the old
            // root (now the left child) and the new sibling (right child),
            // with the promoted key as the only separator.
            let (new_root_id, mut guard) = self.pool.new_page()?;
            let mut new_root = InternalPage::init(guard.page_mut(), root);
            new_root.insert(promoted_key, new_right_child)?;
            drop(guard);
            self.root.set(new_root_id);
        }
        Ok(())
    }

    /// Walk to the right child for `key`, recurse, and insert any promoted
    /// separator into this node on the way back up. Returns `Some((key,
    /// new_right_child))` when this node split.
    fn insert_recursive(
        &self,
        page_id: PageId,
        key: u64,
        tuple: TupleRef,
    ) -> Result<Option<(u64, PageId)>> {
        // Peek at the page type with a read guard first.
        let page_type = {
            let guard = self.pool.fetch_page(page_id)?;
            PageHeader::read(guard.page())?.page_type
        };

        match page_type {
            PageType::BTreeLeaf => self.insert_into_leaf(page_id, key, tuple),
            PageType::BTreeInternal => {
                // Find the child to descend into using a read view.
                let child_id = {
                    let guard = self.pool.fetch_page(page_id)?;
                    let internal = InternalView::new(guard.page())?;
                    internal.find_child(key)
                };
                let result = self.insert_recursive(child_id, key, tuple)?;
                if let Some((promoted_key, new_right_child)) = result {
                    self.insert_into_internal(page_id, promoted_key, new_right_child)
                } else {
                    Ok(None)
                }
            }
            other => Err(StorageError::WrongPageType {
                expected: PageType::BTreeLeaf,
                actual: other,
            }),
        }
    }

    /// Insert into a leaf. Splits when full.
    fn insert_into_leaf(
        &self,
        page_id: PageId,
        key: u64,
        tuple: TupleRef,
    ) -> Result<Option<(u64, PageId)>> {
        // First try the easy path: room in the leaf.
        {
            let mut guard = self.pool.fetch_page_mut(page_id)?;
            let mut leaf = LeafPage::from_bytes(guard.page_mut())?;
            match leaf.insert(key, tuple) {
                Ok(()) => return Ok(None),
                Err(StorageError::BTreeNodeFull { .. }) => {
                    // Fall through to split below.
                }
                Err(e) => return Err(e),
            }
        } // drop guard before allocating

        self.split_leaf_and_insert(page_id, key, tuple).map(Some)
    }

    fn split_leaf_and_insert(
        &self,
        old_id: PageId,
        insert_key: u64,
        insert_tuple: TupleRef,
    ) -> Result<(u64, PageId)> {
        // Allocate the new (right) sibling first. new_page returns a write
        // guard, but we drop it immediately so we can re-fetch in a more
        // controlled order below.
        let (new_id, new_guard) = self.pool.new_page()?;
        drop(new_guard);

        // Snapshot the old leaf's data with a read guard.
        let (mut entries, old_next) = {
            let guard = self.pool.fetch_page(old_id)?;
            let view = LeafView::new(guard.page())?;
            let entries: Vec<(u64, TupleRef)> =
                (0..view.key_count()).map(|i| view.entry_at(i)).collect();
            (entries, view.next_leaf())
        };

        // Splice the new entry in, rejecting duplicates.
        let pos = entries.partition_point(|(k, _)| *k < insert_key);
        if pos < entries.len() && entries[pos].0 == insert_key {
            return Err(StorageError::DuplicateBTreeKey(insert_key));
        }
        entries.insert(pos, (insert_key, insert_tuple));

        let mid = entries.len() / 2;
        let split_key = entries[mid].0;

        // Initialize the new leaf with the right half, pointing at the
        // old leaf's former next_leaf.
        {
            let mut new_guard = self.pool.fetch_page_mut(new_id)?;
            LeafPage::init(new_guard.page_mut(), old_next);
            let mut new_leaf = LeafPage::from_bytes(new_guard.page_mut())?;
            for &(k, t) in &entries[mid..] {
                new_leaf.insert(k, t)?;
            }
        }

        // Rewrite the old leaf with the left half. Reset key_count first
        // so insert can rebuild from scratch; set next_leaf to new_id.
        {
            let mut old_guard = self.pool.fetch_page_mut(old_id)?;
            let mut old_leaf = LeafPage::from_bytes(old_guard.page_mut())?;
            old_leaf.set_key_count(0);
            old_leaf.set_next_leaf(new_id);
            for &(k, t) in &entries[..mid] {
                old_leaf.insert(k, t)?;
            }
        }

        Ok((split_key, new_id))
    }

    /// Insert into an internal node. Splits when full.
    fn insert_into_internal(
        &self,
        page_id: PageId,
        key: u64,
        right_child: PageId,
    ) -> Result<Option<(u64, PageId)>> {
        // Easy path first.
        {
            let mut guard = self.pool.fetch_page_mut(page_id)?;
            let mut internal = InternalPage::from_bytes(guard.page_mut())?;
            match internal.insert(key, right_child) {
                Ok(()) => return Ok(None),
                Err(StorageError::BTreeNodeFull { .. }) => {}
                Err(e) => return Err(e),
            }
        }

        self.split_internal_and_insert(page_id, key, right_child)
            .map(Some)
    }

    fn split_internal_and_insert(
        &self,
        old_id: PageId,
        insert_key: u64,
        insert_right_child: PageId,
    ) -> Result<(u64, PageId)> {
        let (new_id, new_guard) = self.pool.new_page()?;
        drop(new_guard);

        // Snapshot the old internal node.
        let (mut entries, old_first_child) = {
            let guard = self.pool.fetch_page(old_id)?;
            let view = InternalView::new(guard.page())?;
            let entries: Vec<(u64, PageId)> =
                (0..view.key_count()).map(|i| view.entry_at(i)).collect();
            (entries, view.first_child())
        };

        // Splice in the new separator.
        let pos = entries.partition_point(|(k, _)| *k < insert_key);
        if pos < entries.len() && entries[pos].0 == insert_key {
            return Err(StorageError::DuplicateBTreeKey(insert_key));
        }
        entries.insert(pos, (insert_key, insert_right_child));

        // Promote the middle key. Left keeps entries[..mid], right gets
        // first_child = entries[mid].right_child and entries[mid+1..].
        let mid = entries.len() / 2;
        let promoted_key = entries[mid].0;
        let new_first_child = entries[mid].1;
        let left_entries = entries[..mid].to_vec();
        let right_entries = entries[mid + 1..].to_vec();

        // Initialize new internal node.
        {
            let mut new_guard = self.pool.fetch_page_mut(new_id)?;
            InternalPage::init(new_guard.page_mut(), new_first_child);
            let mut new_internal = InternalPage::from_bytes(new_guard.page_mut())?;
            for &(k, c) in &right_entries {
                new_internal.insert(k, c)?;
            }
        }

        // Rewrite old internal node with the left half.
        {
            let mut old_guard = self.pool.fetch_page_mut(old_id)?;
            // Re-init preserves the first_child slot; we use the old one.
            InternalPage::init(old_guard.page_mut(), old_first_child);
            let mut old_internal = InternalPage::from_bytes(old_guard.page_mut())?;
            for &(k, c) in &left_entries {
                old_internal.insert(k, c)?;
            }
        }

        Ok((promoted_key, new_id))
    }

    /// Iterate over entries with keys in `[lo, hi)` (or whatever the
    /// `Bound`s specify). The iterator is lazy: each `next()` may fetch
    /// the next sibling leaf via the buffer pool.
    pub fn range_scan(&self, lo: Bound<u64>, hi: Bound<u64>) -> Result<RangeScan<'pool>> {
        // Walk to the leaf that *might* contain the lower bound.
        let start_key = match lo {
            Bound::Included(k) | Bound::Excluded(k) => k,
            Bound::Unbounded => 0,
        };
        let mut current = self.root.get();
        loop {
            let guard = self.pool.fetch_page(current)?;
            let header = PageHeader::read(guard.page())?;
            match header.page_type {
                PageType::BTreeLeaf => break,
                PageType::BTreeInternal => {
                    let internal = InternalView::new(guard.page())?;
                    current = internal.find_child(start_key);
                }
                other => {
                    return Err(StorageError::WrongPageType {
                        expected: PageType::BTreeLeaf,
                        actual: other,
                    });
                }
            }
        }

        // Position within the start leaf.
        let start_index = {
            let guard = self.pool.fetch_page(current)?;
            let view = LeafView::new(guard.page())?;
            let mut idx = view.lower_bound(start_key);
            if let Bound::Excluded(k) = lo {
                if idx < view.key_count() {
                    let (existing, _) = view.entry_at(idx);
                    if existing == k {
                        idx += 1;
                    }
                }
            }
            idx
        };

        Ok(RangeScan {
            pool: self.pool,
            current_leaf: current,
            current_index: start_index,
            hi,
            done: false,
        })
    }
}

impl std::fmt::Debug for BTree<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BTree")
            .field("root", &self.root.get())
            .finish()
    }
}

/// Lending-style iterator over a B+ tree range scan. Each `next` may fault
/// a leaf page through the buffer pool, so items are `Result`s.
pub struct RangeScan<'pool> {
    pool: &'pool BufferPool,
    current_leaf: PageId,
    current_index: u16,
    hi: Bound<u64>,
    done: bool,
}

impl Iterator for RangeScan<'_> {
    type Item = Result<(u64, TupleRef)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            if self.current_leaf.is_invalid() {
                self.done = true;
                return None;
            }
            let guard = match self.pool.fetch_page(self.current_leaf) {
                Ok(g) => g,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            let view = match LeafView::new(guard.page()) {
                Ok(v) => v,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            if self.current_index >= view.key_count() {
                self.current_leaf = view.next_leaf();
                self.current_index = 0;
                continue;
            }
            let (k, t) = view.entry_at(self.current_index);
            let in_upper = match self.hi {
                Bound::Included(hi) => k <= hi,
                Bound::Excluded(hi) => k < hi,
                Bound::Unbounded => true,
            };
            if !in_upper {
                self.done = true;
                return None;
            }
            self.current_index += 1;
            return Some(Ok((k, t)));
        }
    }
}

impl std::fmt::Debug for RangeScan<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RangeScan")
            .field("current_leaf", &self.current_leaf)
            .field("current_index", &self.current_index)
            .field("done", &self.done)
            .finish()
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

    // ------------------------------------------------------------------------
    // BTree ops tests (insert / search / range_scan with split propagation)
    // ------------------------------------------------------------------------

    use crate::buffer::BufferPool;
    use crate::file::FileManager;
    use tempfile::TempDir;

    fn fresh_tree(pool_size: usize) -> (TempDir, BufferPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = FileManager::open(dir.path().join("tree.db")).expect("open");
        let pool = BufferPool::new(file, pool_size);
        (dir, pool)
    }

    fn tr(slot: u16) -> TupleRef {
        TupleRef::new(PageId::new(1), SlotId::new(slot))
    }

    #[test]
    fn btree_create_makes_single_leaf_root() {
        let (_dir, pool) = fresh_tree(8);
        let tree = BTree::create(&pool).expect("create");
        // Root is a leaf with no entries.
        let g = pool.fetch_page(tree.root_page()).expect("read root");
        let header = PageHeader::read(g.page()).expect("hdr");
        assert_eq!(header.page_type, PageType::BTreeLeaf);
        let view = LeafView::new(g.page()).expect("leaf view");
        assert_eq!(view.key_count(), 0);
    }

    #[test]
    fn btree_search_on_empty_returns_none() {
        let (_dir, pool) = fresh_tree(8);
        let tree = BTree::create(&pool).expect("create");
        assert_eq!(tree.search(42).expect("search"), None);
    }

    #[test]
    fn btree_insert_then_search_small() {
        let (_dir, pool) = fresh_tree(8);
        let tree = BTree::create(&pool).expect("create");
        for (i, k) in [10u64, 20, 30, 5, 15].iter().enumerate() {
            tree.insert(*k, tr(u16::try_from(i).unwrap()))
                .expect("insert");
        }
        for (i, k) in [10u64, 20, 30, 5, 15].iter().enumerate() {
            assert_eq!(
                tree.search(*k).expect("search"),
                Some(tr(u16::try_from(i).unwrap())),
            );
        }
        assert_eq!(tree.search(99).expect("search"), None);
    }

    #[test]
    fn btree_insert_rejects_duplicate() {
        let (_dir, pool) = fresh_tree(8);
        let tree = BTree::create(&pool).expect("create");
        tree.insert(42, tr(0)).expect("first");
        let err = tree.insert(42, tr(1)).expect_err("dup");
        assert!(matches!(err, StorageError::DuplicateBTreeKey(42)));
        assert_eq!(tree.search(42).expect("search"), Some(tr(0)));
    }

    #[test]
    fn btree_leaf_split_propagates_to_new_root() {
        // Insert enough keys to force at least one leaf split. After the
        // first split, root becomes an internal node.
        let (_dir, pool) = fresh_tree(16);
        let tree = BTree::create(&pool).expect("create");
        let n = u64::from(MAX_LEAF_KEYS_U16) + 1;
        for i in 0..n {
            tree.insert(i, tr(0)).expect("insert");
        }
        // Now root should be internal.
        let g = pool.fetch_page(tree.root_page()).expect("read root");
        let header = PageHeader::read(g.page()).expect("hdr");
        assert_eq!(header.page_type, PageType::BTreeInternal);
        drop(g);
        // All keys still findable.
        for i in 0..n {
            assert_eq!(tree.search(i).expect("search"), Some(tr(0)));
        }
        // Keys above the inserted range still miss.
        assert_eq!(tree.search(n + 100).expect("search"), None);
    }

    #[test]
    fn btree_handles_bulk_insert_then_search() {
        // Cross the threshold for an internal split too. Two full leaves
        // plus the root keep things shallow; once we cross ~MAX_INTERNAL
        // separator keys we'd split the internal too. Use a moderate
        // workload to exercise the split path without blowing test time.
        let (_dir, pool) = fresh_tree(64);
        let tree = BTree::create(&pool).expect("create");
        // Use a deterministic but scrambled order.
        let keys: Vec<u64> = (0..2000u64).map(|i| (i * 1009 + 7) % 2003).collect();
        // Dedupe (modulo can collide for prime mismatches, but 1009 vs 2003 are coprime).
        let mut seen = std::collections::HashSet::new();
        for k in &keys {
            if seen.insert(*k) {
                tree.insert(*k, tr((*k % 65535) as u16)).expect("insert");
            }
        }
        // Every inserted key must be findable.
        for k in &keys {
            if seen.contains(k) {
                let want = Some(tr((*k % 65535) as u16));
                assert_eq!(tree.search(*k).expect("search"), want, "key {k}");
            }
        }
    }

    #[test]
    fn btree_range_scan_inclusive_within_single_leaf() {
        let (_dir, pool) = fresh_tree(8);
        let tree = BTree::create(&pool).expect("create");
        for i in 0u64..10 {
            tree.insert(i, tr(u16::try_from(i).unwrap()))
                .expect("insert");
        }
        let scan = tree
            .range_scan(Bound::Included(3), Bound::Included(7))
            .expect("scan");
        let got: Vec<u64> = scan.map(|r| r.unwrap().0).collect();
        assert_eq!(got, vec![3, 4, 5, 6, 7]);
    }

    #[test]
    fn btree_range_scan_excluded_bounds() {
        let (_dir, pool) = fresh_tree(8);
        let tree = BTree::create(&pool).expect("create");
        for i in 0u64..10 {
            tree.insert(i, tr(0)).expect("insert");
        }
        let got: Vec<u64> = tree
            .range_scan(Bound::Excluded(3), Bound::Excluded(7))
            .expect("scan")
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(got, vec![4, 5, 6]);
    }

    #[test]
    fn btree_range_scan_unbounded_returns_everything() {
        let (_dir, pool) = fresh_tree(8);
        let tree = BTree::create(&pool).expect("create");
        for i in 0u64..20 {
            tree.insert(i, tr(0)).expect("insert");
        }
        let got: Vec<u64> = tree
            .range_scan(Bound::Unbounded, Bound::Unbounded)
            .expect("scan")
            .map(|r| r.unwrap().0)
            .collect();
        let expected: Vec<u64> = (0u64..20).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn btree_range_scan_crosses_leaf_boundary() {
        // Insert enough keys to force a leaf split, then range-scan across
        // the boundary.
        let (_dir, pool) = fresh_tree(16);
        let tree = BTree::create(&pool).expect("create");
        let n = u64::from(MAX_LEAF_KEYS_U16) + 1;
        for i in 0..n {
            tree.insert(i, tr(0)).expect("insert");
        }
        // Scan a window that straddles the split point.
        let mid = n / 2;
        let got: Vec<u64> = tree
            .range_scan(Bound::Included(mid - 5), Bound::Included(mid + 5))
            .expect("scan")
            .map(|r| r.unwrap().0)
            .collect();
        let expected: Vec<u64> = (mid - 5..=mid + 5).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn btree_range_scan_empty_result() {
        let (_dir, pool) = fresh_tree(8);
        let tree = BTree::create(&pool).expect("create");
        tree.insert(10, tr(0)).expect("insert");
        tree.insert(20, tr(0)).expect("insert");
        let got: Vec<u64> = tree
            .range_scan(Bound::Included(30), Bound::Included(40))
            .expect("scan")
            .map(|r| r.unwrap().0)
            .collect();
        assert!(got.is_empty());
    }

    #[test]
    fn btree_open_existing_root() {
        // Build a tree, close, re-open with the saved root.
        let (_dir, pool) = fresh_tree(16);
        let root_id;
        {
            let tree = BTree::create(&pool).expect("create");
            for i in 0u64..50 {
                tree.insert(i, tr(u16::try_from(i).unwrap()))
                    .expect("insert");
            }
            root_id = tree.root_page();
        }
        pool.flush_all().expect("flush");
        let tree = BTree::open(&pool, root_id);
        assert_eq!(tree.search(7).expect("search"), Some(tr(7)));
        assert_eq!(tree.search(42).expect("search"), Some(tr(42)));
    }
}
