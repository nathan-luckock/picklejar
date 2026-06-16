//! Transaction manager: xid allocation, status table, and snapshots.
//!
//! This is the in-memory bookkeeping the MVCC layer is built on. It assigns
//! monotonically increasing transaction ids (xids), tracks whether each one
//! is active / committed / aborted, and captures a *snapshot* of the commit
//! state at the moment a transaction begins.
//!
//! # Snapshot model
//!
//! Snapshots are **txid-based**, the model Postgres uses, not
//! commit-sequence-number based. A snapshot is three things:
//!
//! - `xmax`: the first xid that had **not yet started** when the snapshot
//!   was taken. Any version created by an xid `>= xmax` is invisible (it is
//!   "in the future" relative to this snapshot).
//! - `active`: the set of xids that were **in progress** when the snapshot
//!   was taken. A version created by one of these is invisible, because that
//!   transaction had not committed at snapshot time.
//! - `xmin`: the lowest still-active xid (or `xmax` if none were active). A
//!   creator xid below `xmin` is guaranteed finished, a cheap fast path.
//!
//! A creator xid `v` is "committed in the past" for a snapshot iff
//! `status(v) == Committed && v < xmax && !active.contains(v)`. The full
//! visibility rule (which also accounts for the deleter `xmax` of a version
//! and for a reader seeing its own writes) lands in the visibility module
//! (issue #44).
//!
//! This model matches the `xmin` / `xmax` tuple-header language already
//! locked into `docs/design.md`.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

/// Transaction identifier. Monotonic, starts at 1. `0` is reserved as the
/// "no transaction" sentinel (e.g. a version's `xmax == 0` means "not
/// deleted"), so it is never assigned to a real transaction.
pub type Xid = u64;

/// Lifecycle state of a transaction.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TxnState {
    /// In progress: neither committed nor aborted.
    Active,
    /// Committed. Its effects are durable and visible to later snapshots.
    Committed,
    /// Rolled back. Its effects are never visible to anyone.
    Aborted,
}

/// Isolation level for a transaction.
///
/// Only the snapshot *timing* differs between the two; both use the same
/// visibility rule. `ReadCommitted`'s per-statement re-snapshotting is wired
/// up in issue #47; for now both behave as a single begin-time snapshot.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum IsolationLevel {
    /// A fresh snapshot per statement: sees other transactions' commits that
    /// land after this one began.
    ReadCommitted,
    /// One snapshot for the whole transaction (a.k.a. Snapshot isolation).
    /// The default.
    #[default]
    RepeatableRead,
}

/// A point-in-time view of which transactions had committed.
///
/// See the module docs for the meaning of each field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    /// Lowest still-active xid at snapshot time (or `xmax` if none active).
    pub xmin: Xid,
    /// First xid that had not started at snapshot time. `>= xmax` is invisible.
    pub xmax: Xid,
    /// Xids that were in progress at snapshot time.
    pub active: BTreeSet<Xid>,
}

impl Snapshot {
    /// True if a creator xid `v` is committed-and-visible to this snapshot,
    /// ignoring the reader's own writes (the visibility module layers that
    /// on top). A version is from the past iff it started before the
    /// snapshot (`v < xmax`) and was not still in progress at snapshot time.
    #[must_use]
    pub fn committed_in_past(&self, v: Xid, mgr: &TransactionManager) -> bool {
        v < self.xmax && !self.active.contains(&v) && mgr.state(v) == TxnState::Committed
    }
}

struct Inner {
    next_xid: Xid,
    status: HashMap<Xid, TxnState>,
    active: BTreeSet<Xid>,
    /// On reopen, every xid below this watermark is treated as committed
    /// unless `status` records otherwise (see [`TransactionManager::recover`]).
    /// Zero for a fresh manager, so unknown xids stay aborted.
    committed_floor: Xid,
}

/// Allocates xids, tracks transaction state, and hands out snapshots.
///
/// Like the rest of the engine's coordinators (`BufferPool`, `MiniHeap`), it
/// uses interior mutability so every method takes `&self`.
pub struct TransactionManager {
    inner: RefCell<Inner>,
}

impl std::fmt::Debug for TransactionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.borrow();
        f.debug_struct("TransactionManager")
            .field("next_xid", &inner.next_xid)
            .field("active", &inner.active)
            .finish_non_exhaustive()
    }
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionManager {
    /// A fresh manager. The first transaction will get xid 1.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(Inner {
                next_xid: 1,
                status: HashMap::new(),
                active: BTreeSet::new(),
                committed_floor: 0,
            }),
        }
    }

    /// Restore committed state after reopening a database.
    ///
    /// `next_xid` is the watermark saved at the last commit: every xid below it
    /// had finished (committed or aborted) at that point, so it is safe to
    /// treat them all as committed except the `aborted` ones recorded here.
    /// New transactions continue from `next_xid`, so their ids never collide
    /// with the previous session's. Any xid at or above the watermark (for
    /// example a transaction that was still open, or whose pages leaked to disk
    /// via eviction, when the process ended) stays aborted and invisible.
    pub fn recover(&self, next_xid: Xid, aborted: &[Xid]) {
        let mut inner = self.inner.borrow_mut();
        inner.next_xid = next_xid;
        inner.committed_floor = next_xid;
        for &x in aborted {
            inner.status.insert(x, TxnState::Aborted);
        }
    }

    /// The next xid to be assigned: the watermark to persist at a commit.
    #[must_use]
    pub fn next_xid(&self) -> Xid {
        self.inner.borrow().next_xid
    }

    /// The xids known to have aborted, for persisting alongside the watermark.
    #[must_use]
    pub fn aborted_xids(&self) -> Vec<Xid> {
        self.inner
            .borrow()
            .status
            .iter()
            .filter_map(|(&xid, &state)| (state == TxnState::Aborted).then_some(xid))
            .collect()
    }

    /// Begin a transaction with the default isolation level
    /// ([`IsolationLevel::RepeatableRead`]).
    pub fn begin(&self) -> Transaction {
        self.begin_with(IsolationLevel::default())
    }

    /// Begin a transaction at a chosen isolation level.
    pub fn begin_with(&self, level: IsolationLevel) -> Transaction {
        let mut inner = self.inner.borrow_mut();
        let xid = inner.next_xid;
        // Snapshot captures the world as it is *before* this txn starts.
        let snapshot = Self::capture(&inner, xid);
        inner.next_xid = xid + 1;
        inner.status.insert(xid, TxnState::Active);
        inner.active.insert(xid);
        Transaction {
            xid,
            snapshot: RefCell::new(snapshot),
            level,
        }
    }

    /// Capture a snapshot for a transaction about to be assigned `xid`.
    /// `xmax = xid` (everything from `xid` up has not started), and the
    /// active set is whatever is in progress right now (all `< xid`).
    fn capture(inner: &Inner, xid: Xid) -> Snapshot {
        let active = inner.active.clone();
        let xmin = active.iter().next().copied().unwrap_or(xid);
        Snapshot {
            xmin,
            xmax: xid,
            active,
        }
    }

    /// Re-capture a snapshot at the current moment for an already-running
    /// transaction. Used by `ReadCommitted` (issue #47) to refresh per
    /// statement.
    #[must_use]
    pub fn current_snapshot(&self) -> Snapshot {
        let inner = self.inner.borrow();
        Self::capture(&inner, inner.next_xid)
    }

    /// Mark a transaction committed and remove it from the active set.
    pub fn commit(&self, txn: &Transaction) {
        let mut inner = self.inner.borrow_mut();
        inner.status.insert(txn.xid, TxnState::Committed);
        inner.active.remove(&txn.xid);
    }

    /// Mark a transaction aborted and remove it from the active set.
    pub fn abort(&self, txn: &Transaction) {
        let mut inner = self.inner.borrow_mut();
        inner.status.insert(txn.xid, TxnState::Aborted);
        inner.active.remove(&txn.xid);
    }

    /// The recorded state of `xid`. An xid not in the status table reads as
    /// [`TxnState::Committed`] when it is below the recovered watermark (it was
    /// a durably committed transaction from a previous session), otherwise as
    /// [`TxnState::Aborted`] (the safe default: its writes are invisible). For
    /// a fresh manager the watermark is zero, so unknown xids read as aborted.
    #[must_use]
    pub fn state(&self, xid: Xid) -> TxnState {
        let inner = self.inner.borrow();
        inner.status.get(&xid).copied().unwrap_or({
            if xid < inner.committed_floor {
                TxnState::Committed
            } else {
                TxnState::Aborted
            }
        })
    }

    /// Number of currently-active transactions. Handy for tests and stats.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.inner.borrow().active.len()
    }
}

/// A running transaction: its xid, its snapshot, and its isolation level.
#[derive(Debug)]
pub struct Transaction {
    xid: Xid,
    snapshot: RefCell<Snapshot>,
    level: IsolationLevel,
}

impl Transaction {
    /// This transaction's id.
    #[must_use]
    pub const fn xid(&self) -> Xid {
        self.xid
    }

    /// This transaction's isolation level.
    #[must_use]
    pub const fn level(&self) -> IsolationLevel {
        self.level
    }

    /// A clone of the transaction's current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }

    /// Replace the transaction's snapshot. Used by `ReadCommitted` to
    /// refresh per statement (issue #47).
    pub fn set_snapshot(&self, snapshot: Snapshot) {
        *self.snapshot.borrow_mut() = snapshot;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xids_increase_from_one() {
        let mgr = TransactionManager::new();
        let a = mgr.begin();
        let b = mgr.begin();
        let c = mgr.begin();
        assert_eq!(a.xid(), 1);
        assert_eq!(b.xid(), 2);
        assert_eq!(c.xid(), 3);
    }

    #[test]
    fn state_transitions() {
        let mgr = TransactionManager::new();
        let t = mgr.begin();
        assert_eq!(mgr.state(t.xid()), TxnState::Active);
        mgr.commit(&t);
        assert_eq!(mgr.state(t.xid()), TxnState::Committed);

        let t2 = mgr.begin();
        mgr.abort(&t2);
        assert_eq!(mgr.state(t2.xid()), TxnState::Aborted);
    }

    #[test]
    fn unknown_xid_reads_as_aborted() {
        let mgr = TransactionManager::new();
        assert_eq!(mgr.state(999), TxnState::Aborted);
        assert_eq!(mgr.state(0), TxnState::Aborted);
    }

    #[test]
    fn recover_treats_old_xids_as_committed_except_aborts() {
        let mgr = TransactionManager::new();
        mgr.recover(100, &[42, 43]);
        // Below the watermark and not in the aborted set: committed.
        assert_eq!(mgr.state(10), TxnState::Committed);
        assert_eq!(mgr.state(99), TxnState::Committed);
        // Explicitly aborted in the previous session.
        assert_eq!(mgr.state(42), TxnState::Aborted);
        assert_eq!(mgr.state(43), TxnState::Aborted);
        // At or above the watermark: still aborted (never committed).
        assert_eq!(mgr.state(100), TxnState::Aborted);
        assert_eq!(mgr.state(200), TxnState::Aborted);
        // New transactions continue from the watermark.
        assert_eq!(mgr.next_xid(), 100);
        assert_eq!(mgr.begin().xid(), 100);
    }

    #[test]
    fn aborted_xids_round_trips_through_recover() {
        let mgr = TransactionManager::new();
        let a = mgr.begin();
        let b = mgr.begin();
        mgr.commit(&a);
        mgr.abort(&b);
        let aborted = mgr.aborted_xids();
        assert_eq!(aborted, vec![b.xid()]);
        // A fresh manager restored from those facts agrees.
        let restored = TransactionManager::new();
        restored.recover(mgr.next_xid(), &aborted);
        assert_eq!(restored.state(a.xid()), TxnState::Committed);
        assert_eq!(restored.state(b.xid()), TxnState::Aborted);
    }

    #[test]
    fn snapshot_lists_concurrent_active_txn() {
        let mgr = TransactionManager::new();
        let a = mgr.begin(); // xid 1, active
        let b = mgr.begin(); // xid 2; its snapshot should list A as active
        assert!(b.snapshot().active.contains(&a.xid()));
        assert_eq!(b.snapshot().xmax, b.xid());
    }

    #[test]
    fn snapshot_after_commit_excludes_committed_txn() {
        let mgr = TransactionManager::new();
        let a = mgr.begin(); // xid 1
        mgr.commit(&a);
        let c = mgr.begin(); // xid 2; A already committed
        assert!(!c.snapshot().active.contains(&a.xid()));
        // And A is committed-in-past for C.
        assert!(c.snapshot().committed_in_past(a.xid(), &mgr));
    }

    #[test]
    fn concurrent_creator_not_committed_in_past() {
        let mgr = TransactionManager::new();
        let a = mgr.begin(); // xid 1
        let b = mgr.begin(); // xid 2; A is active in B's snapshot
                             // A has not committed, so it is not in B's past.
        assert!(!b.snapshot().committed_in_past(a.xid(), &mgr));
        // Even after A commits, B's (already captured) snapshot still treats
        // A as active -> still not visible. This is the snapshot-stability
        // property RepeatableRead relies on.
        mgr.commit(&a);
        assert!(!b.snapshot().committed_in_past(a.xid(), &mgr));
    }

    #[test]
    fn xmin_tracks_lowest_active() {
        let mgr = TransactionManager::new();
        let a = mgr.begin(); // 1
        let _b = mgr.begin(); // 2
        let c_snapshot = mgr.current_snapshot();
        assert_eq!(c_snapshot.xmin, a.xid(), "lowest active is 1");
        mgr.commit(&a);
        let snap2 = mgr.current_snapshot();
        assert_eq!(snap2.xmin, 2, "after committing 1, lowest active is 2");
    }

    #[test]
    fn active_count_tracks_in_progress() {
        let mgr = TransactionManager::new();
        assert_eq!(mgr.active_count(), 0);
        let a = mgr.begin();
        let b = mgr.begin();
        assert_eq!(mgr.active_count(), 2);
        mgr.commit(&a);
        assert_eq!(mgr.active_count(), 1);
        mgr.abort(&b);
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn default_isolation_is_repeatable_read() {
        let mgr = TransactionManager::new();
        let t = mgr.begin();
        assert_eq!(t.level(), IsolationLevel::RepeatableRead);
    }
}
