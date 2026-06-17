//! Correlated subquery support.
//!
//! An uncorrelated subquery is folded to a literal once, before planning (see
//! [`crate::database`]). A correlated subquery references a column of the outer
//! query, so it must be re-evaluated for each outer row. The binder leaves such
//! a node in the plan, and the executor calls back into a [`SubqueryRunner`]
//! per row.
//!
//! [`CorrelatedRunner`] is that callback. For each outer row it substitutes the
//! outer column references in the subquery with the row's literal values,
//! turning it into an ordinary uncorrelated query, then binds, plans, and runs
//! it against a materialized snapshot of the base tables. The snapshot is taken
//! once under the outer query's transaction, so every per-row evaluation sees
//! the same consistent data.
//!
//! Scope: the correlated subquery's own `FROM` must be plain base tables (no
//! views, derived tables, or further nested subqueries inside it). That covers
//! the common forms (correlated `EXISTS`, `IN`, and scalar-with-aggregate); the
//! excluded shapes fall back to the uncorrelated path or error.

use std::collections::{HashMap, HashSet};

use picklejar_executor::{eval_with, ExecError, Relation, SubqueryRunner, TableSource};
use picklejar_planner::{bind, plan, Catalog};
use picklejar_sql::statement::{Join, Select, SelectItem, Statement, TableRef};
use picklejar_sql::{Expr, Value};

use crate::database::in_list_expr;

/// A read-only base-table source backed by rows materialized in memory. Used to
/// run correlated subqueries against a fixed snapshot.
#[derive(Debug)]
pub struct MaterializedSource {
    tables: HashMap<String, Relation>,
}

impl MaterializedSource {
    /// Wrap already-materialized relations (bare column names, as a base-table
    /// scan returns them).
    #[must_use]
    pub const fn new(tables: HashMap<String, Relation>) -> Self {
        Self { tables }
    }
}

impl TableSource for MaterializedSource {
    fn scan(&self, table: &str) -> std::result::Result<Relation, ExecError> {
        self.tables
            .get(table)
            .cloned()
            .ok_or_else(|| ExecError::Source(format!("unknown table {table}")))
    }
}

/// Evaluates a correlated subquery per outer row against a base-table snapshot.
#[derive(Debug)]
pub struct CorrelatedRunner {
    catalog: Catalog,
    source: MaterializedSource,
}

impl CorrelatedRunner {
    /// Build a runner over `catalog` and the materialized `source`.
    #[must_use]
    pub const fn new(catalog: Catalog, source: MaterializedSource) -> Self {
        Self { catalog, source }
    }

    /// Substitute the outer references in `stmt`, bind, plan, and run it,
    /// returning its columns and rows.
    fn run_query(
        &self,
        stmt: &Statement,
        outer_columns: &[String],
        outer_row: &[Value],
    ) -> std::result::Result<(Vec<String>, Vec<Vec<Value>>), ExecError> {
        let substituted = substitute_stmt(&self.catalog, stmt, outer_columns, outer_row);
        let logical =
            bind(&self.catalog, &substituted).map_err(|e| ExecError::Source(e.to_string()))?;
        let physical =
            plan(&logical, &self.catalog).map_err(|e| ExecError::Source(e.to_string()))?;
        picklejar_executor::run(&physical, &self.source)
    }
}

impl SubqueryRunner for CorrelatedRunner {
    fn eval_subquery(
        &self,
        expr: &Expr,
        outer_columns: &[String],
        outer_row: &[Value],
    ) -> std::result::Result<Value, ExecError> {
        match expr {
            Expr::Subquery(query) => {
                let (columns, mut rows) = self.run_query(query, outer_columns, outer_row)?;
                if columns.len() != 1 {
                    return Err(ExecError::Unsupported(
                        "a scalar subquery must return exactly one column".into(),
                    ));
                }
                match rows.len() {
                    0 => Ok(Value::Null),
                    1 => Ok(rows.remove(0).remove(0)),
                    _ => Err(ExecError::Unsupported(
                        "a scalar subquery returned more than one row".into(),
                    )),
                }
            }
            Expr::Exists(query) => {
                let (_columns, rows) = self.run_query(query, outer_columns, outer_row)?;
                Ok(Value::Bool(!rows.is_empty()))
            }
            Expr::InSubquery {
                expr: lhs,
                query,
                negated,
            } => {
                let lhs_val = eval_with(lhs, outer_row, outer_columns, Some(self))?;
                let (columns, rows) = self.run_query(query, outer_columns, outer_row)?;
                if columns.len() != 1 {
                    return Err(ExecError::Unsupported(
                        "an IN subquery must return exactly one column".into(),
                    ));
                }
                let list: Vec<Value> = rows
                    .into_iter()
                    .filter_map(|mut r| (!r.is_empty()).then(|| r.remove(0)))
                    .collect();
                // Reuse the three-valued IN semantics by building the equivalent
                // OR/AND chain over literals and evaluating it.
                let in_expr = in_list_expr(&Expr::Literal(lhs_val), &list, *negated);
                eval_with(&in_expr, &[], &[], None)
            }
            other => Err(ExecError::Unsupported(format!("{other} is not a subquery"))),
        }
    }
}

/// Whether any subquery node remains anywhere in `stmt` (i.e. the engine left a
/// correlated subquery for per-row evaluation).
#[must_use]
pub fn has_subquery(stmt: &Statement) -> bool {
    match stmt {
        Statement::Select(s) => select_has_subquery(s),
        Statement::Union { left, right, .. } => has_subquery(left) || has_subquery(right),
        _ => false,
    }
}

fn select_has_subquery(s: &Select) -> bool {
    s.projections.iter().any(|p| match p {
        SelectItem::Star => false,
        SelectItem::Expr(e, _) => expr_has_subquery(e),
    }) || s.where_clause.as_ref().is_some_and(expr_has_subquery)
        || s.group_by.iter().any(expr_has_subquery)
        || s.having.as_ref().is_some_and(expr_has_subquery)
        || s.order_by.iter().any(|o| expr_has_subquery(&o.expr))
        || s.joins.iter().any(|j| expr_has_subquery(&j.on))
}

fn expr_has_subquery(expr: &Expr) -> bool {
    match expr {
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_) => true,
        Expr::Binary { left, right, .. } => expr_has_subquery(left) || expr_has_subquery(right),
        Expr::Unary { expr, .. } => expr_has_subquery(expr),
        Expr::Func { args, .. } => args.iter().any(expr_has_subquery),
        Expr::Case {
            operand,
            whens,
            else_result,
        } => {
            operand.as_deref().is_some_and(expr_has_subquery)
                || whens
                    .iter()
                    .any(|(w, t)| expr_has_subquery(w) || expr_has_subquery(t))
                || else_result.as_deref().is_some_and(expr_has_subquery)
        }
        _ => false,
    }
}

/// Whether `stmt` is a correlated subquery over plain base tables: it
/// references at least one column not provided by its own `FROM`/joins.
///
/// Returns `false` if any relation is not a resolvable base table (a view,
/// derived table, or one with a nested subquery), so the uncorrelated fold path
/// handles those instead.
#[must_use]
pub fn is_correlated(catalog: &Catalog, stmt: &Statement) -> bool {
    let Statement::Select(s) = stmt else {
        return false;
    };
    let Some(scope) = base_table_scope(catalog, s) else {
        return false;
    };
    select_refs_outside(s, &scope)
}

/// The qualifiers and column names available inside a single-level select whose
/// `FROM`/joins are plain base tables. `None` if any relation is not such a
/// base table.
struct Scope {
    qualifiers: HashSet<String>,
    columns: HashSet<String>,
}

fn base_table_scope(catalog: &Catalog, s: &Select) -> Option<Scope> {
    let mut scope = Scope {
        qualifiers: HashSet::new(),
        columns: HashSet::new(),
    };
    add_relation(catalog, &s.from, &mut scope)?;
    for join in &s.joins {
        add_relation(catalog, &join.table, &mut scope)?;
    }
    Some(scope)
}

fn add_relation(catalog: &Catalog, table: &TableRef, scope: &mut Scope) -> Option<()> {
    if table.subquery.is_some() {
        return None;
    }
    let meta = catalog.get_table(&table.name)?;
    let qualifier = table.alias.clone().unwrap_or_else(|| table.name.clone());
    scope.qualifiers.insert(qualifier);
    for column in &meta.columns {
        scope.columns.insert(column.name.clone());
    }
    Some(())
}

fn select_refs_outside(s: &Select, scope: &Scope) -> bool {
    s.projections.iter().any(|p| match p {
        SelectItem::Star => false,
        SelectItem::Expr(e, _) => refs_outside(e, scope),
    }) || s
        .where_clause
        .as_ref()
        .is_some_and(|w| refs_outside(w, scope))
        || s.group_by.iter().any(|g| refs_outside(g, scope))
        || s.having.as_ref().is_some_and(|h| refs_outside(h, scope))
        || s.order_by.iter().any(|o| refs_outside(&o.expr, scope))
        || s.joins.iter().any(|j| refs_outside(&j.on, scope))
}

/// Whether `expr` references a column outside `scope`. Nested subquery nodes are
/// not descended into: their references belong to a deeper scope.
fn refs_outside(expr: &Expr, scope: &Scope) -> bool {
    match expr {
        Expr::Column(name) => !scope.columns.contains(name),
        Expr::QualifiedColumn(qualifier, _) => !scope.qualifiers.contains(qualifier),
        Expr::Binary { left, right, .. } => refs_outside(left, scope) || refs_outside(right, scope),
        Expr::Unary { expr, .. } => refs_outside(expr, scope),
        Expr::Func { args, .. } => args.iter().any(|a| refs_outside(a, scope)),
        Expr::Case {
            operand,
            whens,
            else_result,
        } => {
            operand.as_deref().is_some_and(|o| refs_outside(o, scope))
                || whens
                    .iter()
                    .any(|(w, t)| refs_outside(w, scope) || refs_outside(t, scope))
                || else_result
                    .as_deref()
                    .is_some_and(|e| refs_outside(e, scope))
        }
        _ => false,
    }
}

/// Replace every outer column reference in `stmt` with its literal value from
/// `outer_row`, yielding an uncorrelated query. Only base-table selects are
/// substituted; other shapes are returned unchanged (and will fail to bind).
fn substitute_stmt(
    catalog: &Catalog,
    stmt: &Statement,
    outer_columns: &[String],
    outer_row: &[Value],
) -> Statement {
    match stmt {
        Statement::Select(s) => {
            let Some(scope) = base_table_scope(catalog, s) else {
                return stmt.clone();
            };
            Statement::Select(Box::new(substitute_select(
                s,
                &scope,
                outer_columns,
                outer_row,
            )))
        }
        other => other.clone(),
    }
}

fn substitute_select(
    s: &Select,
    scope: &Scope,
    outer_columns: &[String],
    outer_row: &[Value],
) -> Select {
    let sub = |e: &Expr| substitute_expr(e, scope, outer_columns, outer_row);
    Select {
        distinct: s.distinct,
        projections: s
            .projections
            .iter()
            .map(|p| match p {
                SelectItem::Star => SelectItem::Star,
                SelectItem::Expr(e, alias) => SelectItem::Expr(sub(e), alias.clone()),
            })
            .collect(),
        from: s.from.clone(),
        joins: s
            .joins
            .iter()
            .map(|j| Join {
                kind: j.kind,
                table: j.table.clone(),
                on: sub(&j.on),
                using: j.using.clone(),
                natural: j.natural,
            })
            .collect(),
        where_clause: s.where_clause.as_ref().map(&sub),
        group_by: s.group_by.iter().map(&sub).collect(),
        having: s.having.as_ref().map(&sub),
        order_by: s
            .order_by
            .iter()
            .map(|o| picklejar_sql::statement::OrderItem {
                expr: sub(&o.expr),
                desc: o.desc,
                nulls_first: o.nulls_first,
            })
            .collect(),
        limit: s.limit,
        offset: s.offset,
    }
}

fn substitute_expr(
    expr: &Expr,
    scope: &Scope,
    outer_columns: &[String],
    outer_row: &[Value],
) -> Expr {
    match expr {
        Expr::Column(name) => {
            if !scope.columns.contains(name) {
                if let Some(v) = lookup_outer(name, outer_columns, outer_row) {
                    return Expr::Literal(v);
                }
            }
            expr.clone()
        }
        Expr::QualifiedColumn(qualifier, column) => {
            if !scope.qualifiers.contains(qualifier) {
                let full = format!("{qualifier}.{column}");
                if let Some(v) = lookup_outer(&full, outer_columns, outer_row) {
                    return Expr::Literal(v);
                }
            }
            expr.clone()
        }
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(substitute_expr(left, scope, outer_columns, outer_row)),
            right: Box::new(substitute_expr(right, scope, outer_columns, outer_row)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(substitute_expr(expr, scope, outer_columns, outer_row)),
        },
        Expr::Func {
            name,
            distinct,
            args,
        } => Expr::Func {
            name: name.clone(),
            distinct: *distinct,
            args: args
                .iter()
                .map(|a| substitute_expr(a, scope, outer_columns, outer_row))
                .collect(),
        },
        Expr::Case {
            operand,
            whens,
            else_result,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| Box::new(substitute_expr(o, scope, outer_columns, outer_row))),
            whens: whens
                .iter()
                .map(|(w, t)| {
                    (
                        substitute_expr(w, scope, outer_columns, outer_row),
                        substitute_expr(t, scope, outer_columns, outer_row),
                    )
                })
                .collect(),
            else_result: else_result
                .as_ref()
                .map(|e| Box::new(substitute_expr(e, scope, outer_columns, outer_row))),
        },
        // Leaves and nested subqueries are left as-is.
        _ => expr.clone(),
    }
}

/// Look up `name` (qualified or bare) in the outer row, mirroring the
/// executor's column resolution: an exact match first, then a unique
/// bare-suffix match.
fn lookup_outer(name: &str, outer_columns: &[String], outer_row: &[Value]) -> Option<Value> {
    if let Some(i) = outer_columns.iter().position(|c| c == name) {
        return Some(outer_row[i].clone());
    }
    if !name.contains('.') {
        let mut found = None;
        for (i, c) in outer_columns.iter().enumerate() {
            if c.rsplit('.').next() == Some(name) {
                if found.is_some() {
                    return None;
                }
                found = Some(i);
            }
        }
        return found.map(|i| outer_row[i].clone());
    }
    None
}
