//! The physical plan: an executable, cost-annotated tree.
//!
//! [`plan`] lowers a [`LogicalPlan`] into a [`PhysicalPlan`], making the
//! cost-based decisions. The headline one (requirement M6) is the scan
//! choice: a `Filter` directly above a `Scan` is fused into either a
//! [`PhysicalPlan::SeqScan`] or a [`PhysicalPlan::IndexScan`], whichever the
//! cost model ([`crate::cost`]) says is cheaper.
//!
//! Every node carries `est_rows` and `est_cost` so EXPLAIN can show them and
//! parent operators can cost themselves from their children.

use rustdb_sql::{BinOp, Expr, JoinKind, SelectItem};

use crate::catalog::Catalog;
use crate::cost::{estimate_rows, index_scan_cost, sargable_index, selectivity, seq_scan_cost};
use crate::error::Result;
use crate::logical::LogicalPlan;

/// A node in the executable, cost-annotated plan tree.
#[derive(Clone, Debug, PartialEq)]
pub enum PhysicalPlan {
    /// Full table scan, optionally with a residual filter predicate.
    SeqScan {
        /// Table name.
        table: String,
        /// Column qualifier (alias or table name) for this scan's output.
        qualifier: String,
        /// Residual predicate applied per row, if any.
        predicate: Option<Expr>,
        /// Estimated output rows.
        est_rows: u64,
        /// Estimated cost.
        est_cost: f64,
    },
    /// Index scan driven by an indexed predicate.
    IndexScan {
        /// Table name.
        table: String,
        /// Column qualifier (alias or table name) for this scan's output.
        qualifier: String,
        /// Index used.
        index: String,
        /// The predicate (the indexed part drives the scan; the rest is
        /// residual).
        predicate: Expr,
        /// Estimated output rows.
        est_rows: u64,
        /// Estimated cost.
        est_cost: f64,
    },
    /// Per-row filter over a child (used when the child is not a base scan).
    Filter {
        /// The predicate.
        predicate: Expr,
        /// Child plan.
        input: Box<Self>,
        /// Estimated output rows.
        est_rows: u64,
        /// Estimated cost.
        est_cost: f64,
    },
    /// Projection.
    Project {
        /// Projection items.
        items: Vec<SelectItem>,
        /// Child plan.
        input: Box<Self>,
        /// Estimated output rows.
        est_rows: u64,
        /// Estimated cost.
        est_cost: f64,
    },
    /// Sort.
    Sort {
        /// Sort keys with direction.
        keys: Vec<(Expr, bool)>,
        /// Child plan.
        input: Box<Self>,
        /// Estimated output rows.
        est_rows: u64,
        /// Estimated cost.
        est_cost: f64,
    },
    /// Limit.
    Limit {
        /// Row cap.
        n: u64,
        /// Rows to skip first.
        offset: u64,
        /// Child plan.
        input: Box<Self>,
        /// Estimated output rows.
        est_rows: u64,
        /// Estimated cost.
        est_cost: f64,
    },
    /// Remove duplicate rows (`SELECT DISTINCT`).
    Distinct {
        /// Child plan.
        input: Box<Self>,
        /// Estimated output rows.
        est_rows: u64,
        /// Estimated cost.
        est_cost: f64,
    },
    /// Group-by aggregate.
    Aggregate {
        /// Grouping keys.
        group_by: Vec<Expr>,
        /// Aggregate function calls to compute.
        aggregates: Vec<Expr>,
        /// Child plan.
        input: Box<Self>,
        /// Estimated output rows.
        est_rows: u64,
        /// Estimated cost.
        est_cost: f64,
    },
    /// Nested-loop join: scan the right input once per left row. Works for
    /// any join predicate; cost is the cross product.
    NestedLoopJoin {
        /// Inner or left.
        kind: JoinKind,
        /// Left input.
        left: Box<Self>,
        /// Right input.
        right: Box<Self>,
        /// Join predicate.
        on: Expr,
        /// Estimated output rows.
        est_rows: u64,
        /// Estimated cost.
        est_cost: f64,
    },
    /// Hash join: build a hash table on one side, probe with the other.
    /// Valid only for an equality (equi-join) predicate; linear in the
    /// inputs rather than quadratic.
    HashJoin {
        /// Inner or left.
        kind: JoinKind,
        /// Build (left) input.
        left: Box<Self>,
        /// Probe (right) input.
        right: Box<Self>,
        /// Join predicate (an equality).
        on: Expr,
        /// Estimated output rows.
        est_rows: u64,
        /// Estimated cost.
        est_cost: f64,
    },
}

impl PhysicalPlan {
    /// Estimated output rows of this node.
    #[must_use]
    pub const fn est_rows(&self) -> u64 {
        match self {
            Self::SeqScan { est_rows, .. }
            | Self::IndexScan { est_rows, .. }
            | Self::Filter { est_rows, .. }
            | Self::Project { est_rows, .. }
            | Self::Sort { est_rows, .. }
            | Self::Limit { est_rows, .. }
            | Self::Distinct { est_rows, .. }
            | Self::Aggregate { est_rows, .. }
            | Self::NestedLoopJoin { est_rows, .. }
            | Self::HashJoin { est_rows, .. } => *est_rows,
        }
    }

    /// Estimated cumulative cost of this node (including its children).
    #[must_use]
    pub const fn est_cost(&self) -> f64 {
        match self {
            Self::SeqScan { est_cost, .. }
            | Self::IndexScan { est_cost, .. }
            | Self::Filter { est_cost, .. }
            | Self::Project { est_cost, .. }
            | Self::Sort { est_cost, .. }
            | Self::Limit { est_cost, .. }
            | Self::Distinct { est_cost, .. }
            | Self::Aggregate { est_cost, .. }
            | Self::NestedLoopJoin { est_cost, .. }
            | Self::HashJoin { est_cost, .. } => *est_cost,
        }
    }
}

/// Lower a logical plan into a cost-annotated physical plan.
#[allow(clippy::too_many_lines)]
pub fn plan(logical: &LogicalPlan, catalog: &Catalog) -> Result<PhysicalPlan> {
    match logical {
        // A bare scan: full table scan, no predicate.
        LogicalPlan::Scan { table, qualifier } => Ok(scan_no_predicate(table, qualifier, catalog)),

        // Filter directly over a scan: this is the cost-based scan choice.
        LogicalPlan::Filter { predicate, input } if matches!(**input, LogicalPlan::Scan { .. }) => {
            let LogicalPlan::Scan { table, qualifier } = &**input else {
                unreachable!("guarded by the match arm");
            };
            Ok(choose_scan(table, qualifier, predicate, catalog))
        }

        // Filter over a non-scan child: a standalone filter node.
        LogicalPlan::Filter { predicate, input } => {
            let child = plan(input, catalog)?;
            let est_rows = child.est_rows();
            #[allow(clippy::cast_precision_loss)]
            let est_cost = child.est_cost() + child.est_rows() as f64;
            Ok(PhysicalPlan::Filter {
                predicate: predicate.clone(),
                input: Box::new(child),
                est_rows,
                est_cost,
            })
        }

        LogicalPlan::Project { items, input } => {
            let child = plan(input, catalog)?;
            let est_rows = child.est_rows();
            let est_cost = child.est_cost();
            Ok(PhysicalPlan::Project {
                items: items.clone(),
                input: Box::new(child),
                est_rows,
                est_cost,
            })
        }

        LogicalPlan::Aggregate {
            group_by,
            aggregates,
            input,
        } => {
            let child = plan(input, catalog)?;
            // Output is at most one row per group; without group stats,
            // estimate the square root of input rows as a rough distinct
            // count (a common heuristic), clamped to [1, input].
            let in_rows = child.est_rows();
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let est_rows = (in_rows as f64).sqrt().ceil() as u64;
            let est_rows = est_rows.clamp(1, in_rows.max(1));
            #[allow(clippy::cast_precision_loss)]
            let est_cost = child.est_cost() + in_rows as f64;
            Ok(PhysicalPlan::Aggregate {
                group_by: group_by.clone(),
                aggregates: aggregates.clone(),
                input: Box::new(child),
                est_rows,
                est_cost,
            })
        }

        LogicalPlan::Sort { keys, input } => {
            let child = plan(input, catalog)?;
            let rows = child.est_rows();
            // n log n sort cost on top of the child.
            #[allow(clippy::cast_precision_loss)]
            let sort_work = (rows as f64) * (rows as f64 + 1.0).log2();
            let est_cost = child.est_cost() + sort_work;
            Ok(PhysicalPlan::Sort {
                keys: keys.clone(),
                input: Box::new(child),
                est_rows: rows,
                est_cost,
            })
        }

        LogicalPlan::Limit { n, offset, input } => {
            let child = plan(input, catalog)?;
            // After skipping `offset`, at most `n` rows remain.
            let est_rows = child.est_rows().saturating_sub(*offset).min(*n);
            let est_cost = child.est_cost();
            Ok(PhysicalPlan::Limit {
                n: *n,
                offset: *offset,
                input: Box::new(child),
                est_rows,
                est_cost,
            })
        }
        LogicalPlan::Distinct { input } => {
            let child = plan(input, catalog)?;
            // A scan of the child plus, worst case, every row distinct.
            let est_rows = child.est_rows();
            #[allow(clippy::cast_precision_loss)]
            let est_cost = child.est_cost() + child.est_rows() as f64;
            Ok(PhysicalPlan::Distinct {
                input: Box::new(child),
                est_rows,
                est_cost,
            })
        }

        LogicalPlan::Join {
            kind,
            left,
            right,
            on,
        } => Ok(choose_join(
            *kind,
            plan(left, catalog)?,
            plan(right, catalog)?,
            on,
        )),
    }
}

/// Choose a join algorithm. A nested-loop join works for any predicate but
/// costs the cross product (`left * right`); a hash join is linear
/// (`left + right`) but needs an equality predicate. Pick the hash join for
/// an equi-join whenever it is cheaper, which on any non-trivial input it
/// always is.
fn choose_join(kind: JoinKind, lp: PhysicalPlan, rp: PhysicalPlan, on: &Expr) -> PhysicalPlan {
    let child_cost = lp.est_cost() + rp.est_cost();
    #[allow(clippy::cast_precision_loss)]
    let nl_cost = (lp.est_rows() as f64).mul_add(rp.est_rows() as f64, child_cost);
    // Output-row estimate: the cross-product upper bound. Without join-key
    // statistics we cannot do better, so both algorithms share it.
    let est_rows = lp.est_rows().saturating_mul(rp.est_rows()).max(1);

    if is_equi_join(on) {
        #[allow(clippy::cast_precision_loss)]
        let hash_cost = (lp.est_rows() + rp.est_rows()) as f64 + child_cost;
        if hash_cost < nl_cost {
            return PhysicalPlan::HashJoin {
                kind,
                left: Box::new(lp),
                right: Box::new(rp),
                on: on.clone(),
                est_rows,
                est_cost: hash_cost,
            };
        }
    }

    PhysicalPlan::NestedLoopJoin {
        kind,
        left: Box::new(lp),
        right: Box::new(rp),
        on: on.clone(),
        est_rows,
        est_cost: nl_cost,
    }
}

/// An equi-join predicate is one whose every `AND` conjunct is an equality.
/// Such a predicate yields hash keys, so a hash join is applicable.
fn is_equi_join(on: &Expr) -> bool {
    match on {
        Expr::Binary { op: BinOp::Eq, .. } => true,
        Expr::Binary {
            op: BinOp::And,
            left,
            right,
        } => is_equi_join(left) && is_equi_join(right),
        _ => false,
    }
}

/// A full scan with no predicate: every row, cost = row count.
fn scan_no_predicate(table: &str, qualifier: &str, catalog: &Catalog) -> PhysicalPlan {
    let rows = catalog.get_table(table).map_or(0, |t| t.stats.row_count);
    PhysicalPlan::SeqScan {
        table: table.to_string(),
        qualifier: qualifier.to_string(),
        predicate: None,
        est_rows: rows,
        est_cost: seq_scan_cost(rows),
    }
}

/// Choose between a sequential scan and an index scan for `table` filtered by
/// `predicate`, using the cost model. This is the M6 decision.
fn choose_scan(table: &str, qualifier: &str, predicate: &Expr, catalog: &Catalog) -> PhysicalPlan {
    let Some(meta) = catalog.get_table(table) else {
        // The binder validated the table exists; defensively fall back.
        return PhysicalPlan::SeqScan {
            table: table.to_string(),
            qualifier: qualifier.to_string(),
            predicate: Some(predicate.clone()),
            est_rows: 0,
            est_cost: 0.0,
        };
    };
    let rows = meta.stats.row_count;
    let sel = selectivity(predicate, meta);
    let out_rows = estimate_rows(sel, rows);
    let seq_cost = seq_scan_cost(rows);

    // If a sargable indexed predicate exists and the index scan is cheaper,
    // use it.
    if let Some((index_name, _col)) = sargable_index(predicate, meta) {
        let idx_cost = index_scan_cost(rows, sel);
        if idx_cost < seq_cost {
            return PhysicalPlan::IndexScan {
                table: table.to_string(),
                qualifier: qualifier.to_string(),
                index: index_name.to_string(),
                predicate: predicate.clone(),
                est_rows: out_rows,
                est_cost: idx_cost,
            };
        }
    }

    PhysicalPlan::SeqScan {
        table: table.to_string(),
        qualifier: qualifier.to_string(),
        predicate: Some(predicate.clone()),
        est_rows: out_rows,
        est_cost: seq_cost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::bind;
    use crate::catalog::{Catalog, ColumnStats};
    use rustdb_sql::Parser;

    fn stmt(src: &str) -> rustdb_sql::Statement {
        Parser::from_sql(src).unwrap().parse_statement().unwrap()
    }

    /// A catalog with table `t (id INT, name TEXT)`, 1000 rows, id distinct
    /// `distinct_id`, and an index on `id` iff `indexed`.
    fn catalog(distinct_id: u64, indexed: bool) -> Catalog {
        let mut c = Catalog::new();
        c.apply(&stmt("CREATE TABLE t (id INT, name TEXT)"))
            .unwrap();
        if indexed {
            c.apply(&stmt("CREATE INDEX idx ON t (id)")).unwrap();
        }
        c.set_row_count("t", 1000).unwrap();
        c.set_column_stats(
            "t",
            "id",
            ColumnStats {
                distinct: distinct_id,
            },
        )
        .unwrap();
        c
    }

    fn physical(cat: &Catalog, src: &str) -> PhysicalPlan {
        // Strip the outer Project to reach the scan decision.
        let logical = bind(cat, &stmt(src)).unwrap();
        let LogicalPlan::Project { input, .. } = logical else {
            panic!("expected Project at root");
        };
        plan(&input, cat).unwrap()
    }

    #[test]
    fn selective_indexed_equality_picks_index_scan() {
        let c = catalog(1000, true); // high cardinality + index
        let p = physical(&c, "SELECT id FROM t WHERE id = 5");
        assert!(
            matches!(p, PhysicalPlan::IndexScan { .. }),
            "expected IndexScan, got {p:?}"
        );
        assert!(p.est_cost() < 1000.0, "index scan should cost < full scan");
        assert_eq!(p.est_rows(), 1, "equality on unique-ish col -> ~1 row");
    }

    #[test]
    fn no_index_picks_seq_scan() {
        let c = catalog(1000, false); // high cardinality but NO index
        let p = physical(&c, "SELECT id FROM t WHERE id = 5");
        assert!(matches!(p, PhysicalPlan::SeqScan { .. }), "got {p:?}");
    }

    #[test]
    fn low_cardinality_picks_seq_scan_even_with_index() {
        // distinct = 1 -> equality matches everything -> index loses.
        let c = catalog(1, true);
        let p = physical(&c, "SELECT id FROM t WHERE id = 5");
        assert!(
            matches!(p, PhysicalPlan::SeqScan { .. }),
            "a non-selective predicate should not use the index, got {p:?}"
        );
    }

    #[test]
    fn removing_index_flips_choice_back_to_seq_scan() {
        // Same selective predicate; only the index presence differs.
        let with_index = physical(&catalog(1000, true), "SELECT id FROM t WHERE id = 5");
        let without = physical(&catalog(1000, false), "SELECT id FROM t WHERE id = 5");
        assert!(matches!(with_index, PhysicalPlan::IndexScan { .. }));
        assert!(matches!(without, PhysicalPlan::SeqScan { .. }));
    }

    #[test]
    fn bare_scan_costs_all_rows() {
        let c = catalog(1000, true);
        let p = physical(&c, "SELECT * FROM t");
        assert!(matches!(
            p,
            PhysicalPlan::SeqScan {
                predicate: None,
                ..
            }
        ));
        assert_eq!(p.est_rows(), 1000);
        assert!((p.est_cost() - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn index_predicate_inside_and_is_used() {
        let c = catalog(1000, true);
        let p = physical(&c, "SELECT id FROM t WHERE name = 'x' AND id = 5");
        assert!(
            matches!(p, PhysicalPlan::IndexScan { .. }),
            "an indexed conjunct should drive an index scan, got {p:?}"
        );
    }

    #[test]
    fn limit_caps_estimated_rows() {
        let c = catalog(1000, false);
        let logical = bind(&c, &stmt("SELECT * FROM t LIMIT 10")).unwrap();
        let p = plan(&logical, &c).unwrap();
        assert_eq!(p.est_rows(), 10);
    }

    /// Two tables of `rows` rows each, for join tests.
    fn join_catalog(rows: u64) -> Catalog {
        let mut c = Catalog::new();
        c.apply(&stmt("CREATE TABLE orders (id INT, cid INT)"))
            .unwrap();
        c.apply(&stmt("CREATE TABLE customers (id INT, name TEXT)"))
            .unwrap();
        c.set_row_count("orders", rows).unwrap();
        c.set_row_count("customers", rows).unwrap();
        c
    }

    fn join_plan(cat: &Catalog, src: &str) -> PhysicalPlan {
        let logical = bind(cat, &stmt(src)).unwrap();
        let LogicalPlan::Project { input, .. } = logical else {
            panic!("expected Project at root");
        };
        plan(&input, cat).unwrap()
    }

    #[test]
    fn equi_join_on_sizable_inputs_picks_hash_join() {
        let c = join_catalog(1000);
        let p = join_plan(
            &c,
            "SELECT o.id FROM orders AS o INNER JOIN customers AS c ON o.cid = c.id",
        );
        assert!(
            matches!(p, PhysicalPlan::HashJoin { .. }),
            "equi-join should use a hash join, got {p:?}"
        );
        // Hash join (linear) must beat the nested-loop cross product.
        assert!(p.est_cost() < 1000.0 * 1000.0, "hash join should be linear");
    }

    #[test]
    fn non_equi_join_falls_back_to_nested_loop() {
        let c = join_catalog(1000);
        let p = join_plan(
            &c,
            "SELECT o.id FROM orders AS o INNER JOIN customers AS c ON o.cid > c.id",
        );
        assert!(
            matches!(p, PhysicalPlan::NestedLoopJoin { .. }),
            "a range join predicate has no hash key, got {p:?}"
        );
    }

    #[test]
    fn tiny_equi_join_keeps_nested_loop() {
        // With one row per side, the cross product (1) ties or beats the
        // hash build+probe (2), so the simpler nested loop wins.
        let c = join_catalog(1);
        let p = join_plan(
            &c,
            "SELECT o.id FROM orders AS o INNER JOIN customers AS c ON o.cid = c.id",
        );
        assert!(
            matches!(p, PhysicalPlan::NestedLoopJoin { .. }),
            "a trivially small join should not pay for a hash table, got {p:?}"
        );
    }
}
