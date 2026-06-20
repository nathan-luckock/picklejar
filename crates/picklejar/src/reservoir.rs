//! Reservoir sampling: a uniform sample of a memory stream of unknown length.
//!
//! To audit or summarize a tenant's memories you often want a uniform random
//! sample, but the stream's length is not known in advance and it is too large to
//! hold or to scan twice. Reservoir sampling (Algorithm R) keeps a fixed-size
//! reservoir in one pass: the first `k` items fill it, and the `i`-th item
//! thereafter replaces a random reservoir slot with probability `k / i`. At every
//! point the reservoir is a uniform random sample of everything seen so far, in
//! `O(k)` space and one pass.

/// A fixed-size uniform sample drawn from a stream in a single pass.
#[derive(Clone, Debug)]
pub struct Reservoir {
    capacity: usize,
    items: Vec<u64>,
    seen: u64,
    rng: u64,
}

impl Reservoir {
    /// A reservoir holding up to `capacity` items, with a seed for reproducible
    /// sampling (a real sampler seeds from entropy).
    ///
    /// # Panics
    /// Panics if `capacity` is zero.
    #[must_use]
    pub fn with_capacity(capacity: usize, seed: u64) -> Self {
        assert!(capacity > 0, "capacity must be positive");
        Self {
            capacity,
            items: Vec::with_capacity(capacity),
            seen: 0,
            rng: seed | 1,
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

    /// Offer the next stream item to the sample.
    #[allow(clippy::cast_possible_truncation)] // j is reduced mod seen, fits usize
    pub fn offer(&mut self, item: u64) {
        self.seen += 1;
        if self.items.len() < self.capacity {
            self.items.push(item);
            return;
        }
        // Replace a random slot with probability capacity / seen.
        let j = self.next_rng() % self.seen;
        if (j as usize) < self.capacity {
            self.items[j as usize] = item;
        }
    }

    /// The current sample.
    #[must_use]
    pub fn sample(&self) -> &[u64] {
        &self.items
    }

    /// How many items have been offered.
    #[must_use]
    pub const fn seen(&self) -> u64 {
        self.seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_exceeds_capacity_and_fills_when_possible() {
        let mut r = Reservoir::with_capacity(10, 1);
        for i in 0..5u64 {
            r.offer(i);
        }
        assert_eq!(r.sample().len(), 5, "under capacity holds all");
        for i in 5..1000u64 {
            r.offer(i);
        }
        assert_eq!(r.sample().len(), 10, "never exceeds capacity");
        assert_eq!(r.seen(), 1000);
    }

    #[test]
    fn the_sample_is_drawn_from_the_stream() {
        let mut r = Reservoir::with_capacity(8, 42);
        for i in 0..500u64 {
            r.offer(i);
        }
        for &x in r.sample() {
            assert!(x < 500, "every sampled value came from the stream");
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn selection_is_approximately_uniform() {
        // With k=1 over n=100, each item should be the chosen sample about 1% of
        // the time. Count how often a fixed item (id 42) is selected across trials.
        let n = 100u64;
        let trials = 20_000u64;
        let target = 42u64;
        let mut hits = 0u64;
        for t in 0..trials {
            let mut r = Reservoir::with_capacity(1, t.wrapping_mul(2_654_435_761) | 1);
            for i in 0..n {
                r.offer(i);
            }
            if r.sample() == [target] {
                hits += 1;
            }
        }
        let rate = hits as f64 / trials as f64;
        let expected = 1.0 / n as f64;
        assert!(
            (rate - expected).abs() < expected * 0.5,
            "rate {rate} should be near {expected}"
        );
    }
}
