//! Abstract syntax tree for the SQL subset.
//!
//! Statement nodes are added by the DDL / SELECT / DML parser commits; this
//! module starts with the expression tree and the literal / operator types,
//! which everything else builds on.
//!
//! Every node has a `Display` that prints back to canonical SQL. For
//! expressions the printer fully parenthesizes binary and unary operators,
//! so `parse(print(expr)) == expr` for any expression regardless of
//! operator precedence - the basis of the parser round-trip property test.

use std::fmt;

/// A literal value.
///
/// `PartialEq`/`Eq` are hand-written rather than derived because `f64` is not
/// `Eq`. Floats compare by bit pattern, a total reflexive equality (an
/// identical `NaN` equals itself, `+0.0` differs from `-0.0`). That is the
/// structural equality storage and grouping need; the SQL `=` operator's
/// three-valued semantics live in the executor's evaluator.
#[derive(Clone, Debug)]
pub enum Value {
    /// Integer literal.
    Int(i64),
    /// Floating-point literal.
    Float(f64),
    /// String literal.
    Text(String),
    /// Boolean literal.
    Bool(bool),
    /// A `DATE`, as days from the Unix epoch (1970-01-01).
    Date(i64),
    /// A `TIMESTAMP`, as microseconds from the Unix epoch (UTC, no time zone).
    Timestamp(i64),
    /// A `JSON` document, stored as its (validated) text.
    Json(String),
    /// An exact `DECIMAL` / `NUMERIC`, as `(mantissa, scale)`: the value is
    /// `mantissa / 10^scale`.
    Decimal(i128, u32),
    /// SQL `NULL`.
    Null,
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            // Int, Date, and Timestamp are all i64-backed and compare the same.
            (Self::Int(a), Self::Int(b))
            | (Self::Date(a), Self::Date(b))
            | (Self::Timestamp(a), Self::Timestamp(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a.to_bits() == b.to_bits(),
            (Self::Text(a), Self::Text(b)) | (Self::Json(a), Self::Json(b)) => a == b,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            // Decimals are equal by value, so 12.30 and 12.3 group together.
            (Self::Decimal(am, asc), Self::Decimal(bm, bsc)) => {
                crate::decimal::compare(*am, *asc, *bm, *bsc) == std::cmp::Ordering::Equal
            }
            (Self::Null, Self::Null) => true,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(n) => write!(f, "{n}"),
            Self::Float(x) => {
                // Always render a decimal point (or exponent) so re-parsing
                // yields a float, not an integer: `3.0`, not `3`.
                let s = format!("{x}");
                if !x.is_finite() || s.contains(['.', 'e', 'E']) {
                    f.write_str(&s)
                } else {
                    write!(f, "{s}.0")
                }
            }
            Self::Text(s) => {
                // Single-quote, escaping embedded quotes as ''.
                write!(f, "'{}'", s.replace('\'', "''"))
            }
            Self::Bool(true) => write!(f, "TRUE"),
            Self::Bool(false) => write!(f, "FALSE"),
            // Typed-literal form, so `parse(print(v)) == v` and a dumped row
            // re-inserts as the same DATE / TIMESTAMP.
            Self::Date(days) => write!(f, "DATE '{}'", crate::datetime::format_date(*days)),
            Self::Timestamp(micros) => {
                write!(
                    f,
                    "TIMESTAMP '{}'",
                    crate::datetime::format_timestamp(*micros)
                )
            }
            // A quoted-text cast, so the value re-parses through the JSON cast.
            Self::Json(s) => write!(f, "'{}'::json", s.replace('\'', "''")),
            // Typed-literal form, so the exact value re-parses (no float detour).
            Self::Decimal(m, s) => write!(f, "DECIMAL '{}'", crate::decimal::format(*m, *s)),
            Self::Null => write!(f, "NULL"),
        }
    }
}

/// A binary operator.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BinOp {
    /// `=`
    Eq,
    /// `<>` / `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `LIKE` (SQL pattern match: `%` any run, `_` any one character)
    Like,
    /// `||` string concatenation
    Concat,
    /// `->` JSON member / element access, returning JSON.
    JsonGet,
    /// `->>` JSON member / element access, returning text.
    JsonGetText,
}

impl fmt::Display for BinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::And => "AND",
            Self::Or => "OR",
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Like => "LIKE",
            Self::Concat => "||",
            Self::JsonGet => "->",
            Self::JsonGetText => "->>",
        };
        f.write_str(s)
    }
}

/// A unary operator.
///
/// `IsNull` / `IsNotNull` are unary so the existing `Expr::Unary` machinery
/// (binding, aggregate collection) handles `x IS NULL` with no new node; only
/// their rendering is postfix (see `Expr`'s `Display`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum UnOp {
    /// `NOT`
    Not,
    /// unary `-`
    Neg,
    /// postfix `IS NULL`
    IsNull,
    /// postfix `IS NOT NULL`
    IsNotNull,
}

impl fmt::Display for UnOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Not => f.write_str("NOT "),
            Self::Neg => f.write_str("-"),
            // Rendered postfix by `Expr`'s Display; these are for completeness.
            Self::IsNull => f.write_str("IS NULL"),
            Self::IsNotNull => f.write_str("IS NOT NULL"),
        }
    }
}

/// A scalar expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// A bare column reference, `col`.
    Column(String),
    /// A qualified column reference, `table.col`.
    QualifiedColumn(String, String),
    /// A literal value.
    Literal(Value),
    /// A positional parameter `$N` (1-based), bound to a value by the extended
    /// wire protocol before the statement runs.
    Parameter(u32),
    /// `*` (only valid in a projection list; parsed as an expression for
    /// uniformity).
    Star,
    /// A binary operation.
    Binary {
        /// The operator.
        op: BinOp,
        /// Left operand.
        left: Box<Self>,
        /// Right operand.
        right: Box<Self>,
    },
    /// A unary operation.
    Unary {
        /// The operator.
        op: UnOp,
        /// Operand.
        expr: Box<Self>,
    },
    /// A function call, e.g. an aggregate `COUNT(*)` or `SUM(total)`. The name
    /// is canonical upper-case.
    Func {
        /// Upper-cased function name.
        name: String,
        /// `DISTINCT` argument, as in `COUNT(DISTINCT col)`.
        distinct: bool,
        /// Argument expressions (`COUNT(*)` carries a single [`Self::Star`]).
        args: Vec<Self>,
    },
    /// A `CASE` expression. `operand` is set for the simple form
    /// (`CASE x WHEN v THEN ...`) and `None` for the searched form
    /// (`CASE WHEN cond THEN ...`).
    Case {
        /// The value compared against each `WHEN` (simple form), or `None`.
        operand: Option<Box<Self>>,
        /// `(when, then)` branches, tried in order.
        whens: Vec<(Self, Self)>,
        /// The `ELSE` result, or `None` (which yields `NULL`).
        else_result: Option<Box<Self>>,
    },
    /// `CAST(expr AS type)` (or the `expr::type` shorthand): convert a value to
    /// another type. Always printed in the canonical `CAST(... AS ...)` form.
    Cast {
        /// The value being converted.
        expr: Box<Self>,
        /// The target type.
        ty: crate::statement::DataType,
    },
    /// A scalar subquery `(SELECT ...)` used as a value. Evaluated to a single
    /// value before planning (uncorrelated subqueries only).
    Subquery(Box<crate::statement::Statement>),
    /// `expr [NOT] IN (SELECT ...)`. Folded to an `IN`-list before planning.
    InSubquery {
        /// The value tested for membership.
        expr: Box<Self>,
        /// The subquery whose single column is the candidate set.
        query: Box<crate::statement::Statement>,
        /// `NOT IN` when true.
        negated: bool,
    },
    /// `EXISTS (SELECT ...)`: true if the subquery returns any row. Folded to a
    /// boolean before planning. `NOT EXISTS` is the prefix `NOT` over this.
    Exists(Box<crate::statement::Statement>),
    /// A window function call: `func(args) OVER (PARTITION BY ... ORDER BY ...)`.
    /// The Window operator appends one column per distinct window expression;
    /// the projection then resolves it by its printed name, the same mechanism
    /// aggregates use.
    Window {
        /// Upper-cased function name (e.g. `ROW_NUMBER`, `RANK`, `LAG`, `SUM`).
        func: String,
        /// `DISTINCT` argument (carried for symmetry with [`Self::Func`]).
        distinct: bool,
        /// Argument expressions (empty for `ROW_NUMBER` / `RANK`).
        args: Vec<Self>,
        /// `PARTITION BY` keys (empty means the whole input is one partition).
        partition_by: Vec<Self>,
        /// `ORDER BY` items inside the window (empty means unordered).
        order_by: Vec<crate::statement::OrderItem>,
    },
}

impl Expr {
    /// Replace every positional parameter `$N` with `params[N-1]` (or `NULL`
    /// when out of range), recursing through the whole expression and any
    /// nested subqueries. Used by the extended wire protocol to bind values.
    #[must_use]
    pub fn substitute_params(&self, params: &[Value]) -> Self {
        match self {
            Self::Parameter(n) => Self::Literal(
                (*n as usize)
                    .checked_sub(1)
                    .and_then(|i| params.get(i))
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            Self::Binary { op, left, right } => Self::Binary {
                op: *op,
                left: Box::new(left.substitute_params(params)),
                right: Box::new(right.substitute_params(params)),
            },
            Self::Unary { op, expr } => Self::Unary {
                op: *op,
                expr: Box::new(expr.substitute_params(params)),
            },
            Self::Func {
                name,
                distinct,
                args,
            } => Self::Func {
                name: name.clone(),
                distinct: *distinct,
                args: args.iter().map(|a| a.substitute_params(params)).collect(),
            },
            Self::Case {
                operand,
                whens,
                else_result,
            } => Self::Case {
                operand: operand
                    .as_ref()
                    .map(|o| Box::new(o.substitute_params(params))),
                whens: whens
                    .iter()
                    .map(|(w, t)| (w.substitute_params(params), t.substitute_params(params)))
                    .collect(),
                else_result: else_result
                    .as_ref()
                    .map(|e| Box::new(e.substitute_params(params))),
            },
            Self::Cast { expr, ty } => Self::Cast {
                expr: Box::new(expr.substitute_params(params)),
                ty: *ty,
            },
            Self::Subquery(q) => Self::Subquery(Box::new(q.substitute_params(params))),
            Self::Exists(q) => Self::Exists(Box::new(q.substitute_params(params))),
            Self::InSubquery {
                expr,
                query,
                negated,
            } => Self::InSubquery {
                expr: Box::new(expr.substitute_params(params)),
                query: Box::new(query.substitute_params(params)),
                negated: *negated,
            },
            Self::Window {
                func,
                distinct,
                args,
                partition_by,
                order_by,
            } => Self::Window {
                func: func.clone(),
                distinct: *distinct,
                args: args.iter().map(|a| a.substitute_params(params)).collect(),
                partition_by: partition_by
                    .iter()
                    .map(|a| a.substitute_params(params))
                    .collect(),
                order_by: order_by
                    .iter()
                    .map(|o| crate::statement::OrderItem {
                        expr: o.expr.substitute_params(params),
                        desc: o.desc,
                        nulls_first: o.nulls_first,
                    })
                    .collect(),
            },
            Self::Column(_) | Self::QualifiedColumn(..) | Self::Literal(_) | Self::Star => {
                self.clone()
            }
        }
    }

    /// Build a binary expression node.
    #[must_use]
    pub fn binary(op: BinOp, left: Self, right: Self) -> Self {
        Self::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    /// Build a unary expression node.
    #[must_use]
    pub fn unary(op: UnOp, expr: Self) -> Self {
        Self::Unary {
            op,
            expr: Box::new(expr),
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Column(c) => f.write_str(c),
            Self::QualifiedColumn(t, c) => write!(f, "{t}.{c}"),
            Self::Literal(v) => write!(f, "{v}"),
            Self::Parameter(n) => write!(f, "${n}"),
            Self::Star => f.write_str("*"),
            // Fully parenthesized so re-parsing reproduces the same tree.
            Self::Binary { op, left, right } => write!(f, "({left} {op} {right})"),
            // IS NULL / IS NOT NULL render postfix; the other unary ops prefix.
            Self::Unary {
                op: UnOp::IsNull,
                expr,
            } => write!(f, "({expr} IS NULL)"),
            Self::Unary {
                op: UnOp::IsNotNull,
                expr,
            } => write!(f, "({expr} IS NOT NULL)"),
            Self::Unary { op, expr } => write!(f, "({op}{expr})"),
            Self::Func {
                name,
                distinct,
                args,
            } => {
                write!(f, "{name}(")?;
                if *distinct {
                    f.write_str("DISTINCT ")?;
                }
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{a}")?;
                }
                f.write_str(")")
            }
            Self::Case {
                operand,
                whens,
                else_result,
            } => {
                f.write_str("CASE")?;
                if let Some(op) = operand {
                    write!(f, " {op}")?;
                }
                for (w, t) in whens {
                    write!(f, " WHEN {w} THEN {t}")?;
                }
                if let Some(e) = else_result {
                    write!(f, " ELSE {e}")?;
                }
                f.write_str(" END")
            }
            Self::Cast { expr, ty } => write!(f, "CAST({expr} AS {ty})"),
            Self::Subquery(q) => write!(f, "({q})"),
            Self::InSubquery {
                expr,
                query,
                negated,
            } => {
                let kw = if *negated { "NOT IN" } else { "IN" };
                write!(f, "({expr} {kw} ({query}))")
            }
            Self::Exists(q) => write!(f, "EXISTS ({q})"),
            Self::Window {
                func,
                distinct,
                args,
                partition_by,
                order_by,
            } => fmt_window(f, func, *distinct, args, partition_by, order_by),
        }
    }
}

/// Render a window-function call: `func([DISTINCT] args) OVER ([PARTITION BY
/// ...] [ORDER BY ...])`.
fn fmt_window(
    f: &mut fmt::Formatter<'_>,
    func: &str,
    distinct: bool,
    args: &[Expr],
    partition_by: &[Expr],
    order_by: &[crate::statement::OrderItem],
) -> fmt::Result {
    write!(f, "{func}(")?;
    if distinct {
        f.write_str("DISTINCT ")?;
    }
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{a}")?;
    }
    f.write_str(") OVER (")?;
    if !partition_by.is_empty() {
        f.write_str("PARTITION BY ")?;
        for (i, e) in partition_by.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{e}")?;
        }
    }
    if !order_by.is_empty() {
        if !partition_by.is_empty() {
            f.write_str(" ")?;
        }
        f.write_str("ORDER BY ")?;
        for (i, o) in order_by.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{o}")?;
        }
    }
    f.write_str(")")
}
