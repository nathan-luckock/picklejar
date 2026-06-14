//! End-to-end planner tests: SQL text in, physical plan and EXPLAIN out.
//!
//! Each test parses DDL to build a [`Catalog`], parses a `SELECT`, binds and
//! plans it, and asserts both the chosen physical operators and the exact
//! EXPLAIN rendering. The EXPLAIN strings are full snapshots so the demo
//! output format is pinned.

use rustdb_planner::{bind, explain, plan, Catalog, ColumnStats, PhysicalPlan};
use rustdb_sql::{Parser, Statement};

/// Parse one statement.
fn stmt(src: &str) -> Statement {
    Parser::from_sql(src)
        .expect("lex")
        .parse_statement()
        .expect("parse")
}

/// Apply each DDL string to a fresh catalog.
fn catalog(ddl: &[&str]) -> Catalog {
    let mut c = Catalog::new();
    for d in ddl {
        c.apply(&stmt(d)).expect("apply DDL");
    }
    c
}

/// Bind + plan a SELECT against `cat`.
fn physical(cat: &Catalog, sql: &str) -> PhysicalPlan {
    let logical = bind(cat, &stmt(sql)).expect("bind");
    plan(&logical, cat).expect("plan")
}

/// A `parts` catalog: 1000 rows, unique `id`, index `idx_id` on `id`.
fn parts() -> Catalog {
    let mut c = catalog(&[
        "CREATE TABLE parts (id INT, name TEXT)",
        "CREATE INDEX idx_id ON parts (id)",
    ]);
    c.set_row_count("parts", 1000).unwrap();
    c.set_column_stats("parts", "id", ColumnStats { distinct: 1000 })
        .unwrap();
    c
}

#[test]
fn point_lookup_uses_the_index() {
    let c = parts();
    let p = physical(&c, "SELECT name FROM parts WHERE id = 5");

    // Project over an IndexScan (the Filter fused into the access path).
    let PhysicalPlan::Project { input, .. } = &p else {
        panic!("expected Project at root, got {p:?}");
    };
    assert!(
        matches!(**input, PhysicalPlan::IndexScan { .. }),
        "expected IndexScan under Project, got {input:?}"
    );

    // Full EXPLAIN snapshot: the demo artifact for M6.
    assert_eq!(
        explain(&p),
        "Project name  (rows=1 cost=11.0)\n  \
         IndexScan parts USING idx_id  (rows=1 cost=11.0)\n    \
         predicate: (id = 5)"
    );
}

#[test]
fn full_scan_when_no_predicate() {
    let c = parts();
    let p = physical(&c, "SELECT * FROM parts");

    let PhysicalPlan::Project { input, .. } = &p else {
        panic!("expected Project at root, got {p:?}");
    };
    assert!(
        matches!(
            **input,
            PhysicalPlan::SeqScan {
                predicate: None,
                ..
            }
        ),
        "expected a predicate-free SeqScan, got {input:?}"
    );
    assert_eq!(
        explain(&p),
        "Project *  (rows=1000 cost=1000.0)\n  SeqScan parts  (rows=1000 cost=1000.0)"
    );
}

#[test]
fn equi_join_uses_hash_join() {
    let mut c = catalog(&[
        "CREATE TABLE orders (id INT, cid INT)",
        "CREATE TABLE customers (id INT, name TEXT)",
    ]);
    c.set_row_count("orders", 1000).unwrap();
    c.set_row_count("customers", 1000).unwrap();

    let p = physical(
        &c,
        "SELECT o.id, c.name FROM orders AS o INNER JOIN customers AS c ON o.cid = c.id",
    );
    let PhysicalPlan::Project { input, .. } = &p else {
        panic!("expected Project at root, got {p:?}");
    };
    assert!(
        matches!(**input, PhysicalPlan::HashJoin { .. }),
        "a sizable equi-join should hash-join, got {input:?}"
    );

    let out = explain(&p);
    assert!(out.contains("HashJoin INNER ON (o.cid = c.id)"), "{out}");
    assert!(out.contains("SeqScan orders"), "{out}");
    assert!(out.contains("SeqScan customers"), "{out}");
}

#[test]
fn grouped_ordered_limited_query_stacks_operators() {
    let mut c = catalog(&["CREATE TABLE orders (id INT, cid INT, total INT)"]);
    c.set_row_count("orders", 1000).unwrap();

    let p = physical(
        &c,
        "SELECT cid FROM orders WHERE total > 0 GROUP BY cid ORDER BY cid DESC LIMIT 5",
    );

    // Limit is the root and caps the row estimate.
    assert!(matches!(p, PhysicalPlan::Limit { .. }), "got {p:?}");
    assert!(
        p.est_rows() <= 5,
        "LIMIT 5 must cap rows, got {}",
        p.est_rows()
    );

    // The full operator stack, top to bottom.
    let out = explain(&p);
    let lines: Vec<&str> = out.lines().map(str::trim_start).collect();
    assert!(lines[0].starts_with("Limit 5"), "{out}");
    assert!(lines[1].starts_with("Sort cid DESC"), "{out}");
    assert!(lines[2].starts_with("Project cid"), "{out}");
    assert!(lines[3].starts_with("Aggregate GROUP BY cid"), "{out}");
    assert!(out.contains("SeqScan orders"), "{out}");
    assert!(out.contains("predicate: (total > 0)"), "{out}");
}
