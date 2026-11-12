//! Creating torrents: v1, v2 (BEP 52) or hybrid, with BEP 47 padding.
//!
//! [`TorrentCreator`] is sans-IO: it is told the files up front, then fed
//! their bytes in [`TorrentCreator::files`] order, and finally encodes the
//! `.torrent`. Reading files from disk is the caller's job
//! (`dravity_storage::create_torrent` walks a path).
//!
//! ```
//! use dravity_core::create::{CreateOptions, InputFile, TorrentCreator};
//! use dravity_core::Metainfo;
//!
//! let files = vec![InputFile::new(["hello.txt"], 11)];
//! let mut creator =
//!     TorrentCreator::new("hello.txt", files, true, CreateOptions::default()).unwrap();
//! creator.update(b"hello world").unwrap();
//! let bytes = creator.finish().unwrap();
//! let metainfo = Metainfo::from_bytes(&bytes).unwrap();
//! assert_eq!(metainfo.info().name(), "hello.txt");
//! assert!(metainfo.info_hash_v2().is_some()); // hybrid by default
//! ```

use std::borrow::Cow;
use std::collections::BTreeMap;

use dravity_bencode::{Dict, Value};
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::merkle::{self, Hash, LEAF_SIZE};

/// Smallest piece length [`auto_piece_length`] picks (and v2 allows).
pub const MIN_PIECE_LENGTH: u32 = 16 * 1024;

/// Largest piece length [`auto_piece_length`] picks.
pub const MAX_AUTO_PIECE_LENGTH: u32 = 16 * 1024 * 1024;

/// Piece count [`auto_piece_length`] aims to stay under.
const TARGET_PIECES: u64 = 1500;

/// Which hashes a created torrent carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CreateVersion {
    /// SHA-1 pieces only (BEP 3).
    V1,
    /// SHA-256 merkle trees only (BEP 52). Older clients cannot use it.
    V2,
    /// Both, so v1 and v2 clients share one swarm (the default). Files
    /// are padded to piece boundaries (BEP 47), as BEP 52 requires.
    #[default]
    Hybrid,
}

impl CreateVersion {
    /// The version's name, as [`std::str::FromStr`] reads it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
            Self::Hybrid => "hybrid",
        }
    }

    const fn v1(self) -> bool {
        matches!(self, Self::V1 | Self::Hybrid)
    }

    const fn v2(self) -> bool {
        matches!(self, Self::V2 | Self::Hybrid)
    }
}

impl std::str::FromStr for CreateVersion {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "v1" | "1" => Ok(Self::V1),
            "v2" | "2" => Ok(Self::V2),
            "hybrid" => Ok(Self::Hybrid),
            _ => Err(format!("unknown torrent version `{s}` (v1, v2, hybrid)")),
        }
    }
}

/// Everything about a new torrent besides its files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CreateOptions {
    /// v1, v2 or hybrid.
    pub version: CreateVersion,
    /// Piece length; `None` picks one with [`auto_piece_length`]. Must be
    /// a power of two of at least 16 KiB.
    pub piece_length: Option<u32>,
    /// Pad every file but the last to a piece boundary (BEP 47) in a v1
    /// torrent too. Hybrid torrents always pad; v2 needs no padding.
    pub pad_files: bool,
    /// Mark the torrent private (BEP 27): no DHT, PEX or LSD.
    pub private: bool,
    /// Tracker tiers (BEP 12); the first URL also becomes `announce`.
    pub trackers: Vec<Vec<String>>,
    /// Web seed URLs (BEP 19 `url-list`).
    pub web_seeds: Vec<String>,
    /// Free-form comment.
    pub comment: Option<String>,
    /// The creating program.
    pub created_by: Option<String>,
    /// Creation time in seconds since the Unix epoch.
    pub creation_date: Option<i64>,
    /// `source` in the info dictionary, so the same files make a distinct
    /// info hash per private tracker.
    pub source: Option<String>,
}

impl CreateOptions {
    /// Options for `version`, otherwise default.
    #[must_use]
    pub fn new(version: CreateVersion) -> Self {
        Self {
            version,
            ..Self::default()
        }
    }
}

/// One file of a new torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputFile {
    /// Path components below the torrent's name directory (just the file
    /// name for a single-file torrent).
    pub path: Vec<String>,
    /// Length in bytes.
    pub length: u64,
}

impl InputFile {
    /// A file at `path` of `length` bytes.
    pub fn new<S: Into<String>>(path: impl IntoIterator<Item = S>, length: u64) -> Self {
        Self {
            path: path.into_iter().map(Into::into).collect(),
            length,
        }
    }
}

/// Why a torrent could not be created.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CreateError {
    /// The torrent has no name.
    #[error("the torrent name is empty")]
    EmptyName,
    /// No file has any data.
    #[error("the torrent has no data")]
    Empty,
    /// A file path is empty or has a component Dravity would refuse to
    /// download (empty, `.`/`..`, a separator, a control character, …).
    #[error("invalid file path {0:?}")]
    BadPath(Vec<String>),
    /// Two files share a path, or one file's path is another's directory.
    #[error("duplicate or conflicting file path {0:?}")]
    DuplicatePath(Vec<String>),
    /// A single-file torrent was given more or fewer than one file.
    #[error("a single-file torrent needs exactly one file")]
    NotSingleFile,
    /// The piece length is not a power of two of at least 16 KiB.
    #[error("invalid piece length {0} (a power of two, at least 16 KiB)")]
    BadPieceLength(u32),
    /// More bytes were fed than the files hold.
    #[error("more data than the files hold")]
    TooMuchData,
    /// [`TorrentCreator::finish`] was called before all data was fed.
    #[error("{0} bytes of file data are missing")]
    MissingData(u64),
}

/// The piece length for `total` bytes: the smallest power of two from 16
/// KiB to 16 MiB that keeps the piece count at about 1500 or fewer.
#[must_use]
pub fn auto_piece_length(total: u64) -> u32 {
    let mut len = MIN_PIECE_LENGTH;
    while len < MAX_AUTO_PIECE_LENGTH && total.div_ceil(u64::from(len)) > TARGET_PIECES {
        len *= 2;
    }
    len
}

/// One entry of the v1 file list: a real file or BEP 47 padding.
#[derive(Debug, Clone)]
enum Entry {
    File(usize),
    Pad(u64),
}

/// The v2 hashes of a finished file.
#[derive(Debug, Clone)]
struct V2Hashes {
    root: Hash,
    /// Empty for a file of at most one piece.
    layer: Vec<Hash>,
}

/// Streaming state of the file being fed.
#[derive(Debug, Clone, Default)]
struct V2Stream {
    leaves: Vec<Hash>,
    block: Sha256,
    in_block: usize,
}

/// Builds a torrent from its files' bytes; see the [module](self) docs.
#[derive(Debug, Clone)]
pub struct TorrentCreator {
    name: String,
    files: Vec<InputFile>,
    single: bool,
    options: CreateOptions,
    piece_length: u32,
    entries: Vec<Entry>,
    /// Index of the entry being fed and its bytes still to come.
    cursor: usize,
    remaining: u64,
    v1_hasher: Sha1,
    v1_in_piece: u64,
    v1_pieces: Vec<u8>,
    v2_stream: V2Stream,
    v2_done: Vec<Option<V2Hashes>>,
}

impl TorrentCreator {
    /// A creator for `files` under `name`. A `single` torrent has exactly
    /// one file, stored as `name` itself. Multi-file torrents are sorted
    /// by path (BEP 52 orders the file tree that way, and hybrids must
    /// list v1 files in the same order).
    ///
    /// # Errors
    /// An empty name or torrent, a bad or duplicate path, or a bad piece
    /// length.
    pub fn new(
        name: impl Into<String>,
        mut files: Vec<InputFile>,
        single: bool,
        options: CreateOptions,
    ) -> Result<Self, CreateError> {
        let name = name.into();
        if name.is_empty() || !component_ok(&name) {
            return Err(CreateError::EmptyName);
        }
        if single && files.len() != 1 {
            return Err(CreateError::NotSingleFile);
        }
        for f in &files {
            if f.path.is_empty() || !f.path.iter().all(|c| component_ok(c)) {
                return Err(CreateError::BadPath(f.path.clone()));
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        for pair in files.windows(2) {
            if let [a, b] = pair
                && (a.path == b.path || b.path.starts_with(&a.path))
            {
                return Err(CreateError::DuplicatePath(b.path.clone()));
            }
        }
        let total: u64 = files.iter().map(|f| f.length).sum();
        if total == 0 {
            return Err(CreateError::Empty);
        }
        let piece_length = options
            .piece_length
            .unwrap_or_else(|| auto_piece_length(total));
        if piece_length < MIN_PIECE_LENGTH || !piece_length.is_power_of_two() {
            return Err(CreateError::BadPieceLength(piece_length));
        }
        let pad = !single
            && options.version.v1()
            && (options.pad_files || options.version == CreateVersion::Hybrid);
        let piece = u64::from(piece_length);
        let mut entries = Vec::with_capacity(files.len() * 2);
        let last = files.len().saturating_sub(1);
        for (i, f) in files.iter().enumerate() {
            entries.push(Entry::File(i));
            let gap = (piece - f.length % piece) % piece;
            if pad && i != last && gap > 0 {
                entries.push(Entry::Pad(gap));
            }
        }
        let count = files.len();
        let mut creator = Self {
            name,
            files,
            single,
            options,
            piece_length,
            entries,
            cursor: 0,
            remaining: 0,
            v1_hasher: Sha1::new(),
            v1_in_piece: 0,
            v1_pieces: Vec::new(),
            v2_stream: V2Stream::default(),
            v2_done: vec![None; count],
        };
        creator.remaining = creator.entry_len(0);
        creator.settle();
        Ok(creator)
    }

    /// The files in the order their bytes must be fed.
    #[must_use]
    pub fn files(&self) -> &[InputFile] {
        &self.files
    }

    /// The piece length in use.
    #[must_use]
    pub const fn piece_length(&self) -> u32 {
        self.piece_length
    }

    /// Feeds the next bytes of the files, in [`Self::files`] order. Calls
    /// may split the data anywhere.
    ///
    /// # Errors
    /// [`CreateError::TooMuchData`] past the end of the last file.
    pub fn update(&mut self, mut data: &[u8]) -> Result<(), CreateError> {
        while !data.is_empty() {
            if self.cursor >= self.entries.len() {
                return Err(CreateError::TooMuchData);
            }
            let take = usize::try_from(self.remaining)
                .unwrap_or(usize::MAX)
                .min(data.len());
            let (now, rest) = data.split_at(take);
            self.feed_v1(now);
            if self.options.version.v2() {
                self.feed_v2(now);
            }
            self.remaining -= take as u64;
            data = rest;
            self.settle();
        }
        Ok(())
    }

    /// Encodes the finished torrent.
    ///
    /// # Errors
    /// [`CreateError::MissingData`] if not every byte was fed.
    pub fn finish(mut self) -> Result<Vec<u8>, CreateError> {
        if self.cursor < self.entries.len() {
            let missing = self.remaining
                + (self.cursor + 1..self.entries.len())
                    .map(|i| self.entry_len(i))
                    .sum::<u64>();
            return Err(CreateError::MissingData(missing));
        }
        if self.options.version.v1() && self.v1_in_piece > 0 {
            let digest = std::mem::take(&mut self.v1_hasher).finalize();
            self.v1_pieces.extend_from_slice(&digest);
        }
        Ok(self.encode())
    }

    fn entry_len(&self, index: usize) -> u64 {
        match self.entries.get(index) {
            Some(Entry::File(i)) => self.files.get(*i).map_or(0, |f| f.length),
            Some(Entry::Pad(len)) => *len,
            None => 0,
        }
    }

    /// Closes finished entries and feeds padding until the cursor rests
    /// on a file with bytes still to come (or past the end).
    fn settle(&mut self) {
        while self.cursor < self.entries.len() {
            match self.entries.get(self.cursor) {
                Some(Entry::Pad(_)) if self.remaining > 0 => {
                    const ZEROS: [u8; LEAF_SIZE] = [0; LEAF_SIZE];
                    while self.remaining > 0 {
                        let n = usize::try_from(self.remaining)
                            .unwrap_or(LEAF_SIZE)
                            .min(LEAF_SIZE);
                        self.feed_v1(ZEROS.get(..n).unwrap_or(&ZEROS));
                        self.remaining -= n as u64;
                    }
                }
                Some(Entry::File(_)) if self.remaining > 0 => return,
                Some(Entry::File(i)) => {
                    let i = *i;
                    self.finish_v2_file(i);
                }
                _ => {}
            }
            self.cursor += 1;
            self.remaining = self.entry_len(self.cursor);
        }
    }

    fn feed_v1(&mut self, mut data: &[u8]) {
        if !self.options.version.v1() {
            return;
        }
        let piece = u64::from(self.piece_length);
        while !data.is_empty() {
            let room = usize::try_from(piece - self.v1_in_piece).unwrap_or(usize::MAX);
            let (now, rest) = data.split_at(room.min(data.len()));
            self.v1_hasher.update(now);
            self.v1_in_piece += now.len() as u64;
            if self.v1_in_piece == piece {
                let digest = std::mem::take(&mut self.v1_hasher).finalize();
                self.v1_pieces.extend_from_slice(&digest);
                self.v1_in_piece = 0;
            }
            data = rest;
        }
    }

    fn feed_v2(&mut self, mut data: &[u8]) {
        let s = &mut self.v2_stream;
        while !data.is_empty() {
            let (now, rest) = data.split_at((LEAF_SIZE - s.in_block).min(data.len()));
            s.block.update(now);
            s.in_block += now.len();
            if s.in_block == LEAF_SIZE {
                s.leaves
                    .push(std::mem::take(&mut s.block).finalize().into());
                s.in_block = 0;
            }
            data = rest;
        }
    }

    fn finish_v2_file(&mut self, index: usize) {
        if !self.options.version.v2() {
            return;
        }
        let mut s = std::mem::take(&mut self.v2_stream);
        if s.in_block > 0 {
            s.leaves.push(s.block.finalize().into());
        }
        if s.leaves.is_empty() {
            return; // an empty file has no root
        }
        let Some(per_piece) = merkle::leaves_per_piece(self.piece_length) else {
            return;
        };
        let hashes = if s.leaves.len() <= per_piece {
            let width = s.leaves.len().next_power_of_two();
            merkle::root_padded(&s.leaves, width, 0).map(|root| V2Hashes {
                root,
                layer: Vec::new(),
            })
        } else {
            let layer: Option<Vec<Hash>> = s
                .leaves
                .chunks(per_piece)
                .map(|c| merkle::root_padded(c, per_piece, 0))
                .collect();
            layer.and_then(|layer| {
                merkle::root_from_layer(&layer, self.piece_length)
                    .map(|root| V2Hashes { root, layer })
            })
        };
        if let Some(slot) = self.v2_done.get_mut(index) {
            *slot = hashes;
        }
    }

    fn encode(&self) -> Vec<u8> {
        let o = &self.options;
        let mut info = Dict::new();
        put(&mut info, "name", text(&self.name));
        put(&mut info, "piece length", int(u64::from(self.piece_length)));
        if o.private {
            put(&mut info, "private", Value::Int(1));
        }
        if let Some(source) = &o.source {
            put(&mut info, "source", text(source));
        }
        if o.version.v1() {
            put(
                &mut info,
                "pieces",
                Value::Bytes(Cow::Owned(self.v1_pieces.clone())),
            );
            if self.single {
                put(
                    &mut info,
                    "length",
                    int(self.files.first().map_or(0, |f| f.length)),
                );
            } else {
                put(&mut info, "files", Value::List(self.v1_files()));
            }
        }
        if o.version.v2() {
            put(&mut info, "meta version", Value::Int(2));
            put(&mut info, "file tree", self.file_tree());
        }
        let mut root = Dict::new();
        let mut tiers: Vec<&Vec<String>> = o.trackers.iter().filter(|t| !t.is_empty()).collect();
        tiers.dedup();
        if let Some(first) = tiers.first().and_then(|t| t.first()) {
            put(&mut root, "announce", text(first));
            let urls: usize = tiers.iter().map(|t| t.len()).sum();
            if urls > 1 {
                let list = tiers
                    .iter()
                    .map(|t| Value::List(t.iter().map(|u| text(u)).collect()))
                    .collect();
                put(&mut root, "announce-list", Value::List(list));
            }
        }
        if !o.web_seeds.is_empty() {
            let seeds = o.web_seeds.iter().map(|u| text(u)).collect();
            put(&mut root, "url-list", Value::List(seeds));
        }
        if let Some(comment) = &o.comment {
            put(&mut root, "comment", text(comment));
        }
        if let Some(by) = &o.created_by {
            put(&mut root, "created by", text(by));
        }
        if let Some(date) = o.creation_date {
            put(&mut root, "creation date", Value::Int(date));
        }
        if o.version.v2() {
            let mut layers = Dict::new();
            for h in self.v2_done.iter().flatten() {
                if !h.layer.is_empty() {
                    layers.insert(
                        Cow::Owned(h.root.to_vec()),
                        Value::Bytes(Cow::Owned(h.layer.concat())),
                    );
                }
            }
            put(&mut root, "piece layers", Value::Dict(layers));
        }
        put(&mut root, "info", Value::Dict(info));
        Value::Dict(root).to_vec()
    }

    fn v1_files(&self) -> Vec<Value<'static>> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::File(i) => {
                    let f = self.files.get(*i)?;
                    let mut d = Dict::new();
                    put(&mut d, "length", int(f.length));
                    put(
                        &mut d,
                        "path",
                        Value::List(f.path.iter().map(|c| text(c)).collect()),
                    );
                    Some(Value::Dict(d))
                }
                Entry::Pad(len) => {
                    let mut d = Dict::new();
                    put(&mut d, "attr", text("p"));
                    put(&mut d, "length", int(*len));
                    put(
                        &mut d,
                        "path",
                        Value::List(vec![text(".pad"), text(&len.to_string())]),
                    );
                    Some(Value::Dict(d))
                }
            })
            .collect()
    }

    fn file_tree(&self) -> Value<'static> {
        #[derive(Default)]
        struct Node {
            children: BTreeMap<String, Node>,
            leaf: Option<Dict<'static>>,
        }
        fn into_value(node: Node) -> Value<'static> {
            let mut d = Dict::new();
            if let Some(leaf) = node.leaf {
                d.insert(Cow::Owned(Vec::new()), Value::Dict(leaf));
            }
            for (name, child) in node.children {
                d.insert(Cow::Owned(name.into_bytes()), into_value(child));
            }
            Value::Dict(d)
        }
        let mut top = Node::default();
        for (i, f) in self.files.iter().enumerate() {
            let path: Vec<&String> = if self.single {
                vec![&self.name]
            } else {
                f.path.iter().collect()
            };
            let mut node = &mut top;
            for c in path {
                node = node.children.entry(c.clone()).or_default();
            }
            let mut leaf = Dict::new();
            put(&mut leaf, "length", int(f.length));
            if let Some(Some(h)) = self.v2_done.get(i) {
                put(
                    &mut leaf,
                    "pieces root",
                    Value::Bytes(Cow::Owned(h.root.to_vec())),
                );
            }
            node.leaf = Some(leaf);
        }
        into_value(top)
    }
}

/// Dravity's own path rules (the portable subset), so every torrent it
/// creates is one it can download.
fn component_ok(c: &str) -> bool {
    crate::path::validate_component_for(c, crate::path::Platform::Unix).is_ok()
}

fn put(d: &mut Dict<'static>, key: &str, value: Value<'static>) {
    d.insert(Cow::Owned(key.as_bytes().to_vec()), value);
}

fn text(s: &str) -> Value<'static> {
    Value::Bytes(Cow::Owned(s.as_bytes().to_vec()))
}

fn int(n: u64) -> Value<'static> {
    Value::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation
    )]

    use super::*;
    use crate::{MetaVersion, Metainfo};

    fn data(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    /// A file's path and the content fed for it.
    type Fed = (Vec<String>, Vec<u8>);

    /// Creates a torrent from `files` (path, content), feeding `chunk`
    /// bytes at a time.
    fn create(
        files: &[(&[&str], Vec<u8>)],
        single: bool,
        options: CreateOptions,
        chunk: usize,
    ) -> (Metainfo, Vec<Fed>) {
        let inputs = files
            .iter()
            .map(|(p, d)| InputFile::new(p.iter().copied(), d.len() as u64))
            .collect();
        let name = if single { files[0].0[0] } else { "root" };
        let mut c = TorrentCreator::new(name, inputs, single, options).unwrap();
        let order: Vec<Fed> = c
            .files()
            .iter()
            .map(|f| {
                let d = files
                    .iter()
                    .find(|(p, _)| p.iter().copied().eq(f.path.iter().map(String::as_str)))
                    .unwrap()
                    .1
                    .clone();
                (f.path.clone(), d)
            })
            .collect();
        let stream: Vec<u8> = order.iter().flat_map(|(_, d)| d.clone()).collect();
        for part in stream.chunks(chunk.max(1)) {
            c.update(part).unwrap();
        }
        let bytes = c.finish().unwrap();
        (Metainfo::from_bytes(&bytes).unwrap(), order)
    }

    /// Checks every piece of `m` against the files' data.
    fn verify_all(m: &Metainfo, order: &[Fed]) {
        let info = m.info();
        let mut stream = vec![0u8; info.total_length() as usize];
        let mut real = order.iter();
        for f in info.files() {
            if f.is_padding() || f.length() == 0 {
                continue;
            }
            let (_, d) = real.find(|(_, d)| !d.is_empty()).unwrap();
            let at = f.offset() as usize;
            stream[at..at + d.len()].copy_from_slice(d);
        }
        for piece in 0..info.piece_count() {
            let r = info.layout().piece_range(piece).unwrap();
            assert!(
                info.check_piece(piece, &stream[r.start as usize..r.end as usize]),
                "piece {piece}"
            );
        }
    }

    #[test]
    fn single_file_in_every_version() {
        let content = data(100_000, 3);
        for version in [CreateVersion::V1, CreateVersion::V2, CreateVersion::Hybrid] {
            let (m, order) = create(
                &[(&["a.bin"], content.clone())],
                true,
                CreateOptions::new(version),
                7_777,
            );
            let info = m.info();
            assert_eq!(info.name(), "a.bin");
            assert!(!info.is_multi_file());
            assert_eq!(info.total_length(), 100_000);
            assert_eq!(info.piece_length(), MIN_PIECE_LENGTH);
            let expected = match version {
                CreateVersion::V1 => MetaVersion::V1,
                CreateVersion::V2 => MetaVersion::V2,
                CreateVersion::Hybrid => MetaVersion::Hybrid,
            };
            assert_eq!(info.version(), expected);
            assert_eq!(m.info_hash_v2().is_some(), version.v2());
            assert!(info.missing_piece_layers().is_empty());
            if version.v2() {
                let fh = merkle::file_hashes(&content, MIN_PIECE_LENGTH).unwrap();
                assert_eq!(info.pieces_root(0), Some(&fh.root));
            }
            verify_all(&m, &order);
        }
    }

    #[test]
    fn hybrid_multi_file_pads_and_matches_merkle_roots() {
        let files: Vec<(&[&str], Vec<u8>)> = vec![
            (&["z", "late.bin"], data(70_000, 1)),
            (&["a.bin"], data(20_000, 2)),
            (&["empty"], Vec::new()),
            (&["m.bin"], data(16_384, 3)),
        ];
        let mut o = CreateOptions::new(CreateVersion::Hybrid);
        o.piece_length = Some(32 * 1024);
        let (m, order) = create(&files, false, o, 5_000);
        let info = m.info();
        // Sorted by path; padding after unaligned files except the last.
        let names: Vec<String> = info
            .files()
            .iter()
            .map(|f| f.path().components().join("/"))
            .collect();
        assert_eq!(
            names,
            [
                "a.bin",
                ".pad/12768",
                "empty",
                "m.bin",
                ".pad/16384",
                "z/late.bin"
            ]
        );
        assert!(info.files()[1].is_padding());
        for f in info
            .files()
            .iter()
            .filter(|f| !f.is_padding() && f.length() > 0)
        {
            assert_eq!(f.offset() % 32_768, 0, "{:?}", f.path());
        }
        let late = &files[0].1;
        let fh = merkle::file_hashes(late, 32 * 1024).unwrap();
        let index = info
            .files()
            .iter()
            .position(|f| f.path().components().join("/") == "z/late.bin")
            .unwrap();
        assert_eq!(info.pieces_root(index), Some(&fh.root));
        assert_eq!(
            m.info().piece_layers().len(),
            1,
            "only late.bin spans pieces"
        );
        assert!(info.missing_piece_layers().is_empty());
        verify_all(&m, &order);
    }

    #[test]
    fn v1_and_v2_multi_file() {
        let files: Vec<(&[&str], Vec<u8>)> = vec![
            (&["b"], data(40_000, 9)),
            (&["c"], data(1, 8)),
            (&["d"], data(50_000, 7)),
        ];
        for (version, pad) in [
            (CreateVersion::V1, false),
            (CreateVersion::V1, true),
            (CreateVersion::V2, false),
        ] {
            let mut o = CreateOptions::new(version);
            o.pad_files = pad;
            let (m, order) = create(&files, false, o, 1 << 20);
            // A v2 layout aligns files by itself (the parser adds virtual
            // padding), so only v1 shows whether pads were written.
            if version == CreateVersion::V1 {
                let pads = m.info().files().iter().filter(|f| f.is_padding()).count();
                assert_eq!(pads > 0, pad);
            }
            verify_all(&m, &order);
        }
    }

    #[test]
    fn metadata_fields_are_written() {
        let mut o = CreateOptions::new(CreateVersion::V1);
        o.private = true;
        o.source = Some("TRK".into());
        o.trackers = vec![
            vec!["http://a/announce".into(), "http://b/announce".into()],
            vec!["udp://c:80".into()],
        ];
        o.web_seeds = vec!["http://seed/".into()];
        o.comment = Some("hi".into());
        o.created_by = Some("dravity".into());
        o.creation_date = Some(1_700_000_000);
        let (m, _) = create(&[(&["x"], data(10, 0))], true, o.clone(), 3);
        assert!(m.info().is_private());
        assert_eq!(m.info().source(), Some("TRK"));
        assert_eq!(m.announce(), Some("http://a/announce"));
        assert_eq!(m.announce_list(), o.trackers.as_slice());
        assert_eq!(m.web_seeds(), ["http://seed/"]);
        assert_eq!(m.comment(), Some("hi"));
        assert_eq!(m.created_by(), Some("dravity"));
        assert_eq!(m.creation_date(), Some(1_700_000_000));
        // A different source is a different torrent.
        o.source = Some("OTHER".into());
        let (other, _) = create(&[(&["x"], data(10, 0))], true, o, 3);
        assert_ne!(other.info_hash(), m.info_hash());
    }

    #[test]
    fn errors() {
        let one = || vec![InputFile::new(["a"], 5)];
        let d = CreateOptions::default;
        assert_eq!(
            TorrentCreator::new("", one(), true, d()).unwrap_err(),
            CreateError::EmptyName
        );
        assert_eq!(
            TorrentCreator::new("n", vec![InputFile::new(["a"], 0)], true, d()).unwrap_err(),
            CreateError::Empty
        );
        assert!(matches!(
            TorrentCreator::new("n", vec![InputFile::new(["..", "a"], 1)], false, d()),
            Err(CreateError::BadPath(_))
        ));
        let dup = vec![InputFile::new(["a"], 1), InputFile::new(["a", "b"], 1)];
        assert!(matches!(
            TorrentCreator::new("n", dup, false, d()),
            Err(CreateError::DuplicatePath(_))
        ));
        assert_eq!(
            TorrentCreator::new("n", vec![], true, d()).unwrap_err(),
            CreateError::NotSingleFile
        );
        let mut o = d();
        o.piece_length = Some(20_000);
        assert_eq!(
            TorrentCreator::new("n", one(), true, o).unwrap_err(),
            CreateError::BadPieceLength(20_000)
        );
        let mut c = TorrentCreator::new("n", one(), true, d()).unwrap();
        c.update(b"abc").unwrap();
        assert_eq!(c.clone().finish().unwrap_err(), CreateError::MissingData(2));
        assert_eq!(c.update(b"defg").unwrap_err(), CreateError::TooMuchData);
    }

    #[test]
    fn auto_piece_lengths() {
        assert_eq!(auto_piece_length(1), MIN_PIECE_LENGTH);
        assert_eq!(auto_piece_length(1500 * 16 * 1024), MIN_PIECE_LENGTH);
        assert_eq!(auto_piece_length(1500 * 16 * 1024 + 1), 32 * 1024);
        assert_eq!(auto_piece_length(4 << 30), 4 << 20);
        assert_eq!(auto_piece_length(u64::MAX), MAX_AUTO_PIECE_LENGTH);
    }

    #[test]
    fn versions_parse() {
        for v in [CreateVersion::V1, CreateVersion::V2, CreateVersion::Hybrid] {
            assert_eq!(v.as_str().parse::<CreateVersion>(), Ok(v));
        }
        assert!("v3".parse::<CreateVersion>().is_err());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]
        #[test]
        fn any_split_gives_the_same_torrent(chunk in 1usize..40_000) {
            let files: Vec<(&[&str], Vec<u8>)> = vec![
                (&["a"], data(33_333, 1)),
                (&["b"], data(17_000, 2)),
            ];
            let (whole, _) = create(&files, false, CreateOptions::default(), usize::MAX);
            let (split, _) = create(&files, false, CreateOptions::default(), chunk);
            proptest::prop_assert_eq!(whole.info_bytes(), split.info_bytes());
        }
    }
}
