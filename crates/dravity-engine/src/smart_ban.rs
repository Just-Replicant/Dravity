//! Smart banning: finding *which* contributor of a failed piece sent the
//! bad data instead of striking every contributor.
//!
//! When a piece with several contributors fails its hash check, the driver
//! hashes each of its blocks as they are on disk ([`Effect::HashBlocks`],
//! answered by [`Event::BlockHashes`]) before the piece is downloaded again;
//! every block's digest is remembered with the address that sent it. When
//! the piece later passes, its blocks are hashed once more: a remembered
//! digest that differs from the verified block's convicts its sender, who is
//! banned at once; a matching digest clears its sender. Honest peers that
//! merely shared a piece with a poisoner are never struck.
//!
//! For v2 and hybrid torrents the verdict can come before the piece is
//! downloaded again: the 16 KiB leaf hashes of the failed piece are asked
//! from a BEP 52 peer, checked against the piece-layer hash we already
//! trust, and compared with the remembered digests (digests are SHA-256 leaf
//! hashes, [`merkle::hash_leaf`], so both checks use the same values).
//!
//! The failed piece is not handed back to the picker until its digests are
//! in, so no new block can overwrite the evidence while it is read.
//!
//! [`Effect::HashBlocks`]: crate::Effect::HashBlocks
//! [`Event::BlockHashes`]: crate::Event::BlockHashes
//! [`merkle::hash_leaf`]: dravity_core::merkle::hash_leaf

use std::collections::HashMap;
use std::net::IpAddr;

use dravity_core::BLOCK_SIZE;
use dravity_core::merkle::Hash;

/// Most failed pieces tracked at once; beyond this, failures fall back to
/// strikes.
pub(crate) const MAX_TRACKED_PIECES: usize = 256;

/// One remembered block of a failed piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Suspect {
    offset: u32,
    ip: IpAddr,
    digest: Hash,
}

/// What a piece's [`Event::BlockHashes`](crate::Event::BlockHashes) answers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Waiting {
    /// The digests of the bad data: who sent which block.
    Failed(HashMap<u32, IpAddr>),
    /// The digests of the verified data.
    Passed,
}

/// The result of a piece's block digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Digests of a failed piece were recorded. The piece may be downloaded
    /// again; `banned` is non-empty when verified leaves were already known.
    Recorded { banned: Vec<IpAddr> },
    /// The failed piece could not be hashed: fall back to strikes against
    /// these senders. The piece may be downloaded again.
    Unhashed { senders: HashMap<u32, IpAddr> },
    /// The verified piece was compared with the remembered blocks.
    Compared { banned: Vec<IpAddr> },
    /// Nothing was waiting for these digests.
    Ignored,
}

/// Remembered blocks of failed pieces.
#[derive(Debug, Default, Clone)]
pub(crate) struct SmartBan {
    suspects: HashMap<u32, Vec<Suspect>>,
    waiting: HashMap<u32, Waiting>,
    /// Verified leaf hashes of failed pieces, from BEP 52 peers.
    leaves: HashMap<u32, Vec<Hash>>,
}

impl SmartBan {
    /// Whether another failed piece can be tracked.
    pub(crate) fn has_room(&self) -> bool {
        self.suspects.len() + self.waiting.len() < MAX_TRACKED_PIECES
    }

    /// Whether `piece` failed and still waits for its digests (it must not
    /// be downloaded again yet).
    pub(crate) fn is_quarantined(&self, piece: u32) -> bool {
        matches!(self.waiting.get(&piece), Some(Waiting::Failed(_)))
    }

    /// `piece` failed with these per-block senders: its blocks must be
    /// hashed ([`Effect::HashBlocks`](crate::Effect::HashBlocks)).
    pub(crate) fn failed(&mut self, piece: u32, senders: HashMap<u32, IpAddr>) {
        self.waiting.insert(piece, Waiting::Failed(senders));
    }

    /// `piece` passed. Returns whether it has remembered blocks, in which
    /// case its blocks must be hashed to compare them.
    pub(crate) fn passed(&mut self, piece: u32) -> bool {
        self.leaves.remove(&piece);
        if self.suspects.contains_key(&piece) {
            self.waiting.insert(piece, Waiting::Passed);
            true
        } else {
            false
        }
    }

    /// The block digests of `piece` (one per 16 KiB block, in order; empty
    /// when the driver could not read it).
    pub(crate) fn on_digests(&mut self, piece: u32, digests: &[Hash]) -> Verdict {
        match self.waiting.remove(&piece) {
            None => Verdict::Ignored,
            Some(Waiting::Failed(senders)) => {
                if digests.is_empty() {
                    return Verdict::Unhashed { senders };
                }
                let entry = self.suspects.entry(piece).or_default();
                for (offset, ip) in senders {
                    let Some(&digest) = usize::try_from(offset / BLOCK_SIZE)
                        .ok()
                        .and_then(|i| digests.get(i))
                    else {
                        continue;
                    };
                    let suspect = Suspect { offset, ip, digest };
                    if !entry.contains(&suspect) {
                        entry.push(suspect);
                    }
                }
                let banned = match self.leaves.get(&piece) {
                    Some(leaves) => self.convict(piece, leaves.clone().as_slice(), false),
                    None => Vec::new(),
                };
                Verdict::Recorded { banned }
            }
            Some(Waiting::Passed) => {
                if digests.is_empty() {
                    return Verdict::Compared { banned: Vec::new() };
                }
                Verdict::Compared {
                    banned: self.convict(piece, digests, true),
                }
            }
        }
    }

    /// Verified leaf hashes of a failed `piece` arrived from a BEP 52 peer.
    /// Returns the addresses they convict (none yet if the piece's digests
    /// are still being read).
    pub(crate) fn on_leaves(&mut self, piece: u32, leaves: Vec<Hash>) -> Vec<IpAddr> {
        if self.suspects.contains_key(&piece) {
            let banned = self.convict(piece, &leaves, false);
            self.leaves.insert(piece, leaves);
            banned
        } else if self.is_quarantined(piece) {
            self.leaves.insert(piece, leaves);
            Vec::new()
        } else {
            Vec::new()
        }
    }

    /// Compares the remembered blocks of `piece` with `good` (verified
    /// digests per block). Senders of differing blocks are returned, sorted
    /// and deduplicated; their entries and those of cleared senders are
    /// dropped. With `forget`, everything about the piece is dropped.
    fn convict(&mut self, piece: u32, good: &[Hash], forget: bool) -> Vec<IpAddr> {
        let mut banned = Vec::new();
        if let Some(list) = self.suspects.get_mut(&piece) {
            list.retain(|s| {
                let Some(expected) = usize::try_from(s.offset / BLOCK_SIZE)
                    .ok()
                    .and_then(|i| good.get(i))
                else {
                    return !forget;
                };
                if *expected != s.digest {
                    banned.push(s.ip);
                }
                false
            });
            if forget || list.is_empty() {
                self.suspects.remove(&piece);
            }
        }
        banned.sort_unstable();
        banned.dedup();
        banned
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    fn h(n: u8) -> Hash {
        [n; 32]
    }

    fn senders(list: &[(u32, u8)]) -> HashMap<u32, IpAddr> {
        list.iter().map(|&(i, n)| (i * BLOCK_SIZE, ip(n))).collect()
    }

    #[test]
    fn the_sender_of_the_differing_block_is_convicted() {
        let mut s = SmartBan::default();
        s.failed(3, senders(&[(0, 1), (1, 2), (2, 1)]));
        assert!(s.is_quarantined(3));
        // Block 1 (from ip 2) is bad.
        assert_eq!(
            s.on_digests(3, &[h(10), h(99), h(12)]),
            Verdict::Recorded { banned: vec![] }
        );
        assert!(!s.is_quarantined(3));
        assert!(s.passed(3));
        assert_eq!(
            s.on_digests(3, &[h(10), h(11), h(12)]),
            Verdict::Compared {
                banned: vec![ip(2)]
            }
        );
        assert!(!s.passed(3), "forgotten after the verdict");
    }

    #[test]
    fn verified_leaves_convict_before_the_piece_passes() {
        let mut s = SmartBan::default();
        s.failed(0, senders(&[(0, 1), (1, 2)]));
        // Leaves first, digests later.
        assert!(s.on_leaves(0, vec![h(1), h(2)]).is_empty());
        assert_eq!(
            s.on_digests(0, &[h(1), h(7)]),
            Verdict::Recorded {
                banned: vec![ip(2)]
            }
        );
        // Digests first, leaves later.
        s.failed(1, senders(&[(0, 3), (1, 4)]));
        s.on_digests(1, &[h(5), h(6)]);
        assert_eq!(s.on_leaves(1, vec![h(0), h(6)]), vec![ip(3)]);
    }

    #[test]
    fn unreadable_failed_piece_falls_back_to_strikes() {
        let mut s = SmartBan::default();
        let who = senders(&[(0, 1), (1, 2)]);
        s.failed(5, who.clone());
        assert_eq!(s.on_digests(5, &[]), Verdict::Unhashed { senders: who });
        assert_eq!(s.on_digests(5, &[h(1)]), Verdict::Ignored);
        assert!(!s.passed(5));
    }

    #[test]
    fn repeated_failures_accumulate_and_room_is_bounded() {
        let mut s = SmartBan::default();
        s.failed(0, senders(&[(0, 1)]));
        s.on_digests(0, &[h(1)]);
        s.failed(0, senders(&[(0, 2)]));
        s.on_digests(0, &[h(2)]);
        assert!(s.passed(0));
        assert_eq!(
            s.on_digests(0, &[h(3)]),
            Verdict::Compared {
                banned: vec![ip(1), ip(2)]
            }
        );
        for piece in 0..u32::try_from(MAX_TRACKED_PIECES).unwrap() {
            assert!(s.has_room());
            s.failed(piece, HashMap::new());
        }
        assert!(!s.has_room());
    }
}
