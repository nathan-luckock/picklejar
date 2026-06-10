//! Error type for the transaction / MVCC layer.

/// Errors from the transaction and MVCC layer.
#[derive(Debug, thiserror::Error)]
pub enum TxnError {
    /// A versioned value buffer was too short to hold its header.
    #[error("version buffer too short: {len} bytes, need at least {min}")]
    VersionTruncated {
        /// Bytes available.
        len: usize,
        /// Minimum header bytes required.
        min: usize,
    },

    /// A storage-layer operation failed underneath the MVCC layer.
    #[error("storage error: {0}")]
    Storage(#[from] rustdb_storage::StorageError),

    /// A WAL operation failed underneath the MVCC layer.
    #[error("wal error: {0}")]
    Wal(#[from] rustdb_wal::WalError),

    /// An update or delete targeted a key with no version visible to the
    /// transaction.
    #[error("key not found or not visible: {0}")]
    KeyNotVisible(u64),
}

/// Convenience alias for results in the transaction layer.
pub type Result<T> = std::result::Result<T, TxnError>;
