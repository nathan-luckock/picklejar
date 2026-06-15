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
