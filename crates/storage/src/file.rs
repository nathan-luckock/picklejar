//! On-disk page I/O.
//!
//! [`FileManager`] owns the database file and is the single source of truth
//! for raw page reads and writes. Higher layers (buffer pool, WAL flush path)
//! go through it; no other component touches the file directly.
//!
//! # Concurrency
//!
//! `FileManager` requires `&mut self` for I/O. Concurrent reads are the job
//! of the buffer pool layer - it caches pages in memory and serializes I/O
//! through a single `FileManager`. Pushing pread/pwrite-style positional I/O
//! down here would be premature optimization for the capstone scope.
//!
//! # Durability
//!
//! [`FileManager::fsync`] is the only durability primitive. It is never
//! called implicitly by reads or writes - callers (the WAL flush path,
//! transaction commit) decide when bytes need to be on platters.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use tracing::debug;

use crate::error::{Result, StorageError};
use crate::page::{Page, PageId, PAGE_SIZE};

/// Owns the database file and provides page-granular reads and writes.
///
/// See the module-level docs for invariants and threading model.
#[derive(Debug)]
pub struct FileManager {
    file: File,
    page_count: u64,
}

impl FileManager {
    /// Open (or create) a database file at `path`.
    ///
    /// If the file does not exist, it is created empty (zero pages).
    /// If it exists, its length must be a whole multiple of [`PAGE_SIZE`];
    /// otherwise [`StorageError::MisalignedFile`] is returned.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let file_len = file.metadata()?.len();
        if file_len % PAGE_SIZE as u64 != 0 {
            return Err(StorageError::MisalignedFile {
                file_len,
                page_size: PAGE_SIZE,
            });
        }
        let page_count = file_len / PAGE_SIZE as u64;
        debug!(
            path = %path.display(),
            page_count,
            "opened database file"
        );
        Ok(Self { file, page_count })
    }

    /// Allocate a new page at the end of the file, extending it by
    /// [`PAGE_SIZE`] bytes. The new page's contents are zero (the file
    /// system's `set_len` guarantees this for newly extended regions).
    pub fn allocate_page(&mut self) -> Result<PageId> {
        let id = PageId::new(self.page_count);
        let new_len = (self.page_count + 1) * PAGE_SIZE as u64;
        self.file.set_len(new_len)?;
        self.page_count += 1;
        debug!(page_id = %id, "allocated page");
        Ok(id)
    }

    /// Read the page with the given ID into `buf`.
    ///
    /// Returns [`StorageError::PageOutOfBounds`] if `id` refers to a page
    /// beyond the current end of the file.
    pub fn read_page(&mut self, id: PageId, buf: &mut Page) -> Result<()> {
        self.check_in_bounds(id)?;
        self.file.seek(SeekFrom::Start(id.byte_offset()))?;
        self.file.read_exact(buf)?;
        Ok(())
    }

    /// Write `buf` to the page with the given ID.
    ///
    /// Returns [`StorageError::PageOutOfBounds`] if `id` refers to a page
    /// beyond the current end of the file. The write is NOT durable until
    /// [`FileManager::fsync`] is called - callers (the WAL flush path) are
    /// responsible for ordering.
    pub fn write_page(&mut self, id: PageId, buf: &Page) -> Result<()> {
        self.check_in_bounds(id)?;
        self.file.seek(SeekFrom::Start(id.byte_offset()))?;
        self.file.write_all(buf)?;
        Ok(())
    }

    /// Force all buffered writes to durable storage.
    ///
    /// This is the only durability primitive in the storage layer.
    pub fn fsync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Number of pages currently allocated in the file.
    #[must_use]
    pub const fn page_count(&self) -> u64 {
        self.page_count
    }

    const fn check_in_bounds(&self, id: PageId) -> Result<()> {
        if id.get() >= self.page_count {
            return Err(StorageError::PageOutOfBounds {
                requested: id.get(),
                page_count: self.page_count,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: a fresh DB file path that does NOT exist yet, so
    /// `FileManager::open` exercises its create-on-missing path.
    fn fresh_db_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        (dir, path)
    }

    #[test]
    fn open_creates_empty_file() {
        let (_dir, path) = fresh_db_path();
        let fm = FileManager::open(&path).expect("open");
        assert_eq!(fm.page_count(), 0);
        assert!(path.exists());
    }

    #[test]
    fn allocate_then_read_returns_zeros() {
        let (_dir, path) = fresh_db_path();
        let mut fm = FileManager::open(&path).expect("open");
        let id = fm.allocate_page().expect("allocate");
        assert_eq!(id, PageId::new(0));
        assert_eq!(fm.page_count(), 1);

        let mut buf: Page = [0xAB; PAGE_SIZE]; // pre-fill to confirm read overwrites
        fm.read_page(id, &mut buf).expect("read");
        assert!(
            buf.iter().all(|&b| b == 0),
            "newly allocated page must be zero"
        );
    }

    #[test]
    fn write_then_read_round_trips() {
        let (_dir, path) = fresh_db_path();
        let mut fm = FileManager::open(&path).expect("open");
        let id = fm.allocate_page().expect("allocate");

        let mut payload: Page = [0; PAGE_SIZE];
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte = u8::try_from(i % 256).unwrap();
        }
        fm.write_page(id, &payload).expect("write");

        let mut read_back: Page = [0; PAGE_SIZE];
        fm.read_page(id, &mut read_back).expect("read");
        assert_eq!(read_back, payload);
    }

    #[test]
    fn read_past_eof_errors() {
        let (_dir, path) = fresh_db_path();
        let mut fm = FileManager::open(&path).expect("open");
        let _ = fm.allocate_page().expect("allocate");

        let mut buf: Page = [0; PAGE_SIZE];
        let err = fm
            .read_page(PageId::new(5), &mut buf)
            .expect_err("must error");
        assert!(
            matches!(
                err,
                StorageError::PageOutOfBounds {
                    requested: 5,
                    page_count: 1
                }
            ),
            "expected PageOutOfBounds, got {err:?}",
        );
    }

    #[test]
    fn write_past_eof_errors() {
        let (_dir, path) = fresh_db_path();
        let mut fm = FileManager::open(&path).expect("open");
        let _ = fm.allocate_page().expect("allocate");

        let payload: Page = [0x42; PAGE_SIZE];
        let err = fm
            .write_page(PageId::new(2), &payload)
            .expect_err("must error");
        assert!(matches!(err, StorageError::PageOutOfBounds { .. }));
    }

    #[test]
    fn fsync_survives_reopen() {
        let (dir, path) = fresh_db_path();
        let payload_id;
        let payload: Page = {
            let mut p: Page = [0; PAGE_SIZE];
            for (i, byte) in p.iter_mut().enumerate() {
                *byte = u8::try_from((i * 3) % 256).unwrap();
            }
            p
        };

        {
            let mut fm = FileManager::open(&path).expect("open");
            payload_id = fm.allocate_page().expect("allocate");
            fm.write_page(payload_id, &payload).expect("write");
            fm.fsync().expect("fsync");
        } // FileManager dropped, file closed

        let mut fm = FileManager::open(&path).expect("reopen");
        assert_eq!(fm.page_count(), 1, "page count must persist across reopen");
        let mut read_back: Page = [0; PAGE_SIZE];
        fm.read_page(payload_id, &mut read_back).expect("read");
        assert_eq!(read_back, payload);
        drop(dir); // explicit; tempdir cleans up on drop
    }

    #[test]
    fn misaligned_file_rejected_on_open() {
        let (_dir, path) = fresh_db_path();
        // Create a file with an odd length (not a multiple of PAGE_SIZE).
        std::fs::write(&path, b"not a page").expect("write");
        let err = FileManager::open(&path).expect_err("must reject");
        assert!(
            matches!(
                err,
                StorageError::MisalignedFile {
                    file_len: 10,
                    page_size: PAGE_SIZE
                }
            ),
            "got {err:?}",
        );
    }

    #[test]
    fn allocates_multiple_pages_sequentially() {
        let (_dir, path) = fresh_db_path();
        let mut fm = FileManager::open(&path).expect("open");
        for expected in 0u64..32 {
            let id = fm.allocate_page().expect("allocate");
            assert_eq!(id, PageId::new(expected));
        }
        assert_eq!(fm.page_count(), 32);
    }
}
