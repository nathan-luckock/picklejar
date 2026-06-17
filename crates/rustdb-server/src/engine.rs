//! Concurrency: an engine actor that serves many client connections.
//!
//! The engine itself is single-threaded (`!Send`), so it cannot be shared
//! across threads behind a lock. Instead it runs on its own thread and owns the
//! [`Database`]; client connections (each on their own thread) talk to it by
//! sending SQL over a channel and waiting for the reply. The actor processes one
//! statement at a time, so there is never a data race.
//!
//! Isolation across connections is enforced by **transaction exclusivity**: an
//! open explicit transaction (`BEGIN` ... `COMMIT`) owns the engine, so other
//! connections' statements wait until it ends, while auto-commit statements from
//! any connection interleave freely whenever no transaction is open. MVCC then
//! gives each statement a consistent snapshot. (Per-row write-write conflict
//! detection for *overlapping* explicit transactions is future work; exclusivity
//! makes that case serial, hence correct, just not concurrent.)

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use rustdb::{Database, QueryOutcome};

/// The minimal engine surface the wire protocol needs.
///
/// Run a statement and ask whether a transaction is open. Implemented by the
/// live [`Database`] (for the in-process and test paths) and by
/// [`SessionHandle`] (for a networked client).
pub trait Engine {
    /// Run one SQL statement, returning its outcome or a human-readable error.
    ///
    /// # Errors
    ///
    /// Returns the engine's error message if the statement fails.
    fn execute(&mut self, sql: &str) -> Result<QueryOutcome, String>;
    /// Whether this session currently has an open explicit transaction.
    fn in_transaction(&self) -> bool;
}

impl Engine for Database {
    fn execute(&mut self, sql: &str) -> Result<QueryOutcome, String> {
        Self::execute(self, sql).map_err(|e| e.to_string())
    }
    fn in_transaction(&self) -> bool {
        Self::in_transaction(self)
    }
}

/// The reply to one `Execute`: the result and whether the session is now in a
/// transaction (cached by the handle so `in_transaction` needs no round-trip).
type ExecReply = (Result<QueryOutcome, String>, bool);

/// A message to the engine thread.
enum Command {
    /// Run `sql` for `session` and reply on `reply`.
    Execute {
        session: u64,
        sql: String,
        reply: Sender<ExecReply>,
    },
    /// The session disconnected: roll back its transaction if it holds one.
    Close { session: u64 },
}

/// Owns the engine thread. Hand each accepted connection a [`SessionHandle`]
/// from [`session`](Self::session).
#[derive(Debug)]
pub struct EngineActor {
    tx: Sender<Command>,
    next_session: Arc<AtomicU64>,
}

impl EngineActor {
    /// Start the engine thread, which opens the `Database` via `open` and then
    /// owns it for the process's lifetime. The database is created *on* the
    /// engine thread (it is `!Send`, so it cannot be moved across one); `open`
    /// captures only thread-safe data such as the file path.
    ///
    /// # Errors
    ///
    /// Returns the open error if the engine could not open the database.
    pub fn spawn<F>(open: F) -> Result<Self, String>
    where
        F: FnOnce() -> Result<Database, String> + Send + 'static,
    {
        let (tx, rx) = channel::<Command>();
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();
        thread::spawn(move || match open() {
            Ok(db) => {
                let _ = ready_tx.send(Ok(()));
                engine_loop(db, &rx);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                next_session: Arc::new(AtomicU64::new(1)),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("engine thread exited before signalling readiness".to_string()),
        }
    }

    /// A fresh per-connection handle with a unique session id.
    #[must_use]
    pub fn session(&self) -> SessionHandle {
        SessionHandle {
            session: self.next_session.fetch_add(1, Ordering::Relaxed),
            tx: self.tx.clone(),
            in_txn: false,
        }
    }
}

/// A per-connection handle to the engine actor. Sending and `Drop` clean up the
/// session's transaction automatically.
#[derive(Debug)]
pub struct SessionHandle {
    session: u64,
    tx: Sender<Command>,
    in_txn: bool,
}

impl Engine for SessionHandle {
    fn execute(&mut self, sql: &str) -> Result<QueryOutcome, String> {
        let (reply_tx, reply_rx) = channel();
        let sent = self.tx.send(Command::Execute {
            session: self.session,
            sql: sql.to_string(),
            reply: reply_tx,
        });
        if sent.is_err() {
            return Err("engine has stopped".to_string());
        }
        match reply_rx.recv() {
            Ok((result, in_txn)) => {
                self.in_txn = in_txn;
                result
            }
            Err(_) => Err("engine has stopped".to_string()),
        }
    }
    fn in_transaction(&self) -> bool {
        self.in_txn
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // Tell the engine to abort any transaction this session still holds.
        let _ = self.tx.send(Command::Close {
            session: self.session,
        });
    }
}

/// The engine thread's main loop: own the `Database`, and serve commands while
/// enforcing transaction exclusivity.
fn engine_loop(mut db: Database, rx: &Receiver<Command>) {
    // `owner` is the session that holds the open transaction, if any.
    let mut owner: Option<u64> = None;
    let mut pending: VecDeque<Command> = VecDeque::new();
    while let Ok(cmd) = rx.recv() {
        pending.push_back(cmd);
        drain(&mut db, &mut owner, &mut pending);
    }
}

/// Run every currently runnable pending command. A command is runnable when no
/// transaction is open, or it belongs to the session that owns the open one; a
/// `Close` always runs. Running a `BEGIN` makes its session the owner, which
/// blocks the rest until that session commits or rolls back.
fn drain(db: &mut Database, owner: &mut Option<u64>, pending: &mut VecDeque<Command>) {
    while let Some(i) = pending.iter().position(|c| runnable(c, *owner)) {
        match pending.remove(i).expect("index from position") {
            Command::Execute {
                session,
                sql,
                reply,
            } => {
                let result = db.execute(&sql).map_err(|e| e.to_string());
                let in_txn = db.in_transaction();
                *owner = if in_txn { Some(session) } else { None };
                let _ = reply.send((result, in_txn));
            }
            Command::Close { session } => {
                if *owner == Some(session) {
                    let _ = db.execute("ROLLBACK");
                    *owner = None;
                }
            }
        }
    }
}

/// Whether `cmd` may run given the current transaction `owner`.
const fn runnable(cmd: &Command, owner: Option<u64>) -> bool {
    match cmd {
        Command::Close { .. } => true,
        Command::Execute { session, .. } => match owner {
            None => true,
            Some(o) => *session == o,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, EngineActor) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        let actor = EngineActor::spawn(move || Database::open(&path).map_err(|e| e.to_string()))
            .expect("spawn");
        (dir, actor)
    }

    #[test]
    fn two_sessions_share_one_engine() {
        let (_dir, actor) = open();
        let mut a = actor.session();
        let mut b = actor.session();
        a.execute("CREATE TABLE t (id INT)").unwrap();
        // A second connection sees the first's committed (auto-commit) writes.
        a.execute("INSERT INTO t VALUES (1), (2)").unwrap();
        let out = b.execute("SELECT id FROM t").unwrap();
        match out {
            QueryOutcome::Rows { rows, .. } => assert_eq!(rows.len(), 2),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    #[test]
    fn a_transaction_is_isolated_until_commit() {
        let (_dir, actor) = open();
        let mut a = actor.session();
        a.execute("CREATE TABLE t (id INT)").unwrap();
        a.execute("INSERT INTO t VALUES (1)").unwrap();
        // While `a` holds an open transaction it owns the engine, so we drive it
        // to completion on this thread; `in_transaction` tracks the state.
        a.execute("BEGIN").unwrap();
        assert!(a.in_transaction());
        a.execute("INSERT INTO t VALUES (2)").unwrap();
        a.execute("ROLLBACK").unwrap();
        assert!(!a.in_transaction());
        // The rolled-back row is gone; a fresh session sees only the committed one.
        let mut b = actor.session();
        let out = b.execute("SELECT id FROM t").unwrap();
        match out {
            QueryOutcome::Rows { rows, .. } => assert_eq!(rows.len(), 1),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    #[test]
    fn concurrent_threads_share_one_engine() {
        let (_dir, actor) = open();
        let mut setup = actor.session();
        setup.execute("CREATE TABLE t (id INT)").unwrap();
        // Eight real OS threads each insert through their own session handle.
        let mut joins = Vec::new();
        for i in 0..8 {
            let mut s = actor.session();
            joins.push(thread::spawn(move || {
                s.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
        // Every concurrent insert landed.
        match setup.execute("SELECT id FROM t").unwrap() {
            QueryOutcome::Rows { rows, .. } => assert_eq!(rows.len(), 8),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    #[test]
    fn dropping_a_session_aborts_its_open_transaction() {
        let (_dir, actor) = open();
        let mut a = actor.session();
        a.execute("CREATE TABLE t (id INT)").unwrap();
        {
            let mut tx = actor.session();
            tx.execute("BEGIN").unwrap();
            tx.execute("INSERT INTO t VALUES (9)").unwrap();
            // `tx` drops here without committing: the engine must roll it back,
            // releasing ownership so `a` can run again.
        }
        let out = a.execute("SELECT id FROM t").unwrap();
        match out {
            QueryOutcome::Rows { rows, .. } => {
                assert!(rows.is_empty(), "the dropped transaction's row was kept");
            }
            other => panic!("expected rows, got {other:?}"),
        }
    }
}
