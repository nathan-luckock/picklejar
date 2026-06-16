//! The binder: lower a parsed `SELECT` into a [`LogicalPlan`], resolving
//! names against the [`Catalog`].
//!
//! Emits nodes bottom-up in SQL's logical order (Scan, joins, WHERE, GROUP
//! BY, projection, ORDER BY, LIMIT) and validates that every referenced
//! table and column exists. A single-table WHERE is placed directly above
//! the Scan (predicate pushdown); with joins it sits above the join tree.

use rustdb_sql::{Expr, Select, SelectItem, Statement, TableRef};

use crate::catalog::{Catalog, TableMeta};
use crate::error::{PlanError, Result};
use crate::logical::LogicalPlan;

/// One table visible in a query: the name used to qualify its columns
/// (alias if present, else table name) and its catalog metadata.
struct ScopeEntry<'c> {
    qualifier: String,
    meta: &'c TableMeta,
}

/// Bind a statement to a logical plan. Only `SELECT` is supported here; DDL
/// goes through [`Catalog::apply`] and DML lowering arrives with the
/// executor.
pub fn bind(catalog: &Catalog, stmt: &Statement) -> Result<LogicalPlan> {
    match stmt {
        Statement::Select(select) => bind_select(catalog, select),
        other => Err(PlanError::Unsupported(format!("cannot plan: {other}"))),
    }
}

fn bind_select(catalog: &Catalog, select: &Select) -> Result<LogicalPlan> {
    // 1. Build the scope (FROM + joins) and the join tree.
    let mut scope: Vec<ScopeEntry<'_>> = Vec::new();
    let mut plan = scan_table(catalog, &select.from, &mut scope)?;
    for join in &select.joins {
        let right = scan_table(catalog, &join.table, &mut scope)?;
        resolve_expr(&join.on, &scope)?;
        plan = LogicalPlan::Join {
            kind: join.kind,
            left: Box::new(plan),
            right: Box::new(right),
            on: join.on.clone(),
        };
    }

    // 2. WHERE.
    if let Some(predicate) = &select.where_clause {
        resolve_expr(predicate, &scope)?;
        plan = LogicalPlan::Filter {
            predicate: predicate.clone(),
            input: Box::new(plan),
        };
    }

    // 3. Aggregation. Trigger on an explicit GROUP BY or on aggregate
    //    functions in the projection (a bare `SELECT COUNT(*)` groups the
    //    whole table into one row).
    let mut aggregates = collect_aggregates(&select.projections);
    // HAVING may reference aggregates the projection does not, so they must be
    // computed too.
    if let Some(having) = &select.having {
        collect_aggs_in(having, &mut aggregates);
    }
    if !select.group_by.is_empty() || !aggregates.is_empty() {
        for key in &select.group_by {
            resolve_expr(key, &scope)?;
        }
        for agg in &aggregates {
            resolve_expr(agg, &scope)?;
        }
        plan = LogicalPlan::Aggregate {
            group_by: select.group_by.clone(),
            aggregates,
            input: Box::new(plan),
        };
    }

    // 3b. HAVING: a filter over the aggregated rows. Its column and aggregate
    //     references resolve against the same scope (an aggregate reference
    //     reads back the column the aggregate operator emits).
    if let Some(having) = &select.having {
        resolve_expr(having, &scope)?;
        plan = LogicalPlan::Filter {
            predicate: having.clone(),
            input: Box::new(plan),
        };
    }

    // 4. ORDER BY. Placed *below* the projection so a sort key can be any
    //    column in scope, not only the projected ones (SQL allows ORDER BY on
    //    columns absent from the SELECT list). The projection above preserves
    //    row order, so the final output is still sorted.
    if !select.order_by.is_empty() {
        let mut keys = Vec::with_capacity(select.order_by.len());
        for item in &select.order_by {
            resolve_expr(&item.expr, &scope)?;
            keys.push((item.expr.clone(), item.desc));
        }
        plan = LogicalPlan::Sort {
            keys,
            input: Box::new(plan),
        };
    }

    // 5. Projection. Resolve every projected expression; `*` needs nothing.
    for item in &select.projections {
        if let SelectItem::Expr(expr, _) = item {
            resolve_expr(expr, &scope)?;
        }
    }
    plan = LogicalPlan::Project {
        items: select.projections.clone(),
        input: Box::new(plan),
    };

    // 5b. DISTINCT: dedup the projected rows (so it sees the output columns).
    if select.distinct {
        plan = LogicalPlan::Distinct {
            input: Box::new(plan),
        };
    }

    // 6. LIMIT.
    if let Some(n) = select.limit {
        plan = LogicalPlan::Limit {
            n,
            input: Box::new(plan),
        };
    }

    Ok(plan)
}

/// Build a Scan for `table_ref`, checking the table exists and adding it to
/// `scope`.
fn scan_table<'c>(
    catalog: &'c Catalog,
    table_ref: &TableRef,
    scope: &mut Vec<ScopeEntry<'c>>,
) -> Result<LogicalPlan> {
    let meta = catalog
        .get_table(&table_ref.name)
        .ok_or_else(|| PlanError::UnknownTable(table_ref.name.clone()))?;
    let qualifier = table_ref
        .alias
        .clone()
        .unwrap_or_else(|| table_ref.name.clone());
    scope.push(ScopeEntry {
        qualifier: qualifier.clone(),
        meta,
    });
    Ok(LogicalPlan::Scan {
        table: table_ref.name.clone(),
        qualifier,
    })
}

/// Validate that every column reference in `expr` resolves against `scope`.
fn resolve_expr(expr: &Expr, scope: &[ScopeEntry<'_>]) -> Result<()> {
    match expr {
        Expr::Literal(_) | Expr::Star => Ok(()),
        Expr::Column(name) => resolve_bare_column(name, scope),
        Expr::QualifiedColumn(qualifier, column) => {
            resolve_qualified_column(qualifier, column, scope)
        }
        Expr::Binary { left, right, .. } => {
            resolve_expr(left, scope)?;
            resolve_expr(right, scope)
        }
        Expr::Unary { expr, .. } => resolve_expr(expr, scope),
        Expr::Func { args, .. } => {
            for arg in args {
                resolve_expr(arg, scope)?;
            }
            Ok(())
        }
    }
}

/// Whether `name` (already upper-cased by the parser) is an aggregate.
fn is_aggregate(name: &str) -> bool {
    matches!(name, "COUNT" | "SUM" | "MIN" | "MAX" | "AVG")
}

/// Push `expr` into `out` unless an equal (by printed form) entry is present.
fn push_unique(out: &mut Vec<Expr>, expr: &Expr) {
    let printed = expr.to_string();
    if !out.iter().any(|e| e.to_string() == printed) {
        out.push(expr.clone());
    }
}

/// Collect the aggregate calls in the projection list, deduplicated by their
/// printed form (so `SUM(x)` used twice is computed once).
fn collect_aggregates(projections: &[SelectItem]) -> Vec<Expr> {
    let mut found: Vec<Expr> = Vec::new();
    for item in projections {
        if let SelectItem::Expr(e, _) = item {
            collect_aggs_in(e, &mut found);
        }
    }
    found
}

/// Walk `expr`, pushing aggregate calls into `out`. Does not descend into an
/// aggregate's own arguments (nested aggregates are not supported).
fn collect_aggs_in(expr: &Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::Func { name, .. } if is_aggregate(name) => push_unique(out, expr),
        Expr::Func { args, .. } => {
            for a in args {
                collect_aggs_in(a, out);
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_aggs_in(left, out);
            collect_aggs_in(right, out);
        }
        Expr::Unary { expr, .. } => collect_aggs_in(expr, out),
        _ => {}
    }
}

/// A bare column must exist in at least one table in scope. Ambiguity
/// (present in more than one) is tolerated here and resolved by the
/// executor with full column mapping; only "exists nowhere" is an error.
fn resolve_bare_column(name: &str, scope: &[ScopeEntry<'_>]) -> Result<()> {
    let found = scope.iter().any(|e| e.meta.column_index(name).is_some());
    if found {
        Ok(())
    } else {
        Err(PlanError::UnknownColumn {
            table: scope
                .iter()
                .map(|e| e.qualifier.as_str())
                .collect::<Vec<_>>()
                .join(","),
            column: name.to_string(),
        })
    }
}

/// A qualified column `q.c` must name an in-scope table (by alias or name)
/// that has column `c`.
fn resolve_qualified_column(qualifier: &str, column: &str, scope: &[ScopeEntry<'_>]) -> Result<()> {
    let entry = scope
        .iter()
        .find(|e| e.qualifier == qualifier)
        .ok_or_else(|| PlanError::UnknownTable(qualifier.to_string()))?;
    if entry.meta.column_index(column).is_some() {
        Ok(())
    } else {
        Err(PlanError::UnknownColumn {
            table: qualifier.to_string(),
            column: column.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustdb_sql::Parser;

    fn stmt(src: &str) -> Statement {
        Parser::from_sql(src)
            .expect("lex")
            .parse_statement()
            .expect("parse")
    }

    fn catalog() -> Catalog {
        let mut c = Catalog::new();
        for ddl in [
            "CREATE TABLE orders (id INT, cid INT, total INT)",
            "CREATE TABLE customers (id INT, name TEXT)",
        ] {
            c.apply(&stmt(ddl)).expect("apply");
        }
        c
    }

    fn plan(src: &str) -> LogicalPlan {
        bind(&catalog(), &stmt(src)).expect("bind")
    }

    #[test]
    fn single_table_select_star() {
        let p = plan("SELECT * FROM orders");
        // Project { Scan }
        assert!(matches!(p, LogicalPlan::Project { input, .. }
            if matches!(*input, LogicalPlan::Scan { ref table, .. } if table == "orders")));
    }

    #[test]
    fn where_pushes_filter_directly_above_scan() {
        let p = plan("SELECT id FROM orders WHERE total > 100");
        // Project { Filter { Scan } } - the Filter sits adjacent to Scan.
        let LogicalPlan::Project { input, .. } = p else {
            panic!("expected Project");
        };
        let LogicalPlan::Filter { input, .. } = *input else {
            panic!("expected Filter under Project");
        };
        assert!(
            matches!(*input, LogicalPlan::Scan { .. }),
            "Filter must wrap Scan (pushdown)"
        );
    }

    #[test]
    fn clause_order_is_canonical() {
        // Limit { Project { Sort { Aggregate { Filter { Scan } } } } }
        // Sort sits below Project so ORDER BY can reference any in-scope column.
        let p = plan("SELECT id FROM orders WHERE total > 0 GROUP BY id ORDER BY id DESC LIMIT 5");
        let printed = p.to_string();
        let lines: Vec<&str> = printed.lines().map(str::trim_start).collect();
        assert_eq!(lines[0], "Limit 5");
        assert!(lines[1].starts_with("Project"));
        assert_eq!(lines[2], "Sort id DESC");
        assert!(lines[3].starts_with("Aggregate GROUP BY"));
        assert!(lines[4].starts_with("Filter"));
        assert!(lines[5].starts_with("Scan orders"));
    }

    #[test]
    fn join_builds_a_join_node() {
        let p =
            plan("SELECT o.id, c.name FROM orders AS o INNER JOIN customers AS c ON o.cid = c.id");
        let LogicalPlan::Project { input, .. } = p else {
            panic!("expected Project");
        };
        assert!(matches!(*input, LogicalPlan::Join { .. }));
    }

    #[test]
    fn unknown_table_errors() {
        let err = bind(&catalog(), &stmt("SELECT * FROM ghosts")).expect_err("err");
        assert!(matches!(err, PlanError::UnknownTable(t) if t == "ghosts"));
    }

    #[test]
    fn unknown_column_in_where_errors() {
        let err = bind(&catalog(), &stmt("SELECT id FROM orders WHERE nope = 1")).expect_err("err");
        assert!(matches!(err, PlanError::UnknownColumn { column, .. } if column == "nope"));
    }

    #[test]
    fn unknown_column_in_projection_errors() {
        let err = bind(&catalog(), &stmt("SELECT bogus FROM orders")).expect_err("err");
        assert!(matches!(err, PlanError::UnknownColumn { column, .. } if column == "bogus"));
    }

    #[test]
    fn qualified_column_unknown_table_errors() {
        let err = bind(&catalog(), &stmt("SELECT z.id FROM orders")).expect_err("err");
        assert!(matches!(err, PlanError::UnknownTable(t) if t == "z"));
    }

    #[test]
    fn join_resolves_columns_across_both_tables() {
        // Bare columns from either table resolve; this must not error.
        let p = bind(
            &catalog(),
            &stmt("SELECT name FROM orders INNER JOIN customers ON cid = customers.id WHERE total > 5"),
        );
        assert!(
            p.is_ok(),
            "columns across joined tables should resolve: {p:?}"
        );
    }
}
