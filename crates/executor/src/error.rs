//! Executor error type.

use thiserror::Error;

/// Errors raised while encoding, decoding, or executing rows.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExecError {
    /// A row had a different number of values than the schema has columns.
    #[error("row has {got} values but schema has {expected} columns")]
    RowArity {
        /// Columns in the schema.
        expected: usize,
        /// Values supplied.
        got: usize,
    },
    /// A value did not match its column's declared type.
    #[error("column {column}: expected {expected}, found {found}")]
    RowType {
        /// Zero-based column index.
        column: usize,
        /// The column's declared type.
        expected: &'static str,
        /// The value's actual type.
        found: &'static str,
    },
    /// Encoded row bytes ended before the schema was fully decoded.
    #[error("row bytes truncated while decoding column {column}")]
    RowTruncated {
        /// The column being decoded when the bytes ran out.
        column: usize,
    },
    /// A TEXT column's bytes were not valid UTF-8.
    #[error("column {column}: invalid UTF-8 in TEXT value")]
    RowUtf8 {
        /// Zero-based column index.
        column: usize,
    },
    /// An expression referenced a column not in the current row.
    #[error("unknown column in expression: {0}")]
    UnknownColumn(String),
    /// An expression combined values of incompatible types.
    #[error("type error: {0}")]
    Type(String),
    /// The underlying table source failed (storage or transaction error).
    #[error("table source error: {0}")]
    Source(String),
    /// A plan node the executor does not run yet.
    #[error("{0} is not supported by the executor yet")]
    Unsupported(String),
}

/// Executor result alias.
pub type Result<T> = std::result::Result<T, ExecError>;
