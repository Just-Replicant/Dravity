//! How [`FileStorage`](crate::FileStorage) moves bytes in and out of an open
//! file.
//!
//! The filesystem backend owns everything about *which* file and *where*:
//! path safety, the descriptor cache, padding, flushing and stamps. A
//! [`FileIo`] only performs one positional read, write or flush on a file it
//! is handed, so another I/O mechanism (such as io_uring, behind the `uring`
//! feature) slots in without touching those rules.

use std::fs::File;
use std::io;

use crate::pos;

/// Positional reads and writes on an open file.
///
/// Calls block until the I/O is complete and may come from several threads
/// at once.
pub trait FileIo: Send + Sync + std::fmt::Debug {
    /// Short name for logs and status (`"blocking"`, `"io_uring"`).
    fn name(&self) -> &'static str;

    /// Writes all of `buf` at `offset`.
    ///
    /// # Errors
    /// Any I/O error; a write of zero bytes is [`io::ErrorKind::WriteZero`].
    fn write_all_at(&self, file: &File, buf: &[u8], offset: u64) -> io::Result<()>;

    /// Fills `buf` from `offset`.
    ///
    /// # Errors
    /// Any I/O error; reading past the end of the file is
    /// [`io::ErrorKind::UnexpectedEof`].
    fn read_exact_at(&self, file: &File, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// Makes the file's data durable (`fdatasync`).
    ///
    /// # Errors
    /// Any I/O error from the flush.
    fn sync_data(&self, file: &File) -> io::Result<()> {
        file.sync_data()
    }
}

/// The default: `pread`/`pwrite` (or `seek_read`/`seek_write` on Windows)
/// on the calling thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockingIo;

impl FileIo for BlockingIo {
    fn name(&self) -> &'static str {
        "blocking"
    }

    fn write_all_at(&self, file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
        pos::write_all_at(file, buf, offset)
    }

    fn read_exact_at(&self, file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
        pos::read_exact_at(file, buf, offset)
    }
}
