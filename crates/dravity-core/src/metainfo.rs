//! `.torrent` (metainfo) parsing for BitTorrent v1 (BEP 3) and the v1 part of
//! hybrid torrents.
//!
//! The parser is strict where it matters for safety and integrity (the `info`
//! dictionary, piece hashes, file paths and lengths) and lenient for optional
//! descriptive metadata (`comment`, malformed `announce-list` entries, ...),
//! which real-world torrents frequently get wrong.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

use dravity_bencode::{RawValue, Value};
use serde::Deserialize;

use crate::info_hash::InfoHash;
use crate::layout::{FileLayout, FileSlice, LayoutError, PieceLayout};
use crate::path::{PathError, SafePath, validate_component};

/// Errors produced while parsing metainfo.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MetainfoError {
    /// The input is not valid bencode or does not have the expected shape
    /// (missing `info`, wrong field types, ...).
    #[error("malformed metainfo: {0}")]
    Bencode(#[from] dravity_bencode::Error),
    /// The `info` value is not a dictionary.
    #[error("`info` is not a dictionary")]
    InfoNotDict,
    /// `pieces` is missing from a v1/hybrid torrent.
    #[error("`pieces` is missing")]
    MissingPieces,
    /// The torrent is v2-only (BEP 52), which is not supported yet.
    #[error("v2-only torrents are not supported yet")]
    UnsupportedV2Only,
    /// `pieces` length is not a multiple of 20.
    #[error("`pieces` length {0} is not a multiple of 20")]
    InvalidPiecesLength(usize),
    /// `piece length` is zero or does not fit in 32 bits.
    #[error("invalid piece length {0}")]
    InvalidPieceLength(u64),
    /// The number of piece hashes does not match the content length.
    #[error("expected {expected} piece hashes, found {actual}")]
    PieceCountMismatch {
        /// Pieces implied by total length and piece length.
        expected: u64,
        /// Hashes present in `pieces`.
        actual: usize,
    },
    /// Both `length` and `files` are present.
    #[error("both `length` and `files` are present")]
    AmbiguousMode,
    /// Neither `length` nor `files` is present.
    #[error("neither `length` nor `files` is present")]
    MissingLength,
    /// `files` is an empty list.
    #[error("`files` is empty")]
    NoFiles,
    /// `private` is present and is not `0` or `1`.
    #[error("`private` flag must be 0 or 1, found {0}")]
    InvalidPrivate(i64),
    /// A file name or path component is unsafe.
    #[error("unsafe path {path:?}: {source}")]
    UnsafePath {
        /// The offending path, components joined with `/`.
        path: String,
        /// Why it was rejected.
        #[source]
        source: PathError,
    },
    /// Two files have the same path.
    #[error("duplicate file path {0:?}")]
    DuplicatePath(String),
    /// A file path is also used as a directory by another file.
    #[error("file path {0:?} is also used as a directory")]
    PathConflict(String),
    /// Piece/file geometry is invalid.
    #[error(transparent)]
    Layout(#[from] LayoutError),
}

/// A parsed `.torrent` file.
///
/// ```
/// use dravity_core::Metainfo;
///
/// let torrent = b"d8:announce14:http://t/a/ann4:infod6:lengthi3e4:name5:a.txt\
/// 12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
/// let m = Metainfo::from_bytes(torrent).unwrap();
/// assert_eq!(m.info().name(), "a.txt");
/// assert_eq!(m.info().total_length(), 3);
/// assert_eq!(m.info_hash(), dravity_core::InfoHash::from_info_bytes(m.info_bytes()));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Metainfo {
    info_hash: InfoHash,
    info_bytes: Vec<u8>,
    info: Info,
    announce: Option<String>,
    announce_list: Vec<Vec<String>>,
    comment: Option<String>,
    created_by: Option<String>,
    creation_date: Option<i64>,
}

impl fmt::Debug for Metainfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Metainfo")
            .field("info_hash", &self.info_hash)
            .field("info", &self.info)
            .field("announce", &self.announce)
            .field("announce_list", &self.announce_list)
            .field("comment", &self.comment)
            .field("created_by", &self.created_by)
            .field("creation_date", &self.creation_date)
            .finish_non_exhaustive()
    }
}

impl Metainfo {
    /// Parses and validates a `.torrent` file.
    ///
    /// The info-hash is the SHA-1 of the `info` dictionary's bytes exactly as
    /// they appear in `bytes`.
    ///
    /// # Errors
    /// Returns a [`MetainfoError`] describing the first problem found.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MetainfoError> {
        let raw: RawTorrent<'_> = dravity_bencode::from_bytes(bytes)?;
        let info_bytes = raw.info.as_bytes();
        if info_bytes.first() != Some(&b'd') {
            return Err(MetainfoError::InfoNotDict);
        }
        let info = Info::from_raw(raw.info.deserialize()?)?;
        Ok(Self {
            info_hash: InfoHash::from_info_bytes(info_bytes),
            info_bytes: info_bytes.to_vec(),
            info,
            announce: raw.announce.map(|t| t.0).filter(|s| !s.is_empty()),
            announce_list: raw.announce_list.as_ref().map(tiers).unwrap_or_default(),
            comment: raw.comment.map(|t| t.0),
            created_by: raw.created_by.map(|t| t.0),
            creation_date: raw.creation_date.as_ref().and_then(Value::as_int),
        })
    }

    /// Builds metainfo from a bare `info` dictionary, e.g. one fetched from
    /// peers with BEP 9 for a magnet link. `trackers` become the
    /// `announce-list` tiers (empty tiers and URLs are dropped).
    ///
    /// The info-hash is computed from `info`; callers fetching metadata for a
    /// known hash must compare it.
    ///
    /// ```
    /// use dravity_core::{InfoHash, Metainfo};
    ///
    /// let info = b"d6:lengthi3e4:name5:a.txt12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaae";
    /// let m = Metainfo::from_info_bytes(info, vec![vec!["udp://t:1".into()]]).unwrap();
    /// assert_eq!(m.info_hash(), InfoHash::from_info_bytes(info));
    /// assert_eq!(m.trackers(), [["udp://t:1"]]);
    /// assert!(Metainfo::from_info_bytes(b"i1e", Vec::new()).is_err());
    /// ```
    ///
    /// # Errors
    /// The same validation errors as [`Metainfo::from_bytes`] for `info`;
    /// trailing bytes after the dictionary are rejected.
    pub fn from_info_bytes(info: &[u8], trackers: Vec<Vec<String>>) -> Result<Self, MetainfoError> {
        if info.first() != Some(&b'd') {
            return Err(MetainfoError::InfoNotDict);
        }
        let raw: RawInfo<'_> = dravity_bencode::from_bytes(info)?;
        let parsed = Info::from_raw(raw)?;
        let announce_list: Vec<Vec<String>> = trackers
            .into_iter()
            .map(|tier| {
                tier.into_iter()
                    .filter(|u| !u.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|tier| !tier.is_empty())
            .collect();
        Ok(Self {
            info_hash: InfoHash::from_info_bytes(info),
            info_bytes: info.to_vec(),
            info: parsed,
            announce: None,
            announce_list,
            comment: None,
            created_by: None,
            creation_date: None,
        })
    }

    /// The v1 info-hash.
    #[must_use]
    pub const fn info_hash(&self) -> InfoHash {
        self.info_hash
    }

    /// The exact bencoded `info` dictionary (served to peers via BEP 9).
    #[must_use]
    pub fn info_bytes(&self) -> &[u8] {
        &self.info_bytes
    }

    /// The validated `info` dictionary.
    #[must_use]
    pub const fn info(&self) -> &Info {
        &self.info
    }

    /// The `announce` URL, if any.
    #[must_use]
    pub fn announce(&self) -> Option<&str> {
        self.announce.as_deref()
    }

    /// The BEP 12 `announce-list` tiers (malformed entries dropped).
    #[must_use]
    pub fn announce_list(&self) -> &[Vec<String>] {
        &self.announce_list
    }

    /// Tracker tiers to use: `announce-list` if present, otherwise `announce`.
    #[must_use]
    pub fn trackers(&self) -> Vec<Vec<String>> {
        if self.announce_list.is_empty() {
            self.announce.iter().map(|a| vec![a.clone()]).collect()
        } else {
            self.announce_list.clone()
        }
    }

    /// Free-form comment.
    #[must_use]
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }

    /// Creating program.
    #[must_use]
    pub fn created_by(&self) -> Option<&str> {
        self.created_by.as_deref()
    }

    /// Creation time as seconds since the Unix epoch.
    #[must_use]
    pub const fn creation_date(&self) -> Option<i64> {
        self.creation_date
    }
}

/// One file of a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    path: SafePath,
    length: u64,
    offset: u64,
    padding: bool,
}

impl FileEntry {
    /// Path relative to the torrent root (see [`Info::file_path`]).
    #[must_use]
    pub const fn path(&self) -> &SafePath {
        &self.path
    }

    /// Length in bytes.
    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }

    /// Offset of the file's first byte in the content stream.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Whether this is a BEP 47 padding file (never written to disk).
    #[must_use]
    pub const fn is_padding(&self) -> bool {
        self.padding
    }
}

/// The validated `info` dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    name: String,
    pieces: Vec<[u8; 20]>,
    files: Vec<FileEntry>,
    multi_file: bool,
    private: bool,
    source: Option<String>,
    layout: PieceLayout,
    file_layout: FileLayout,
}

impl Info {
    fn from_raw(raw: RawInfo<'_>) -> Result<Self, MetainfoError> {
        if raw.pieces.is_none() && raw.meta_version == Some(2) {
            return Err(MetainfoError::UnsupportedV2Only);
        }
        let pieces_raw = raw.pieces.ok_or(MetainfoError::MissingPieces)?;
        if pieces_raw.len() % 20 != 0 {
            return Err(MetainfoError::InvalidPiecesLength(pieces_raw.len()));
        }
        let piece_length = u32::try_from(raw.piece_length)
            .ok()
            .filter(|&l| l > 0)
            .ok_or(MetainfoError::InvalidPieceLength(raw.piece_length))?;

        let name = raw.name_utf8.unwrap_or(raw.name).0;
        validate_component(&name).map_err(|source| MetainfoError::UnsafePath {
            path: name.clone(),
            source,
        })?;

        let (multi_file, specs) = match (raw.length, raw.files) {
            (Some(_), Some(_)) => return Err(MetainfoError::AmbiguousMode),
            (None, None) => return Err(MetainfoError::MissingLength),
            (Some(length), None) => (false, vec![(vec![name.clone()], length, false)]),
            (None, Some(files)) => {
                if files.is_empty() {
                    return Err(MetainfoError::NoFiles);
                }
                let specs = files
                    .into_iter()
                    .map(|f| {
                        let path = f
                            .path_utf8
                            .unwrap_or(f.path)
                            .into_iter()
                            .map(|t| t.0)
                            .collect();
                        let padding = f.attr.is_some_and(|a| a.0.contains('p'));
                        (path, f.length, padding)
                    })
                    .collect();
                (true, specs)
            }
        };

        let files = build_files(specs)?;
        let file_layout = FileLayout::new(files.iter().map(FileEntry::length))?;
        let layout = PieceLayout::new(piece_length, file_layout.total_length())?;
        let actual = pieces_raw.len() / 20;
        if u64::from(layout.piece_count()) != actual as u64 {
            return Err(MetainfoError::PieceCountMismatch {
                expected: u64::from(layout.piece_count()),
                actual,
            });
        }
        let pieces = pieces_raw
            .as_chunks::<20>()
            .0
            .iter()
            .map(|c| {
                let mut h = [0u8; 20];
                h.copy_from_slice(c);
                h
            })
            .collect();

        Ok(Self {
            name,
            pieces,
            files,
            multi_file,
            private: match raw.private {
                None | Some(0) => false,
                Some(1) => true,
                Some(other) => return Err(MetainfoError::InvalidPrivate(other)),
            },
            source: raw.source.map(|t| t.0),
            layout,
            file_layout,
        })
    }

    /// Suggested name: the file name (single-file) or root directory (multi-file).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the torrent uses the multi-file layout.
    #[must_use]
    pub const fn is_multi_file(&self) -> bool {
        self.multi_file
    }

    /// Whether the `private` flag (BEP 27) is set.
    #[must_use]
    pub const fn is_private(&self) -> bool {
        self.private
    }

    /// The `source` tag used by some private trackers.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// Piece geometry.
    #[must_use]
    pub const fn layout(&self) -> &PieceLayout {
        &self.layout
    }

    /// Nominal piece length.
    #[must_use]
    pub const fn piece_length(&self) -> u32 {
        self.layout.piece_length()
    }

    /// Number of pieces.
    #[must_use]
    pub const fn piece_count(&self) -> u32 {
        self.layout.piece_count()
    }

    /// Total content length.
    #[must_use]
    pub const fn total_length(&self) -> u64 {
        self.layout.total_length()
    }

    /// SHA-1 of piece `index`.
    #[must_use]
    pub fn piece_hash(&self, index: u32) -> Option<&[u8; 20]> {
        self.pieces.get(usize::try_from(index).ok()?)
    }

    /// All piece hashes.
    #[must_use]
    pub fn piece_hashes(&self) -> &[[u8; 20]] {
        &self.pieces
    }

    /// Files in content-stream order. A single-file torrent has one entry whose
    /// path is `[name]`; multi-file paths are relative to the `name` directory.
    #[must_use]
    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    /// Relative on-disk path of file `index`, including the root directory for
    /// multi-file torrents. Always safe to join below a download directory.
    #[must_use]
    pub fn file_path(&self, index: usize) -> Option<PathBuf> {
        let f = self.files.get(index)?;
        let mut p = PathBuf::new();
        if self.multi_file {
            p.push(&self.name);
        }
        p.push(f.path.to_path_buf());
        Some(p)
    }

    /// The file slices that piece `index` covers.
    pub fn piece_file_slices(&self, index: u32) -> impl Iterator<Item = FileSlice> + '_ {
        let range = self.layout.piece_range(index).unwrap_or(0..0);
        self.file_layout.slices(range)
    }
}

fn build_files(specs: Vec<(Vec<String>, u64, bool)>) -> Result<Vec<FileEntry>, MetainfoError> {
    let mut seen: HashSet<Vec<String>> = HashSet::with_capacity(specs.len());
    let mut dirs: HashSet<Vec<String>> = HashSet::new();
    let mut files = Vec::with_capacity(specs.len());
    let mut offset: u64 = 0;
    for (components, length, padding) in specs {
        let joined = components.join("/");
        let path = SafePath::new(components).map_err(|source| MetainfoError::UnsafePath {
            path: joined.clone(),
            source,
        })?;
        let comps = path.components();
        for i in 1..comps.len() {
            dirs.insert(comps.get(..i).unwrap_or_default().to_vec());
        }
        if !seen.insert(comps.to_vec()) {
            return Err(MetainfoError::DuplicatePath(joined));
        }
        files.push(FileEntry {
            path,
            length,
            offset,
            padding,
        });
        offset = offset
            .checked_add(length)
            .ok_or(LayoutError::LengthOverflow)?;
    }
    if let Some(f) = files.iter().find(|f| dirs.contains(f.path.components())) {
        return Err(MetainfoError::PathConflict(f.path.to_string()));
    }
    Ok(files)
}

/// Leniently interprets `announce-list`: keeps tiers that are lists, and the
/// byte-string URLs inside them; drops everything else and empty tiers.
fn tiers(v: &Value<'_>) -> Vec<Vec<String>> {
    v.as_list()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_list)
        .map(|tier| {
            tier.iter()
                .filter_map(Value::as_bytes)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|t| !t.is_empty())
        .collect()
}

/// A byte string decoded as UTF-8, replacing invalid sequences.
struct Text(String);

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Text;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Text, E> {
                Ok(Text(String::from_utf8_lossy(v).into_owned()))
            }
            fn visit_str<E>(self, v: &str) -> Result<Text, E> {
                Ok(Text(v.to_owned()))
            }
        }
        d.deserialize_bytes(V)
    }
}

#[derive(Deserialize)]
struct RawTorrent<'a> {
    #[serde(borrow)]
    info: RawValue<'a>,
    announce: Option<Text>,
    #[serde(rename = "announce-list", borrow)]
    announce_list: Option<Value<'a>>,
    comment: Option<Text>,
    #[serde(rename = "created by")]
    created_by: Option<Text>,
    #[serde(rename = "creation date", borrow)]
    creation_date: Option<Value<'a>>,
}

#[derive(Deserialize)]
struct RawInfo<'a> {
    name: Text,
    #[serde(rename = "name.utf-8")]
    name_utf8: Option<Text>,
    #[serde(rename = "piece length")]
    piece_length: u64,
    #[serde(borrow)]
    pieces: Option<&'a [u8]>,
    length: Option<u64>,
    files: Option<Vec<RawFile>>,
    private: Option<i64>,
    source: Option<Text>,
    #[serde(rename = "meta version")]
    meta_version: Option<i64>,
}

#[derive(Deserialize)]
struct RawFile {
    length: u64,
    path: Vec<Text>,
    #[serde(rename = "path.utf-8")]
    path_utf8: Option<Vec<Text>>,
    attr: Option<Text>,
}
