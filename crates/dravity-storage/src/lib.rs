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
//! * [`CachedStorage`] — a middleware that keeps recently read chunks in
//!   a shared, byte-budgeted [`ReadCache`] (for seeding).
//!
//! [`FileStorage`] performs its reads and writes through a [`FileIo`], which
//! adopts each file the storage opens as an [`OpenFile`]:
//! [`BlockingIo`] (`pread`/`pwrite`, the default) or, with the `uring`
//! feature on Linux, `UringIo` (io_uring through `tokio-uring` on a
//! dedicated thread; see its docs for falling back when the kernel or a
//! container refuses io_uring).
//!
//! [`write_block`], [`write_blocks`], [`read_block`], [`read_blocks`] and
//! [`verify_piece`] adapt piece/block addresses to stream offsets; the
//! batched forms let io_uring submit many blocks per round trip. [`resume`] stores verified pieces plus file stamps so a restart
//! can skip the hash check. All calls block; the session runs them on a
//! blocking thread pool.

mod alloc;
pub mod cache;
mod error;
mod file;
mod io;
mod memory;
mod open;
mod pos;
pub mod prescan;
pub mod resume;
#[cfg(all(feature = "uring", target_os = "linux"))]
mod uring;

use bytes::Bytes;
use dravity_core::{BlockRef, Info, PieceLayout};

pub use alloc::{Durability, PreallocateMode};
pub use cache::{CacheStats, CachedStorage, ReadCache};
pub use error::StorageError;
pub use file::{DEFAULT_MAX_OPEN_FILES, FileStorage};
pub use io::{BlockingIo, FileIo, OpenFile};
pub use memory::MemoryStorage;
pub use prescan::absent_pieces;
pub use resume::{FileStamp, ResumeData, ResumeError, load_resume, remove_resume, save_resume};
#[cfg(all(feature = "uring", target_os = "linux"))]
pub use uring::{DEFAULT_URING_ENTRIES, UringIo};

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

    /// Writes owned `data` at content offset `offset`. Backends that hand
    /// buffers to the kernel (io_uring) move it there instead of copying;
    /// the default borrows it for [`write_at`](Self::write_at).
    ///
    /// # Errors
    /// As [`write_at`](Self::write_at).
    fn write_bytes(&self, offset: u64, data: Bytes) -> Result<(), StorageError> {
        self.write_at(offset, &data)
    }

    /// Reads `len` bytes from content offset `offset` into a new vector.
    /// Backends that need owned buffers fill it in place; the default
    /// zero-fills it for [`read_at`](Self::read_at).
    ///
    /// # Errors
    /// As [`read_at`](Self::read_at).
    fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>, StorageError> {
        let mut buf = vec![0; len];
        self.read_at(offset, &mut buf)?;
        Ok(buf)
    }

    /// Writes each `(offset, data)` of `writes` (non-overlapping ranges, in
    /// any order). [`FileStorage`] hands each file's share to its
    /// [`FileIo`] at once, so io_uring submits them together; the default
    /// calls [`write_bytes`](Self::write_bytes) for each.
    ///
    /// # Errors
    /// The first error, as [`write_at`](Self::write_at); some of the writes
    /// may have happened.
    fn write_many(&self, writes: Vec<(u64, Bytes)>) -> Result<(), StorageError> {
        for (offset, data) in writes {
            self.write_bytes(offset, data)?;
        }
        Ok(())
    }

    /// Reads each `(offset, len)` of `reads`, in order. [`FileStorage`]
    /// hands each file's share to its [`FileIo`] at once (io_uring merges
    /// adjacent ranges into one request); the default calls
    /// [`read_vec`](Self::read_vec) for each.
    ///
    /// # Errors
    /// The first error, as [`read_at`](Self::read_at).
    fn read_many(&self, reads: &[(u64, usize)]) -> Result<Vec<Bytes>, StorageError> {
        reads
            .iter()
            .map(|&(offset, len)| self.read_vec(offset, len).map(Bytes::from))
            .collect()
    }

    /// Flushes written data to durable storage.
    ///
    /// # Errors
    /// I/O errors from the backend.
    fn flush(&self) -> Result<(), StorageError>;

    /// Forces the bytes of `offset..offset + len` to disk. The default
    /// flushes everything; file storage syncs only the files the range
    /// touches.
    ///
    /// # Errors
    /// I/O errors from the backend.
    fn sync_range(&self, offset: u64, len: u64) -> Result<(), StorageError> {
        let _ = (offset, len);
        self.flush()
    }

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

/// Writes one received block, handing `data` to the backend without
/// copying it.
///
/// # Errors
/// [`StorageError::InvalidBlock`] if `block` is not valid for `layout` or
/// `data` has the wrong length; otherwise errors from the backend.
pub fn write_block(
    storage: &dyn Storage,
    layout: &PieceLayout,
    block: BlockRef,
    data: Bytes,
) -> Result<(), StorageError> {
    let len_ok = usize::try_from(block.length).is_ok_and(|l| l == data.len());
    let start = layout.piece_offset(block.piece);
    match start {
        Some(start) if len_ok && layout.is_valid_request(block) => {
            storage.write_bytes(start + u64::from(block.offset), data)
        }
        _ => Err(StorageError::InvalidBlock(block)),
    }
}

/// Writes received blocks together (see [`Storage::write_many`]), handing
/// their data to the backend without copying it.
///
/// # Errors
/// [`StorageError::InvalidBlock`] for the first block that is not valid for
/// `layout` or has the wrong length (nothing is written then); otherwise the
/// first error from the backend.
pub fn write_blocks(
    storage: &dyn Storage,
    layout: &PieceLayout,
    blocks: Vec<(BlockRef, Bytes)>,
) -> Result<(), StorageError> {
    let writes = blocks
        .into_iter()
        .map(|(block, data)| Ok((block_offset(layout, block, Some(data.len()))?, data)))
        .collect::<Result<Vec<_>, StorageError>>()?;
    storage.write_many(writes)
}

/// Reads blocks together (see [`Storage::read_many`]), e.g. the requests of
/// one or more peers to upload; returned in the same order.
///
/// # Errors
/// [`StorageError::InvalidBlock`] for the first block that is not valid for
/// `layout`; otherwise the first error from the backend (including data
/// never written).
pub fn read_blocks(
    storage: &dyn Storage,
    layout: &PieceLayout,
    blocks: &[BlockRef],
) -> Result<Vec<Bytes>, StorageError> {
    let reads = blocks
        .iter()
        .map(|&block| {
            let len =
                usize::try_from(block.length).map_err(|_| StorageError::InvalidBlock(block))?;
            Ok((block_offset(layout, block, None)?, len))
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    storage.read_many(&reads)
}

/// The stream offset of `block`, if it is valid for `layout` (and `len`,
/// when given, is its length).
fn block_offset(
    layout: &PieceLayout,
    block: BlockRef,
    len: Option<usize>,
) -> Result<u64, StorageError> {
    let len_ok = len.is_none_or(|l| usize::try_from(block.length).is_ok_and(|b| b == l));
    match layout.piece_offset(block.piece) {
        Some(start) if len_ok && layout.is_valid_request(block) => {
            Ok(start + u64::from(block.offset))
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
            storage.read_vec(start + u64::from(block.offset), len)
        }
        _ => Err(StorageError::InvalidBlock(block)),
    }
}

/// Reads piece `piece` and returns the SHA-256 leaf hash
/// ([`dravity_core::merkle::hash_leaf`]) of each 16 KiB block, in order (the
/// last block may be shorter). Used to find which block of a failed piece
/// was bad (smart banning).
///
/// # Errors
/// [`StorageError::InvalidBlock`] for a piece index out of range, or any
/// read error, including missing data.
pub fn block_hashes(
    storage: &dyn Storage,
    info: &Info,
    piece: u32,
) -> Result<Vec<dravity_core::merkle::Hash>, StorageError> {
    let Some(range) = info.layout().piece_range(piece) else {
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
    let data = storage.read_vec(range.start, len)?;
    Ok(data
        .chunks(dravity_core::BLOCK_SIZE as usize)
        .map(dravity_core::merkle::hash_leaf)
        .collect())
}

/// Reads piece `piece` and checks it against the metainfo with
/// [`Info::check_piece`]: SHA-1 for v1, the BEP 52 Merkle hash for v2, both
/// for hybrid torrents.
///
/// Returns `Ok(false)` on a mismatch *or* when the data cannot be read
/// because it was never written (missing file, short file). Other errors are
/// returned.
///
/// # Errors
/// [`StorageError::InvalidBlock`] for an out-of-range piece; unexpected I/O
/// errors from the backend.
pub fn verify_piece(storage: &dyn Storage, info: &Info, piece: u32) -> Result<bool, StorageError> {
    let Some(range) = info.layout().piece_range(piece) else {
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
    match storage.read_vec(range.start, len) {
        Ok(buf) => Ok(info.check_piece(piece, &buf)),
        Err(e) if e.is_missing_data() => Ok(false),
        Err(e) => Err(e),
    }
}
