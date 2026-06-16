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
    /// SQL `NULL`.
    Null,
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a.to_bits() == b.to_bits(),
            (Self::Text(a), Self::Text(b)) => a == b,
            (Self::Bool(a), Self::Bool(b)) => a == b,
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
}

impl Expr {
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
            Self::Func { name, args } => {
                write!(f, "{name}(")?;
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
        }
    }
}
