//! In-memory storage backend.

use std::sync::{Mutex, PoisonError};

use crate::{Storage, StorageError, check_range};

/// Keeps the whole content stream in memory. Unwritten bytes read as zero.
///
/// ```
/// use dravity_storage::{MemoryStorage, Storage};
///
/// let s = MemoryStorage::new(8);
/// s.write_at(2, b"abc").unwrap();
/// let mut buf = [0u8; 4];
/// s.read_at(1, &mut buf).unwrap();
/// assert_eq!(&buf, b"\0abc");
/// ```
#[derive(Debug)]
pub struct MemoryStorage {
    data: Mutex<Vec<u8>>,
    len: u64,
}

impl MemoryStorage {
    /// Zero-filled storage of `len` bytes.
    ///
    /// # Panics
    /// If `len` does not fit in memory addressing (`usize`).
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn new(len: u64) -> Self {
        let size = usize::try_from(len).expect("torrent too large for memory storage");
        Self {
            data: Mutex::new(vec![0; size]),
            len,
        }
    }

    /// A copy of the current contents.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        self.data
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Storage for MemoryStorage {
    fn len(&self) -> u64 {
        self.len
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        check_range(offset, data.len(), self.len)?;
        let mut guard = self.data.lock().unwrap_or_else(PoisonError::into_inner);
        let start = usize::try_from(offset).map_err(|_| StorageError::OutOfBounds {
            offset,
            len: data.len(),
        })?;
        let dst = guard
            .get_mut(start..start + data.len())
            .ok_or(StorageError::OutOfBounds {
                offset,
                len: data.len(),
            })?;
        dst.copy_from_slice(data);
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        check_range(offset, buf.len(), self.len)?;
        let guard = self.data.lock().unwrap_or_else(PoisonError::into_inner);
        let start = usize::try_from(offset).map_err(|_| StorageError::OutOfBounds {
            offset,
            len: buf.len(),
        })?;
        let src = guard
            .get(start..start + buf.len())
            .ok_or(StorageError::OutOfBounds {
                offset,
                len: buf.len(),
            })?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn flush(&self) -> Result<(), StorageError> {
        Ok(())
    }
}
