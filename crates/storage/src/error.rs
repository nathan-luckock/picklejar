//! Storage-layer error type.

/// Errors returned by the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// Underlying I/O failed. Wraps [`std::io::Error`] without further
    /// classification — most callers will surface this directly.
    #[error("storage io error: {0}")]
    Io(#[from] std::io::Error),

    /// A page ID was requested that is beyond the current end of the file.
    /// Returned by [`FileManager::read_page`] / [`FileManager::write_page`]
    /// when the caller asks for a page that has not been allocated.
    ///
    /// [`FileManager::read_page`]: crate::file::FileManager::read_page
    /// [`FileManager::write_page`]: crate::file::FileManager::write_page
    #[error("page id {requested} is out of bounds (file has {page_count} page(s))")]
    PageOutOfBounds {
        /// The page ID the caller asked for.
        requested: u64,
        /// The number of pages currently allocated in the file.
        page_count: u64,
    },

    /// The database file's length is not a whole multiple of `PAGE_SIZE`.
    /// Indicates either corruption or a mismatched page-size build.
    #[error("database file length {file_len} is not a multiple of page size {page_size}")]
    MisalignedFile {
        /// Length of the file in bytes.
        file_len: u64,
        /// The page size this build expects.
        page_size: usize,
    },

    /// The page header's `page_type` discriminant doesn't match any known
    /// [`crate::header::PageType`] variant. Likely on-disk corruption.
    #[error("invalid page type discriminant: {0}")]
    InvalidPageType(u16),

    /// A [`HeapPage`] was opened over a page whose header reports a
    /// different `page_type`.
    ///
    /// [`HeapPage`]: crate::heap::HeapPage
    #[error("wrong page type: expected {expected:?}, got {actual:?}")]
    WrongPageType {
        /// The type the caller expected.
        expected: crate::header::PageType,
        /// The type actually stored in the page header.
        actual: crate::header::PageType,
    },

    /// Tuple is larger than what a heap page can ever hold.
    /// Hard cap is `PAGE_SIZE - HEADER_SIZE - SLOT_SIZE`.
    #[error("tuple size {size} bytes is too large for a single page")]
    TupleTooLarge {
        /// The attempted tuple size in bytes.
        size: usize,
    },

    /// Empty tuples are rejected — a zero-length payload would collide with
    /// the tombstone encoding (slot length = 0 ⇒ deleted).
    #[error("empty tuples are not supported on heap pages")]
    EmptyTuple,

    /// The page does not have enough free space (slot directory + tuple
    /// payload) to satisfy this insert.
    #[error("heap page full: needed {needed} bytes, only {available} free")]
    PageFull {
        /// Bytes required by this insert (tuple length + slot entry size).
        needed: u16,
        /// Bytes currently free between the slot directory and the tuple
        /// region.
        available: u16,
    },

    /// `SlotId` is not present in the heap page's slot directory.
    #[error("invalid slot id {slot}: heap page has {slot_count} slot(s)")]
    InvalidSlot {
        /// The caller's slot id.
        slot: u16,
        /// Slots currently in the directory (live + tombstoned).
        slot_count: u16,
    },

    /// Caller tried to delete a slot that was already tombstoned.
    #[error("slot {0} is already tombstoned")]
    SlotAlreadyDeleted(u16),
}

/// Convenience alias for results returned by the storage layer.
pub type Result<T> = std::result::Result<T, StorageError>;
