//! Property-based tests for MVCC against a reference oracle.
//!
//! The oracle is a plain `HashMap<key, value>` tracking the last *committed*
//! value of each key. The MVCC table, driven through the same workload, must
//! agree with the oracle for any interleaving of commits and aborts. Aborted
//! transactions never change the oracle, so "the table matches the oracle"
//! simultaneously proves committed values are visible and aborted ones are
//! not.

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;
use rustdb_storage::{BufferPool, FileManager};
use rustdb_txn::{IsolationLevel, MvccTable, TransactionManager};
use rustdb_wal::{WalSyncHandle, WalWriter};

/// Small key space so collisions (and thus update / delete chains) are
/// common.
const KEYS: u64 = 6;

#[derive(Debug, Clone)]
enum Op {
    /// Set `key` to a value derived from the byte. Insert if absent, update
    /// if present.
    Set(u64, u8),
    /// Delete `key` if present.
    Delete(u64),
}

fn op_strategy() -> impl Strategy<Value = (Op, bool)> {
    let key = 0..KEYS;
    let kind = prop_oneof![
        3 => (0..KEYS, any::<u8>()).prop_map(|(k, v)| Op::Set(k, v)),
        1 => key.prop_map(Op::Delete),
    ];
    // (op, commit?) — about 80% commit.
    (kind, prop::bool::weighted(0.8))
}

fn value_of(byte: u8) -> Vec<u8> {
    vec![byte, byte ^ 0xAA, byte.wrapping_add(7)]
}

struct Env {
    _dir: tempfile::TempDir,
    pool: BufferPool,
    wal: WalSyncHandle,
    mgr: TransactionManager,
}

fn env() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let writer = WalWriter::open(dir.path().join("wal.log")).expect("wal");
    let wal = WalSyncHandle::new(writer);
    let file = FileManager::open(dir.path().join("data.db")).expect("data");
    let pool = BufferPool::with_wal(file, 128, wal.as_hook());
    Env {
        _dir: dir,
        pool,
        wal,
        mgr: TransactionManager::new(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// After any sequence of committed/aborted operations, a fresh reader
    /// sees exactly the oracle's committed state for every key.
    #[test]
    fn table_matches_oracle_after_workload(ops in prop::collection::vec(op_strategy(), 1..=80)) {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");
        let mut oracle: HashMap<u64, Vec<u8>> = HashMap::new();

        for (op, commit) in ops {
            let txn = e.mgr.begin();
            match &op {
                Op::Set(k, b) => {
                    let val = value_of(*b);
                    if oracle.contains_key(k) {
                        table.update(&txn, *k, &val).expect("update");
                    } else {
                        table.insert(&txn, *k, &val).expect("insert");
                    }
                    if commit {
                        oracle.insert(*k, val);
                    }
                }
                Op::Delete(k) => {
                    if oracle.contains_key(k) {
                        table.delete(&txn, *k).expect("delete");
                        if commit {
                            oracle.remove(k);
                        }
                    } else {
                        // Nothing committed to delete; skip the table op.
                        if commit {
                            e.mgr.commit(&txn);
                        } else {
                            e.mgr.abort(&txn);
                        }
                        continue;
                    }
                }
            }
            if commit {
                e.mgr.commit(&txn);
            } else {
                e.mgr.abort(&txn);
            }
        }

        // A fresh reader must agree with the oracle for every key.
        let reader = e.mgr.begin();
        for k in 0..KEYS {
            let got = table.get(&reader, k).expect("get");
            prop_assert_eq!(got, oracle.get(&k).cloned(), "key {} disagrees", k);
        }
    }

    /// A RepeatableRead reader's view is frozen: committing more work after
    /// it began never changes what it sees.
    #[test]
    fn repeatable_read_snapshot_is_stable(
        setup in prop::collection::vec((0..KEYS, any::<u8>()), 1..=20),
        later in prop::collection::vec(op_strategy(), 1..=40),
    ) {
        let e = env();
        let table = MvccTable::create(&e.pool, e.wal.clone(), &e.mgr).expect("create");

        // Build and commit an initial state.
        let mut present: HashSet<u64> = HashSet::new();
        for (k, b) in setup {
            let txn = e.mgr.begin();
            let val = value_of(b);
            if present.contains(&k) {
                table.update(&txn, k, &val).expect("update");
            } else {
                table.insert(&txn, k, &val).expect("insert");
                present.insert(k);
            }
            e.mgr.commit(&txn);
        }

        // Begin a RepeatableRead reader and record its view.
        let reader = e.mgr.begin_with(IsolationLevel::RepeatableRead);
        let mut frozen: HashMap<u64, Option<Vec<u8>>> = HashMap::new();
        for k in 0..KEYS {
            frozen.insert(k, table.get(&reader, k).expect("get"));
        }

        // Commit arbitrary more work.
        for (op, commit) in later {
            let txn = e.mgr.begin();
            match &op {
                Op::Set(k, b) => {
                    let val = value_of(*b);
                    if present.contains(k) {
                        table.update(&txn, *k, &val).expect("update");
                    } else {
                        table.insert(&txn, *k, &val).expect("insert");
                        if commit { present.insert(*k); }
                    }
                }
                Op::Delete(k) => {
                    if present.contains(k) {
                        table.delete(&txn, *k).expect("delete");
                        if commit { present.remove(k); }
                    } else {
                        if commit { e.mgr.commit(&txn); } else { e.mgr.abort(&txn); }
                        continue;
                    }
                }
            }
            if commit { e.mgr.commit(&txn); } else { e.mgr.abort(&txn); }
        }

        // The reader's view must be byte-for-byte unchanged.
        for k in 0..KEYS {
            let now = table.get(&reader, k).expect("re-get");
            prop_assert_eq!(&now, frozen.get(&k).unwrap(), "key {} drifted under RepeatableRead", k);
        }
    }
}
