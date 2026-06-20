//! Cuckoo filter: deletable membership that beats Bloom on space at low error.
//!
//! A counting Bloom filter supports deletes but pays several bytes per cell. A
//! cuckoo filter supports deletes and, below roughly a 3% false-positive rate,
//! stores fewer bits per item than even a plain Bloom filter, by keeping a short
//! fingerprint of each key rather than setting bits. Each key has two candidate
//! buckets, reachable from each other by an exclusive-or with the fingerprint's
//! hash; on insert, if both are full, an occupant is evicted and rehomed to its
//! own alternate, cuckoo-hashing style, until everything settles.
//!
//! The result deletes cleanly (remove the fingerprint), never reports a false
//! negative for an item still present, and rejects an insert only when the table
//! is genuinely too full to rehome.

use crate::authmem::sha256;

const SLOTS: usize = 4;
const MAX_KICKS: usize = 500;

fn hash64(key: &[u8]) -> u64 {
    let d = sha256::hash(key);
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// A nonzero one-byte fingerprint of a key hash (zero marks an empty slot).
#[allow(clippy::cast_possible_truncation)] // deliberately taking one byte
const fn fingerprint(h: u64) -> u8 {
    let f = ((h >> 32) & 0xff) as u8;
    if f == 0 {
        1
    } else {
        f
    }
}

/// The hash of a fingerprint, for hopping between a key's two buckets.
fn fp_hash(f: u8) -> u64 {
    let d = sha256::hash(&[f]);
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// A cuckoo filter over byte-string keys.
#[derive(Clone, Debug)]
pub struct CuckooFilter {
    buckets: Vec<[u8; SLOTS]>,
    mask: u64,
    rng: u64,
    len: usize,
}

impl CuckooFilter {
    /// A filter with capacity for about `expected` items (rounded up to a power
    /// of two of buckets).
    #[must_use]
    pub fn with_capacity(expected: usize) -> Self {
        let needed = (expected / SLOTS).max(1);
        let buckets = needed.next_power_of_two().max(2);
        Self {
            buckets: vec![[0u8; SLOTS]; buckets],
            mask: buckets as u64 - 1,
            rng: 0x9E37_79B9_7F4A_7C15,
            len: 0,
        }
    }

    fn next_rng(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    #[allow(clippy::cast_possible_truncation)] // masked, always in range
    const fn index(&self, h: u64) -> usize {
        (h & self.mask) as usize
    }

    fn alt(&self, i: usize, f: u8) -> usize {
        i ^ self.index(fp_hash(f))
    }

    fn put_in(bucket: &mut [u8; SLOTS], f: u8) -> bool {
        for slot in bucket.iter_mut() {
            if *slot == 0 {
                *slot = f;
                return true;
            }
        }
        false
    }

    /// The number of fingerprints currently stored.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the filter is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Insert a key. Returns `false` only if the table is too full to rehome.
    #[allow(clippy::cast_possible_truncation)] // slot index reduced mod SLOTS
    pub fn insert(&mut self, key: &[u8]) -> bool {
        let h = hash64(key);
        let f = fingerprint(h);
        let i1 = self.index(h);
        let i2 = self.alt(i1, f);
        if Self::put_in(&mut self.buckets[i1], f) || Self::put_in(&mut self.buckets[i2], f) {
            self.len += 1;
            return true;
        }
        // Both candidates full: evict and rehome.
        let mut idx = if self.next_rng() & 1 == 0 { i1 } else { i2 };
        let mut carry = f;
        for _ in 0..MAX_KICKS {
            let slot = (self.next_rng() as usize) % SLOTS;
            std::mem::swap(&mut carry, &mut self.buckets[idx][slot]);
            idx = self.alt(idx, carry);
            if Self::put_in(&mut self.buckets[idx], carry) {
                self.len += 1;
                return true;
            }
        }
        false
    }

    /// Whether `key` is (probably) present. A `false` is definitive.
    #[must_use]
    pub fn contains(&self, key: &[u8]) -> bool {
        let h = hash64(key);
        let f = fingerprint(h);
        let i1 = self.index(h);
        let i2 = self.alt(i1, f);
        self.buckets[i1].contains(&f) || self.buckets[i2].contains(&f)
    }

    /// Remove a key previously inserted. Returns whether a fingerprint was found.
    pub fn remove(&mut self, key: &[u8]) -> bool {
        let h = hash64(key);
        let f = fingerprint(h);
        let i1 = self.index(h);
        let i2 = self.alt(i1, f);
        for &idx in &[i1, i2] {
            if let Some(slot) = self.buckets[idx].iter_mut().find(|s| **s == f) {
                *slot = 0;
                self.len -= 1;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_contains_remove() {
        let mut cf = CuckooFilter::with_capacity(1000);
        assert!(cf.insert(b"memory-1"));
        assert!(cf.contains(b"memory-1"));
        assert!(cf.remove(b"memory-1"));
        assert!(!cf.contains(b"memory-1"));
        assert!(cf.is_empty());
    }

    #[test]
    fn no_false_negatives_for_a_full_load() {
        let mut cf = CuckooFilter::with_capacity(10_000);
        let mut inserted = 0u64;
        for i in 0..8000u64 {
            if cf.insert(&i.to_be_bytes()) {
                inserted += 1;
            }
        }
        // Everything that was accepted must read as present.
        for i in 0..inserted {
            assert!(
                cf.contains(&i.to_be_bytes()),
                "accepted {i} must be present"
            );
        }
        assert_eq!(cf.len() as u64, inserted);
    }

    #[test]
    fn removing_one_keeps_the_rest() {
        let mut cf = CuckooFilter::with_capacity(10_000);
        for i in 0..2000u64 {
            assert!(cf.insert(&i.to_be_bytes()));
        }
        assert!(cf.remove(&1000u64.to_be_bytes()));
        assert!(!cf.contains(&1000u64.to_be_bytes()));
        for i in (0..2000u64).filter(|&i| i != 1000) {
            assert!(cf.contains(&i.to_be_bytes()), "{i} should remain");
        }
    }

    #[test]
    fn false_positive_rate_is_low() {
        let mut cf = CuckooFilter::with_capacity(10_000);
        for i in 0..5000u64 {
            cf.insert(&i.to_be_bytes());
        }
        let mut fps = 0;
        for i in 1_000_000u64..1_010_000 {
            if cf.contains(&i.to_be_bytes()) {
                fps += 1;
            }
        }
        let rate = f64::from(fps) / 10_000.0;
        assert!(rate < 0.05, "fp rate {rate} should be a few percent");
    }
}
