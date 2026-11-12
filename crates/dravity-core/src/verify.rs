//! Hash-on-arrival: verify a piece from its blocks as they are written,
//! without reading it back from storage.
//!
//! A [`PieceHasher`] takes a piece's blocks in any order. The SHA-1 (v1 and
//! hybrid torrents) is fed in order: the hasher holds a block that arrives
//! early until the gap before it is filled. The SHA-256 leaves (v2 and
//! hybrid) are hashed when each block arrives, because a block is exactly
//! one leaf. [`PieceHasher::verify`] then checks the digests with the same
//! rules as [`Info::check_piece`].
//!
//! A hasher that receives a block twice, or one that does not fit the
//! piece, stops ([`PieceHasher::add`] returns `false`). Storage holds the
//! last write, which the hasher cannot vouch for, so the caller must fall
//! back to reading the piece back.

use std::collections::BTreeMap;
use std::fmt;

use sha1::{Digest, Sha1};

use crate::layout::BLOCK_SIZE;
use crate::merkle::{self, Hash};
use crate::metainfo::Info;

// One block is one v2 leaf, so leaves can be hashed per block.
const _: () = assert!(BLOCK_SIZE as usize == merkle::LEAF_SIZE);

/// Incremental verification of one piece. `B` is the block buffer type
/// (`Vec<u8>`, `bytes::Bytes`, ...).
pub struct PieceHasher<B> {
    piece: u32,
    length: u32,
    /// Blocks received, by block index.
    got: Vec<bool>,
    /// SHA-1 of the piece so far (v1 and hybrid).
    sha1: Option<Sha1>,
    /// Bytes fed to `sha1`.
    next: u32,
    /// Early blocks waiting for `sha1` to reach them.
    pending: BTreeMap<u32, B>,
    pending_bytes: usize,
    /// Length of the v2 file's part of the piece, and its leaves so far.
    v2: Option<(usize, Vec<Option<Hash>>)>,
    broken: bool,
}

impl<B: AsRef<[u8]>> PieceHasher<B> {
    /// A hasher for piece `piece` of `info`. `None` for a piece out of
    /// range.
    #[must_use]
    pub fn new(info: &Info, piece: u32) -> Option<Self> {
        let length = info.layout().piece_size(piece)?;
        let blocks = usize::try_from(length.div_ceil(BLOCK_SIZE)).ok()?;
        let v2 = info
            .v2_part_len(piece)
            .map(|len| (len, vec![None; len.div_ceil(merkle::LEAF_SIZE)]));
        Some(Self {
            piece,
            length,
            got: vec![false; blocks],
            sha1: info.version().has_v1().then(Sha1::new),
            next: 0,
            pending: BTreeMap::new(),
            pending_bytes: 0,
            v2,
            broken: false,
        })
    }

    /// The piece this hasher is for.
    #[must_use]
    pub const fn piece(&self) -> u32 {
        self.piece
    }

    /// Adds the block at `offset` (in the piece). Returns `false` once the
    /// hasher cannot vouch for the piece any more: the block was already
    /// added, or it is misaligned or the wrong length.
    pub fn add(&mut self, offset: u32, data: B) -> bool {
        if self.broken {
            return false;
        }
        let expected = self.length.saturating_sub(offset).min(BLOCK_SIZE);
        let index = usize::try_from(offset / BLOCK_SIZE).unwrap_or(usize::MAX);
        let fits = offset.is_multiple_of(BLOCK_SIZE)
            && expected > 0
            && u32::try_from(data.as_ref().len()).ok() == Some(expected);
        match self.got.get_mut(index) {
            Some(got) if fits && !*got => *got = true,
            _ => {
                self.broken = true;
                self.pending.clear();
                self.pending_bytes = 0;
                return false;
            }
        }
        if let Some((len, leaves)) = &mut self.v2 {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if let (Some(leaf), Some(part)) = (
                leaves.get_mut(index),
                data.as_ref()
                    .get(..len.saturating_sub(start).min(data.as_ref().len())),
            ) && !part.is_empty()
            {
                *leaf = Some(merkle::hash_leaf(part));
            }
        }
        if let Some(sha1) = &mut self.sha1 {
            if offset == self.next {
                sha1.update(data.as_ref());
                self.next += expected;
                while let Some(block) = self.pending.remove(&self.next) {
                    let len = block.as_ref().len();
                    sha1.update(block.as_ref());
                    self.pending_bytes -= len;
                    self.next += u32::try_from(len).unwrap_or(0);
                }
            } else {
                self.pending_bytes += data.as_ref().len();
                self.pending.insert(offset, data);
            }
        }
        true
    }

    /// Bytes held for in-order SHA-1 feeding.
    #[must_use]
    pub const fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }

    /// Whether every block was added (and the hasher is still usable).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.broken && self.got.iter().all(|&g| g)
    }

    /// Whether the piece matches the torrent's hashes, as
    /// [`Info::check_piece`] would say for the same bytes. `None` when the
    /// hasher is incomplete or broken (then read the piece back).
    #[must_use]
    pub fn verify(self, info: &Info) -> Option<bool> {
        if !self.is_complete() || (self.sha1.is_some() && self.next != self.length) {
            return None;
        }
        let sha1: Option<[u8; 20]> = self.sha1.map(|s| s.finalize().into());
        let leaves = match self.v2 {
            Some((_, leaves)) => leaves.into_iter().collect::<Option<Vec<Hash>>>()?,
            None => Vec::new(),
        };
        Some(info.check_digests(self.piece, sha1.as_ref(), &leaves))
    }
}

impl<B> fmt::Debug for PieceHasher<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PieceHasher")
            .field("piece", &self.piece)
            .field("length", &self.length)
            .field("received", &self.got.iter().filter(|&&g| g).count())
            .field("pending_bytes", &self.pending_bytes)
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}
