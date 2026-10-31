//! Filesystem backend.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use dravity_core::Info;
use dravity_core::layout::FileLayout;

use crate::io::{BlockingIo, FileIo, OpenFile};
use crate::open::open_beneath;
use crate::{FileStamp, Storage, StorageError, check_range};

/// Default cap on simultaneously open files.
pub const DEFAULT_MAX_OPEN_FILES: usize = 128;

#[derive(Debug)]
struct FileSpec {
    /// Absolute path (download directory joined with the safe relative path).
    path: PathBuf,
    /// The safe relative path below the download directory.
    rel: PathBuf,
    /// BEP 47 padding files are never created; they read as zeros.
    padding: bool,
}

#[derive(Debug, Default)]
struct FdCache {
    /// Files adopted by the storage's [`FileIo`]; dropping the last `Arc`
    /// closes one.
    open: HashMap<usize, Arc<dyn OpenFile>>,
    /// Least recently used first.
    order: VecDeque<usize>,
}

/// Stores a torrent's files below a download directory.
///
/// * Paths come from [`Info::file_path`], which only yields components that
///   passed `dravity_core::path` validation, so nothing escapes the directory.
/// * Symbolic links anywhere between the directory and a file are refused
///   (a hostile torrent cannot be combined with a pre-planted link). On Unix
///   every component is opened relative to its parent directory with
///   `O_NOFOLLOW`, so a link swapped in concurrently is refused too.
/// * Open files are kept in an LRU cache; reads and writes are positional,
///   so concurrent block writes never race on a shared cursor. They go
///   through a [`FileIo`]: [`BlockingIo`] unless [`with_io`](Self::with_io)
///   picks another (io_uring with the `uring` feature). Each file is handed
///   to it once, when it enters the cache, and closed when it leaves.
/// * [`Storage::write_bytes`] and [`Storage::read_vec`] move owned buffers
///   to the backend without copying; the borrowed
///   [`write_at`](Storage::write_at) / [`read_at`](Storage::read_at) copy
///   only where the backend needs owned buffers (io_uring).
/// * Files are created lazily on first write. Reading data that was never
///   written reports [`StorageError::is_missing_data`].
#[derive(Debug)]
pub struct FileStorage {
    root: PathBuf,
    files: Vec<FileSpec>,
    layout: FileLayout,
    cache: Mutex<FdCache>,
    max_open: usize,
    io: Arc<dyn FileIo>,
}

impl FileStorage {
    /// Storage for `info` below `root`. Nothing is created until data is
    /// written.
    ///
    /// # Errors
    /// [`StorageError::OutOfBounds`] if the file lengths overflow (cannot
    /// happen for a validated [`Info`]).
    pub fn new(root: impl Into<PathBuf>, info: &Info) -> Result<Self, StorageError> {
        let root = root.into();
        let files = info
            .files()
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let rel = info.file_path(i).unwrap_or_default();
                FileSpec {
                    path: root.join(&rel),
                    rel,
                    padding: f.is_padding(),
                }
            })
            .collect();
        let layout = FileLayout::new(info.files().iter().map(dravity_core::FileEntry::length))
            .map_err(|_| StorageError::OutOfBounds {
                offset: 0,
                len: usize::MAX,
            })?;
        Ok(Self {
            root,
            files,
            layout,
            cache: Mutex::new(FdCache::default()),
            max_open: DEFAULT_MAX_OPEN_FILES,
            io: Arc::new(BlockingIo),
        })
    }

    /// Performs reads and writes through `io` instead of [`BlockingIo`].
    /// Set it before any I/O: files already cached keep their backend.
    #[must_use]
    pub fn with_io(mut self, io: Arc<dyn FileIo>) -> Self {
        self.io = io;
        self
    }

    /// The I/O mechanism in use.
    #[must_use]
    pub fn io(&self) -> &dyn FileIo {
        self.io.as_ref()
    }

    /// Sets the maximum number of simultaneously open files (at least 1).
    #[must_use]
    pub fn with_max_open_files(mut self, max: usize) -> Self {
        self.max_open = max.max(1);
        self
    }

    /// The download directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Absolute path of file `index`.
    #[must_use]
    pub fn file_path(&self, index: usize) -> Option<&Path> {
        self.files.get(index).map(|f| f.path.as_path())
    }

    /// Number of currently cached open files.
    #[must_use]
    pub fn open_files(&self) -> usize {
        self.lock().open.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FdCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn handle(&self, index: usize, create: bool) -> Result<Arc<dyn OpenFile>, StorageError> {
        let mut cache = self.lock();
        if let Some(file) = cache.open.get(&index).cloned() {
            cache.order.retain(|&i| i != index);
            cache.order.push_back(index);
            return Ok(file);
        }
        let spec = self
            .files
            .get(index)
            .ok_or(StorageError::OutOfBounds { offset: 0, len: 0 })?;
        let file = self
            .io
            .adopt(open_beneath(&self.root, &spec.rel, create)?)
            .map_err(|e| StorageError::io(&spec.path, e))?;
        while cache.open.len() >= self.max_open {
            let Some(evict) = cache.order.pop_front() else {
                break;
            };
            if let Some(open) = cache.open.remove(&evict) {
                // Data must be durable before the descriptor leaves the cache,
                // or a later resume can trust bytes that never reached disk.
                let path = self
                    .files
                    .get(evict)
                    .map(|spec| spec.path.clone())
                    .unwrap_or_default();
                open.sync_data().map_err(|e| StorageError::io(path, e))?;
            }
        }
        cache.open.insert(index, Arc::clone(&file));
        cache.order.push_back(index);
        Ok(file)
    }

    fn sync_index(&self, index: usize, file: &dyn OpenFile) -> Result<(), StorageError> {
        file.sync_data()
            .map_err(|e| StorageError::io(self.path_of(index), e))
    }

    fn path_of(&self, index: usize) -> PathBuf {
        self.files
            .get(index)
            .map(|spec| spec.path.clone())
            .unwrap_or_default()
    }

    /// The parts of `offset..offset + len`, one per file it touches.
    fn pieces(&self, offset: u64, len: usize) -> Result<Vec<Part<'_>>, StorageError> {
        check_range(offset, len, self.len())?;
        let end = offset + len as u64;
        let mut out = Vec::new();
        let mut at = 0usize;
        for slice in self.layout.slices(offset..end) {
            let n = usize::try_from(slice.length)
                .map_err(|_| StorageError::OutOfBounds { offset, len })?;
            let file = self
                .files
                .get(slice.file_index)
                .filter(|spec| !spec.padding)
                .map(|spec| (slice.file_index, spec));
            out.push(Part {
                file,
                file_offset: slice.file_offset,
                at,
                len: n,
            });
            at += n;
        }
        if at == len {
            Ok(out)
        } else {
            Err(StorageError::OutOfBounds { offset, len })
        }
    }
}

/// The part of a content range that falls in one file.
struct Part<'a> {
    /// The file's index and spec; `None` for padding (never stored, reads as
    /// zeros).
    file: Option<(usize, &'a FileSpec)>,
    file_offset: u64,
    /// Where the part starts within the range.
    at: usize,
    len: usize,
}

/// Length and modification time of the file at `path`, if it exists.
fn stamp(path: &Path) -> Result<Option<FileStamp>, StorageError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(FileStamp::Missing)),
        Err(e) => return Err(StorageError::io(path, e)),
    };
    if !meta.is_file() {
        // A symlink or directory in place of a file: never trust it.
        return Ok(None);
    }
    let Ok(modified) = meta.modified() else {
        return Ok(None);
    };
    let mtime_ns = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok());
    Ok(mtime_ns.map(|mtime_ns| FileStamp::Present {
        len: meta.len(),
        mtime_ns,
    }))
}

impl Storage for FileStorage {
    /// Stamps every non-padding file; `None` if a file has no usable
    /// modification time or is not a regular file.
    fn stamps(&self) -> Result<Option<Vec<FileStamp>>, StorageError> {
        let mut out = Vec::with_capacity(self.files.len());
        for spec in &self.files {
            if spec.padding {
                out.push(FileStamp::Missing);
                continue;
            }
            match stamp(&spec.path)? {
                Some(s) => out.push(s),
                None => return Ok(None),
            }
        }
        Ok(Some(out))
    }

    fn len(&self) -> u64 {
        self.layout.total_length()
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        for Part {
            file,
            file_offset,
            at,
            len: n,
        } in self.pieces(offset, data.len())?
        {
            let Some((index, spec)) = file else { continue };
            let chunk = data.get(at..at + n).ok_or(StorageError::OutOfBounds {
                offset,
                len: data.len(),
            })?;
            self.handle(index, true)?
                .write_slice_at(chunk, file_offset)
                .map_err(|e| StorageError::io(&spec.path, e))?;
        }
        Ok(())
    }

    fn write_bytes(&self, offset: u64, data: Bytes) -> Result<(), StorageError> {
        for Part {
            file,
            file_offset,
            at,
            len: n,
        } in self.pieces(offset, data.len())?
        {
            let Some((index, spec)) = file else { continue };
            // `pieces` covers exactly `0..data.len()`, so this is in range.
            let chunk = data.slice(at..at + n);
            self.handle(index, true)?
                .write_all_at(chunk, file_offset)
                .map_err(|e| StorageError::io(&spec.path, e))?;
        }
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        let total = buf.len();
        for Part {
            file,
            file_offset,
            at,
            len: n,
        } in self.pieces(offset, total)?
        {
            let chunk = buf
                .get_mut(at..at + n)
                .ok_or(StorageError::OutOfBounds { offset, len: total })?;
            let Some((index, spec)) = file else {
                chunk.fill(0);
                continue;
            };
            self.handle(index, false)?
                .read_slice_at(chunk, file_offset)
                .map_err(|e| StorageError::io(&spec.path, e))?;
        }
        Ok(())
    }

    fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>, StorageError> {
        let mut buf = Vec::with_capacity(len);
        for Part {
            file,
            file_offset,
            len: n,
            ..
        } in self.pieces(offset, len)?
        {
            let Some((index, spec)) = file else {
                buf.resize(buf.len() + n, 0);
                continue;
            };
            buf = self
                .handle(index, false)?
                .read_exact_at(buf, n, file_offset)
                .map_err(|e| StorageError::io(&spec.path, e))?;
        }
        Ok(buf)
    }

    fn write_many(&self, writes: Vec<(u64, Bytes)>) -> Result<(), StorageError> {
        let mut by_file: BTreeMap<usize, Vec<(u64, Bytes)>> = BTreeMap::new();
        for (offset, data) in writes {
            for part in self.pieces(offset, data.len())? {
                let Some((index, _)) = part.file else {
                    continue;
                };
                // `pieces` covers exactly `0..data.len()`, so this is in range.
                let chunk = data.slice(part.at..part.at + part.len);
                by_file
                    .entry(index)
                    .or_default()
                    .push((part.file_offset, chunk));
            }
        }
        for (index, ops) in by_file {
            self.handle(index, true)?
                .write_batch(ops)
                .map_err(|e| StorageError::io(self.path_of(index), e))?;
        }
        Ok(())
    }

    fn read_many(&self, reads: &[(u64, usize)]) -> Result<Vec<Bytes>, StorageError> {
        let mut out: Vec<Option<Bytes>> = vec![None; reads.len()];
        // Reads that lie in one file, by file: (position in `out`, file
        // offset, length).
        let mut by_file: BTreeMap<usize, Vec<(usize, u64, usize)>> = BTreeMap::new();
        for (i, &(offset, len)) in reads.iter().enumerate() {
            let parts = self.pieces(offset, len)?;
            match parts.as_slice() {
                [
                    Part {
                        file: Some((index, _)),
                        file_offset,
                        ..
                    },
                ] => by_file
                    .entry(*index)
                    .or_default()
                    .push((i, *file_offset, len)),
                // Across files or padding (rare): on its own.
                _ => {
                    if let Some(slot) = out.get_mut(i) {
                        *slot = Some(Bytes::from(self.read_vec(offset, len)?));
                    }
                }
            }
        }
        for (index, ops) in by_file {
            let ranges: Vec<(u64, usize)> = ops.iter().map(|&(_, o, l)| (o, l)).collect();
            let data = self
                .handle(index, false)?
                .read_batch(&ranges)
                .map_err(|e| StorageError::io(self.path_of(index), e))?;
            for ((i, _, _), buf) in ops.into_iter().zip(data) {
                if let Some(slot) = out.get_mut(i) {
                    *slot = Some(Bytes::from(buf));
                }
            }
        }
        out.into_iter()
            .zip(reads)
            .map(|(b, &(offset, len))| b.ok_or(StorageError::OutOfBounds { offset, len }))
            .collect()
    }

    fn flush(&self) -> Result<(), StorageError> {
        let cached: Vec<(usize, Arc<dyn OpenFile>)> = self
            .lock()
            .open
            .iter()
            .map(|(i, f)| (*i, Arc::clone(f)))
            .collect();
        let mut synced = HashSet::new();
        for (index, file) in cached {
            self.sync_index(index, file.as_ref())?;
            synced.insert(index);
        }
        // Files evicted from the LRU cache are not in `open`. Sync those too,
        // so resume data is not written ahead of bytes still in the page cache.
        for index in 0..self.files.len() {
            if synced.contains(&index) {
                continue;
            }
            let Some(spec) = self.files.get(index) else {
                continue;
            };
            if spec.padding {
                continue;
            }
            let file = match open_beneath(&self.root, &spec.rel, false) {
                Ok(file) => file,
                Err(e) if e.is_missing_data() => continue,
                Err(e) => return Err(e),
            };
            self.io
                .adopt(file)
                .and_then(|file| file.sync_data())
                .map_err(|e| StorageError::io(&spec.path, e))?;
        }
        Ok(())
    }
}
