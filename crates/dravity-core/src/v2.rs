//! The BEP 52 parts of the `info` dictionary: the `file tree` and the
//! per-file hashes that verify v2 pieces.

use dravity_bencode::Value;

use crate::merkle::{self, Hash};
use crate::metainfo::MetainfoError;
use crate::path::{SafePath, validate_component};

/// Which hash schemes a torrent carries (BEP 52).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MetaVersion {
    /// SHA-1 piece hashes only (BEP 3).
    V1,
    /// A `file tree` with per-file Merkle roots and no v1 `pieces`.
    V2,
    /// Both: v1 `pieces` over a padded, piece-aligned file list, and a v2
    /// `file tree` describing the same files.
    Hybrid,
}

impl MetaVersion {
    /// Whether the torrent carries v1 SHA-1 piece hashes.
    #[must_use]
    pub const fn has_v1(self) -> bool {
        matches!(self, Self::V1 | Self::Hybrid)
    }

    /// Whether the torrent carries a v2 file tree.
    #[must_use]
    pub const fn has_v2(self) -> bool {
        matches!(self, Self::V2 | Self::Hybrid)
    }
}

/// One file read from a `file tree`, in tree order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeFile {
    pub(crate) path: Vec<String>,
    pub(crate) length: u64,
    pub(crate) root: Option<Hash>,
}

/// The v2 hashes of one non-empty file, positioned in the content stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V2File {
    /// Index into [`Info::files`](crate::Info::files).
    pub(crate) file: usize,
    /// First byte of the file in the content stream (piece-aligned).
    pub(crate) offset: u64,
    /// File length.
    pub(crate) length: u64,
    /// Pieces root.
    pub(crate) root: Hash,
    /// First piece of the file.
    pub(crate) first_piece: u32,
    /// Number of pieces the file covers.
    pub(crate) pieces: u32,
    /// The piece layer; empty for a file of one piece (the root is enough).
    /// `None` while a layer is needed but not known yet (a magnet link).
    pub(crate) layer: Option<Vec<Hash>>,
}

impl V2File {
    /// Whether verifying this file's pieces needs a piece layer.
    pub(crate) const fn needs_layer(&self) -> bool {
        self.pieces > 1
    }

    /// Whether `leaves` (the leaf hashes of the file's part of piece
    /// `piece`) match.
    pub(crate) fn check_leaves(&self, piece: u32, leaves: &[Hash], piece_length: u32) -> bool {
        let Some(index) = piece.checked_sub(self.first_piece) else {
            return false;
        };
        if leaves.is_empty() {
            return false;
        }
        if !self.needs_layer() {
            let Some(width) = leaves.len().checked_next_power_of_two() else {
                return false;
            };
            return index == 0 && merkle::root_padded(leaves, width, 0) == Some(self.root);
        }
        let Some(per_piece) = merkle::leaves_per_piece(piece_length) else {
            return false;
        };
        let expected = self
            .layer
            .as_ref()
            .and_then(|l| l.get(usize::try_from(index).ok()?));
        expected.is_some_and(|e| merkle::root_padded(leaves, per_piece, 0).as_ref() == Some(e))
    }

    /// Checks a candidate piece layer against the pieces root.
    pub(crate) fn accepts_layer(&self, layer: &[Hash], piece_length: u32) -> bool {
        u32::try_from(layer.len()).is_ok_and(|n| n == self.pieces)
            && merkle::root_from_layer(layer, piece_length) == Some(self.root)
    }
}

/// Splits a `piece layers` value (concatenated 32-byte hashes).
pub(crate) fn split_layer(bytes: &[u8]) -> Option<Vec<Hash>> {
    let (chunks, rest) = bytes.as_chunks::<32>();
    rest.is_empty().then(|| chunks.to_vec())
}

/// Reads a `file tree` into files in tree (sorted key) order. Paths are
/// validated; `name` is the torrent name for single-file detection, which
/// the caller does.
pub(crate) fn parse_tree(tree: &Value<'_>) -> Result<Vec<TreeFile>, MetainfoError> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    walk(tree, &mut path, &mut out)?;
    if out.is_empty() {
        return Err(MetainfoError::NoFiles);
    }
    Ok(out)
}

fn walk(
    node: &Value<'_>,
    path: &mut Vec<String>,
    out: &mut Vec<TreeFile>,
) -> Result<(), MetainfoError> {
    let dict = node
        .as_dict()
        .ok_or_else(|| MetainfoError::InvalidFileTree(path.join("/")))?;
    if let Some(leaf) = dict.get(&b""[..]) {
        // A file: the empty key holds its properties, and nothing else may
        // sit beside it (a path cannot be a file and a directory).
        if dict.len() != 1 || path.is_empty() {
            return Err(MetainfoError::PathConflict(path.join("/")));
        }
        return leaf_file(leaf, path, out);
    }
    for (key, child) in dict {
        // BEP 52 names are UTF-8. A lossy decode could map two keys onto
        // one path, or save a file under a name that is not its key.
        let name = std::str::from_utf8(key)
            .map_err(|_| MetainfoError::NonUtf8Name(String::from_utf8_lossy(key).into_owned()))?
            .to_owned();
        let joined = || {
            let mut p = path.clone();
            p.push(name.clone());
            p.join("/")
        };
        validate_component(&name).map_err(|source| MetainfoError::UnsafePath {
            path: joined(),
            source,
        })?;
        path.push(name);
        walk(child, path, out)?;
        path.pop();
    }
    Ok(())
}

fn leaf_file(
    leaf: &Value<'_>,
    path: &[String],
    out: &mut Vec<TreeFile>,
) -> Result<(), MetainfoError> {
    let joined = || path.join("/");
    let length = leaf
        .get(b"length")
        .and_then(Value::as_int)
        .and_then(|l| u64::try_from(l).ok())
        .ok_or_else(|| MetainfoError::InvalidFileTree(joined()))?;
    let root = match leaf.get(b"pieces root").map(Value::as_bytes) {
        None => None,
        Some(Some(bytes)) => {
            Some(<Hash>::try_from(bytes).map_err(|_| MetainfoError::InvalidFileTree(joined()))?)
        }
        Some(None) => return Err(MetainfoError::InvalidFileTree(joined())),
    };
    if length > 0 && root.is_none() {
        return Err(MetainfoError::InvalidFileTree(joined()));
    }
    // Validates every component once more, with the platform rules.
    SafePath::new(path.iter().cloned()).map_err(|source| MetainfoError::UnsafePath {
        path: joined(),
        source,
    })?;
    out.push(TreeFile {
        path: path.to_vec(),
        length,
        root: root.filter(|_| length > 0),
    });
    Ok(())
}
