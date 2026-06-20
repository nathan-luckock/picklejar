//! HyperLogLog: count distinct memories in kilobytes, at any scale.
//!
//! How many distinct memories has a tenant ever stored? Keeping a set to answer
//! exactly costs memory proportional to the count, which is hopeless on a small
//! node holding billions of items. HyperLogLog estimates the cardinality in a
//! fixed, tiny footprint, by watching the rarest hash pattern it sees. If the
//! longest run of leading zeros in any item's hash is `r`, then roughly `2^r`
//! distinct items have probably been seen; averaging that estimator over many
//! independent buckets tightens it to a small relative error.
//!
//! This uses 2^14 one-byte registers, about 16 KiB, for a standard error near
//! 0.8%, regardless of whether the true count is a thousand or a billion.

#![allow(clippy::doc_markdown)] // "HyperLogLog" reads as prose, not code

use crate::authmem::sha256;

/// log2 of the register count.
const P: u32 = 14;
/// Number of registers.
const M: usize = 1 << P;

fn hash64(key: &[u8]) -> u64 {
    let d = sha256::hash(key);
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// A HyperLogLog distinct-count estimator.
#[derive(Clone, Debug)]
pub struct HyperLogLog {
    registers: Vec<u8>,
}

impl Default for HyperLogLog {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperLogLog {
    /// A fresh estimator (about 16 KiB).
    #[must_use]
    pub fn new() -> Self {
        Self {
            registers: vec![0u8; M],
        }
    }

    /// Observe a memory key.
    #[allow(clippy::cast_possible_truncation)] // index is the top P bits; rank fits u8
    pub fn add(&mut self, key: &[u8]) {
        let h = hash64(key);
        let idx = (h >> (64 - P)) as usize;
        // Rank: position of the leftmost 1 in the remaining bits, capped.
        let w = h << P;
        let rank = (w.leading_zeros() + 1).min(64 - P + 1) as u8;
        if rank > self.registers[idx] {
            self.registers[idx] = rank;
        }
    }

    /// Estimate the number of distinct keys observed.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn estimate(&self) -> f64 {
        let m = M as f64;
        let alpha = 0.7213 / (1.0 + 1.079 / m);
        let mut sum = 0.0_f64;
        let mut zeros = 0u32;
        for &r in &self.registers {
            sum += 2.0_f64.powi(-i32::from(r));
            if r == 0 {
                zeros += 1;
            }
        }
        let raw = alpha * m * m / sum;
        // Small-range correction: linear counting when many registers are empty.
        if raw <= 2.5 * m && zeros > 0 {
            m * (m / f64::from(zeros)).ln()
        } else {
            raw
        }
    }

    /// Merge another estimator (union of the two observed sets), register-wise
    /// maximum. Lets partitioned nodes combine their counts.
    pub fn merge(&mut self, other: &Self) {
        for (a, &b) in self.registers.iter_mut().zip(&other.registers) {
            *a = (*a).max(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn rel_error(estimate: f64, truth: u64) -> f64 {
        (estimate - truth as f64).abs() / truth as f64
    }

    #[test]
    fn estimates_a_large_cardinality_within_a_few_percent() {
        let mut hll = HyperLogLog::new();
        let n = 200_000u64;
        for i in 0..n {
            hll.add(&i.to_be_bytes());
        }
        let err = rel_error(hll.estimate(), n);
        assert!(
            err < 0.03,
            "relative error {err} should be within a few percent"
        );
    }

    #[test]
    fn duplicates_do_not_inflate_the_count() {
        let mut hll = HyperLogLog::new();
        for _ in 0..100 {
            for i in 0..1000u64 {
                hll.add(&i.to_be_bytes());
            }
        }
        let err = rel_error(hll.estimate(), 1000);
        assert!(err < 0.05, "100x duplicates of 1000 keys, error {err}");
    }

    #[test]
    fn small_cardinalities_use_linear_counting() {
        let mut hll = HyperLogLog::new();
        for i in 0..50u64 {
            hll.add(&i.to_be_bytes());
        }
        let est = hll.estimate();
        assert!(
            (est - 50.0).abs() < 5.0,
            "small count estimate {est} should be close to 50"
        );
    }

    #[test]
    fn merge_is_the_union() {
        let mut a = HyperLogLog::new();
        let mut b = HyperLogLog::new();
        for i in 0..100_000u64 {
            a.add(&i.to_be_bytes());
        }
        for i in 50_000..150_000u64 {
            b.add(&i.to_be_bytes());
        }
        a.merge(&b);
        // Union is 0..150_000.
        let err = rel_error(a.estimate(), 150_000);
        assert!(err < 0.03, "merged estimate error {err}");
    }
}
