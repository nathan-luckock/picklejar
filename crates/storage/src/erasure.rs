//! Reed-Solomon erasure coding over GF(2^8), from scratch.
//!
//! This is the primitive behind the project's mass-efficiency claim. Surviving
//! `m` simultaneous device failures the hardware way means launching `m` extra
//! copies (mirroring or triplication: `+100%` to `+200%` mass for the same usable
//! bytes). Erasure coding survives the same `m` failures with only `m` parity
//! shards over `k` data shards, an overhead of `m / k`. For `k = 10, m = 2` that
//! is `+20%` instead of `+200%`, which in a place where every launched kilogram
//! costs thousands of dollars is the whole argument for doing reliability in
//! software on cheap commodity storage instead of in heavy radiation-hardened,
//! triple-redundant hardware.
//!
//! The construction is the standard systematic Reed-Solomon used by production
//! object stores: a `(k + m) x k` Vandermonde matrix made systematic so the `k`
//! data shards pass through unchanged and the `m` parity shards are linear
//! combinations of them. Because any `k` rows of a Vandermonde matrix over
//! distinct field nodes are linearly independent, any `k` surviving shards (data
//! or parity, in any mix) determine the original data exactly: invert the `k x k`
//! submatrix of the surviving rows and multiply. That is the "reconstruct any `k`
//! of `k + m`" property, and it tolerates *erasures* (a shard known to be missing
//! or, paired with the page CRC that says which shard is bad, a shard known to be
//! corrupt), which is exactly the failure mode radiation produces once a checksum
//! has localized it.
//!
//! All arithmetic is in GF(2^8) with the standard primitive polynomial
//! `0x11D`, so a shard byte and its codeword byte share a width and the code is
//! byte-oriented. Everything is deterministic, which is what lets the simulator
//! replay an erasure pattern exactly.

use std::sync::OnceLock;

/// What can go wrong building or using a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErasureError {
    /// `k` was zero, or `k + m` exceeded the 256 distinct nodes GF(2^8) offers.
    BadShape {
        /// Number of data shards requested.
        k: usize,
        /// Number of parity shards requested.
        m: usize,
    },
    /// The slice of shards handed in did not have `k + m` entries, or they were
    /// not all the same length.
    ShardLayout,
    /// Fewer than `k` shards survived, so reconstruction is impossible.
    TooManyErasures {
        /// Shards still present.
        have: usize,
        /// Shards needed.
        need: usize,
    },
}

impl std::fmt::Display for ErasureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadShape { k, m } => {
                write!(
                    f,
                    "invalid erasure shape: k={k}, m={m} (need k>=1, k+m<=256)"
                )
            }
            Self::ShardLayout => f.write_str("shards must number k+m and share one length"),
            Self::TooManyErasures { have, need } => {
                write!(f, "only {have} shards survived, need {need} to reconstruct")
            }
        }
    }
}

impl std::error::Error for ErasureError {}

/// Precomputed GF(2^8) log and antilog tables for the polynomial `0x11D`.
struct Gf {
    /// `exp[i] = generator^i`, doubled in length so a product index never wraps.
    exp: [u8; 512],
    /// `log[x]` is the discrete log of `x` (undefined, and unused, for `x = 0`).
    log: [u8; 256],
}

impl Gf {
    fn build() -> Self {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u8 = 1;
        for (i, slot) in exp.iter_mut().take(255).enumerate() {
            *slot = x;
            log[x as usize] = u8::try_from(i).expect("i < 255");
            // Multiply by the generator (2): shift, and reduce by 0x1D on carry.
            let carry = x & 0x80;
            x <<= 1;
            if carry != 0 {
                x ^= 0x1D;
            }
        }
        // The second half repeats the first so a product index never wraps.
        exp.copy_within(0..257, 255);
        Self { exp, log }
    }

    #[inline]
    const fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }

    #[inline]
    const fn inv(&self, a: u8) -> u8 {
        debug_assert!(a != 0, "0 has no inverse in GF(2^8)");
        self.exp[255 - self.log[a as usize] as usize]
    }
}

/// The field tables are immutable and identical everywhere, so build them once.
fn gf() -> &'static Gf {
    static TABLES: OnceLock<Gf> = OnceLock::new();
    TABLES.get_or_init(Gf::build)
}

/// `a^e` in GF(2^8), with `0^0 = 1`.
fn gf_pow(field: &Gf, a: u8, e: usize) -> u8 {
    let mut result = 1u8;
    for _ in 0..e {
        result = field.mul(result, a);
    }
    result
}

/// Invert a square `n x n` matrix over GF(2^8) by Gauss-Jordan elimination, or
/// return `None` if it is singular. Rows are `Vec<u8>` of length `n`.
fn invert(field: &Gf, mut m: Vec<Vec<u8>>) -> Option<Vec<Vec<u8>>> {
    let n = m.len();
    // Augment with the identity.
    for (i, row) in m.iter_mut().enumerate() {
        row.resize(2 * n, 0);
        row[n + i] = 1;
    }
    for col in 0..n {
        // Find a pivot row with a nonzero entry in this column.
        let pivot = (col..n).find(|&r| m[r][col] != 0)?;
        m.swap(col, pivot);
        // Normalize the pivot row so the pivot becomes 1.
        let inv = field.inv(m[col][col]);
        for cell in &mut m[col] {
            *cell = field.mul(*cell, inv);
        }
        // Eliminate this column from every other row, against a copy of the
        // pivot row so the borrows do not overlap.
        let pivot_row = m[col].clone();
        for (r, row) in m.iter_mut().enumerate() {
            if r != col && row[col] != 0 {
                let factor = row[col];
                for (cell, &pv) in row.iter_mut().zip(pivot_row.iter()) {
                    *cell ^= field.mul(factor, pv);
                }
            }
        }
    }
    Some(m.into_iter().map(|row| row[n..].to_vec()).collect())
}

/// A systematic Reed-Solomon code: `k` data shards plus `m` parity shards, able
/// to reconstruct all `k` data shards from any `k` of the `k + m` total.
#[derive(Debug, Clone)]
pub struct ReedSolomon {
    k: usize,
    m: usize,
    /// The systematic generator matrix, `(k + m) x k`. The top `k` rows are the
    /// identity (data passes through); the bottom `m` rows are the parity
    /// coefficients.
    matrix: Vec<Vec<u8>>,
}

impl ReedSolomon {
    /// Build a code for `k` data and `m` parity shards.
    ///
    /// # Errors
    ///
    /// Returns [`ErasureError::BadShape`] if `k == 0` or `k + m > 256`.
    pub fn new(k: usize, m: usize) -> Result<Self, ErasureError> {
        if k == 0 || k + m > 256 {
            return Err(ErasureError::BadShape { k, m });
        }
        let field = gf();
        // Vandermonde matrix over distinct nodes 0, 1, ..., k+m-1.
        let vander: Vec<Vec<u8>> = (0..k + m)
            .map(|i| {
                let node = u8::try_from(i).expect("k+m<=256");
                (0..k).map(|j| gf_pow(field, node, j)).collect()
            })
            .collect();
        // Make it systematic: multiply on the right by the inverse of the top
        // k x k block, so the top becomes the identity and the data shards pass
        // through unchanged. The top block is invertible because it is itself a
        // Vandermonde matrix over distinct nodes.
        let top: Vec<Vec<u8>> = vander[..k].to_vec();
        let top_inv = invert(field, top).ok_or(ErasureError::BadShape { k, m })?;
        let matrix = matmul(field, &vander, &top_inv);
        Ok(Self { k, m, matrix })
    }

    /// Data shards in the code.
    #[must_use]
    pub const fn data_shards(&self) -> usize {
        self.k
    }

    /// Parity shards in the code.
    #[must_use]
    pub const fn parity_shards(&self) -> usize {
        self.m
    }

    /// The storage overhead of this code as a fraction (`m / k`): the extra bytes
    /// stored per data byte to tolerate `m` failures.
    #[must_use]
    pub fn overhead(&self) -> f64 {
        // k is never zero (checked in `new`), and both are small counts.
        #[allow(clippy::cast_precision_loss)]
        {
            self.m as f64 / self.k as f64
        }
    }

    /// Fill the `m` parity shards from the `k` data shards.
    ///
    /// `shards` must hold `k + m` shards of one common length: the first `k` are
    /// the data (read), the last `m` are the parity (written).
    ///
    /// # Errors
    ///
    /// Returns [`ErasureError::ShardLayout`] if the count or lengths are wrong.
    pub fn encode(&self, shards: &mut [Vec<u8>]) -> Result<(), ErasureError> {
        let len = self.checked_len(shards)?;
        let field = gf();
        for p in 0..self.m {
            // Parity row p over the data shards.
            let coeffs = &self.matrix[self.k + p];
            let mut out = vec![0u8; len];
            for (i, coeff) in coeffs.iter().enumerate().take(self.k) {
                let src = &shards[i];
                for b in 0..len {
                    out[b] ^= field.mul(*coeff, src[b]);
                }
            }
            shards[self.k + p] = out;
        }
        Ok(())
    }

    /// Reconstruct every missing shard in place. `present[i]` is whether shard `i`
    /// survived; missing shards may hold any bytes and are overwritten. At least
    /// `k` shards must be present.
    ///
    /// # Errors
    ///
    /// Returns [`ErasureError::ShardLayout`] on a bad layout, or
    /// [`ErasureError::TooManyErasures`] if fewer than `k` shards survived.
    pub fn reconstruct(
        &self,
        shards: &mut [Vec<u8>],
        present: &[bool],
    ) -> Result<(), ErasureError> {
        let len = self.checked_len(shards)?;
        if present.len() != self.k + self.m {
            return Err(ErasureError::ShardLayout);
        }
        let have = present.iter().filter(|p| **p).count();
        if have < self.k {
            return Err(ErasureError::TooManyErasures { have, need: self.k });
        }
        let field = gf();

        // Take the first k present shards as the basis for recovery.
        let rows: Vec<usize> = present
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.then_some(i))
            .take(self.k)
            .collect();
        // The k x k submatrix of the generator for those rows, and its inverse.
        let sub: Vec<Vec<u8>> = rows.iter().map(|&r| self.matrix[r].clone()).collect();
        let sub_inv =
            invert(field, sub).ok_or(ErasureError::TooManyErasures { have, need: self.k })?;

        // Recover the original data shards: data = sub_inv * present_values.
        let mut data: Vec<Vec<u8>> = vec![vec![0u8; len]; self.k];
        for (i, drow) in data.iter_mut().enumerate() {
            for (r, &row) in rows.iter().enumerate() {
                let coeff = sub_inv[i][r];
                if coeff == 0 {
                    continue;
                }
                let src = &shards[row];
                for b in 0..len {
                    drow[b] ^= field.mul(coeff, src[b]);
                }
            }
        }

        // Write recovered data back into any missing data slots.
        for i in 0..self.k {
            if !present[i] {
                shards[i].clone_from(&data[i]);
            }
        }
        // Put the data in place so a full re-encode can refill missing parity.
        for i in 0..self.k {
            shards[i].clone_from(&data[i]);
        }
        // Recompute any missing parity from the now-correct data.
        for p in 0..self.m {
            if !present[self.k + p] {
                let coeffs = &self.matrix[self.k + p];
                let mut out = vec![0u8; len];
                for (i, coeff) in coeffs.iter().enumerate().take(self.k) {
                    let src = &shards[i];
                    for b in 0..len {
                        out[b] ^= field.mul(*coeff, src[b]);
                    }
                }
                shards[self.k + p] = out;
            }
        }
        Ok(())
    }

    /// Validate the shard count and shared length, returning the length.
    fn checked_len(&self, shards: &[Vec<u8>]) -> Result<usize, ErasureError> {
        if shards.len() != self.k + self.m {
            return Err(ErasureError::ShardLayout);
        }
        let len = shards.first().map_or(0, Vec::len);
        if shards.iter().any(|s| s.len() != len) {
            return Err(ErasureError::ShardLayout);
        }
        Ok(len)
    }
}

/// Multiply `a` (`p x q`) by `b` (`q x r`) over GF(2^8), giving a `p x r` matrix.
fn matmul(field: &Gf, a: &[Vec<u8>], b: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let q = b.len();
    let r = b.first().map_or(0, Vec::len);
    a.iter()
        .map(|arow| {
            (0..r)
                .map(|col| {
                    let mut acc = 0u8;
                    for k in 0..q {
                        acc ^= field.mul(arow[k], b[k][col]);
                    }
                    acc
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ErasureError, Gf, ReedSolomon};

    /// `SplitMix64`, so erasure patterns and payloads replay exactly.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % n as u64).expect("fits")
        }
        fn byte(&mut self) -> u8 {
            u8::try_from(self.next() & 0xFF).expect("masked")
        }
    }

    #[test]
    fn field_inverse_is_consistent() {
        let f = Gf::build();
        for a in 1u16..=255 {
            let a = u8::try_from(a).unwrap();
            assert_eq!(f.mul(a, f.inv(a)), 1, "a * a^-1 must be 1 for {a}");
        }
    }

    #[test]
    fn field_multiply_is_commutative_and_distributive() {
        let f = Gf::build();
        for a in [0u8, 1, 2, 7, 19, 200, 255] {
            for b in [0u8, 1, 3, 8, 100, 254] {
                assert_eq!(f.mul(a, b), f.mul(b, a));
                for c in [0u8, 5, 9, 250] {
                    // a*(b+c) = a*b + a*c, with + being XOR.
                    assert_eq!(f.mul(a, b ^ c), f.mul(a, b) ^ f.mul(a, c));
                }
            }
        }
    }

    #[test]
    fn data_shards_pass_through_unchanged() {
        // Systematic: encoding must not alter the data shards.
        let rs = ReedSolomon::new(6, 3).unwrap();
        let mut shards: Vec<Vec<u8>> = (0..9).map(|_| vec![0u8; 32]).collect();
        let mut rng = Rng(1);
        for s in shards.iter_mut().take(6) {
            for b in s.iter_mut() {
                *b = rng.byte();
            }
        }
        let original = shards.clone();
        rs.encode(&mut shards).unwrap();
        assert_eq!(
            shards[..6],
            original[..6],
            "data shards changed during encode"
        );
    }

    #[test]
    fn reconstructs_after_every_erasure_up_to_m() {
        // For several shapes, erase every subset of up to m shards and require an
        // exact reconstruction; erasing m+1 must be rejected, not silently wrong.
        let shapes = [(4usize, 2usize), (6, 3), (10, 4), (3, 1), (8, 8)];
        let mut rng = Rng(0xC0FF_EE12);
        for (k, m) in shapes {
            let rs = ReedSolomon::new(k, m).unwrap();
            let len = 48usize;
            let mut full: Vec<Vec<u8>> = (0..k + m).map(|_| vec![0u8; len]).collect();
            for s in full.iter_mut().take(k) {
                for b in s.iter_mut() {
                    *b = rng.byte();
                }
            }
            rs.encode(&mut full).unwrap();

            for _ in 0..40 {
                let erase = rng.below(m + 1); // 0..=m erasures: always recoverable
                let mut present = vec![true; k + m];
                let mut shards = full.clone();
                for _ in 0..erase {
                    let i = rng.below(k + m);
                    present[i] = false;
                    for b in &mut shards[i] {
                        *b = rng.byte(); // overwrite the "lost" shard with garbage
                    }
                }
                rs.reconstruct(&mut shards, &present).unwrap();
                assert_eq!(shards, full, "k={k} m={m}: reconstruction differed");
            }
        }
    }

    #[test]
    fn too_many_erasures_is_an_error_not_a_wrong_answer() {
        let rs = ReedSolomon::new(5, 2).unwrap();
        let len = 16usize;
        let mut full: Vec<Vec<u8>> = (0..7).map(|_| vec![1u8; len]).collect();
        rs.encode(&mut full).unwrap();
        // Lose three shards with only two parity: unrecoverable, must error.
        let present = [true, false, false, false, true, true, true];
        let mut shards = full.clone();
        let err = rs.reconstruct(&mut shards, &present).unwrap_err();
        assert_eq!(err, ErasureError::TooManyErasures { have: 4, need: 5 });
    }

    #[test]
    fn bad_shapes_are_rejected() {
        assert!(matches!(
            ReedSolomon::new(0, 2),
            Err(ErasureError::BadShape { .. })
        ));
        assert!(matches!(
            ReedSolomon::new(200, 100),
            Err(ErasureError::BadShape { .. })
        ));
        assert!(ReedSolomon::new(255, 1).is_ok());
    }

    #[test]
    fn overhead_reports_the_mass_story() {
        // 10 data + 2 parity survives 2 failures at 20% overhead, the number the
        // mass-efficiency argument rests on.
        let rs = ReedSolomon::new(10, 2).unwrap();
        assert!((rs.overhead() - 0.2).abs() < 1e-9);
        assert_eq!(rs.data_shards(), 10);
        assert_eq!(rs.parity_shards(), 2);
    }
}
