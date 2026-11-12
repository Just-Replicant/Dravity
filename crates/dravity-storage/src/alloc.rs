//! Preallocation and durability policies for file storage.
//!
//! [`PreallocateMode`] decides what happens when a file is first created.
//! [`Durability`] decides when written data is forced to disk.

use std::fs::File;
use std::io;
use std::str::FromStr;

/// How a file's space is reserved when it is created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PreallocateMode {
    /// Grow files as blocks arrive (the default).
    #[default]
    None,
    /// Set the full length at once without reserving blocks (a sparse
    /// file on filesystems that support it).
    Sparse,
    /// Reserve every block up front (`fallocate`/`posix_fallocate`). A
    /// full disk fails at creation instead of mid-download, and the file
    /// is less fragmented. Falls back to [`Sparse`](Self::Sparse) where
    /// the filesystem cannot reserve space.
    Full,
}

impl PreallocateMode {
    /// The mode's name, as [`FromStr`] reads it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Sparse => "sparse",
            Self::Full => "full",
        }
    }
}

impl FromStr for PreallocateMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "sparse" => Ok(Self::Sparse),
            "full" => Ok(Self::Full),
            _ => Err(format!(
                "unknown preallocation mode `{s}` (none, sparse, full)"
            )),
        }
    }
}

/// When written data is forced to disk (`fsync`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Durability {
    /// Only when correctness needs it: before resume data is saved, when a
    /// file leaves the descriptor cache, and when a torrent stops (the
    /// default).
    #[default]
    Never,
    /// After every verified piece, before the engine hears it passed:
    /// a crash loses no piece the torrent already counted.
    PerPiece,
    /// Once, when the download completes.
    OnComplete,
}

impl Durability {
    /// The mode's name, as [`FromStr`] reads it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::PerPiece => "per-piece",
            Self::OnComplete => "on-complete",
        }
    }
}

impl FromStr for Durability {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "never" => Ok(Self::Never),
            "per-piece" | "piece" => Ok(Self::PerPiece),
            "on-complete" | "complete" => Ok(Self::OnComplete),
            _ => Err(format!(
                "unknown durability `{s}` (never, per-piece, on-complete)"
            )),
        }
    }
}

/// Applies `mode` to `file`, which should be `len` bytes long. A file that
/// is already that long (or longer) is left alone.
pub(crate) fn preallocate(file: &File, len: u64, mode: PreallocateMode) -> io::Result<()> {
    if mode == PreallocateMode::None || len == 0 || file.metadata()?.len() >= len {
        return Ok(());
    }
    if mode == PreallocateMode::Full && reserve(file, len)? {
        return Ok(());
    }
    file.set_len(len)
}

/// Reserves `len` bytes of disk for `file`. `Ok(false)` where the platform
/// or filesystem cannot.
#[cfg(all(
    unix,
    not(any(
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "redox",
        target_os = "nto"
    ))
))]
fn reserve(file: &File, len: u64) -> io::Result<bool> {
    use rustix::io::Errno;
    match rustix::fs::fallocate(file, rustix::fs::FallocateFlags::empty(), 0, len) {
        Ok(()) => Ok(true),
        Err(Errno::OPNOTSUPP | Errno::NOSYS | Errno::INVAL) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(all(
    unix,
    not(any(
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "redox",
        target_os = "nto"
    ))
)))]
fn reserve(_file: &File, _len: u64) -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_and_print() {
        for m in [
            PreallocateMode::None,
            PreallocateMode::Sparse,
            PreallocateMode::Full,
        ] {
            assert_eq!(m.as_str().parse::<PreallocateMode>(), Ok(m));
        }
        for d in [
            Durability::Never,
            Durability::PerPiece,
            Durability::OnComplete,
        ] {
            assert_eq!(d.as_str().parse::<Durability>(), Ok(d));
        }
        assert!("bogus".parse::<PreallocateMode>().is_err());
        assert!("bogus".parse::<Durability>().is_err());
    }

    #[test]
    fn sparse_and_full_set_the_length() {
        let dir = tempfile::tempdir().unwrap();
        for (name, mode) in [
            ("s", PreallocateMode::Sparse),
            ("f", PreallocateMode::Full),
            ("n", PreallocateMode::None),
        ] {
            let path = dir.path().join(name);
            let file = File::options()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            preallocate(&file, 100_000, mode).unwrap();
            let expected = if mode == PreallocateMode::None {
                0
            } else {
                100_000
            };
            assert_eq!(file.metadata().unwrap().len(), expected, "{mode:?}");
        }
    }

    #[test]
    fn longer_files_are_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        std::fs::write(&path, vec![7u8; 500]).unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        preallocate(&file, 100, PreallocateMode::Full).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; 500]);
    }
}
