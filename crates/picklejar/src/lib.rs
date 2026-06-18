//! Top-level picklejar library.
//!
//! Re-exports the public surface of the embedded engine and provides the
//! single entry point used by the CLI and (eventually) the HTTP API server.

#![forbid(unsafe_code)]

pub mod correlated;
pub mod database;
pub mod error;
pub mod hnsw;
pub mod index;
pub mod keyenc;
pub mod persist;
pub mod security;
pub mod vecsim;

pub use database::{Database, QueryOutcome};
pub use error::{DbError, Result};

// Re-export the value and type vocabulary so callers (the CLI, a future HTTP
// API) can render results without depending on the SQL crate directly.
pub use picklejar_sql::ast;
pub use picklejar_sql::datetime;
pub use picklejar_sql::decimal;
pub use picklejar_sql::statement::DataType;
pub use picklejar_sql::Value;
