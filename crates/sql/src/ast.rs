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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// Integer literal.
    Int(i64),
    /// String literal.
    Text(String),
    /// Boolean literal.
    Bool(bool),
    /// SQL `NULL`.
    Null,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(n) => write!(f, "{n}"),
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
        };
        f.write_str(s)
    }
}

/// A unary operator.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum UnOp {
    /// `NOT`
    Not,
    /// unary `-`
    Neg,
}

impl fmt::Display for UnOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Not => f.write_str("NOT "),
            Self::Neg => f.write_str("-"),
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
            Self::Unary { op, expr } => write!(f, "({op}{expr})"),
        }
    }
}
