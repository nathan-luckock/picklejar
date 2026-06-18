//! In-memory HNSW index for approximate nearest-neighbor search.
//!
//! HNSW (Hierarchical Navigable Small World, Malkov and Yashunin 2016) is the
//! index that makes vector search fast at scale: a multi-layer proximity graph
//! where search greedily descends from a sparse top layer to a dense base layer,
//! turning a linear scan into something close to logarithmic.
//!
//! This is the index structure on its own: it builds from a set of embeddings
//! and answers top-k queries. Wiring it into the planner so that
//! `ORDER BY embedding <-> :q LIMIT k` uses it (while preserving the row-level
//! security filter, which must be applied before the top-k is taken) is the next
//! step; the brute-force path stays the correctness baseline this index is
//! checked against.
//!
//! # Determinism
//!
//! Node levels are assigned from a seeded [`Rng`], so a given sequence of inserts
//! builds the identical graph every time. That keeps the index reproducible for
//! testing and for the deterministic simulator, exactly like the rest of the
//! engine.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};

/// `SplitMix64`: the same small deterministic PRNG used elsewhere in the engine.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform `f64` in the half-open range `(0, 1]`.
    fn unit(&mut self) -> f64 {
        // 53 mantissa bits give a value in [0, 1); shift to (0, 1] so ln() is finite.
        let bits = self.next_u64() >> 11;
        #[allow(clippy::cast_precision_loss)]
        let frac = bits as f64 / (1u64 << 53) as f64;
        1.0 - frac
    }
}

/// A candidate node paired with its distance to the query, ordered by distance.
/// Ordering uses `f32::total_cmp`, so it is total even with NaN present.
#[derive(Clone, Copy)]
struct Scored {
    dist: f32,
    id: usize,
}

impl PartialEq for Scored {
    fn eq(&self, other: &Self) -> bool {
        self.dist.total_cmp(&other.dist) == std::cmp::Ordering::Equal
    }
}
impl Eq for Scored {}
impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Scored {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.dist.total_cmp(&other.dist)
    }
}

/// One node: its embedding and its neighbor lists, one per graph layer it
/// belongs to (index 0 is the dense base layer).
struct Node {
    vector: Vec<f32>,
    neighbors: Vec<Vec<usize>>,
}

impl Node {
    fn level(&self) -> usize {
        self.neighbors.len() - 1
    }
}

/// An in-memory HNSW index over fixed-dimension embeddings.
pub struct Hnsw {
    dim: usize,
    /// Max neighbors per node on layers above the base.
    m: usize,
    /// Max neighbors per node on the base layer (denser, conventionally `2*m`).
    m0: usize,
    /// Beam width while building.
    ef_construction: usize,
    /// Level-generation scale, `1 / ln(m)`.
    ml: f64,
    rng: Rng,
    nodes: Vec<Node>,
    entry: Option<usize>,
    max_level: usize,
}

impl std::fmt::Debug for Hnsw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hnsw")
            .field("dim", &self.dim)
            .field("m", &self.m)
            .field("len", &self.nodes.len())
            .field("max_level", &self.max_level)
            .finish_non_exhaustive()
    }
}

/// Squared Euclidean distance. Monotonic with L2, so it ranks identically while
/// avoiding a square root in the inner loop.
fn dist_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

impl Hnsw {
    /// Create an empty index over `dim`-dimensional vectors. `m` is the graph
    /// degree (16 is a good default); `ef_construction` is the build-time beam
    /// width (a larger value builds a better graph more slowly). `seed` makes
    /// level assignment reproducible.
    #[must_use]
    pub fn new(dim: usize, m: usize, ef_construction: usize, seed: u64) -> Self {
        let m = m.max(2);
        // `m` is a small graph degree; the precision of its f64 image is exact.
        #[allow(clippy::cast_precision_loss)]
        let ml = 1.0 / (m as f64).ln();
        Self {
            dim,
            m,
            m0: m * 2,
            ef_construction: ef_construction.max(m),
            ml,
            rng: Rng::new(seed),
            nodes: Vec::new(),
            entry: None,
            max_level: 0,
        }
    }

    /// Number of indexed vectors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the index holds no vectors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// A random level from the exponential distribution HNSW uses, so most nodes
    /// land on layer 0 and a thinning few reach higher layers.
    fn random_level(&mut self) -> usize {
        let r = self.rng.unit();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let level = (-r.ln() * self.ml).floor() as usize;
        level
    }

    /// Insert one embedding, returning the id assigned to it (its insertion
    /// index). The vector's length must equal the index dimension.
    ///
    /// # Panics
    ///
    /// Panics if `vector.len()` does not equal the index's dimension.
    pub fn insert(&mut self, vector: Vec<f32>) -> usize {
        assert_eq!(vector.len(), self.dim, "vector dimension mismatch");
        let level = self.random_level();
        let id = self.nodes.len();
        self.nodes.push(Node {
            vector,
            neighbors: vec![Vec::new(); level + 1],
        });

        let Some(entry) = self.entry else {
            // First node: it becomes the entry point.
            self.entry = Some(id);
            self.max_level = level;
            return id;
        };

        let query = self.nodes[id].vector.clone();
        // Descend from the top down to just above the new node's top layer.
        let mut cur = self.greedy_descent(&query, self.max_level, level, entry);

        // Connect at each layer from the node's top down to the base.
        let top = level.min(self.max_level);
        for layer in (0..=top).rev() {
            let found = self.search_layer(&query, &[cur], self.ef_construction, layer);
            let max = if layer == 0 { self.m0 } else { self.m };
            for cand in found.iter().take(self.m) {
                self.connect(id, cand.id, layer, max);
            }
            if let Some(nearest) = found.first() {
                cur = nearest.id;
            }
        }

        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(id);
        }
        id
    }

    /// The `k` approximate nearest neighbors of `query`, nearest first, as
    /// `(id, distance)` pairs where the distance is true L2 (not squared).
    /// `ef_search` is the query-time beam width (at least `k`); larger trades
    /// speed for recall.
    #[must_use]
    pub fn search(&self, query: &[f32], k: usize, ef_search: usize) -> Vec<(usize, f32)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if k == 0 {
            return Vec::new();
        }
        let cur = self.greedy_descent(query, self.max_level, 0, entry);
        let ef = ef_search.max(k);
        let mut found = self.search_layer(query, &[cur], ef, 0);
        found.truncate(k);
        found
            .into_iter()
            .map(|s| (s.id, s.dist.max(0.0).sqrt()))
            .collect()
    }

    /// Greedily walk from `from_level` down to just above `to_level`, hopping to
    /// a strictly closer neighbor until none improves, one layer at a time.
    fn greedy_descent(
        &self,
        query: &[f32],
        from_level: usize,
        to_level: usize,
        entry: usize,
    ) -> usize {
        let mut cur = entry;
        let mut cur_d = dist_sq(query, &self.nodes[cur].vector);
        let mut layer = from_level;
        while layer > to_level {
            loop {
                let mut improved = false;
                if layer <= self.nodes[cur].level() {
                    for &nb in &self.nodes[cur].neighbors[layer] {
                        let d = dist_sq(query, &self.nodes[nb].vector);
                        if d < cur_d {
                            cur_d = d;
                            cur = nb;
                            improved = true;
                        }
                    }
                }
                if !improved {
                    break;
                }
            }
            layer -= 1;
        }
        cur
    }

    /// The standard HNSW layer search: a best-first expansion from `entry_points`
    /// that keeps the `ef` closest nodes seen, returned sorted nearest-first.
    fn search_layer(
        &self,
        query: &[f32],
        entry_points: &[usize],
        ef: usize,
        layer: usize,
    ) -> Vec<Scored> {
        let mut visited: HashSet<usize> = HashSet::new();
        // Min-heap of frontier to explore, max-heap of the best `ef` found.
        let mut frontier: BinaryHeap<Reverse<Scored>> = BinaryHeap::new();
        let mut best: BinaryHeap<Scored> = BinaryHeap::new();
        for &ep in entry_points {
            let d = dist_sq(query, &self.nodes[ep].vector);
            let s = Scored { dist: d, id: ep };
            visited.insert(ep);
            frontier.push(Reverse(s));
            best.push(s);
        }
        while let Some(Reverse(c)) = frontier.pop() {
            // If the nearest frontier node is farther than our current worst kept
            // and we already hold `ef`, no closer node can be found.
            if let Some(worst) = best.peek() {
                if c.dist > worst.dist && best.len() >= ef {
                    break;
                }
            }
            if layer > self.nodes[c.id].level() {
                continue;
            }
            for &nb in &self.nodes[c.id].neighbors[layer] {
                if !visited.insert(nb) {
                    continue;
                }
                let d = dist_sq(query, &self.nodes[nb].vector);
                let worst = best.peek().map_or(f32::INFINITY, |s| s.dist);
                if best.len() < ef || d < worst {
                    let s = Scored { dist: d, id: nb };
                    frontier.push(Reverse(s));
                    best.push(s);
                    if best.len() > ef {
                        best.pop();
                    }
                }
            }
        }
        let mut out: Vec<Scored> = best.into_vec();
        out.sort_unstable();
        out
    }

    /// Add an undirected edge between `a` and `b` at `layer`, then prune both
    /// endpoints back to `max` neighbors (keeping the closest) so node degree
    /// stays bounded.
    fn connect(&mut self, a: usize, b: usize, layer: usize, max: usize) {
        if a == b {
            return;
        }
        self.nodes[a].neighbors[layer].push(b);
        self.nodes[b].neighbors[layer].push(a);
        self.prune(a, layer, max);
        self.prune(b, layer, max);
    }

    /// Trim node `id`'s neighbor list at `layer` to its `max` closest neighbors.
    fn prune(&mut self, id: usize, layer: usize, max: usize) {
        if self.nodes[id].neighbors[layer].len() <= max {
            return;
        }
        let v = self.nodes[id].vector.clone();
        let mut scored: Vec<Scored> = self.nodes[id].neighbors[layer]
            .iter()
            .map(|&nb| Scored {
                dist: dist_sq(&v, &self.nodes[nb].vector),
                id: nb,
            })
            .collect();
        scored.sort_unstable();
        scored.truncate(max);
        self.nodes[id].neighbors[layer] = scored.into_iter().map(|s| s.id).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic vector generator for tests (its own `SplitMix` stream).
    fn gen_vectors(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut rng = Rng::new(seed);
        (0..n)
            .map(|_| {
                (0..dim)
                    .map(|_| {
                        let raw = i16::try_from(rng.next_u64() % 2001).unwrap_or(0) - 1000;
                        f32::from(raw)
                    })
                    .collect()
            })
            .collect()
    }

    /// Exact top-k by brute force, ids nearest first.
    fn brute_force(data: &[Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
        let mut scored: Vec<(f32, usize)> = data
            .iter()
            .enumerate()
            .map(|(i, v)| (dist_sq(query, v), i))
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored.into_iter().take(k).map(|(_, i)| i).collect()
    }

    #[test]
    fn empty_index_returns_nothing() {
        let index = Hnsw::new(4, 16, 100, 1);
        assert!(index.is_empty());
        assert!(index.search(&[0.0; 4], 5, 50).is_empty());
    }

    #[test]
    fn finds_the_exact_nearest_on_a_small_set() {
        let dim = 8;
        let data = gen_vectors(200, dim, 42);
        let mut index = Hnsw::new(dim, 16, 100, 7);
        for v in &data {
            index.insert(v.clone());
        }
        assert_eq!(index.len(), data.len());
        // For several queries, the single nearest neighbor must be exact.
        for q in 0..20 {
            let query = &data[q * 7 % data.len()];
            let got = index.search(query, 1, 50);
            let exact = brute_force(&data, query, 1);
            assert_eq!(got[0].0, exact[0], "nearest neighbor should be exact");
            // The query vector is in the set, so its nearest distance is 0.
            assert!(got[0].1.abs() < 1e-6);
        }
    }

    #[test]
    fn recall_at_ten_is_high() {
        let dim = 16;
        let data = gen_vectors(1000, dim, 123);
        let mut index = Hnsw::new(dim, 16, 200, 99);
        for v in &data {
            index.insert(v.clone());
        }
        let mut hits = 0usize;
        let mut total = 0usize;
        let queries = gen_vectors(50, dim, 555);
        for query in &queries {
            let approx: HashSet<usize> = index
                .search(query, 10, 100)
                .into_iter()
                .map(|(i, _)| i)
                .collect();
            let exact = brute_force(&data, query, 10);
            for id in exact {
                total += 1;
                if approx.contains(&id) {
                    hits += 1;
                }
            }
        }
        // Approximate, but a good graph recovers the large majority of the true
        // top-10 on random data.
        let recall =
            f64::from(u32::try_from(hits).unwrap()) / f64::from(u32::try_from(total).unwrap());
        assert!(recall > 0.90, "recall@10 was {recall:.3}, expected > 0.90");
    }

    #[test]
    fn building_is_deterministic_for_a_seed() {
        let dim = 8;
        let data = gen_vectors(300, dim, 17);
        let build = || {
            let mut index = Hnsw::new(dim, 16, 100, 4);
            for v in &data {
                index.insert(v.clone());
            }
            index.search(&data[0], 10, 64)
        };
        assert_eq!(build(), build());
    }
}
