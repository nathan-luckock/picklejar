//! The HNSW index path must be a transparent accelerator: when it is enabled, a
//! `SELECT ... ORDER BY col <op> :q LIMIT k` returns the same answer the exact
//! scan would, and it must never weaken the isolation or permission guarantees.
//! These tests pin both properties by running each query twice (index off, then
//! on) and comparing, and by checking that row-level security and table grants
//! still fence the indexed path.

use picklejar::{Database, QueryOutcome, Value};
use tempfile::tempdir;

/// Run a query and return its rows, failing loudly on anything else.
fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        QueryOutcome::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// A store of well-separated embeddings on a line, so the exact nearest-neighbor
/// answer is unambiguous and the approximate path is expected to reproduce it.
fn seeded_store() -> Database {
    let dir = tempdir().expect("tempdir");
    // Leak the dir so the file outlives this function; the OS reclaims it.
    let path = Box::leak(Box::new(dir)).path().join("v.db");
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE items (id INT, tag TEXT, e VECTOR(2))")
        .unwrap();
    for i in 0..60i64 {
        let x = f32::from(i16::try_from(i).expect("small"));
        db.execute(&format!(
            "INSERT INTO items VALUES ({i}, 't{}', '[{x}, 0]')",
            i % 3
        ))
        .unwrap();
    }
    db
}

#[test]
fn index_path_matches_exact_path_for_star_and_projection() {
    let mut db = seeded_store();

    // Several KNN shapes the index path accepts. Each must agree with the exact
    // scan for every distance operator we support.
    let queries = [
        "SELECT * FROM items ORDER BY e <-> '[3.2, 0]' LIMIT 5",
        "SELECT id, tag FROM items ORDER BY e <-> '[40.3, 0]' LIMIT 8",
        "SELECT id FROM items ORDER BY e <#> '[10, 0]' LIMIT 4",
        "SELECT id, e FROM items ORDER BY e <+> '[7.1, 0]' LIMIT 6",
    ];

    for q in queries {
        db.set_vector_index(false);
        let exact = rows(&mut db, q);
        db.set_vector_index(true);
        let indexed = rows(&mut db, q);
        assert_eq!(
            indexed, exact,
            "the index path disagreed with the exact path for `{q}`"
        );
    }
}

#[test]
fn unsupported_shapes_fall_through_unchanged() {
    let mut db = seeded_store();
    db.set_vector_index(true);

    // A WHERE clause, a join-like projection, no LIMIT, or a non-vector ORDER BY
    // are all outside the accepted shape; they must still return correct results
    // by falling through to the exact evaluator (identical to index-off).
    let shapes = [
        "SELECT id FROM items WHERE id < 10 ORDER BY e <-> '[2, 0]' LIMIT 3",
        "SELECT id FROM items ORDER BY e <-> '[2, 0]'",
        "SELECT id FROM items ORDER BY id LIMIT 3",
        "SELECT count(*) FROM items",
    ];
    for q in shapes {
        db.set_vector_index(true);
        let on = rows(&mut db, q);
        db.set_vector_index(false);
        let off = rows(&mut db, q);
        assert_eq!(on, off, "fall-through changed the answer for `{q}`");
    }
}

#[test]
fn index_path_preserves_row_level_security() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("rls.db");
    let mut db = Database::open(&path).expect("open");

    db.execute("CREATE TABLE memories (id INT, tenant TEXT, e VECTOR(2))")
        .unwrap();
    // Two tenants, interleaved, with overlapping embedding space so a naive
    // index over all rows would surface the other tenant's vectors.
    for i in 0..20i64 {
        let tenant = if i % 2 == 0 { "orion" } else { "vega" };
        let x = f32::from(i16::try_from(i).expect("small"));
        db.execute(&format!(
            "INSERT INTO memories VALUES ({i}, '{tenant}', '[{x}, 0]')"
        ))
        .unwrap();
    }
    db.execute("GRANT SELECT ON memories TO PUBLIC").unwrap();
    db.execute("CREATE ROLE orion LOGIN").unwrap();
    db.execute("CREATE ROLE vega LOGIN").unwrap();
    db.execute("CREATE POLICY tenant ON memories USING ((tenant = current_user()))")
        .unwrap();
    db.execute("ALTER TABLE memories ENABLE ROW LEVEL SECURITY")
        .unwrap();

    // With the index path ENABLED, orion's nearest-neighbor search must still see
    // only orion's rows. The folded RLS predicate becomes a WHERE, which the
    // index shape rejects, so the query falls through to the fenced exact path.
    db.set_vector_index(true);
    db.set_session_user("orion");
    let hits = rows(
        &mut db,
        "SELECT id, tenant FROM memories ORDER BY e <-> '[9, 0]' LIMIT 100",
    );
    assert!(!hits.is_empty(), "orion should see her own memories");
    for row in &hits {
        assert_eq!(
            row.get(1),
            Some(&Value::Text("orion".to_string())),
            "isolation breach: the indexed path surfaced another tenant's row"
        );
    }
    // Exactly orion's ten even-id rows, nothing leaked.
    assert_eq!(hits.len(), 10, "orion has exactly ten memories");
}

#[test]
fn a_write_invalidates_the_cached_index() {
    let mut db = seeded_store();
    db.set_vector_index(true);

    // Warm the cache: the nearest row to [100, 0] is the largest x, id 59.
    let before = rows(
        &mut db,
        "SELECT id FROM items ORDER BY e <-> '[100, 0]' LIMIT 1",
    );
    assert_eq!(before, vec![vec![Value::Int(59)]]);

    // Insert a row that sits exactly on the query point. If the cache were stale,
    // the old index would still rank id 59 first; invalidation must let id 999 win.
    db.execute("INSERT INTO items VALUES (999, 't0', '[100, 0]')")
        .unwrap();
    let after = rows(
        &mut db,
        "SELECT id FROM items ORDER BY e <-> '[100, 0]' LIMIT 1",
    );
    assert_eq!(after, vec![vec![Value::Int(999)]]);
}

#[test]
fn a_cached_index_is_not_reused_across_roles() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("roles.db");
    let mut db = Database::open(&path).expect("open");

    db.execute("CREATE TABLE items (id INT, e VECTOR(2))")
        .unwrap();
    db.execute("INSERT INTO items VALUES (1, '[1, 0]'), (2, '[0, 1]')")
        .unwrap();
    db.execute("CREATE ROLE alice LOGIN").unwrap();
    db.execute("CREATE ROLE mallory LOGIN").unwrap();
    db.execute("GRANT SELECT ON items TO alice").unwrap();
    db.set_vector_index(true);

    // alice has SELECT and warms an index for (alice, items, e, L2).
    db.set_session_user("alice");
    let a = rows(
        &mut db,
        "SELECT id FROM items ORDER BY e <-> '[1, 0]' LIMIT 1",
    );
    assert_eq!(a, vec![vec![Value::Int(1)]]);

    // mallory has no grant. The cache is keyed by role, so mallory cannot reuse
    // alice's index; the rebuild goes through a permission-checked SELECT and is
    // denied. (set_session_user is an API call, so it does not itself clear the
    // cache, which is exactly the case this guards.)
    db.set_session_user("mallory");
    let denied = db.execute("SELECT id FROM items ORDER BY e <-> '[1, 0]' LIMIT 1");
    assert!(
        denied.is_err(),
        "a role without SELECT read through another role's cached index"
    );
}

#[test]
fn enabling_rls_after_caching_still_fences() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("rls-late.db");
    let mut db = Database::open(&path).expect("open");

    db.execute("CREATE TABLE memories (id INT, tenant TEXT, e VECTOR(2))")
        .unwrap();
    for i in 0..10i64 {
        let tenant = if i % 2 == 0 { "orion" } else { "vega" };
        let x = f32::from(i16::try_from(i).expect("small"));
        db.execute(&format!(
            "INSERT INTO memories VALUES ({i}, '{tenant}', '[{x}, 0]')"
        ))
        .unwrap();
    }
    db.execute("GRANT SELECT ON memories TO PUBLIC").unwrap();
    db.execute("CREATE ROLE orion LOGIN").unwrap();
    db.execute("CREATE ROLE vega LOGIN").unwrap();
    db.set_vector_index(true);

    // Warm the cache before any policy exists: the owner sees all ten rows.
    let all = rows(
        &mut db,
        "SELECT id FROM memories ORDER BY e <-> '[5, 0]' LIMIT 100",
    );
    assert_eq!(all.len(), 10);

    // Turning RLS on is DDL, which clears the cache. orion's later query carries a
    // folded WHERE, so it falls through to the exact fenced path and sees only her
    // own rows, never the stale all-rows index.
    db.execute("CREATE POLICY tenant ON memories USING ((tenant = current_user()))")
        .unwrap();
    db.execute("ALTER TABLE memories ENABLE ROW LEVEL SECURITY")
        .unwrap();
    db.set_session_user("orion");
    let hits = rows(
        &mut db,
        "SELECT id, tenant FROM memories ORDER BY e <-> '[5, 0]' LIMIT 100",
    );
    assert!(!hits.is_empty(), "orion should see her own memories");
    for row in &hits {
        assert_eq!(
            row.get(1),
            Some(&Value::Text("orion".to_string())),
            "a stale all-rows index leaked another tenant's row after RLS was enabled"
        );
    }
    assert_eq!(hits.len(), 5, "orion has exactly five memories");
}

#[test]
fn index_path_respects_table_permissions() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("perm.db");
    let mut db = Database::open(&path).expect("open");

    db.execute("CREATE TABLE secrets (id INT, e VECTOR(2))")
        .unwrap();
    db.execute("INSERT INTO secrets VALUES (1, '[1, 0]'), (2, '[0, 1]')")
        .unwrap();
    db.execute("CREATE ROLE intruder LOGIN").unwrap();

    // intruder was never granted SELECT. Even with the index path on, the query
    // must be denied, because the candidate rows are fetched through the engine's
    // own permission-checked SELECT.
    db.set_vector_index(true);
    db.set_session_user("intruder");
    let denied = db.execute("SELECT id FROM secrets ORDER BY e <-> '[1, 0]' LIMIT 1");
    assert!(
        denied.is_err(),
        "the indexed path served a table the role cannot read"
    );
}
