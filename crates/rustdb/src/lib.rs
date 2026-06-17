//! Top-level rustdb library.
//!
//! Re-exports the public surface of the embedded engine and provides the
//! single entry point used by the CLI and (eventually) the HTTP API server.

#![forbid(unsafe_code)]

pub mod correlated;
pub mod database;
pub mod error;
pub mod index;
pub mod persist;

pub use database::{Database, QueryOutcome};
pub use error::{DbError, Result};

// Re-export the value and type vocabulary so callers (the CLI, a future HTTP
// API) can render results without depending on the SQL crate directly.
pub use rustdb_sql::statement::DataType;
pub use rustdb_sql::Value;
