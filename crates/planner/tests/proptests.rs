//! Planner cost-model property test.
//!
//! The invariant the optimizer must never violate: for any catalog and any
//! predicate, the scan it chooses costs no more than a full sequential scan.
//! An index scan is only ever picked when it is genuinely cheaper, so the
//! planner can never cost-regress a query.
//!
//! Oracle: `chosen_scan.est_cost() <= seq_scan_cost(row_count)`.

use proptest::prelude::*;
use rustdb_planner::cost::seq_scan_cost;
use rustdb_planner::{bind, plan, Catalog, ColumnStats, PhysicalPlan};
use rustdb_sql::{Parser, Statement};

fn stmt(src: &str) -> Statement {
    Parser::from_sql(src)
        .expect("lex")
        .parse_statement()
        .expect("parse")
}

/// A single `column op literal` comparison as SQL text.
fn conjunct() -> impl Strategy<Value = String> {
    let col = prop_oneof![Just("id"), Just("val")];
    let op = prop_oneof![
        Just("="),
        Just("!="),
        Just("<"),
        Just("<="),
        Just(">"),
        Just(">=")
    ];
    (col, op, 0i64..10_000).prop_map(|(c, o, lit)| format!("{c} {o} {lit}"))
}

/// A WHERE predicate: one to three conjuncts joined by AND.
fn predicate() -> impl Strategy<Value = String> {
    prop::collection::vec(conjunct(), 1..=3).prop_map(|parts| parts.join(" AND "))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// The chosen scan never costs more than a full table scan.
    #[test]
    fn chosen_scan_never_costs_more_than_seq_scan(
        rows in 0u64..100_000,
        distinct in 1u64..100_000,
        indexed in any::<bool>(),
        pred in predicate(),
    ) {
        let mut c = Catalog::new();
        c.apply(&stmt("CREATE TABLE t (id INT, val INT)")).unwrap();
        if indexed {
            c.apply(&stmt("CREATE INDEX idx ON t (id)")).unwrap();
        }
        c.set_row_count("t", rows).unwrap();
        c.set_column_stats("t", "id", ColumnStats { distinct }).unwrap();

        let sql = format!("SELECT * FROM t WHERE {pred}");
        let logical = bind(&c, &stmt(&sql)).unwrap();
        let physical = plan(&logical, &c).unwrap();

        // Reach through the projection to the access path.
        let scan = match physical {
            PhysicalPlan::Project { input, .. } => *input,
            other => other,
        };

        let seq = seq_scan_cost(rows);
        prop_assert!(
            scan.est_cost() <= seq + 1e-9,
            "chosen cost {} exceeds seq scan cost {} for `{}` (indexed={})",
            scan.est_cost(),
            seq,
            sql,
            indexed,
        );

        // And whatever it chose, it is always a base access path here.
        prop_assert!(
            matches!(scan, PhysicalPlan::SeqScan { .. } | PhysicalPlan::IndexScan { .. }),
            "single-table WHERE should fuse into a scan, got {scan:?}"
        );
    }
}
