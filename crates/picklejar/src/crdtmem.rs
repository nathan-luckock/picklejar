//! Conflict-free replicated memory: edit while partitioned, merge without conflict.
//!
//! Two memory nodes at the edge, or in orbit, may be unable to reach each other
//! for hours. Each keeps serving and accepting writes locally. When the link
//! returns they must reconcile, and "last writer wins, but which one?" cannot
//! depend on who synced first. This is a conflict-free replicated data type: a
//! last-write-wins map of memory id to value where merge is the element-wise
//! join of a semilattice, so it is commutative, associative, and idempotent.
//! Any set of replicas that have observed the same updates, in any order, with
//! any pattern of pairwise merges, converge to the identical state.
//!
//! Order is established by a Lamport clock, with the replica id breaking ties, so
//! every replica resolves a concurrent conflict the same way without
//! coordination. Deletes are tombstones (a slot with no value) so that a delete
//! and a concurrent write resolve by the same rule as two writes.

use std::collections::BTreeMap;

/// One memory's state at a replica: its value (absent means deleted), and the
/// Lamport timestamp and origin replica that last wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    /// The value, or `None` for a tombstone (a deleted memory).
    pub value: Option<Vec<u8>>,
    /// The Lamport timestamp of the write.
    pub ts: u64,
    /// The replica that made the write, used only to break ties deterministically.
    pub origin: u64,
}

impl Slot {
    /// Whether this slot wins over `other` under the total order `(ts, origin)`.
    fn dominates(&self, other: &Self) -> bool {
        (self.ts, self.origin) > (other.ts, other.origin)
    }
}

/// One replica of the replicated memory.
#[derive(Clone, Debug)]
pub struct Replica {
    id: u64,
    clock: u64,
    slots: BTreeMap<u64, Slot>,
}

impl Replica {
    /// A fresh, empty replica with a stable id (the id must be unique per
    /// replica; it only ever breaks ties).
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self {
            id,
            clock: 0,
            slots: BTreeMap::new(),
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Write a memory locally.
    pub fn set(&mut self, mem_id: u64, value: &[u8]) {
        let ts = self.tick();
        self.slots.insert(
            mem_id,
            Slot {
                value: Some(value.to_vec()),
                ts,
                origin: self.id,
            },
        );
    }

    /// Delete a memory locally, leaving a tombstone so the delete itself merges.
    pub fn remove(&mut self, mem_id: u64) {
        let ts = self.tick();
        self.slots.insert(
            mem_id,
            Slot {
                value: None,
                ts,
                origin: self.id,
            },
        );
    }

    /// Read a live memory, or `None` if it is absent or tombstoned.
    #[must_use]
    pub fn get(&self, mem_id: u64) -> Option<&[u8]> {
        self.slots.get(&mem_id).and_then(|s| s.value.as_deref())
    }

    /// The ids of all live (non-tombstoned) memories.
    #[must_use]
    pub fn live_ids(&self) -> Vec<u64> {
        self.slots
            .iter()
            .filter(|(_, s)| s.value.is_some())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Merge another replica's state into this one. This is the join: for every
    /// memory, keep the slot that wins the `(ts, origin)` order. Commutative,
    /// associative, and idempotent.
    pub fn merge(&mut self, other: &Self) {
        for (id, their) in &other.slots {
            let keep = match self.slots.get(id) {
                Some(mine) if mine.dominates(their) => continue,
                _ => their.clone(),
            };
            self.slots.insert(*id, keep);
        }
        // Advance the clock past anything we have now seen, so a later local
        // write is ordered after the writes we just merged.
        self.clock = self.clock.max(other.clock);
    }

    /// Whether two replicas have converged to the same observable state.
    #[must_use]
    pub fn converged_with(&self, other: &Self) -> bool {
        self.slots == other.slots
    }

    /// The merged-state map, for inspection in tests and demos.
    #[must_use]
    pub const fn slots(&self) -> &BTreeMap<u64, Slot> {
        &self.slots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic generator for the randomized convergence test.
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

    #[test]
    fn a_partitioned_edit_merges() {
        let mut a = Replica::new(1);
        let mut b = Replica::new(2);
        a.set(10, b"from A");
        b.set(20, b"from B");
        a.merge(&b);
        b.merge(&a);
        assert!(a.converged_with(&b));
        assert_eq!(a.get(10), Some(&b"from A"[..]));
        assert_eq!(a.get(20), Some(&b"from B"[..]));
    }

    #[test]
    fn merge_is_idempotent() {
        let mut a = Replica::new(1);
        a.set(1, b"x");
        let before = a.slots().clone();
        let snapshot = a.clone();
        a.merge(&snapshot);
        a.merge(&snapshot);
        assert_eq!(*a.slots(), before);
    }

    #[test]
    fn merge_is_commutative() {
        let mut a = Replica::new(1);
        let mut b = Replica::new(2);
        a.set(1, b"a1");
        a.set(2, b"a2");
        b.set(2, b"b2");
        b.set(3, b"b3");

        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b.clone();
        ba.merge(&a);
        assert_eq!(ab.slots(), ba.slots(), "merge order must not matter");
    }

    #[test]
    fn a_concurrent_conflict_resolves_the_same_everywhere() {
        // Both write memory 1 at the same Lamport time; the higher origin wins,
        // and every replica agrees without coordination.
        let mut a = Replica::new(1);
        let mut b = Replica::new(2);
        a.set(1, b"value from A");
        b.set(1, b"value from B");
        let mut x = a.clone();
        x.merge(&b);
        let mut y = b.clone();
        y.merge(&a);
        assert!(x.converged_with(&y));
        assert_eq!(
            x.get(1),
            Some(&b"value from B"[..]),
            "origin 2 breaks the tie"
        );
    }

    #[test]
    fn a_delete_and_a_write_merge_by_timestamp() {
        let mut a = Replica::new(1);
        a.set(1, b"alive");
        let mut b = a.clone();
        // b deletes after a's write; b's tombstone is later, so it wins.
        b.remove(1);
        a.merge(&b);
        assert_eq!(a.get(1), None, "the later delete wins");
    }

    #[test]
    fn three_replicas_converge_under_random_merges() {
        let mut rng = Rng(0x0CD7_5EED);
        let mut reps = [Replica::new(1), Replica::new(2), Replica::new(3)];

        // Random local edits across the replicas.
        for _ in 0..200 {
            let r = (rng.next() % 3) as usize;
            let mem = rng.next() % 8;
            if rng.next() % 5 == 0 {
                reps[r].remove(mem);
            } else {
                let v = rng.next().to_be_bytes();
                reps[r].set(mem, &v);
            }
        }

        // Gossip: merge random pairs many times, in arbitrary order.
        for _ in 0..200 {
            let i = (rng.next() % 3) as usize;
            let j = (rng.next() % 3) as usize;
            if i != j {
                let src = reps[j].clone();
                reps[i].merge(&src);
            }
        }
        // A final full round so everyone has seen everyone.
        for _ in 0..3 {
            let a = reps[0].clone();
            let b = reps[1].clone();
            let c = reps[2].clone();
            reps[0].merge(&b);
            reps[0].merge(&c);
            reps[1].merge(&a);
            reps[1].merge(&c);
            reps[2].merge(&a);
            reps[2].merge(&b);
        }

        assert!(reps[0].converged_with(&reps[1]));
        assert!(reps[1].converged_with(&reps[2]));
    }
}
