//! Fastresume: remember which pieces were verified so a restart can skip the
//! full hash check.
//!
//! A [`ResumeData`] records the info-hash, the verified pieces (wire bitfield
//! bytes) and a [`FileStamp`] (length and modification time) for every file.
//! It is only trusted when all stamps still match the files on disk; any
//! change — a file touched, truncated, deleted or created — means the data
//! must be checked again. Write it only after [`Storage::flush`] and only
//! when no further writes are pending (on stop or completion), otherwise the
//! stamps would describe a state that never reached the disk.
//!
//! The file format is a bencoded dictionary:
//!
//! ```text
//! d 5:files l (d 3:leni..e 5:mtimei..e e | de) ... e
//!   4:have <bitfield bytes> 9:info-hash 20:<hash> 6:piecesi..e 7:versioni1e e
//! ```
//!
//! [`Storage::flush`]: crate::Storage::flush

use std::borrow::Cow;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dravity_bencode::{Dict, Value};
use dravity_core::InfoHash;

/// Current format version.
const VERSION: i64 = 1;

/// Upper bound on a resume file; anything bigger is not ours.
pub const MAX_RESUME_FILE: u64 = 16 * 1024 * 1024;

/// Errors loading or saving resume data.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResumeError {
    /// The file is not valid bencode.
    #[error("resume data is not valid bencode: {0}")]
    Decode(#[from] dravity_bencode::Error),
    /// The bencode does not describe resume data.
    #[error("invalid resume data: {0}")]
    Invalid(&'static str),
    /// Reading or writing the file failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

/// What a file looked like when resume data was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileStamp {
    /// The file did not exist (never written, or a BEP 47 padding file).
    Missing,
    /// The file existed.
    Present {
        /// Length in bytes.
        len: u64,
        /// Modification time in nanoseconds since the Unix epoch.
        mtime_ns: u64,
    },
}

/// Verified pieces of one torrent plus the file stamps that vouch for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeData {
    /// Torrent the data belongs to.
    pub info_hash: InfoHash,
    /// Number of pieces in the torrent.
    pub piece_count: u32,
    /// Verified pieces as a wire bitfield (`ceil(piece_count / 8)` bytes,
    /// high bit first, spare bits zero).
    pub have: Vec<u8>,
    /// One stamp per file, in torrent order.
    pub files: Vec<FileStamp>,
}

fn key(k: &str) -> Cow<'static, [u8]> {
    Cow::Owned(k.as_bytes().to_vec())
}

fn int(v: u64) -> Value<'static> {
    Value::Int(i64::try_from(v).unwrap_or(i64::MAX))
}

fn get_u64(d: &Dict<'_>, k: &str) -> Result<u64, ResumeError> {
    d.get(k.as_bytes())
        .and_then(Value::as_int)
        .and_then(|v| u64::try_from(v).ok())
        .ok_or(ResumeError::Invalid("missing or negative integer"))
}

impl ResumeData {
    /// Bencodes the data.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let files = self
            .files
            .iter()
            .map(|stamp| {
                let mut d = Dict::new();
                if let FileStamp::Present { len, mtime_ns } = *stamp {
                    d.insert(key("len"), int(len));
                    d.insert(key("mtime"), int(mtime_ns));
                }
                Value::Dict(d)
            })
            .collect();
        let mut d = Dict::new();
        d.insert(key("version"), Value::Int(VERSION));
        d.insert(
            key("info-hash"),
            Value::Bytes(Cow::Owned(self.info_hash.as_bytes().to_vec())),
        );
        d.insert(key("pieces"), int(u64::from(self.piece_count)));
        d.insert(key("have"), Value::Bytes(Cow::Owned(self.have.clone())));
        d.insert(key("files"), Value::List(files));
        Value::Dict(d).to_vec()
    }

    /// Parses bencoded resume data, validating its shape.
    ///
    /// # Errors
    /// [`ResumeError::Decode`] for malformed bencode, [`ResumeError::Invalid`]
    /// for a wrong version, missing fields, a bitfield of the wrong length or
    /// with spare bits set.
    pub fn decode(input: &[u8]) -> Result<Self, ResumeError> {
        let value = Value::decode(input)?;
        let d = value
            .as_dict()
            .ok_or(ResumeError::Invalid("not a dictionary"))?;
        if d.get(b"version".as_slice()).and_then(Value::as_int) != Some(VERSION) {
            return Err(ResumeError::Invalid("unsupported version"));
        }
        let info_hash: [u8; 20] = d
            .get(b"info-hash".as_slice())
            .and_then(Value::as_bytes)
            .and_then(|b| b.try_into().ok())
            .ok_or(ResumeError::Invalid("bad info-hash"))?;
        let piece_count = u32::try_from(get_u64(d, "pieces")?)
            .map_err(|_| ResumeError::Invalid("too many pieces"))?;
        let have = d
            .get(b"have".as_slice())
            .and_then(Value::as_bytes)
            .ok_or(ResumeError::Invalid("missing bitfield"))?
            .to_vec();
        check_bitfield(&have, piece_count)?;
        let files = d
            .get(b"files".as_slice())
            .and_then(Value::as_list)
            .ok_or(ResumeError::Invalid("missing file list"))?
            .iter()
            .map(|f| {
                let f = f
                    .as_dict()
                    .ok_or(ResumeError::Invalid("file entry is not a dictionary"))?;
                if f.is_empty() {
                    return Ok(FileStamp::Missing);
                }
                Ok(FileStamp::Present {
                    len: get_u64(f, "len")?,
                    mtime_ns: get_u64(f, "mtime")?,
                })
            })
            .collect::<Result<Vec<_>, ResumeError>>()?;
        Ok(Self {
            info_hash: InfoHash::from(info_hash),
            piece_count,
            have,
            files,
        })
    }

    /// Whether this data may be trusted for a torrent with `info_hash` and
    /// `piece_count` pieces whose files currently look like `stamps`.
    #[must_use]
    pub fn is_valid_for(
        &self,
        info_hash: &InfoHash,
        piece_count: u32,
        stamps: &[FileStamp],
    ) -> bool {
        &self.info_hash == info_hash
            && self.piece_count == piece_count
            && self.files.as_slice() == stamps
    }
}

fn check_bitfield(have: &[u8], piece_count: u32) -> Result<(), ResumeError> {
    let expected = piece_count.div_ceil(8);
    if u32::try_from(have.len()).ok() != Some(expected) {
        return Err(ResumeError::Invalid("bitfield has the wrong length"));
    }
    let spare = expected * 8 - piece_count;
    if spare > 0 {
        let mask = (1u8 << spare) - 1;
        if have.last().is_some_and(|last| last & mask != 0) {
            return Err(ResumeError::Invalid("bitfield has spare bits set"));
        }
    }
    Ok(())
}

/// Loads resume data from `path`.
///
/// Returns `Ok(None)` when the file does not exist.
///
/// # Errors
/// I/O errors other than "not found", files larger than
/// [`MAX_RESUME_FILE`], and decoding errors.
pub fn load_resume(path: &Path) -> Result<Option<ResumeData>, ResumeError> {
    let io_err = |source| ResumeError::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(e)),
    };
    if meta.len() > MAX_RESUME_FILE {
        return Err(ResumeError::Invalid("file too large"));
    }
    let bytes = fs::read(path).map_err(io_err)?;
    ResumeData::decode(&bytes).map(Some)
}

/// Writes resume data to `path` atomically: the bytes go to a temporary file
/// in the same directory, which is synced and then renamed over `path`. The
/// directory is created if needed.
///
/// # Errors
/// I/O errors creating, writing, syncing or renaming the file.
pub fn save_resume(path: &Path, data: &ResumeData) -> Result<(), ResumeError> {
    let io_err = |p: &Path| {
        let p = p.to_path_buf();
        move |source| ResumeError::Io { path: p, source }
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let mut file = fs::File::create(&tmp).map_err(io_err(&tmp))?;
        file.write_all(&data.encode()).map_err(io_err(&tmp))?;
        file.sync_all().map_err(io_err(&tmp))?;
        fs::rename(&tmp, path).map_err(io_err(path))
    })();
    if result.is_err() {
        // Best effort: do not leave a half-written temporary behind.
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Removes the resume file at `path`, if any.
///
/// # Errors
/// I/O errors other than "not found".
pub fn remove_resume(path: &Path) -> Result<(), ResumeError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ResumeError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ResumeData {
        ResumeData {
            info_hash: InfoHash::from([7; 20]),
            piece_count: 10,
            have: vec![0b1010_0000, 0b0100_0000],
            files: vec![
                FileStamp::Present {
                    len: 100,
                    mtime_ns: 1_700_000_000_123_456_789,
                },
                FileStamp::Missing,
            ],
        }
    }

    #[test]
    fn roundtrip() {
        let data = sample();
        assert_eq!(ResumeData::decode(&data.encode()).unwrap(), data);
    }

    #[test]
    fn rejects_bad_bitfields() {
        let mut data = sample();
        data.have = vec![0xff, 0xff];
        assert!(matches!(
            ResumeData::decode(&data.encode()),
            Err(ResumeError::Invalid(_))
        ));
        data.have = vec![0];
        assert!(ResumeData::decode(&data.encode()).is_err());
    }

    #[test]
    fn validity() {
        let data = sample();
        let hash = InfoHash::from([7; 20]);
        assert!(data.is_valid_for(&hash, 10, &data.files));
        assert!(!data.is_valid_for(&InfoHash::from([8; 20]), 10, &data.files));
        assert!(!data.is_valid_for(&hash, 11, &data.files));
        assert!(!data.is_valid_for(&hash, 10, &[FileStamp::Missing, FileStamp::Missing]));
    }
}
