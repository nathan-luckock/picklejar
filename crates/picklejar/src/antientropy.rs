//! Merkle anti-entropy: find exactly which memories two replicas disagree on,
//! without shipping either replica's whole set.
//!
//! Two nodes that drifted apart during a partition need to reconcile, but
//! streaming every memory across a thin link to compare them is wasteful when
//! almost all of them already match. This builds a Merkle tree over a fixed
//! partition of the key space: each leaf bucket hashes the memories that fall in
//! it, and internal nodes hash their children, so the root commits to the whole
//! set. To compare, two replicas walk their trees top-down and descend only where
//! node hashes differ. Identical subtrees are skipped after a single hash
//! comparison, so the work is proportional to the number of differences, not the
//! size of the data. This is how Dynamo-style stores keep replicas in sync.

use std::collections::BTreeMap;

use crate::authmem::sha256;

/// Hash a key into 64 bits to spread keys across buckets.
fn key_hash(key: u64) -> u64 {
    let d = sha256::hash(&key.to_be_bytes());
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

fn bucket_hash(entries: &[(u64, [u8; 32])]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(entries.len() * 40);
    for (k, h) in entries {
        buf.extend_from_slice(&k.to_be_bytes());
        buf.extend_from_slice(h);
    }
    sha256::hash(&buf)
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(left);
    buf[32..].copy_from_slice(right);
    sha256::hash(&buf)
}

/// A replica's set of memories, indexed into a fixed-depth Merkle tree over the
/// key space for efficient reconciliation.
#[derive(Clone, Debug)]
pub struct MerkleSet {
    depth: u32,
    /// `2^depth` buckets, each a sorted list of `(key, value_hash)`.
    buckets: Vec<Vec<(u64, [u8; 32])>>,
    /// Tree levels: `levels[0]` are the leaf bucket hashes, the last is the root.
    levels: Vec<Vec<[u8; 32]>>,
}

impl MerkleSet {
    /// An empty set with `2^depth` buckets.
    #[must_use]
    pub fn new(depth: u32) -> Self {
        let buckets = vec![Vec::new(); 1usize << depth];
        let mut set = Self {
            depth,
            buckets,
            levels: Vec::new(),
        };
        set.rebuild();
        set
    }

    /// Build a set from `(key, value)` memories.
    #[must_use]
    pub fn from_entries(depth: u32, entries: &[(u64, Vec<u8>)]) -> Self {
        let mut set = Self::new(depth);
        for (k, v) in entries {
            set.insert(*k, v);
        }
        set
    }

    #[allow(clippy::cast_possible_truncation)] // masked to depth bits, always fits
    fn bucket_of(&self, key: u64) -> usize {
        (key_hash(key) & ((1u64 << self.depth) - 1)) as usize
    }

    /// Insert or overwrite a memory, then refresh the tree.
    pub fn insert(&mut self, key: u64, value: &[u8]) {
        let b = self.bucket_of(key);
        let h = sha256::hash(value);
        let bucket = &mut self.buckets[b];
        match bucket.binary_search_by_key(&key, |(k, _)| *k) {
            Ok(i) => bucket[i].1 = h,
            Err(i) => bucket.insert(i, (key, h)),
        }
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let leaves: Vec<[u8; 32]> = self.buckets.iter().map(|b| bucket_hash(b)).collect();
        let mut levels = vec![leaves];
        while levels.last().expect("non-empty").len() > 1 {
            let prev = levels.last().expect("non-empty");
            let mut up = Vec::with_capacity(prev.len() / 2);
            let mut i = 0;
            while i < prev.len() {
                up.push(node_hash(&prev[i], &prev[i + 1]));
                i += 2;
            }
            levels.push(up);
        }
        self.levels = levels;
    }

    /// The root hash committing to the whole set.
    #[must_use]
    pub fn root(&self) -> [u8; 32] {
        *self
            .levels
            .last()
            .expect("non-empty")
            .last()
            .expect("non-empty")
    }

    /// The keys whose values differ between the two sets (present on one side, or
    /// holding different values), plus the number of tree-node hash comparisons
    /// the reconciliation needed.
    ///
    /// # Panics
    /// Panics if the two sets do not share a tree depth.
    #[must_use]
    pub fn diff(&self, other: &Self) -> (Vec<u64>, usize) {
        assert_eq!(self.depth, other.depth, "sets must share a depth");
        let top = self.levels.len() - 1;
        let mut stack = vec![(top, 0usize)];
        let mut differing_buckets = Vec::new();
        let mut compares = 0usize;

        while let Some((level, index)) = stack.pop() {
            compares += 1;
            if self.levels[level][index] == other.levels[level][index] {
                continue; // identical subtree, skip it entirely
            }
            if level == 0 {
                differing_buckets.push(index);
            } else {
                stack.push((level - 1, index * 2));
                stack.push((level - 1, index * 2 + 1));
            }
        }

        // Within each differing bucket, find the exact keys that disagree.
        let mut keys = Vec::new();
        for &b in &differing_buckets {
            let mine: BTreeMap<u64, [u8; 32]> = self.buckets[b].iter().copied().collect();
            let theirs: BTreeMap<u64, [u8; 32]> = other.buckets[b].iter().copied().collect();
            for (k, hv) in &mine {
                if theirs.get(k) != Some(hv) {
                    keys.push(*k);
                }
            }
            for k in theirs.keys() {
                if !mine.contains_key(k) {
                    keys.push(*k);
                }
            }
        }
        keys.sort_unstable();
        keys.dedup();
        (keys, compares)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Vec<(u64, Vec<u8>)> {
        (0..1000u64)
            .map(|k| (k, format!("memory {k}").into_bytes()))
            .collect()
    }

    #[test]
    fn identical_replicas_have_no_diff_and_cost_one_compare() {
        let a = MerkleSet::from_entries(10, &base());
        let b = MerkleSet::from_entries(10, &base());
        assert_eq!(a.root(), b.root());
        let (keys, compares) = a.diff(&b);
        assert!(keys.is_empty());
        assert_eq!(compares, 1, "matching roots end the walk immediately");
    }

    #[test]
    fn a_changed_value_is_found_cheaply() {
        let a = MerkleSet::from_entries(10, &base());
        let mut b = MerkleSet::from_entries(10, &base());
        b.insert(500, b"a different memory");
        let (keys, compares) = a.diff(&b);
        assert_eq!(keys, vec![500]);
        // One differing leaf out of 1024: only its root-to-leaf path is walked.
        assert!(
            compares <= 3 * a.depth as usize,
            "walk should be path-sized, was {compares}"
        );
    }

    #[test]
    fn keys_present_on_only_one_side_are_found() {
        let a = MerkleSet::from_entries(8, &[(1, b"a".to_vec()), (2, b"b".to_vec())]);
        let b = MerkleSet::from_entries(8, &[(1, b"a".to_vec()), (3, b"c".to_vec())]);
        let (mut keys, _) = a.diff(&b);
        keys.sort_unstable();
        assert_eq!(keys, vec![2, 3], "2 only on a, 3 only on b");
    }

    #[test]
    fn several_differences_are_all_found() {
        let a = MerkleSet::from_entries(10, &base());
        let mut b = MerkleSet::from_entries(10, &base());
        for k in [10, 200, 777, 999] {
            b.insert(k, b"changed".as_slice());
        }
        let (keys, compares) = a.diff(&b);
        assert_eq!(keys, vec![10, 200, 777, 999]);
        assert!(
            compares < 100,
            "still far cheaper than 1024 leaves, was {compares}"
        );
    }
}
