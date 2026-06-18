//! Expression evaluation over a single row.
//!
//! [`eval`] computes the [`Value`] of an [`Expr`] against one row, whose
//! values are positionally aligned with a list of column names (the operator's
//! output schema). Column references resolve by name; comparisons and
//! arithmetic follow SQL's three-valued logic, where anything involving NULL
//! yields NULL.

use std::cmp::Ordering;

use picklejar_sql::statement::DataType;
use picklejar_sql::{BinOp, Expr, UnOp, Value};

use crate::error::{ExecError, Result};

thread_local! {
    /// The `(current_user, session_user)` the niladic session functions report.
    /// The engine sets this before running a statement; it defaults to empty.
    static SESSION_IDENTITY: std::cell::RefCell<(String, String)> =
        const { std::cell::RefCell::new((String::new(), String::new())) };
}

/// Set the role names the niladic session functions report on this thread.
///
/// `current_user` / `current_role` report the first, `session_user` the second.
/// The engine calls this before each statement so authorization-aware
/// expressions (and RLS policies) see the active role.
pub fn set_session_identity(current_user: &str, session_user: &str) {
    SESSION_IDENTITY.with(|id| {
        *id.borrow_mut() = (current_user.to_string(), session_user.to_string());
    });
}

/// The current user (what `current_user` / `current_role` evaluate to).
fn current_user() -> String {
    SESSION_IDENTITY.with(|id| id.borrow().0.clone())
}

/// The session user (what `session_user` evaluates to).
fn session_user() -> String {
    SESSION_IDENTITY.with(|id| id.borrow().1.clone())
}

/// Evaluates a subquery expression (`Expr::Subquery`, `Expr::InSubquery`, or
/// `Expr::Exists`) against the row currently being evaluated, so a correlated
/// subquery can see the outer query's columns.
///
/// The engine implements this. The executor invokes it only when such a node
/// survives to evaluation, which happens exactly when the subquery is
/// correlated: an uncorrelated subquery is folded to a literal before the plan
/// is built, so it never reaches here.
pub trait SubqueryRunner {
    /// Evaluate `expr` (a subquery node) with `outer_row` bound positionally to
    /// `outer_columns`. Returns a scalar for a scalar subquery and a boolean
    /// for `IN` / `EXISTS`.
    ///
    /// # Errors
    ///
    /// Returns an error if the subquery cannot be planned or run, or if a
    /// scalar subquery yields more than one row.
    fn eval_subquery(
        &self,
        expr: &Expr,
        outer_columns: &[String],
        outer_row: &[Value],
    ) -> Result<Value>;
}

/// Evaluate `expr` against `row`, resolving column references against
/// `columns` (positionally aligned with `row`).
///
/// Subquery nodes are rejected; use [`eval_with`] to supply a
/// [`SubqueryRunner`] for correlated subqueries.
pub fn eval(expr: &Expr, row: &[Value], columns: &[String]) -> Result<Value> {
    eval_with(expr, row, columns, None)
}

/// Like [`eval`], but `runner` evaluates any correlated subquery node against
/// the current `row`. A `None` runner rejects subquery nodes.
pub fn eval_with(
    expr: &Expr,
    row: &[Value],
    columns: &[String],
    runner: Option<&dyn SubqueryRunner>,
) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column(name) => resolve(name, row, columns),
        // A qualified reference resolves the full `qualifier.column`, which
        // disambiguates a column present in both sides of a join.
        Expr::QualifiedColumn(qualifier, name) => {
            resolve(&format!("{qualifier}.{name}"), row, columns)
        }
        Expr::Star => Err(ExecError::Unsupported("`*` used as a value".into())),
        Expr::Parameter(n) => Err(ExecError::Unsupported(format!("unbound parameter ${n}"))),
        Expr::Unary { op, expr } => eval_unary(*op, expr, row, columns, runner),
        Expr::Binary { op, left, right } => eval_binary(*op, left, right, row, columns, runner),
        // A function that appears verbatim as an output column is a materialized
        // value (a GROUP BY key, or an aggregate result computed by the Aggregate
        // operator) and resolves to it; recomputing would fail against a grouped
        // row that no longer carries the inputs. Otherwise it is a scalar call.
        Expr::Func { name, args, .. } => {
            let key = expr.to_string();
            if let Some(i) = columns.iter().position(|c| *c == key) {
                Ok(row[i].clone())
            } else {
                eval_scalar_func(name, args, row, columns, runner)?
                    .map_or_else(|| resolve(&key, row, columns), Ok)
            }
        }
        Expr::Cast { expr, ty } => cast(&eval_with(expr, row, columns, runner)?, *ty),
        Expr::Case {
            operand,
            whens,
            else_result,
        } => eval_case(
            operand.as_deref(),
            whens,
            else_result.as_deref(),
            row,
            columns,
            runner,
        ),
        // A window function resolves to the column the Window operator appended
        // for it, named by the expression's printed form (the same scheme
        // aggregates use).
        Expr::Window { .. } => resolve(&expr.to_string(), row, columns),
        // A correlated subquery: evaluate it against the outer row via the
        // runner. Without a runner (e.g. an operator that does not support
        // correlation), this is an error.
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_) => runner.map_or_else(
            || {
                Err(ExecError::Unsupported(
                    "subquery reached the evaluator (should have been folded)".into(),
                ))
            },
            |r| r.eval_subquery(expr, columns, row),
        ),
    }
}

/// Evaluate a `CASE` expression. The simple form compares `operand` to each
/// `WHEN` value for equality; the searched form treats each `WHEN` as a
/// predicate. The first match's `THEN` is returned, else the `ELSE` (or NULL).
fn eval_case(
    operand: Option<&Expr>,
    whens: &[(Expr, Expr)],
    else_result: Option<&Expr>,
    row: &[Value],
    columns: &[String],
    runner: Option<&dyn SubqueryRunner>,
) -> Result<Value> {
    let target = match operand {
        Some(op) => Some(eval_with(op, row, columns, runner)?),
        None => None,
    };
    for (when, then) in whens {
        let when_val = eval_with(when, row, columns, runner)?;
        let matched = match &target {
            // Simple form: equal, with NULL never matching (SQL semantics).
            Some(t) => {
                !matches!(t, Value::Null)
                    && !matches!(when_val, Value::Null)
                    && compare(t, &when_val)? == Ordering::Equal
            }
            // Searched form: the WHEN is a predicate.
            None => is_truthy(&when_val),
        };
        if matched {
            return eval_with(then, row, columns, runner);
        }
    }
    else_result.map_or(Ok(Value::Null), |e| eval_with(e, row, columns, runner))
}

/// Evaluate a scalar (non-aggregate) function call, or `Ok(None)` if `name` is
/// not a known scalar function (e.g. an aggregate), so the caller can fall back
/// to resolving it as a column.
fn eval_scalar_func(
    name: &str,
    args: &[Expr],
    row: &[Value],
    columns: &[String],
    runner: Option<&dyn SubqueryRunner>,
) -> Result<Option<Value>> {
    match name {
        // Niladic session functions report the active role names.
        "CURRENT_USER" | "CURRENT_ROLE" => Ok(Some(Value::Text(current_user()))),
        "SESSION_USER" => Ok(Some(Value::Text(session_user()))),
        // COALESCE returns the first non-NULL argument; it evaluates lazily.
        "COALESCE" => {
            for a in args {
                let v = eval_with(a, row, columns, runner)?;
                if !matches!(v, Value::Null) {
                    return Ok(Some(v));
                }
            }
            Ok(Some(Value::Null))
        }
        "LENGTH" | "UPPER" | "LOWER" | "ABS" | "ROUND" | "CONCAT" | "NULLIF" | "SUBSTR"
        | "SUBSTRING" | "TRIM" | "LTRIM" | "RTRIM" | "REPLACE" | "MOD" | "POWER" | "POW"
        | "SQRT" | "FLOOR" | "CEIL" | "CEILING" | "RIGHT" | "REPEAT" | "REVERSE" | "INITCAP"
        | "STRPOS" | "POSITION" | "SIGN" | "TRUNC" | "TRUNCATE" | "EXP" | "LN" | "LOG"
        | "GREATEST" | "LEAST" | "DATE_PART" | "DATE_TRUNC" => {
            let vals = args
                .iter()
                .map(|a| eval_with(a, row, columns, runner))
                .collect::<Result<Vec<_>>>()?;
            Ok(Some(apply_scalar(name, &vals)?))
        }
        // Vector / embedding functions (the function forms of the distance
        // operators, plus dimension and magnitude). NULL in any argument yields
        // NULL, as with the other value functions.
        "VECTOR_DIMS" | "L2_NORM" | "L2_DISTANCE" | "L1_DISTANCE" | "COSINE_DISTANCE"
        | "INNER_PRODUCT" => {
            let vals = args
                .iter()
                .map(|a| eval_with(a, row, columns, runner))
                .collect::<Result<Vec<_>>>()?;
            if vals.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Some(Value::Null));
            }
            Ok(Some(apply_vector_func(name, &vals)?))
        }
        _ => Ok(None),
    }
}

/// Apply a vector / embedding scalar function to already-evaluated arguments.
/// `VECTOR_DIMS` returns the component count, `L2_NORM` the Euclidean magnitude,
/// and `L2_DISTANCE` / `COSINE_DISTANCE` / `INNER_PRODUCT` are the function forms
/// of the `<->` / `<=>` / `<#>` operators (pgvector exposes both spellings).
fn apply_vector_func(name: &str, vals: &[Value]) -> Result<Value> {
    let bad = || ExecError::Type(format!("{name} applied to wrong argument types"));
    match (name, vals) {
        ("VECTOR_DIMS", [v]) => {
            let a = as_vector(v)?;
            Ok(Value::Int(i64::try_from(a.len()).unwrap_or(i64::MAX)))
        }
        ("L2_NORM", [v]) => {
            let a = as_vector(v)?;
            let sum: f64 = a.iter().map(|x| f64::from(*x) * f64::from(*x)).sum();
            Ok(Value::Float(sum.sqrt()))
        }
        ("L2_DISTANCE", [a, b]) => vector_distance(BinOp::VecL2, a, b),
        ("L1_DISTANCE", [a, b]) => vector_distance(BinOp::VecL1, a, b),
        ("COSINE_DISTANCE", [a, b]) => vector_distance(BinOp::VecCosine, a, b),
        // pgvector's inner_product is the positive dot product; `<#>` is its
        // negation, so flip the operator's sign back.
        ("INNER_PRODUCT", [a, b]) => match vector_distance(BinOp::VecInner, a, b)? {
            Value::Float(neg) => Ok(Value::Float(-neg)),
            other => Ok(other),
        },
        _ => Err(bad()),
    }
}

/// Apply a fixed-arity scalar function to already-evaluated arguments.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn apply_scalar(name: &str, vals: &[Value]) -> Result<Value> {
    // NULL propagates through the value functions; CONCAT skips NULLs, NULLIF
    // compares them, and GREATEST / LEAST ignore them, so those opt out of the
    // blanket rule.
    if !matches!(name, "CONCAT" | "NULLIF" | "GREATEST" | "LEAST")
        && vals.iter().any(|v| matches!(v, Value::Null))
    {
        return Ok(Value::Null);
    }
    let bad = || ExecError::Type(format!("{name} applied to wrong argument types"));
    let v = match (name, vals) {
        ("LENGTH", [Value::Text(s)]) => {
            Value::Int(i64::try_from(s.chars().count()).unwrap_or(i64::MAX))
        }
        ("UPPER", [Value::Text(s)]) => Value::Text(s.to_uppercase()),
        ("LOWER", [Value::Text(s)]) => Value::Text(s.to_lowercase()),
        ("TRIM", [Value::Text(s)]) => Value::Text(s.trim().to_string()),
        ("LTRIM", [Value::Text(s)]) => Value::Text(s.trim_start().to_string()),
        ("RTRIM", [Value::Text(s)]) => Value::Text(s.trim_end().to_string()),
        ("REPLACE", [Value::Text(s), Value::Text(from), Value::Text(to)]) => {
            Value::Text(s.replace(from.as_str(), to))
        }
        ("SUBSTR" | "SUBSTRING", [Value::Text(s), Value::Int(start)]) => {
            let from = (*start).max(1) as usize - 1;
            Value::Text(s.chars().skip(from).collect())
        }
        ("SUBSTR" | "SUBSTRING", [Value::Text(s), Value::Int(start), Value::Int(len)]) => {
            let from = (*start).max(1) as usize - 1;
            let take = (*len).max(0) as usize;
            Value::Text(s.chars().skip(from).take(take).collect())
        }
        ("ABS", [Value::Int(n)]) => Value::Int(n.wrapping_abs()),
        ("ABS", [Value::Float(x)]) => Value::Float(x.abs()),
        ("MOD", [Value::Int(a), Value::Int(b)]) => {
            if *b == 0 {
                return Err(ExecError::Type("modulo by zero".into()));
            }
            Value::Int(a.wrapping_rem(*b))
        }
        ("POWER" | "POW", [a, b]) => Value::Float(numeric(a)?.powf(numeric(b)?)),
        ("SQRT", [a]) => Value::Float(numeric(a)?.sqrt()),
        // Integer floor / ceil / round / trunc (no fractional part) are no-ops.
        ("FLOOR" | "CEIL" | "CEILING" | "ROUND" | "TRUNC" | "TRUNCATE", [Value::Int(n)]) => {
            Value::Int(*n)
        }
        ("FLOOR", [Value::Float(x)]) => Value::Float(x.floor()),
        ("CEIL" | "CEILING", [Value::Float(x)]) => Value::Float(x.ceil()),
        ("ROUND", [Value::Float(x)]) => Value::Float(x.round()),
        ("ROUND", [Value::Float(x), Value::Int(d)]) => {
            let f = 10f64.powi(i32::try_from(*d).unwrap_or(0));
            Value::Float((x * f).round() / f)
        }
        ("NULLIF", [a, b]) => {
            if a == b {
                Value::Null
            } else {
                a.clone()
            }
        }
        ("CONCAT", parts) => Value::Text(parts.iter().map(value_text).collect()),
        ("RIGHT", [Value::Text(s), Value::Int(n)]) => {
            let count = s.chars().count();
            let take = (*n).max(0) as usize;
            let skip = count.saturating_sub(take);
            Value::Text(s.chars().skip(skip).collect())
        }
        ("REPEAT", [Value::Text(s), Value::Int(n)]) => Value::Text(s.repeat((*n).max(0) as usize)),
        ("REVERSE", [Value::Text(s)]) => Value::Text(s.chars().rev().collect()),
        ("INITCAP", [Value::Text(s)]) => Value::Text(initcap(s)),
        // 1-based index of the first occurrence of the substring, or 0.
        ("STRPOS" | "POSITION", [Value::Text(s), Value::Text(sub)]) => {
            let pos = s
                .find(sub.as_str())
                .map_or(0, |byte| s[..byte].chars().count() + 1);
            Value::Int(i64::try_from(pos).unwrap_or(i64::MAX))
        }
        ("SIGN", [Value::Int(n)]) => Value::Int(n.signum()),
        ("SIGN", [Value::Float(x)]) => Value::Float(if *x == 0.0 { 0.0 } else { x.signum() }),
        ("TRUNC" | "TRUNCATE", [Value::Float(x)]) => Value::Float(x.trunc()),
        ("TRUNC" | "TRUNCATE", [Value::Float(x), Value::Int(d)]) => {
            let f = 10f64.powi(i32::try_from(*d).unwrap_or(0));
            Value::Float((x * f).trunc() / f)
        }
        ("EXP", [a]) => Value::Float(numeric(a)?.exp()),
        ("LN", [a]) => Value::Float(numeric(a)?.ln()),
        ("LOG", [a]) => Value::Float(numeric(a)?.log10()),
        ("LOG", [base, a]) => Value::Float(numeric(a)?.log(numeric(base)?)),
        // GREATEST / LEAST ignore NULLs and return NULL only if all are NULL.
        ("GREATEST" | "LEAST", parts) => greatest_least(name == "GREATEST", parts)?,
        ("DATE_PART", [Value::Text(field), value]) => date_part(field, value)?,
        ("DATE_TRUNC", [Value::Text(field), value]) => date_trunc(field, value)?,
        _ => return Err(bad()),
    };
    Ok(v)
}

/// Split a `DATE` or `TIMESTAMP` value into (epoch days, microseconds into the
/// day). A `DATE` is at midnight.
fn temporal_parts(v: &Value) -> Result<(i64, i64)> {
    use picklejar_sql::datetime::MICROS_PER_DAY;
    match v {
        Value::Date(d) => Ok((*d, 0)),
        Value::Timestamp(m) => Ok((m.div_euclid(MICROS_PER_DAY), m.rem_euclid(MICROS_PER_DAY))),
        other => Err(ExecError::Type(format!(
            "date/time function needs a DATE or TIMESTAMP, found {other:?}"
        ))),
    }
}

/// `DATE_PART(field, value)` / `EXTRACT(field FROM value)`: a numeric component
/// of a temporal value.
fn date_part(field: &str, v: &Value) -> Result<Value> {
    use picklejar_sql::datetime::{civil_from_days, days_from_civil};
    let (days, tod) = temporal_parts(v)?;
    let (year, month, day) = civil_from_days(days);
    let secs = tod / 1_000_000;
    let n = match field.to_ascii_lowercase().as_str() {
        "year" => year,
        "month" => month,
        "day" => day,
        "hour" => secs / 3600,
        "minute" => (secs / 60) % 60,
        "second" => secs % 60,
        // Day of week, Sunday = 0 (1970-01-01 was a Thursday, day 4).
        "dow" => (days.rem_euclid(7) + 4) % 7,
        // Day of year, 1-based.
        "doy" => days - days_from_civil(year, 1, 1) + 1,
        other => return Err(ExecError::Type(format!("unknown date part {other:?}"))),
    };
    Ok(Value::Int(n))
}

/// `DATE_TRUNC(field, value)`: the temporal value floored to the start of the
/// given field, always returned as a `TIMESTAMP`.
fn date_trunc(field: &str, v: &Value) -> Result<Value> {
    use picklejar_sql::datetime::{civil_from_days, days_from_civil, MICROS_PER_DAY};
    let (days, tod) = temporal_parts(v)?;
    let (year, month, _) = civil_from_days(days);
    let secs = tod / 1_000_000;
    let micros = match field.to_ascii_lowercase().as_str() {
        "year" => days_from_civil(year, 1, 1) * MICROS_PER_DAY,
        "month" => days_from_civil(year, month, 1) * MICROS_PER_DAY,
        "day" => days * MICROS_PER_DAY,
        "hour" => days * MICROS_PER_DAY + (secs / 3600) * 3600 * 1_000_000,
        "minute" => days * MICROS_PER_DAY + (secs / 60) * 60 * 1_000_000,
        "second" => days * MICROS_PER_DAY + secs * 1_000_000,
        other => return Err(ExecError::Type(format!("unknown date part {other:?}"))),
    };
    Ok(Value::Timestamp(micros))
}

/// `GREATEST` (when `want_greatest`) or `LEAST` over `vals`, ignoring NULLs and
/// returning NULL only when every argument is NULL.
fn greatest_least(want_greatest: bool, vals: &[Value]) -> Result<Value> {
    let mut best: Option<&Value> = None;
    for v in vals.iter().filter(|v| !matches!(v, Value::Null)) {
        let take = match best {
            None => true,
            Some(cur) => {
                let ord = compare(v, cur)?;
                (want_greatest && ord == Ordering::Greater)
                    || (!want_greatest && ord == Ordering::Less)
            }
        };
        if take {
            best = Some(v);
        }
    }
    Ok(best.cloned().unwrap_or(Value::Null))
}

/// Title-case each whitespace-separated word: first letter upper, rest lower.
fn initcap(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start_of_word = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            start_of_word = true;
            out.push(ch);
        } else if start_of_word {
            out.extend(ch.to_uppercase());
            start_of_word = false;
        } else {
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// Resolve a (possibly qualified) column name to its value in `row`.
///
/// Columns are stored qualified (`qualifier.col`). An exact match wins first
/// (so a qualified reference like `o.cid` resolves directly); otherwise a bare
/// name matches the column whose part after the last `.` equals it, erroring
/// if that is ambiguous across a join.
fn resolve(name: &str, row: &[Value], columns: &[String]) -> Result<Value> {
    if let Some(i) = columns.iter().position(|c| c == name) {
        return Ok(row[i].clone());
    }
    if !name.contains('.') {
        let mut found = None;
        for (i, c) in columns.iter().enumerate() {
            if c.rsplit('.').next() == Some(name) {
                if found.is_some() {
                    return Err(ExecError::UnknownColumn(format!("{name} is ambiguous")));
                }
                found = Some(i);
            }
        }
        if let Some(i) = found {
            return Ok(row[i].clone());
        }
    }
    Err(ExecError::UnknownColumn(name.to_string()))
}

fn eval_unary(
    op: UnOp,
    expr: &Expr,
    row: &[Value],
    columns: &[String],
    runner: Option<&dyn SubqueryRunner>,
) -> Result<Value> {
    let v = eval_with(expr, row, columns, runner)?;
    match (op, v) {
        // IS [NOT] NULL is a total predicate: it never returns NULL, so it is
        // checked before the NULL-propagating arm below.
        (UnOp::IsNull, v) => Ok(Value::Bool(matches!(v, Value::Null))),
        (UnOp::IsNotNull, v) => Ok(Value::Bool(!matches!(v, Value::Null))),
        (_, Value::Null) => Ok(Value::Null),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnOp::Neg, Value::Int(n)) => Ok(Value::Int(n.wrapping_neg())),
        (UnOp::Neg, Value::Float(x)) => Ok(Value::Float(-x)),
        (op, v) => Err(ExecError::Type(format!("cannot apply {op:?} to {v:?}"))),
    }
}

fn eval_binary(
    op: BinOp,
    left: &Expr,
    right: &Expr,
    row: &[Value],
    columns: &[String],
    runner: Option<&dyn SubqueryRunner>,
) -> Result<Value> {
    // AND / OR short-circuit and use three-valued logic, so they evaluate
    // their operands lazily rather than through the NULL-propagating path.
    match op {
        BinOp::And => return eval_and(left, right, row, columns, runner),
        BinOp::Or => return eval_or(left, right, row, columns, runner),
        _ => {}
    }

    let l = eval_with(left, row, columns, runner)?;
    let r = eval_with(right, row, columns, runner)?;
    // NULL propagates through comparisons and arithmetic.
    if l == Value::Null || r == Value::Null {
        return Ok(Value::Null);
    }
    match op {
        BinOp::Eq => Ok(Value::Bool(compare(&l, &r)? == Ordering::Equal)),
        BinOp::Ne => Ok(Value::Bool(compare(&l, &r)? != Ordering::Equal)),
        BinOp::Lt => Ok(Value::Bool(compare(&l, &r)? == Ordering::Less)),
        BinOp::Le => Ok(Value::Bool(compare(&l, &r)? != Ordering::Greater)),
        BinOp::Gt => Ok(Value::Bool(compare(&l, &r)? == Ordering::Greater)),
        BinOp::Ge => Ok(Value::Bool(compare(&l, &r)? != Ordering::Less)),
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => arithmetic(op, &l, &r),
        BinOp::Like => like(&l, &r),
        BinOp::Concat => Ok(Value::Text(value_text(&l) + &value_text(&r))),
        BinOp::JsonGet | BinOp::JsonGetText => json_get(&l, &r, op == BinOp::JsonGetText),
        BinOp::VecL2 | BinOp::VecCosine | BinOp::VecInner | BinOp::VecL1 => {
            vector_distance(op, &l, &r)
        }
        BinOp::And | BinOp::Or => unreachable!("handled above"),
    }
}

/// Read a value as a vector of `f32` components, accepting either a `VECTOR`
/// value or a text literal in the bracketed `[a,b,c]` form (so a query vector can
/// be written inline as a string).
fn as_vector(v: &Value) -> Result<Vec<f32>> {
    match v {
        Value::Vector(x) => Ok(x.clone()),
        Value::Text(s) => picklejar_sql::ast::parse_vector(s)
            .ok_or_else(|| ExecError::Type(format!("not a vector literal: {s:?}"))),
        other => Err(ExecError::Type(format!(
            "a vector distance operator needs a vector operand, found {other:?}"
        ))),
    }
}

/// Evaluate a vector distance operator to a `FLOAT`. Following pgvector:
/// `<->` is L2 (Euclidean) distance, `<=>` is cosine distance
/// (`1 - cosine similarity`), and `<#>` is the negative inner product (negated so
/// ascending order ranks the most similar first). Both operands must share a
/// dimension.
fn vector_distance(op: BinOp, left: &Value, right: &Value) -> Result<Value> {
    let a = as_vector(left)?;
    let b = as_vector(right)?;
    if a.len() != b.len() {
        return Err(ExecError::Type(format!(
            "vector distance needs equal dimensions, got {} and {}",
            a.len(),
            b.len()
        )));
    }
    // Accumulate in f64 to keep the sums well-conditioned for long vectors.
    let dist = match op {
        BinOp::VecL2 => a
            .iter()
            .zip(&b)
            .map(|(x, y)| {
                let d = f64::from(*x) - f64::from(*y);
                d * d
            })
            .sum::<f64>()
            .sqrt(),
        BinOp::VecCosine => {
            let dot: f64 = a
                .iter()
                .zip(&b)
                .map(|(x, y)| f64::from(*x) * f64::from(*y))
                .sum();
            let na: f64 = a.iter().map(|x| f64::from(*x) * f64::from(*x)).sum();
            let nb: f64 = b.iter().map(|y| f64::from(*y) * f64::from(*y)).sum();
            let denom = na.sqrt() * nb.sqrt();
            if denom == 0.0 {
                // Cosine is undefined for a zero vector; define two zeros as
                // identical (0) and a zero against a real vector as maximally
                // dissimilar (1), so the result stays in the usual [0, 2] range.
                if na == 0.0 && nb == 0.0 {
                    0.0
                } else {
                    1.0
                }
            } else {
                1.0 - dot / denom
            }
        }
        BinOp::VecInner => {
            let dot: f64 = a
                .iter()
                .zip(&b)
                .map(|(x, y)| f64::from(*x) * f64::from(*y))
                .sum();
            -dot
        }
        BinOp::VecL1 => a
            .iter()
            .zip(&b)
            .map(|(x, y)| (f64::from(*x) - f64::from(*y)).abs())
            .sum(),
        _ => unreachable!("vector_distance only handles the vector operators"),
    };
    Ok(Value::Float(dist))
}

/// `json -> key` / `json ->> key`: navigate a JSON value by a text member name
/// or an integer array index. `as_text` selects the `->>` form (scalar text);
/// otherwise the result is JSON. A missing member or a non-JSON left side
/// yields NULL.
fn json_get(left: &Value, right: &Value, as_text: bool) -> Result<Value> {
    let Value::Json(text) = left else {
        return Err(ExecError::Type(format!(
            "the -> operator needs JSON on the left, found {left:?}"
        )));
    };
    let Some(doc) = picklejar_sql::json::parse(text) else {
        return Ok(Value::Null);
    };
    let found = match right {
        Value::Text(key) => doc.get_key(key),
        Value::Int(i) => doc.get_index(*i),
        other => {
            return Err(ExecError::Type(format!(
                "the -> operator needs a text key or integer index, found {other:?}"
            )))
        }
    };
    Ok(match found {
        None => Value::Null,
        Some(v) if as_text => Value::Text(v.as_text()),
        Some(v) => Value::Json(picklejar_sql::json::to_string(v)),
    })
}

/// Convert a value to `ty`, the engine for `CAST(expr AS ty)` and `expr::ty`.
///
/// NULL casts to NULL. A text source is parsed; a number, boolean, or temporal
/// value converts where there is a meaningful conversion, and errors otherwise.
///
/// # Errors
///
/// Returns an error if there is no conversion to `ty`, or a text source does not
/// parse as the target type.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn cast(v: &Value, ty: DataType) -> Result<Value> {
    use picklejar_sql::{datetime, decimal};
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    let bad = || ExecError::Type(format!("cannot cast {v:?} to {ty:?}"));
    let parse_fail = |what: &str, s: &str| ExecError::Type(format!("invalid {what}: {s:?}"));
    let out = match ty {
        DataType::Int => match v {
            Value::Int(n) => Value::Int(*n),
            // Float to int rounds to the nearest integer (saturating).
            Value::Float(x) => Value::Int(x.round() as i64),
            Value::Bool(b) => Value::Int(i64::from(*b)),
            Value::Text(s) => Value::Int(s.trim().parse().map_err(|_| parse_fail("integer", s))?),
            Value::Decimal(m, sc) => Value::Int(
                decimal::to_i64(*m, *sc).ok_or_else(|| parse_fail("integer", "decimal"))?,
            ),
            _ => return Err(bad()),
        },
        DataType::Float => match v {
            Value::Float(x) => Value::Float(*x),
            Value::Int(n) => Value::Float(*n as f64),
            Value::Text(s) => Value::Float(s.trim().parse().map_err(|_| parse_fail("float", s))?),
            Value::Decimal(m, sc) => Value::Float(decimal::to_f64(*m, *sc)),
            _ => return Err(bad()),
        },
        DataType::Bool => match v {
            Value::Bool(b) => Value::Bool(*b),
            Value::Int(n) => Value::Bool(*n != 0),
            Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" => Value::Bool(true),
                "false" | "f" | "0" => Value::Bool(false),
                _ => return Err(parse_fail("boolean", s)),
            },
            _ => return Err(bad()),
        },
        // Any value renders to its canonical text form.
        DataType::Text => Value::Text(value_text(v)),
        DataType::Date => match v {
            Value::Date(d) => Value::Date(*d),
            Value::Timestamp(micros) => Value::Date(micros.div_euclid(datetime::MICROS_PER_DAY)),
            Value::Text(s) => {
                Value::Date(datetime::parse_date(s.trim()).ok_or_else(|| parse_fail("date", s))?)
            }
            _ => return Err(bad()),
        },
        DataType::Timestamp => match v {
            Value::Timestamp(m) => Value::Timestamp(*m),
            Value::Date(d) => Value::Timestamp(*d * datetime::MICROS_PER_DAY),
            Value::Text(s) => Value::Timestamp(
                datetime::parse_timestamp(s.trim()).ok_or_else(|| parse_fail("timestamp", s))?,
            ),
            _ => return Err(bad()),
        },
        DataType::Json => match v {
            Value::Json(s) => Value::Json(s.clone()),
            // Parse to validate, then store the original text.
            Value::Text(s) if picklejar_sql::json::is_valid(s) => Value::Json(s.clone()),
            Value::Text(s) => return Err(parse_fail("json", s)),
            _ => return Err(bad()),
        },
        DataType::Decimal => match v {
            Value::Decimal(m, sc) => Value::Decimal(*m, *sc),
            Value::Int(n) => {
                let (m, sc) = decimal::from_i64(*n);
                Value::Decimal(m, sc)
            }
            // Text is exact; a float goes through its shortest decimal text.
            Value::Text(s) => {
                let (m, sc) = decimal::parse(s.trim()).ok_or_else(|| parse_fail("decimal", s))?;
                Value::Decimal(m, sc)
            }
            Value::Float(x) => {
                let text = x.to_string();
                let (m, sc) = decimal::parse(&text).ok_or_else(|| parse_fail("decimal", &text))?;
                Value::Decimal(m, sc)
            }
            _ => return Err(bad()),
        },
        // A vector casts from its text form (`'[1,2,3]'::vector`) or itself.
        DataType::Vector(_) => match v {
            Value::Vector(vec) => Value::Vector(vec.clone()),
            Value::Text(s) => Value::Vector(
                picklejar_sql::ast::parse_vector(s).ok_or_else(|| parse_fail("vector", s))?,
            ),
            _ => return Err(bad()),
        },
    };
    Ok(out)
}

/// Render a non-NULL value as the text `||` and `CONCAT` use.
fn value_text(v: &Value) -> String {
    match v {
        Value::Text(t) => t.clone(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Date(days) => picklejar_sql::datetime::format_date(*days),
        Value::Timestamp(micros) => picklejar_sql::datetime::format_timestamp(*micros),
        Value::Json(s) => s.clone(),
        Value::Decimal(m, s) => picklejar_sql::decimal::format(*m, *s),
        Value::Vector(v) => picklejar_sql::ast::format_vector(v),
        Value::Null => String::new(),
    }
}

/// SQL `LIKE`: both operands must be text. `%` matches any run of characters
/// (including empty), `_` matches exactly one character; every other character
/// matches literally.
fn like(l: &Value, r: &Value) -> Result<Value> {
    let (Value::Text(text), Value::Text(pattern)) = (l, r) else {
        return Err(ExecError::Type(format!(
            "LIKE needs text operands, found {l:?} and {r:?}"
        )));
    };
    Ok(Value::Bool(like_match(text, pattern)))
}

/// Backtracking matcher for a SQL `LIKE` pattern over character slices.
fn like_match(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();
    // `star_*` remembers the last `%`: its pattern position and where in the
    // text it began matching, so a failed branch resumes by letting that `%`
    // absorb one more character.
    let (mut t, mut p) = (0usize, 0usize);
    let (mut star_p, mut star_t): (Option<usize>, usize) = (None, 0);
    while t < text.len() {
        if p < pat.len() && (pat[p] == '_' || pat[p] == text[t]) {
            t += 1;
            p += 1;
        } else if p < pat.len() && pat[p] == '%' {
            star_p = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '%' {
        p += 1;
    }
    p == pat.len()
}

/// SQL three-valued AND: false dominates, then NULL, then true.
fn eval_and(
    left: &Expr,
    right: &Expr,
    row: &[Value],
    columns: &[String],
    runner: Option<&dyn SubqueryRunner>,
) -> Result<Value> {
    let l = truth(&eval_with(left, row, columns, runner)?)?;
    if l == Some(false) {
        return Ok(Value::Bool(false));
    }
    let r = truth(&eval_with(right, row, columns, runner)?)?;
    if r == Some(false) {
        return Ok(Value::Bool(false));
    }
    Ok(match (l, r) {
        (Some(true), Some(true)) => Value::Bool(true),
        _ => Value::Null,
    })
}

/// SQL three-valued OR: true dominates, then NULL, then false.
fn eval_or(
    left: &Expr,
    right: &Expr,
    row: &[Value],
    columns: &[String],
    runner: Option<&dyn SubqueryRunner>,
) -> Result<Value> {
    let l = truth(&eval_with(left, row, columns, runner)?)?;
    if l == Some(true) {
        return Ok(Value::Bool(true));
    }
    let r = truth(&eval_with(right, row, columns, runner)?)?;
    if r == Some(true) {
        return Ok(Value::Bool(true));
    }
    Ok(match (l, r) {
        (Some(false), Some(false)) => Value::Bool(false),
        _ => Value::Null,
    })
}

/// A boolean operand's truth: `Some` for a bool, `None` for NULL, error for
/// any other type.
fn truth(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(ExecError::Type(format!(
            "expected a boolean, found {other:?}"
        ))),
    }
}

/// Total order between two non-NULL values. Ints and floats are comparable to
/// each other (the int is promoted to a float); floats use a total order, so
/// `NaN` sorts consistently rather than erroring.
#[allow(clippy::cast_precision_loss)]
fn compare(l: &Value, r: &Value) -> Result<Ordering> {
    match (l, r) {
        // Int and the i64-backed temporal types compare by their value.
        (Value::Int(a), Value::Int(b))
        | (Value::Date(a), Value::Date(b))
        | (Value::Timestamp(a), Value::Timestamp(b)) => Ok(a.cmp(b)),
        (Value::Float(a), Value::Float(b)) => Ok(a.total_cmp(b)),
        (Value::Int(a), Value::Float(b)) => Ok((*a as f64).total_cmp(b)),
        (Value::Float(a), Value::Int(b)) => Ok(a.total_cmp(&(*b as f64))),
        (Value::Text(a), Value::Text(b)) => Ok(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Ok(a.cmp(b)),
        // Decimal compares exactly with decimal or int; a float operand drops to
        // float comparison.
        (Value::Decimal(am, asc), Value::Decimal(bm, bsc)) => {
            Ok(picklejar_sql::decimal::compare(*am, *asc, *bm, *bsc))
        }
        (Value::Decimal(am, asc), Value::Int(n)) => Ok(picklejar_sql::decimal::compare(
            *am,
            *asc,
            i128::from(*n),
            0,
        )),
        (Value::Int(n), Value::Decimal(bm, bsc)) => Ok(picklejar_sql::decimal::compare(
            i128::from(*n),
            0,
            *bm,
            *bsc,
        )),
        (Value::Decimal(am, asc), Value::Float(x)) => {
            Ok(picklejar_sql::decimal::to_f64(*am, *asc).total_cmp(x))
        }
        (Value::Float(x), Value::Decimal(bm, bsc)) => {
            Ok(x.total_cmp(&picklejar_sql::decimal::to_f64(*bm, *bsc)))
        }
        _ => Err(ExecError::Type(format!("cannot compare {l:?} with {r:?}"))),
    }
}

/// The numeric value of an int or float operand, for arithmetic promotion.
#[allow(clippy::cast_precision_loss)]
fn numeric(v: &Value) -> Result<f64> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        Value::Decimal(m, s) => Ok(picklejar_sql::decimal::to_f64(*m, *s)),
        _ => Err(ExecError::Type(format!(
            "arithmetic needs a number, found {v:?}"
        ))),
    }
}

/// Arithmetic over numbers (already known non-NULL). Two ints stay integer
/// (wrapping); any float operand promotes the whole expression to float.
fn arithmetic(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    if let (Value::Int(a), Value::Int(b)) = (l, r) {
        let out = match op {
            BinOp::Add => a.wrapping_add(*b),
            BinOp::Sub => a.wrapping_sub(*b),
            BinOp::Mul => a.wrapping_mul(*b),
            BinOp::Div => {
                if *b == 0 {
                    return Err(ExecError::Type("division by zero".into()));
                }
                a.wrapping_div(*b)
            }
            _ => unreachable!("only arithmetic ops reach here"),
        };
        return Ok(Value::Int(out));
    }
    // Exact decimal arithmetic when both operands are decimal or integer (no
    // float in play). A float operand falls through to float promotion below.
    let is_dec_or_int = |v: &Value| matches!(v, Value::Int(_) | Value::Decimal(..));
    let has_decimal = matches!(l, Value::Decimal(..)) || matches!(r, Value::Decimal(..));
    if has_decimal && is_dec_or_int(l) && is_dec_or_int(r) {
        use picklejar_sql::decimal;
        let to_dec = |v: &Value| match v {
            Value::Decimal(m, s) => (*m, *s),
            Value::Int(n) => decimal::from_i64(*n),
            _ => unreachable!("guarded to int or decimal"),
        };
        let ((am, asc), (bm, bsc)) = (to_dec(l), to_dec(r));
        let res = match op {
            BinOp::Add => decimal::add(am, asc, bm, bsc),
            BinOp::Sub => decimal::sub(am, asc, bm, bsc),
            BinOp::Mul => decimal::mul(am, asc, bm, bsc),
            BinOp::Div => decimal::div(am, asc, bm, bsc),
            _ => unreachable!("only arithmetic ops reach here"),
        };
        let (m, s) =
            res.ok_or_else(|| ExecError::Type("decimal overflow or division by zero".into()))?;
        return Ok(Value::Decimal(m, s));
    }
    let a = numeric(l)?;
    let b = numeric(r)?;
    let out = match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => {
            if b == 0.0 {
                return Err(ExecError::Type("division by zero".into()));
            }
            a / b
        }
        _ => unreachable!("only arithmetic ops reach here"),
    };
    Ok(Value::Float(out))
}

/// Whether a predicate value passes a WHERE filter: only literal `true` does.
/// NULL and `false` exclude the row, matching SQL.
#[must_use]
pub const fn is_truthy(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use picklejar_sql::Parser;

    fn cols() -> Vec<String> {
        vec!["id".into(), "name".into()]
    }

    fn row() -> Vec<Value> {
        vec![Value::Int(5), Value::Text("alice".into())]
    }

    fn ev(src: &str) -> Value {
        let expr = Parser::from_sql(src).unwrap().parse_expr().unwrap();
        eval(&expr, &row(), &cols()).expect("eval")
    }

    #[test]
    fn literals_and_columns() {
        assert_eq!(ev("42"), Value::Int(42));
        assert_eq!(ev("id"), Value::Int(5));
        assert_eq!(ev("name"), Value::Text("alice".into()));
    }

    #[test]
    fn comparisons_and_arithmetic() {
        assert_eq!(ev("id > 3"), Value::Bool(true));
        assert_eq!(ev("id = 5"), Value::Bool(true));
        assert_eq!(ev("id < 5"), Value::Bool(false));
        assert_eq!(ev("id + 10"), Value::Int(15));
        assert_eq!(ev("id * 2 - 1"), Value::Int(9));
        assert_eq!(ev("name = 'alice'"), Value::Bool(true));
    }

    #[test]
    fn three_valued_logic_with_null() {
        // NULL propagates through comparison.
        assert_eq!(ev("NULL = 5"), Value::Null);
        // false AND null -> false (false dominates).
        assert_eq!(ev("id > 100 AND NULL = 1"), Value::Bool(false));
        // true AND null -> null.
        assert_eq!(ev("id = 5 AND NULL = 1"), Value::Null);
        // true OR null -> true (true dominates).
        assert_eq!(ev("id = 5 OR NULL = 1"), Value::Bool(true));
        // A NULL predicate is not truthy, so the row is filtered out.
        assert!(!is_truthy(&ev("NULL = 1")));
    }

    #[test]
    fn unary_ops() {
        assert_eq!(ev("-id"), Value::Int(-5));
        assert_eq!(ev("NOT id = 5"), Value::Bool(false));
    }

    #[test]
    fn unknown_column_errors() {
        let expr = Parser::from_sql("ghost").unwrap().parse_expr().unwrap();
        let err = eval(&expr, &row(), &cols()).unwrap_err();
        assert!(matches!(err, ExecError::UnknownColumn(c) if c == "ghost"));
    }

    #[test]
    fn division_by_zero_errors() {
        let expr = Parser::from_sql("id / 0").unwrap().parse_expr().unwrap();
        assert!(matches!(
            eval(&expr, &row(), &cols()),
            Err(ExecError::Type(_))
        ));
    }

    #[test]
    fn like_match_patterns() {
        // Literal.
        assert!(like_match("abc", "abc"));
        assert!(!like_match("abc", "abd"));
        // `_` matches exactly one character.
        assert!(like_match("abc", "a_c"));
        assert!(!like_match("ac", "a_c"));
        // `%` matches any run, including empty.
        assert!(like_match("abc", "a%"));
        assert!(like_match("abc", "%c"));
        assert!(like_match("abc", "%b%"));
        assert!(like_match("abc", "%"));
        assert!(like_match("", "%"));
        assert!(like_match("abc", "abc%"));
        // Backtracking: overlapping `%` segments.
        assert!(like_match("xabxaby", "x%ab%y"));
        assert!(!like_match("abc", "%d%"));
        assert!(!like_match("abc", "abcd"));
    }
}
