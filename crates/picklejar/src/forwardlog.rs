//! Forward-secure audit log: past entries stay unforgeable after a key leak.
//!
//! The tamper-evident ledger proves a history was not rewritten, as long as the
//! signing secret stays secret. But an unreachable node can be physically seized,
//! and once an attacker holds its current key they could rewrite the whole log to
//! cover their tracks. Forward security removes that power for everything that
//! happened before the seizure. Each entry is authenticated with a key that is
//! then ratcheted forward by a one-way hash and erased: `k_{i+1} = H(k_i)`. An
//! attacker who captures the live key can forge future entries, but cannot run the
//! hash backward to recover any earlier key, so every entry written before the
//! compromise remains unforgeable. A verifier who knows only the initial key (or
//! pins it) can replay the ratchet forward and check every tag.

use crate::authmem::sha256;
use crate::captoken::hmac_sha256;

/// One authenticated log entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The position in the log, starting at 0.
    pub seq: u64,
    /// The logged message.
    pub message: Vec<u8>,
    /// The MAC under the (now-erased) key for this position.
    pub tag: [u8; 32],
}

/// The bytes authenticated for an entry.
fn signed_bytes(seq: u64, message: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 + message.len());
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(message);
    buf
}

/// A forward-secure append-only log. The node keeps only the current key.
#[derive(Clone, Debug)]
pub struct ForwardSecureLog {
    key: [u8; 32],
    entries: Vec<Entry>,
}

impl ForwardSecureLog {
    /// Start a log from an initial key. The verifier must know (or pin) this key
    /// or its first value; the node ratchets and discards it as it goes.
    #[must_use]
    pub const fn new(initial_key: [u8; 32]) -> Self {
        Self {
            key: initial_key,
            entries: Vec::new(),
        }
    }

    /// Append a message, authenticating it with the current key, then ratchet the
    /// key forward and forget the old one.
    pub fn append(&mut self, message: &[u8]) {
        let seq = self.entries.len() as u64;
        let tag = hmac_sha256(&self.key, &signed_bytes(seq, message));
        self.entries.push(Entry {
            seq,
            message: message.to_vec(),
            tag,
        });
        // Ratchet: the new key replaces the old, which is gone.
        self.key = sha256::hash(&self.key);
    }

    /// The recorded entries.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The current (live) key, the only one a present-day compromise reveals.
    #[must_use]
    pub const fn current_key(&self) -> [u8; 32] {
        self.key
    }
}

/// Verify a log against the initial key by replaying the ratchet forward.
///
/// # Errors
/// Returns the sequence number of the first entry whose tag does not verify.
pub fn verify(initial_key: [u8; 32], entries: &[Entry]) -> Result<(), u64> {
    let mut key = initial_key;
    for (i, e) in entries.iter().enumerate() {
        if e.seq != i as u64 {
            return Err(e.seq);
        }
        let expected = hmac_sha256(&key, &signed_bytes(e.seq, &e.message));
        if expected != e.tag {
            return Err(e.seq);
        }
        key = sha256::hash(&key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(initial: [u8; 32], msgs: &[&str]) -> ForwardSecureLog {
        let mut log = ForwardSecureLog::new(initial);
        for m in msgs {
            log.append(m.as_bytes());
        }
        log
    }

    #[test]
    fn an_honest_log_verifies() {
        let log = build([7u8; 32], &["a", "b", "c", "d"]);
        assert!(verify([7u8; 32], log.entries()).is_ok());
    }

    #[test]
    fn editing_a_past_entry_is_caught() {
        let log = build([9u8; 32], &["one", "two", "three"]);
        let mut entries = log.entries().to_vec();
        entries[1].message = b"forged".to_vec();
        assert_eq!(verify([9u8; 32], &entries), Err(1));
    }

    #[test]
    fn a_compromised_key_cannot_forge_an_earlier_entry() {
        // The node logs five entries, then is seized: the attacker learns only
        // the current (post-ratchet) key.
        let initial = [0x42u8; 32];
        let log = build(initial, &["e0", "e1", "e2", "e3", "e4"]);
        let stolen = log.current_key();

        // The attacker rewrites entry 2 and tries every key they can derive from
        // the stolen one (ratcheting forward), since they cannot ratchet back.
        let mut entries = log.entries().to_vec();
        entries[2].message = b"history rewritten".to_vec();
        let mut forge_key = stolen;
        let mut forged_ok = false;
        for _ in 0..64 {
            entries[2].tag = hmac_sha256(&forge_key, &signed_bytes(2, &entries[2].message));
            if verify(initial, &entries).is_ok() {
                forged_ok = true;
                break;
            }
            forge_key = sha256::hash(&forge_key);
        }
        assert!(
            !forged_ok,
            "no forward-derived key should forge a pre-compromise entry"
        );
    }

    #[test]
    fn the_real_earlier_key_is_not_recoverable_from_the_current_one() {
        // Sanity: the ratchet is one-way. Ratcheting the stolen key forward never
        // reproduces an earlier key.
        let initial = [1u8; 32];
        let mut keys = vec![initial];
        for _ in 0..10 {
            keys.push(sha256::hash(keys.last().unwrap()));
        }
        let current = keys[10];
        let mut forward = current;
        for _ in 0..100 {
            forward = sha256::hash(&forward);
            assert_ne!(
                forward, keys[3],
                "forward ratchet must never hit an earlier key"
            );
        }
    }
}
