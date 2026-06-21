//! Top-level picklejar library.
//!
//! Re-exports the public surface of the embedded engine and provides the
//! single entry point used by the CLI and (eventually) the HTTP API server.

#![forbid(unsafe_code)]

pub mod antientropy;
pub mod authmem;
pub mod authsql;
pub mod blindvec;
pub mod bloom;
pub mod captoken;
pub mod certify;
pub mod cmsketch;
pub mod consistenthash;
pub mod correlated;
pub mod countbloom;
pub mod crdtmem;
pub mod cuckoo;
pub mod database;
pub mod error;
pub mod faults;
pub mod forgetmem;
pub mod forwardlog;
pub mod freshness_model;
pub mod hll;
pub mod hnsw;
pub mod index;
pub mod isolation_model;
pub mod keyenc;
pub mod ledger;
pub mod lsh;
pub mod persist;
pub mod pq;
pub mod quantile;
pub mod quantize;
pub mod radiation;
pub mod ratelimit;
pub mod rendezvous;
pub mod reservoir;
pub mod retrieval;
pub mod roaring;
pub mod security;
pub mod shamir;
pub mod skiplist;
pub mod spacesaving;
pub mod valid_time_model;
pub mod vclock;
pub mod vecsim;

pub use database::{BackupReport, Database, ProtectReport, QueryOutcome, RestoreReport};
pub use error::{DbError, Result};

// Re-export the value and type vocabulary so callers (the CLI, a future HTTP
// API) can render results without depending on the SQL crate directly.
pub use picklejar_sql::ast;
pub use picklejar_sql::datetime;
pub use picklejar_sql::decimal;
pub use picklejar_sql::statement::DataType;
pub use picklejar_sql::Value;
