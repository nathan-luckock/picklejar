//! Volcano/iterator-style query executor.
//!
//! Each physical plan node implements an iterator interface (`open` / `next` /
//! `close`). Supports seq scan, index scan, hash join, and nested-loop join.
//!
//! The [`row`] module is the tuple codec shared by the storage glue (which
//! encodes rows on insert) and the scan operators (which decode them).

#![forbid(unsafe_code)]

pub mod error;
pub mod row;

pub use error::{ExecError, Result};
pub use row::{decode_row, encode_row};
