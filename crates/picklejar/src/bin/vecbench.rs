//! A benchmark for the HNSW index: how much faster is approximate
//! nearest-neighbor search than a brute-force scan, and at what recall.
//!
//! ```text
//! cargo run --release --bin vecbench                  # 50k vectors, dim 128
//! cargo run --release --bin vecbench -- 200000 256    # 200k vectors, dim 256
//! ```
//!
//! Prints the index build time, the average query latency for brute force versus
//! HNSW, the resulting speedup, and recall@10 (the fraction of the true top-10
//! the index recovers). Timing is wall-clock, so run it on an idle machine in
//! release for meaningful numbers.

use std::collections::HashSet;
use std::time::Instant;

use picklejar::hnsw::Hnsw;

/// A small deterministic PRNG so a run is reproducible across machines.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A pseudo-random `f32` in `[-1, 1)`.
    fn unit(&mut self) -> f32 {
        let bits = self.next_u64() >> 40; // 24 bits
        #[allow(clippy::cast_precision_loss)]
        let frac = bits as f32 / f32::from(1u16 << 12) / f32::from(1u16 << 12);
        frac.mul_add(2.0, -1.0)
    }
}

fn random_vectors(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    (0..n)
        .map(|_| (0..dim).map(|_| rng.unit()).collect())
        .collect()
}

fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

/// Exact top-k by brute force, ids nearest first.
fn brute_force(data: &[Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
    let mut scored: Vec<(f32, usize)> = data
        .iter()
        .enumerate()
        .map(|(i, v)| (l2_sq(query, v), i))
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    scored.into_iter().take(k).map(|(_, i)| i).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let dim: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(128);
    let queries = 200usize;
    let k = 10usize;

    println!("building {n} vectors of dim {dim}...");
    let data = random_vectors(n, dim, 1);
    let probes = random_vectors(queries, dim, 2);

    // Build the index (m = 16, ef_construction = 200 are solid defaults).
    let build_start = Instant::now();
    let mut index = Hnsw::new(dim, 16, 200, 42);
    for v in &data {
        index.insert(v.clone());
    }
    let build = build_start.elapsed();

    // Brute force: the exact baseline.
    let bf_start = Instant::now();
    let exact: Vec<Vec<usize>> = probes.iter().map(|q| brute_force(&data, q, k)).collect();
    let bf = bf_start.elapsed();

    // HNSW search.
    let hnsw_start = Instant::now();
    let approx: Vec<Vec<usize>> = probes
        .iter()
        .map(|q| {
            index
                .search(q, k, 100)
                .into_iter()
                .map(|(i, _)| i)
                .collect()
        })
        .collect();
    let hnsw = hnsw_start.elapsed();

    // Recall@k: fraction of the true top-k the index recovered.
    let mut hits = 0usize;
    let mut total = 0usize;
    for (a, e) in approx.iter().zip(&exact) {
        let set: HashSet<usize> = a.iter().copied().collect();
        for id in e {
            total += 1;
            if set.contains(id) {
                hits += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let recall = hits as f64 / total as f64;
    #[allow(clippy::cast_precision_loss)]
    let q = queries as f64;

    let bf_avg_us = bf.as_secs_f64() * 1e6 / q;
    let hnsw_avg_us = hnsw.as_secs_f64() * 1e6 / q;
    let speedup = bf.as_secs_f64() / hnsw.as_secs_f64().max(f64::MIN_POSITIVE);

    println!("index build:     {:.2}s", build.as_secs_f64());
    println!("brute force:     {bf_avg_us:.1} us/query");
    println!("HNSW:            {hnsw_avg_us:.1} us/query");
    println!("speedup:         {speedup:.1}x");
    println!("recall@{k}:       {recall:.3}");
}
