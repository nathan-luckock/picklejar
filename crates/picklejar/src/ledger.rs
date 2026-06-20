//! Verifiable history: a tamper-evident ledger of memory operations.
//!
//! Durable state answers "what is true now." An auditor on unreachable hardware
//! often needs more: "what did this memory say at version 7, and has the history
//! been rewritten since?" This module keeps an append-only, hash-chained log of
//! every memory operation. Each entry commits to the one before it, so the head
//! hash commits to the entire history. Pin that one 32-byte head and any
//! retroactive edit, insertion, or deletion is detectable.
//!
//! Two checks, with different powers:
//!
//! - [`Ledger::audit`] recomputes the chain from genesis and pinpoints the first
//!   entry whose stored hash or back-link does not add up. It catches lazy
//!   tampering (someone edited a row in place) and names the offending version.
//! - [`Ledger::head`] compared against a previously pinned value catches *any*
//!   change, including a sophisticated forger who edits an old entry and then
//!   recomputes every later hash to keep the chain internally consistent. That
//!   forgery passes [`Ledger::audit`] but cannot reproduce the pinned head.
//!
//! The lesson the demo makes concrete: internal consistency is not enough; you
//! have to pin the head.

use crate::authmem::sha256;

/// A memory operation recorded in the history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// A memory was created.
    Insert,
    /// A memory's value changed.
    Update,
    /// A memory was removed.
    Delete,
}

impl Op {
    const fn tag(self) -> u8 {
        match self {
            Self::Insert => 1,
            Self::Update => 2,
            Self::Delete => 3,
        }
    }
}

/// One recorded operation, carrying the back-link to the previous entry and its
/// own hash. The fields are public so an auditor (or an attacker, in the demo)
/// can inspect and reconstruct the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The version number, starting at 0.
    pub seq: u64,
    /// What happened.
    pub op: Op,
    /// The memory's row id.
    pub rowid: u64,
    /// The hash of the value written (all-zero for a delete).
    pub value_hash: [u8; 32],
    /// The hash of the previous entry (the genesis hash for the first).
    pub prev: [u8; 32],
    /// This entry's hash, which the next entry links to.
    pub hash: [u8; 32],
}

/// The hash that binds an entry's contents to its predecessor. Public because
/// the hashing is not a secret: tamper-evidence comes from pinning the head, not
/// from hiding how entries are hashed.
#[must_use]
pub fn entry_hash(
    seq: u64,
    op: Op,
    rowid: u64,
    value_hash: &[u8; 32],
    prev: &[u8; 32],
) -> [u8; 32] {
    let mut buf = Vec::with_capacity(8 + 1 + 8 + 32 + 32);
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.push(op.tag());
    buf.extend_from_slice(&rowid.to_be_bytes());
    buf.extend_from_slice(value_hash);
    buf.extend_from_slice(prev);
    sha256::hash(&buf)
}

/// The hash of a written value.
#[must_use]
pub fn value_hash(value: &[u8]) -> [u8; 32] {
    sha256::hash(value)
}

/// Where and why an audit found the history broken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// An entry's back-link does not match the previous entry's hash.
    BrokenChain { seq: u64 },
    /// An entry's stored hash does not match its recomputed contents.
    AlteredEntry { seq: u64 },
    /// The sequence numbers are not 0, 1, 2, ... in order.
    Misnumbered { seq: u64, expected: u64 },
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BrokenChain { seq } => write!(
                f,
                "entry {seq}: back-link does not match the previous entry"
            ),
            Self::AlteredEntry { seq } => {
                write!(f, "entry {seq}: contents were altered (hash mismatch)")
            }
            Self::Misnumbered { seq, expected } => {
                write!(
                    f,
                    "entry out of order: found seq {seq}, expected {expected}"
                )
            }
        }
    }
}

/// An append-only, hash-chained history of memory operations.
#[derive(Clone, Debug)]
pub struct Ledger {
    genesis: [u8; 32],
    entries: Vec<Entry>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

impl Ledger {
    /// A fresh ledger with a fixed genesis hash.
    #[must_use]
    pub fn new() -> Self {
        Self {
            genesis: sha256::hash(b"picklejar ledger genesis"),
            entries: Vec::new(),
        }
    }

    /// The head hash: the last entry's hash, or the genesis hash when empty. This
    /// is the single value an auditor pins.
    #[must_use]
    pub fn head(&self) -> [u8; 32] {
        self.entries.last().map_or(self.genesis, |e| e.hash)
    }

    /// The number of recorded operations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ledger has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The recorded entries, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Append an operation, chaining it onto the current head.
    pub fn record(&mut self, op: Op, rowid: u64, value: &[u8]) -> &Entry {
        let seq = self.entries.len() as u64;
        let vh = value_hash(value);
        let prev = self.head();
        let hash = entry_hash(seq, op, rowid, &vh, &prev);
        self.entries.push(Entry {
            seq,
            op,
            rowid,
            value_hash: vh,
            prev,
            hash,
        });
        self.entries.last().expect("just pushed")
    }

    /// Rebuild a ledger from stored entries without recomputing anything, as if
    /// loading it from disk. The entries are trusted as given, so [`Self::audit`]
    /// is what re-establishes their integrity.
    #[must_use]
    pub const fn from_entries(genesis: [u8; 32], entries: Vec<Entry>) -> Self {
        Self { genesis, entries }
    }

    /// The genesis hash this ledger chains from.
    #[must_use]
    pub const fn genesis(&self) -> [u8; 32] {
        self.genesis
    }

    /// Recompute the chain from genesis and report the first inconsistency.
    ///
    /// This catches in-place tampering and broken links and names the version.
    /// It does not, on its own, catch a forger who rewrote every later hash to
    /// stay consistent; for that, compare [`Self::head`] against a pinned value.
    ///
    /// # Errors
    /// Returns the first [`Fault`] found, if any.
    pub fn audit(&self) -> Result<(), Fault> {
        let mut prev = self.genesis;
        for (i, e) in self.entries.iter().enumerate() {
            let expected_seq = i as u64;
            if e.seq != expected_seq {
                return Err(Fault::Misnumbered {
                    seq: e.seq,
                    expected: expected_seq,
                });
            }
            if e.prev != prev {
                return Err(Fault::BrokenChain { seq: e.seq });
            }
            if e.hash != entry_hash(e.seq, e.op, e.rowid, &e.value_hash, &e.prev) {
                return Err(Fault::AlteredEntry { seq: e.seq });
            }
            prev = e.hash;
        }
        Ok(())
    }

    /// Whether the history still matches a previously pinned head. This catches
    /// any retroactive change at all, including a fully re-chained forgery.
    #[must_use]
    pub fn matches_pinned(&self, pinned_head: &[u8; 32]) -> bool {
        &self.head() == pinned_head
    }

    /// Prove that the value written at version `seq` had a given hash, by
    /// returning the entry. A verifier confirms the entry hashes correctly and
    /// chains up to a head it trusts.
    #[must_use]
    pub fn entry_at(&self, seq: u64) -> Option<&Entry> {
        self.entries.get(usize::try_from(seq).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Ledger {
        let mut l = Ledger::new();
        l.record(Op::Insert, 1, b"v1");
        l.record(Op::Update, 1, b"v2");
        l.record(Op::Insert, 2, b"hello");
        l.record(Op::Delete, 1, b"");
        l
    }

    #[test]
    fn an_untouched_history_audits_clean() {
        let l = sample();
        assert_eq!(l.len(), 4);
        assert!(l.audit().is_ok());
    }

    #[test]
    fn the_head_commits_to_the_whole_history() {
        let l = sample();
        let pinned = l.head();
        assert!(l.matches_pinned(&pinned));
        // An identical replay produces the identical head.
        let again = sample();
        assert_eq!(again.head(), pinned);
    }

    #[test]
    fn an_in_place_edit_is_caught_and_located() {
        let mut entries = sample().entries().to_vec();
        let genesis = sample().genesis();
        // Tamper: rewrite the value recorded at version 1, without re-chaining.
        entries[1].value_hash = value_hash(b"forged");
        let forged = Ledger::from_entries(genesis, entries);
        assert_eq!(forged.audit(), Err(Fault::AlteredEntry { seq: 1 }));
    }

    #[test]
    fn a_dropped_entry_is_caught() {
        let mut entries = sample().entries().to_vec();
        let genesis = sample().genesis();
        // Tamper: delete version 2 from the middle of the history.
        entries.remove(2);
        let forged = Ledger::from_entries(genesis, entries);
        // The surviving entries are renumbered against their original seq.
        assert!(forged.audit().is_err());
    }

    #[test]
    fn a_re_chained_forgery_passes_audit_but_fails_the_pinned_head() {
        let honest = sample();
        let pinned = honest.head();

        // A sophisticated forger edits version 1 and recomputes every later
        // hash so the chain stays internally consistent.
        let genesis = honest.genesis();
        let mut entries = honest.entries().to_vec();
        entries[1].value_hash = value_hash(b"forged");
        // Re-chain from version 1 onward; version 0 is unchanged.
        let mut prev = entries[0].hash;
        for e in entries.iter_mut().skip(1) {
            e.prev = prev;
            e.hash = entry_hash(e.seq, e.op, e.rowid, &e.value_hash, &e.prev);
            prev = e.hash;
        }
        let forged = Ledger::from_entries(genesis, entries);

        // Internal audit is fooled...
        assert!(forged.audit().is_ok());
        // ...but the pinned head is not.
        assert!(!forged.matches_pinned(&pinned));
    }

    #[test]
    fn entry_at_returns_the_version() {
        let l = sample();
        assert_eq!(l.entry_at(2).map(|e| e.rowid), Some(2));
        assert_eq!(l.entry_at(99), None);
    }
}
