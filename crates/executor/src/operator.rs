//! Volcano-style operators and the plan-to-executor builder.
//!
//! Each operator is a pull-based iterator: [`Executor::next`] returns the next
//! row or `None` at end of stream, and [`Executor::columns`] names the output
//! columns for downstream name resolution and the result header. [`build`]
//! lowers a [`PhysicalPlan`] into a tree of these, and [`run`] drains it.
//!
//! Base-table access is abstracted behind [`TableSource`] so the executor does
//! not depend on the storage stack: the engine materializes a table's visible
//! rows once (via its MVCC scan) and hands them over as a [`Relation`]. The
//! operators above the scan are pure in-memory transforms.

use std::cmp::Ordering;

use rustdb_planner::PhysicalPlan;
use rustdb_sql::statement::SelectItem;
use rustdb_sql::{Expr, Value};

use crate::error::{ExecError, Result};
use crate::eval::{eval, is_truthy};

/// One output row: values positionally aligned with an operator's columns.
pub type Row = Vec<Value>;

/// A materialized base relation: column names and all visible rows.
#[derive(Debug, Clone)]
pub struct Relation {
    /// Column names, in order.
    pub columns: Vec<String>,
    /// The rows.
    pub rows: Vec<Row>,
}

/// Supplies base-table rows to the executor. The engine implements this by
/// scanning the table's MVCC store under the current snapshot.
pub trait TableSource {
    /// Return every visible row of `table`, with its column names.
    ///
    /// # Errors
    ///
    /// Returns an error if the table cannot be read.
    fn scan(&self, table: &str) -> Result<Relation>;
}

/// A pull-based query operator.
pub trait Executor {
    /// The operator's output column names.
    fn columns(&self) -> &[String];
    /// Produce the next row, or `None` at end of stream.
    ///
    /// # Errors
    ///
    /// Returns an error if evaluating a row fails (for example a type error).
    fn next(&mut self) -> Result<Option<Row>>;
}

/// A leaf operator over already-materialized rows (a scan's output).
struct Values {
    columns: Vec<String>,
    rows: std::vec::IntoIter<Row>,
}

impl Executor for Values {
    fn columns(&self) -> &[String] {
        &self.columns
    }
    fn next(&mut self) -> Result<Option<Row>> {
        Ok(self.rows.next())
    }
}

/// Keep only rows for which `predicate` evaluates to `true`.
struct Filter {
    input: Box<dyn Executor>,
    predicate: Expr,
}

impl Executor for Filter {
    fn columns(&self) -> &[String] {
        self.input.columns()
    }
    fn next(&mut self) -> Result<Option<Row>> {
        while let Some(row) = self.input.next()? {
            let keep = is_truthy(&eval(&self.predicate, &row, self.input.columns())?);
            if keep {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

/// Compute a projection list per row.
struct Project {
    input: Box<dyn Executor>,
    exprs: Vec<Expr>,
    out_columns: Vec<String>,
}

impl Project {
    /// Build a projection from `items`, expanding `*` to the child's columns.
    fn new(input: Box<dyn Executor>, items: &[SelectItem]) -> Self {
        let mut exprs = Vec::new();
        let mut out_columns = Vec::new();
        for item in items {
            match item {
                SelectItem::Star => {
                    for c in input.columns() {
                        exprs.push(Expr::Column(c.clone()));
                        out_columns.push(c.clone());
                    }
                }
                SelectItem::Expr(e, alias) => {
                    out_columns.push(output_name(e, alias.as_deref()));
                    exprs.push(e.clone());
                }
            }
        }
        Self {
            input,
            exprs,
            out_columns,
        }
    }
}

impl Executor for Project {
    fn columns(&self) -> &[String] {
        &self.out_columns
    }
    fn next(&mut self) -> Result<Option<Row>> {
        let Some(row) = self.input.next()? else {
            return Ok(None);
        };
        let cols = self.input.columns();
        let out = self
            .exprs
            .iter()
            .map(|e| eval(e, &row, cols))
            .collect::<Result<Row>>()?;
        Ok(Some(out))
    }
}

/// The output column name for a projected expression: its alias, else its
/// column name, else its printed form.
fn output_name(expr: &Expr, alias: Option<&str>) -> String {
    if let Some(a) = alias {
        return a.to_string();
    }
    match expr {
        Expr::Column(name) | Expr::QualifiedColumn(_, name) => name.clone(),
        other => other.to_string(),
    }
}

/// Sort all input rows by the given keys (a blocking operator).
struct Sort {
    input: Box<dyn Executor>,
    keys: Vec<(Expr, bool)>,
    buffered: Option<std::vec::IntoIter<Row>>,
}

impl Sort {
    /// Buffer the whole input, sort it by the keys, and store the result.
    fn materialize(&mut self) -> Result<()> {
        let cols = self.input.columns().to_vec();
        let mut keyed: Vec<(Vec<Value>, Row)> = Vec::new();
        while let Some(row) = self.input.next()? {
            let key = self
                .keys
                .iter()
                .map(|(e, _)| eval(e, &row, &cols))
                .collect::<Result<Vec<_>>>()?;
            keyed.push((key, row));
        }
        keyed.sort_by(|a, b| cmp_keys(&a.0, &b.0, &self.keys));
        let mut rows: Vec<Row> = Vec::with_capacity(keyed.len());
        for (_, row) in keyed {
            rows.push(row);
        }
        self.buffered = Some(rows.into_iter());
        Ok(())
    }
}

impl Executor for Sort {
    fn columns(&self) -> &[String] {
        self.input.columns()
    }
    fn next(&mut self) -> Result<Option<Row>> {
        if self.buffered.is_none() {
            self.materialize()?;
        }
        Ok(self.buffered.as_mut().expect("materialized").next())
    }
}

/// Compare two key vectors honoring each key's descending flag.
fn cmp_keys(a: &[Value], b: &[Value], keys: &[(Expr, bool)]) -> Ordering {
    for (i, (_, desc)) in keys.iter().enumerate() {
        let ord = sort_cmp(&a[i], &b[i]);
        let ord = if *desc { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Total order over values for sorting, with NULLs last (ascending).
fn sort_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// Emit at most `remaining` rows, then stop.
struct Limit {
    input: Box<dyn Executor>,
    remaining: u64,
}

impl Executor for Limit {
    fn columns(&self) -> &[String] {
        self.input.columns()
    }
    fn next(&mut self) -> Result<Option<Row>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        match self.input.next()? {
            Some(row) => {
                self.remaining -= 1;
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }
}

/// Lower a physical plan into an executor tree, reading base tables through
/// `source`.
///
/// # Errors
///
/// Returns an error for plan nodes the executor does not run yet (joins,
/// aggregates) or if a base table cannot be read.
pub fn build(plan: &PhysicalPlan, source: &dyn TableSource) -> Result<Box<dyn Executor>> {
    match plan {
        PhysicalPlan::SeqScan {
            table, predicate, ..
        } => {
            let base = scan(table, source)?;
            Ok(match predicate {
                Some(p) => Box::new(Filter {
                    input: base,
                    predicate: p.clone(),
                }),
                None => base,
            })
        }
        PhysicalPlan::IndexScan {
            table, predicate, ..
        } => {
            // No physical index lookup yet: a full scan with the predicate as
            // a residual filter is correct, just not faster than a seq scan.
            Ok(Box::new(Filter {
                input: scan(table, source)?,
                predicate: predicate.clone(),
            }))
        }
        PhysicalPlan::Filter {
            predicate, input, ..
        } => Ok(Box::new(Filter {
            input: build(input, source)?,
            predicate: predicate.clone(),
        })),
        PhysicalPlan::Project { items, input, .. } => {
            Ok(Box::new(Project::new(build(input, source)?, items)))
        }
        PhysicalPlan::Sort { keys, input, .. } => Ok(Box::new(Sort {
            input: build(input, source)?,
            keys: keys.clone(),
            buffered: None,
        })),
        PhysicalPlan::Limit { n, input, .. } => Ok(Box::new(Limit {
            input: build(input, source)?,
            remaining: *n,
        })),
        PhysicalPlan::Aggregate { .. } => Err(ExecError::Unsupported("GROUP BY".into())),
        PhysicalPlan::NestedLoopJoin { .. } | PhysicalPlan::HashJoin { .. } => {
            Err(ExecError::Unsupported("joins".into()))
        }
    }
}

/// Materialize a base table as a `Values` operator.
fn scan(table: &str, source: &dyn TableSource) -> Result<Box<dyn Executor>> {
    let rel = source.scan(table)?;
    Ok(Box::new(Values {
        columns: rel.columns,
        rows: rel.rows.into_iter(),
    }))
}

/// Build and drain a plan, returning the output column names and all rows.
///
/// # Errors
///
/// Propagates any build or evaluation error.
pub fn run(plan: &PhysicalPlan, source: &dyn TableSource) -> Result<(Vec<String>, Vec<Row>)> {
    let mut op = build(plan, source)?;
    let columns = op.columns().to_vec();
    let mut rows = Vec::new();
    while let Some(row) = op.next()? {
        rows.push(row);
    }
    Ok((columns, rows))
}
