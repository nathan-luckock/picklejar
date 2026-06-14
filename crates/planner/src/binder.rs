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

    // 3. GROUP BY.
    if !select.group_by.is_empty() {
        for key in &select.group_by {
            resolve_expr(key, &scope)?;
        }
        plan = LogicalPlan::Aggregate {
            group_by: select.group_by.clone(),
            input: Box::new(plan),
        };
    }

    // 4. Projection. Resolve every projected expression; `*` needs nothing.
    for item in &select.projections {
        if let SelectItem::Expr(expr, _) = item {
            resolve_expr(expr, &scope)?;
        }
    }
    plan = LogicalPlan::Project {
        items: select.projections.clone(),
        input: Box::new(plan),
    };

    // 5. ORDER BY.
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
    scope.push(ScopeEntry { qualifier, meta });
    Ok(LogicalPlan::Scan {
        table: table_ref.name.clone(),
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
            if matches!(*input, LogicalPlan::Scan { ref table } if table == "orders")));
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
        // Limit { Sort { Project { Aggregate { Filter { Scan } } } } }
        let p = plan("SELECT id FROM orders WHERE total > 0 GROUP BY id ORDER BY id DESC LIMIT 5");
        let printed = p.to_string();
        let lines: Vec<&str> = printed.lines().map(str::trim_start).collect();
        assert_eq!(lines[0], "Limit 5");
        assert_eq!(lines[1], "Sort id DESC");
        assert!(lines[2].starts_with("Project"));
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
