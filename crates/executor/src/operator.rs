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
use std::collections::{HashMap, HashSet};

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

    /// Return candidate rows of `table` resolved through `index` to satisfy the
    /// equality in `predicate` on the indexed column. The rows are already
    /// visibility-filtered; the caller still applies `predicate` as a residual
    /// filter, so returning a superset (or falling back to a full scan) is
    /// always correct.
    ///
    /// The default implementation falls back to [`scan`](Self::scan), so a
    /// source with no physical index is still correct, just not faster.
    ///
    /// # Errors
    ///
    /// Returns an error if the table cannot be read.
    fn index_scan(&self, table: &str, index: &str, predicate: &Expr) -> Result<Relation> {
        let _ = (index, predicate);
        self.scan(table)
    }
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
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        #[allow(clippy::cast_precision_loss)]
        (Value::Int(x), Value::Float(y)) => (*x as f64).total_cmp(y),
        #[allow(clippy::cast_precision_loss)]
        (Value::Float(x), Value::Int(y)) => x.total_cmp(&(*y as f64)),
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

/// Remove duplicate rows, preserving first-occurrence order (`SELECT
/// DISTINCT`). Uniqueness is keyed by the same canonical byte encoding the
/// group-by operator uses, so equal value lists (floats by bit pattern) dedup.
struct Distinct {
    input: Box<dyn Executor>,
    seen: HashSet<Vec<u8>>,
}

impl Executor for Distinct {
    fn columns(&self) -> &[String] {
        self.input.columns()
    }
    fn next(&mut self) -> Result<Option<Row>> {
        while let Some(row) = self.input.next()? {
            if self.seen.insert(group_key_bytes(&row)) {
                return Ok(Some(row));
            }
        }
        Ok(None)
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

/// An aggregate function.
#[derive(Clone, Copy)]
enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

/// A parsed aggregate: a function and its argument (`None` for `COUNT(*)`).
struct AggSpec {
    func: AggFunc,
    arg: Option<Expr>,
}

/// Per-group, per-aggregate running state. Each field is used only by the
/// aggregate whose function needs it.
#[derive(Default)]
struct Acc {
    /// Rows counted (all rows for `COUNT(*)`, non-null for `COUNT(expr)`).
    count: u64,
    /// Running integer sum and the count of summed numbers (for SUM / AVG).
    sum: i64,
    /// Running float sum, used when the summed column is FLOAT.
    fsum: f64,
    /// Whether any summed value was a float (selects the SUM/AVG result type).
    saw_float: bool,
    num: u64,
    /// Running min / max of non-null values.
    min: Option<Value>,
    max: Option<Value>,
}

/// Parse an aggregate call expression into a spec.
fn parse_agg(expr: &Expr) -> Result<AggSpec> {
    let Expr::Func { name, args } = expr else {
        return Err(ExecError::Unsupported(format!(
            "non-aggregate expression {expr} in an aggregate query"
        )));
    };
    let func = match name.as_str() {
        "COUNT" => AggFunc::Count,
        "SUM" => AggFunc::Sum,
        "MIN" => AggFunc::Min,
        "MAX" => AggFunc::Max,
        "AVG" => AggFunc::Avg,
        other => return Err(ExecError::Unsupported(format!("aggregate {other}"))),
    };
    let arg = match args.as_slice() {
        [Expr::Star] => None,
        [e] => Some(e.clone()),
        _ => {
            return Err(ExecError::Unsupported(format!(
                "{name} takes exactly one argument"
            )))
        }
    };
    Ok(AggSpec { func, arg })
}

/// Fold one input row into an accumulator.
fn update_acc(acc: &mut Acc, spec: &AggSpec, row: &[Value], cols: &[String]) -> Result<()> {
    let Some(arg) = &spec.arg else {
        // COUNT(*): every row counts.
        acc.count += 1;
        return Ok(());
    };
    let v = eval(arg, row, cols)?;
    if matches!(v, Value::Null) {
        return Ok(());
    }
    acc.count += 1;
    match &v {
        Value::Int(n) => {
            acc.sum += *n;
            acc.num += 1;
        }
        Value::Float(x) => {
            acc.fsum += *x;
            acc.saw_float = true;
            acc.num += 1;
        }
        _ => {}
    }
    acc.min = Some(match acc.min.take() {
        Some(m) if sort_cmp(&m, &v) == Ordering::Less => m,
        _ => v.clone(),
    });
    acc.max = Some(match acc.max.take() {
        Some(m) if sort_cmp(&m, &v) == Ordering::Greater => m,
        _ => v,
    });
    Ok(())
}

/// Produce the final aggregate value from an accumulator.
fn finalize_acc(acc: &Acc, spec: &AggSpec) -> Value {
    let as_int = |n: u64| Value::Int(i64::try_from(n).unwrap_or(i64::MAX));
    match spec.func {
        AggFunc::Count => as_int(acc.count),
        AggFunc::Sum => {
            if acc.num == 0 {
                Value::Null
            } else if acc.saw_float {
                Value::Float(acc.fsum)
            } else {
                Value::Int(acc.sum)
            }
        }
        AggFunc::Avg => {
            if acc.num == 0 {
                Value::Null
            } else if acc.saw_float {
                #[allow(clippy::cast_precision_loss)]
                Value::Float(acc.fsum / acc.num as f64)
            } else {
                Value::Int(acc.sum / i64::try_from(acc.num).unwrap_or(1).max(1))
            }
        }
        AggFunc::Min => acc.min.clone().unwrap_or(Value::Null),
        AggFunc::Max => acc.max.clone().unwrap_or(Value::Null),
    }
}

/// A canonical byte key for a group, so equal value lists hash equal.
fn group_key_bytes(values: &[Value]) -> Vec<u8> {
    let mut b = Vec::new();
    for v in values {
        match v {
            Value::Null => b.push(0),
            Value::Int(n) => {
                b.push(1);
                b.extend_from_slice(&n.to_le_bytes());
            }
            Value::Text(s) => {
                b.push(2);
                b.extend_from_slice(&(s.len() as u64).to_le_bytes());
                b.extend_from_slice(s.as_bytes());
            }
            Value::Bool(x) => {
                b.push(3);
                b.push(u8::from(*x));
            }
            Value::Float(x) => {
                // Group by bit pattern so equal floats (and identical NaNs)
                // land in the same group.
                b.push(4);
                b.extend_from_slice(&x.to_bits().to_le_bytes());
            }
        }
    }
    b
}

/// Lexicographic order over two value lists, for deterministic group output.
fn cmp_value_lists(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let ord = sort_cmp(x, y);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// Group-by aggregate (a blocking operator): read all input, group by the key
/// expressions, compute each aggregate per group, and emit one row per group
/// (`[group keys..., aggregates...]`). A keyless aggregate over an empty input
/// still emits one row, matching SQL (`COUNT(*)` is 0, others NULL).
struct Aggregate {
    input: Box<dyn Executor>,
    group_by: Vec<Expr>,
    specs: Vec<AggSpec>,
    columns: Vec<String>,
    buffered: Option<std::vec::IntoIter<Row>>,
}

impl Aggregate {
    fn new(input: Box<dyn Executor>, group_by: Vec<Expr>, aggregates: &[Expr]) -> Result<Self> {
        let specs = aggregates
            .iter()
            .map(parse_agg)
            .collect::<Result<Vec<_>>>()?;
        let mut columns: Vec<String> = group_by.iter().map(ToString::to_string).collect();
        columns.extend(aggregates.iter().map(ToString::to_string));
        Ok(Self {
            input,
            group_by,
            specs,
            columns,
            buffered: None,
        })
    }

    fn materialize(&mut self) -> Result<()> {
        let in_cols = self.input.columns().to_vec();
        let mut groups: Vec<(Vec<Value>, Vec<Acc>)> = Vec::new();
        let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut any_row = false;

        while let Some(row) = self.input.next()? {
            any_row = true;
            let key = self
                .group_by
                .iter()
                .map(|e| eval(e, &row, &in_cols))
                .collect::<Result<Vec<_>>>()?;
            let kb = group_key_bytes(&key);
            let idx = if let Some(&i) = index.get(&kb) {
                i
            } else {
                let i = groups.len();
                index.insert(kb, i);
                groups.push((key, self.specs.iter().map(|_| Acc::default()).collect()));
                i
            };
            for (acc, spec) in groups[idx].1.iter_mut().zip(&self.specs) {
                update_acc(acc, spec, &row, &in_cols)?;
            }
        }

        // A whole-table aggregate over no rows still yields one summary row.
        if self.group_by.is_empty() && !any_row {
            groups.push((
                Vec::new(),
                self.specs.iter().map(|_| Acc::default()).collect(),
            ));
        }
        groups.sort_by(|a, b| cmp_value_lists(&a.0, &b.0));

        let mut out = Vec::with_capacity(groups.len());
        for (key, accs) in groups {
            let mut row = key;
            for (acc, spec) in accs.iter().zip(&self.specs) {
                row.push(finalize_acc(acc, spec));
            }
            out.push(row);
        }
        self.buffered = Some(out.into_iter());
        Ok(())
    }
}

impl Executor for Aggregate {
    fn columns(&self) -> &[String] {
        &self.columns
    }
    fn next(&mut self) -> Result<Option<Row>> {
        if self.buffered.is_none() {
            self.materialize()?;
        }
        Ok(self.buffered.as_mut().expect("materialized").next())
    }
}

/// Lower a physical plan into an executor tree, reading base tables through
/// `source`.
///
/// # Errors
///
/// Returns an error for plan nodes or expressions the executor does not run
/// yet, or if a base table cannot be read.
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
            index,
            predicate,
            ..
        } => {
            // Resolve candidate rows through the index, then apply the full
            // predicate as a residual filter. The residual is what verifies a
            // candidate against the visible row, so a stale or over-broad
            // index result can never produce a wrong row.
            let rel = source.index_scan(table, index, predicate)?;
            Ok(Box::new(Filter {
                input: Box::new(Values {
                    columns: qualify(qualifier, &rel.columns),
                    rows: rel.rows.into_iter(),
                }),
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
        PhysicalPlan::Distinct { input, .. } => Ok(Box::new(Distinct {
            input: build(input, source)?,
            seen: HashSet::new(),
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
        PhysicalPlan::Aggregate {
            group_by,
            aggregates,
            input,
            ..
        } => Ok(Box::new(Aggregate::new(
            build(input, source)?,
            group_by.clone(),
            aggregates,
        )?)),
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
