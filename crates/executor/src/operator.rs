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
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use rustdb_planner::PhysicalPlan;
use rustdb_sql::statement::SelectItem;
use rustdb_sql::{BinOp, Expr, JoinKind, Value};

use crate::error::{ExecError, Result};
use crate::eval::{eval, eval_with, is_truthy, SubqueryRunner};

/// A shared, correlated-subquery evaluator passed down the operator tree. The
/// operators that evaluate expressions (`Filter`, `Project`) call it when a
/// subquery node survives to evaluation. `None` for a plan with no correlated
/// subqueries.
type Runner = Option<Rc<dyn SubqueryRunner>>;

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
    runner: Runner,
}

impl Executor for Filter {
    fn columns(&self) -> &[String] {
        self.input.columns()
    }
    fn next(&mut self) -> Result<Option<Row>> {
        while let Some(row) = self.input.next()? {
            let value = eval_with(
                &self.predicate,
                &row,
                self.input.columns(),
                self.runner.as_deref(),
            )?;
            if is_truthy(&value) {
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
    runner: Runner,
}

impl Project {
    /// Build a projection from `items`, expanding `*` to the child's columns.
    fn new(input: Box<dyn Executor>, items: &[SelectItem], runner: Runner) -> Self {
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
            runner,
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
            .map(|e| eval_with(e, &row, cols, self.runner.as_deref()))
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

/// Skip `to_skip` rows, then emit at most `remaining` more.
struct Limit {
    input: Box<dyn Executor>,
    to_skip: u64,
    remaining: u64,
}

impl Executor for Limit {
    fn columns(&self) -> &[String] {
        self.input.columns()
    }
    fn next(&mut self) -> Result<Option<Row>> {
        // Drain the OFFSET first.
        while self.to_skip > 0 {
            match self.input.next()? {
                Some(_) => self.to_skip -= 1,
                None => return Ok(None),
            }
        }
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

/// A derived table: pass the subquery's rows through unchanged, but present its
/// columns re-qualified under the table alias (`x.col`), so the outer query can
/// reference them as `x.col` or by bare name.
struct DerivedScan {
    input: Box<dyn Executor>,
    columns: Vec<String>,
}

impl Executor for DerivedScan {
    fn columns(&self) -> &[String] {
        &self.columns
    }
    fn next(&mut self) -> Result<Option<Row>> {
        self.input.next()
    }
}

/// Concatenate two inputs (`UNION ALL`), optionally removing rows seen across
/// either side (`UNION`). Output columns are the left input's.
struct Union {
    left: Box<dyn Executor>,
    right: Box<dyn Executor>,
    columns: Vec<String>,
    all: bool,
    seen: HashSet<Vec<u8>>,
    on_left: bool,
}

impl Executor for Union {
    fn columns(&self) -> &[String] {
        &self.columns
    }
    fn next(&mut self) -> Result<Option<Row>> {
        loop {
            let row = if self.on_left {
                let Some(r) = self.left.next()? else {
                    self.on_left = false;
                    continue;
                };
                r
            } else {
                let Some(r) = self.right.next()? else {
                    return Ok(None);
                };
                r
            };
            if self.all || self.seen.insert(group_key_bytes(&row)) {
                return Ok(Some(row));
            }
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

/// A build/probe equi-join. The right (build) side is hashed by its join-key
/// columns; each left (probe) row finds matching right rows by key in O(1), then
/// the full `ON` predicate confirms each candidate (so any extra, non-equi
/// conditions still apply). This replaces the nested-loop scan for equi-joins,
/// turning an O(n*m) join into O(n+m). A join with no usable equality key uses
/// the nested-loop executor instead (decided at build time).
struct HashJoin {
    kind: JoinKind,
    columns: Vec<String>,
    right_width: usize,
    left: Box<dyn Executor>,
    left_columns: Vec<String>,
    /// Key expressions evaluated against a left row.
    left_keys: Vec<Expr>,
    /// The full `ON` predicate, re-checked per candidate as a residual.
    on: Expr,
    /// Build-side rows indexed by their encoded join key.
    table: HashMap<Vec<u8>, Vec<Row>>,
    /// Output rows produced from the current left row, drained one at a time.
    pending: VecDeque<Row>,
}

impl HashJoin {
    fn new(
        left: Box<dyn Executor>,
        mut right: Box<dyn Executor>,
        kind: JoinKind,
        on: Expr,
        left_keys: Vec<Expr>,
        right_keys: &[Expr],
    ) -> Result<Self> {
        let left_columns = left.columns().to_vec();
        let right_columns = right.columns().to_vec();
        let mut columns = left_columns.clone();
        columns.extend(right_columns.iter().cloned());
        let right_width = right_columns.len();

        // Build phase: hash every right row by its key. A row whose key contains
        // NULL can never satisfy an equi-join (NULL = NULL is unknown), so it is
        // dropped from the build side.
        let mut table: HashMap<Vec<u8>, Vec<Row>> = HashMap::new();
        while let Some(r) = right.next()? {
            let key = right_keys
                .iter()
                .map(|e| eval(e, &r, &right_columns))
                .collect::<Result<Vec<_>>>()?;
            if let Some(kb) = join_key_bytes(&key) {
                table.entry(kb).or_default().push(r);
            }
        }

        Ok(Self {
            kind,
            columns,
            right_width,
            left,
            left_columns,
            left_keys,
            on,
            table,
            pending: VecDeque::new(),
        })
    }
}

impl Executor for HashJoin {
    fn columns(&self) -> &[String] {
        &self.columns
    }
    fn next(&mut self) -> Result<Option<Row>> {
        loop {
            if let Some(row) = self.pending.pop_front() {
                return Ok(Some(row));
            }
            let Some(left_row) = self.left.next()? else {
                return Ok(None);
            };
            let key = self
                .left_keys
                .iter()
                .map(|e| eval(e, &left_row, &self.left_columns))
                .collect::<Result<Vec<_>>>()?;
            let mut matched = false;
            if let Some(kb) = join_key_bytes(&key) {
                if let Some(rights) = self.table.get(&kb) {
                    for right in rights {
                        let mut combined = left_row.clone();
                        combined.extend_from_slice(right);
                        if is_truthy(&eval(&self.on, &combined, &self.columns)?) {
                            matched = true;
                            self.pending.push_back(combined);
                        }
                    }
                }
            }
            // A LEFT join keeps an unmatched left row, padded with NULLs.
            if self.kind == JoinKind::Left && !matched {
                let mut combined = left_row;
                combined.extend(std::iter::repeat(Value::Null).take(self.right_width));
                self.pending.push_back(combined);
            }
        }
    }
}

/// Encode join-key values to comparable bytes, or `None` if any is NULL (a NULL
/// key never matches in an equi-join).
fn join_key_bytes(values: &[Value]) -> Option<Vec<u8>> {
    if values.iter().any(|v| matches!(v, Value::Null)) {
        return None;
    }
    Some(group_key_bytes(values))
}

/// Which input a column reference belongs to.
enum Side {
    Left,
    Right,
}

/// Extract column-equality join keys from `on`: for each top-level `AND`
/// conjunct of the form `left_col = right_col`, the left and right key
/// expressions. Returns `None` if there is no usable equality (the caller then
/// falls back to a nested-loop join).
fn extract_equi_keys(
    on: &Expr,
    left_cols: &[String],
    right_cols: &[String],
) -> Option<(Vec<Expr>, Vec<Expr>)> {
    let mut conjuncts = Vec::new();
    collect_and(on, &mut conjuncts);
    let mut left_keys = Vec::new();
    let mut right_keys = Vec::new();
    for c in conjuncts {
        if let Expr::Binary {
            op: BinOp::Eq,
            left,
            right,
        } = c
        {
            match (
                column_side(left, left_cols, right_cols),
                column_side(right, left_cols, right_cols),
            ) {
                (Some(Side::Left), Some(Side::Right)) => {
                    left_keys.push((**left).clone());
                    right_keys.push((**right).clone());
                }
                (Some(Side::Right), Some(Side::Left)) => {
                    left_keys.push((**right).clone());
                    right_keys.push((**left).clone());
                }
                _ => {}
            }
        }
    }
    (!left_keys.is_empty()).then_some((left_keys, right_keys))
}

/// Flatten an `AND` tree into its leaf conjuncts.
fn collect_and<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::Binary {
        op: BinOp::And,
        left,
        right,
    } = expr
    {
        collect_and(left, out);
        collect_and(right, out);
    } else {
        out.push(expr);
    }
}

/// Whether `expr` is a single column that resolves to the left or right input.
fn column_side(expr: &Expr, left_cols: &[String], right_cols: &[String]) -> Option<Side> {
    let name = match expr {
        Expr::Column(n) => n.clone(),
        Expr::QualifiedColumn(q, c) => format!("{q}.{c}"),
        _ => return None,
    };
    if resolves_in(&name, left_cols) {
        Some(Side::Left)
    } else if resolves_in(&name, right_cols) {
        Some(Side::Right)
    } else {
        None
    }
}

/// Whether `name` resolves to exactly one column in `cols` (exact match, or a
/// unique bare-name match), mirroring the evaluator's column resolution.
fn resolves_in(name: &str, cols: &[String]) -> bool {
    if cols.iter().any(|c| c == name) {
        return true;
    }
    !name.contains('.')
        && cols
            .iter()
            .filter(|c| c.rsplit('.').next() == Some(name))
            .count()
            == 1
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

/// A parsed aggregate: a function, its argument (`None` for `COUNT(*)`), and
/// whether it is a `DISTINCT` aggregate.
struct AggSpec {
    func: AggFunc,
    arg: Option<Expr>,
    distinct: bool,
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
    /// Values already folded, for a `DISTINCT` aggregate (empty otherwise).
    seen: HashSet<Vec<u8>>,
}

/// Parse an aggregate call expression into a spec.
fn parse_agg(expr: &Expr) -> Result<AggSpec> {
    let Expr::Func {
        name,
        distinct,
        args,
    } = expr
    else {
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
    Ok(AggSpec {
        func,
        arg,
        distinct: *distinct,
    })
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
    // A DISTINCT aggregate folds each value at most once per group.
    if spec.distinct && !acc.seen.insert(group_key_bytes(std::slice::from_ref(&v))) {
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

/// Window functions (a blocking operator): read all input, then for each window
/// expression compute a value per row over its partition and append it as a new
/// column, preserving input order. Supports `ROW_NUMBER`, `RANK`, `DENSE_RANK`,
/// `LAG`, `LEAD`, and the aggregate functions over the whole partition.
///
/// Frame note: an aggregate window (e.g. `SUM(x) OVER (...)`) is computed over
/// the entire partition and is constant per partition; running-frame semantics
/// (a different value per row when `ORDER BY` is present) are not implemented.
struct Window {
    input: Box<dyn Executor>,
    windows: Vec<Expr>,
    columns: Vec<String>,
    buffered: Option<std::vec::IntoIter<Row>>,
}

impl Window {
    fn new(input: Box<dyn Executor>, windows: Vec<Expr>) -> Self {
        let mut columns = input.columns().to_vec();
        columns.extend(windows.iter().map(ToString::to_string));
        Self {
            input,
            windows,
            columns,
            buffered: None,
        }
    }

    fn materialize(&mut self) -> Result<()> {
        let in_cols = self.input.columns().to_vec();
        let mut rows: Vec<Row> = Vec::new();
        while let Some(r) = self.input.next()? {
            rows.push(r);
        }
        // One result column per window expression, each aligned to `rows`.
        let mut cols: Vec<Vec<Value>> = Vec::with_capacity(self.windows.len());
        for w in &self.windows {
            cols.push(compute_window(w, &rows, &in_cols)?);
        }
        let mut out = Vec::with_capacity(rows.len());
        for (i, mut row) in rows.into_iter().enumerate() {
            for col in &cols {
                row.push(col[i].clone());
            }
            out.push(row);
        }
        self.buffered = Some(out.into_iter());
        Ok(())
    }
}

impl Executor for Window {
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

/// Compute one window expression's value for every input row, returned aligned
/// to `rows` (index `i` holds the value for `rows[i]`).
fn compute_window(expr: &Expr, rows: &[Row], cols: &[String]) -> Result<Vec<Value>> {
    let Expr::Window {
        func,
        distinct,
        args,
        partition_by,
        order_by,
    } = expr
    else {
        return Err(ExecError::Unsupported(format!(
            "not a window function: {expr}"
        )));
    };
    let mut result = vec![Value::Null; rows.len()];

    // Partition the row indices by the partition keys, preserving first-seen
    // order (irrelevant to output, which is written back by original index).
    let mut parts: Vec<Vec<usize>> = Vec::new();
    let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let key = partition_by
            .iter()
            .map(|e| eval(e, row, cols))
            .collect::<Result<Vec<_>>>()?;
        let kb = group_key_bytes(&key);
        let p = if let Some(&p) = index.get(&kb) {
            p
        } else {
            let p = parts.len();
            index.insert(kb, p);
            parts.push(Vec::new());
            p
        };
        parts[p].push(i);
    }

    let descs: Vec<bool> = order_by.iter().map(|o| o.desc).collect();
    for part in &parts {
        // Order the partition by the window ORDER BY. Precompute the keys so
        // the comparator stays infallible, and use a stable sort so peers keep
        // their input order.
        let mut keyed: Vec<(usize, Vec<Value>)> = Vec::with_capacity(part.len());
        for &i in part {
            let k = order_by
                .iter()
                .map(|o| eval(&o.expr, &rows[i], cols))
                .collect::<Result<Vec<_>>>()?;
            keyed.push((i, k));
        }
        keyed.sort_by(|a, b| cmp_order_keys(&a.1, &b.1, &descs));
        compute_partition(func, *distinct, args, &keyed, rows, cols, &mut result)?;
    }
    Ok(result)
}

/// Assign one partition's window values into `result`, indexed by each row's
/// original position. `ordered` is `(original index, order key)` in window
/// order.
fn compute_partition(
    func: &str,
    distinct: bool,
    args: &[Expr],
    ordered: &[(usize, Vec<Value>)],
    rows: &[Row],
    cols: &[String],
    result: &mut [Value],
) -> Result<()> {
    let int = |n: usize| Value::Int(i64::try_from(n).unwrap_or(i64::MAX));
    match func {
        "ROW_NUMBER" => {
            for (pos, (i, _)) in ordered.iter().enumerate() {
                result[*i] = int(pos + 1);
            }
        }
        "RANK" => {
            let mut rank = 0usize;
            for (pos, (i, key)) in ordered.iter().enumerate() {
                if pos == 0 || *key != ordered[pos - 1].1 {
                    rank = pos + 1;
                }
                result[*i] = int(rank);
            }
        }
        "DENSE_RANK" => {
            let mut rank = 0usize;
            for (pos, (i, key)) in ordered.iter().enumerate() {
                if pos == 0 || *key != ordered[pos - 1].1 {
                    rank += 1;
                }
                result[*i] = int(rank);
            }
        }
        "LAG" | "LEAD" => {
            let value_expr = args.first().ok_or_else(|| {
                ExecError::Unsupported(format!("{func} requires a value argument"))
            })?;
            // The offset (default 1) is a constant; evaluate it against any row.
            let offset = match args.get(1) {
                Some(e) => match eval(e, &rows[ordered[0].0], cols)? {
                    Value::Int(n) => isize::try_from(n).unwrap_or(1),
                    _ => 1,
                },
                None => 1,
            };
            let default_expr = args.get(2);
            let len = ordered.len();
            for (pos, &(i, _)) in ordered.iter().enumerate() {
                let signed = isize::try_from(pos).unwrap_or(0);
                let target = if func == "LAG" {
                    signed - offset
                } else {
                    signed + offset
                };
                // A target inside the partition resolves; otherwise the default.
                let in_range = usize::try_from(target).ok().filter(|&t| t < len);
                result[i] = match in_range {
                    Some(t) => eval(value_expr, &rows[ordered[t].0], cols)?,
                    None => match default_expr {
                        Some(d) => eval(d, &rows[i], cols)?,
                        None => Value::Null,
                    },
                };
            }
        }
        "COUNT" | "SUM" | "MIN" | "MAX" | "AVG" => {
            // Whole-partition aggregate: one value shared by every row.
            let spec = parse_agg(&Expr::Func {
                name: func.to_string(),
                distinct,
                args: args.to_vec(),
            })?;
            let mut acc = Acc::default();
            for (i, _) in ordered {
                update_acc(&mut acc, &spec, &rows[*i], cols)?;
            }
            let value = finalize_acc(&acc, &spec);
            for (i, _) in ordered {
                result[*i] = value.clone();
            }
        }
        other => return Err(ExecError::Unsupported(format!("window function {other}"))),
    }
    Ok(())
}

/// Lexicographic order over two window order keys, flipping per-key for `DESC`.
fn cmp_order_keys(a: &[Value], b: &[Value], descs: &[bool]) -> Ordering {
    for ((x, y), desc) in a.iter().zip(b).zip(descs) {
        let ord = sort_cmp(x, y);
        let ord = if *desc { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Lower a physical plan into an executor tree, reading base tables through
/// `source`.
///
/// # Errors
///
/// Returns an error for plan nodes or expressions the executor does not run
/// yet, or if a base table cannot be read.
pub fn build(plan: &PhysicalPlan, source: &dyn TableSource) -> Result<Box<dyn Executor>> {
    build_with(plan, source, &None)
}

/// Like [`build`], but `runner` is threaded into the expression-evaluating
/// operators (`Filter`, `Project`) so they can resolve correlated subqueries.
#[allow(clippy::too_many_lines)]
fn build_with(
    plan: &PhysicalPlan,
    source: &dyn TableSource,
    runner: &Runner,
) -> Result<Box<dyn Executor>> {
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
                    runner: runner.clone(),
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
                runner: runner.clone(),
            }))
        }
        PhysicalPlan::Filter {
            predicate, input, ..
        } => Ok(Box::new(Filter {
            input: build_with(input, source, runner)?,
            predicate: predicate.clone(),
            runner: runner.clone(),
        })),
        PhysicalPlan::Project { items, input, .. } => Ok(Box::new(Project::new(
            build_with(input, source, runner)?,
            items,
            runner.clone(),
        ))),
        PhysicalPlan::Sort { keys, input, .. } => Ok(Box::new(Sort {
            input: build_with(input, source, runner)?,
            keys: keys.clone(),
            buffered: None,
        })),
        PhysicalPlan::Limit {
            n, offset, input, ..
        } => Ok(Box::new(Limit {
            input: build_with(input, source, runner)?,
            to_skip: *offset,
            remaining: *n,
        })),
        PhysicalPlan::Distinct { input, .. } => Ok(Box::new(Distinct {
            input: build_with(input, source, runner)?,
            seen: HashSet::new(),
        })),
        PhysicalPlan::DerivedScan { plan, alias, .. } => {
            let input = build_with(plan, source, runner)?;
            let columns = input
                .columns()
                .iter()
                .map(|c| format!("{alias}.{}", strip_qualifier(c)))
                .collect();
            Ok(Box::new(DerivedScan { input, columns }))
        }
        PhysicalPlan::Union {
            all, left, right, ..
        } => {
            let left = build_with(left, source, runner)?;
            let right = build_with(right, source, runner)?;
            let columns = left.columns().to_vec();
            if right.columns().len() != columns.len() {
                return Err(ExecError::Unsupported(format!(
                    "UNION requires matching column counts ({} vs {})",
                    columns.len(),
                    right.columns().len()
                )));
            }
            Ok(Box::new(Union {
                left,
                right,
                columns,
                all: *all,
                seen: HashSet::new(),
                on_left: true,
            }))
        }
        PhysicalPlan::NestedLoopJoin {
            kind,
            left,
            right,
            on,
            ..
        } => Ok(Box::new(NestedLoopJoin::new(
            build_with(left, source, runner)?,
            build_with(right, source, runner)?,
            *kind,
            on.clone(),
        )?)),
        // The planner chooses a hash join for an equi-join on sizable inputs.
        // Build a real build/probe hash join when an equality key can be
        // extracted from ON; otherwise fall back to the nested-loop executor.
        PhysicalPlan::HashJoin {
            kind,
            left,
            right,
            on,
            ..
        } => {
            let left = build_with(left, source, runner)?;
            let right = build_with(right, source, runner)?;
            match extract_equi_keys(on, left.columns(), right.columns()) {
                Some((left_keys, right_keys)) => Ok(Box::new(HashJoin::new(
                    left,
                    right,
                    *kind,
                    on.clone(),
                    left_keys,
                    &right_keys,
                )?)),
                None => Ok(Box::new(NestedLoopJoin::new(
                    left,
                    right,
                    *kind,
                    on.clone(),
                )?)),
            }
        }
        PhysicalPlan::Aggregate {
            group_by,
            aggregates,
            input,
            ..
        } => Ok(Box::new(Aggregate::new(
            build_with(input, source, runner)?,
            group_by.clone(),
            aggregates,
        )?)),
        PhysicalPlan::Window { windows, input, .. } => Ok(Box::new(Window::new(
            build_with(input, source, runner)?,
            windows.clone(),
        ))),
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
    drain(build(plan, source)?)
}

/// Like [`run`], but `runner` resolves correlated subqueries against the row
/// being evaluated. Use this when the plan may contain correlated subqueries.
///
/// # Errors
///
/// Propagates any build or evaluation error.
pub fn run_with(
    plan: &PhysicalPlan,
    source: &dyn TableSource,
    runner: Rc<dyn SubqueryRunner>,
) -> Result<(Vec<String>, Vec<Row>)> {
    drain(build_with(plan, source, &Some(runner))?)
}

/// Drain an operator to its end, collecting the output columns and all rows.
fn drain(mut op: Box<dyn Executor>) -> Result<(Vec<String>, Vec<Row>)> {
    let columns = op.columns().to_vec();
    let mut rows = Vec::new();
    while let Some(row) = op.next()? {
        rows.push(row);
    }
    Ok((columns, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(columns: &[&str], rows: Vec<Row>) -> Box<dyn Executor> {
        Box::new(Values {
            columns: columns.iter().map(|s| (*s).to_string()).collect(),
            rows: rows.into_iter(),
        })
    }

    fn eq(left: &str, right: &str) -> Expr {
        Expr::Binary {
            op: BinOp::Eq,
            left: Box::new(Expr::Column(left.to_string())),
            right: Box::new(Expr::Column(right.to_string())),
        }
    }

    fn hash_join(
        left: Box<dyn Executor>,
        right: Box<dyn Executor>,
        kind: JoinKind,
        on: Expr,
    ) -> Vec<Row> {
        let (lk, rk) = extract_equi_keys(&on, left.columns(), right.columns()).expect("equi keys");
        let mut op: Box<dyn Executor> =
            Box::new(HashJoin::new(left, right, kind, on, lk, &rk).unwrap());
        let mut rows = Vec::new();
        while let Some(r) = op.next().unwrap() {
            rows.push(r);
        }
        rows
    }

    #[test]
    fn hash_join_inner_matches_by_key() {
        let left = values(
            &["a.id"],
            vec![
                vec![Value::Int(1)],
                vec![Value::Int(2)],
                vec![Value::Int(3)],
            ],
        );
        let right = values(
            &["b.aid", "b.tag"],
            vec![
                vec![Value::Int(1), Value::Text("x".into())],
                vec![Value::Int(1), Value::Text("y".into())],
                vec![Value::Int(3), Value::Text("z".into())],
            ],
        );
        // 1 matches x and y, 3 matches z, 2 matches nothing.
        let rows = hash_join(left, right, JoinKind::Inner, eq("a.id", "b.aid"));
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r[0] == r[1]));
    }

    #[test]
    fn hash_join_left_keeps_unmatched_and_null_keys() {
        let left = values(
            &["a.id"],
            vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Null]],
        );
        let right = values(&["b.aid"], vec![vec![Value::Int(1)]]);
        let rows = hash_join(left, right, JoinKind::Left, eq("a.id", "b.aid"));
        // 1 matches; 2 and NULL are kept, padded with NULL on the right.
        assert_eq!(rows.len(), 3);
        assert!(rows.contains(&vec![Value::Int(1), Value::Int(1)]));
        assert!(rows.contains(&vec![Value::Int(2), Value::Null]));
        assert!(rows.contains(&vec![Value::Null, Value::Null]));
    }

    #[test]
    fn extract_equi_keys_rejects_non_equi_join() {
        let on = Expr::Binary {
            op: BinOp::Gt,
            left: Box::new(Expr::Column("a.id".into())),
            right: Box::new(Expr::Column("b.aid".into())),
        };
        assert!(extract_equi_keys(&on, &["a.id".to_string()], &["b.aid".to_string()]).is_none());
    }
}
