//! An exhaustive model check of snapshot isolation's read-stability invariant,
//! from scratch.
//!
//! The companion to the WAL ordering model in `picklejar-wal`. Where that one
//! proves crash durability's foundation, this one proves the foundation of
//! isolation: **a transaction reading the same item twice within its snapshot
//! sees the same value, no matter what commits concurrently.** That is the
//! read-stability guarantee snapshot isolation gives, and it is what keeps a
//! tenant's view of memory consistent while other tenants write.
//!
//! The model abstracts the system to a single versioned item, a monotonically
//! advancing commit clock, and one reader holding a snapshot. The check is a
//! breadth-first sweep of every reachable interleaving of commits and reads. As
//! with the WAL model, a deliberately buggy visibility rule (ignore the snapshot
//! and read the latest committed version) is caught with a concrete
//! counterexample, so the proof is not vacuous.

use std::collections::{HashSet, VecDeque};

/// An abstract state: how many versions have committed, the reader's snapshot (if
/// it has begun), the value it saw on its first read (if any), and whether
/// read-stability has been broken.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct State {
    /// Highest committed version (versions `1..=committed` exist). The commit
    /// clock only advances.
    pub committed: u8,
    /// The reader's snapshot, set when it begins; `None` before then.
    pub snapshot: Option<u8>,
    /// The version the reader returned on its first read, for comparison.
    pub first_read: Option<u8>,
    /// Set once a read returns a different value than the first one did.
    pub violated: bool,
}

impl State {
    const fn start() -> Self {
        Self {
            committed: 0,
            snapshot: None,
            first_read: None,
            violated: false,
        }
    }
}

/// The version a reader at `snapshot` sees over `committed` versions.
///
/// The correct rule returns the newest version committed at or before the
/// snapshot; the buggy rule ignores the snapshot and returns the newest committed
/// version.
const fn visible(snapshot: u8, committed: u8, correct: bool) -> u8 {
    if correct {
        // The commit clock only advances and the snapshot is taken at a commit
        // point, so `snapshot <= committed` always holds here; the snapshot is the
        // newest version the reader may see.
        snapshot
    } else {
        committed
    }
}

/// Every state reachable from `s` in one step: a commit, the reader beginning, or
/// the reader performing a read.
fn successors(s: State, max: u8, correct_rule: bool) -> Vec<State> {
    let mut out = Vec::new();
    // A new version commits.
    if s.committed < max {
        out.push(State {
            committed: s.committed + 1,
            ..s
        });
    }
    // The reader begins, taking its snapshot at the current commit point.
    if s.snapshot.is_none() {
        out.push(State {
            snapshot: Some(s.committed),
            ..s
        });
    }
    // The reader reads. It must see the same value every time within its snapshot.
    if let Some(snapshot) = s.snapshot {
        let v = visible(snapshot, s.committed, correct_rule);
        let first = s.first_read.unwrap_or(v);
        out.push(State {
            first_read: Some(first),
            violated: s.violated || first != v,
            ..s
        });
    }
    out
}

/// Exhaustively check read-stability over the bounded model.
///
/// Returns `None` if no reachable state breaks it (a proof for this bound), or
/// `Some(state)` with the first violating state. `correct_rule` selects
/// snapshot-respecting visibility
/// (the real engine) or a buggy latest-wins rule, so a test can confirm the check
/// has teeth.
#[must_use]
pub fn check(max: u8, correct_rule: bool) -> Option<State> {
    let mut seen: HashSet<State> = HashSet::new();
    let mut queue: VecDeque<State> = VecDeque::new();
    let start = State::start();
    seen.insert(start);
    queue.push_back(start);
    while let Some(s) = queue.pop_front() {
        if s.violated {
            return Some(s);
        }
        for next in successors(s, max, correct_rule) {
            if seen.insert(next) {
                queue.push_back(next);
            }
        }
    }
    None
}

/// Distinct reachable states for a bound, for reporting coverage.
#[must_use]
pub fn reachable_states(max: u8, correct_rule: bool) -> usize {
    let mut seen: HashSet<State> = HashSet::new();
    let mut queue: VecDeque<State> = VecDeque::new();
    let start = State::start();
    seen.insert(start);
    queue.push_back(start);
    while let Some(s) = queue.pop_front() {
        for next in successors(s, max, correct_rule) {
            if seen.insert(next) {
                queue.push_back(next);
            }
        }
    }
    seen.len()
}

#[cfg(test)]
mod tests {
    use super::{check, reachable_states};

    #[test]
    fn read_stability_holds_over_every_interleaving() {
        for max in 1..=8 {
            assert_eq!(
                check(max, true),
                None,
                "snapshot read-stability was violated at bound {max}"
            );
        }
        assert!(reachable_states(6, true) > 20);
    }

    #[test]
    fn the_check_has_teeth_a_snapshot_ignoring_read_is_caught() {
        // A reader that ignores its snapshot and returns the latest committed
        // version sees a non-repeatable read once a concurrent commit lands.
        let counterexample = check(3, false).expect("a snapshot-ignoring read must be caught");
        assert!(counterexample.violated);
    }
}
