//! Rarest-first piece picker with per-block state, priorities and an
//! urgent (streaming) list.
//!
//! The picker tracks, for every piece we do not have yet:
//!
//! * its **priority** ([`Priority`]): skipped pieces are never started;
//!   among the others a higher priority always wins over rarity;
//! * its **availability** (how many connected peers have it), kept in an
//!   ordered set of `(priority rank, availability, piece)` so the best wanted
//!   piece is found without scanning the whole torrent (rarity buckets);
//! * once started, the state of each block: missing → requested → received →
//!   written. When every block is written the piece is handed out for hashing.
//!
//! **Urgent** pieces ([`Picker::set_urgent`]) come before everything else,
//! in the order given and regardless of rarity or priority: a streaming
//! reader marks the pieces just ahead of its read position this way.
//!
//! Partially downloaded pieces are finished before new pieces are started,
//! which keeps the number of partial pieces (and wasted memory and
//! re-requests) low.
//!
//! Once every wanted block is requested, [`Picker::requested_blocks`] lists
//! blocks that are in flight so the engine can request them from a second
//! peer (endgame mode and stealing from slow peers).

use std::collections::{BTreeMap, BTreeSet};

use dravity_core::{BlockRef, PieceLayout};

use crate::bitfield::Bitfield;

/// Download priority of a piece (or, in the session, of a file).
///
/// Ordered from lowest to highest. [`Priority::Skip`] pieces are not
/// downloaded at all (unless urgent); for the others, any piece of a higher
/// priority is started before any piece of a lower one, and rarest-first
/// applies within one priority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Priority {
    /// Do not download.
    Skip,
    /// Download after everything else.
    Low,
    /// The default.
    #[default]
    Normal,
    /// Download before normal and low pieces.
    High,
}

impl Priority {
    /// All priorities, lowest first.
    pub const ALL: [Self; 4] = [Self::Skip, Self::Low, Self::Normal, Self::High];

    /// Whether pieces of this priority are downloaded.
    #[must_use]
    pub const fn is_wanted(self) -> bool {
        !matches!(self, Self::Skip)
    }

    /// Sort rank: 0 for the highest priority.
    const fn rank(self) -> u8 {
        match self {
            Self::High => 0,
            Self::Normal => 1,
            Self::Low => 2,
            Self::Skip => 3,
        }
    }

    /// A short lowercase name (`skip`, `low`, `normal`, `high`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error parsing a [`Priority`] from text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown priority `{0}` (expected skip, low, normal or high)")]
pub struct ParsePriorityError(String);

impl std::str::FromStr for Priority {
    type Err = ParsePriorityError;

    /// Parses `skip`/`off`/`0`, `low`/`1`, `normal`/`2`, `high`/`3`
    /// (case-insensitive).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "skip" | "off" | "0" => Ok(Self::Skip),
            "low" | "1" => Ok(Self::Low),
            "normal" | "2" => Ok(Self::Normal),
            "high" | "3" => Ok(Self::High),
            _ => Err(ParsePriorityError(s.to_owned())),
        }
    }
}

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
    /// Blocks in [`BlockState::Missing`].
    missing: usize,
    hashing: bool,
}

impl Partial {
    fn new(blocks: u32) -> Self {
        let n = usize::try_from(blocks).unwrap_or(0);
        Self {
            blocks: vec![BlockState::Missing; n],
            written: 0,
            missing: n,
            hashing: false,
        }
    }

    fn reset(&mut self) {
        self.blocks.fill(BlockState::Missing);
        self.written = 0;
        self.missing = self.blocks.len();
        self.hashing = false;
    }

    /// Moves block `idx` from `from` to `to`; returns whether it was in `from`.
    fn transition(&mut self, idx: usize, from: BlockState, to: BlockState) -> bool {
        match self.blocks.get_mut(idx) {
            Some(s) if *s == from => {
                *s = to;
                if from == BlockState::Missing {
                    self.missing -= 1;
                }
                if to == BlockState::Missing {
                    self.missing += 1;
                }
                true
            }
            _ => false,
        }
    }

    /// The first missing block, marked requested.
    fn take_missing(&mut self) -> Option<usize> {
        if self.hashing || self.missing == 0 {
            return None;
        }
        let b = self.blocks.iter().position(|s| *s == BlockState::Missing)?;
        self.transition(b, BlockState::Missing, BlockState::Requested);
        Some(b)
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

/// Rarest-first piece and block picker with priorities.
#[derive(Debug, Clone)]
pub struct Picker {
    layout: PieceLayout,
    have: Bitfield,
    availability: Vec<u32>,
    priority: Vec<Priority>,
    /// Wanted, not yet started pieces ordered by
    /// `(priority rank, availability, index)`.
    queue: BTreeSet<(u8, u32, u32)>,
    partial: BTreeMap<u32, Partial>,
    /// Pieces to fetch before anything else, in this order. Never contains
    /// a piece we have.
    urgent: Vec<u32>,
    /// Pieces with [`Priority::Skip`].
    skipped: u32,
    /// Pieces that are wanted (not skipped) and missing.
    wanted_missing: u32,
    /// Bytes of the pieces in `wanted_missing`.
    wanted_left: u64,
}

impl Picker {
    /// A picker for `layout` that already has the pieces in `have`. Every
    /// piece starts at [`Priority::Normal`].
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
        let rank = Priority::Normal.rank();
        let queue = (0..count)
            .filter(|&p| !have.get(p))
            .map(|p| (rank, 0, p))
            .collect();
        let wanted_left = (0..count)
            .filter(|&p| !have.get(p))
            .map(|p| u64::from(layout.piece_size(p).unwrap_or(0)))
            .sum();
        Self {
            layout,
            wanted_missing: count - have.count(),
            have,
            availability: vec![0; usize::try_from(count).unwrap_or(0)],
            priority: vec![Priority::Normal; usize::try_from(count).unwrap_or(0)],
            queue,
            partial: BTreeMap::new(),
            urgent: Vec::new(),
            skipped: 0,
            wanted_left,
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

    /// Whether every wanted piece (not skipped, or urgent) is verified.
    /// Equal to [`Self::is_complete`] when nothing is skipped.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.wanted_missing == 0 && self.urgent.is_empty()
    }

    /// Bytes of wanted pieces we do not have yet.
    #[must_use]
    pub const fn wanted_left(&self) -> u64 {
        self.wanted_left
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

    /// Priority of `piece` ([`Priority::Skip`] if out of range).
    #[must_use]
    pub fn priority(&self, piece: u32) -> Priority {
        usize::try_from(piece)
            .ok()
            .and_then(|i| self.priority.get(i))
            .copied()
            .unwrap_or(Priority::Skip)
    }

    /// Pieces marked urgent and not yet verified, in pick order.
    #[must_use]
    pub fn urgent(&self) -> &[u32] {
        &self.urgent
    }

    /// Whether `piece` is missing and should be downloaded.
    #[must_use]
    pub fn is_wanted(&self, piece: u32) -> bool {
        piece < self.layout.piece_count()
            && !self.have.get(piece)
            && (self.priority(piece).is_wanted() || self.urgent.contains(&piece))
    }

    /// Sets the priority of one piece. Pieces already started stay started;
    /// a skipped partial piece is simply not continued.
    pub fn set_priority(&mut self, piece: u32, priority: Priority) {
        let Some(slot) = usize::try_from(piece)
            .ok()
            .and_then(|i| self.priority.get_mut(i))
        else {
            return;
        };
        let old = *slot;
        if old == priority {
            return;
        }
        *slot = priority;
        if old == Priority::Skip {
            self.skipped -= 1;
        }
        if priority == Priority::Skip {
            self.skipped += 1;
        }
        if self.have.get(piece) {
            return;
        }
        let size = u64::from(self.layout.piece_size(piece).unwrap_or(0));
        match (old.is_wanted(), priority.is_wanted()) {
            (true, false) => {
                self.wanted_missing -= 1;
                self.wanted_left = self.wanted_left.saturating_sub(size);
            }
            (false, true) => {
                self.wanted_missing += 1;
                self.wanted_left = self.wanted_left.saturating_add(size);
            }
            _ => {}
        }
        if self.partial.contains_key(&piece) {
            return;
        }
        let avail = self.availability(piece);
        self.queue.remove(&(old.rank(), avail, piece));
        if priority.is_wanted() {
            self.queue.insert((priority.rank(), avail, piece));
        }
    }

    /// Sets every piece's priority. Returns `false` (and changes nothing)
    /// when `priorities` does not have one entry per piece.
    pub fn set_priorities(&mut self, priorities: &[Priority]) -> bool {
        if priorities.len() != self.priority.len() {
            return false;
        }
        for (piece, &priority) in (0u32..).zip(priorities) {
            self.set_priority(piece, priority);
        }
        true
    }

    /// Replaces the urgent list: these pieces are picked first, in order,
    /// even when skipped. Out-of-range, duplicate and verified pieces are
    /// dropped.
    pub fn set_urgent(&mut self, pieces: impl IntoIterator<Item = u32>) {
        let count = self.layout.piece_count();
        self.urgent.clear();
        for piece in pieces {
            if piece < count && !self.have.get(piece) && !self.urgent.contains(&piece) {
                self.urgent.push(piece);
            }
        }
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
        let rank = self.priority(piece).rank();
        if self.queue.remove(&(rank, old, piece)) {
            self.queue.insert((rank, new, piece));
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
        if self.skipped == 0 {
            return pieces.has_any_missing_from(&self.have);
        }
        self.urgent.iter().any(|&p| pieces.get(p))
            || pieces
                .ones()
                .any(|p| !self.have.get(p) && self.priority(p).is_wanted())
    }

    /// Whether a started piece should be continued.
    fn continues(&self, piece: u32) -> bool {
        self.priority(piece).is_wanted() || self.urgent.contains(&piece)
    }

    /// Whether some wanted block is neither requested nor received: a new
    /// piece can be started or a partial piece has a missing block. When
    /// this is `false` the download is in its endgame.
    #[must_use]
    pub fn has_unrequested(&self) -> bool {
        !self.queue.is_empty()
            || self.urgent.iter().any(|p| !self.partial.contains_key(p))
            || self
                .partial
                .iter()
                .any(|(&i, p)| !p.hashing && p.missing > 0 && self.continues(i))
    }

    /// Blocks that are requested but not yet received, in wanted pieces the
    /// peer with `pieces` has. Used to request them again from another peer.
    pub fn requested_blocks<'a>(
        &'a self,
        pieces: &'a Bitfield,
    ) -> impl Iterator<Item = BlockRef> + 'a {
        self.partial
            .iter()
            .filter(|(index, p)| !p.hashing && pieces.get(**index) && self.continues(**index))
            .flat_map(move |(&index, p)| {
                p.blocks
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| **s == BlockState::Requested)
                    .filter_map(move |(b, _)| self.layout.block(index, u32::try_from(b).ok()?))
            })
    }

    /// Picks the next block to request from a peer with `pieces`, marking it
    /// requested. Urgent pieces come first, then unfinished pieces, then the
    /// best new piece the peer has: highest priority, then rarest (ties
    /// broken by lowest index).
    pub fn pick(&mut self, pieces: &Bitfield) -> Option<BlockRef> {
        self.pick_with_hint(pieces, &[])
    }

    /// Like [`Self::pick`], but a new piece in `hint` (e.g. the peer's BEP 6
    /// `suggest`s) is preferred over rarest-first when its priority is at
    /// least that of the piece rarest-first would start.
    pub fn pick_with_hint(&mut self, pieces: &Bitfield, hint: &[u32]) -> Option<BlockRef> {
        if let Some(block) = self.pick_urgent(pieces) {
            return Some(block);
        }
        if let Some(block) = self.pick_from_partial(pieces) {
            return Some(block);
        }
        let best = self
            .queue
            .iter()
            .find(|&&(_, _, p)| pieces.get(p))
            .copied()?;
        let chosen = hint
            .iter()
            .filter(|&&p| pieces.get(p))
            .map(|&p| (self.priority(p).rank(), self.availability(p), p))
            .find(|key| key.0 <= best.0 && self.queue.contains(key))
            .unwrap_or(best);
        self.start(chosen.2)
    }

    /// Starts `piece` (removing it from the queue) and returns its first
    /// block.
    fn start(&mut self, piece: u32) -> Option<BlockRef> {
        let key = (self.priority(piece).rank(), self.availability(piece), piece);
        self.queue.remove(&key);
        let blocks = self.layout.block_count(piece)?;
        let partial = self
            .partial
            .entry(piece)
            .or_insert_with(|| Partial::new(blocks));
        let b = partial.take_missing()?;
        self.layout.block(piece, u32::try_from(b).ok()?)
    }

    fn pick_urgent(&mut self, pieces: &Bitfield) -> Option<BlockRef> {
        let piece = self.urgent.iter().copied().find(|&p| {
            pieces.get(p)
                && self
                    .partial
                    .get(&p)
                    .is_none_or(|partial| !partial.hashing && partial.missing > 0)
        })?;
        self.start(piece)
    }

    fn pick_from_partial(&mut self, pieces: &Bitfield) -> Option<BlockRef> {
        let index = self
            .partial
            .iter()
            .find(|&(&index, partial)| {
                !partial.hashing
                    && partial.missing > 0
                    && pieces.get(index)
                    && self.continues(index)
            })
            .map(|(&index, _)| index)?;
        let partial = self.partial.get_mut(&index)?;
        let b = partial.take_missing()?;
        self.layout.block(index, u32::try_from(b).ok()?)
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
            partial.transition(idx, BlockState::Requested, BlockState::Missing);
        }
    }

    /// Records that `block`'s data arrived. Late data for a block that timed
    /// out (and is missing again) is accepted too.
    pub fn block_received(&mut self, block: BlockRef) -> Received {
        let Some((partial, idx)) = self.block_state(block) else {
            return Received::Ignored;
        };
        if partial.transition(idx, BlockState::Requested, BlockState::Received)
            || partial.transition(idx, BlockState::Missing, BlockState::Received)
        {
            Received::Accepted
        } else {
            Received::Ignored
        }
    }

    /// Records that `block` was written to storage. Returns the piece index if
    /// the piece is now complete and should be hashed.
    pub fn block_written(&mut self, block: BlockRef) -> Option<u32> {
        let (partial, idx) = self.block_state(block)?;
        if !partial.transition(idx, BlockState::Received, BlockState::Written) {
            return None;
        }
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
            partial.transition(idx, BlockState::Received, BlockState::Missing);
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
        self.urgent.retain(|&p| p != piece);
        let priority = self.priority(piece);
        let avail = self.availability(piece);
        self.queue.remove(&(priority.rank(), avail, piece));
        if priority.is_wanted() {
            self.wanted_missing -= 1;
            let size = u64::from(self.layout.piece_size(piece).unwrap_or(0));
            self.wanted_left = self.wanted_left.saturating_sub(size);
        }
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
    fn endgame_lists_requested_blocks() {
        let mut p = Picker::new(layout(2, 2 * BLOCK_SIZE), Bitfield::new(2));
        let all = Bitfield::full(2);
        let mut only_1 = Bitfield::new(2);
        only_1.set(1);
        p.add_peer_pieces(&all);
        let picked: Vec<BlockRef> = std::iter::from_fn(|| p.pick(&all)).collect();
        assert_eq!(picked.len(), 4);
        assert!(!p.has_unrequested());
        assert_eq!(p.requested_blocks(&all).collect::<Vec<_>>(), picked);
        assert_eq!(p.requested_blocks(&only_1).count(), 2);
        p.block_received(picked[0]);
        assert_eq!(p.requested_blocks(&all).count(), 3);
        p.release(picked[1]);
        assert!(p.has_unrequested());
        assert_eq!(p.pick(&all), Some(picked[1]));
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
    #[test]
    fn higher_priority_beats_rarity() {
        let mut p = Picker::new(layout(3, BLOCK_SIZE), Bitfield::new(3));
        let all = Bitfield::full(3);
        let mut only_0 = Bitfield::new(3);
        only_0.set(0);
        p.add_peer_pieces(&all);
        p.add_peer_pieces(&all);
        p.add_peer_pieces(&only_0);
        p.set_priority(1, Priority::Low);
        p.set_priority(2, Priority::High);
        // 0 is commonest, 2 is high: high wins, then normal, then low.
        let order: Vec<u32> = std::iter::from_fn(|| p.pick(&all).map(|b| b.piece)).collect();
        assert_eq!(order, vec![2, 0, 1]);
    }

    #[test]
    fn skipped_pieces_are_not_picked_and_done_ignores_them() {
        let mut p = Picker::new(layout(3, BLOCK_SIZE), Bitfield::new(3));
        let all = Bitfield::full(3);
        assert!(p.set_priorities(&[Priority::Skip, Priority::Normal, Priority::Skip]));
        assert!(!p.set_priorities(&[Priority::Skip]), "wrong length");
        assert_eq!(p.wanted_left(), u64::from(BLOCK_SIZE));
        let mut only_0 = Bitfield::new(3);
        only_0.set(0);
        assert!(!p.is_interesting(&only_0));
        assert!(p.is_interesting(&all));
        let a = p.pick(&all).unwrap();
        assert_eq!(a.piece, 1);
        assert_eq!(p.pick(&all), None);
        p.block_received(a);
        assert_eq!(p.block_written(a), Some(1));
        assert!(!p.is_done());
        assert!(p.piece_hashed(1, true));
        assert!(p.is_done());
        assert!(!p.is_complete());
        assert_eq!(p.wanted_left(), 0);
        // Wanting a skipped piece again re-opens the download.
        p.set_priority(2, Priority::Low);
        assert!(!p.is_done());
        assert_eq!(p.pick(&all).map(|b| b.piece), Some(2));
    }

    #[test]
    fn skipping_a_partial_piece_pauses_it() {
        let mut p = Picker::new(layout(2, 2 * BLOCK_SIZE), Bitfield::new(2));
        let all = Bitfield::full(2);
        let a = p.pick(&all).unwrap();
        assert_eq!(a.piece, 0);
        p.set_priority(0, Priority::Skip);
        assert_eq!(p.pick(&all).map(|b| b.piece), Some(1));
        assert_eq!(p.requested_blocks(&all).filter(|b| b.piece == 0).count(), 0);
        p.set_priority(0, Priority::Normal);
        let next = p.pick(&all).unwrap();
        assert_eq!((next.piece, next.offset), (0, BLOCK_SIZE), "continued");
    }

    #[test]
    fn urgent_pieces_come_first_in_order() {
        let mut p = Picker::new(layout(5, BLOCK_SIZE), Bitfield::new(5));
        let all = Bitfield::full(5);
        p.set_priority(4, Priority::Skip);
        p.set_urgent([4, 3, 3, 9]);
        assert_eq!(p.urgent(), &[4, 3]);
        assert!(p.is_wanted(4), "urgent overrides skip");
        let mut only_0 = Bitfield::new(5);
        only_0.set(4);
        assert!(p.is_interesting(&only_0));
        let first = p.pick(&all).unwrap();
        let second = p.pick(&all).unwrap();
        assert_eq!((first.piece, second.piece), (4, 3));
        p.block_received(first);
        p.block_written(first);
        assert!(p.piece_hashed(4, true));
        assert_eq!(p.urgent(), &[3]);
        assert_eq!(p.pick(&all).map(|b| b.piece), Some(0));
    }

    #[test]
    fn hints_win_within_the_same_priority() {
        let mut p = Picker::new(layout(4, BLOCK_SIZE), Bitfield::new(4));
        let all = Bitfield::full(4);
        p.add_peer_pieces(&all);
        let mut rare = Bitfield::new(4);
        rare.set(0);
        p.remove_peer_pieces(&rare);
        // Rarest-first would start 0; the hint asks for 3.
        assert_eq!(p.pick_with_hint(&all, &[3]).map(|b| b.piece), Some(3));
        p.set_priority(1, Priority::High);
        // A hint never beats a higher priority.
        assert_eq!(p.pick_with_hint(&all, &[2]).map(|b| b.piece), Some(1));
        // Hints the peer lacks or that already started are ignored.
        assert_eq!(p.pick_with_hint(&rare, &[2, 3]).map(|b| b.piece), Some(0));
    }

    #[test]
    fn priorities_parse_and_print() {
        for prio in Priority::ALL {
            assert_eq!(prio.as_str().parse::<Priority>(), Ok(prio));
        }
        assert_eq!("OFF".parse::<Priority>(), Ok(Priority::Skip));
        assert_eq!("3".parse::<Priority>(), Ok(Priority::High));
        assert!("urgent".parse::<Priority>().is_err());
        assert!(Priority::High > Priority::Normal && Priority::Low > Priority::Skip);
        assert_eq!(Priority::default(), Priority::Normal);
    }
}
