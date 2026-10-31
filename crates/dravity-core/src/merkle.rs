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

/// Largest number of base hashes asked for in one hash request (BEP 52:
/// "length SHOULD NOT be greater than 512").
pub const MAX_HASH_REQUEST: usize = 512;

/// Base layer hashes and their uncles: the payload of a BEP 52 `hashes`
/// message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashProof {
    /// The requested hashes of the base layer (padding hashes past the end
    /// of the file included).
    pub base: Vec<Hash>,
    /// One sibling per layer above the subtree the base spans, from the
    /// leaf side towards the root.
    pub uncles: Vec<Hash>,
}

/// One hash request that fetches part of a file's piece layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerRequest {
    /// Offset of the first hash in the piece layer.
    pub index: usize,
    /// Number of hashes (a power of two, at least 2).
    pub length: usize,
    /// Proof layers that reach the pieces root from this block.
    pub proof_layers: u32,
}

/// Every layer of the tree whose bottom layer is `bottom` at `height`,
/// padded to a power of two: the padded bottom first, `[root]` last.
fn tree_layers(bottom: &[Hash], height: u32) -> Option<Vec<Vec<Hash>>> {
    let width = bottom.len().checked_next_power_of_two()?;
    let mut layer = bottom.to_vec();
    layer.resize(width, pad_hash(height));
    let mut out = vec![layer];
    while let Some(last) = out.last().filter(|l| l.len() > 1) {
        let next = last
            .chunks(2)
            .map(|pair| match pair {
                [l, r] => hash_pair(l, r),
                _ => [0; 32],
            })
            .collect();
        out.push(next);
    }
    Some(out)
}

/// Hashes answering a BEP 52 hash request for `length` hashes at `index`
/// of layer `base_layer`, from a file's piece layer `layer`, with up to
/// `proof_layers` layers of proof (the first `log2(length) - 1` are implied
/// by the base hashes and carry no uncle).
///
/// Only layers at or above the piece layer can be served from it. `None`
/// for a lower base layer, an invalid piece length, a `length` that is not
/// a power of two of at least 2, an `index` not a multiple of `length`, or
/// a range outside the (padded) layer.
#[must_use]
pub fn layer_proof(
    layer: &[Hash],
    piece_length: u32,
    base_layer: u32,
    index: usize,
    length: usize,
    proof_layers: u32,
) -> Option<HashProof> {
    let height = piece_height(leaves_per_piece(piece_length)?);
    let up = usize::try_from(base_layer.checked_sub(height)?).ok()?;
    if layer.is_empty() || length < 2 || !length.is_power_of_two() || !index.is_multiple_of(length)
    {
        return None;
    }
    let levels = tree_layers(layer, height)?;
    let base = levels
        .get(up)?
        .get(index..index.checked_add(length)?)?
        .to_vec();
    let mut wanted = proof_layers.saturating_sub(length.ilog2() - 1);
    let mut level = up + usize::try_from(length.ilog2()).ok()?;
    let mut pos = index / length;
    let mut uncles = Vec::new();
    while wanted > 0 {
        let Some(nodes) = levels.get(level).filter(|n| n.len() > 1) else {
            break;
        };
        uncles.push(*nodes.get(pos ^ 1)?);
        pos /= 2;
        level += 1;
        wanted -= 1;
    }
    Some(HashProof { base, uncles })
}

/// Whether `base` (hashes at `index` of some layer, a power of two of them)
/// and `uncles` (leaf side first) lead exactly to `root`.
#[must_use]
pub fn verify_proof(base: &[Hash], uncles: &[Hash], index: usize, root: &Hash) -> bool {
    let length = base.len();
    if length == 0 || !length.is_power_of_two() || !index.is_multiple_of(length) {
        return false;
    }
    let mut layer = base.to_vec();
    while layer.len() > 1 {
        layer = layer
            .chunks(2)
            .map(|pair| match pair {
                [l, r] => hash_pair(l, r),
                _ => [0; 32],
            })
            .collect();
    }
    let Some(mut node) = layer.first().copied() else {
        return false;
    };
    let mut pos = index / length;
    for uncle in uncles {
        node = if pos.is_multiple_of(2) {
            hash_pair(&node, uncle)
        } else {
            hash_pair(uncle, &node)
        };
        pos /= 2;
    }
    pos == 0 && node == *root
}

/// The hash requests that fetch the piece layer of a file with `pieces`
/// pieces (at least 2): blocks of at most [`MAX_HASH_REQUEST`] hashes, each
/// with the proof layers that reach the pieces root. Blocks made only of
/// padding are not requested.
#[must_use]
pub fn layer_requests(pieces: usize) -> Vec<LayerRequest> {
    let Some(width) = pieces.checked_next_power_of_two() else {
        return Vec::new();
    };
    if pieces < 2 {
        return Vec::new();
    }
    let length = width.min(MAX_HASH_REQUEST);
    let proof_layers = width.ilog2() - 1;
    (0..pieces)
        .step_by(length)
        .map(|index| LayerRequest {
            index,
            length,
            proof_layers,
        })
        .collect()
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

    #[test]
    fn proofs_from_a_piece_layer() {
        let piece = 16 * 1024;
        let data: Vec<u8> = (0usize..20 * 16 * 1024 + 5)
            .map(|i| (i % 251) as u8)
            .collect();
        let tree = file_hashes(&data, piece).unwrap();
        assert_eq!(tree.layer.len(), 21); // padded to 32
        // The whole layer at once needs no uncles.
        let all = layer_proof(&tree.layer, piece, 0, 0, 32, 4).unwrap();
        assert!(all.uncles.is_empty());
        assert_eq!(&all.base[..21], &tree.layer[..]);
        assert!(all.base[21..].iter().all(|h| *h == [0; 32]));
        assert!(verify_proof(&all.base, &[], 0, &tree.root));
        // A block of 4 needs log2(32 / 4) = 3 uncles.
        let part = layer_proof(&tree.layer, piece, 0, 8, 4, 4).unwrap();
        assert_eq!(part.uncles.len(), 3);
        assert!(verify_proof(&part.base, &part.uncles, 8, &tree.root));
        assert!(!verify_proof(&part.base, &part.uncles, 12, &tree.root));
        assert!(!verify_proof(&part.base, &part.uncles[..2], 8, &tree.root));
        let mut bad = part.clone();
        bad.base[1][0] ^= 1;
        assert!(!verify_proof(&bad.base, &bad.uncles, 8, &tree.root));
        // Too few proof layers: fewer uncles, no proof of the root.
        let short = layer_proof(&tree.layer, piece, 0, 8, 4, 2).unwrap();
        assert_eq!(short.uncles.len(), 1);
        // Above the piece layer.
        let up = layer_proof(&tree.layer, piece, 1, 0, 2, 3).unwrap();
        assert!(verify_proof(&up.base, &up.uncles, 0, &tree.root));
        // Leaves cannot be served from a 32 KiB piece layer; bad shapes.
        let tree32 = file_hashes(&data, 2 * piece).unwrap();
        assert!(layer_proof(&tree32.layer, 2 * piece, 0, 0, 2, 0).is_none());
        assert!(layer_proof(&tree.layer, piece, 0, 2, 4, 0).is_none());
        assert!(layer_proof(&tree.layer, piece, 0, 0, 3, 0).is_none());
        assert!(layer_proof(&tree.layer, piece, 0, 32, 2, 0).is_none());
        assert!(layer_proof(&tree.layer, piece, 0, 0, 64, 0).is_none());
    }

    #[test]
    fn layer_request_plans() {
        assert!(layer_requests(1).is_empty());
        assert_eq!(
            layer_requests(3),
            [LayerRequest {
                index: 0,
                length: 4,
                proof_layers: 1
            }]
        );
        let big = layer_requests(1300);
        assert_eq!(big.len(), 3);
        assert!(big.iter().all(|r| r.length == 512 && r.proof_layers == 10));
        assert_eq!(big[2].index, 1024);
    }

    proptest! {
        #[test]
        fn requested_blocks_prove_the_layer(pieces in 2usize..1500, seed in any::<u8>()) {
            let layer: Vec<Hash> = (0..pieces).map(|i| hash_leaf(&[seed, (i % 256) as u8, (i / 256) as u8])).collect();
            let piece = 16 * 1024;
            let root = root_from_layer(&layer, piece).unwrap();
            let mut rebuilt = vec![[0u8; 32]; pieces];
            for r in layer_requests(pieces) {
                let proof = layer_proof(&layer, piece, 0, r.index, r.length, r.proof_layers).unwrap();
                prop_assert!(verify_proof(&proof.base, &proof.uncles, r.index, &root));
                let n = r.length.min(pieces - r.index);
                rebuilt[r.index..r.index + n].copy_from_slice(&proof.base[..n]);
            }
            prop_assert_eq!(rebuilt, layer);
        }

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
