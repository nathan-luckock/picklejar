//! Conflict-free replicated vector index: similarity search that merges cleanly.
//!
//! The conflict-free replicated memory let two partitioned nodes merge key-value
//! state. This does the same for a *similarity index*: each node builds its own
//! approximate-nearest-neighbor set while offline, and when the link returns the
//! two indexes merge into one that every node agrees on, with nearest-neighbor
//! results identical no matter the merge order. Each embedding is a last-write-wins
//! register keyed by id (vector, Lamport timestamp, origin); the merge is the
//! element-wise join, so it is commutative, associative, and idempotent, and a KNN
//! query runs over the converged live set. A replicated index that two
//! disconnected nodes can reconcile without a coordinator is genuinely uncommon.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    /// The embedding, or `None` for a tombstoned (deleted) id.
    vector: Option<Vec<f32>>,
    ts: u64,
    origin: u64,
}

impl Entry {
    fn dominates(&self, other: &Self) -> bool {
        (self.ts, self.origin) > (other.ts, other.origin)
    }
}

fn l2_sq(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = f64::from(*x) - f64::from(*y);
            d * d
        })
        .sum()
}

/// One replica of the vector index.
#[derive(Clone, Debug)]
pub struct CrdtVectorIndex {
    id: u64,
    clock: u64,
    entries: BTreeMap<u64, Entry>,
}

impl CrdtVectorIndex {
    /// A fresh replica with a unique id (used only to break ties).
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self {
            id,
            clock: 0,
            entries: BTreeMap::new(),
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Index (or re-index) an embedding under `mem_id`.
    pub fn insert(&mut self, mem_id: u64, vector: Vec<f32>) {
        let ts = self.tick();
        self.entries.insert(
            mem_id,
            Entry {
                vector: Some(vector),
                ts,
                origin: self.id,
            },
        );
    }

    /// Remove an embedding, leaving a tombstone so the delete merges.
    pub fn remove(&mut self, mem_id: u64) {
        let ts = self.tick();
        self.entries.insert(
            mem_id,
            Entry {
                vector: None,
                ts,
                origin: self.id,
            },
        );
    }

    /// The live embedding for `mem_id`, if any.
    #[must_use]
    pub fn get(&self, mem_id: u64) -> Option<&[f32]> {
        self.entries.get(&mem_id).and_then(|e| e.vector.as_deref())
    }

    /// The number of live (non-tombstoned) embeddings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.values().filter(|e| e.vector.is_some()).count()
    }

    /// Whether the index has no live embeddings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Merge another replica in (element-wise last-write-wins join). Commutative,
    /// associative, and idempotent.
    pub fn merge(&mut self, other: &Self) {
        for (id, their) in &other.entries {
            let keep = match self.entries.get(id) {
                Some(mine) if mine.dominates(their) => continue,
                _ => their.clone(),
            };
            self.entries.insert(*id, keep);
        }
        self.clock = self.clock.max(other.clock);
    }

    /// Whether two replicas have converged to the same state.
    #[must_use]
    pub fn converged_with(&self, other: &Self) -> bool {
        self.entries == other.entries
    }

    /// The `k` nearest live embeddings to `query`, as `(id, squared distance)`,
    /// nearest first. Ties broken by id for a deterministic order.
    #[must_use]
    pub fn knn(&self, query: &[f32], k: usize) -> Vec<(u64, f64)> {
        let mut scored: Vec<(u64, f64)> = self
            .entries
            .iter()
            .filter_map(|(id, e)| e.vector.as_ref().map(|v| (*id, l2_sq(query, v))))
            .collect();
        scored.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        scored.truncate(k);
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(hits: &[(u64, f64)]) -> Vec<u64> {
        hits.iter().map(|(id, _)| *id).collect()
    }

    #[test]
    fn partitioned_inserts_merge_and_knn_sees_the_union() {
        let mut a = CrdtVectorIndex::new(1);
        let mut b = CrdtVectorIndex::new(2);
        a.insert(10, vec![0.0, 0.0]);
        a.insert(11, vec![5.0, 5.0]);
        b.insert(20, vec![0.1, 0.1]);
        a.merge(&b);
        b.merge(&a);
        assert!(a.converged_with(&b));
        // Query near the origin: ids 10 and 20 are closest, from both replicas.
        assert_eq!(ids(&a.knn(&[0.0, 0.0], 2)), vec![10, 20]);
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn knn_is_identical_regardless_of_merge_order() {
        let mut a = CrdtVectorIndex::new(1);
        let mut b = CrdtVectorIndex::new(2);
        for i in 0..20u64 {
            a.insert(i, vec![i as f32, 0.0]);
        }
        for i in 10..30u64 {
            b.insert(i, vec![0.0, i as f32]);
        }
        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b.clone();
        ba.merge(&a);
        assert!(ab.converged_with(&ba));
        let q = [3.0, 3.0];
        assert_eq!(
            ab.knn(&q, 5),
            ba.knn(&q, 5),
            "KNN must not depend on merge order"
        );
    }

    #[test]
    fn a_concurrent_conflict_resolves_the_same_everywhere() {
        let mut a = CrdtVectorIndex::new(1);
        let mut b = CrdtVectorIndex::new(2);
        a.insert(1, vec![1.0, 0.0]);
        b.insert(1, vec![0.0, 1.0]); // same id, concurrent
        let mut x = a.clone();
        x.merge(&b);
        let mut y = b.clone();
        y.merge(&a);
        assert!(x.converged_with(&y));
        // The higher origin (2) wins the tie, so id 1 is b's vector everywhere.
        assert_eq!(x.get(1), Some(&[0.0, 1.0][..]));
    }

    #[test]
    fn a_tombstone_is_excluded_from_knn_and_merges() {
        let mut a = CrdtVectorIndex::new(1);
        a.insert(1, vec![0.0, 0.0]);
        a.insert(2, vec![1.0, 1.0]);
        let mut b = a.clone();
        b.merge(&a);
        b.remove(1); // delete the nearest
        a.merge(&b);
        assert_eq!(a.get(1), None, "the later delete wins");
        assert_eq!(
            ids(&a.knn(&[0.0, 0.0], 2)),
            vec![2],
            "tombstoned id is not a candidate"
        );
    }
}
