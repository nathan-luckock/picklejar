//! EXPLAIN rendering: a physical plan as a human-readable, cost-annotated
//! indented tree.
//!
//! This is the evidence for requirement M6. Every node prints its operator
//! and its `(rows=.. cost=..)` estimate, so the cost-based choices (index vs
//! seq scan, hash vs nested-loop join) are visible at a glance. Scans and
//! filters print their predicate on an indented line below the node.

use std::fmt::Write as _;

use picklejar_sql::JoinKind;

use crate::physical::PhysicalPlan;

/// Render a physical plan as the indented, cost-annotated tree that
/// `EXPLAIN` returns. The result has no trailing newline.
#[must_use]
pub fn explain(plan: &PhysicalPlan) -> String {
    let mut out = String::new();
    render(plan, 0, &mut out);
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Short tag for a join kind, e.g. `INNER`.
const fn kind_tag(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "INNER",
        JoinKind::Left => "LEFT",
        JoinKind::Right => "RIGHT",
        JoinKind::Full => "FULL",
    }
}

#[allow(clippy::too_many_lines)]
fn render(plan: &PhysicalPlan, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let stats = format!("(rows={} cost={:.1})", plan.est_rows(), plan.est_cost());
    // `writeln!` into a String is infallible; the `let _` discards the Result.
    match plan {
        PhysicalPlan::SeqScan {
            table, predicate, ..
        } => {
            let _ = writeln!(out, "{pad}SeqScan {table}  {stats}");
            if let Some(p) = predicate {
                let _ = writeln!(out, "{pad}  predicate: {p}");
            }
        }
        PhysicalPlan::IndexScan {
            table,
            index,
            predicate,
            ..
        } => {
            let _ = writeln!(out, "{pad}IndexScan {table} USING {index}  {stats}");
            let _ = writeln!(out, "{pad}  predicate: {predicate}");
        }
        PhysicalPlan::Filter {
            predicate, input, ..
        } => {
            let _ = writeln!(out, "{pad}Filter  {stats}");
            let _ = writeln!(out, "{pad}  predicate: {predicate}");
            render(input, depth + 1, out);
        }
        PhysicalPlan::Project { items, input, .. } => {
            let cols = items
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "{pad}Project {cols}  {stats}");
            render(input, depth + 1, out);
        }
        PhysicalPlan::Window { windows, input, .. } => {
            let ws = windows
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "{pad}Window [{ws}]  {stats}");
            render(input, depth + 1, out);
        }
        PhysicalPlan::Sort { keys, input, .. } => {
            let ks = keys
                .iter()
                .map(|(e, desc)| {
                    if *desc {
                        format!("{e} DESC")
                    } else {
                        e.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "{pad}Sort {ks}  {stats}");
            render(input, depth + 1, out);
        }
        PhysicalPlan::Limit {
            n, offset, input, ..
        } => {
            if *offset > 0 {
                let _ = writeln!(out, "{pad}Limit {n} OFFSET {offset}  {stats}");
            } else {
                let _ = writeln!(out, "{pad}Limit {n}  {stats}");
            }
            render(input, depth + 1, out);
        }
        PhysicalPlan::Distinct { input, .. } => {
            let _ = writeln!(out, "{pad}Distinct  {stats}");
            render(input, depth + 1, out);
        }
        PhysicalPlan::Union {
            op,
            all,
            left,
            right,
            ..
        } => {
            let _ = writeln!(
                out,
                "{pad}{}{}  {stats}",
                op.keyword(),
                if *all { " ALL" } else { "" }
            );
            render(left, depth + 1, out);
            render(right, depth + 1, out);
        }
        PhysicalPlan::DerivedScan { plan, alias, .. } => {
            let _ = writeln!(out, "{pad}DerivedScan AS {alias}  {stats}");
            render(plan, depth + 1, out);
        }
        PhysicalPlan::Aggregate {
            group_by,
            aggregates,
            input,
            ..
        } => {
            let keys = group_by
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let aggs = aggregates
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "{pad}Aggregate GROUP BY [{keys}] AGG [{aggs}]  {stats}"
            );
            render(input, depth + 1, out);
        }
        PhysicalPlan::NestedLoopJoin {
            kind,
            left,
            right,
            on,
            ..
        } => {
            let _ = writeln!(
                out,
                "{pad}NestedLoopJoin {} ON {on}  {stats}",
                kind_tag(*kind)
            );
            render(left, depth + 1, out);
            render(right, depth + 1, out);
        }
        PhysicalPlan::HashJoin {
            kind,
            left,
            right,
            on,
            ..
        } => {
            let _ = writeln!(out, "{pad}HashJoin {} ON {on}  {stats}", kind_tag(*kind));
            render(left, depth + 1, out);
            render(right, depth + 1, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::bind;
    use crate::catalog::{Catalog, ColumnStats};
    use crate::physical::plan;
    use picklejar_sql::Parser;

    fn stmt(src: &str) -> picklejar_sql::Statement {
        Parser::from_sql(src).unwrap().parse_statement().unwrap()
    }

    fn explain_sql(cat: &Catalog, src: &str) -> String {
        let logical = bind(cat, &stmt(src)).unwrap();
        explain(&plan(&logical, cat).unwrap())
    }

    fn parts_catalog() -> Catalog {
        let mut c = Catalog::new();
        c.apply(&stmt("CREATE TABLE parts (id INT, name TEXT)"))
            .unwrap();
        c.apply(&stmt("CREATE INDEX idx_id ON parts (id)")).unwrap();
        c.set_row_count("parts", 1000).unwrap();
        c.set_column_stats(
            "parts",
            "id",
            ColumnStats {
                distinct: 1000,
                ..Default::default()
            },
        )
        .unwrap();
        c
    }

    #[test]
    fn index_scan_explain_shows_index_and_predicate() {
        let c = parts_catalog();
        let out = explain_sql(&c, "SELECT id FROM parts WHERE id = 5");
        // The index path, with rows and cost annotated.
        assert!(
            out.contains("IndexScan parts USING idx_id"),
            "missing index line:\n{out}"
        );
        assert!(
            out.contains("predicate: (id = 5)"),
            "missing predicate:\n{out}"
        );
        assert!(out.contains("Project id"), "missing project:\n{out}");
        assert!(out.contains("rows="), "missing row estimate:\n{out}");
    }

    #[test]
    fn seq_scan_explain_when_no_index() {
        let mut c = Catalog::new();
        c.apply(&stmt("CREATE TABLE t (id INT, name TEXT)"))
            .unwrap();
        c.set_row_count("t", 1000).unwrap();
        let out = explain_sql(&c, "SELECT * FROM t WHERE id = 5");
        assert!(out.contains("SeqScan t"), "expected SeqScan:\n{out}");
        assert!(
            out.contains("predicate: (id = 5)"),
            "missing predicate:\n{out}"
        );
    }

    #[test]
    fn join_explain_renders_join_node() {
        let mut c = Catalog::new();
        c.apply(&stmt("CREATE TABLE orders (id INT, cid INT)"))
            .unwrap();
        c.apply(&stmt("CREATE TABLE customers (id INT, name TEXT)"))
            .unwrap();
        c.set_row_count("orders", 1000).unwrap();
        c.set_row_count("customers", 1000).unwrap();
        let out = explain_sql(
            &c,
            "SELECT o.id FROM orders AS o INNER JOIN customers AS c ON o.cid = c.id",
        );
        // An equi-join on sizable inputs should choose the hash join.
        assert!(
            out.contains("HashJoin INNER ON"),
            "expected HashJoin:\n{out}"
        );
        assert!(out.contains("SeqScan orders"), "missing left scan:\n{out}");
        assert!(
            out.contains("SeqScan customers"),
            "missing right scan:\n{out}"
        );
    }
}
