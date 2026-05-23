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
}

/// Convenience alias for results returned by the storage layer.
pub type Result<T> = std::result::Result<T, StorageError>;
