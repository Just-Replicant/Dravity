//! Opening content files without following symbolic links.
//!
//! Paths below the download directory come from hostile metainfo. They are
//! validated by `dravity_core::path`, but a symlink planted in the download
//! directory (by another local user, or a previous torrent that was later
//! replaced) could still redirect a write outside it.
//!
//! * **Unix:** the download directory is opened once per call and every
//!   component below it is opened relative to its parent's descriptor with
//!   `O_NOFOLLOW` (`openat`/`mkdirat`). Swapping a directory for a link
//!   between the check and the open cannot redirect the write: there is no
//!   separate check.
//! * **Elsewhere (Windows):** existing components are checked with
//!   `symlink_metadata` before opening (not race-free), and the file itself
//!   is opened without following a reparse point and refused if it is one.
//!
//! The download directory itself is trusted and may be a link.

use std::fs::{self, File};
use std::path::{Component, Path};

use crate::StorageError;

/// Opens `root/rel` for reading and writing, creating it (and its parent
/// directories) when `create` is set. Fails with [`StorageError::Symlink`]
/// if any component below `root` is a symbolic link, and with
/// [`StorageError::UnsafePath`] if `rel` is not a plain relative path.
pub(crate) fn open_beneath(root: &Path, rel: &Path, create: bool) -> Result<File, StorageError> {
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            // `SafePath` only yields normal components; anything else is a bug
            // upstream and must not reach the filesystem.
            _ => return Err(StorageError::UnsafePath(rel.to_path_buf())),
        }
    }
    let Some((name, dirs)) = parts.split_last() else {
        return Err(StorageError::UnsafePath(rel.to_path_buf()));
    };
    if create {
        fs::create_dir_all(root).map_err(|e| StorageError::io(root, e))?;
    }
    imp::open(root, dirs, name, create)
}

#[cfg(unix)]
mod imp {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::os::fd::OwnedFd;
    use std::path::Path;

    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;

    use crate::StorageError;

    fn dir_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    /// Opens directory `name` below `parent` without following a link.
    fn open_dir(parent: &OwnedFd, name: &OsStr, create: bool) -> Result<OwnedFd, Errno> {
        match rustix::fs::openat(parent, name, dir_flags(), Mode::empty()) {
            Err(Errno::NOENT) if create => {
                match rustix::fs::mkdirat(parent, name, Mode::from_raw_mode(0o777)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(e),
                }
                rustix::fs::openat(parent, name, dir_flags(), Mode::empty())
            }
            other => other,
        }
    }

    /// Whether `name` below `parent` is a symbolic link (for error reports).
    fn is_link(parent: &OwnedFd, name: &OsStr) -> bool {
        rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|st| rustix::fs::FileType::from_raw_mode(st.st_mode).is_symlink())
    }

    pub(super) fn open(
        root: &Path,
        dirs: &[&OsStr],
        name: &OsStr,
        create: bool,
    ) -> Result<File, StorageError> {
        let mut dir = rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| StorageError::io(root, e.into()))?;
        let mut current = root.to_path_buf();
        for part in dirs {
            current.push(part);
            dir = match open_dir(&dir, part, create) {
                Ok(fd) => fd,
                // A link opened with O_NOFOLLOW | O_DIRECTORY fails with ELOOP
                // on Linux and ENOTDIR on some BSDs.
                Err(Errno::LOOP) => return Err(StorageError::Symlink(current)),
                Err(Errno::NOTDIR) if is_link(&dir, part) => {
                    return Err(StorageError::Symlink(current));
                }
                Err(e) => return Err(StorageError::io(current, e.into())),
            };
        }
        current.push(name);
        let mut flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        if create {
            flags |= OFlags::CREATE;
        }
        match rustix::fs::openat(&dir, name, flags, Mode::from_raw_mode(0o666)) {
            Ok(fd) => Ok(File::from(fd)),
            Err(Errno::LOOP) => Err(StorageError::Symlink(current)),
            // FreeBSD reports a final-component link as EMLINK.
            Err(Errno::MLINK) if is_link(&dir, name) => Err(StorageError::Symlink(current)),
            Err(e) => Err(StorageError::io(current, e.into())),
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::ffi::OsStr;
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::path::Path;

    use crate::StorageError;

    /// Refuses a link at `path`; a missing path is fine.
    fn refuse_link(path: &Path) -> Result<bool, StorageError> {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                Err(StorageError::Symlink(path.to_path_buf()))
            }
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StorageError::io(path, e)),
        }
    }

    pub(super) fn open(
        root: &Path,
        dirs: &[&OsStr],
        name: &OsStr,
        create: bool,
    ) -> Result<File, StorageError> {
        let mut current = root.to_path_buf();
        for part in dirs {
            current.push(part);
            if !refuse_link(&current)? {
                if !create {
                    return Err(StorageError::io(current, io::ErrorKind::NotFound.into()));
                }
                fs::create_dir(&current)
                    .or_else(|e| {
                        if e.kind() == io::ErrorKind::AlreadyExists {
                            Ok(())
                        } else {
                            Err(e)
                        }
                    })
                    .map_err(|e| StorageError::io(&current, e))?;
                refuse_link(&current)?;
            }
        }
        current.push(name);
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(create)
            .truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            /// `FILE_FLAG_OPEN_REPARSE_POINT`: open a link itself, not its target.
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options
            .open(&current)
            .map_err(|e| StorageError::io(&current, e))?;
        let meta = file.metadata().map_err(|e| StorageError::io(&current, e))?;
        if meta.file_type().is_symlink() {
            return Err(StorageError::Symlink(current));
        }
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_nested_files_and_reopens_them() {
        let dir = tempfile::tempdir().unwrap();
        let rel = Path::new("a").join("b").join("c.bin");
        assert!(open_beneath(dir.path(), &rel, false).is_err(), "missing");
        open_beneath(dir.path(), &rel, true).unwrap();
        assert!(dir.path().join(&rel).is_file());
        open_beneath(dir.path(), &rel, false).unwrap();
    }

    #[test]
    fn refuses_non_normal_components() {
        let dir = tempfile::tempdir().unwrap();
        for rel in ["../x", "/abs", "a/../b", ""] {
            assert!(
                matches!(
                    open_beneath(dir.path(), Path::new(rel), true),
                    Err(StorageError::UnsafePath(_))
                ),
                "{rel}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_links_at_every_level() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("target"), b"secret").unwrap();
        // Intermediate directory is a link.
        symlink(outside.path(), dir.path().join("sub")).unwrap();
        let err = open_beneath(dir.path(), Path::new("sub/file"), true).unwrap_err();
        assert!(matches!(err, StorageError::Symlink(_)), "{err}");
        assert!(!outside.path().join("file").exists());
        // Final component is a link (dangling or not).
        symlink(outside.path().join("target"), dir.path().join("leaf")).unwrap();
        let err = open_beneath(dir.path(), Path::new("leaf"), true).unwrap_err();
        assert!(matches!(err, StorageError::Symlink(_)), "{err}");
        symlink(outside.path().join("nothing"), dir.path().join("dangling")).unwrap();
        let err = open_beneath(dir.path(), Path::new("dangling"), true).unwrap_err();
        assert!(matches!(err, StorageError::Symlink(_)), "{err}");
        assert!(!outside.path().join("nothing").exists());
        assert_eq!(fs::read(outside.path().join("target")).unwrap(), b"secret");
        // The download directory itself may be a link.
        let linked_root = dir.path().join("root-link");
        symlink(outside.path(), &linked_root).unwrap();
        open_beneath(&linked_root, Path::new("ok/file"), true).unwrap();
        assert!(outside.path().join("ok/file").is_file());
    }
}
