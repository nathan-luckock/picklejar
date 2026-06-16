//! End-to-end engine session: drive the public `Database` API through a full
//! CREATE -> INSERT -> SELECT -> EXPLAIN script, the way the CLI does.

use rustdb::{Database, QueryOutcome, Value};
use tempfile::tempdir;

fn col_names(columns: &[String]) -> Vec<&str> {
    columns.iter().map(String::as_str).collect()
}

#[test]
fn create_insert_select_explain_session() {
    let dir = tempdir().expect("tempdir");
    let mut db = Database::open(dir.path().join("session.db")).expect("open");

    assert_eq!(
        db.execute("CREATE TABLE parts (id INT, name TEXT)")
            .unwrap(),
        QueryOutcome::Ddl
    );
    assert_eq!(
        db.execute("INSERT INTO parts (id, name) VALUES (3, 'bolt'), (1, 'nut'), (2, 'washer')")
            .unwrap(),
        QueryOutcome::Mutation { affected: 3 }
    );

    // WHERE + projection + ORDER BY a selected column + LIMIT.
    match db
        .execute("SELECT id, name FROM parts WHERE id > 1 ORDER BY id DESC LIMIT 1")
        .unwrap()
    {
        QueryOutcome::Rows { columns, rows } => {
            assert_eq!(col_names(&columns), ["id", "name"]);
            assert_eq!(rows, vec![vec![Value::Int(3), Value::Text("bolt".into())]]);
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // EXPLAIN renders the cost-annotated plan.
    match db
        .execute("EXPLAIN SELECT name FROM parts WHERE id = 2")
        .unwrap()
    {
        QueryOutcome::Explain(text) => {
            assert!(text.contains("SeqScan parts"), "plan:\n{text}");
            assert!(text.contains("predicate: (id = 2)"), "plan:\n{text}");
            // Costs are real now: the engine fed the 3-row count to the
            // planner, so the scan is not estimated at zero rows.
            assert!(
                text.contains("rows=3"),
                "plan should reflect 3 rows:\n{text}"
            );
        }
        other => panic!("expected explain, got {other:?}"),
    }
}

#[test]
fn order_by_a_non_projected_column() {
    let dir = tempdir().expect("tempdir");
    let mut db = Database::open(dir.path().join("orderby.db")).expect("open");
    db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    db.execute("INSERT INTO t (id, name) VALUES (2, 'b'), (1, 'a'), (3, 'c')")
        .unwrap();

    // Sort by `id` even though only `name` is selected. This used to fail.
    match db.execute("SELECT name FROM t ORDER BY id").unwrap() {
        QueryOutcome::Rows { columns, rows } => {
            assert_eq!(col_names(&columns), ["name"]);
            assert_eq!(
                rows,
                vec![
                    vec![Value::Text("a".into())],
                    vec![Value::Text("b".into())],
                    vec![Value::Text("c".into())],
                ]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn inner_and_left_joins() {
    let dir = tempdir().expect("tempdir");
    let mut db = Database::open(dir.path().join("join.db")).expect("open");
    db.execute("CREATE TABLE customers (id INT, name TEXT)")
        .unwrap();
    db.execute("CREATE TABLE orders (id INT, cid INT)").unwrap();
    db.execute("INSERT INTO customers (id, name) VALUES (1, 'alice'), (2, 'bob')")
        .unwrap();
    db.execute("INSERT INTO orders (id, cid) VALUES (10, 1), (11, 1)")
        .unwrap();

    // INNER JOIN: only customers with orders, one row per order.
    match db
        .execute("SELECT c.name, o.id FROM orders AS o INNER JOIN customers AS c ON o.cid = c.id")
        .unwrap()
    {
        QueryOutcome::Rows { columns, rows } => {
            assert_eq!(col_names(&columns), ["name", "id"]);
            assert_eq!(
                rows,
                vec![
                    vec![Value::Text("alice".into()), Value::Int(10)],
                    vec![Value::Text("alice".into()), Value::Int(11)],
                ]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // LEFT JOIN: bob has no orders, so o.id is NULL for bob.
    match db
        .execute("SELECT c.name, o.id FROM customers AS c LEFT JOIN orders AS o ON c.id = o.cid")
        .unwrap()
    {
        QueryOutcome::Rows { rows, .. } => {
            assert_eq!(
                rows,
                vec![
                    vec![Value::Text("alice".into()), Value::Int(10)],
                    vec![Value::Text("alice".into()), Value::Int(11)],
                    vec![Value::Text("bob".into()), Value::Null],
                ]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn group_by_and_whole_table_aggregates() {
    let dir = tempdir().expect("tempdir");
    let mut db = Database::open(dir.path().join("agg.db")).expect("open");
    db.execute("CREATE TABLE sales (region TEXT, amount INT)")
        .unwrap();
    db.execute(
        "INSERT INTO sales (region, amount) VALUES ('west', 100), ('east', 50), ('west', 200)",
    )
    .unwrap();

    // GROUP BY region with COUNT/SUM, groups sorted by key.
    match db
        .execute("SELECT region, COUNT(*), SUM(amount) FROM sales GROUP BY region")
        .unwrap()
    {
        QueryOutcome::Rows { columns, rows } => {
            assert_eq!(col_names(&columns), ["region", "COUNT(*)", "SUM(amount)"]);
            assert_eq!(
                rows,
                vec![
                    vec![Value::Text("east".into()), Value::Int(1), Value::Int(50)],
                    vec![Value::Text("west".into()), Value::Int(2), Value::Int(300)],
                ]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // Whole-table aggregate: one summary row, no GROUP BY.
    match db
        .execute("SELECT COUNT(*), MIN(amount), MAX(amount) FROM sales")
        .unwrap()
    {
        QueryOutcome::Rows { rows, .. } => {
            assert_eq!(
                rows,
                vec![vec![Value::Int(3), Value::Int(50), Value::Int(200)]]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Run `SELECT id FROM t ORDER BY id` and return the ids.
fn ids(db: &mut Database) -> Vec<i64> {
    match db.execute("SELECT id FROM t ORDER BY id").unwrap() {
        QueryOutcome::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                ref v => panic!("expected int, got {v:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn explicit_transactions_commit_and_rollback() {
    let dir = tempdir().expect("tempdir");
    let mut db = Database::open(dir.path().join("txn.db")).expect("open");
    db.execute("CREATE TABLE t (id INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();

    // BEGIN ... ROLLBACK discards the write, though it is visible inside the
    // transaction.
    assert_eq!(db.execute("BEGIN").unwrap(), QueryOutcome::Message("BEGIN"));
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    assert_eq!(ids(&mut db), vec![1, 2]);
    assert_eq!(
        db.execute("ROLLBACK").unwrap(),
        QueryOutcome::Message("ROLLBACK")
    );
    assert_eq!(ids(&mut db), vec![1]);

    // BEGIN ... COMMIT keeps the write.
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (3)").unwrap();
    assert_eq!(
        db.execute("COMMIT").unwrap(),
        QueryOutcome::Message("COMMIT")
    );
    assert_eq!(ids(&mut db), vec![1, 3]);

    // COMMIT/ROLLBACK with no open transaction, and a nested BEGIN, error.
    assert!(db.execute("COMMIT").is_err());
    assert!(db.execute("ROLLBACK").is_err());
    db.execute("BEGIN").unwrap();
    assert!(db.execute("BEGIN").is_err());
}

#[test]
fn insert_without_column_list_fills_all_columns() {
    let dir = tempdir().expect("tempdir");
    let mut db = Database::open(dir.path().join("ins.db")).expect("open");
    db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")
        .unwrap();
    match db.execute("SELECT id, name FROM t ORDER BY id").unwrap() {
        QueryOutcome::Rows { rows, .. } => assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Int(2), Value::Text("b".into())],
            ]
        ),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn update_and_delete() {
    let dir = tempdir().expect("tempdir");
    let mut db = Database::open(dir.path().join("upd.db")).expect("open");
    db.execute("CREATE TABLE t (id INT, qty INT)").unwrap();
    db.execute("INSERT INTO t (id, qty) VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();

    // UPDATE: the SET expression sees the existing value.
    assert_eq!(
        db.execute("UPDATE t SET qty = qty + 5 WHERE id = 2")
            .unwrap(),
        QueryOutcome::Mutation { affected: 1 }
    );
    match db.execute("SELECT id, qty FROM t ORDER BY id").unwrap() {
        QueryOutcome::Rows { rows, .. } => assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(10)],
                vec![Value::Int(2), Value::Int(25)],
                vec![Value::Int(3), Value::Int(30)],
            ]
        ),
        other => panic!("expected rows, got {other:?}"),
    }

    // DELETE the rows matching a predicate.
    assert_eq!(
        db.execute("DELETE FROM t WHERE qty > 25").unwrap(),
        QueryOutcome::Mutation { affected: 1 }
    );
    match db.execute("SELECT id FROM t ORDER BY id").unwrap() {
        QueryOutcome::Rows { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // No WHERE: UPDATE touches every row, DELETE clears the table.
    assert_eq!(
        db.execute("UPDATE t SET qty = 0").unwrap(),
        QueryOutcome::Mutation { affected: 2 }
    );
    assert_eq!(
        db.execute("DELETE FROM t").unwrap(),
        QueryOutcome::Mutation { affected: 2 }
    );
    match db.execute("SELECT COUNT(*) FROM t").unwrap() {
        QueryOutcome::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(0)]]),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn data_and_schema_survive_a_reopen() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("persist.db");

    // Session one: create a table and index, insert, then drop the handle.
    {
        let mut db = Database::open(&path).expect("open");
        db.execute("CREATE TABLE parts (id INT, name TEXT)")
            .unwrap();
        db.execute("CREATE INDEX idx ON parts (id)").unwrap();
        db.execute("INSERT INTO parts (id, name) VALUES (1, 'nut'), (2, 'bolt')")
            .unwrap();
    }

    // Session two: reopen the same path. The catalog and rows are back.
    let mut db = Database::open(&path).expect("reopen");
    assert_eq!(db.table_names(), vec!["parts".to_string()]);
    assert_eq!(db.columns("parts").expect("columns").len(), 2);

    match db
        .execute("SELECT id, name FROM parts ORDER BY id")
        .unwrap()
    {
        QueryOutcome::Rows { rows, .. } => {
            assert_eq!(
                rows,
                vec![
                    vec![Value::Int(1), Value::Text("nut".into())],
                    vec![Value::Int(2), Value::Text("bolt".into())],
                ]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // The rowid counter persisted too, so a new insert does not collide.
    db.execute("INSERT INTO parts (id, name) VALUES (3, 'washer')")
        .unwrap();
    match db.execute("SELECT id FROM parts ORDER BY id").unwrap() {
        QueryOutcome::Rows { rows, .. } => {
            assert_eq!(
                rows,
                vec![
                    vec![Value::Int(1)],
                    vec![Value::Int(2)],
                    vec![Value::Int(3)]
                ]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn introspection_lists_and_describes_tables() {
    let dir = tempdir().expect("tempdir");
    let mut db = Database::open(dir.path().join("introspect.db")).expect("open");
    db.execute("CREATE TABLE a (x INT)").unwrap();
    db.execute("CREATE TABLE b (y TEXT, z INT)").unwrap();

    assert_eq!(db.table_names(), vec!["a".to_string(), "b".to_string()]);
    let cols = db.columns("b").expect("table b");
    let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["y", "z"]);
    assert!(db.columns("ghost").is_none());
}
