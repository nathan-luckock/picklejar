//! Product quantization: compress embeddings to a few bytes, keep them rankable.
//!
//! The scalar quantizer trims an embedding from `f32` to one byte per dimension,
//! a 4x win. Product quantization goes much further. It splits each vector into
//! `m` contiguous sub-vectors, learns a small codebook of representative points
//! for each sub-space with k-means, and stores only which codeword each
//! sub-vector is closest to: `m` bytes for the whole embedding, often a 16x to
//! 64x reduction. The reconstruction is approximate, but close enough that
//! nearest-neighbor ranking over the compressed codes still surfaces the right
//! memories, so a node can hold far more vectors in the same space.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

/// A trained product quantizer.
#[derive(Clone, Debug)]
pub struct ProductQuantizer {
    sub_dim: usize,
    /// `m` codebooks, each `k` centroids of length `sub_dim`.
    codebooks: Vec<Vec<Vec<f32>>>,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn nearest(centroids: &[Vec<f32>], v: &[f32]) -> usize {
    let mut best = 0;
    let mut best_d = f32::INFINITY;
    for (i, c) in centroids.iter().enumerate() {
        let d = l2(c, v);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// One k-means run over `points`, returning `k` centroids.
fn kmeans(points: &[Vec<f32>], k: usize, iters: usize, rng: &mut Rng) -> Vec<Vec<f32>> {
    let dim = points[0].len();
    // Initialize centroids from random distinct points.
    let mut centroids: Vec<Vec<f32>> = Vec::with_capacity(k);
    for _ in 0..k {
        let idx = (rng.next() as usize) % points.len();
        centroids.push(points[idx].clone());
    }
    for _ in 0..iters {
        let mut sums = vec![vec![0.0f32; dim]; k];
        let mut counts = vec![0usize; k];
        for p in points {
            let c = nearest(&centroids, p);
            counts[c] += 1;
            for (s, x) in sums[c].iter_mut().zip(p) {
                *s += x;
            }
        }
        for c in 0..k {
            if counts[c] > 0 {
                for (cv, s) in centroids[c].iter_mut().zip(&sums[c]) {
                    *cv = s / counts[c] as f32;
                }
            }
        }
    }
    centroids
}

impl ProductQuantizer {
    /// Train a quantizer on `vectors`, splitting each into `m` sub-spaces with a
    /// `k`-codeword codebook per sub-space (`k` at most 256).
    ///
    /// # Panics
    /// Panics if the inputs are empty, `dims` is not divisible by `m`, or `k`
    /// exceeds 256.
    #[must_use]
    pub fn train(vectors: &[Vec<f32>], m: usize, k: usize, seed: u64) -> Self {
        assert!(!vectors.is_empty(), "need training vectors");
        assert!((1..=256).contains(&k), "k in 1..=256");
        let dims = vectors[0].len();
        assert!(
            m >= 1 && dims % m == 0,
            "dims must divide into m sub-spaces"
        );
        let sub_dim = dims / m;
        let mut rng = Rng(seed | 1);
        let codebooks = (0..m)
            .map(|s| {
                let sub: Vec<Vec<f32>> = vectors
                    .iter()
                    .map(|v| v[s * sub_dim..(s + 1) * sub_dim].to_vec())
                    .collect();
                kmeans(&sub, k.min(sub.len()), 12, &mut rng)
            })
            .collect();
        Self { sub_dim, codebooks }
    }

    /// The number of sub-spaces (and the code length in bytes).
    #[must_use]
    pub fn code_len(&self) -> usize {
        self.codebooks.len()
    }

    /// Encode a vector to one codeword index per sub-space.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // centroid index < k <= 256
    pub fn encode(&self, v: &[f32]) -> Vec<u8> {
        self.codebooks
            .iter()
            .enumerate()
            .map(|(s, book)| nearest(book, &v[s * self.sub_dim..(s + 1) * self.sub_dim]) as u8)
            .collect()
    }

    /// Reconstruct an approximate vector from its code.
    #[must_use]
    pub fn decode(&self, code: &[u8]) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.codebooks.len() * self.sub_dim);
        for (s, &c) in code.iter().enumerate() {
            out.extend_from_slice(&self.codebooks[s][c as usize]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clustered(n: usize, dims: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut rng = Rng(seed | 1);
        let centers = 8;
        let mut cs: Vec<Vec<f32>> = Vec::new();
        for _ in 0..centers {
            cs.push(
                (0..dims)
                    .map(|_| (rng.next() >> 40) as f32 / 16_777_216.0)
                    .collect(),
            );
        }
        (0..n)
            .map(|i| {
                let c = &cs[i % centers];
                c.iter()
                    .map(|x| x + ((rng.next() >> 40) as f32 / 16_777_216.0 - 0.5) * 0.02)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn compresses_and_reconstructs_closely() {
        let data = clustered(2000, 32, 1);
        let pq = ProductQuantizer::train(&data, 8, 256, 42);
        assert_eq!(
            pq.code_len(),
            8,
            "32-dim vector -> 8-byte code (16x smaller than 128 bytes)"
        );

        let mut total_rel = 0.0f32;
        for v in data.iter().take(200) {
            let r = pq.decode(&pq.encode(v));
            let err = l2(v, &r).sqrt();
            let norm = l2(v, &vec![0.0; v.len()]).sqrt().max(1e-6);
            total_rel += err / norm;
        }
        let avg = total_rel / 200.0;
        assert!(
            avg < 0.2,
            "average reconstruction error {avg} should be small"
        );
    }

    #[test]
    fn ranking_is_mostly_preserved() {
        let data = clustered(1000, 32, 7);
        let pq = ProductQuantizer::train(&data, 8, 256, 3);
        let codes: Vec<Vec<u8>> = data.iter().map(|v| pq.encode(v)).collect();

        // For several queries, the exact nearest should appear among the
        // top few PQ-ranked candidates.
        let mut hits = 0;
        for (qi, q) in data.iter().enumerate().take(50) {
            let exact = data
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != qi)
                .min_by(|a, b| l2(q, a.1).total_cmp(&l2(q, b.1)))
                .map(|(i, _)| i)
                .unwrap();
            // Rank all by reconstructed distance.
            let mut ranked: Vec<(usize, f32)> = codes
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != qi)
                .map(|(i, c)| (i, l2(q, &pq.decode(c))))
                .collect();
            ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
            if ranked.iter().take(10).any(|(i, _)| *i == exact) {
                hits += 1;
            }
        }
        assert!(
            hits >= 40,
            "exact nearest should be in PQ top-10 most of the time, was {hits}/50"
        );
    }
}
