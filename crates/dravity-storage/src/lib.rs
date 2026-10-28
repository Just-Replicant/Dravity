//! Storage for torrent content.
//!
//! A torrent's files form one contiguous content stream (see
//! [`dravity_core::layout`]). A [`Storage`] reads and writes byte ranges of
//! that stream; backends map the ranges onto files, memory, or anything else.
//!
//! * [`FileStorage`] — files below a download directory, opened without
//!   following symbolic links, an LRU cache of open file descriptors, and
//!   positional reads/writes (no seek per block).
//! * [`MemoryStorage`] — a byte buffer, for tests and small torrents.
//!
//! [`write_block`] and [`verify_piece`] adapt piece/block addresses to stream
//! offsets. [`resume`] stores verified pieces plus file stamps so a restart
//! can skip the hash check. All calls block; the session runs them on a
//! blocking thread pool.

mod error;
mod file;
mod memory;
mod open;
mod pos;
pub mod resume;

use dravity_core::{BlockRef, Info, PieceLayout};
use sha1::{Digest, Sha1};

pub use error::StorageError;
pub use file::{DEFAULT_MAX_OPEN_FILES, FileStorage};
pub use memory::MemoryStorage;
pub use resume::{FileStamp, ResumeData, ResumeError, load_resume, remove_resume, save_resume};

/// Byte-range access to a torrent's content stream.
///
/// Implementations must be safe to call from several threads at once; calls
/// for different ranges may run concurrently.
pub trait Storage: Send + Sync + std::fmt::Debug {
    /// Total length of the content stream.
    fn len(&self) -> u64;

    /// Whether the content stream is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Writes `data` at content offset `offset`.
    ///
    /// # Errors
    /// [`StorageError::OutOfBounds`] if the range exceeds [`len`](Self::len),
    /// or an I/O error from the backend.
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StorageError>;

    /// Fills `buf` from content offset `offset`.
    ///
    /// # Errors
    /// [`StorageError::OutOfBounds`] if the range exceeds [`len`](Self::len),
    /// or an I/O error (including data that was never written to disk).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError>;

    /// Flushes written data to durable storage.
    ///
    /// # Errors
    /// I/O errors from the backend.
    fn flush(&self) -> Result<(), StorageError>;

    /// A [`FileStamp`] per file for fastresume, or `None` if the backend
    /// cannot vouch for its data across restarts (the default).
    ///
    /// # Errors
    /// I/O errors from the backend.
    fn stamps(&self) -> Result<Option<Vec<FileStamp>>, StorageError> {
        Ok(None)
    }
}

/// Checks that `offset..offset + len` lies within `total`.
pub(crate) fn check_range(offset: u64, len: usize, total: u64) -> Result<(), StorageError> {
    let len64 = u64::try_from(len).map_err(|_| StorageError::OutOfBounds { offset, len })?;
    match offset.checked_add(len64) {
        Some(end) if end <= total => Ok(()),
        _ => Err(StorageError::OutOfBounds { offset, len }),
    }
}

/// Writes one received block.
///
/// # Errors
/// [`StorageError::InvalidBlock`] if `block` is not valid for `layout` or
/// `data` has the wrong length; otherwise errors from the backend.
pub fn write_block(
    storage: &dyn Storage,
    layout: &PieceLayout,
    block: BlockRef,
    data: &[u8],
) -> Result<(), StorageError> {
    let len_ok = usize::try_from(block.length).is_ok_and(|l| l == data.len());
    let start = layout.piece_offset(block.piece);
    match start {
        Some(start) if len_ok && layout.is_valid_request(block) => {
            storage.write_at(start + u64::from(block.offset), data)
        }
        _ => Err(StorageError::InvalidBlock(block)),
    }
}

/// Reads one block, e.g. to upload it to a peer.
///
/// # Errors
/// [`StorageError::InvalidBlock`] if `block` is not valid for `layout`;
/// otherwise errors from the backend (including data never written).
pub fn read_block(
    storage: &dyn Storage,
    layout: &PieceLayout,
    block: BlockRef,
) -> Result<Vec<u8>, StorageError> {
    let start = layout.piece_offset(block.piece);
    let len = usize::try_from(block.length);
    match (start, len) {
        (Some(start), Ok(len)) if layout.is_valid_request(block) => {
            let mut buf = vec![0u8; len];
            storage.read_at(start + u64::from(block.offset), &mut buf)?;
            Ok(buf)
        }
        _ => Err(StorageError::InvalidBlock(block)),
    }
}

/// Reads piece `piece` and compares its SHA-1 with the metainfo.
///
/// Returns `Ok(false)` on a mismatch *or* when the data cannot be read
/// because it was never written (missing file, short file). Other errors are
/// returned.
///
/// # Errors
/// [`StorageError::InvalidBlock`] for an out-of-range piece; unexpected I/O
/// errors from the backend.
pub fn verify_piece(storage: &dyn Storage, info: &Info, piece: u32) -> Result<bool, StorageError> {
    let layout = info.layout();
    let (Some(range), Some(expected)) = (layout.piece_range(piece), info.piece_hash(piece)) else {
        return Err(StorageError::InvalidBlock(BlockRef {
            piece,
            offset: 0,
            length: 0,
        }));
    };
    let len = usize::try_from(range.end - range.start).map_err(|_| StorageError::OutOfBounds {
        offset: range.start,
        len: usize::MAX,
    })?;
    let mut buf = vec![0u8; len];
    match storage.read_at(range.start, &mut buf) {
        Ok(()) => {}
        Err(e) if e.is_missing_data() => return Ok(false),
        Err(e) => return Err(e),
    }
    let digest: [u8; 20] = Sha1::digest(&buf).into();
    Ok(&digest == expected)
}
