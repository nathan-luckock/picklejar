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
    /// The similarity metric the graph navigates by.
    metric: Metric,
    rng: Rng,
    nodes: Vec<Node>,
    /// Tombstones, one per node: a removed node stays in the graph for
    /// connectivity but is never returned from a search.
    tombstones: Vec<bool>,
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

/// The similarity metric the graph navigates by, matching the SQL operators:
/// `L2` is `<->`, `Cosine` is `<=>`, `InnerProduct` is `<#>`, `L1` is `<+>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    /// Euclidean distance.
    L2,
    /// Cosine distance, `1 - cosine similarity`.
    Cosine,
    /// Negative inner product (so smaller still means nearer).
    InnerProduct,
    /// L1 (Manhattan / taxicab) distance.
    L1,
}

impl Metric {
    /// The serialized tag for this metric.
    const fn tag(self) -> u8 {
        match self {
            Self::L2 => 0,
            Self::Cosine => 1,
            Self::InnerProduct => 2,
            Self::L1 => 3,
        }
    }

    /// Inverse of [`tag`](Self::tag).
    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::L2),
            1 => Some(Self::Cosine),
            2 => Some(Self::InnerProduct),
            3 => Some(Self::L1),
            _ => None,
        }
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

/// Inner (dot) product.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Cosine distance, `1 - cosine similarity`, with a zero vector defined as
/// distance 0 from another zero and 1 from any real vector (so it never yields
/// NaN).
fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let na = dot(a, a).sqrt();
    let nb = dot(b, b).sqrt();
    let denom = na * nb;
    if denom == 0.0 {
        if na == 0.0 && nb == 0.0 {
            0.0
        } else {
            1.0
        }
    } else {
        1.0 - dot(a, b) / denom
    }
}

/// The graph-ranking score for `metric`: smaller always means nearer, whichever
/// metric is in use, so the search machinery is metric-agnostic.
fn rank(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        Metric::L2 => dist_sq(a, b),
        Metric::Cosine => cosine_distance(a, b),
        Metric::InnerProduct => -dot(a, b),
        Metric::L1 => a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum(),
    }
}

/// Convert a graph-ranking score back to the user-facing distance: the true L2
/// distance for `L2` (the rank is squared), the score itself otherwise (cosine,
/// negative inner product, and L1 are already in their final units).
fn present(metric: Metric, score: f32) -> f32 {
    match metric {
        Metric::L2 => score.max(0.0).sqrt(),
        Metric::Cosine | Metric::InnerProduct | Metric::L1 => score,
    }
}

impl Hnsw {
    /// Create an empty index over `dim`-dimensional vectors. `m` is the graph
    /// degree (16 is a good default); `ef_construction` is the build-time beam
    /// width (a larger value builds a better graph more slowly). `seed` makes
    /// level assignment reproducible.
    #[must_use]
    pub fn new(dim: usize, m: usize, ef_construction: usize, seed: u64) -> Self {
        Self::new_with_metric(dim, m, ef_construction, seed, Metric::L2)
    }

    /// Like [`new`](Self::new) but choosing the similarity metric the graph
    /// navigates by (L2, cosine, or inner product), matching the SQL operators.
    #[must_use]
    pub fn new_with_metric(
        dim: usize,
        m: usize,
        ef_construction: usize,
        seed: u64,
        metric: Metric,
    ) -> Self {
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
            metric,
            rng: Rng::new(seed),
            nodes: Vec::new(),
            tombstones: Vec::new(),
            entry: None,
            max_level: 0,
        }
    }

    /// Number of live (non-removed) indexed vectors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len() - self.tombstones.iter().filter(|&&t| t).count()
    }

    /// Whether the index holds no live vectors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether node `id` has been removed.
    fn is_deleted(&self, id: usize) -> bool {
        self.tombstones.get(id).copied().unwrap_or(false)
    }

    /// Remove the vector with id `id` (its insertion index): it is tombstoned, so
    /// it stays in the graph as a routing waypoint but is never returned from a
    /// search again. Returns `false` if `id` is unknown or already removed. The
    /// graph is not restructured, matching how production HNSW handles deletes.
    pub fn remove(&mut self, id: usize) -> bool {
        if id < self.tombstones.len() && !self.tombstones[id] {
            self.tombstones[id] = true;
            true
        } else {
            false
        }
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
        self.tombstones.push(false);

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
            for nb in self.select_neighbors_heuristic(&found, self.m) {
                self.connect(id, nb, layer, max);
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
    /// `(id, distance)` pairs. The distance is in the index's metric: true L2 for
    /// `L2`, cosine distance for `Cosine`, negative inner product for
    /// `InnerProduct`. `ef_search` is the query-time beam width (at least `k`);
    /// larger trades speed for recall.
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
            .map(|s| (s.id, present(self.metric, s.dist)))
            .collect()
    }

    /// Serialize the whole graph to a self-contained byte buffer that
    /// [`from_bytes`](Self::from_bytes) restores exactly. This lets the index
    /// survive a restart, the same as the rest of the durable memory layer,
    /// rather than being rebuilt from scratch on every open.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"PJHN");
        out.extend_from_slice(&4u32.to_le_bytes());
        out.push(self.metric.tag());
        for field in [
            self.dim as u64,
            self.m as u64,
            self.m0 as u64,
            self.ef_construction as u64,
            self.ml.to_bits(),
            self.rng.0,
            self.max_level as u64,
            self.entry.map_or(u64::MAX, |e| e as u64),
            self.nodes.len() as u64,
        ] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        for node in &self.nodes {
            out.extend_from_slice(&(node.neighbors.len() as u64).to_le_bytes());
            for &x in &node.vector {
                out.extend_from_slice(&x.to_le_bytes());
            }
            for layer in &node.neighbors {
                out.extend_from_slice(&(layer.len() as u64).to_le_bytes());
                for &id in layer {
                    out.extend_from_slice(&(id as u64).to_le_bytes());
                }
            }
        }
        // One tombstone byte per node, trailing the node section.
        for &t in &self.tombstones {
            out.push(u8::from(t));
        }
        // A trailing CRC32 over the whole image. A bit flip anywhere (radiation,
        // silent corruption) changes the checksum, so a corrupted index is
        // detected on load and never silently trusted.
        let checksum = picklejar_storage::crc32::crc32(&out);
        out.extend_from_slice(&checksum.to_le_bytes());
        out
    }

    /// Restore an index from [`to_bytes`](Self::to_bytes) output. Returns `None`
    /// if the bytes are not a version-4 index image, are truncated, or fail their
    /// CRC32 check, so a corrupt sidecar is detected and rejected rather than
    /// trusted.
    #[must_use]
    #[allow(clippy::similar_names)] // the read_u32 / read_u64 / read_f32 readers are deliberately parallel
    pub fn from_bytes(full: &[u8]) -> Option<Self> {
        // Verify the trailing CRC32 before parsing anything: this is the line
        // that makes "never load a silently corrupted index" true.
        let split = full.len().checked_sub(4)?;
        let stored = u32::from_le_bytes(full.get(split..)?.try_into().ok()?);
        if picklejar_storage::crc32::crc32(&full[..split]) != stored {
            return None;
        }
        let bytes = &full[..split];
        if bytes.get(0..4)? != b"PJHN" {
            return None;
        }
        let mut p = 4usize;
        let read_u32 = |p: &mut usize| -> Option<u32> {
            let b = bytes.get(*p..*p + 4)?;
            *p += 4;
            Some(u32::from_le_bytes(b.try_into().ok()?))
        };
        let read_u64 = |p: &mut usize| -> Option<u64> {
            let b = bytes.get(*p..*p + 8)?;
            *p += 8;
            Some(u64::from_le_bytes(b.try_into().ok()?))
        };
        let read_f32 = |p: &mut usize| -> Option<f32> {
            let b = bytes.get(*p..*p + 4)?;
            *p += 4;
            Some(f32::from_le_bytes(b.try_into().ok()?))
        };
        if read_u32(&mut p)? != 4 {
            return None;
        }
        let metric = Metric::from_tag(*bytes.get(p)?)?;
        p += 1;
        let dim = usize::try_from(read_u64(&mut p)?).ok()?;
        let m = usize::try_from(read_u64(&mut p)?).ok()?;
        let m0 = usize::try_from(read_u64(&mut p)?).ok()?;
        let ef_construction = usize::try_from(read_u64(&mut p)?).ok()?;
        let ml = f64::from_bits(read_u64(&mut p)?);
        let rng = Rng(read_u64(&mut p)?);
        let max_level = usize::try_from(read_u64(&mut p)?).ok()?;
        let entry_raw = read_u64(&mut p)?;
        let entry = if entry_raw == u64::MAX {
            None
        } else {
            Some(usize::try_from(entry_raw).ok()?)
        };
        let node_count = usize::try_from(read_u64(&mut p)?).ok()?;
        let mut nodes = Vec::new();
        for _ in 0..node_count {
            let layers = usize::try_from(read_u64(&mut p)?).ok()?;
            if layers == 0 {
                return None;
            }
            let mut vector = Vec::new();
            for _ in 0..dim {
                vector.push(read_f32(&mut p)?);
            }
            let mut neighbors = Vec::new();
            for _ in 0..layers {
                let count = usize::try_from(read_u64(&mut p)?).ok()?;
                let mut layer = Vec::new();
                for _ in 0..count {
                    layer.push(usize::try_from(read_u64(&mut p)?).ok()?);
                }
                neighbors.push(layer);
            }
            nodes.push(Node { vector, neighbors });
        }
        // One tombstone byte per node.
        let mut tombstones = Vec::new();
        for _ in 0..node_count {
            tombstones.push(*bytes.get(p)? != 0);
            p += 1;
        }
        Some(Self {
            dim,
            m,
            m0,
            ef_construction,
            ml,
            metric,
            rng,
            nodes,
            tombstones,
            entry,
            max_level,
        })
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
        let mut cur_d = rank(self.metric, query, &self.nodes[cur].vector);
        let mut layer = from_level;
        while layer > to_level {
            loop {
                let mut improved = false;
                if layer <= self.nodes[cur].level() {
                    for &nb in &self.nodes[cur].neighbors[layer] {
                        let d = rank(self.metric, query, &self.nodes[nb].vector);
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
            let d = rank(self.metric, query, &self.nodes[ep].vector);
            let s = Scored { dist: d, id: ep };
            visited.insert(ep);
            // A tombstoned node still routes the search but is never a result.
            frontier.push(Reverse(s));
            if !self.is_deleted(ep) {
                best.push(s);
            }
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
                let d = rank(self.metric, query, &self.nodes[nb].vector);
                let worst = best.peek().map_or(f32::INFINITY, |s| s.dist);
                // Traverse toward any promising node; keep only live ones as
                // results, so a removed node still bridges the graph.
                if best.len() < ef || d < worst {
                    let s = Scored { dist: d, id: nb };
                    frontier.push(Reverse(s));
                    if !self.is_deleted(nb) {
                        best.push(s);
                        if best.len() > ef {
                            best.pop();
                        }
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

    /// HNSW heuristic neighbor selection (Malkov and Yashunin, Algorithm 4): from
    /// `candidates` sorted nearest-first by their distance to the base node, keep
    /// up to `m` that are *diverse*. A candidate is dropped if it is closer to an
    /// already-kept neighbor than it is to the base node, which spreads a node's
    /// edges out instead of pointing them all into one tight cluster. This is what
    /// keeps the graph navigable through dense or near-duplicate data, where
    /// "keep the m closest" collapses recall. Each candidate's `dist` already
    /// holds its distance to the base node, so the base vector is not needed.
    fn select_neighbors_heuristic(&self, candidates: &[Scored], m: usize) -> Vec<usize> {
        let mut kept: Vec<usize> = Vec::new();
        for c in candidates {
            if kept.len() >= m {
                break;
            }
            let diverse = kept.iter().all(|&r| {
                c.dist < rank(self.metric, &self.nodes[c.id].vector, &self.nodes[r].vector)
            });
            if diverse {
                kept.push(c.id);
            }
        }
        kept
    }

    /// Trim node `id`'s neighbor list at `layer` back to `max`, using the
    /// diversity heuristic rather than simply keeping the closest.
    fn prune(&mut self, id: usize, layer: usize, max: usize) {
        if self.nodes[id].neighbors[layer].len() <= max {
            return;
        }
        let v = self.nodes[id].vector.clone();
        let mut scored: Vec<Scored> = self.nodes[id].neighbors[layer]
            .iter()
            .map(|&nb| Scored {
                dist: rank(self.metric, &v, &self.nodes[nb].vector),
                id: nb,
            })
            .collect();
        scored.sort_unstable();
        self.nodes[id].neighbors[layer] = self.select_neighbors_heuristic(&scored, max);
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

    // --- realistic and adversarial distributions, the cases where ANN fails ---

    /// A random integer-valued component in `[-1000, 1000]`.
    fn comp(rng: &mut Rng) -> f32 {
        f32::from(i16::try_from(rng.next_u64() % 2001).unwrap_or(0) - 1000)
    }

    /// `n` points drawn from `clusters` Gaussian-ish clusters: a random center per
    /// cluster, each point the center plus small uniform jitter. Overlapping
    /// clusters are where a graph index starts to miss neighbors.
    fn clustered_vectors(n: usize, dim: usize, clusters: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut rng = Rng::new(seed);
        let centers: Vec<Vec<f32>> = (0..clusters)
            .map(|_| (0..dim).map(|_| comp(&mut rng)).collect())
            .collect();
        (0..n)
            .map(|_| {
                let c = usize::try_from(rng.next_u64() % clusters as u64).unwrap_or(0);
                centers[c]
                    .iter()
                    .map(|&x| x + f32::from(i16::try_from(rng.next_u64() % 21).unwrap_or(0) - 10))
                    .collect()
            })
            .collect()
    }

    /// `n` points clustered tightly around a few base vectors: the near-duplicate
    /// case, where the k-th neighbor distance is tiny and many candidates tie.
    fn near_duplicate_vectors(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut rng = Rng::new(seed);
        let bases: Vec<Vec<f32>> = (0..8)
            .map(|_| (0..dim).map(|_| comp(&mut rng)).collect())
            .collect();
        (0..n)
            .map(|_| {
                let b = usize::try_from(rng.next_u64() % 8).unwrap_or(0);
                bases[b]
                    .iter()
                    .map(|&x| x + f32::from(i16::try_from(rng.next_u64() % 5).unwrap_or(0) - 2))
                    .collect()
            })
            .collect()
    }

    /// `n` unit-norm vectors, the natural input for cosine similarity.
    fn normalized_vectors(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut rng = Rng::new(seed);
        (0..n)
            .map(|_| {
                let v: Vec<f32> = (0..dim).map(|_| comp(&mut rng)).collect();
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm == 0.0 {
                    v
                } else {
                    v.iter().map(|x| x / norm).collect()
                }
            })
            .collect()
    }

    /// Build an index over `data` and measure recall@k against the exact
    /// brute-force oracle over `queries`: the fraction of true neighbors the
    /// index returns. Exact brute force is a sound oracle for an approximate
    /// index's recall, which is how this answers the oracle problem.
    fn measure_recall(
        data: &[Vec<f32>],
        queries: &[Vec<f32>],
        k: usize,
        ef: usize,
        metric: Metric,
        seed: u64,
    ) -> f64 {
        let dim = data[0].len();
        let mut index = Hnsw::new_with_metric(dim, 16, 200, seed, metric);
        for v in data {
            index.insert(v.clone());
        }
        let mut hits = 0usize;
        let mut total = 0usize;
        for q in queries {
            let approx: HashSet<usize> =
                index.search(q, k, ef).into_iter().map(|(i, _)| i).collect();
            for id in brute_force_metric(data, q, k, metric) {
                total += 1;
                if approx.contains(&id) {
                    hits += 1;
                }
            }
        }
        f64::from(u32::try_from(hits).unwrap_or(u32::MAX))
            / f64::from(u32::try_from(total.max(1)).unwrap_or(u32::MAX))
    }

    #[test]
    fn recall_gate_clustered() {
        let data = clustered_vectors(2000, 32, 20, 11);
        let queries = clustered_vectors(80, 32, 20, 99);
        let r = measure_recall(&data, &queries, 10, 150, Metric::L2, 7);
        assert!(r > 0.98, "clustered recall@10 was {r:.3}, expected > 0.98");
    }

    #[test]
    fn recall_gate_near_duplicates() {
        let data = near_duplicate_vectors(2000, 32, 23);
        let queries = near_duplicate_vectors(80, 32, 71);
        let r = measure_recall(&data, &queries, 10, 200, Metric::L2, 5);
        assert!(
            r > 0.98,
            "near-duplicate recall@10 was {r:.3}, expected > 0.98"
        );
    }

    #[test]
    fn recall_gate_normalized_cosine() {
        let data = normalized_vectors(2000, 64, 31);
        let queries = normalized_vectors(80, 64, 53);
        let r = measure_recall(&data, &queries, 10, 150, Metric::Cosine, 3);
        assert!(
            r > 0.97,
            "normalized cosine recall@10 was {r:.3}, expected > 0.97"
        );
    }

    // --- metamorphic oracle ---
    //
    // You cannot know the exact correct approximate result in general, but you
    // know relations that must always hold between inputs and outputs. Checking
    // those relations is the accepted research answer to the oracle problem for
    // systems whose exact output you cannot predict. These are those relations
    // for nearest-neighbor search.

    #[test]
    fn corruption_in_the_serialized_index_is_detected() {
        // The headline 2B invariant for the durable index: a single flipped bit
        // anywhere in the image is detected on load, so a corrupted graph is
        // never silently trusted.
        let dim = 8;
        let data = gen_vectors(200, dim, 55);
        let mut index = Hnsw::new(dim, 16, 100, 9);
        for v in &data {
            index.insert(v.clone());
        }
        let good = index.to_bytes();
        assert!(Hnsw::from_bytes(&good).is_some(), "a clean image must load");
        // Flip one bit at a sampling of positions; every one must be caught.
        for pos in (0..good.len()).step_by(7) {
            let mut bad = good.clone();
            bad[pos] ^= 0x01;
            assert!(
                Hnsw::from_bytes(&bad).is_none(),
                "a flipped bit at byte {pos} must be detected"
            );
        }
    }

    #[test]
    fn metamorphic_self_retrieval() {
        // Every stored vector is its own nearest neighbor.
        let dim = 24;
        let data = gen_vectors(800, dim, 41);
        let mut index = Hnsw::new(dim, 16, 200, 9);
        for v in &data {
            index.insert(v.clone());
        }
        let mut ok = 0usize;
        for (i, v) in data.iter().enumerate() {
            if index.search(v, 1, 64)[0].0 == i {
                ok += 1;
            }
        }
        let rate = f64::from(u32::try_from(ok).unwrap_or(u32::MAX))
            / f64::from(u32::try_from(data.len()).unwrap_or(u32::MAX));
        assert!(rate > 0.99, "self-retrieval rate was {rate:.4}");
    }

    #[test]
    fn metamorphic_monotonic_insertion() {
        // Inserting a point strictly closer to a query than its current k-th
        // neighbor must make that point appear in the new top-k.
        let dim = 16;
        let data = gen_vectors(500, dim, 7);
        let mut index = Hnsw::new(dim, 16, 200, 3);
        for v in &data {
            index.insert(v.clone());
        }
        let q = data[10].clone();
        // A point a tiny step from q is far closer than any other random vector.
        let mut closer = q.clone();
        closer[0] += 0.5;
        let id = index.insert(closer);
        let after = index.search(&q, 5, 64);
        assert!(
            after.iter().any(|&(i, _)| i == id),
            "a strictly closer inserted point must enter the top-k"
        );
    }

    #[test]
    fn metamorphic_duplicate_insertion() {
        // A duplicate of a stored vector means the two nearest to it are the
        // original and the duplicate, both at distance zero.
        let dim = 16;
        let data = gen_vectors(400, dim, 21);
        let mut index = Hnsw::new(dim, 16, 200, 4);
        for v in &data {
            index.insert(v.clone());
        }
        let v = data[5].clone();
        let dup = index.insert(v.clone());
        let got = index.search(&v, 2, 64);
        let ids: Vec<usize> = got.iter().map(|&(i, _)| i).collect();
        assert!(
            ids.contains(&5) && ids.contains(&dup),
            "the original and its duplicate must be the two nearest, got {ids:?}"
        );
        assert!(
            got[0].1.abs() < 1e-6 && got[1].1.abs() < 1e-6,
            "both nearest distances must be zero"
        );
    }

    #[test]
    fn metamorphic_deletion_consistency() {
        // A removed vector never appears in any result, while its neighbors
        // remain findable.
        let dim = 16;
        let data = gen_vectors(400, dim, 88);
        let mut index = Hnsw::new(dim, 16, 200, 2);
        for v in &data {
            index.insert(v.clone());
        }
        for victim in [3usize, 50, 199, 372] {
            index.remove(victim);
        }
        for (i, v) in data.iter().enumerate() {
            let ids: Vec<usize> = index
                .search(v, 10, 80)
                .into_iter()
                .map(|(j, _)| j)
                .collect();
            assert!(
                !ids.contains(&3)
                    && !ids.contains(&50)
                    && !ids.contains(&199)
                    && !ids.contains(&372),
                "a removed vector appeared in the results for query {i}"
            );
        }
    }

    #[test]
    fn metamorphic_recall_monotone_in_ef() {
        // Recall is monotone non-decreasing in the query beam width ef: searching
        // harder never finds fewer true neighbors.
        let dim = 24;
        let data = clustered_vectors(1500, dim, 15, 17);
        let queries = clustered_vectors(40, dim, 15, 88);
        let r_low = measure_recall(&data, &queries, 10, 12, Metric::L2, 6);
        let r_high = measure_recall(&data, &queries, 10, 200, Metric::L2, 6);
        assert!(
            r_high + 0.02 >= r_low,
            "recall must not fall as ef grows: {r_low:.3} at ef=12 then {r_high:.3} at ef=200"
        );
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
    fn serialize_round_trips_and_rejects_garbage() {
        let dim = 12;
        let data = gen_vectors(400, dim, 31);
        let mut index = Hnsw::new(dim, 16, 100, 8);
        for v in &data {
            index.insert(v.clone());
        }
        let bytes = index.to_bytes();
        let restored = Hnsw::from_bytes(&bytes).expect("restore a valid image");
        assert_eq!(restored.len(), index.len());
        // The restored graph answers every query identically.
        for q in 0..15 {
            let query = &data[q * 11 % data.len()];
            assert_eq!(index.search(query, 10, 64), restored.search(query, 10, 64));
        }
        // A truncated or unrecognized buffer is rejected, never panicked on.
        assert!(Hnsw::from_bytes(&bytes[..bytes.len() / 2]).is_none());
        assert!(Hnsw::from_bytes(b"nope").is_none());
        assert!(Hnsw::from_bytes(&[]).is_none());
    }

    /// Exact top-k by brute force under a given metric, ids nearest first.
    fn brute_force_metric(
        data: &[Vec<f32>],
        query: &[f32],
        k: usize,
        metric: Metric,
    ) -> Vec<usize> {
        let mut scored: Vec<(f32, usize)> = data
            .iter()
            .enumerate()
            .map(|(i, v)| (rank(metric, query, v), i))
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored.into_iter().take(k).map(|(_, i)| i).collect()
    }

    #[test]
    fn cosine_metric_recovers_the_nearest() {
        let dim = 16;
        let data = gen_vectors(600, dim, 71);
        let mut index = Hnsw::new_with_metric(dim, 16, 200, 3, Metric::Cosine);
        for v in &data {
            index.insert(v.clone());
        }
        // Each query is in the set, so its own cosine distance (0) is the minimum
        // and must come back as the exact nearest.
        for q in 0..20 {
            let query = &data[q * 13 % data.len()];
            let got = index.search(query, 1, 80);
            let exact = brute_force_metric(&data, query, 1, Metric::Cosine);
            assert_eq!(got[0].0, exact[0], "cosine nearest should be exact");
        }
    }

    #[test]
    fn l1_metric_recovers_the_nearest() {
        let dim = 12;
        let data = gen_vectors(500, dim, 64);
        let mut index = Hnsw::new_with_metric(dim, 16, 150, 9, Metric::L1);
        for v in &data {
            index.insert(v.clone());
        }
        // Each query is in the set, so its own L1 distance (0) is the minimum.
        for q in 0..20 {
            let query = &data[q * 11 % data.len()];
            let got = index.search(query, 1, 80);
            let exact = brute_force_metric(&data, query, 1, Metric::L1);
            assert_eq!(got[0].0, exact[0], "L1 nearest should be exact");
            assert!(got[0].1.abs() < 1e-6, "own L1 distance should be 0");
        }
    }

    #[test]
    fn inner_product_metric_has_high_recall() {
        let dim = 16;
        let data = gen_vectors(800, dim, 202);
        let mut index = Hnsw::new_with_metric(dim, 16, 200, 5, Metric::InnerProduct);
        for v in &data {
            index.insert(v.clone());
        }
        let mut hits = 0usize;
        let mut total = 0usize;
        let queries = gen_vectors(40, dim, 909);
        for query in &queries {
            let approx: HashSet<usize> = index
                .search(query, 10, 120)
                .into_iter()
                .map(|(i, _)| i)
                .collect();
            for id in brute_force_metric(&data, query, 10, Metric::InnerProduct) {
                total += 1;
                if approx.contains(&id) {
                    hits += 1;
                }
            }
        }
        let recall =
            f64::from(u32::try_from(hits).unwrap()) / f64::from(u32::try_from(total).unwrap());
        assert!(recall > 0.80, "inner-product recall@10 was {recall:.3}");
    }

    #[test]
    fn serialized_metric_round_trips() {
        let dim = 10;
        let data = gen_vectors(150, dim, 44);
        let mut index = Hnsw::new_with_metric(dim, 16, 100, 6, Metric::Cosine);
        for v in &data {
            index.insert(v.clone());
        }
        let restored = Hnsw::from_bytes(&index.to_bytes()).expect("restore");
        // The metric is preserved, so queries rank identically after a reload.
        for q in 0..10 {
            let query = &data[q * 7 % data.len()];
            assert_eq!(index.search(query, 5, 64), restored.search(query, 5, 64));
        }
    }

    #[test]
    fn removed_vectors_are_never_returned() {
        let dim = 12;
        let data = gen_vectors(500, dim, 91);
        let mut index = Hnsw::new(dim, 16, 150, 12);
        for v in &data {
            index.insert(v.clone());
        }
        assert_eq!(index.len(), data.len());
        // Remove the true nearest neighbor of a handful of queries, then confirm
        // the search no longer returns it but still finds the next ones.
        for q in 0..15 {
            let query = data[q * 17 % data.len()].clone();
            let exact = brute_force(&data, &query, 2);
            let removed = index.remove(exact[0]);
            assert!(removed, "removing a live id should succeed");
            assert!(!index.remove(exact[0]), "removing twice should be a no-op");
            let leaked = index
                .search(&query, 5, 80)
                .iter()
                .any(|&(i, _)| i == exact[0]);
            assert!(!leaked, "a removed id must not be returned");
        }
        // The live count dropped by the number removed, and a removed id round
        // trips through serialization (the index stays consistent on reload).
        assert!(index.len() < data.len());
        let restored = Hnsw::from_bytes(&index.to_bytes()).expect("restore");
        assert_eq!(restored.len(), index.len());
        let probe = &data[3];
        assert_eq!(index.search(probe, 5, 80), restored.search(probe, 5, 80));
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
