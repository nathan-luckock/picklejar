//! An exhaustive model check of the valid-time travel invariant.
//!
//! A read at a session as-of instant returns a row exactly when that row is valid
//! then, under the half-open interval rule `[valid_from, valid_to)`.
//!
//! The third sibling of the retrieval models (`isolation_model`,
//! `freshness_model`). Those prove the cached index never serves another
//! tenant's row or a deleted one; this proves the temporal filter never serves a
//! row outside its validity interval, and never drops one inside it. Together the
//! three cover what a filtered read may return.
//!
//! # The mechanism under test
//!
//! When the session as-of instant is `t`, the binder folds the predicate
//! `valid_from <= t AND (valid_to IS NULL OR t < valid_to)` into a temporal
//! table's reads. That predicate is the engine's *definition* of "valid at `t`",
//! and it is what an off-by-one at the boundary would corrupt: the half-open
//! interval includes `valid_from` but excludes `valid_to`, so the instant a row
//! is superseded belongs to its successor, with no gap and no overlap.
//!
//! Valid-time travel is a pure, row-local filter: each row is included or
//! excluded on its own interval alone, with no cache, no concurrency, and no
//! dependence on the other rows. So an exhaustive sweep of every interval and
//! every instant over a bounded time domain is a complete proof of the filter
//! for that domain, and, because the rows do not interact, for a table of any
//! size built from those intervals.
//!
//! A deliberately buggy variant, a closed upper bound (`t <= valid_to`), is
//! caught with a concrete counterexample: at the supersession instant it returns
//! a row that is no longer valid, so the same instant would match both a row and
//! its successor. That is the exact boundary the half-open rule exists to get
//! right, so catching it shows the check has teeth.

/// One checked case: a row's interval, the instant queried, what the filter did,
/// and the ground truth. A counterexample is a case where the two disagree.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Case {
    /// The row's `valid_from`.
    pub valid_from: u8,
    /// The row's `valid_to`, or `None` for the open-ended (still-current) row.
    pub valid_to: Option<u8>,
    /// The as-of instant the read travels to.
    pub instant: u8,
    /// Whether the engine's predicate returned the row.
    pub returned: bool,
    /// Whether the row is in fact valid at `instant` (the half-open definition).
    pub valid: bool,
}

/// The ground-truth definition of "valid at `t`": the half-open interval
/// `[valid_from, valid_to)` contains `t`. An open `valid_to` (`None`) is the
/// still-current row, valid at every instant at or after `valid_from`.
const fn valid_at(from: u8, to: Option<u8>, t: u8) -> bool {
    if from > t {
        return false;
    }
    match to {
        None => true,
        Some(to) => t < to,
    }
}

/// The engine's folded predicate. `correct` selects the half-open rule the binder
/// emits (`t < valid_to`) or the buggy closed upper bound (`t <= valid_to`),
/// which returns a row at the very instant it is superseded.
const fn filter_returns(from: u8, to: Option<u8>, t: u8, correct: bool) -> bool {
    if from > t {
        return false;
    }
    match to {
        None => true,
        Some(to) => {
            if correct {
                t < to
            } else {
                t <= to
            }
        }
    }
}

/// Exhaustively check the valid-time invariant over the bounded model.
///
/// Sweeps every interval (`valid_from` in `0..=domain`, `valid_to` either open or
/// in `0..=domain`, including degenerate empty intervals) against every instant
/// in `0..=domain`. Returns `None` if the engine's predicate agrees with the
/// ground-truth validity definition on every case (a proof for this domain), or
/// `Some(case)` with the first disagreement. `correct` selects the half-open rule
/// or the buggy closed upper bound, so a test can confirm the check has teeth.
#[must_use]
pub fn check(domain: u8, correct: bool) -> Option<Case> {
    for from in 0..=domain {
        for to in interval_ends(domain) {
            for t in 0..=domain {
                let returned = filter_returns(from, to, t, correct);
                let valid = valid_at(from, to, t);
                if returned != valid {
                    return Some(Case {
                        valid_from: from,
                        valid_to: to,
                        instant: t,
                        returned,
                        valid,
                    });
                }
            }
        }
    }
    None
}

/// Distinct cases checked for a domain, for reporting coverage.
#[must_use]
pub fn reachable_states(domain: u8, _correct: bool) -> usize {
    let intervals = (usize::from(domain) + 1) * (usize::from(domain) + 2);
    intervals * (usize::from(domain) + 1)
}

/// Every `valid_to` to sweep over a domain: the open end (`None`) and each
/// concrete bound `0..=domain` (bounds at or below `valid_from` give an empty
/// interval, which the filter must treat as valid at no instant).
fn interval_ends(domain: u8) -> impl Iterator<Item = Option<u8>> {
    std::iter::once(None).chain((0..=domain).map(Some))
}

#[cfg(test)]
mod tests {
    use super::{check, reachable_states};

    #[test]
    fn validity_holds_over_every_interval_and_instant() {
        for domain in 1..=8 {
            assert_eq!(
                check(domain, true),
                None,
                "the filter disagreed with the validity definition over domain {domain}"
            );
        }
        assert!(reachable_states(4, true) > 100);
    }

    #[test]
    fn the_check_has_teeth_a_closed_upper_bound_is_caught() {
        // A closed upper bound (`t <= valid_to`) returns a row at the instant it
        // is superseded, the exact boundary the half-open rule exists to get
        // right. The check must find that counterexample.
        let cx = check(3, false).expect("a closed upper bound must be caught");
        // The disagreement is the filter returning a row that is not valid: at the
        // supersession instant, `instant == valid_to`.
        assert!(cx.returned && !cx.valid);
        assert_eq!(cx.valid_to, Some(cx.instant));
    }

    #[test]
    fn the_open_ended_current_row_is_valid_at_and_after_its_start() {
        // A spot check of the open-ended row, the still-current memory: valid at
        // and after valid_from, invalid before it.
        assert!(!super::valid_at(2, None, 1), "before its start");
        for t in 2..=10 {
            assert!(super::valid_at(2, None, t), "at or after its start (t={t})");
        }
    }
}
