//! Secondary indexes.
//!
//! A secondary index is a B+ tree, one per indexed column, mapping the column
//! value to the rowid of the row that holds it. It turns an equality lookup
//! (`WHERE id = 42`) into a point get instead of a full table scan.
//!
//! # Correctness under MVCC
//!
//! The index is maintained by upsert only, never delete. On an insert, or on
//! an update that changes the value, the engine upserts `key(value) -> rowid`.
//! Rows that are deleted, and the old values left behind by updates, are not
//! removed from the tree. This is deliberate:
//!
//! - Every lookup is verified. The engine resolves the candidate rowid through
//!   the MVCC primary index ([`MvccTable::get`](picklejar_txn::MvccTable::get)),
//!   which enforces the transaction's snapshot, and the executor re-applies the
//!   predicate as a residual filter. A stale entry therefore produces a
//!   candidate that is filtered out, never a wrong row.
//! - Because nothing is ever removed, an aborted transaction leaves extra
//!   entries in the tree but never deletes one a concurrent reader still needs.
//!   Visibility stays correct with no index rollback.
//!
//! The cost is that the tree accumulates dead entries over a table's lifetime
//! (index bloat), which a periodic rebuild would reclaim. Only columns with a
//! UNIQUE or PRIMARY KEY constraint are indexed, so at most one live row holds
//! a given key and the unique-keyed B+ tree never sees a genuine duplicate.

use std::ops::Bound;

use picklejar_sql::Value;
use picklejar_storage::{BTree, BufferPool, PageId, SlotId, TupleRef};

use crate::error::Result;

/// Map an indexable value to an order-preserving `u64` B+ tree key, or `None`
/// for a type that is not indexed.
///
/// The indexed types are the ones with a bijective, order-preserving map into
/// `u64`, so distinct values never collide (which is what lets the unique-keyed
/// B+ tree serve as the index) and the key order matches the value order (which
/// is what lets a range predicate drive a [range scan](Index::range_lookup)):
///
/// - `INT`, `DATE`, and `TIMESTAMP` are all `i64`-backed. Reinterpreting the
///   bits and flipping the sign bit maps signed order onto unsigned order.
/// - `BOOL` maps to `0` / `1`.
///
/// `FLOAT` is excluded because `NaN` has no total order (a row holding it could
/// not be found by a range scan); `TEXT` and `DECIMAL` do not fit `u64`
/// bijectively, so they need duplicate-key support and are left to a follow-up.
#[must_use]
pub const fn index_key(value: &Value) -> Option<u64> {
    match value {
        // Reinterpret the bits (no sign loss) and flip the sign bit, so the
        // unsigned key order matches the signed value order. DATE and TIMESTAMP
        // are stored as an `i64` epoch offset, so they share the transform.
        Value::Int(n) | Value::Date(n) | Value::Timestamp(n) => {
            Some(u64::from_ne_bytes(n.to_ne_bytes()) ^ (1 << 63))
        }
        Value::Bool(b) => Some(*b as u64),
        _ => None,
    }
}

/// Map a `Value` range bound to a `u64` key bound. A non-indexable bound value
/// widens to `Unbounded` (the residual filter still removes the extra rows), so
/// the worst case is a wider scan, never a wrong answer.
fn key_bound(bound: Bound<&Value>) -> Bound<u64> {
    match bound {
        Bound::Included(v) => index_key(v).map_or(Bound::Unbounded, Bound::Included),
        Bound::Excluded(v) => index_key(v).map_or(Bound::Unbounded, Bound::Excluded),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// A secondary index: a thin typed wrapper over a B+ tree storing
/// `value -> rowid`.
pub struct Index<'pool> {
    tree: BTree<'pool>,
}

impl std::fmt::Debug for Index<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("root", &self.tree.root_page())
            .finish()
    }
}

impl<'pool> Index<'pool> {
    /// Create a new empty index and return its handle.
    pub fn create(pool: &'pool BufferPool) -> Result<Self> {
        Ok(Self {
            tree: BTree::create(pool)?,
        })
    }

    /// Open an existing index rooted at `root`.
    #[must_use]
    pub const fn open(pool: &'pool BufferPool, root: PageId) -> Self {
        Self {
            tree: BTree::open(pool, root),
        }
    }

    /// The current root page id (it can move as the tree grows).
    #[must_use]
    pub fn root(&self) -> PageId {
        self.tree.root_page()
    }

    /// Record that `rowid` holds `value`. Returns `false` (a no-op) for a value
    /// whose type is not indexed. Uses upsert, so re-assigning a value
    /// overwrites any stale entry for the same key.
    pub fn put(&self, value: &Value, rowid: u64) -> Result<bool> {
        let Some(key) = index_key(value) else {
            return Ok(false);
        };
        self.tree
            .upsert(key, TupleRef::new(PageId::new(rowid), SlotId::new(0)))?;
        Ok(true)
    }

    /// Look up the rowid recorded for `value`, if any. Returns `None` for a
    /// non-indexable value or an absent key.
    pub fn lookup(&self, value: &Value) -> Result<Option<u64>> {
        let Some(key) = index_key(value) else {
            return Ok(None);
        };
        Ok(self.tree.search(key)?.map(|t| t.page_id.get()))
    }

    /// Collect the candidate rowids whose recorded value falls in the range
    /// `[lo, hi]` (per the bounds). The order-preserving key map means a value
    /// range is a contiguous key range, so this is one B+ tree range scan.
    ///
    /// The result may contain duplicates and stale entries (the index is
    /// upsert-only): a row updated `5 -> 7 -> 5` leaves both keys pointing at it.
    /// The caller resolves each rowid through MVCC and re-applies the predicate,
    /// so callers that must not double-count a row should dedup the rowids.
    pub fn range_lookup(&self, lo: Bound<&Value>, hi: Bound<&Value>) -> Result<Vec<u64>> {
        let scan = self.tree.range_scan(key_bound(lo), key_bound(hi))?;
        let mut rowids = Vec::new();
        for item in scan {
            let (_key, tuple) = item?;
            rowids.push(tuple.page_id.get());
        }
        Ok(rowids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use picklejar_storage::{BufferPool, FileManager};

    fn pool() -> (tempfile::TempDir, BufferPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = FileManager::open(dir.path().join("idx.db")).expect("open");
        (dir, BufferPool::new(file, 32))
    }

    #[test]
    fn index_key_covers_fixed_types_and_preserves_order() {
        // Non-bijective / unordered types are not indexed.
        assert_eq!(index_key(&Value::Text("x".into())), None);
        assert_eq!(index_key(&Value::Null), None);
        assert_eq!(index_key(&Value::Float(1.5)), None);
        // Order-preserving: negative sorts below zero sorts below positive.
        let neg = index_key(&Value::Int(-5)).unwrap();
        let zero = index_key(&Value::Int(0)).unwrap();
        let pos = index_key(&Value::Int(5)).unwrap();
        assert!(neg < zero && zero < pos);
        // DATE / TIMESTAMP share the i64 transform, so they order the same way.
        let early = index_key(&Value::Date(-1)).unwrap();
        let late = index_key(&Value::Date(100)).unwrap();
        assert!(early < late);
        assert_eq!(index_key(&Value::Timestamp(7)), index_key(&Value::Int(7)));
        // BOOL maps false below true.
        assert!(index_key(&Value::Bool(false)).unwrap() < index_key(&Value::Bool(true)).unwrap());
    }

    #[test]
    fn put_and_lookup_round_trip() {
        let (_d, pool) = pool();
        let idx = Index::create(&pool).unwrap();
        assert!(idx.put(&Value::Int(42), 7).unwrap());
        assert_eq!(idx.lookup(&Value::Int(42)).unwrap(), Some(7));
        assert_eq!(idx.lookup(&Value::Int(43)).unwrap(), None);
    }

    #[test]
    fn put_overwrites_a_stale_entry() {
        let (_d, pool) = pool();
        let idx = Index::create(&pool).unwrap();
        idx.put(&Value::Int(1), 100).unwrap();
        // Re-assigning the same value (the old holder moved away) overwrites
        // the rowid rather than failing on a duplicate key. This is what keeps
        // the unique-keyed B+ tree usable as the index under updates.
        idx.put(&Value::Int(1), 200).unwrap();
        assert_eq!(idx.lookup(&Value::Int(1)).unwrap(), Some(200));
    }

    #[test]
    fn non_indexable_values_are_skipped() {
        let (_d, pool) = pool();
        let idx = Index::create(&pool).unwrap();
        assert!(!idx.put(&Value::Text("x".into()), 1).unwrap());
        assert_eq!(idx.lookup(&Value::Text("x".into())).unwrap(), None);
    }
}
