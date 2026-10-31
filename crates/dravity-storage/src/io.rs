//! How [`FileStorage`](crate::FileStorage) moves bytes in and out of an open
//! file.
//!
//! The filesystem backend owns everything about *which* file and *where*:
//! path safety, the descriptor cache, padding, flushing and stamps. A
//! [`FileIo`] adopts each file the storage opens and returns an
//! [`OpenFile`], which performs positional reads, writes and flushes on it
//! until the storage's cache drops it. Another I/O mechanism (such as
//! io_uring, behind the `uring` feature) slots in without touching those
//! rules.
//!
//! Buffers are owned: a write takes [`Bytes`] and a read hands back the
//! `Vec<u8>` it filled, so a backend that must own memory while the kernel
//! uses it (io_uring) moves the caller's buffer instead of copying it.

use std::fs::File;
use std::io;
use std::sync::Arc;

use bytes::Bytes;

use crate::pos;

/// A way of doing file I/O.
///
/// Implementations may be called from several threads at once.
pub trait FileIo: Send + Sync + std::fmt::Debug {
    /// Short name for logs and status (`"blocking"`, `"io_uring"`).
    fn name(&self) -> &'static str;

    /// Takes ownership of an open `file`. It is closed when the returned
    /// handle (and every clone of the `Arc`) is dropped.
    ///
    /// # Errors
    /// When the backend cannot take the file (for io_uring: its thread has
    /// stopped).
    fn adopt(&self, file: File) -> io::Result<Arc<dyn OpenFile>>;
}

/// A file adopted by a [`FileIo`].
///
/// Calls block until the I/O is complete and may come from several threads
/// at once.
pub trait OpenFile: Send + Sync + std::fmt::Debug {
    /// Writes all of `data` at `offset`.
    ///
    /// # Errors
    /// Any I/O error; a write of zero bytes is [`io::ErrorKind::WriteZero`].
    fn write_all_at(&self, data: Bytes, offset: u64) -> io::Result<()>;

    /// Reads exactly `len` bytes from `offset` and appends them to `buf`,
    /// which it returns. Reserve the capacity up front to avoid
    /// reallocating.
    ///
    /// # Errors
    /// Any I/O error; reading past the end of the file is
    /// [`io::ErrorKind::UnexpectedEof`].
    fn read_exact_at(&self, buf: Vec<u8>, len: usize, offset: u64) -> io::Result<Vec<u8>>;

    /// Makes the file's data durable (`fdatasync`).
    ///
    /// # Errors
    /// Any I/O error from the flush.
    fn sync_data(&self) -> io::Result<()>;

    /// Writes each `(offset, data)` of `writes`, in any order (the ranges
    /// do not overlap). Backends with a per-call cost (io_uring) submit them
    /// together and merge adjacent ones; the default writes them one by
    /// one.
    ///
    /// # Errors
    /// The first I/O error; the other writes may or may not have happened.
    fn write_batch(&self, writes: Vec<(u64, Bytes)>) -> io::Result<()> {
        for (offset, data) in writes {
            self.write_all_at(data, offset)?;
        }
        Ok(())
    }

    /// Reads each `(offset, len)` of `reads` into a new vector, returned in
    /// the same order. Backends with a per-call cost (io_uring) submit them
    /// together and merge adjacent ones; the default reads them one by one.
    ///
    /// # Errors
    /// The first I/O error, as [`read_exact_at`](Self::read_exact_at).
    fn read_batch(&self, reads: &[(u64, usize)]) -> io::Result<Vec<Vec<u8>>> {
        reads
            .iter()
            .map(|&(offset, len)| self.read_exact_at(Vec::with_capacity(len), len, offset))
            .collect()
    }

    /// Writes borrowed `data` at `offset`. The default copies it into
    /// [`Bytes`] for [`write_all_at`](Self::write_all_at); backends that can
    /// write from a borrowed slice override it.
    ///
    /// # Errors
    /// As [`write_all_at`](Self::write_all_at).
    fn write_slice_at(&self, data: &[u8], offset: u64) -> io::Result<()> {
        self.write_all_at(Bytes::copy_from_slice(data), offset)
    }

    /// Fills borrowed `buf` from `offset`. The default reads into a new
    /// vector and copies; backends that can read into a borrowed slice
    /// override it.
    ///
    /// # Errors
    /// As [`read_exact_at`](Self::read_exact_at).
    fn read_slice_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let data = self.read_exact_at(Vec::with_capacity(buf.len()), buf.len(), offset)?;
        buf.copy_from_slice(&data);
        Ok(())
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

    fn adopt(&self, file: File) -> io::Result<Arc<dyn OpenFile>> {
        Ok(Arc::new(BlockingFile(file)))
    }
}

/// A file read and written on the calling thread.
#[derive(Debug)]
struct BlockingFile(File);

impl OpenFile for BlockingFile {
    fn write_all_at(&self, data: Bytes, offset: u64) -> io::Result<()> {
        pos::write_all_at(&self.0, &data, offset)
    }

    fn read_exact_at(&self, mut buf: Vec<u8>, len: usize, offset: u64) -> io::Result<Vec<u8>> {
        let start = buf.len();
        let end = start
            .checked_add(len)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        buf.resize(end, 0);
        let target = buf
            .get_mut(start..end)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        pos::read_exact_at(&self.0, target, offset)?;
        Ok(buf)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.0.sync_data()
    }

    fn write_slice_at(&self, data: &[u8], offset: u64) -> io::Result<()> {
        pos::write_all_at(&self.0, data, offset)
    }

    fn read_slice_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        pos::read_exact_at(&self.0, buf, offset)
    }
}
