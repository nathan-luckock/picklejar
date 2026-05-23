//! Page constants and the `PageId` newtype.
//!
//! A page is the unit of I/O between the storage layer and disk: every read
//! and write goes one page at a time. Page size is fixed for the lifetime of
//! a database; mixing files of different page sizes is unsupported by design.

/// The size of a single page, in bytes. Fixed at 8 KiB.
///
/// Matches Postgres' default. Large enough to amortize per-page overhead
/// (header, slot directory, fsync cost) and small enough that the
/// buffer-pool memory ratio stays reasonable on a laptop demo.
pub const PAGE_SIZE: usize = 8192;

/// A fixed-size buffer holding exactly one page.
pub type Page = [u8; PAGE_SIZE];

/// A monotonically increasing identifier for a page on disk.
///
/// `PageId(0)` is the first page in the file. Page IDs are stable for the
/// life of the database — once allocated, an ID always refers to the same
/// 8 KiB region in the file. Indexes and the WAL both rely on this.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct PageId(pub u64);

impl PageId {
    /// Construct a `PageId` from a raw u64.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw u64 identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The byte offset in the file where this page begins.
    #[must_use]
    pub const fn byte_offset(self) -> u64 {
        self.0 * PAGE_SIZE as u64
    }
}

impl std::fmt::Display for PageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "page#{}", self.0)
    }
}
