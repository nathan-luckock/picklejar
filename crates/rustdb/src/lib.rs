//! Top-level rustdb library.
//!
//! Re-exports the public surface of the embedded engine and provides the
//! single entry point used by the CLI and (eventually) the HTTP API server.

#![forbid(unsafe_code)]

pub mod database;
pub mod error;

pub use database::{Database, QueryOutcome};
pub use error::{DbError, Result};
