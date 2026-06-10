//! Transaction manager, lock manager, and MVCC.
//!
//! Coordinates concurrent transactions over the storage + WAL layers.
//! Provides snapshot isolation as the baseline; isolation level
//! configuration is a nice-to-have.
//!
//! # Sprint 5 surface
//!
//! - [`manager::TransactionManager`]: xid allocation, status table, snapshots.
//! - [`manager::Snapshot`], [`manager::Transaction`], [`manager::TxnState`],
//!   [`manager::IsolationLevel`].
//! - MVCC visibility, versioned values, and the MVCC table land in
//!   subsequent commits.

#![forbid(unsafe_code)]

pub mod error;
pub mod manager;
pub mod version;
pub mod visibility;

pub use error::{Result, TxnError};
pub use manager::{IsolationLevel, Snapshot, Transaction, TransactionManager, TxnState, Xid};
pub use version::{set_xmax, Version, VERSION_HEADER_SIZE};
