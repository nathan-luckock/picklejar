//! Planner error type.

/// Errors raised while building the catalog or planning a query.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PlanError {
    /// A `CREATE TABLE` named a table that already exists.
    #[error("table already exists: {0}")]
    TableExists(String),

    /// A statement referenced a table that is not in the catalog.
    #[error("unknown table: {0}")]
    UnknownTable(String),

    /// A statement referenced a column that the table does not have.
    #[error("unknown column: {table}.{column}")]
    UnknownColumn {
        /// The table that was searched.
        table: String,
        /// The column that was not found.
        column: String,
    },

    /// A `CREATE INDEX` named a column the table does not have.
    #[error("cannot index unknown column {table}.{column}")]
    IndexUnknownColumn {
        /// Target table.
        table: String,
        /// Target column.
        column: String,
    },

    /// A statement kind cannot be applied to the catalog or planned.
    #[error("unsupported statement for this operation: {0}")]
    Unsupported(String),
}

/// Result alias for the planner.
pub type Result<T> = std::result::Result<T, PlanError>;
