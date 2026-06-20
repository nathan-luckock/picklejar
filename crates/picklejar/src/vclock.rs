//! Vector clocks: tell a causal memory update from a concurrent one.
//!
//! The conflict-free replicated memory resolves concurrent writes by a
//! last-writer-wins rule, which is deterministic but throws information away: it
//! cannot tell whether one write genuinely came after another (and so should
//! supersede it) or whether the two happened concurrently on partitioned nodes
//! (a real conflict a human or a merge function should see). A vector clock
//! carries that distinction.
//!
//! Each node keeps a counter per node. A local event ticks its own counter;
//! receiving another node's state merges by taking the element-wise maximum. One
//! clock happens-before another when it is less than or equal in every component
//! and strictly less in at least one; if neither happens-before the other, the
//! two events are concurrent.

use std::collections::BTreeMap;

/// How two vector clocks relate in causal time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Causality {
    /// The two clocks are identical.
    Equal,
    /// This clock happened strictly before the other.
    Before,
    /// This clock happened strictly after the other.
    After,
    /// Neither happened before the other: they are concurrent (a conflict).
    Concurrent,
}

/// A vector clock: a counter per node id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VectorClock {
    counters: BTreeMap<u64, u64>,
}

impl VectorClock {
    /// A fresh clock with all counters at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counters: BTreeMap::new(),
        }
    }

    /// The counter for a node (zero if never seen).
    #[must_use]
    pub fn get(&self, node: u64) -> u64 {
        self.counters.get(&node).copied().unwrap_or(0)
    }

    /// Record a local event on `node` by advancing its counter.
    pub fn increment(&mut self, node: u64) {
        *self.counters.entry(node).or_insert(0) += 1;
    }

    /// Merge another clock in, taking the element-wise maximum. This is what a
    /// node does when it learns another node's state.
    pub fn merge(&mut self, other: &Self) {
        for (&node, &count) in &other.counters {
            let slot = self.counters.entry(node).or_insert(0);
            *slot = (*slot).max(count);
        }
    }

    /// Classify this clock's causal relationship to `other`.
    #[must_use]
    pub fn compare(&self, other: &Self) -> Causality {
        let mut less = false; // some component strictly less than other's
        let mut greater = false; // some component strictly greater
        for node in self.counters.keys().chain(other.counters.keys()) {
            let a = self.get(*node);
            let b = other.get(*node);
            if a < b {
                less = true;
            } else if a > b {
                greater = true;
            }
        }
        match (less, greater) {
            (false, false) => Causality::Equal,
            (true, false) => Causality::Before,
            (false, true) => Causality::After,
            (true, true) => Causality::Concurrent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_pair_is_equal() {
        assert_eq!(
            VectorClock::new().compare(&VectorClock::new()),
            Causality::Equal
        );
    }

    #[test]
    fn a_local_event_happens_after() {
        let before = VectorClock::new();
        let mut after = before.clone();
        after.increment(1);
        assert_eq!(after.compare(&before), Causality::After);
        assert_eq!(before.compare(&after), Causality::Before);
    }

    #[test]
    fn a_causal_chain_is_ordered() {
        // Node 1 acts, sends to node 2, which acts: the second clock is after.
        let mut a = VectorClock::new();
        a.increment(1);
        let mut b = a.clone(); // node 2 received a's state
        b.merge(&a);
        b.increment(2);
        assert_eq!(b.compare(&a), Causality::After);
    }

    #[test]
    fn concurrent_updates_are_detected() {
        // Both start from a shared state, then each acts without seeing the other.
        let mut shared = VectorClock::new();
        shared.increment(1);
        let mut a = shared.clone();
        a.increment(1); // node 1 acts again
        let mut b = shared.clone();
        b.increment(2); // node 2 acts concurrently
        assert_eq!(a.compare(&b), Causality::Concurrent);
        assert_eq!(b.compare(&a), Causality::Concurrent);
    }

    #[test]
    fn merge_takes_the_maximum_and_orders_after_both() {
        let mut a = VectorClock::new();
        a.increment(1);
        a.increment(1);
        let mut b = VectorClock::new();
        b.increment(2);
        let mut merged = a.clone();
        merged.merge(&b);
        assert_eq!(merged.get(1), 2);
        assert_eq!(merged.get(2), 1);
        assert_eq!(merged.compare(&a), Causality::After);
        assert_eq!(merged.compare(&b), Causality::After);
    }
}
