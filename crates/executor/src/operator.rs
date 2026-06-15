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
use rustdb_sql::{Expr, JoinKind, Value};

use crate::error::{ExecError, Result};
use crate::eval::{eval, is_truthy};

/// Qualify a scan's bare column names with a table qualifier, so joins can
/// disambiguate columns and `q.col` references resolve.
fn qualify(qualifier: &str, columns: &[String]) -> Vec<String> {
    columns.iter().map(|c| format!("{qualifier}.{c}")).collect()
}

/// The bare column name (the part after the last `.`), for result headers.
fn strip_qualifier(col: &str) -> String {
    col.rsplit('.').next().unwrap_or(col).to_string()
}

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
                        // Select the (qualified) column exactly, but present
                        // it under its bare name in the output.
                        exprs.push(Expr::Column(c.clone()));
                        out_columns.push(strip_qualifier(c));
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

/// Join two inputs by iterating the right side once per left row.
///
/// This backs both the `NestedLoopJoin` and the `HashJoin` physical nodes: the
/// result is identical, and the hash build/probe is a runtime optimization
/// deferred the same way the index scan is (the planner's algorithm choice is
/// still shown by EXPLAIN). Output columns are the left columns followed by the
/// right columns, both qualified.
struct NestedLoopJoin {
    kind: JoinKind,
    left: Box<dyn Executor>,
    /// The right side, materialized so it can be rescanned per left row.
    right_rows: Vec<Row>,
    right_width: usize,
    on: Expr,
    columns: Vec<String>,
    /// The left row currently being matched, and the right index within it.
    current_left: Option<Row>,
    right_idx: usize,
    /// Whether the current left row matched any right row (for LEFT joins).
    matched: bool,
}

impl NestedLoopJoin {
    fn new(
        left: Box<dyn Executor>,
        mut right: Box<dyn Executor>,
        kind: JoinKind,
        on: Expr,
    ) -> Result<Self> {
        let mut columns = left.columns().to_vec();
        let right_columns = right.columns().to_vec();
        columns.extend(right_columns.iter().cloned());
        let mut right_rows = Vec::new();
        while let Some(r) = right.next()? {
            right_rows.push(r);
        }
        Ok(Self {
            kind,
            left,
            right_rows,
            right_width: right_columns.len(),
            on,
            columns,
            current_left: None,
            right_idx: 0,
            matched: false,
        })
    }
}

impl Executor for NestedLoopJoin {
    fn columns(&self) -> &[String] {
        &self.columns
    }
    fn next(&mut self) -> Result<Option<Row>> {
        loop {
            // Advance to the next left row when the current one is exhausted.
            if self.current_left.is_none() {
                match self.left.next()? {
                    Some(l) => {
                        self.current_left = Some(l);
                        self.right_idx = 0;
                        self.matched = false;
                    }
                    None => return Ok(None),
                }
            }
            let left_row = self.current_left.clone().expect("left row present");

            while self.right_idx < self.right_rows.len() {
                let mut combined = left_row.clone();
                combined.extend_from_slice(&self.right_rows[self.right_idx]);
                self.right_idx += 1;
                if is_truthy(&eval(&self.on, &combined, &self.columns)?) {
                    self.matched = true;
                    return Ok(Some(combined));
                }
            }

            // The right side is exhausted for this left row.
            let unmatched_left = !self.matched;
            self.current_left = None;
            if self.kind == JoinKind::Left && unmatched_left {
                // Emit the left row padded with NULLs for the right columns.
                let mut combined = left_row;
                combined.extend(std::iter::repeat(Value::Null).take(self.right_width));
                return Ok(Some(combined));
            }
        }
    }
}

/// Lower a physical plan into an executor tree, reading base tables through
/// `source`.
///
/// # Errors
///
/// Returns an error for plan nodes the executor does not run yet (aggregates)
/// or if a base table cannot be read.
pub fn build(plan: &PhysicalPlan, source: &dyn TableSource) -> Result<Box<dyn Executor>> {
    match plan {
        PhysicalPlan::SeqScan {
            table,
            qualifier,
            predicate,
            ..
        } => {
            let base = scan(table, qualifier, source)?;
            Ok(match predicate {
                Some(p) => Box::new(Filter {
                    input: base,
                    predicate: p.clone(),
                }),
                None => base,
            })
        }
        PhysicalPlan::IndexScan {
            table,
            qualifier,
            predicate,
            ..
        } => {
            // No physical index lookup yet: a full scan with the predicate as
            // a residual filter is correct, just not faster than a seq scan.
            Ok(Box::new(Filter {
                input: scan(table, qualifier, source)?,
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
        // Both join algorithms run through the nested-loop executor; the hash
        // build/probe is a deferred runtime optimization (the planner's choice
        // is still shown by EXPLAIN).
        PhysicalPlan::NestedLoopJoin {
            kind,
            left,
            right,
            on,
            ..
        }
        | PhysicalPlan::HashJoin {
            kind,
            left,
            right,
            on,
            ..
        } => Ok(Box::new(NestedLoopJoin::new(
            build(left, source)?,
            build(right, source)?,
            *kind,
            on.clone(),
        )?)),
        PhysicalPlan::Aggregate { .. } => Err(ExecError::Unsupported("GROUP BY".into())),
    }
}

/// Materialize a base table as a `Values` operator with qualified columns.
fn scan(table: &str, qualifier: &str, source: &dyn TableSource) -> Result<Box<dyn Executor>> {
    let rel = source.scan(table)?;
    Ok(Box::new(Values {
        columns: qualify(qualifier, &rel.columns),
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
