//! An exhaustive model check of the row-level-security retrieval invariant: a
//! tenant's query, accelerated by the approximate index or not, can never return
//! another tenant's row.
//!
//! This is the companion to the WAL-ordering model (`picklejar-wal`) and the
//! snapshot-isolation model (`picklejar-txn`). Where those prove the foundations
//! of crash durability and read-stability, this one proves the foundation of the
//! memory layer's central promise: **engine-enforced tenant isolation survives
//! the index.**
//!
//! # Why this is the hard case
//!
//! Isolation on a plain table is folding a `WHERE owner = current_role` predicate
//! into the scan. A vector memory layer adds a complication: a nearest-neighbor
//! query can be served two ways. The exact path folds the row-level-security
//! predicate in before it ranks, so it is fenced. The cached approximate index is
//! fast, but it knows nothing about policies. The engine's rule is that the index
//! path is taken *only* when no policy applies to the session, so a row-level-
//! security-fenced query always falls back to the exact, fenced path. This model
//! proves that rule is sufficient: across every reachable interleaving of inserts,
//! cache invalidations, role switches, policy changes, index builds, and queries,
//! no query returns a row the caller's policy forbids.
//!
//! A deliberately buggy dispatch (take the index path even when a policy is in
//! force) is caught with a concrete counterexample, so the proof is not vacuous.
//! To the best of the surveyed literature, no vector or AI-memory database
//! model-checks its filtered-retrieval isolation this way.

use std::collections::{HashSet, VecDeque};

/// Two tenants are enough to express a cross-tenant leak.
const ROLE_A: u8 = 0;
/// The second tenant.
const ROLE_B: u8 = 1;

/// An abstract state of a single table under row-level security.
///
/// Captures how many rows each tenant owns, whether a per-tenant policy is in
/// force, which role the session runs as, the cached approximate index (if any),
/// and whether a leak has occurred.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct State {
    /// Rows owned by tenant A (bounded by `max`).
    pub a_rows: u8,
    /// Rows owned by tenant B (bounded by `max`).
    pub b_rows: u8,
    /// Whether an `owner = current_role` policy is in force on the table.
    pub policy_on: bool,
    /// The session's current role.
    pub role: u8,
    /// The cached approximate index, if built: the role it was built for and the
    /// `(A, B)` row counts it captured. `None` means no cache is present.
    pub cache: Option<(u8, u8, u8)>,
    /// Set once a query returns a row the caller's policy forbids.
    pub violated: bool,
}

impl State {
    const fn start() -> Self {
        Self {
            a_rows: 0,
            b_rows: 0,
            policy_on: false,
            role: ROLE_A,
            cache: None,
            violated: false,
        }
    }

    /// The other tenant, relative to the current role.
    const fn other(self) -> u8 {
        if self.role == ROLE_A {
            ROLE_B
        } else {
            ROLE_A
        }
    }
}

/// Whether a query in state `s` leaks: it exposes a row owned by the other tenant
/// while a per-tenant policy restricts the caller to its own rows.
///
/// `correct` selects the engine's real dispatch (the index path is taken only
/// when no policy applies, so a fenced query uses the exact path) or the buggy
/// dispatch (the index path is taken whenever a cache exists, policy or not).
const fn query_leaks(s: &State, correct: bool) -> bool {
    // The engine takes the index path only when no policy applies; the bug takes
    // it whenever a cache is present.
    let use_index = if correct {
        !s.policy_on
    } else {
        s.cache.is_some()
    };

    // How many of the other tenant's rows the chosen path would expose.
    let other_exposed = if use_index {
        match s.cache {
            // The approximate index is unfenced: it returns whatever it captured.
            Some((_, a_snap, b_snap)) => {
                if s.other() == ROLE_A {
                    a_snap
                } else {
                    b_snap
                }
            }
            None => 0,
        }
    } else if s.policy_on {
        // The exact path folds the policy in, so it returns only the caller's own
        // rows: none of the other tenant's.
        0
    } else if s.other() == ROLE_A {
        s.a_rows
    } else {
        s.b_rows
    };

    // A violation is exposing the other tenant's row while a per-tenant policy is
    // in force. Without a policy the role may see every row, so nothing leaks.
    s.policy_on && other_exposed > 0
}

/// Every state reachable from `s` in one step.
fn successors(s: State, max: u8, correct: bool) -> Vec<State> {
    let mut out = Vec::new();

    // The current role inserts a row it owns. Any write invalidates the cache.
    let own = if s.role == ROLE_A { s.a_rows } else { s.b_rows };
    if own < max {
        let mut n = s;
        if s.role == ROLE_A {
            n.a_rows += 1;
        } else {
            n.b_rows += 1;
        }
        n.cache = None;
        out.push(n);
    }

    // Switch role: a read-only session change, so the cache persists.
    out.push(State {
        role: s.other(),
        ..s
    });

    // Toggle the policy: a security change, so it invalidates the cache.
    out.push(State {
        policy_on: !s.policy_on,
        cache: None,
        ..s
    });

    // Build (cache) the approximate index for the current role over current rows.
    out.push(State {
        cache: Some((s.role, s.a_rows, s.b_rows)),
        ..s
    });

    // Query: may set the violation flag.
    out.push(State {
        violated: s.violated || query_leaks(&s, correct),
        ..s
    });

    out
}

/// Exhaustively check the isolation invariant over the bounded model.
///
/// Returns `None` if no reachable state leaks (a proof for this bound), or
/// `Some(state)` with the first violating state. `correct` selects the engine's
/// dispatch (index path only when no policy applies) or a buggy dispatch (index
/// path even under a policy), so a test can confirm the check has teeth.
#[must_use]
pub fn check(max: u8, correct: bool) -> Option<State> {
    let mut seen: HashSet<State> = HashSet::new();
    let mut queue: VecDeque<State> = VecDeque::new();
    let start = State::start();
    seen.insert(start);
    queue.push_back(start);
    while let Some(s) = queue.pop_front() {
        if s.violated {
            return Some(s);
        }
        for next in successors(s, max, correct) {
            if seen.insert(next) {
                queue.push_back(next);
            }
        }
    }
    None
}

/// Distinct reachable states for a bound, for reporting coverage.
#[must_use]
pub fn reachable_states(max: u8, correct: bool) -> usize {
    let mut seen: HashSet<State> = HashSet::new();
    let mut queue: VecDeque<State> = VecDeque::new();
    let start = State::start();
    seen.insert(start);
    queue.push_back(start);
    while let Some(s) = queue.pop_front() {
        for next in successors(s, max, correct) {
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
    fn isolation_holds_over_every_interleaving() {
        for max in 1..=4 {
            assert_eq!(
                check(max, true),
                None,
                "a tenant query leaked another tenant's row at bound {max}"
            );
        }
        // The model is non-trivial: many distinct interleavings are explored.
        assert!(reachable_states(3, true) > 100);
    }

    #[test]
    fn the_check_has_teeth_a_policy_ignoring_index_is_caught() {
        // Serving the approximate index under an active policy exposes the other
        // tenant's rows. The check must find a concrete counterexample.
        let counterexample =
            check(2, false).expect("an index path taken under a policy must be caught");
        assert!(counterexample.violated);
        assert!(counterexample.policy_on);
    }
}
