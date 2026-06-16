//! Expression evaluation over a single row.
//!
//! [`eval`] computes the [`Value`] of an [`Expr`] against one row, whose
//! values are positionally aligned with a list of column names (the operator's
//! output schema). Column references resolve by name; comparisons and
//! arithmetic follow SQL's three-valued logic, where anything involving NULL
//! yields NULL.

use std::cmp::Ordering;

use rustdb_sql::{BinOp, Expr, UnOp, Value};

use crate::error::{ExecError, Result};

/// Evaluate `expr` against `row`, resolving column references against
/// `columns` (positionally aligned with `row`).
pub fn eval(expr: &Expr, row: &[Value], columns: &[String]) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column(name) => resolve(name, row, columns),
        // A qualified reference resolves the full `qualifier.column`, which
        // disambiguates a column present in both sides of a join.
        Expr::QualifiedColumn(qualifier, name) => {
            resolve(&format!("{qualifier}.{name}"), row, columns)
        }
        Expr::Star => Err(ExecError::Unsupported("`*` used as a value".into())),
        Expr::Unary { op, expr } => eval_unary(*op, expr, row, columns),
        Expr::Binary { op, left, right } => eval_binary(*op, left, right, row, columns),
        // An aggregate is computed by the Aggregate operator below; above it,
        // the call resolves to that operator's output column by its name.
        Expr::Func { .. } => resolve(&expr.to_string(), row, columns),
    }
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

fn eval_unary(op: UnOp, expr: &Expr, row: &[Value], columns: &[String]) -> Result<Value> {
    let v = eval(expr, row, columns)?;
    match (op, v) {
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
) -> Result<Value> {
    // AND / OR short-circuit and use three-valued logic, so they evaluate
    // their operands lazily rather than through the NULL-propagating path.
    match op {
        BinOp::And => return eval_and(left, right, row, columns),
        BinOp::Or => return eval_or(left, right, row, columns),
        _ => {}
    }

    let l = eval(left, row, columns)?;
    let r = eval(right, row, columns)?;
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
        BinOp::And | BinOp::Or => unreachable!("handled above"),
    }
}

/// SQL three-valued AND: false dominates, then NULL, then true.
fn eval_and(left: &Expr, right: &Expr, row: &[Value], columns: &[String]) -> Result<Value> {
    let l = truth(&eval(left, row, columns)?)?;
    if l == Some(false) {
        return Ok(Value::Bool(false));
    }
    let r = truth(&eval(right, row, columns)?)?;
    if r == Some(false) {
        return Ok(Value::Bool(false));
    }
    Ok(match (l, r) {
        (Some(true), Some(true)) => Value::Bool(true),
        _ => Value::Null,
    })
}

/// SQL three-valued OR: true dominates, then NULL, then false.
fn eval_or(left: &Expr, right: &Expr, row: &[Value], columns: &[String]) -> Result<Value> {
    let l = truth(&eval(left, row, columns)?)?;
    if l == Some(true) {
        return Ok(Value::Bool(true));
    }
    let r = truth(&eval(right, row, columns)?)?;
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
        (Value::Int(a), Value::Int(b)) => Ok(a.cmp(b)),
        (Value::Float(a), Value::Float(b)) => Ok(a.total_cmp(b)),
        (Value::Int(a), Value::Float(b)) => Ok((*a as f64).total_cmp(b)),
        (Value::Float(a), Value::Int(b)) => Ok(a.total_cmp(&(*b as f64))),
        (Value::Text(a), Value::Text(b)) => Ok(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Ok(a.cmp(b)),
        _ => Err(ExecError::Type(format!("cannot compare {l:?} with {r:?}"))),
    }
}

/// The numeric value of an int or float operand, for arithmetic promotion.
#[allow(clippy::cast_precision_loss)]
fn numeric(v: &Value) -> Result<f64> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
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
    use rustdb_sql::Parser;

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
}
