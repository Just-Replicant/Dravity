//! `.torrent` (metainfo) parsing for BitTorrent v1 (BEP 3), v2 (BEP 52) and
//! hybrid torrents.
//!
//! A v2 torrent describes its files in a `file tree` and verifies them with
//! per-file SHA-256 Merkle trees ([`crate::merkle`]). Dravity maps every
//! torrent onto one content stream: a v2-only torrent gets the same
//! piece-aligned layout a hybrid torrent spells out with BEP 47 padding
//! files, so piece arithmetic, storage and the engine do not care which
//! version they handle. [`Info::check_piece`] verifies a piece with every
//! hash the torrent has.
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

use sha1::{Digest, Sha1};

use crate::info_hash::{InfoHash, InfoHashV2};
use crate::layout::{FileLayout, FileSlice, LayoutError, PieceLayout};
use crate::merkle::{self, Hash};
use crate::path::{PathError, SafePath, validate_component};
use crate::v2::{self, MetaVersion, TreeFile, V2File};

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
    /// Neither v1 `pieces` nor a v2 `file tree` (with `meta version` 2) is
    /// present.
    #[error("`pieces` is missing")]
    MissingPieces,
    /// `meta version` is 2 but there is no `file tree`.
    #[error("`meta version` is 2 but `file tree` is missing")]
    MissingFileTree,
    /// A `file tree` node is malformed (not a dictionary, a bad `length`, a
    /// missing or malformed `pieces root`).
    #[error("malformed file tree entry {0:?}")]
    InvalidFileTree(String),
    /// The v1 file list and the v2 file tree of a hybrid torrent disagree,
    /// or a file is not piece-aligned.
    #[error("hybrid torrent's v1 and v2 parts disagree: {0}")]
    HybridMismatch(String),
    /// A file larger than one piece has no entry in `piece layers`.
    #[error("missing piece layer for pieces root {0}")]
    MissingPieceLayer(String),
    /// A piece layer does not hash to its pieces root, or has the wrong
    /// number of hashes.
    #[error("piece layer for pieces root {0} does not match")]
    InvalidPieceLayer(String),
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
    /// A file name or path component (`name`, `path`, or a `file tree`
    /// key) is not valid UTF-8 and has no valid `.utf-8` alternative. The
    /// payload shows it with invalid bytes replaced by U+FFFD; Dravity does
    /// not guess at the encoding, because two different names could then
    /// collapse into one.
    #[error("file name {0:?} is not valid UTF-8")]
    NonUtf8Name(String),
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
    info_hash_v1: Option<InfoHash>,
    info_hash_v2: Option<InfoHashV2>,
    info_bytes: Vec<u8>,
    web_seeds: Vec<String>,
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
            .field("info_hash_v2", &self.info_hash_v2)
            .field("web_seeds", &self.web_seeds)
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
    /// Info-hashes are computed over the `info` dictionary's bytes exactly as
    /// they appear in `bytes`. For v2 and hybrid torrents every file larger
    /// than one piece must have a valid entry in `piece layers`.
    ///
    /// # Errors
    /// Returns a [`MetainfoError`] describing the first problem found.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MetainfoError> {
        let raw: RawTorrent<'_> = dravity_bencode::from_bytes(bytes)?;
        let info_bytes = raw.info.as_bytes();
        if info_bytes.first() != Some(&b'd') {
            return Err(MetainfoError::InfoNotDict);
        }
        let mut info = Info::from_raw(raw.info.deserialize()?)?;
        let layers = raw.piece_layers.as_ref().and_then(Value::as_dict);
        for root in info.missing_piece_layers() {
            let hex = crate::encoding::hex_encode(&root);
            let bytes = layers
                .and_then(|d| d.get(&root[..]))
                .ok_or_else(|| MetainfoError::MissingPieceLayer(hex.clone()))?
                .as_bytes()
                .ok_or_else(|| MetainfoError::InvalidPieceLayer(hex.clone()))?;
            let layer = v2::split_layer(bytes).ok_or(MetainfoError::InvalidPieceLayer(hex))?;
            info.set_piece_layer(&root, layer)?;
        }
        let (info_hash, info_hash_v1, info_hash_v2) = hashes(info_bytes, info.version());
        Ok(Self {
            info_hash,
            info_hash_v1,
            info_hash_v2,
            info_bytes: info_bytes.to_vec(),
            web_seeds: raw.url_list.as_ref().map(url_list).unwrap_or_default(),
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
    /// The info-hashes are computed from `info`; callers fetching metadata
    /// for a known hash must compare it. A v2 torrent built this way has no
    /// piece layers yet ([`Info::missing_piece_layers`]); add them with
    /// [`Metainfo::set_piece_layer`] before downloading files larger than
    /// one piece.
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
        let (info_hash, info_hash_v1, info_hash_v2) = hashes(info, parsed.version());
        Ok(Self {
            info_hash,
            info_hash_v1,
            info_hash_v2,
            info_bytes: info.to_vec(),
            web_seeds: Vec::new(),
            info: parsed,
            announce: None,
            announce_list,
            comment: None,
            created_by: None,
            creation_date: None,
        })
    }

    /// The 20-byte swarm id used in handshakes, with trackers and in the
    /// DHT: the v1 info-hash, or for a v2-only torrent the truncated v2
    /// info-hash (BEP 52).
    #[must_use]
    pub const fn info_hash(&self) -> InfoHash {
        self.info_hash
    }

    /// The v1 info-hash (SHA-1 of `info`), for v1 and hybrid torrents.
    #[must_use]
    pub const fn info_hash_v1(&self) -> Option<InfoHash> {
        self.info_hash_v1
    }

    /// The v2 info-hash (SHA-256 of `info`), for v2 and hybrid torrents.
    #[must_use]
    pub const fn info_hash_v2(&self) -> Option<InfoHashV2> {
        self.info_hash_v2
    }

    /// Every 20-byte swarm id the torrent is known by: [`Self::info_hash`]
    /// first, then the truncated v2 hash of a hybrid torrent.
    #[must_use]
    pub fn swarm_hashes(&self) -> Vec<InfoHash> {
        let mut out = vec![self.info_hash];
        if let Some(v2) = self.info_hash_v2.map(|h| h.truncated())
            && v2 != self.info_hash
        {
            out.push(v2);
        }
        out
    }

    /// Web seed URLs (BEP 19 `url-list`), in order, duplicates removed.
    #[must_use]
    pub fn web_seeds(&self) -> &[String] {
        &self.web_seeds
    }

    /// Adds web seeds (e.g. a magnet link's `ws` parameters), keeping the
    /// order and dropping duplicates and empty URLs.
    #[must_use]
    pub fn with_web_seeds(mut self, seeds: impl IntoIterator<Item = String>) -> Self {
        for seed in seeds {
            if !seed.is_empty() && !self.web_seeds.contains(&seed) {
                self.web_seeds.push(seed);
            }
        }
        self
    }

    /// Supplies the piece layer of the file(s) whose pieces root is `root`,
    /// e.g. hashes received from peers for a v2 magnet link.
    ///
    /// # Errors
    /// [`MetainfoError::InvalidPieceLayer`] if no file needs this layer, or
    /// it does not hash to `root`.
    pub fn set_piece_layer(&mut self, root: &Hash, layer: Vec<Hash>) -> Result<(), MetainfoError> {
        self.info.set_piece_layer(root, layer)
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
    version: MetaVersion,
    /// v2 hashes of the non-empty files, in stream order.
    v2: Vec<V2File>,
}

impl Info {
    fn from_raw(raw: RawInfo<'_>) -> Result<Self, MetainfoError> {
        let piece_length = u32::try_from(raw.piece_length)
            .ok()
            .filter(|&l| l > 0)
            .ok_or(MetainfoError::InvalidPieceLength(raw.piece_length))?;

        let name = raw
            .name_utf8
            .filter(|t| t.1)
            .unwrap_or(raw.name)
            .into_name()?;
        validate_component(&name).map_err(|source| MetainfoError::UnsafePath {
            path: name.clone(),
            source,
        })?;
        let private = match raw.private {
            None | Some(0) => false,
            Some(1) => true,
            Some(other) => return Err(MetainfoError::InvalidPrivate(other)),
        };
        let source = raw.source.map(|t| t.0);

        // The file tree only counts with `meta version` 2 (BEP 52); a v1
        // torrent carrying stray keys stays v1.
        let tree = match raw.meta_version {
            Some(2) => Some(v2::parse_tree(
                raw.file_tree
                    .as_ref()
                    .ok_or(MetainfoError::MissingFileTree)?,
            )?),
            _ => None,
        };
        let Some(pieces_raw) = raw.pieces else {
            let tree = tree.ok_or(MetainfoError::MissingPieces)?;
            return Self::v2_only(name, piece_length, &tree, private, source);
        };
        if pieces_raw.len() % 20 != 0 {
            return Err(MetainfoError::InvalidPiecesLength(pieces_raw.len()));
        }

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
                            .filter(|p| p.iter().all(|t| t.1))
                            .unwrap_or(f.path)
                            .into_iter()
                            .map(Text::into_name)
                            .collect::<Result<Vec<_>, _>>()?;
                        let padding = f.attr.is_some_and(|a| a.0.contains('p'));
                        Ok((path, f.length, padding))
                    })
                    .collect::<Result<Vec<_>, MetainfoError>>()?;
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

        let mut info = Self {
            name,
            pieces,
            files,
            multi_file,
            private,
            source,
            layout,
            file_layout,
            version: MetaVersion::V1,
            v2: Vec::new(),
        };
        if let Some(tree) = tree {
            info.v2 = info.place_v2(&tree)?;
            info.version = MetaVersion::Hybrid;
        }
        Ok(info)
    }

    /// A v2-only torrent: the files of the tree, each followed by padding up
    /// to the next piece boundary (except the last file with data), exactly
    /// the stream a hybrid torrent would describe.
    fn v2_only(
        name: String,
        piece_length: u32,
        tree: &[TreeFile],
        private: bool,
        source: Option<String>,
    ) -> Result<Self, MetainfoError> {
        if merkle::leaves_per_piece(piece_length).is_none() {
            return Err(MetainfoError::InvalidPieceLength(piece_length.into()));
        }
        let multi_file = !matches!(tree, [only] if only.path == [name.as_str()]);
        let last_data = tree.iter().rposition(|f| f.length > 0);
        let piece = u64::from(piece_length);
        let mut specs = Vec::with_capacity(tree.len() * 2);
        for (i, f) in tree.iter().enumerate() {
            specs.push((f.path.clone(), f.length, false));
            let rem = f.length % piece;
            if rem != 0 && Some(i) != last_data {
                let pad = piece - rem;
                specs.push((vec![".pad".to_owned(), pad.to_string()], pad, true));
            }
        }
        let files = build_files(specs)?;
        let file_layout = FileLayout::new(files.iter().map(FileEntry::length))?;
        let layout = PieceLayout::new(piece_length, file_layout.total_length())?;
        let mut info = Self {
            name,
            pieces: Vec::new(),
            files,
            multi_file,
            private,
            source,
            layout,
            file_layout,
            version: MetaVersion::V2,
            v2: Vec::new(),
        };
        info.v2 = info.place_v2(tree)?;
        Ok(info)
    }

    /// Pairs the tree's non-empty files with this info's non-empty real
    /// files, which must be the same paths and lengths in the same order,
    /// each starting on a piece boundary.
    fn place_v2(&self, tree: &[TreeFile]) -> Result<Vec<V2File>, MetainfoError> {
        let piece_length = self.piece_length();
        if merkle::leaves_per_piece(piece_length).is_none() {
            return Err(MetainfoError::InvalidPieceLength(piece_length.into()));
        }
        let piece = u64::from(piece_length);
        let mismatch = |what: String| MetainfoError::HybridMismatch(what);
        let mut ours = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.padding && f.length > 0);
        let mut out = Vec::new();
        for t in tree.iter().filter(|t| t.length > 0) {
            let joined = t.path.join("/");
            let (index, file) = ours
                .next()
                .ok_or_else(|| mismatch(format!("{joined:?} is only in the file tree")))?;
            if file.path.components() != t.path.as_slice() || file.length != t.length {
                return Err(mismatch(format!(
                    "file tree has {joined:?} where the file list has {:?}",
                    file.path.to_string()
                )));
            }
            if file.offset % piece != 0 {
                return Err(mismatch(format!("{joined:?} does not start a piece")));
            }
            let root = t.root.ok_or(MetainfoError::InvalidFileTree(joined))?;
            let first_piece =
                u32::try_from(file.offset / piece).map_err(|_| LayoutError::TooManyPieces)?;
            let pieces = u32::try_from(file.length.div_ceil(piece))
                .map_err(|_| LayoutError::TooManyPieces)?;
            out.push(V2File {
                file: index,
                offset: file.offset,
                length: file.length,
                root,
                first_piece,
                pieces,
                layer: (pieces <= 1).then(Vec::new),
            });
        }
        if let Some((_, extra)) = ours.next() {
            return Err(mismatch(format!(
                "{:?} is only in the file list",
                extra.path.to_string()
            )));
        }
        Ok(out)
    }

    /// Which hashes the torrent carries.
    #[must_use]
    pub const fn version(&self) -> MetaVersion {
        self.version
    }

    /// The v2 pieces root of file `index` (`None` for v1 torrents, padding
    /// and empty files).
    #[must_use]
    pub fn pieces_root(&self, index: usize) -> Option<&Hash> {
        self.v2.iter().find(|f| f.file == index).map(|f| &f.root)
    }

    /// Pieces roots of files whose piece layer is needed but not known yet
    /// (deduplicated, in stream order). Always empty for a v1 torrent or
    /// one parsed from a `.torrent` file.
    #[must_use]
    pub fn missing_piece_layers(&self) -> Vec<Hash> {
        let mut out: Vec<Hash> = Vec::new();
        for f in self.v2.iter().filter(|f| f.layer.is_none()) {
            if !out.contains(&f.root) {
                out.push(f.root);
            }
        }
        out
    }

    /// The known piece layers by pieces root (deduplicated, in stream
    /// order), for writing a `.torrent` file's `piece layers`.
    #[must_use]
    pub fn piece_layers(&self) -> Vec<(Hash, &[Hash])> {
        let mut out: Vec<(Hash, &[Hash])> = Vec::new();
        for f in &self.v2 {
            if let Some(layer) = f.layer.as_deref().filter(|l| !l.is_empty())
                && !out.iter().any(|(r, _)| *r == f.root)
            {
                out.push((f.root, layer));
            }
        }
        out
    }

    /// The known piece layer of the file(s) with pieces root `root`.
    #[must_use]
    pub fn piece_layer(&self, root: &Hash) -> Option<&[Hash]> {
        self.v2
            .iter()
            .find(|f| f.root == *root)
            .and_then(|f| f.layer.as_deref())
            .filter(|l| !l.is_empty())
    }

    /// Number of pieces in the piece layer of the file(s) with pieces root
    /// `root` (`None` if no file needs a layer for it).
    #[must_use]
    pub fn piece_layer_len(&self, root: &Hash) -> Option<u32> {
        self.v2
            .iter()
            .find(|f| f.root == *root && f.needs_layer())
            .map(|f| f.pieces)
    }

    fn set_piece_layer(&mut self, root: &Hash, layer: Vec<Hash>) -> Result<(), MetainfoError> {
        let piece_length = self.piece_length();
        let invalid = || MetainfoError::InvalidPieceLayer(crate::encoding::hex_encode(root));
        let mut matched = false;
        for f in self
            .v2
            .iter_mut()
            .filter(|f| f.root == *root && f.needs_layer())
        {
            if !f.accepts_layer(&layer, piece_length) {
                return Err(invalid());
            }
            f.layer = Some(layer.clone());
            matched = true;
        }
        if matched { Ok(()) } else { Err(invalid()) }
    }

    /// The v2 file that piece `index` belongs to.
    fn v2_file(&self, index: u32) -> Option<&V2File> {
        let at = self.v2.partition_point(|f| f.first_piece <= index);
        let f = self.v2.get(at.checked_sub(1)?)?;
        (index - f.first_piece < f.pieces).then_some(f)
    }

    /// Verifies `data`, the whole of piece `index` as stored in the content
    /// stream (padding included), with every hash the torrent has: SHA-1
    /// for v1, the Merkle piece hash (or the file's root) for v2, both for
    /// hybrid torrents. A v2 piece whose layer is not known yet fails, unless
    /// a hybrid's SHA-1 vouches for it.
    #[must_use]
    pub fn check_piece(&self, index: u32, data: &[u8]) -> bool {
        let Some(range) = self.layout.piece_range(index) else {
            return false;
        };
        if u64::try_from(data.len()).ok() != Some(range.end - range.start) {
            return false;
        }
        if self.version.has_v1() {
            let digest: [u8; 20] = Sha1::digest(data).into();
            if self.piece_hash(index) != Some(&digest) {
                return false;
            }
        }
        if !self.version.has_v2() {
            return true;
        }
        let Some(file) = self.v2_file(index) else {
            // Only a hybrid can have a piece outside every v2 file (a run
            // of padding), and its SHA-1 already matched.
            return self.version == MetaVersion::Hybrid;
        };
        if file.layer.is_none() && self.version == MetaVersion::Hybrid {
            return true;
        }
        let end = range.end.min(file.offset + file.length);
        let len = usize::try_from(end.saturating_sub(range.start)).unwrap_or(usize::MAX);
        data.get(..len)
            .is_some_and(|d| file.check(index, d, self.piece_length()))
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

    /// SHA-1 of piece `index` (`None` for a v2-only torrent).
    #[must_use]
    pub fn piece_hash(&self, index: u32) -> Option<&[u8; 20]> {
        self.pieces.get(usize::try_from(index).ok()?)
    }

    /// All v1 piece hashes (empty for a v2-only torrent).
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
        // BEP 47 padding files are never written, and creators reuse their
        // names (`.pad/16384` again and again), so only real files must be
        // unique.
        if !padding {
            for i in 1..comps.len() {
                dirs.insert(comps.get(..i).unwrap_or_default().to_vec());
            }
            if !seen.insert(comps.to_vec()) {
                return Err(MetainfoError::DuplicatePath(joined));
            }
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
    if let Some(f) = files
        .iter()
        .find(|f| !f.padding && dirs.contains(f.path.components()))
    {
        return Err(MetainfoError::PathConflict(f.path.to_string()));
    }
    Ok(files)
}

/// The swarm id, v1 hash and v2 hash for `info` bytes of `version`.
fn hashes(info: &[u8], version: MetaVersion) -> (InfoHash, Option<InfoHash>, Option<InfoHashV2>) {
    let v1 = version.has_v1().then(|| InfoHash::from_info_bytes(info));
    let v2 = version.has_v2().then(|| InfoHashV2::from_info_bytes(info));
    let swarm = v1
        .or(v2.map(|h| h.truncated()))
        .unwrap_or_else(|| InfoHash::from_info_bytes(info));
    (swarm, v1, v2)
}

/// Leniently interprets `url-list` (BEP 19): one URL or a list of them;
/// non-strings, empty strings and duplicates are dropped.
fn url_list(v: &Value<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let items: Vec<&Value<'_>> = match v {
        Value::List(items) => items.iter().collect(),
        other => vec![other],
    };
    for url in items
        .into_iter()
        .filter_map(Value::as_bytes)
        .map(|b| String::from_utf8_lossy(b).trim().to_owned())
    {
        if !url.is_empty() && !out.contains(&url) {
            out.push(url);
        }
    }
    out
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

/// A byte string decoded as UTF-8, replacing invalid sequences, and
/// whether it was valid. Fine for free text (comments, URLs); names must be
/// valid ([`Text::into_name`]).
struct Text(String, bool);

impl Text {
    /// The string, if the bytes were valid UTF-8.
    fn into_name(self) -> Result<String, MetainfoError> {
        if self.1 {
            Ok(self.0)
        } else {
            Err(MetainfoError::NonUtf8Name(self.0))
        }
    }
}

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Text;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Text, E> {
                Ok(match std::str::from_utf8(v) {
                    Ok(s) => Text(s.to_owned(), true),
                    Err(_) => Text(String::from_utf8_lossy(v).into_owned(), false),
                })
            }
            fn visit_str<E>(self, v: &str) -> Result<Text, E> {
                Ok(Text(v.to_owned(), true))
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
    #[serde(rename = "piece layers", borrow)]
    piece_layers: Option<Value<'a>>,
    #[serde(rename = "url-list", borrow)]
    url_list: Option<Value<'a>>,
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
    #[serde(rename = "file tree", borrow)]
    file_tree: Option<Value<'a>>,
}

#[derive(Deserialize)]
struct RawFile {
    length: u64,
    path: Vec<Text>,
    #[serde(rename = "path.utf-8")]
    path_utf8: Option<Vec<Text>>,
    attr: Option<Text>,
}
