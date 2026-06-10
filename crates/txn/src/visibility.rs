//! MVCC visibility: is a tuple version visible to a given reader?
//!
//! A tuple version carries two transaction markers:
//! - `xmin`: the transaction that **created** it.
//! - `xmax`: the transaction that **deleted** it (`0` = not deleted).
//!
//! Given a reader's [`Snapshot`], the transaction status table
//! ([`TransactionManager`]), and the reader's own xid, [`Snapshot::is_visible`]
//! decides whether the reader should see the version.
//!
//! # The rule (snapshot isolation)
//!
//! A version is visible to the reader iff **both**:
//!
//! 1. **Its creator is visible.** Either the reader created it
//!    (`xmin == reader_xid`, so a transaction always sees its own writes),
//!    or `xmin` committed in the snapshot's past
//!    ([`Snapshot::committed_in_past`]): it started before the snapshot and
//!    had committed and was not still in progress at snapshot time.
//! 2. **Its deletion is *not* visible.** If `xmax == 0` the version was
//!    never deleted, so it stays. Otherwise the deletion hides the version
//!    iff the reader performed it (`xmax == reader_xid`) or the deleter
//!    committed in the snapshot's past. A deleter that aborted, or that is
//!    still in progress relative to the snapshot, does **not** hide the
//!    version.
//!
//! Every branch is covered by the table-driven tests below.

use crate::manager::{Snapshot, TransactionManager, Xid};

impl Snapshot {
    /// Decide whether a version `(xmin, xmax)` is visible to `reader_xid`
    /// under this snapshot. `xmax == 0` means the version is not deleted.
    ///
    /// See the module docs for the full rule.
    #[must_use]
    pub fn is_visible(
        &self,
        xmin: Xid,
        xmax: Xid,
        mgr: &TransactionManager,
        reader_xid: Xid,
    ) -> bool {
        // 1. Creator must be visible.
        let creator_visible = xmin == reader_xid || self.committed_in_past(xmin, mgr);
        if !creator_visible {
            return false;
        }
        // 2. A visible deletion hides the version.
        if xmax == 0 {
            return true; // never deleted
        }
        let deletion_visible = xmax == reader_xid || self.committed_in_past(xmax, mgr);
        !deletion_visible
    }
}

#[cfg(test)]
mod tests {
    use crate::manager::{Transaction, TransactionManager};

    /// Outcome label for readable assertions.
    fn vis(snapshot_owner: &Transaction, mgr: &TransactionManager, xmin: u64, xmax: u64) -> bool {
        snapshot_owner
            .snapshot()
            .is_visible(xmin, xmax, mgr, snapshot_owner.xid())
    }

    #[test]
    fn own_insert_is_visible() {
        let mgr = TransactionManager::new();
        let reader = mgr.begin(); // xid 1
                                  // Version created by the reader itself, not deleted.
        assert!(vis(&reader, &mgr, reader.xid(), 0));
    }

    #[test]
    fn own_delete_hides() {
        let mgr = TransactionManager::new();
        let creator = mgr.begin(); // 1
        mgr.commit(&creator);
        let reader = mgr.begin(); // 2
                                  // Reader sees a committed row, then deletes it itself.
        assert!(
            vis(&reader, &mgr, creator.xid(), 0),
            "visible before delete"
        );
        assert!(
            !vis(&reader, &mgr, creator.xid(), reader.xid()),
            "own delete hides it"
        );
    }

    #[test]
    fn committed_before_snapshot_is_visible() {
        let mgr = TransactionManager::new();
        let a = mgr.begin(); // 1
        mgr.commit(&a);
        let reader = mgr.begin(); // 2, snapshot after A committed
        assert!(vis(&reader, &mgr, a.xid(), 0));
    }

    #[test]
    fn committed_after_snapshot_is_invisible() {
        let mgr = TransactionManager::new();
        let reader = mgr.begin(); // 1, snapshot taken now (xmax = 1)
        let b = mgr.begin(); // 2
        mgr.commit(&b); // commits AFTER reader's snapshot
                        // B started after the snapshot (xid 2 >= snapshot.xmax 1) -> invisible.
        assert!(!vis(&reader, &mgr, b.xid(), 0));
    }

    #[test]
    fn aborted_creator_is_invisible() {
        let mgr = TransactionManager::new();
        let a = mgr.begin(); // 1
        mgr.abort(&a);
        let reader = mgr.begin(); // 2
        assert!(!vis(&reader, &mgr, a.xid(), 0));
    }

    #[test]
    fn aborted_deleter_keeps_row_visible() {
        let mgr = TransactionManager::new();
        let creator = mgr.begin(); // 1
        mgr.commit(&creator);
        let deleter = mgr.begin(); // 2
        mgr.abort(&deleter); // deletion never took effect
        let reader = mgr.begin(); // 3
        assert!(vis(&reader, &mgr, creator.xid(), deleter.xid()));
    }

    #[test]
    fn in_progress_concurrent_creator_is_invisible() {
        let mgr = TransactionManager::new();
        let a = mgr.begin(); // 1, stays active
        let reader = mgr.begin(); // 2, snapshot.active = {1}
        assert!(!vis(&reader, &mgr, a.xid(), 0));
        // Even after A commits, the already-captured snapshot keeps it hidden.
        mgr.commit(&a);
        assert!(!vis(&reader, &mgr, a.xid(), 0));
    }

    #[test]
    fn in_progress_concurrent_deleter_keeps_row_visible() {
        let mgr = TransactionManager::new();
        let creator = mgr.begin(); // 1
        mgr.commit(&creator);
        let deleter = mgr.begin(); // 2, stays active (concurrent with reader)
        let reader = mgr.begin(); // 3, snapshot.active = {2}
                                  // The delete by an in-progress txn is not visible -> row stays.
        assert!(vis(&reader, &mgr, creator.xid(), deleter.xid()));
        let _ = deleter;
    }

    #[test]
    fn committed_concurrent_deleter_after_snapshot_keeps_row_visible() {
        // A deleter that commits AFTER the reader's snapshot must not hide
        // the row from that snapshot (snapshot stability).
        let mgr = TransactionManager::new();
        let creator = mgr.begin(); // 1
        mgr.commit(&creator);
        let reader = mgr.begin(); // 2, snapshot.xmax = 2
        let deleter = mgr.begin(); // 3
        mgr.commit(&deleter); // commits after reader's snapshot
        assert!(
            vis(&reader, &mgr, creator.xid(), deleter.xid()),
            "a delete committed after the snapshot must not be visible",
        );
    }

    #[test]
    fn edge_xids_do_not_panic() {
        let mgr = TransactionManager::new();
        let reader = mgr.begin();
        // xmin 0 (sentinel) -> treated as aborted/unknown creator -> invisible.
        assert!(!vis(&reader, &mgr, 0, 0));
        // xmax u64::MAX as a deleter that never existed -> not deleted-visible.
        assert!(vis(&reader, &mgr, reader.xid(), u64::MAX));
    }
}
