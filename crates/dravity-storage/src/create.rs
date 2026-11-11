//! Creating a `.torrent` from files on disk.
//!
//! [`create_torrent`] walks a file or directory and feeds it through
//! [`dravity_core::create::TorrentCreator`].

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use dravity_core::create::{CreateError, CreateOptions, InputFile, TorrentCreator};

/// Why [`create_torrent`] failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CreateTorrentError {
    /// Reading the files failed.
    #[error("{path}: {source}")]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// A file name is not UTF-8 (torrent paths are text).
    #[error("{0}: the name is not valid UTF-8")]
    NonUtf8(PathBuf),
    /// A file changed size while it was hashed.
    #[error("{0}: the file changed while it was read")]
    Changed(PathBuf),
    /// The torrent is not valid.
    #[error(transparent)]
    Create(#[from] CreateError),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> CreateTorrentError + '_ {
    move |source| CreateTorrentError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Creates a torrent of `path`: a single-file torrent for a file, a
/// multi-file torrent named after the directory otherwise. Symbolic links
/// are skipped (they could point outside the tree). `progress` is called
/// with the bytes hashed so far.
///
/// # Errors
/// I/O errors, non-UTF-8 names, files that change while read, and
/// [`CreateError`]s (an empty tree, a bad piece length, …).
pub fn create_torrent(
    path: &Path,
    options: CreateOptions,
    mut progress: impl FnMut(u64),
) -> Result<Vec<u8>, CreateTorrentError> {
    let meta = std::fs::metadata(path).map_err(io_err(path))?;
    let name = path
        .file_name()
        .ok_or_else(|| CreateTorrentError::NonUtf8(path.to_owned()))?
        .to_str()
        .ok_or_else(|| CreateTorrentError::NonUtf8(path.to_owned()))?
        .to_owned();
    let single = meta.is_file();
    let files = if single {
        vec![InputFile::new([name.clone()], meta.len())]
    } else {
        let mut files = Vec::new();
        walk(path, &mut Vec::new(), &mut files)?;
        files
    };
    let mut creator = TorrentCreator::new(name, files, single, options)?;
    let order: Vec<InputFile> = creator.files().to_vec();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    for f in order {
        let full = if single {
            path.to_owned()
        } else {
            f.path.iter().fold(path.to_owned(), |p, c| p.join(c))
        };
        let mut file = File::open(&full).map_err(io_err(&full))?;
        let mut left = f.length;
        while left > 0 {
            let want = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
            let slice = buf.get_mut(..want).unwrap_or_default();
            let n = file.read(slice).map_err(io_err(&full))?;
            if n == 0 {
                return Err(CreateTorrentError::Changed(full));
            }
            creator.update(slice.get(..n).unwrap_or_default())?;
            left -= n as u64;
            done += n as u64;
            progress(done);
        }
        if file.read(&mut [0u8; 1]).map_err(io_err(&full))? != 0 {
            return Err(CreateTorrentError::Changed(full));
        }
    }
    Ok(creator.finish()?)
}

fn walk(
    dir: &Path,
    prefix: &mut Vec<String>,
    out: &mut Vec<InputFile>,
) -> Result<(), CreateTorrentError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(io_err(dir))?
        .collect::<Result<_, _>>()
        .map_err(io_err(dir))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type().map_err(io_err(&path))?;
        if kind.is_symlink() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| CreateTorrentError::NonUtf8(path.clone()))?;
        prefix.push(name);
        if kind.is_dir() {
            walk(&path, prefix, out)?;
        } else if kind.is_file() {
            let len = entry.metadata().map_err(io_err(&path))?.len();
            out.push(InputFile::new(prefix.iter().cloned(), len));
        }
        prefix.pop();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use dravity_core::Metainfo;
    use dravity_core::create::CreateVersion;

    use super::*;
    use crate::{FileStorage, verify_piece};

    #[test]
    fn a_directory_becomes_a_verifiable_torrent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("payload");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.bin"), vec![1u8; 40_000]).unwrap();
        std::fs::write(root.join("sub/b.bin"), vec![2u8; 70_000]).unwrap();
        std::fs::write(root.join("sub/empty"), b"").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap();
        for version in [CreateVersion::V1, CreateVersion::V2, CreateVersion::Hybrid] {
            let mut seen = 0;
            let bytes = create_torrent(&root, CreateOptions::new(version), |n| seen = n).unwrap();
            assert_eq!(seen, 110_000);
            let m = Metainfo::from_bytes(&bytes).unwrap();
            assert_eq!(m.info().name(), "payload");
            let paths: Vec<String> = m
                .info()
                .files()
                .iter()
                .filter(|f| !f.is_padding())
                .map(|f| f.path().components().join("/"))
                .collect();
            assert_eq!(paths, ["a.bin", "sub/b.bin", "sub/empty"], "{version:?}");
            let storage = FileStorage::new(dir.path(), m.info()).unwrap();
            for piece in 0..m.info().piece_count() {
                assert!(verify_piece(&storage, m.info(), piece).unwrap(), "{piece}");
            }
        }
    }

    #[test]
    fn a_single_file_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("one.iso");
        std::fs::write(&file, vec![9u8; 1000]).unwrap();
        let bytes = create_torrent(&file, CreateOptions::default(), |_| {}).unwrap();
        let m = Metainfo::from_bytes(&bytes).unwrap();
        assert!(!m.info().is_multi_file());
        assert_eq!(m.info().total_length(), 1000);
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(matches!(
            create_torrent(&empty, CreateOptions::default(), |_| {}),
            Err(CreateTorrentError::Create(CreateError::Empty))
        ));
        assert!(matches!(
            create_torrent(&dir.path().join("nope"), CreateOptions::default(), |_| {}),
            Err(CreateTorrentError::Io { .. })
        ));
    }
}
