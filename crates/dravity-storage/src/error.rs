//! Storage errors.

use std::io;
use std::path::PathBuf;

use dravity_core::BlockRef;

/// Errors from a [`Storage`](crate::Storage) backend.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// The byte range lies outside the content stream.
    #[error("range of {len} bytes at offset {offset} is outside the torrent")]
    OutOfBounds {
        /// Requested content offset.
        offset: u64,
        /// Requested length.
        len: usize,
    },
    /// A block or piece address is invalid for the torrent.
    #[error("invalid block {0:?}")]
    InvalidBlock(BlockRef),
    /// A path on disk is (or goes through) a symbolic link.
    #[error("refusing to follow symbolic link at {0}")]
    Symlink(PathBuf),
    /// An I/O error on a file.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// File or directory involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

impl StorageError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Whether the error means the data simply is not there yet (missing or
    /// short file), as opposed to a real failure.
    #[must_use]
    pub fn is_missing_data(&self) -> bool {
        matches!(
            self,
            Self::Io { source, .. }
                if matches!(source.kind(), io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof)
        )
    }
}
