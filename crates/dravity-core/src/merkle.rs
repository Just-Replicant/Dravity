//! BEP 52 Merkle trees: SHA-256 over 16 KiB leaves.
//!
//! A v2 torrent hashes every file on its own. The file is cut into
//! [`LEAF_SIZE`] blocks (the last one may be shorter), each block is hashed
//! with SHA-256, and the leaves are combined pairwise up to a single root,
//! the file's *pieces root*. The tree is padded to a power of two with
//! leaves of 32 zero bytes, so a padding subtree's hash depends only on its
//! height ([`pad_hash`]).
//!
//! A file larger than one piece also has a *piece layer*: the hashes of the
//! subtrees that cover one piece each (the last one padded like the rest of
//! the tree). The piece layer travels outside the `info` dictionary (in the
//! `.torrent` file's `piece layers`, or from peers) and is checked against
//! the pieces root with [`root_from_layer`].
//!
//! ```
//! use dravity_core::merkle;
//!
//! let piece_length = 32 * 1024;
//! let data = vec![7u8; 40_000]; // two pieces of 32 KiB, the second short
//! let tree = merkle::file_hashes(&data, piece_length).unwrap();
//! assert_eq!(tree.layer.len(), 2);
//! assert_eq!(merkle::root_from_layer(&tree.layer, piece_length), Some(tree.root));
//! assert_eq!(merkle::piece_hash(&data[32 * 1024..], piece_length), Some(tree.layer[1]));
//! ```
//!
//! Everything here is pure computation; the session runs it on its blocking
//! pool.

use sha2::{Digest, Sha256};

/// Size of a Merkle leaf (BEP 52: 16 KiB, the same as a request block).
pub const LEAF_SIZE: usize = 16 * 1024;

/// A SHA-256 digest: one node of a Merkle tree.
pub type Hash = [u8; 32];

/// The root and piece layer of one file ([`file_hashes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHashes {
    /// The file's pieces root.
    pub root: Hash,
    /// One hash per piece. Empty when the file fits in one piece: then the
    /// root is the only hash a v2 torrent stores for it.
    pub layer: Vec<Hash>,
}

/// SHA-256 of one leaf's data (at most [`LEAF_SIZE`] bytes).
#[must_use]
pub fn hash_leaf(data: &[u8]) -> Hash {
    Sha256::digest(data).into()
}

/// The parent of two nodes.
#[must_use]
pub fn hash_pair(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// The hash of a subtree of `height` levels made only of padding leaves
/// (height 0 is a single zero leaf).
#[must_use]
pub fn pad_hash(height: u32) -> Hash {
    let mut h = [0u8; 32];
    for _ in 0..height {
        h = hash_pair(&h, &h);
    }
    h
}

/// Leaf hashes of `data`, one per [`LEAF_SIZE`] block.
#[must_use]
pub fn leaf_hashes(data: &[u8]) -> Vec<Hash> {
    data.chunks(LEAF_SIZE).map(hash_leaf).collect()
}

/// The root of a tree whose bottom layer is `nodes` (at level `height`
/// above the leaves) padded to `width` nodes, `width` a power of two not
/// smaller than `nodes.len()`. Returns `None` when `width` is not such a
/// power of two.
#[must_use]
pub fn root_padded(nodes: &[Hash], width: usize, height: u32) -> Option<Hash> {
    if !width.is_power_of_two() || nodes.len() > width {
        return None;
    }
    let mut layer = nodes.to_vec();
    let mut level = height;
    let mut width = width;
    while width > 1 {
        let pad = pad_hash(level);
        layer = layer
            .chunks(2)
            .map(|pair| match pair {
                [l, r] => hash_pair(l, r),
                [l] => hash_pair(l, &pad),
                _ => pad,
            })
            .collect();
        width /= 2;
        level += 1;
    }
    Some(layer.first().copied().unwrap_or_else(|| pad_hash(level)))
}

/// Leaves per piece for a v2 piece length: `None` unless `piece_length` is a
/// power of two of at least [`LEAF_SIZE`].
#[must_use]
pub fn leaves_per_piece(piece_length: u32) -> Option<usize> {
    let len = usize::try_from(piece_length).ok()?;
    (len >= LEAF_SIZE && len.is_power_of_two()).then_some(len / LEAF_SIZE)
}

/// Height of a piece's subtree above the leaves.
fn piece_height(per_piece: usize) -> u32 {
    per_piece.trailing_zeros()
}

/// The piece-layer hash of one piece's data: its leaves padded to a full
/// piece. `data` is the part of the piece inside the file (shorter for the
/// file's last piece). `None` for an invalid piece length or data longer
/// than a piece.
#[must_use]
pub fn piece_hash(data: &[u8], piece_length: u32) -> Option<Hash> {
    let per_piece = leaves_per_piece(piece_length)?;
    root_padded(&leaf_hashes(data), per_piece, 0)
}

/// The pieces root of a file no longer than one piece: its leaves padded to
/// the next power of two. `None` for an empty file (it has no root).
#[must_use]
pub fn small_file_root(data: &[u8]) -> Option<Hash> {
    let leaves = leaf_hashes(data);
    let width = leaves.len().checked_next_power_of_two()?;
    if leaves.is_empty() {
        return None;
    }
    root_padded(&leaves, width, 0)
}

/// The pieces root implied by a file's piece layer. The layer is padded to
/// a power of two with the hash of an all-padding piece.
#[must_use]
pub fn root_from_layer(layer: &[Hash], piece_length: u32) -> Option<Hash> {
    let per_piece = leaves_per_piece(piece_length)?;
    if layer.is_empty() {
        return None;
    }
    let width = layer.len().checked_next_power_of_two()?;
    root_padded(layer, width, piece_height(per_piece))
}

/// The pieces root and piece layer of a whole file. `None` for an empty file
/// or an invalid piece length.
#[must_use]
pub fn file_hashes(data: &[u8], piece_length: u32) -> Option<FileHashes> {
    let piece = usize::try_from(piece_length).ok()?;
    leaves_per_piece(piece_length)?;
    if data.is_empty() {
        return None;
    }
    if data.len() <= piece {
        return Some(FileHashes {
            root: small_file_root(data)?,
            layer: Vec::new(),
        });
    }
    let layer = data
        .chunks(piece)
        .map(|p| piece_hash(p, piece_length))
        .collect::<Option<Vec<_>>>()?;
    Some(FileHashes {
        root: root_from_layer(&layer, piece_length)?,
        layer,
    })
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The tree built the slow, obvious way: every leaf padded out to a power
    /// of two (at least `min_width`), then halved until one node is left.
    fn naive_root(data: &[u8], min_width: usize) -> Hash {
        let mut layer = leaf_hashes(data);
        let width = layer.len().next_power_of_two().max(min_width);
        layer.resize(width, [0; 32]);
        while layer.len() > 1 {
            layer = layer.chunks(2).map(|p| hash_pair(&p[0], &p[1])).collect();
        }
        layer[0]
    }

    #[test]
    fn known_values() {
        // SHA-256("") and the zero pad.
        assert_eq!(
            hash_leaf(b"")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(pad_hash(0), [0; 32]);
        assert_eq!(pad_hash(1), hash_pair(&[0; 32], &[0; 32]));
    }

    #[test]
    fn one_leaf_file_root_is_the_leaf_hash() {
        assert_eq!(small_file_root(b"abc"), Some(hash_leaf(b"abc")));
        assert_eq!(small_file_root(b""), None);
        let tree = file_hashes(b"abc", 16 * 1024).unwrap();
        assert!(tree.layer.is_empty());
        assert_eq!(tree.root, hash_leaf(b"abc"));
    }

    #[test]
    fn invalid_piece_lengths() {
        assert_eq!(leaves_per_piece(0), None);
        assert_eq!(leaves_per_piece(8 * 1024), None);
        assert_eq!(leaves_per_piece(48 * 1024), None);
        assert_eq!(leaves_per_piece(64 * 1024), Some(4));
        assert_eq!(piece_hash(&[0; 10], 3), None);
        assert_eq!(piece_hash(&[0; 40_000], 32 * 1024), None);
        assert_eq!(root_from_layer(&[], 32 * 1024), None);
        assert_eq!(root_padded(&[[1; 32]; 3], 2, 0), None);
    }

    #[test]
    fn last_piece_is_padded_to_a_full_piece() {
        let piece = 64 * 1024;
        let data = vec![3u8; 70_000];
        let tree = file_hashes(&data, piece).unwrap();
        assert_eq!(tree.layer.len(), 2);
        assert_eq!(tree.layer[1], naive_root(&data[65_536..], 4));
        assert_eq!(tree.root, naive_root(&data, 1));
    }

    proptest! {
        #[test]
        fn matches_the_naive_tree(len in 1usize..300_000, shift in 0u32..3, byte in any::<u8>()) {
            let piece_length = (16 * 1024) << shift;
            let data: Vec<u8> = (0..len).map(|i| ((i % 256) as u8).wrapping_mul(byte)).collect();
            let tree = file_hashes(&data, piece_length).unwrap();
            prop_assert_eq!(tree.root, naive_root(&data, 1));
            let piece = piece_length as usize;
            if len > piece {
                prop_assert_eq!(tree.layer.len(), len.div_ceil(piece));
                for (i, chunk) in data.chunks(piece).enumerate() {
                    prop_assert_eq!(tree.layer[i], naive_root(chunk, piece / LEAF_SIZE));
                }
                prop_assert_eq!(root_from_layer(&tree.layer, piece_length), Some(tree.root));
            } else {
                prop_assert!(tree.layer.is_empty());
            }
        }
    }
}
