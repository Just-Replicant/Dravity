//! Rarest-first piece picker with per-block state.
//!
//! The picker tracks, for every piece we do not have yet:
//!
//! * its **availability** (how many connected peers have it), kept in an
//!   ordered set of `(availability, piece)` so the rarest wanted piece is found
//!   without scanning the whole torrent (rarity buckets);
//! * once started, the state of each block: missing → requested → received →
//!   written. When every block is written the piece is handed out for hashing.
//!
//! Partially downloaded pieces are always finished before new pieces are
//! started, which keeps the number of partial pieces (and wasted memory and
//! re-requests) low.

use std::collections::{BTreeMap, BTreeSet};

use dravity_core::{BlockRef, PieceLayout};

use crate::bitfield::Bitfield;

/// Download state of one block of a started piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockState {
    Missing,
    Requested,
    Received,
    Written,
}

/// A started (partial) piece.
#[derive(Debug, Clone)]
struct Partial {
    blocks: Vec<BlockState>,
    written: usize,
    hashing: bool,
}

impl Partial {
    fn new(blocks: u32) -> Self {
        Self {
            blocks: vec![BlockState::Missing; usize::try_from(blocks).unwrap_or(0)],
            written: 0,
            hashing: false,
        }
    }

    fn reset(&mut self) {
        self.blocks.fill(BlockState::Missing);
        self.written = 0;
        self.hashing = false;
    }
}

/// Outcome of [`Picker::block_received`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// The block was wanted; write it to storage.
    Accepted,
    /// The block was not wanted (already received, piece complete, invalid).
    Ignored,
}

/// Rarest-first piece and block picker.
#[derive(Debug, Clone)]
pub struct Picker {
    layout: PieceLayout,
    have: Bitfield,
    availability: Vec<u32>,
    /// Wanted, not yet started pieces ordered by `(availability, index)`.
    queue: BTreeSet<(u32, u32)>,
    partial: BTreeMap<u32, Partial>,
}

impl Picker {
    /// A picker for `layout` that already has the pieces in `have`.
    ///
    /// # Panics
    /// Never; a `have` bitfield of the wrong length is treated as empty.
    #[must_use]
    pub fn new(layout: PieceLayout, have: Bitfield) -> Self {
        let count = layout.piece_count();
        let have = if have.len() == count {
            have
        } else {
            Bitfield::new(count)
        };
        let queue = (0..count)
            .filter(|&p| !have.get(p))
            .map(|p| (0, p))
            .collect();
        Self {
            layout,
            have,
            availability: vec![0; usize::try_from(count).unwrap_or(0)],
            queue,
            partial: BTreeMap::new(),
        }
    }

    /// The piece geometry.
    #[must_use]
    pub const fn layout(&self) -> &PieceLayout {
        &self.layout
    }

    /// Pieces we have (hash-verified).
    #[must_use]
    pub const fn have(&self) -> &Bitfield {
        &self.have
    }

    /// Whether every piece is verified.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.have.is_full()
    }

    /// Number of started but unverified pieces.
    #[must_use]
    pub fn partial_count(&self) -> usize {
        self.partial.len()
    }

    /// Availability of `piece` among connected peers.
    #[must_use]
    pub fn availability(&self, piece: u32) -> u32 {
        self.avail_slot(piece).copied().unwrap_or(0)
    }

    fn avail_slot(&self, piece: u32) -> Option<&u32> {
        usize::try_from(piece)
            .ok()
            .and_then(|i| self.availability.get(i))
    }

    fn change_availability(&mut self, piece: u32, up: bool) {
        let Some(slot) = usize::try_from(piece)
            .ok()
            .and_then(|i| self.availability.get_mut(i))
        else {
            return;
        };
        let old = *slot;
        let new = if up {
            old.saturating_add(1)
        } else {
            old.saturating_sub(1)
        };
        *slot = new;
        if self.queue.remove(&(old, piece)) {
            self.queue.insert((new, piece));
        }
    }

    /// Records that a peer with `pieces` connected (or sent its bitfield).
    pub fn add_peer_pieces(&mut self, pieces: &Bitfield) {
        for p in pieces.ones() {
            self.change_availability(p, true);
        }
    }

    /// Records that a peer with `pieces` disconnected.
    pub fn remove_peer_pieces(&mut self, pieces: &Bitfield) {
        for p in pieces.ones() {
            self.change_availability(p, false);
        }
    }

    /// Records a peer's `have` message for `piece`.
    pub fn add_peer_piece(&mut self, piece: u32) {
        self.change_availability(piece, true);
    }

    /// Whether a peer with `pieces` has something we still want.
    #[must_use]
    pub fn is_interesting(&self, pieces: &Bitfield) -> bool {
        pieces.has_any_missing_from(&self.have)
    }

    /// Picks the next block to request from a peer with `pieces`, marking it
    /// requested. Unfinished pieces come first, then the rarest new piece the
    /// peer has (ties broken by lowest index).
    pub fn pick(&mut self, pieces: &Bitfield) -> Option<BlockRef> {
        if let Some(block) = self.pick_from_partial(pieces) {
            return Some(block);
        }
        let piece = self.queue.iter().find(|&&(_, p)| pieces.get(p)).copied()?;
        self.queue.remove(&piece);
        let index = piece.1;
        let blocks = self.layout.block_count(index)?;
        self.partial.insert(index, Partial::new(blocks));
        self.pick_from_partial(pieces)
    }

    fn pick_from_partial(&mut self, pieces: &Bitfield) -> Option<BlockRef> {
        for (&index, partial) in &mut self.partial {
            if partial.hashing || !pieces.get(index) {
                continue;
            }
            if let Some((b, state)) = partial
                .blocks
                .iter_mut()
                .enumerate()
                .find(|(_, s)| **s == BlockState::Missing)
            {
                let block = self.layout.block(index, u32::try_from(b).ok()?)?;
                *state = BlockState::Requested;
                return Some(block);
            }
        }
        None
    }

    fn block_state(&mut self, block: BlockRef) -> Option<(&mut Partial, usize)> {
        if !self.layout.is_valid_request(block) || block.offset % dravity_core::BLOCK_SIZE != 0 {
            return None;
        }
        let expected = self
            .layout
            .block(block.piece, block.offset / dravity_core::BLOCK_SIZE)?;
        if expected != block {
            return None;
        }
        let idx = usize::try_from(block.offset / dravity_core::BLOCK_SIZE).ok()?;
        let partial = self.partial.get_mut(&block.piece)?;
        Some((partial, idx))
    }

    /// Returns a requested block to the pool (peer choked, disconnected, or
    /// the request timed out). No-op if the block was already received.
    pub fn release(&mut self, block: BlockRef) {
        if let Some((partial, idx)) = self.block_state(block) {
            if let Some(s) = partial.blocks.get_mut(idx) {
                if *s == BlockState::Requested {
                    *s = BlockState::Missing;
                }
            }
        }
    }

    /// Records that `block`'s data arrived. Late data for a block that timed
    /// out (and is missing again) is accepted too.
    pub fn block_received(&mut self, block: BlockRef) -> Received {
        let Some((partial, idx)) = self.block_state(block) else {
            return Received::Ignored;
        };
        match partial.blocks.get_mut(idx) {
            Some(s @ (BlockState::Missing | BlockState::Requested)) => {
                *s = BlockState::Received;
                Received::Accepted
            }
            _ => Received::Ignored,
        }
    }

    /// Records that `block` was written to storage. Returns the piece index if
    /// the piece is now complete and should be hashed.
    pub fn block_written(&mut self, block: BlockRef) -> Option<u32> {
        let (partial, idx) = self.block_state(block)?;
        let s = partial.blocks.get_mut(idx)?;
        if *s != BlockState::Received {
            return None;
        }
        *s = BlockState::Written;
        partial.written += 1;
        if partial.written == partial.blocks.len() && !partial.hashing {
            partial.hashing = true;
            return Some(block.piece);
        }
        None
    }

    /// Records that writing `block` failed; it will be downloaded again.
    pub fn block_write_failed(&mut self, block: BlockRef) {
        if let Some((partial, idx)) = self.block_state(block) {
            if let Some(s) = partial.blocks.get_mut(idx) {
                if *s == BlockState::Received {
                    *s = BlockState::Missing;
                }
            }
        }
    }

    /// Records a hash result. On success the piece is marked as had and
    /// `true` is returned if it was newly added. On failure every block of
    /// the piece is downloaded again.
    pub fn piece_hashed(&mut self, piece: u32, ok: bool) -> bool {
        if ok {
            self.mark_have(piece)
        } else {
            if let Some(partial) = self.partial.get_mut(&piece) {
                partial.reset();
            }
            false
        }
    }

    /// Marks `piece` as verified (e.g. found on disk during a check).
    /// Returns `true` if newly set.
    pub fn mark_have(&mut self, piece: u32) -> bool {
        if !self.have.set(piece) {
            return false;
        }
        self.partial.remove(&piece);
        let avail = self.availability(piece);
        self.queue.remove(&(avail, piece));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dravity_core::BLOCK_SIZE;

    fn layout(pieces: u32, piece_len: u32) -> PieceLayout {
        PieceLayout::new(piece_len, u64::from(pieces) * u64::from(piece_len)).unwrap()
    }

    #[test]
    fn rarest_first() {
        let mut p = Picker::new(layout(4, BLOCK_SIZE), Bitfield::new(4));
        let all = Bitfield::full(4);
        let mut some = Bitfield::new(4);
        some.set(0);
        some.set(1);
        some.set(3);
        p.add_peer_pieces(&all);
        p.add_peer_pieces(&some);
        // Piece 2 is held by one peer only.
        assert_eq!(p.pick(&all).map(|b| b.piece), Some(2));
        // The other peer does not have 2: lowest index among rarest it has.
        assert_eq!(p.pick(&some).map(|b| b.piece), Some(0));
    }

    #[test]
    fn finishes_partial_pieces_first() {
        let mut p = Picker::new(layout(2, 2 * BLOCK_SIZE), Bitfield::new(2));
        let all = Bitfield::full(2);
        p.add_peer_pieces(&all);
        let a = p.pick(&all).unwrap();
        let b = p.pick(&all).unwrap();
        assert_eq!((a.piece, a.offset), (0, 0));
        assert_eq!((b.piece, b.offset), (0, BLOCK_SIZE));
        assert_eq!(p.pick(&all).map(|b| b.piece), Some(1));
    }

    #[test]
    fn full_lifecycle_and_hash_failure() {
        let l = PieceLayout::new(2 * BLOCK_SIZE, u64::from(BLOCK_SIZE) + 100).unwrap();
        let mut p = Picker::new(l, Bitfield::new(1));
        let all = Bitfield::full(1);
        let a = p.pick(&all).unwrap();
        let b = p.pick(&all).unwrap();
        assert_eq!(b.length, 100);
        assert_eq!(p.pick(&all), None);
        assert_eq!(p.block_received(a), Received::Accepted);
        assert_eq!(p.block_received(a), Received::Ignored);
        assert_eq!(p.block_written(a), None);
        assert_eq!(p.block_received(b), Received::Accepted);
        assert_eq!(p.block_written(b), Some(0));
        assert_eq!(p.pick(&all), None, "hashing piece is not re-requested");
        assert!(!p.piece_hashed(0, false));
        assert_eq!(p.pick(&all), Some(a));
        p.release(a);
        assert_eq!(p.pick(&all), Some(a));
        assert!(p.piece_hashed(0, true));
        assert!(p.is_complete());
        assert_eq!(p.partial_count(), 0);
        assert_eq!(p.block_received(a), Received::Ignored);
    }

    #[test]
    fn rejects_malformed_blocks() {
        let mut p = Picker::new(layout(1, 2 * BLOCK_SIZE), Bitfield::new(1));
        let all = Bitfield::full(1);
        let a = p.pick(&all).unwrap();
        for bad in [
            BlockRef { length: 1, ..a },
            BlockRef { offset: 1, ..a },
            BlockRef { piece: 9, ..a },
        ] {
            assert_eq!(p.block_received(bad), Received::Ignored);
        }
        assert_eq!(p.block_received(a), Received::Accepted);
    }

    #[test]
    fn write_failure_requeues() {
        let mut p = Picker::new(layout(1, BLOCK_SIZE), Bitfield::new(1));
        let all = Bitfield::full(1);
        let a = p.pick(&all).unwrap();
        p.block_received(a);
        p.block_write_failed(a);
        assert_eq!(p.pick(&all), Some(a));
    }

    #[test]
    fn availability_tracks_peers() {
        let mut p = Picker::new(layout(3, BLOCK_SIZE), Bitfield::new(3));
        let all = Bitfield::full(3);
        p.add_peer_pieces(&all);
        p.add_peer_piece(1);
        assert_eq!(p.availability(1), 2);
        p.remove_peer_pieces(&all);
        assert_eq!(p.availability(1), 1);
        assert_eq!(p.availability(0), 0);
        assert!(p.is_interesting(&all));
        assert!(p.mark_have(0) && p.mark_have(1) && p.mark_have(2));
        assert!(!p.is_interesting(&all));
    }
}
