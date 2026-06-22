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
use picklejar::consistenthash::HashRing;
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

/// A cluster of replicated memory nodes with controllable partition and
/// membership state.
///
/// Placement is consistent-hashed: each key's replicas are its preference list
/// on a [`HashRing`] of the live nodes, so a node joining or leaving moves only
/// its share of keys. Nodes carry a partition-group id (two nodes communicate
/// only when they share a group and both are up) and a membership status. A
/// per-observer heartbeat detector tracks who each node has lately heard from.
#[derive(Debug)]
pub struct Cluster {
    nodes: Vec<Replica>,
    group: Vec<u32>,
    up: Vec<bool>,
    last_heard: Vec<Vec<u64>>,
    ring: HashRing,
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
        let mut ring = HashRing::new(64);
        for i in 0..n {
            ring.add_node(&i.to_string());
        }
        Self {
            nodes,
            group: vec![0; n],
            up: vec![true; n],
            last_heard: vec![vec![0; n]; n],
            ring,
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
        self.up[a] && self.up[b] && self.group[a] == self.group[b]
    }

    /// The replica indices responsible for `key`: its preference list on the
    /// consistent-hash ring of the live nodes. A membership change moves only
    /// the keys whose responsible node changed, not all of them.
    #[must_use]
    pub fn replicas(&self, key: u64) -> Vec<usize> {
        self.ring
            .preference(&key.to_be_bytes(), self.rf)
            .into_iter()
            .filter_map(|name| name.parse::<usize>().ok())
            .collect()
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

    /// Crash a node: it stops serving and leaves the placement ring, so its keys
    /// shift to their successors. Its stored data persists (a crashed node keeps
    /// its disk); [`Cluster::restart`] rejoins it.
    pub fn crash(&mut self, node: usize) {
        if self.up[node] {
            self.up[node] = false;
            self.ring.remove_node(&node.to_string());
        }
    }

    /// Restart a crashed node: it rejoins the ring and serves again. Anti-entropy
    /// catches it up on the writes it missed while down.
    pub fn restart(&mut self, node: usize) {
        if !self.up[node] {
            self.up[node] = true;
            self.ring.add_node(&node.to_string());
        }
    }

    /// Whether a node is currently a live member.
    #[must_use]
    pub fn is_up(&self, node: usize) -> bool {
        self.up[node]
    }

    /// The number of live nodes.
    #[must_use]
    pub fn alive_count(&self) -> usize {
        self.up.iter().filter(|&&u| u).count()
    }

    /// One heartbeat round: every live node refreshes, in its own view, the time
    /// it last heard from each node it can currently reach. A node it cannot
    /// reach (crashed, or across a partition) goes stale, which is how failure
    /// detection works.
    pub fn heartbeat(&mut self, now: u64) {
        for o in 0..self.nodes.len() {
            if !self.up[o] {
                continue;
            }
            for t in 0..self.nodes.len() {
                if self.reachable(o, t) {
                    self.last_heard[o][t] = now;
                }
            }
        }
    }

    /// The nodes `observer` suspects have failed: those it has not heard from
    /// within `timeout`. Catches a crash and, indistinguishably, the far side of
    /// a partition, which is the honest limit of failure detection.
    #[must_use]
    pub fn suspects(&self, observer: usize, now: u64, timeout: u64) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&t| {
                t != observer && now.saturating_sub(self.last_heard[observer][t]) > timeout
            })
            .collect()
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

    /// Whether every live node has converged to the identical observable state.
    /// Use after a heal and anti-entropy to assert global convergence. A crashed
    /// node is frozen and excluded until it restarts.
    #[must_use]
    pub fn fully_converged(&self) -> bool {
        let live: Vec<&Replica> = (0..self.nodes.len())
            .filter(|&i| self.up[i])
            .map(|i| &self.nodes[i])
            .collect();
        live.windows(2).all(|w| w[0].converged_with(w[1]))
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
            0..=64 => {
                let coord = (rng.below(nodes as u64)) as usize;
                if cluster.is_up(coord) {
                    let key = rng.below(keyspace);
                    let value = rng.next_u64().to_le_bytes();
                    cluster.write(coord, key, &value);
                    writes += 1;
                }
            }
            65..=74 => {
                let groups: Vec<u32> = (0..nodes).map(|_| rng.below(2) as u32).collect();
                cluster.set_partitions(&groups);
                partitions += 1;
            }
            75..=82 => cluster.heal(),
            83..=88 => {
                let node = (rng.below(nodes as u64)) as usize;
                if cluster.alive_count() > rf {
                    cluster.crash(node);
                }
            }
            89..=93 => {
                let node = (rng.below(nodes as u64)) as usize;
                cluster.restart(node);
            }
            _ => transfers += cluster.anti_entropy(),
        }
    }

    // Final reconciliation: bring every node back, heal the network, converge.
    for i in 0..nodes {
        cluster.restart(i);
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

    #[test]
    fn survives_a_crash_and_rejoin() {
        let mut c = Cluster::new(5, 3, 2, 2);
        for k in 0..30u64 {
            c.write((k % 5) as usize, k, &k.to_le_bytes());
        }
        let _ = c.anti_entropy();

        // Crash a node; the cluster keeps taking writes, routed around it.
        c.crash(2);
        assert!(!c.is_up(2));
        for k in 30..60u64 {
            c.write(0, k, &k.to_le_bytes());
        }

        // It rejoins and anti-entropy catches it up on everything it missed.
        c.restart(2);
        let _ = c.anti_entropy();
        assert!(c.fully_converged());
        assert_eq!(
            c.node(2).get(45),
            Some(45u64.to_le_bytes().as_slice()),
            "the rejoined node caught up on writes from while it was down"
        );
    }

    #[test]
    fn crash_is_detected_by_the_heartbeat() {
        let mut c = Cluster::new(4, 3, 2, 2);
        c.heartbeat(10);
        assert!(c.suspects(0, 10, 5).is_empty(), "all freshly heard");
        c.crash(2);
        c.heartbeat(20); // node 2 is silent now; others refresh
        let suspects = c.suspects(0, 20, 5);
        assert!(suspects.contains(&2), "the crashed node is suspected");
        assert!(!suspects.contains(&1), "a live node is not suspected");
    }

    #[test]
    fn a_partition_looks_like_failure_then_clears() {
        let mut c = Cluster::new(4, 3, 2, 2);
        c.heartbeat(10);
        c.set_partitions(&[0, 0, 1, 1]); // {0,1} | {2,3}
        c.heartbeat(20); // node 0 can only hear node 1
        let suspects = c.suspects(0, 20, 5);
        assert!(
            suspects.contains(&2) && suspects.contains(&3),
            "the far side of a partition is suspected, like a failure"
        );
        assert!(!suspects.contains(&1), "the same-side peer is fine");

        c.heal();
        c.heartbeat(30);
        assert!(
            c.suspects(0, 30, 5).is_empty(),
            "suspicion clears once the link returns"
        );
    }
}
