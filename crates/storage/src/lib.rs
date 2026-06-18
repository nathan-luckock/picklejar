//! Storage layer for the picklejar engine.
//!
//! Provides page-based on-disk storage, a buffer pool with pin/unpin
//! semantics (Sprint 2), and a B+ tree index (Sprint 2). Everything above
//! this crate (WAL, MVCC, SQL) treats storage as the canonical source of
//! truth for committed data - but mutations flow through the WAL first
//! (see `picklejar-wal`).
//!
//! # Layout
//!
//! - Page size: 8 KiB (see [`page::PAGE_SIZE`]).
//! - Slotted-page format for heap tables (Sprint 1, follow-up issue #4).
//! - B+ tree pages: separate header layout (Sprint 2).
//!
//! # Invariants
//!
//! - A pinned page is never evicted (Sprint 2).
//! - All page handles are RAII; dropping a `PageGuard` unpins exactly once
//!   (Sprint 2).
//! - Writes go through the buffer pool, never directly to disk (Sprint 2).
//!
//! # Sprint 1 surface
//!
//! - [`page::PageId`], [`page::PAGE_SIZE`], [`page::Page`]
//! - [`file::FileManager`] - raw page-granular I/O
//! - [`error::StorageError`]

#![forbid(unsafe_code)]

pub mod btree;
pub mod buffer;
pub mod crc32;
pub mod erasure;
pub mod error;
pub mod file;
pub mod header;
pub mod heap;
pub mod page;
pub mod resilient;
pub mod varbtree;

pub use btree::{
    BTree, InternalPage, LeafPage, RangeScan, TupleRef, INTERNAL_ENTRY_SIZE, LEAF_ENTRY_SIZE,
    MAX_INTERNAL_KEYS, MAX_INTERNAL_KEYS_U16, MAX_LEAF_KEYS, MAX_LEAF_KEYS_U16,
};
pub use buffer::{BufferPool, PageReadGuard, PageWriteGuard, WalSyncHook, K};
pub use error::{Result, StorageError};
pub use file::{Disk, FileManager};
pub use header::{
    compute_checksum, recompute_checksum, verify_checksum, PageHeader, PageType, FLAG_DIRTY,
    FLAG_NEEDS_VACUUM, HEADER_SIZE, HEADER_SIZE_U16,
};
pub use heap::{HeapPage, SlotId, MAX_TUPLE_SIZE, SLOT_SIZE, SLOT_SIZE_U16};
pub use page::{Page, PageId, PAGE_SIZE, PAGE_SIZE_U16};
pub use varbtree::{VarBTree, VarRangeScan, MAX_VAR_KEY};
