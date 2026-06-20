//! Blind vector search: rank nearest neighbors without revealing the embeddings.
//!
//! A memory node you cannot reach is a node that can read whatever you store on
//! it. Raw embeddings are not opaque: an attacker with the vectors can often run
//! an inversion model and recover the text they were built from. This module
//! lets the client keep a secret and hand the server only a rotated view of
//! every embedding.
//!
//! The trick is that an orthonormal rotation preserves Euclidean distance
//! exactly: for any rotation `Q`, `||Qx - Qy|| == ||x - y||`. So the server can
//! store `Q v` for every memory, receive `Q q` for a query, and rank by distance
//! exactly as it would on the real vectors, while never seeing a real
//! coordinate. Only the client, holding `Q`, can map between the true space and
//! the rotated one.
//!
//! Honest scope: this hides the *content* of the embeddings (their coordinates,
//! and so the axes a learned inverter would need), not their *geometry*. The
//! server still learns pairwise distances, because ranking needs them. It defeats
//! an honest-but-curious node reading raw vectors to reconstruct the source text;
//! it is not full encrypted search.

/// A small deterministic xorshift generator, so a seed reproduces a rotation.
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

    /// A value in `[-1, 1)`.
    #[allow(clippy::cast_precision_loss)]
    fn signed(&mut self) -> f64 {
        let unit = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        unit.mul_add(2.0, -1.0)
    }
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// An orthonormal rotation of `dim`-dimensional space. It preserves L2 distance
/// exactly, and the client keeps it secret.
#[derive(Clone, Debug)]
pub struct Rotation {
    dim: usize,
    rows: Vec<Vec<f64>>,
}

impl Rotation {
    /// Derive a rotation from a secret seed. Builds a random basis and
    /// orthonormalizes it with Gram-Schmidt, so the rows form an orthonormal
    /// matrix.
    #[must_use]
    pub fn from_seed(dim: usize, seed: u64) -> Self {
        let mut rng = Rng(seed | 1);
        let mut rows: Vec<Vec<f64>> = Vec::with_capacity(dim);
        for _ in 0..dim {
            loop {
                let mut v: Vec<f64> = (0..dim).map(|_| rng.signed()).collect();
                for u in &rows {
                    let proj = dot(&v, u);
                    for (vi, ui) in v.iter_mut().zip(u) {
                        *vi -= proj * ui;
                    }
                }
                let norm = dot(&v, &v).sqrt();
                if norm > 1e-9 {
                    for vi in &mut v {
                        *vi /= norm;
                    }
                    rows.push(v);
                    break;
                }
                // Degenerate draw (vanishingly rare); try again.
            }
        }
        Self { dim, rows }
    }

    /// The dimensionality this rotation acts on.
    #[must_use]
    pub const fn dim(&self) -> usize {
        self.dim
    }

    /// Rotate a true embedding into the blinded space the server sees.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn rotate(&self, v: &[f32]) -> Vec<f32> {
        self.rows
            .iter()
            .map(|row| {
                row.iter()
                    .zip(v)
                    .map(|(a, b)| a * f64::from(*b))
                    .sum::<f64>() as f32
            })
            .collect()
    }

    /// Map a blinded vector back to the true space (the inverse rotation, which
    /// for an orthonormal matrix is its transpose). Only the key holder can.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn unrotate(&self, w: &[f32]) -> Vec<f32> {
        (0..self.dim)
            .map(|j| {
                self.rows
                    .iter()
                    .zip(w)
                    .map(|(row, wi)| row[j] * f64::from(*wi))
                    .sum::<f64>() as f32
            })
            .collect()
    }
}

/// Squared L2 distance.
#[must_use]
pub fn l2_sq(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = f64::from(*x) - f64::from(*y);
            d * d
        })
        .sum()
}

/// Server-side k-nearest-neighbor search. It runs on whatever vectors it is
/// given, knowing nothing about any rotation, and returns ids with distances.
#[must_use]
pub fn knn(db: &[(u64, Vec<f32>)], query: &[f32], k: usize) -> Vec<(u64, f64)> {
    let mut scored: Vec<(u64, f64)> = db.iter().map(|(id, v)| (*id, l2_sq(query, v))).collect();
    scored.sort_by(|a, b| a.1.total_cmp(&b.1));
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps * (1.0 + a.abs().max(b.abs()))
    }

    #[test]
    fn rotation_preserves_distance() {
        let r = Rotation::from_seed(8, 0xABCD_1234);
        let a = [0.1, 0.5, -0.3, 0.8, 0.0, 0.2, -0.9, 0.4];
        let b = [0.7, -0.2, 0.1, 0.3, 0.6, -0.5, 0.2, 0.1];
        let plain = l2_sq(&a, &b);
        let blinded = l2_sq(&r.rotate(&a), &r.rotate(&b));
        assert!(
            approx(plain, blinded, 1e-4),
            "plain {plain} vs blinded {blinded}"
        );
    }

    #[test]
    fn unrotate_inverts_rotate() {
        let r = Rotation::from_seed(8, 7);
        let v = [0.1, 0.5, -0.3, 0.8, 0.0, 0.2, -0.9, 0.4];
        let back = r.unrotate(&r.rotate(&v));
        for (x, y) in v.iter().zip(&back) {
            assert!(approx(f64::from(*x), f64::from(*y), 1e-4));
        }
    }

    #[test]
    fn rows_are_orthonormal() {
        let r = Rotation::from_seed(6, 99);
        for (i, ri) in r.rows.iter().enumerate() {
            assert!(approx(dot(ri, ri), 1.0, 1e-9), "row {i} not unit length");
            for rj in r.rows.iter().skip(i + 1) {
                assert!(dot(ri, rj).abs() < 1e-9, "rows not orthogonal");
            }
        }
    }

    #[test]
    fn blind_search_returns_the_same_neighbors_as_plaintext() {
        // Well-separated memories so ranking is unambiguous.
        let db: Vec<(u64, Vec<f32>)> = vec![
            (1, vec![0.0, 0.0, 0.0, 0.0]),
            (2, vec![1.0, 1.0, 1.0, 1.0]),
            (3, vec![5.0, 5.0, 5.0, 5.0]),
            (4, vec![0.2, 0.1, 0.0, 0.1]),
            (5, vec![2.0, 2.0, 2.0, 2.0]),
        ];
        let query = vec![0.1, 0.05, 0.0, 0.05];

        let plain = knn(&db, &query, 3);

        let r = Rotation::from_seed(4, 0xFEED);
        let blinded_db: Vec<(u64, Vec<f32>)> =
            db.iter().map(|(id, v)| (*id, r.rotate(v))).collect();
        let blinded_query = r.rotate(&query);
        let blind = knn(&blinded_db, &blinded_query, 3);

        let plain_ids: Vec<u64> = plain.iter().map(|(id, _)| *id).collect();
        let blind_ids: Vec<u64> = blind.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            plain_ids, blind_ids,
            "blind ranking must match plaintext ranking"
        );
    }

    #[test]
    fn the_server_view_is_not_the_plaintext() {
        let r = Rotation::from_seed(8, 1);
        let v = [0.1, 0.5, -0.3, 0.8, 0.0, 0.2, -0.9, 0.4];
        let rotated = r.rotate(&v);
        assert_ne!(
            rotated.as_slice(),
            v.as_slice(),
            "rotated view should differ from the plaintext"
        );
    }
}
