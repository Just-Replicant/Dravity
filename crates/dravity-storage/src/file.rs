//! Filesystem backend.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use dravity_core::Info;
use dravity_core::layout::FileLayout;

use crate::{FileStamp, Storage, StorageError, check_range, pos};

/// Default cap on simultaneously open files.
pub const DEFAULT_MAX_OPEN_FILES: usize = 128;

#[derive(Debug)]
struct FileSpec {
    /// Absolute path (download directory joined with the safe relative path).
    path: PathBuf,
    /// BEP 47 padding files are never created; they read as zeros.
    padding: bool,
}

#[derive(Debug, Default)]
struct FdCache {
    open: HashMap<usize, Arc<File>>,
    /// Least recently used first.
    order: VecDeque<usize>,
}

/// Stores a torrent's files below a download directory.
///
/// * Paths come from [`Info::file_path`], which only yields components that
///   passed `dravity_core::path` validation, so nothing escapes the directory.
/// * Existing symbolic links anywhere between the directory and a file are
///   refused (a hostile torrent cannot be combined with a pre-planted link).
/// * Open files are kept in an LRU cache; reads and writes are positional,
///   so concurrent block writes never race on a shared cursor.
/// * Files are created lazily on first write. Reading data that was never
///   written reports [`StorageError::is_missing_data`].
#[derive(Debug)]
pub struct FileStorage {
    root: PathBuf,
    files: Vec<FileSpec>,
    layout: FileLayout,
    cache: Mutex<FdCache>,
    max_open: usize,
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
            .map(|(i, f)| FileSpec {
                path: root.join(info.file_path(i).unwrap_or_default()),
                padding: f.is_padding(),
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
        })
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

    /// Refuses symlinks on every existing component below the root.
    fn check_no_symlinks(&self, path: &Path) -> Result<(), StorageError> {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return Err(StorageError::Symlink(path.to_path_buf()));
        };
        let mut current = self.root.clone();
        for component in rel.components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(StorageError::Symlink(current));
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(e) => return Err(StorageError::io(current, e)),
            }
        }
        Ok(())
    }

    fn handle(&self, index: usize, create: bool) -> Result<Arc<File>, StorageError> {
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
        self.check_no_symlinks(&spec.path)?;
        if create {
            if let Some(parent) = spec.path.parent() {
                fs::create_dir_all(parent).map_err(|e| StorageError::io(parent, e))?;
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(&spec.path)
            .map_err(|e| StorageError::io(&spec.path, e))?;
        let file = Arc::new(file);
        while cache.open.len() >= self.max_open {
            let Some(evict) = cache.order.pop_front() else {
                break;
            };
            cache.open.remove(&evict);
        }
        cache.open.insert(index, Arc::clone(&file));
        cache.order.push_back(index);
        Ok(file)
    }
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
        check_range(offset, data.len(), self.len())?;
        let end = offset + data.len() as u64;
        let mut consumed = 0usize;
        for slice in self.layout.slices(offset..end) {
            let len = usize::try_from(slice.length).map_err(|_| StorageError::OutOfBounds {
                offset,
                len: data.len(),
            })?;
            let chunk = data
                .get(consumed..consumed + len)
                .ok_or(StorageError::OutOfBounds {
                    offset,
                    len: data.len(),
                })?;
            consumed += len;
            let Some(spec) = self.files.get(slice.file_index) else {
                continue;
            };
            if spec.padding {
                continue;
            }
            let file = self.handle(slice.file_index, true)?;
            pos::write_all_at(&file, chunk, slice.file_offset)
                .map_err(|e| StorageError::io(&spec.path, e))?;
        }
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        check_range(offset, buf.len(), self.len())?;
        let total = buf.len();
        let end = offset + total as u64;
        let mut filled = 0usize;
        for slice in self.layout.slices(offset..end) {
            let len = usize::try_from(slice.length)
                .map_err(|_| StorageError::OutOfBounds { offset, len: total })?;
            let chunk = buf
                .get_mut(filled..filled + len)
                .ok_or(StorageError::OutOfBounds { offset, len: total })?;
            filled += len;
            let Some(spec) = self.files.get(slice.file_index) else {
                continue;
            };
            if spec.padding {
                chunk.fill(0);
                continue;
            }
            let file = self.handle(slice.file_index, false)?;
            pos::read_exact_at(&file, chunk, slice.file_offset)
                .map_err(|e| StorageError::io(&spec.path, e))?;
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), StorageError> {
        let files: Vec<(usize, Arc<File>)> = self
            .lock()
            .open
            .iter()
            .map(|(i, f)| (*i, Arc::clone(f)))
            .collect();
        for (index, file) in files {
            file.sync_data().map_err(|e| {
                StorageError::io(
                    self.files
                        .get(index)
                        .map(|f| f.path.clone())
                        .unwrap_or_default(),
                    e,
                )
            })?;
        }
        Ok(())
    }
}
