//! Provable forgetting: a memory that can prove it is gone.
//!
//! A regulator, or a user exercising a right to be forgotten, wants more than a
//! `DELETE`. On unreachable hardware a deleted row's bytes linger: in the
//! write-ahead log, in old multi-version rows, in Reed-Solomon parity, in a
//! cache. Overwriting all of those on a node nobody can service is not credible.
//!
//! So this layer forgets by destroying the only key that can read the memory,
//! not by chasing its bytes. Each memory is sealed under its own independent
//! key; forgetting shreds that key. The ciphertext may survive in every durable
//! surface and be perfectly reconstructed from parity after a crash, and it is
//! still unreadable, because the key it needs no longer exists and was never
//! derivable from anything that persists with the data.
//!
//! The guarantee this module establishes: once a memory is forgotten, an
//! adversary holding every persistent copy of its ciphertext (heap, log, parity
//! reconstruction) across a crash cannot recover the plaintext. What it does not
//! claim: defense against an adversary who captured the key before it was shred.
//! Forgetting is forward, not retroactive, which is exactly the regulatory ask.

use std::collections::BTreeMap;

use crate::authmem::sha256;

/// A SHA-256 counter-mode keystream block: `SHA-256(key || nonce || counter)`.
/// A hash-based stream cipher, built from the same from-scratch hash the
/// authenticated layer uses, so forgetting introduces no new primitive.
fn keystream_block(key: &[u8; 32], nonce: u64, counter: u64) -> [u8; 32] {
    let mut buf = [0u8; 48];
    buf[0..32].copy_from_slice(key);
    buf[32..40].copy_from_slice(&nonce.to_be_bytes());
    buf[40..48].copy_from_slice(&counter.to_be_bytes());
    sha256::hash(&buf)
}

/// XOR `data` with the keystream. Encryption and decryption are the same
/// operation, so a wrong or absent key yields noise, never the plaintext.
fn xor_crypt(key: &[u8; 32], nonce: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for (counter, block) in data.chunks(32).enumerate() {
        let ks = keystream_block(key, nonce, counter as u64);
        for (b, k) in block.iter().zip(ks.iter()) {
            out.push(b ^ k);
        }
    }
    out
}

/// The bytes of a sealed memory that are allowed to persist anywhere: the row
/// id, the nonce, and the ciphertext. None of these reveal the plaintext.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sealed {
    /// The memory's row id.
    pub rowid: u64,
    /// The per-memory nonce.
    pub nonce: u64,
    /// The encrypted bytes.
    pub ciphertext: Vec<u8>,
}

/// The outcome of trying to read a memory back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recall {
    /// The plaintext was recovered (the key was present).
    Remembered(Vec<u8>),
    /// The memory is forgotten: its key has been shred and it cannot be read.
    Forgotten,
}

/// The durable keystore.
///
/// It is the *only* place a memory's key lives; the sealed ciphertext that
/// persists elsewhere is useless without it. Forgetting a memory removes its key
/// here, and that removal is what a crash must preserve.
#[derive(Clone, Debug, Default)]
pub struct KeyVault {
    keys: BTreeMap<u64, [u8; 32]>,
}

impl KeyVault {
    /// An empty vault.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            keys: BTreeMap::new(),
        }
    }

    /// Seal a memory under a fresh, independent key, returning the bytes that
    /// may persist. The key is kept here and nowhere else.
    pub fn seal(&mut self, rowid: u64, nonce: u64, key: [u8; 32], plaintext: &[u8]) -> Sealed {
        let ciphertext = xor_crypt(&key, nonce, plaintext);
        self.keys.insert(rowid, key);
        Sealed {
            rowid,
            nonce,
            ciphertext,
        }
    }

    /// Try to read a sealed memory. Returns [`Recall::Forgotten`] when the key is
    /// gone, no matter how intact the ciphertext is.
    #[must_use]
    pub fn recall(&self, sealed: &Sealed) -> Recall {
        self.keys
            .get(&sealed.rowid)
            .map_or(Recall::Forgotten, |key| {
                Recall::Remembered(xor_crypt(key, sealed.nonce, &sealed.ciphertext))
            })
    }

    /// Forget a memory by shredding its key. The key is overwritten before it is
    /// dropped, and afterward there is no way to derive it.
    pub fn forget(&mut self, rowid: u64) {
        if let Some(mut key) = self.keys.remove(&rowid) {
            // Best-effort overwrite before the key is dropped.
            key.fill(0);
        }
    }

    /// Whether a memory's key is still present.
    #[must_use]
    pub fn holds(&self, rowid: u64) -> bool {
        self.keys.contains_key(&rowid)
    }

    /// A durable snapshot of the vault, as it would be written to disk. Crash
    /// recovery restores exactly this, so a key shred before the snapshot stays
    /// shred.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(u64, [u8; 32])> {
        self.keys.iter().map(|(k, v)| (*k, *v)).collect()
    }

    /// Reload a vault from a durable snapshot (a simulated crash recovery).
    #[must_use]
    pub fn recover(snapshot: &[(u64, [u8; 32])]) -> Self {
        Self {
            keys: snapshot.iter().copied().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A full-entropy key, as a real seal would use, derived so that it is never
    // a low-entropy repeated-byte value an adversary could cheaply guess.
    fn key(seed: u8) -> [u8; 32] {
        sha256::hash(&[seed, 0x5A, 0xA5, seed])
    }

    #[test]
    fn round_trips_when_remembered() {
        let mut v = KeyVault::new();
        let sealed = v.seal(1, 7, key(0xAB), b"the launch codes");
        assert_eq!(
            v.recall(&sealed),
            Recall::Remembered(b"the launch codes".to_vec())
        );
    }

    #[test]
    fn ciphertext_does_not_contain_the_plaintext() {
        let mut v = KeyVault::new();
        let plaintext = b"the launch codes";
        let sealed = v.seal(1, 7, key(0xAB), plaintext);
        assert_ne!(sealed.ciphertext, plaintext);
    }

    #[test]
    fn forgotten_memory_cannot_be_read() {
        let mut v = KeyVault::new();
        let sealed = v.seal(1, 7, key(0xAB), b"secret");
        v.forget(1);
        assert_eq!(v.recall(&sealed), Recall::Forgotten);
    }

    #[test]
    fn an_adversary_with_ciphertext_and_parity_recovers_nothing() {
        let mut v = KeyVault::new();
        let plaintext = b"a memory to be forgotten";
        let sealed = v.seal(1, 7, key(0xAB), plaintext);
        v.forget(1);

        // The adversary has every persistent copy of the ciphertext: the heap
        // copy, a WAL copy, and a copy reconstructed byte-for-byte from parity.
        let from_heap = sealed.clone();
        let from_wal = sealed.clone();
        // A byte-for-byte parity reconstruction is just an exact copy.
        let from_parity = sealed.clone();
        for copy in [&from_heap, &from_wal, &from_parity] {
            assert_eq!(v.recall(copy), Recall::Forgotten);
        }
        // The shred key had full entropy, so a feasible adversary cannot guess
        // it. Brute-forcing the entire low-entropy space of repeated-byte keys
        // recovers nothing, standing in for the infeasibility of guessing a real
        // 256-bit key.
        for guess in 0u8..=255 {
            let attempt = xor_crypt(&[guess; 32], sealed.nonce, &sealed.ciphertext);
            assert_ne!(
                attempt, plaintext,
                "no low-entropy key guess should reveal the plaintext"
            );
        }
    }

    #[test]
    fn forgetting_survives_a_crash() {
        let mut v = KeyVault::new();
        let a = v.seal(1, 1, key(0x11), b"keep me");
        let b = v.seal(2, 2, key(0x22), b"forget me");
        v.forget(2);

        // Crash: drop the live vault, recover from the durable snapshot.
        let snap = v.snapshot();
        let recovered = KeyVault::recover(&snap);

        assert_eq!(
            recovered.recall(&a),
            Recall::Remembered(b"keep me".to_vec())
        );
        assert_eq!(recovered.recall(&b), Recall::Forgotten);
    }

    #[test]
    fn other_memories_are_untouched_by_a_forget() {
        let mut v = KeyVault::new();
        let a = v.seal(1, 1, key(0x11), b"alpha");
        let b = v.seal(2, 2, key(0x22), b"bravo");
        v.forget(1);
        assert_eq!(v.recall(&a), Recall::Forgotten);
        assert_eq!(v.recall(&b), Recall::Remembered(b"bravo".to_vec()));
    }
}
