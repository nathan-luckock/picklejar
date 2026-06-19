//! Transaction-time travel: `SET transaction_time = <point>` runs a read against
//! the historical MVCC snapshot as of that transaction point, so a query sees the
//! database as it was then, bounded by retained version history. The point is the
//! database's own logical clock, a transaction-id watermark from `txid_current()`.
//! These tests pin the travel, that an as-of-past read walks the version chain to
//! the version live then, deletes reappearing, new rows being excluded, that
//! writes still act on the latest state, and a clean reset.

use picklejar::{Database, QueryOutcome, Value};
use tempfile::tempdir;

/// Run a query and return its rows, failing loudly on anything else.
fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        QueryOutcome::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// The single text value in a one-row, one-column result.
fn one_text(db: &mut Database, sql: &str) -> String {
    let r = rows(db, sql);
    assert_eq!(r.len(), 1, "expected one row from `{sql}`, got {r:?}");
    match &r[0][0] {
        Value::Text(s) => s.clone(),
        other => panic!("expected text from `{sql}`, got {other:?}"),
    }
}

/// A memory table with one row, inserted then updated, so its version chain has a
/// past version to travel back to. Returns the database and the transaction point
/// captured between the insert and the update.
fn one_row_history() -> (Database, u64) {
    let dir = tempdir().expect("tempdir");
    let path = Box::leak(Box::new(dir)).path().join("t.db");
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE m (id INT, v TEXT)").unwrap();
    db.execute("INSERT INTO m VALUES (1, 'v1')").unwrap();
    let point = db.current_txid();
    db.execute("UPDATE m SET v = 'v2' WHERE id = 1").unwrap();
    (db, point)
}

#[test]
fn as_of_a_past_point_sees_the_old_version() {
    let (mut db, point) = one_row_history();

    // Latest read: the updated value.
    assert_eq!(one_text(&mut db, "SELECT v FROM m WHERE id = 1"), "v2");

    // Travelled read: the value that was live at the captured point.
    db.execute(&format!("SET transaction_time = {point}"))
        .unwrap();
    assert_eq!(one_text(&mut db, "SELECT v FROM m WHERE id = 1"), "v1");

    // Reset returns to the latest.
    db.execute("RESET transaction_time").unwrap();
    assert_eq!(one_text(&mut db, "SELECT v FROM m WHERE id = 1"), "v2");
}

#[test]
fn a_deleted_row_reappears_when_travelling_before_the_delete() {
    let dir = tempdir().expect("tempdir");
    let path = Box::leak(Box::new(dir)).path().join("d.db");
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE m (id INT, v TEXT)").unwrap();
    db.execute("INSERT INTO m VALUES (1, 'x')").unwrap();
    let point = db.current_txid();
    db.execute("DELETE FROM m WHERE id = 1").unwrap();

    // Gone in the present.
    assert!(rows(&mut db, "SELECT v FROM m").is_empty());

    // Present again as of before the delete.
    db.execute(&format!("SET transaction_time = {point}"))
        .unwrap();
    assert_eq!(one_text(&mut db, "SELECT v FROM m"), "x");
}

#[test]
fn a_row_inserted_after_the_point_is_excluded() {
    let dir = tempdir().expect("tempdir");
    let path = Box::leak(Box::new(dir)).path().join("i.db");
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE m (id INT, v TEXT)").unwrap();
    db.execute("INSERT INTO m VALUES (1, 'a')").unwrap();
    let point = db.current_txid();
    db.execute("INSERT INTO m VALUES (2, 'b')").unwrap();

    db.execute(&format!("SET transaction_time = {point}"))
        .unwrap();
    let ids: Vec<i64> = rows(&mut db, "SELECT id FROM m ORDER BY id")
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            ref other => panic!("expected int, got {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1], "only the row that existed at the point");
}

#[test]
fn writes_act_on_the_latest_state_even_while_travelled() {
    let (mut db, point) = one_row_history();
    // Travel into the past, then write: the write must act on the latest state,
    // not the travelled one. Transaction-time travel is a read concept.
    db.execute(&format!("SET transaction_time = {point}"))
        .unwrap();
    db.execute("INSERT INTO m VALUES (2, 'new')").unwrap();
    // The travelled read still sees only the historical single row.
    assert_eq!(rows(&mut db, "SELECT id FROM m").len(), 1);
    // After reset, the latest state has both the updated row and the new one.
    db.execute("RESET transaction_time").unwrap();
    assert_eq!(rows(&mut db, "SELECT id FROM m").len(), 2);
    assert_eq!(one_text(&mut db, "SELECT v FROM m WHERE id = 1"), "v2");
}

#[test]
fn off_clears_the_point() {
    let (mut db, point) = one_row_history();
    db.execute(&format!("SET transaction_time = {point}"))
        .unwrap();
    assert_eq!(one_text(&mut db, "SELECT v FROM m WHERE id = 1"), "v1");
    db.execute("SET transaction_time = off").unwrap();
    assert_eq!(one_text(&mut db, "SELECT v FROM m WHERE id = 1"), "v2");
}

#[test]
fn txid_current_reports_the_advancing_watermark() {
    let dir = tempdir().expect("tempdir");
    let path = Box::leak(Box::new(dir)).path().join("x.db");
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE one (n INT)").unwrap();
    db.execute("INSERT INTO one VALUES (1)").unwrap();

    let before = match rows(&mut db, "SELECT txid_current() FROM one")[0][0] {
        Value::Int(n) => n,
        ref other => panic!("expected int, got {other:?}"),
    };
    // A committed write advances the watermark.
    db.execute("INSERT INTO one VALUES (2)").unwrap();
    let after = match rows(&mut db, "SELECT txid_current() FROM one")[0][0] {
        Value::Int(n) => n,
        ref other => panic!("expected int, got {other:?}"),
    };
    assert!(
        after > before,
        "txid_current advanced from {before} to {after}"
    );
}

#[test]
fn a_fresh_session_is_not_travelled() {
    let dir = tempdir().expect("tempdir");
    let path = Box::leak(Box::new(dir)).path().join("s.db");
    let point;
    {
        let mut db = Database::open(&path).expect("open");
        db.execute("CREATE TABLE m (id INT, v TEXT)").unwrap();
        db.execute("INSERT INTO m VALUES (1, 'v1')").unwrap();
        point = db.current_txid();
        db.execute("UPDATE m SET v = 'v2' WHERE id = 1").unwrap();
        db.execute(&format!("SET transaction_time = {point}"))
            .unwrap();
        assert_eq!(one_text(&mut db, "SELECT v FROM m WHERE id = 1"), "v1");
    }
    // A new session has no as-of point set: it reads the latest committed state.
    let mut db = Database::open(&path).expect("reopen");
    assert_eq!(one_text(&mut db, "SELECT v FROM m WHERE id = 1"), "v2");
}
