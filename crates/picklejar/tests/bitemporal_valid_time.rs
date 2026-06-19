//! Valid-time travel: a session as-of instant (`SET valid_time = <timestamp>`)
//! filters reads on temporal tables (those with `valid_from` / `valid_to`
//! columns) to the rows valid at that instant, over the half-open interval
//! `[valid_from, valid_to)`. These tests pin the travel itself, the half-open
//! boundary, a clean reset, that non-temporal tables and writes are untouched,
//! and that aliases and joins are handled.

use picklejar::{Database, QueryOutcome, Value};
use tempfile::tempdir;

/// Run a query and return its rows, failing loudly on anything else.
fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        QueryOutcome::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// The single integer in a one-row, one-column result.
fn one_int(db: &mut Database, sql: &str) -> i64 {
    let r = rows(db, sql);
    assert_eq!(r.len(), 1, "expected one row from `{sql}`, got {r:?}");
    match r[0][0] {
        Value::Int(n) => n,
        ref other => panic!("expected an int from `{sql}`, got {other:?}"),
    }
}

/// A price history for one SKU as a temporal table: 100 from Jan to Jun 2020,
/// then 150 from Jun 2020 onward (the open-ended current row).
fn price_history() -> Database {
    let dir = tempdir().expect("tempdir");
    let path = Box::leak(Box::new(dir)).path().join("t.db");
    let mut db = Database::open(&path).expect("open");
    db.execute(
        "CREATE TABLE prices (sku TEXT, price INT, \
         valid_from TIMESTAMP, valid_to TIMESTAMP)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO prices VALUES \
         ('A', 100, TIMESTAMP '2020-01-01 00:00:00', TIMESTAMP '2020-06-01 00:00:00')",
    )
    .unwrap();
    db.execute(
        "INSERT INTO prices VALUES \
         ('A', 150, TIMESTAMP '2020-06-01 00:00:00', NULL)",
    )
    .unwrap();
    db
}

#[test]
fn as_of_instant_selects_the_row_valid_then() {
    let mut db = price_history();

    // Mid-way through the first interval: the old price.
    db.execute("SET valid_time = TIMESTAMP '2020-03-01 00:00:00'")
        .unwrap();
    assert_eq!(one_int(&mut db, "SELECT price FROM prices"), 100);

    // After the change, still inside the open-ended current row: the new price.
    db.execute("SET valid_time = TIMESTAMP '2020-09-01 00:00:00'")
        .unwrap();
    assert_eq!(one_int(&mut db, "SELECT price FROM prices"), 150);
}

#[test]
fn the_interval_is_half_open_at_the_boundary() {
    let mut db = price_history();

    // Exactly at the changeover instant, the half-open `[from, to)` rule excludes
    // the row whose `valid_to` is this instant and includes the successor whose
    // `valid_from` is this instant: no gap, no overlap, exactly one row.
    db.execute("SET valid_time = TIMESTAMP '2020-06-01 00:00:00'")
        .unwrap();
    assert_eq!(one_int(&mut db, "SELECT price FROM prices"), 150);

    // One microsecond before the changeover: still the old row.
    db.execute("SET valid_time = TIMESTAMP '2020-05-31 23:59:59.999999'")
        .unwrap();
    assert_eq!(one_int(&mut db, "SELECT price FROM prices"), 100);
}

#[test]
fn before_all_history_returns_nothing() {
    let mut db = price_history();
    db.execute("SET valid_time = TIMESTAMP '2019-01-01 00:00:00'")
        .unwrap();
    assert!(
        rows(&mut db, "SELECT price FROM prices").is_empty(),
        "no row is valid before the first valid_from"
    );
}

#[test]
fn off_and_reset_read_the_latest_full_history() {
    let mut db = price_history();

    db.execute("SET valid_time = TIMESTAMP '2020-03-01 00:00:00'")
        .unwrap();
    assert_eq!(rows(&mut db, "SELECT price FROM prices").len(), 1);

    // `off` clears the instant: the read sees every version again.
    db.execute("SET valid_time = off").unwrap();
    assert_eq!(
        rows(&mut db, "SELECT price FROM prices ORDER BY price").len(),
        2
    );

    // `RESET valid_time` is the same clear.
    db.execute("SET valid_time = TIMESTAMP '2020-03-01 00:00:00'")
        .unwrap();
    db.execute("RESET valid_time").unwrap();
    assert_eq!(rows(&mut db, "SELECT price FROM prices").len(), 2);
}

#[test]
fn travel_respects_table_alias() {
    let mut db = price_history();
    db.execute("SET valid_time = TIMESTAMP '2020-03-01 00:00:00'")
        .unwrap();
    // The validity predicate is qualified by the alias, not the table name.
    assert_eq!(
        one_int(&mut db, "SELECT p.price FROM prices p WHERE p.sku = 'A'"),
        100
    );
}

#[test]
fn non_temporal_tables_are_untouched_by_travel() {
    let mut db = price_history();
    db.execute("CREATE TABLE notes (id INT, body TEXT)")
        .unwrap();
    db.execute("INSERT INTO notes VALUES (1, 'x')").unwrap();
    db.execute("INSERT INTO notes VALUES (2, 'y')").unwrap();

    // An instant that hides one price row must not hide anything in a table with
    // no valid_from/valid_to: the travel only applies to temporal tables.
    db.execute("SET valid_time = TIMESTAMP '2020-03-01 00:00:00'")
        .unwrap();
    assert_eq!(rows(&mut db, "SELECT id FROM notes").len(), 2);
    // The temporal table is still filtered in the same session.
    assert_eq!(rows(&mut db, "SELECT price FROM prices").len(), 1);
}

#[test]
fn join_filters_only_the_temporal_side() {
    let mut db = price_history();
    db.execute("CREATE TABLE skus (sku TEXT, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO skus VALUES ('A', 'Widget')")
        .unwrap();

    db.execute("SET valid_time = TIMESTAMP '2020-09-01 00:00:00'")
        .unwrap();
    let r = rows(
        &mut db,
        "SELECT s.name, p.price FROM skus s \
         JOIN prices p ON s.sku = p.sku",
    );
    assert_eq!(r.len(), 1, "exactly the one price valid at the instant");
    assert_eq!(r[0][1], Value::Int(150));
}

#[test]
fn writes_act_on_the_latest_state_not_the_instant() {
    let mut db = price_history();
    // Travel to a past instant, then delete: the delete must act on the latest
    // state (every matching row), not only the rows valid at the instant. Valid
    // time travel is a read concept.
    db.execute("SET valid_time = TIMESTAMP '2020-03-01 00:00:00'")
        .unwrap();
    db.execute("DELETE FROM prices WHERE sku = 'A'").unwrap();
    db.execute("RESET valid_time").unwrap();
    assert!(
        rows(&mut db, "SELECT price FROM prices").is_empty(),
        "the delete removed both versions, not just the one valid at the instant"
    );
}

#[test]
fn the_instant_survives_a_reopen_is_not_expected_but_a_fresh_session_reads_latest() {
    // A session setting is per-connection and not persisted: a freshly opened
    // database has no instant set and reads the latest state.
    let dir = tempdir().expect("tempdir");
    let path = Box::leak(Box::new(dir)).path().join("p.db");
    {
        let mut db = Database::open(&path).expect("open");
        db.execute(
            "CREATE TABLE prices (sku TEXT, price INT, valid_from TIMESTAMP, valid_to TIMESTAMP)",
        )
        .unwrap();
        db.execute("INSERT INTO prices VALUES ('A', 100, TIMESTAMP '2020-01-01 00:00:00', NULL)")
            .unwrap();
        db.execute("SET valid_time = TIMESTAMP '2019-01-01 00:00:00'")
            .unwrap();
        assert!(rows(&mut db, "SELECT price FROM prices").is_empty());
    }
    let mut db = Database::open(&path).expect("reopen");
    assert_eq!(one_int(&mut db, "SELECT price FROM prices"), 100);
}

#[test]
fn travel_composes_with_row_level_security() {
    let dir = tempdir().expect("tempdir");
    let path = Box::leak(Box::new(dir)).path().join("r.db");
    let mut db = Database::open(&path).expect("open");

    // A temporal memory table fenced by tenant. Tenant a has a superseded fact
    // and a current one; tenant b has its own current fact.
    db.execute(
        "CREATE TABLE mem (tenant TEXT, fact TEXT, valid_from TIMESTAMP, valid_to TIMESTAMP)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO mem VALUES \
         ('a', 'old-a', TIMESTAMP '2020-01-01 00:00:00', TIMESTAMP '2020-06-01 00:00:00')",
    )
    .unwrap();
    db.execute("INSERT INTO mem VALUES ('a', 'new-a', TIMESTAMP '2020-06-01 00:00:00', NULL)")
        .unwrap();
    db.execute("INSERT INTO mem VALUES ('b', 'b-fact', TIMESTAMP '2020-01-01 00:00:00', NULL)")
        .unwrap();
    db.execute("GRANT SELECT ON mem TO PUBLIC").unwrap();
    db.execute("CREATE POLICY tenant ON mem USING ((tenant = current_user))")
        .unwrap();
    db.execute("ALTER TABLE mem ENABLE ROW LEVEL SECURITY")
        .unwrap();
    db.execute("CREATE ROLE a LOGIN").unwrap();
    db.execute("CREATE ROLE b LOGIN").unwrap();

    // As tenant a, travel into the first interval: the tenant fence and the
    // validity predicate both apply, so the only row is a's superseded fact.
    db.set_session_user("a");
    db.execute("SET valid_time = TIMESTAMP '2020-03-01 00:00:00'")
        .unwrap();
    let r = rows(&mut db, "SELECT fact FROM mem");
    assert_eq!(r, vec![vec![Value::Text("old-a".into())]]);

    // Clearing the instant lifts only the time filter; the tenant fence stays,
    // so a still never sees b's facts.
    db.execute("RESET valid_time").unwrap();
    let mut facts: Vec<String> = rows(&mut db, "SELECT fact FROM mem")
        .into_iter()
        .map(|row| match &row[0] {
            Value::Text(s) => s.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect();
    facts.sort();
    assert_eq!(facts, vec!["new-a".to_string(), "old-a".to_string()]);
}
