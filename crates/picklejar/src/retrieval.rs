//! Proof of retrievability: make an unreachable node prove it still has your data.
//!
//! You cannot walk up to an orbital disk and check that your memories are still
//! there. A node could silently drop or rot chunks for months and you would not
//! know until you asked for one and it was gone. This module turns that into a
//! cheap, frequent audit: the client pins a commitment to the stored chunks, then
//! issues a random challenge for a handful of them. The node must return each
//! challenged chunk with a proof that it hashes into the pinned commitment. A
//! node that lost a chunk cannot answer a challenge that lands on it.
//!
//! The audit is probabilistic and that is the point: the client never downloads
//! everything, just spot-checks a few chunks. Challenging `q` random chunks
//! catches a node missing a fraction `f` of them with probability `1 - (1 - f)^q`,
//! which climbs to near-certainty after a few rounds. The challenge must be fresh
//! and unpredictable so the node cannot keep only the chunks it expects to be
//! asked about.

use crate::authmem::sha256;

const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;

fn leaf_hash(chunk: &[u8]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(1 + chunk.len());
    buf.push(LEAF_PREFIX);
    buf.extend_from_slice(chunk);
    sha256::hash(&buf)
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 65];
    buf[0] = NODE_PREFIX;
    buf[1..33].copy_from_slice(left);
    buf[33..65].copy_from_slice(right);
    sha256::hash(&buf)
}

fn parent_level(level: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let mut next = Vec::with_capacity(level.len().div_ceil(2));
    let mut i = 0;
    while i < level.len() {
        let left = &level[i];
        let right = if i + 1 < level.len() {
            &level[i + 1]
        } else {
            left
        };
        next.push(node_hash(left, right));
        i += 2;
    }
    next
}

fn root_of(leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        return sha256::hash(b"empty store");
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        level = parent_level(&level);
    }
    level[0]
}

/// The client's pinned commitment to a stored set of chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Commitment(pub [u8; 32]);

/// One challenged chunk and its inclusion proof, returned by the node.
#[derive(Clone, Debug)]
pub struct ChunkProof {
    /// The chunk's index in the store.
    pub index: usize,
    /// The chunk bytes the node claims to hold.
    pub chunk: Vec<u8>,
    /// The sibling hashes from this leaf up to the root.
    pub siblings: Vec<[u8; 32]>,
}

/// The node holding the data (the prover).
#[derive(Clone, Debug)]
pub struct Store {
    chunks: Vec<Vec<u8>>,
}

impl Store {
    /// Build a store over the chunks the client uploaded.
    #[must_use]
    pub const fn new(chunks: Vec<Vec<u8>>) -> Self {
        Self { chunks }
    }

    /// The number of stored chunks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// The commitment the client pins after upload.
    #[must_use]
    pub fn commit(&self) -> Commitment {
        let leaves: Vec<[u8; 32]> = self.chunks.iter().map(|c| leaf_hash(c)).collect();
        Commitment(root_of(&leaves))
    }

    /// Answer a challenge: return each requested chunk with its inclusion proof.
    /// A node missing a chunk cannot produce a valid proof for it.
    #[must_use]
    pub fn answer(&self, challenge: &[usize]) -> Vec<ChunkProof> {
        let leaves: Vec<[u8; 32]> = self.chunks.iter().map(|c| leaf_hash(c)).collect();
        challenge
            .iter()
            .filter_map(|&index| {
                let chunk = self.chunks.get(index)?.clone();
                Some(ChunkProof {
                    index,
                    chunk,
                    siblings: prove(&leaves, index),
                })
            })
            .collect()
    }

    /// Corrupt a chunk in place, for tests and the demo (a stand-in for bit-rot
    /// or a silent drop).
    pub fn corrupt(&mut self, index: usize, replacement: &[u8]) {
        if let Some(slot) = self.chunks.get_mut(index) {
            *slot = replacement.to_vec();
        }
    }
}

fn prove(leaves: &[[u8; 32]], index: usize) -> Vec<[u8; 32]> {
    let mut siblings = Vec::new();
    let mut level = leaves.to_vec();
    let mut idx = index;
    while level.len() > 1 {
        let sib = if idx % 2 == 0 {
            if idx + 1 < level.len() {
                level[idx + 1]
            } else {
                level[idx]
            }
        } else {
            level[idx - 1]
        };
        siblings.push(sib);
        level = parent_level(&level);
        idx /= 2;
    }
    siblings
}

fn replay(mut node: [u8; 32], index: usize, siblings: &[[u8; 32]]) -> [u8; 32] {
    let mut idx = index;
    for sib in siblings {
        node = if idx % 2 == 0 {
            node_hash(&node, sib)
        } else {
            node_hash(sib, &node)
        };
        idx /= 2;
    }
    node
}

/// A small deterministic generator so a challenge is reproducible from a nonce.
/// In use the nonce is fresh and unpredictable to the node.
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

/// Pick `q` random distinct chunk indices to challenge, from a fresh nonce.
#[must_use]
pub fn challenge(nonce: u64, chunk_count: usize, q: usize) -> Vec<usize> {
    if chunk_count == 0 {
        return Vec::new();
    }
    let mut rng = Rng(nonce | 1);
    let mut picked = Vec::new();
    let want = q.min(chunk_count);
    while picked.len() < want {
        let idx = usize::try_from(rng.next() % chunk_count as u64).unwrap_or(0);
        if !picked.contains(&idx) {
            picked.push(idx);
        }
    }
    picked
}

/// Why an audit failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The node did not answer a challenged index.
    Missing { index: usize },
    /// The returned chunk does not hash into the pinned commitment.
    BadProof { index: usize },
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { index } => write!(f, "chunk {index}: the node did not return it"),
            Self::BadProof { index } => write!(
                f,
                "chunk {index}: proof does not match the pinned commitment"
            ),
        }
    }
}

/// Verify a node's answer to a challenge against the pinned commitment.
///
/// # Errors
/// Returns the first [`Fault`] found: a missing chunk or a chunk that fails its
/// inclusion proof.
pub fn verify(
    commitment: Commitment,
    challenge: &[usize],
    answer: &[ChunkProof],
) -> Result<(), Fault> {
    for &index in challenge {
        let Some(proof) = answer.iter().find(|p| p.index == index) else {
            return Err(Fault::Missing { index });
        };
        let recomputed = replay(leaf_hash(&proof.chunk), proof.index, &proof.siblings);
        if recomputed != commitment.0 {
            return Err(Fault::BadProof { index });
        }
    }
    Ok(())
}

/// The probability that challenging `q` random chunks catches a node missing
/// `missing` of `total` chunks: `1 - product (1 - missing/(total - i))`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn detection_probability(missing: usize, total: usize, q: usize) -> f64 {
    if total == 0 || missing == 0 {
        return 0.0;
    }
    let mut prob_all_good = 1.0_f64;
    for i in 0..q.min(total) {
        let good_remaining = (total - missing).saturating_sub(i) as f64;
        let remaining = (total - i) as f64;
        prob_all_good *= good_remaining / remaining;
    }
    1.0 - prob_all_good
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::new(
            (0..16u8)
                .map(|i| vec![i, i.wrapping_mul(7), i ^ 0x5A])
                .collect(),
        )
    }

    #[test]
    fn an_honest_node_passes() {
        let s = store();
        let commit = s.commit();
        let ch = challenge(0xABCD, s.len(), 5);
        let answer = s.answer(&ch);
        assert!(verify(commit, &ch, &answer).is_ok());
    }

    #[test]
    fn a_corrupted_chunk_is_caught_when_challenged() {
        let mut s = store();
        let commit = s.commit();
        s.corrupt(7, b"rotted bytes");
        // Challenge that index directly.
        let ch = vec![7];
        let answer = s.answer(&ch);
        assert_eq!(
            verify(commit, &ch, &answer),
            Err(Fault::BadProof { index: 7 })
        );
    }

    #[test]
    fn a_dropped_chunk_is_reported_missing() {
        let s = store();
        let commit = s.commit();
        let ch = vec![3, 20]; // 20 is out of range, the node cannot answer it
        let answer = s.answer(&ch);
        assert_eq!(
            verify(commit, &ch, &answer),
            Err(Fault::Missing { index: 20 })
        );
    }

    #[test]
    fn challenge_picks_distinct_in_range_indices() {
        let picks = challenge(42, 8, 5);
        assert_eq!(picks.len(), 5);
        for &p in &picks {
            assert!(p < 8);
        }
        // Distinct.
        let mut sorted = picks.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), picks.len());
    }

    #[test]
    fn detection_probability_climbs_with_more_challenges() {
        // Missing 1 of 100 chunks: one challenge catches it ~1%, ten ~9.6%.
        let one = detection_probability(1, 100, 1);
        let ten = detection_probability(1, 100, 10);
        assert!((one - 0.01).abs() < 1e-9);
        assert!(ten > one && ten < 0.11);
    }

    #[test]
    fn a_few_rounds_make_detection_near_certain() {
        // Missing 10% of chunks, 50 challenges: practically certain.
        let p = detection_probability(100, 1000, 50);
        assert!(p > 0.99, "expected near-certain detection, got {p}");
    }
}
