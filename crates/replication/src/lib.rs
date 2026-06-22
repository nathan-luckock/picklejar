//! AP (Dynamo-style) replication for the picklejar memory layer.
//!
//! A single picklejar node survives disk faults: corruption, crashes, radiation.
//! It cannot survive being *gone*, or a network that splits it from its peers.
//! This layer turns the node into a cluster of replicas that keep serving through
//! a partition and converge once the link returns, which is the right model for
//! intermittent, unreachable hardware (a satellite constellation, an edge fleet).
//!
//! The design is eventual-consistency (AP in CAP), built from the engine's own
//! from-scratch parts. Each node holds a conflict-free replicated memory store
//! ([`picklejar::crdtmem::Replica`]): a last-write-wins map whose merge is a
//! semilattice join, so replicas that saw the same writes in any order, with any
//! pattern of pairwise merges, converge to the identical state. On top sit
//! placement (which replicas own a key), an availability-first quorum write/read
//! path, and anti-entropy repair after a partition heals.
//!
//! The point is the proof, not the demo: [`run_seed`] drives random writes under
//! random partitions and heals, then asserts every node converges, so a failure
//! is a single `u64` seed you replay exactly. The same philosophy as the
//! single-node crash simulator, now across a cluster.

use picklejar::antientropy::MerkleSet;
use picklejar::crdtmem::{Replica, Slot};

/// A deterministic xorshift64 PRNG, so a simulation run replays from its seed.
#[derive(Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded by `seed` (a zero seed is nudged to a valid state).
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    /// The next pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A pseudo-random value in `0..n` (returns 0 when `n` is 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

/// A strong scramble of a key into a placement position.
const fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// What happened to a write under availability-first semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Acknowledged by at least the write quorum `w` replicas.
    Durable(usize),
    /// Accepted by at least one but fewer than `w` replicas (a partition): the
    /// write is live locally and will converge when the link returns.
    Degraded(usize),
    /// No responsible replica was reachable; the write did not land.
    Unavailable,
}

impl WriteOutcome {
    /// Whether the write was accepted anywhere (durable or degraded).
    #[must_use]
    pub const fn accepted(self) -> bool {
        matches!(self, Self::Durable(_) | Self::Degraded(_))
    }
}

/// A cluster of replicated memory nodes with a controllable partition state.
///
/// Nodes carry a partition-group id; two nodes can communicate only when they
/// share a group. A fresh cluster is fully connected (every node in group 0).
#[derive(Debug)]
pub struct Cluster {
    nodes: Vec<Replica>,
    group: Vec<u32>,
    rf: usize,
    r: usize,
    w: usize,
}

impl Cluster {
    /// A cluster of `n` nodes with replication factor `rf` and read/write quorums
    /// `r` / `w`.
    ///
    /// # Panics
    /// Panics if `n` is zero or `rf` is not in `1..=n`.
    #[must_use]
    pub fn new(n: usize, rf: usize, r: usize, w: usize) -> Self {
        assert!(n > 0, "a cluster needs at least one node");
        assert!(rf > 0 && rf <= n, "replication factor must be in 1..=n");
        let nodes = (0..n).map(|i| Replica::new(i as u64)).collect();
        Self {
            nodes,
            group: vec![0; n],
            rf,
            r,
            w,
        }
    }

    /// The number of nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the cluster has no nodes (never true after [`Cluster::new`]).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Read-only access to a node's replica.
    #[must_use]
    pub fn node(&self, i: usize) -> &Replica {
        &self.nodes[i]
    }

    /// The replication factor (replicas per key).
    #[must_use]
    pub const fn replication_factor(&self) -> usize {
        self.rf
    }

    /// The read quorum (replicas a quorum-strength read consults).
    #[must_use]
    pub const fn read_quorum(&self) -> usize {
        self.r
    }

    /// The write quorum (acks for a durable write).
    #[must_use]
    pub const fn write_quorum(&self) -> usize {
        self.w
    }

    fn reachable(&self, a: usize, b: usize) -> bool {
        self.group[a] == self.group[b]
    }

    /// The `rf` replica indices responsible for `key`: a preference list of the
    /// primary (by hash) and its successors around the ring of node indices.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn replicas(&self, key: u64) -> Vec<usize> {
        let n = self.nodes.len();
        let primary = (splitmix64(key) % n as u64) as usize;
        (0..self.rf).map(|i| (primary + i) % n).collect()
    }

    /// Write `value` for `key`, coordinated by node `coord`. Availability-first:
    /// the write is applied to every responsible replica the coordinator can
    /// currently reach, succeeding (possibly degraded) as long as one is reachable.
    pub fn write(&mut self, coord: usize, key: u64, value: &[u8]) -> WriteOutcome {
        let mut acks = 0;
        for r in self.replicas(key) {
            if self.reachable(coord, r) {
                self.nodes[r].set(key, value);
                acks += 1;
            }
        }
        if acks == 0 {
            WriteOutcome::Unavailable
        } else if acks >= self.w {
            WriteOutcome::Durable(acks)
        } else {
            WriteOutcome::Degraded(acks)
        }
    }

    /// Read `key`, coordinated by node `coord`, reconciling the reachable
    /// replicas' views at read time. Returns `None` if no replica is reachable
    /// or the key is absent/tombstoned in the merged view. The read is at quorum
    /// strength when at least `r` replicas answered.
    #[must_use]
    pub fn read(&self, coord: usize, key: u64) -> Option<Vec<u8>> {
        let reachable: Vec<usize> = self
            .replicas(key)
            .into_iter()
            .filter(|&r| self.reachable(coord, r))
            .collect();
        let (first, rest) = reachable.split_first()?;
        let mut view = self.nodes[*first].clone();
        for &r in rest {
            view.merge(&self.nodes[r]);
        }
        view.get(key).map(<[u8]>::to_vec)
    }

    /// Assign each node to a partition group; nodes in different groups cannot
    /// communicate. Models a network split.
    ///
    /// # Panics
    /// Panics if `groups` does not have one id per node.
    pub fn set_partitions(&mut self, groups: &[u32]) {
        assert_eq!(groups.len(), self.nodes.len(), "one group id per node");
        self.group.copy_from_slice(groups);
    }

    /// Heal every partition: all nodes can reach all nodes again.
    pub fn heal(&mut self) {
        for g in &mut self.group {
            *g = 0;
        }
    }

    /// Reconcile the cluster by Merkle-diff anti-entropy, run to a fixed point.
    ///
    /// Every pair of nodes that can currently reach each other builds a Merkle
    /// tree over its memories and descends only where hashes differ, so it ships
    /// exactly the slots that diverged, not the whole replica. Returns the number
    /// of slot transfers performed (the work saved versus a full-state sync).
    /// Within each connected partition group, the nodes converge.
    #[must_use]
    pub fn anti_entropy(&mut self) -> usize {
        let mut transfers = 0usize;
        loop {
            let trees: Vec<MerkleSet> = self.nodes.iter().map(node_merkle).collect();
            let mut changed = false;
            for a in 0..self.nodes.len() {
                for b in (a + 1)..self.nodes.len() {
                    if !self.reachable(a, b) {
                        continue;
                    }
                    let (keys, _compares) = trees[a].diff(&trees[b]);
                    for key in keys {
                        let from_a = self.nodes[a].slots().get(&key).cloned();
                        let from_b = self.nodes[b].slots().get(&key).cloned();
                        if let Some(slot) = from_b {
                            self.nodes[a].merge_slot(key, slot);
                        }
                        if let Some(slot) = from_a {
                            self.nodes[b].merge_slot(key, slot);
                        }
                        transfers += 1;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        transfers
    }

    /// Whether every node has converged to the identical observable state. Use
    /// after a heal and anti-entropy to assert global convergence.
    #[must_use]
    pub fn fully_converged(&self) -> bool {
        self.nodes.windows(2).all(|w| w[0].converged_with(&w[1]))
    }
}

/// Tree depth for anti-entropy reconciliation (2^6 = 64 buckets), sized for the
/// cluster's working set; the diff cost is proportional to differences, not size.
const MERKLE_DEPTH: u32 = 6;

/// Build a Merkle tree over a replica's memories, keyed by memory id with each
/// leaf hashing the slot's full identity (value, timestamp, origin) so a
/// diverged slot produces a diverged hash.
fn node_merkle(replica: &Replica) -> MerkleSet {
    let entries: Vec<(u64, Vec<u8>)> = replica
        .slots()
        .iter()
        .map(|(id, slot)| (*id, encode_slot(slot)))
        .collect();
    MerkleSet::from_entries(MERKLE_DEPTH, &entries)
}

/// Encode a slot to the bytes its Merkle leaf hashes: timestamp, origin, and the
/// value (or a tombstone marker), so any change to the slot changes the hash.
fn encode_slot(slot: &Slot) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(17);
    bytes.extend_from_slice(&slot.ts.to_le_bytes());
    bytes.extend_from_slice(&slot.origin.to_le_bytes());
    match &slot.value {
        Some(v) => {
            bytes.push(1);
            bytes.extend_from_slice(v);
        }
        None => bytes.push(0),
    }
    bytes
}

/// The result of one seeded simulation run.
#[derive(Debug, Clone, Copy)]
pub struct SimReport {
    /// The seed that produced this run (replay it to reproduce exactly).
    pub seed: u64,
    /// Number of nodes.
    pub nodes: usize,
    /// Operations applied.
    pub ops: usize,
    /// Writes issued.
    pub writes: usize,
    /// Partition events induced.
    pub partitions: usize,
    /// Slot transfers performed by anti-entropy (Merkle ships only diffs).
    pub transfers: usize,
    /// Whether all nodes converged after the final heal and anti-entropy.
    pub converged: bool,
}

/// Run one deterministic simulation and report whether the cluster converged.
///
/// Random writes through random coordinators, random partitions and heals, and
/// opportunistic anti-entropy, then a final heal and anti-entropy. The
/// invariant: the cluster always converges.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn run_seed(seed: u64, nodes: usize, ops: usize) -> SimReport {
    let mut rng = Rng::new(seed);
    let rf = 3.min(nodes);
    let quorum = (rf / 2) + 1;
    let mut cluster = Cluster::new(nodes, rf, quorum, quorum);
    let keyspace = 24u64;
    let (mut writes, mut partitions, mut transfers) = (0usize, 0usize, 0usize);

    for _ in 0..ops {
        match rng.below(100) {
            0..=69 => {
                let coord = (rng.below(nodes as u64)) as usize;
                let key = rng.below(keyspace);
                let value = rng.next_u64().to_le_bytes();
                cluster.write(coord, key, &value);
                writes += 1;
            }
            70..=81 => {
                let groups: Vec<u32> = (0..nodes).map(|_| rng.below(2) as u32).collect();
                cluster.set_partitions(&groups);
                partitions += 1;
            }
            82..=90 => cluster.heal(),
            _ => transfers += cluster.anti_entropy(),
        }
    }

    cluster.heal();
    transfers += cluster.anti_entropy();
    SimReport {
        seed,
        nodes,
        ops,
        writes,
        partitions,
        transfers,
        converged: cluster.fully_converged(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_under_partition_then_converges() {
        // 3 replicas of every key. Split into {0} | {1,2}.
        let mut c = Cluster::new(3, 3, 2, 2);
        c.set_partitions(&[0, 1, 1]);
        // Both sides accept a write to the same key: the minority degraded, the
        // majority durable. Neither is rejected.
        let a = c.write(0, 5, b"left");
        let b = c.write(1, 5, b"right");
        assert!(a.accepted(), "minority side stays available: {a:?}");
        assert!(b.accepted(), "majority side stays available: {b:?}");

        // Heal and reconcile: every node converges to the same value.
        c.heal();
        let _ = c.anti_entropy();
        assert!(c.fully_converged());
        let v0 = c.node(0).get(5).map(<[u8]>::to_vec);
        let v2 = c.node(2).get(5).map(<[u8]>::to_vec);
        assert_eq!(v0, v2, "all replicas agree on the resolved value");
        assert!(v0.is_some(), "the key survived the partition");
    }

    #[test]
    fn concurrent_writes_resolve_identically_on_every_node() {
        let mut c = Cluster::new(5, 3, 2, 2);
        // Partition so the replicas of key 7 land on both sides.
        c.set_partitions(&[0, 0, 1, 1, 1]);
        for i in 0..20u64 {
            let coord = (i % 5) as usize;
            c.write(coord, i % 8, &i.to_le_bytes());
        }
        c.heal();
        let _ = c.anti_entropy();
        assert!(c.fully_converged(), "the whole cluster converges");
    }

    #[test]
    fn merkle_repair_ships_only_what_diverged() {
        // Two nodes, both replicas of every key. Fill 200 keys and sync.
        let mut c = Cluster::new(2, 2, 1, 1);
        for k in 0..200u64 {
            c.write(0, k, &k.to_le_bytes());
        }
        let _ = c.anti_entropy();
        assert!(c.fully_converged(), "in sync after the initial fill");

        // Diverge exactly one key on node 1 only (split so node 0 misses it).
        c.set_partitions(&[0, 1]);
        c.write(1, 5, b"changed");
        c.heal();

        // Anti-entropy ships only the one diverged slot, not all 200.
        let transfers = c.anti_entropy();
        assert!(c.fully_converged());
        assert!(
            transfers <= 2,
            "Merkle should sync only the diverged key, not the whole set; got {transfers}"
        );
        assert_eq!(c.node(0).get(5), Some(b"changed".as_slice()));
    }

    #[test]
    fn many_seeds_all_converge() {
        // A representative sweep for the gate; repsim runs the headline volume.
        for seed in 0..200u64 {
            let report = run_seed(seed, 5, 200);
            assert!(report.converged, "seed {seed} diverged: {report:?}");
        }
    }
}
