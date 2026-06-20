//! Skip list: an ordered key-value index with expected logarithmic operations.
//!
//! The engine's B+ tree is the workhorse ordered index, but a skip list is a
//! second, very different way to keep keys sorted with fast search, insert, and
//! delete. Each entry is linked at several levels: the bottom level is a plain
//! sorted list, and each higher level skips over more entries, like express lanes
//! over a local road. A search rides the high lanes until it overshoots, drops a
//! level, and repeats, so it touches only about log(n) entries. The level of a
//! new entry is chosen by coin flips, which keeps the structure balanced in
//! expectation with no rotations or rebalancing logic at all.
//!
//! This implementation is index-based over a node arena, so it needs no unsafe
//! code, which the crate forbids.

const MAX_LEVEL: usize = 16;
const HEAD: usize = 0;

#[derive(Debug)]
struct Node {
    key: u64,
    value: Vec<u8>,
    forward: Vec<Option<usize>>,
}

/// An ordered map from `u64` keys to byte-string values.
#[derive(Debug)]
pub struct SkipList {
    nodes: Vec<Node>,
    level: usize,
    rng: u64,
    len: usize,
}

impl Default for SkipList {
    fn default() -> Self {
        Self::new(0xC0FF_EE12_3456_789A)
    }
}

impl SkipList {
    /// A new empty skip list, seeded for reproducible level choices.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let head = Node {
            key: 0,
            value: Vec::new(),
            forward: vec![None; MAX_LEVEL],
        };
        Self {
            nodes: vec![head],
            level: 1,
            rng: seed | 1,
            len: 0,
        }
    }

    fn coin(&mut self) -> bool {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x & 1 == 1
    }

    fn random_level(&mut self) -> usize {
        let mut lvl = 1;
        while lvl < MAX_LEVEL && self.coin() {
            lvl += 1;
        }
        lvl
    }

    /// The number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the list is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Fill `update[i]` with the last node at level `i` whose key is `< key`.
    fn predecessors(&self, key: u64) -> [usize; MAX_LEVEL] {
        let mut update = [HEAD; MAX_LEVEL];
        let mut x = HEAD;
        for i in (0..self.level).rev() {
            while let Some(nx) = self.nodes[x].forward[i] {
                if self.nodes[nx].key < key {
                    x = nx;
                } else {
                    break;
                }
            }
            update[i] = x;
        }
        update
    }

    /// Insert or overwrite `key` with `value`.
    pub fn insert(&mut self, key: u64, value: Vec<u8>) {
        let update = self.predecessors(key);
        if let Some(nx) = self.nodes[update[0]].forward[0] {
            if self.nodes[nx].key == key {
                self.nodes[nx].value = value;
                return;
            }
        }
        let lvl = self.random_level();
        let new_idx = self.nodes.len();
        let mut forward = vec![None; lvl];
        for (i, slot) in forward.iter_mut().enumerate() {
            let pred = if i < self.level { update[i] } else { HEAD };
            *slot = self.nodes[pred].forward[i];
            self.nodes[pred].forward[i] = Some(new_idx);
        }
        if lvl > self.level {
            self.level = lvl;
        }
        self.nodes.push(Node {
            key,
            value,
            forward,
        });
        self.len += 1;
    }

    /// Get the value for `key`.
    #[must_use]
    pub fn get(&self, key: u64) -> Option<&[u8]> {
        let mut x = HEAD;
        for i in (0..self.level).rev() {
            while let Some(nx) = self.nodes[x].forward[i] {
                if self.nodes[nx].key < key {
                    x = nx;
                } else {
                    break;
                }
            }
        }
        match self.nodes[x].forward[0] {
            Some(nx) if self.nodes[nx].key == key => Some(&self.nodes[nx].value),
            _ => None,
        }
    }

    /// Remove `key`, returning whether it was present.
    pub fn remove(&mut self, key: u64) -> bool {
        let update = self.predecessors(key);
        let Some(target) = self.nodes[update[0]].forward[0] else {
            return false;
        };
        if self.nodes[target].key != key {
            return false;
        }
        let tlevel = self.nodes[target].forward.len();
        for (i, &pred) in update.iter().enumerate().take(tlevel) {
            if self.nodes[pred].forward[i] == Some(target) {
                let next = self.nodes[target].forward[i];
                self.nodes[pred].forward[i] = next;
            }
        }
        self.len -= 1;
        true
    }

    /// All entries in ascending key order.
    #[must_use]
    pub fn entries(&self) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::with_capacity(self.len);
        let mut x = self.nodes[HEAD].forward[0];
        while let Some(idx) = x {
            out.push((self.nodes[idx].key, self.nodes[idx].value.clone()));
            x = self.nodes[idx].forward[0];
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get() {
        let mut s = SkipList::new(1);
        s.insert(5, b"five".to_vec());
        s.insert(2, b"two".to_vec());
        s.insert(9, b"nine".to_vec());
        assert_eq!(s.get(5), Some(&b"five"[..]));
        assert_eq!(s.get(2), Some(&b"two"[..]));
        assert_eq!(s.get(7), None);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn overwrite_replaces_value() {
        let mut s = SkipList::new(2);
        s.insert(1, b"a".to_vec());
        s.insert(1, b"b".to_vec());
        assert_eq!(s.get(1), Some(&b"b"[..]));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn entries_come_out_sorted() {
        let mut s = SkipList::new(7);
        let mut rng = 0x1234_5678u64;
        let mut inserted = Vec::new();
        for _ in 0..1000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let k = rng % 5000;
            s.insert(k, k.to_be_bytes().to_vec());
            inserted.push(k);
        }
        let keys: Vec<u64> = s.entries().iter().map(|(k, _)| *k).collect();
        // Sorted and unique.
        for w in keys.windows(2) {
            assert!(w[0] < w[1], "keys must be strictly ascending");
        }
        // Every distinct inserted key is present.
        inserted.sort_unstable();
        inserted.dedup();
        assert_eq!(keys.len(), inserted.len());
    }

    #[test]
    fn remove_works() {
        let mut s = SkipList::new(3);
        for i in 0..100u64 {
            s.insert(i, vec![]);
        }
        assert!(s.remove(50));
        assert_eq!(s.get(50), None);
        assert!(!s.remove(50), "second remove is a no-op");
        assert_eq!(s.len(), 99);
        // The rest survive and stay ordered.
        let keys: Vec<u64> = s.entries().iter().map(|(k, _)| *k).collect();
        assert_eq!(keys.len(), 99);
        assert!(!keys.contains(&50));
    }
}
