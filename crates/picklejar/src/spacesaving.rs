//! Space-Saving: the top-K most-accessed memories in bounded space.
//!
//! A node wants the handful of hottest memories to keep resident, but cannot
//! afford a counter per distinct item over a long stream. Space-Saving keeps only
//! `k` counters. A hit on a tracked memory increments it; a hit on an untracked
//! one, when the table is full, evicts the currently-coldest entry and takes over
//! its slot, inheriting its count as an upper bound on its own error. The effect
//! is that a genuinely hot memory can never be pushed out, while cold one-offs
//! churn through the weakest slot.
//!
//! The guarantee: any memory whose true frequency exceeds `total / k` is
//! definitely tracked, and every tracked count is an overestimate by no more than
//! the recorded error, so the heavy hitters are found with a bound on the slack.

use std::collections::HashMap;

/// A tracked memory's estimated count and the maximum it may overestimate by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tracked {
    /// The estimated access count (an upper bound on the truth).
    pub count: u64,
    /// The most this estimate may exceed the true count.
    pub error: u64,
}

/// A Space-Saving summary tracking the heaviest `capacity` keys.
#[derive(Clone, Debug)]
pub struct SpaceSaving {
    capacity: usize,
    counters: HashMap<Vec<u8>, Tracked>,
    total: u64,
}

impl SpaceSaving {
    /// A summary that tracks at most `capacity` keys.
    ///
    /// # Panics
    /// Panics if `capacity` is zero.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be positive");
        Self {
            capacity,
            counters: HashMap::new(),
            total: 0,
        }
    }

    /// Record one access of `key`.
    pub fn offer(&mut self, key: &[u8]) {
        self.total += 1;
        if let Some(t) = self.counters.get_mut(key) {
            t.count += 1;
            return;
        }
        if self.counters.len() < self.capacity {
            self.counters
                .insert(key.to_vec(), Tracked { count: 1, error: 0 });
            return;
        }
        // Full: evict the coldest entry and take its slot.
        let victim = self
            .counters
            .iter()
            .min_by_key(|(_, t)| t.count)
            .map(|(k, t)| (k.clone(), t.count))
            .expect("non-empty when full");
        self.counters.remove(&victim.0);
        // The new key inherits the evicted count as its count and its error.
        self.counters.insert(
            key.to_vec(),
            Tracked {
                count: victim.1 + 1,
                error: victim.1,
            },
        );
    }

    /// The estimated count for `key`, or `None` if it is not tracked.
    #[must_use]
    pub fn estimate(&self, key: &[u8]) -> Option<Tracked> {
        self.counters.get(key).copied()
    }

    /// The `n` heaviest tracked memories, highest count first.
    #[must_use]
    pub fn top(&self, n: usize) -> Vec<(Vec<u8>, Tracked)> {
        let mut all: Vec<(Vec<u8>, Tracked)> =
            self.counters.iter().map(|(k, t)| (k.clone(), *t)).collect();
        all.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0)));
        all.truncate(n);
        all
    }

    /// The total number of accesses offered.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heavy_hitters_survive_a_flood_of_cold_keys() {
        let mut ss = SpaceSaving::with_capacity(16);
        // Three hot keys, hit often, amid a flood of unique cold keys.
        for round in 0..2000u64 {
            ss.offer(b"hot-A");
            if round % 2 == 0 {
                ss.offer(b"hot-B");
            }
            if round % 4 == 0 {
                ss.offer(b"hot-C");
            }
            ss.offer(&round.to_be_bytes()); // a unique cold key each round
        }
        let top = ss.top(3);
        let keys: Vec<&[u8]> = top.iter().map(|(k, _)| k.as_slice()).collect();
        assert!(keys.contains(&b"hot-A".as_slice()), "hot-A must be tracked");
        assert!(keys.contains(&b"hot-B".as_slice()), "hot-B must be tracked");
        assert!(keys.contains(&b"hot-C".as_slice()), "hot-C must be tracked");
    }

    #[test]
    fn estimates_never_fall_below_the_truth() {
        let mut ss = SpaceSaving::with_capacity(8);
        for _ in 0..500 {
            ss.offer(b"frequent");
        }
        let est = ss.estimate(b"frequent").expect("tracked");
        // True count is 500; the estimate is an upper bound.
        assert!(est.count >= 500);
        assert!(
            est.count - est.error <= 500,
            "true count within [count-error, count]"
        );
    }

    #[test]
    fn a_frequency_above_total_over_k_is_always_tracked() {
        let mut ss = SpaceSaving::with_capacity(4);
        // "whale" gets 60% of all traffic; with k=4 and threshold total/4=25%,
        // it must be tracked.
        for i in 0..1000u64 {
            ss.offer(b"whale");
            ss.offer(&i.to_be_bytes()); // 1000 distinct cold keys
        }
        assert!(
            ss.estimate(b"whale").is_some(),
            "a dominant key must be tracked"
        );
    }
}
