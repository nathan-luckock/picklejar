//! Consistent hashing: shard memories across nodes with minimal reshuffling.
//!
//! Spreading memories over a fleet of storage nodes by `hash(key) mod n` is
//! simple until the fleet changes: adding or losing one node remaps almost every
//! key, a catastrophe when each remap means moving data between unreachable
//! machines. Consistent hashing places both nodes and keys on a hash ring and
//! assigns each key to the next node clockwise. Adding a node only steals the
//! keys in the arc just before it; removing one only spills its keys to its
//! successor. So a membership change moves about `1/n` of the keys, not all of
//! them. Virtual nodes (many ring points per physical node) keep the load even.

use std::collections::BTreeMap;

use crate::authmem::sha256;

fn hash_point(s: &str) -> u64 {
    let d = sha256::hash(s.as_bytes());
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// A consistent-hashing ring over named nodes.
#[derive(Clone, Debug)]
pub struct HashRing {
    vnodes: u32,
    ring: BTreeMap<u64, String>,
}

impl HashRing {
    /// A ring giving each node `vnodes` points for even load.
    #[must_use]
    pub fn new(vnodes: u32) -> Self {
        Self {
            vnodes: vnodes.max(1),
            ring: BTreeMap::new(),
        }
    }

    /// Add a node, scattering its virtual points around the ring.
    pub fn add_node(&mut self, name: &str) {
        for i in 0..self.vnodes {
            self.ring
                .insert(hash_point(&format!("{name}#{i}")), name.to_string());
        }
    }

    /// Remove a node and all its points.
    pub fn remove_node(&mut self, name: &str) {
        self.ring.retain(|_, n| n != name);
    }

    /// The node responsible for `key`: the first node clockwise from the key's
    /// point, wrapping around the ring.
    #[must_use]
    pub fn route(&self, key: &[u8]) -> Option<&str> {
        if self.ring.is_empty() {
            return None;
        }
        let d = sha256::hash(key);
        let h = u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
        self.ring
            .range(h..)
            .next()
            .or_else(|| self.ring.iter().next())
            .map(|(_, n)| n.as_str())
    }

    /// The preference list for `key`: the first `count` distinct physical nodes
    /// clockwise from the key's point, wrapping around the ring. This is the
    /// replica set for a key under replication. Returns fewer than `count` only
    /// when the ring holds fewer than `count` distinct nodes.
    #[must_use]
    pub fn preference(&self, key: &[u8], count: usize) -> Vec<&str> {
        if self.ring.is_empty() || count == 0 {
            return Vec::new();
        }
        let d = sha256::hash(key);
        let h = u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
        let mut out: Vec<&str> = Vec::with_capacity(count);
        for name in self.ring.range(h..).chain(self.ring.iter()).map(|(_, n)| n) {
            if out.len() == count {
                break;
            }
            if !out.contains(&name.as_str()) {
                out.push(name.as_str());
            }
        }
        out
    }

    /// The number of distinct physical nodes on the ring.
    #[must_use]
    pub fn node_count(&self) -> usize {
        let mut names: Vec<&str> = self.ring.values().map(String::as_str).collect();
        names.sort_unstable();
        names.dedup();
        names.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_all(ring: &HashRing, n: u64) -> Vec<String> {
        (0..n)
            .map(|i| ring.route(&i.to_be_bytes()).expect("routed").to_string())
            .collect()
    }

    #[test]
    fn load_is_roughly_even() {
        let mut ring = HashRing::new(200);
        for name in ["a", "b", "c", "d"] {
            ring.add_node(name);
        }
        let routed = route_all(&ring, 40_000);
        for name in ["a", "b", "c", "d"] {
            let share = routed.iter().filter(|r| *r == name).count();
            // Each of four nodes should get roughly a quarter (10k), within 25%.
            assert!(
                (7_500..=12_500).contains(&share),
                "{name} got {share}, expected ~10000"
            );
        }
    }

    #[test]
    fn adding_a_node_moves_about_one_nth_of_keys() {
        let mut ring = HashRing::new(200);
        for name in ["a", "b", "c"] {
            ring.add_node(name);
        }
        let before = route_all(&ring, 30_000);
        ring.add_node("d");
        let after = route_all(&ring, 30_000);

        let moved = before.iter().zip(&after).filter(|(b, a)| b != a).count();
        // Going from 3 to 4 nodes should move about 1/4 of keys; allow a wide band.
        assert!(
            (4_000..=12_000).contains(&moved),
            "moved {moved}, expected ~7500"
        );
        // And every key that moved went to the new node.
        for (b, a) in before.iter().zip(&after) {
            if b != a {
                assert_eq!(a, "d", "a moved key must land on the new node");
            }
        }
    }

    #[test]
    fn removing_a_node_only_spills_its_keys() {
        let mut ring = HashRing::new(200);
        for name in ["a", "b", "c", "d"] {
            ring.add_node(name);
        }
        let before = route_all(&ring, 20_000);
        ring.remove_node("c");
        let after = route_all(&ring, 20_000);
        // Only keys that were on "c" may have moved.
        for (b, a) in before.iter().zip(&after) {
            if b != a {
                assert_eq!(b, "c", "only c's keys should move when c leaves");
            }
        }
        assert_eq!(ring.node_count(), 3);
    }

    #[test]
    fn an_empty_ring_routes_nothing() {
        let ring = HashRing::new(10);
        assert!(ring.route(b"key").is_none());
    }

    #[test]
    fn preference_returns_distinct_replicas_led_by_the_primary() {
        let mut ring = HashRing::new(200);
        for name in ["a", "b", "c", "d"] {
            ring.add_node(name);
        }
        let pref = ring.preference(b"some-key", 3);
        assert_eq!(pref.len(), 3, "three distinct replicas");
        assert_eq!(pref[0], ring.route(b"some-key").unwrap(), "primary leads");
        let mut sorted = pref.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "no duplicate nodes");
    }

    #[test]
    fn preference_caps_at_the_number_of_nodes() {
        let mut ring = HashRing::new(50);
        ring.add_node("only");
        assert_eq!(ring.preference(b"k", 3), vec!["only"]);
    }
}
