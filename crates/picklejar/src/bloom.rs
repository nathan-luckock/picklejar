//! Bloom filter: ask "have I already stored this memory?" in a few bits each.
//!
//! Before an agent writes a memory it often wants to know whether it has seen
//! that exact item before, to avoid storing a duplicate or re-running expensive
//! work. Keeping every key in a set is exact but costs a key's worth of space per
//! item. A Bloom filter answers the membership question in a handful of bits per
//! item, with one tradeoff: it never says "absent" about something present (no
//! false negatives), but it may occasionally say "present" about something absent
//! (a tunable false-positive rate). For a dedup pre-check that is exactly the
//! right shape: a "maybe" sends you to the real store, a "no" is always trusted.

use crate::authmem::sha256;

/// A Bloom filter over byte-string keys.
#[derive(Clone, Debug)]
pub struct BloomFilter {
    words: Vec<u64>,
    bits: u64,
    hashes: u32,
}

/// Two independent 64-bit hashes of a key, used for double hashing.
fn base_hashes(key: &[u8]) -> (u64, u64) {
    let d = sha256::hash(key);
    let h1 = u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
    let h2 = u64::from_be_bytes([d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]]);
    (h1, h2 | 1) // keep h2 odd so the probe sequence covers the table well
}

impl BloomFilter {
    /// Size a filter for `expected` items at a target `fp_rate` (e.g. 0.01).
    ///
    /// # Panics
    /// Panics if `fp_rate` is not in `(0, 1)` or `expected` is zero.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn with_capacity(expected: usize, fp_rate: f64) -> Self {
        assert!(expected > 0, "expected must be positive");
        assert!(fp_rate > 0.0 && fp_rate < 1.0, "fp_rate must be in (0, 1)");
        let n = expected as f64;
        let ln2 = std::f64::consts::LN_2;
        // Optimal m = -n ln p / (ln 2)^2, k = (m/n) ln 2.
        let bits = (-n * fp_rate.ln() / (ln2 * ln2)).ceil().max(64.0) as u64;
        let hashes = ((bits as f64 / n) * ln2).round().clamp(1.0, 32.0) as u32;
        let words = vec![0u64; bits.div_ceil(64) as usize];
        Self {
            words,
            bits,
            hashes,
        }
    }

    fn probe(&self, i: u32, h1: u64, h2: u64) -> u64 {
        h1.wrapping_add(u64::from(i).wrapping_mul(h2)) % self.bits
    }

    /// Record that `key` has been seen.
    pub fn insert(&mut self, key: &[u8]) {
        let (h1, h2) = base_hashes(key);
        for i in 0..self.hashes {
            let bit = self.probe(i, h1, h2);
            self.words[(bit / 64) as usize] |= 1u64 << (bit % 64);
        }
    }

    /// Whether `key` might have been seen. A `false` is definitive; a `true` is a
    /// "maybe" with the configured false-positive rate.
    #[must_use]
    pub fn contains(&self, key: &[u8]) -> bool {
        let (h1, h2) = base_hashes(key);
        (0..self.hashes).all(|i| {
            let bit = self.probe(i, h1, h2);
            self.words[(bit / 64) as usize] & (1u64 << (bit % 64)) != 0
        })
    }

    /// The number of bits in the filter.
    #[must_use]
    pub const fn bit_len(&self) -> u64 {
        self.bits
    }

    /// The number of hash probes per key.
    #[must_use]
    pub const fn hash_count(&self) -> u32 {
        self.hashes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_has_a_false_negative() {
        let mut bf = BloomFilter::with_capacity(10_000, 0.01);
        for i in 0..10_000u64 {
            bf.insert(&i.to_be_bytes());
        }
        for i in 0..10_000u64 {
            assert!(
                bf.contains(&i.to_be_bytes()),
                "inserted {i} must be present"
            );
        }
    }

    #[test]
    fn false_positive_rate_is_near_the_target() {
        let target = 0.01;
        let mut bf = BloomFilter::with_capacity(10_000, target);
        for i in 0..10_000u64 {
            bf.insert(&i.to_be_bytes());
        }
        // Probe 10k keys that were never inserted.
        let mut fps = 0;
        for i in 1_000_000u64..1_010_000 {
            if bf.contains(&i.to_be_bytes()) {
                fps += 1;
            }
        }
        let rate = f64::from(fps) / 10_000.0;
        assert!(
            rate < target * 3.0,
            "fp rate {rate} should be near {target}"
        );
    }

    #[test]
    fn an_empty_filter_contains_nothing() {
        let bf = BloomFilter::with_capacity(100, 0.01);
        assert!(!bf.contains(b"anything"));
    }

    #[test]
    fn sizing_is_reasonable() {
        let bf = BloomFilter::with_capacity(1000, 0.01);
        // ~9.6 bits/item and ~7 hashes for 1% is textbook.
        assert!(bf.bit_len() >= 9000 && bf.bit_len() <= 11000);
        assert!((6..=8).contains(&bf.hash_count()));
    }
}
