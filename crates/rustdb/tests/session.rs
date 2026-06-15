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
        }
        other => panic!("expected explain, got {other:?}"),
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
