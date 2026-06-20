//! Rendezvous (highest-random-weight) hashing: pick a node with no ring state.
//!
//! Like consistent hashing, rendezvous hashing shards memories across nodes so
//! that a membership change moves only a small fraction of keys. It gets there
//! differently and arguably more simply: for a given key, every node computes a
//! score from `hash(node, key)`, and the key goes to the highest-scoring node.
//! There is no ring to maintain, weights fall out naturally (a node twice as
//! capable gets twice the keys), and when a node leaves, only the keys for which
//! it was the winner move, each to its own second choice. The cost is scoring
//! every node per lookup, which is fine for a modest fleet.

use crate::authmem::sha256;

fn score(name: &str, weight: f64, key: &[u8]) -> f64 {
    let mut buf = Vec::with_capacity(name.len() + 1 + key.len());
    buf.extend_from_slice(name.as_bytes());
    buf.push(0xff);
    buf.extend_from_slice(key);
    let d = sha256::hash(&buf);
    let h = u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
    // Map the hash to a uniform value in (0, 1), then use the weighted
    // rendezvous score -weight / ln(u), which a higher weight increases.
    #[allow(clippy::cast_precision_loss)]
    let u = ((h >> 11) as f64 + 1.0) / ((1u64 << 53) as f64 + 1.0);
    -weight / u.ln()
}

/// A rendezvous-hashing node set.
#[derive(Clone, Debug, Default)]
pub struct Rendezvous {
    nodes: Vec<(String, f64)>,
}

impl Rendezvous {
    /// An empty node set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node with a relative `weight` (1.0 is the baseline).
    pub fn add_node(&mut self, name: &str, weight: f64) {
        self.nodes.retain(|(n, _)| n != name);
        self.nodes.push((name.to_string(), weight));
    }

    /// Remove a node.
    pub fn remove_node(&mut self, name: &str) {
        self.nodes.retain(|(n, _)| n != name);
    }

    /// The node responsible for `key`: the highest-scoring one.
    #[must_use]
    pub fn route(&self, key: &[u8]) -> Option<&str> {
        self.nodes
            .iter()
            .max_by(|a, b| score(&a.0, a.1, key).total_cmp(&score(&b.0, b.1, key)))
            .map(|(n, _)| n.as_str())
    }

    /// The number of nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether there are no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_all(r: &Rendezvous, n: u64) -> Vec<String> {
        (0..n)
            .map(|i| r.route(&i.to_be_bytes()).expect("routed").to_string())
            .collect()
    }

    #[test]
    fn equal_weights_spread_evenly() {
        let mut r = Rendezvous::new();
        for name in ["a", "b", "c", "d"] {
            r.add_node(name, 1.0);
        }
        let routed = route_all(&r, 40_000);
        for name in ["a", "b", "c", "d"] {
            let share = routed.iter().filter(|x| *x == name).count();
            assert!(
                (8_500..=11_500).contains(&share),
                "{name} got {share}, expected ~10000"
            );
        }
    }

    #[test]
    fn weight_shifts_load_proportionally() {
        let mut r = Rendezvous::new();
        r.add_node("big", 2.0);
        r.add_node("s1", 1.0);
        r.add_node("s2", 1.0);
        r.add_node("s3", 1.0);
        let routed = route_all(&r, 50_000);
        let big = routed.iter().filter(|x| *x == "big").count();
        // big has 2 of 5 total weight -> ~40% of keys.
        assert!(
            (17_000..=23_000).contains(&big),
            "big got {big}, expected ~20000"
        );
    }

    #[test]
    fn removing_a_node_only_moves_its_own_keys() {
        let mut r = Rendezvous::new();
        for name in ["a", "b", "c", "d"] {
            r.add_node(name, 1.0);
        }
        let before = route_all(&r, 20_000);
        r.remove_node("b");
        let after = route_all(&r, 20_000);
        for (bf, af) in before.iter().zip(&after) {
            if bf != af {
                assert_eq!(bf, "b", "only b's keys should move");
            }
        }
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn an_empty_set_routes_nothing() {
        assert!(Rendezvous::new().route(b"k").is_none());
    }
}
