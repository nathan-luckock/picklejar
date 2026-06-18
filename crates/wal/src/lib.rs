//! Write-ahead log (WAL) and crash recovery.
//!
//! Implements an ARIES-style log: every page mutation produces a log record
//! with a monotonically increasing LSN, the record is fsync'd before the
//! corresponding dirty page can be flushed (WAL ordering invariant), and on
//! restart a three-phase recovery (analysis, redo, undo) restores the
//! database to a consistent committed state.
//!
//! # Sprint 3 surface
//!
//! - [`lsn::Lsn`] and [`lsn::TxnId`] newtypes.
//! - [`record::LogRecord`] enum + serialization.
//! - Writer and reader land in subsequent commits.
//!
//! # Invariant
//!
//! No dirty page is flushed to disk before its log record is durable on disk.

#![forbid(unsafe_code)]

pub mod archive;
pub mod error;
pub mod hook;
pub mod lsn;
pub mod reader;
pub mod record;
pub mod recovery;
pub mod sim;
pub mod workload;
pub mod writer;

pub use error::{Result, WalError};
pub use hook::WalSyncHandle;
pub use lsn::{Lsn, TxnId};
pub use reader::WalReader;
pub use record::{LogRecord, RecordHeader, RecordKind, HEADER_BYTES, MIN_RECORD_BYTES};
pub use recovery::{analyze, recover, redo, undo, Analysis, RecoveryStats, TxnStatus};
pub use sim::{run_seed, FaultDisk, Outcome};
pub use workload::{MiniHeap, Txn};
pub use writer::WalWriter;
