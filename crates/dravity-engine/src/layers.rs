//! Fetching BEP 52 piece layers with hash requests, shared by a magnet's
//! [`MetadataState`](crate::MetadataState) (before the download) and a
//! hybrid's [`TorrentState`](crate::TorrentState) (in the background, after
//! `piece_layer_timeout`).
//!
//! Each missing layer is split into requests of at most
//! [`merkle::MAX_HASH_REQUEST`] hashes with the uncle hashes up to the
//! file's `pieces root`; every answer is checked against the root before it
//! is kept.

use std::time::Instant;

use dravity_core::merkle::{self, Hash, LayerRequest};
use dravity_wire::HashRequest;

use crate::types::PeerKey;

/// State of one piece of something fetched in parts from peers (metadata
/// pieces, layer blocks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Piece {
    /// Not downloaded. `avoid` is a peer that just failed this piece; other
    /// peers may take it immediately, and that peer only after the instant.
    Missing {
        avoid: Option<(PeerKey, Instant)>,
    },
    Requested {
        peer: PeerKey,
        since: Instant,
    },
    Have,
}

impl Piece {
    pub(crate) const fn is_have(self) -> bool {
        matches!(self, Self::Have)
    }

    pub(crate) const fn is_requested(self) -> bool {
        matches!(self, Self::Requested { .. })
    }

    /// Whether `key` may be asked for this piece at `now`.
    pub(crate) fn can_ask(self, key: PeerKey, now: Instant) -> bool {
        match self {
            Self::Missing { avoid: None } => true,
            Self::Missing {
                avoid: Some((peer, at)),
            } => peer != key || now >= at,
            _ => false,
        }
    }

    /// A request by `key` that will not be answered: free it.
    fn release(&mut self, key: PeerKey) {
        if matches!(*self, Self::Requested { peer, .. } if peer == key) {
            *self = Self::Missing { avoid: None };
        }
    }
}

/// One hash request of a piece-layer fetch.
#[derive(Debug, Clone)]
struct LayerBlock {
    root: Hash,
    request: LayerRequest,
    state: Piece,
}

/// What a `hashes` reply did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Not a request outstanding with this peer: ignore it.
    Unexpected,
    /// The proof does not reach the pieces root: the peer lies.
    Invalid,
    /// Kept. `completed` is the layer this reply finished, if any.
    Accepted {
        completed: Option<(Hash, Vec<Hash>)>,
    },
}

/// Missing piece layers being fetched.
#[derive(Debug)]
pub(crate) struct LayerSet {
    /// Layer above the leaves that holds one hash per piece.
    base_layer: u32,
    blocks: Vec<LayerBlock>,
    /// Layers being assembled, by root (one hash per piece).
    layers: Vec<(Hash, Vec<Hash>)>,
}

impl LayerSet {
    /// A fetch of `roots` (each with its number of pieces) for a torrent
    /// with `piece_length`; `None` when there is nothing to fetch.
    pub(crate) fn new(
        piece_length: u32,
        roots: impl IntoIterator<Item = (Hash, usize)>,
    ) -> Option<Self> {
        let per_piece = merkle::leaves_per_piece(piece_length)?;
        let mut blocks = Vec::new();
        let mut layers = Vec::new();
        for (root, pieces) in roots {
            for request in merkle::layer_requests(pieces) {
                blocks.push(LayerBlock {
                    root,
                    request,
                    state: Piece::Missing { avoid: None },
                });
            }
            layers.push((root, vec![[0; 32]; pieces]));
        }
        (!blocks.is_empty()).then(|| Self {
            base_layer: per_piece.trailing_zeros(),
            blocks,
            layers,
        })
    }

    fn wire(&self, block: &LayerBlock) -> Option<HashRequest> {
        Some(HashRequest {
            pieces_root: block.root,
            base_layer: self.base_layer,
            index: u32::try_from(block.request.index).ok()?,
            length: u32::try_from(block.request.length).ok()?,
            proof_layers: block.request.proof_layers,
        })
    }

    /// The block `request` answers, if it is outstanding with `key`.
    fn find(&self, key: PeerKey, request: &HashRequest) -> Option<usize> {
        self.blocks.iter().position(|b| {
            matches!(b.state, Piece::Requested { peer, .. } if peer == key)
                && self.wire(b).is_some_and(|w| {
                    w.pieces_root == request.pieces_root
                        && w.base_layer == request.base_layer
                        && w.index == request.index
                        && w.length == request.length
                })
        })
    }

    /// Assigns blocks to `key` until it has `limit` outstanding; returns
    /// the requests to send.
    pub(crate) fn assign(&mut self, key: PeerKey, now: Instant, limit: usize) -> Vec<HashRequest> {
        let mut outstanding = self
            .blocks
            .iter()
            .filter(|b| matches!(b.state, Piece::Requested { peer, .. } if peer == key))
            .count();
        let mut out = Vec::new();
        for i in 0..self.blocks.len() {
            if outstanding >= limit {
                break;
            }
            let Some(block) = self.blocks.get(i) else {
                break;
            };
            if !block.state.can_ask(key, now) {
                continue;
            }
            let Some(request) = self.wire(block) else {
                continue;
            };
            if let Some(b) = self.blocks.get_mut(i) {
                b.state = Piece::Requested {
                    peer: key,
                    since: now,
                };
            }
            outstanding += 1;
            out.push(request);
        }
        out
    }

    /// A `hashes` reply from `key`: checked against the pieces root with
    /// its uncle hashes.
    pub(crate) fn on_hashes(
        &mut self,
        key: PeerKey,
        request: &HashRequest,
        hashes: &[u8],
    ) -> Answer {
        let Some(i) = self.find(key, request) else {
            return Answer::Unexpected;
        };
        let Some(block) = self.blocks.get(i).cloned() else {
            return Answer::Unexpected;
        };
        let (chunks, rest) = hashes.as_chunks::<32>();
        let length = block.request.length;
        let valid = rest.is_empty()
            && chunks.len() >= length
            && chunks
                .get(..length)
                .zip(chunks.get(length..))
                .is_some_and(|(base, uncles)| {
                    merkle::verify_proof(base, uncles, block.request.index, &block.root)
                });
        if !valid {
            if let Some(b) = self.blocks.get_mut(i) {
                b.state = Piece::Missing { avoid: None };
            }
            return Answer::Invalid;
        }
        if let Some((_, layer)) = self.layers.iter_mut().find(|(r, _)| *r == block.root) {
            let start = block.request.index;
            let n = length.min(layer.len().saturating_sub(start));
            if let (Some(dst), Some(src)) = (layer.get_mut(start..start + n), chunks.get(..n)) {
                dst.copy_from_slice(src);
            }
        }
        if let Some(b) = self.blocks.get_mut(i) {
            b.state = Piece::Have;
        }
        let completed = self
            .is_complete(&block.root)
            .then(|| self.layers.iter().find(|(r, _)| *r == block.root).cloned());
        Answer::Accepted {
            completed: completed.flatten(),
        }
    }

    /// A `hash reject` from `key`: the block goes to other peers, and back
    /// to `key` only after `retry_at`.
    pub(crate) fn on_reject(&mut self, key: PeerKey, request: &HashRequest, retry_at: Instant) {
        if let Some(i) = self.find(key, request)
            && let Some(block) = self.blocks.get_mut(i)
        {
            block.state = Piece::Missing {
                avoid: Some((key, retry_at)),
            };
        }
    }

    /// `key` is gone: its requests go to other peers.
    pub(crate) fn release(&mut self, key: PeerKey) {
        for block in &mut self.blocks {
            block.state.release(key);
        }
    }

    /// Requests older than `timeout` go to other peers; the peer that
    /// stalled waits until `now + timeout`.
    pub(crate) fn expire(&mut self, now: Instant, timeout: std::time::Duration) {
        for block in &mut self.blocks {
            if let Piece::Requested { peer, since } = block.state
                && now.saturating_duration_since(since) >= timeout
            {
                block.state = Piece::Missing {
                    avoid: Some((peer, now + timeout)),
                };
            }
        }
    }

    fn is_complete(&self, root: &Hash) -> bool {
        self.blocks
            .iter()
            .filter(|b| b.root == *root)
            .all(|b| b.state.is_have())
    }

    /// Whether every layer is complete.
    pub(crate) fn is_done(&self) -> bool {
        self.blocks.iter().all(|b| b.state.is_have())
    }

    /// Layers with at least one block still missing.
    pub(crate) fn missing_layers(&self) -> u32 {
        let missing = self.layers.iter().filter(|(r, _)| !self.is_complete(r));
        u32::try_from(missing.count()).unwrap_or(u32::MAX)
    }

    /// The layers whose every block arrived (all of them once
    /// [`Self::is_done`]).
    pub(crate) fn into_complete(self) -> Vec<(Hash, Vec<Hash>)> {
        let done: Vec<bool> = self
            .layers
            .iter()
            .map(|(r, _)| self.is_complete(r))
            .collect();
        self.layers
            .into_iter()
            .zip(done)
            .filter_map(|(layer, done)| done.then_some(layer))
            .collect()
    }

    /// Drops layers already known (complete ones, reported before).
    pub(crate) fn retain_incomplete(&mut self) {
        let done: Vec<Hash> = self
            .layers
            .iter()
            .map(|(r, _)| *r)
            .filter(|r| self.is_complete(r))
            .collect();
        self.layers.retain(|(r, _)| !done.contains(r));
        self.blocks.retain(|b| !done.contains(&b.root));
    }
}
