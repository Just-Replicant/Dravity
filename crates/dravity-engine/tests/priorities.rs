//! Piece priorities, urgent (streaming) pieces and BEP 6 suggestions.

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation, clippy::panic)]

use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use dravity_core::{BLOCK_SIZE, BlockRef, PieceLayout};
use dravity_engine::{
    Alert, AnnounceEvent, Bitfield, Config, Effect, Event, PeerKey, Priority, TorrentState,
    TorrentStatus,
};
use dravity_wire::{BlockRequest, Message, Reserved};
use proptest::prelude::*;

const PIECES: u32 = 4;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 2], 7000 + n))
}

fn config() -> Config {
    let mut c = Config::default();
    c.check_existing = false;
    c.max_requests_per_peer = 2;
    c.initial_requests_per_peer = 2;
    c
}

/// Four pieces of two blocks.
fn layout() -> PieceLayout {
    PieceLayout::new(2 * BLOCK_SIZE, u64::from(PIECES * 2 * BLOCK_SIZE)).unwrap()
}

struct Harness {
    t: TorrentState,
    now: Instant,
}

impl Harness {
    fn new(config: Config, priorities: &[Priority]) -> Self {
        Self {
            t: TorrentState::new(layout(), config).with_priorities(priorities),
            now: Instant::now(),
        }
    }

    fn ev(&mut self, e: Event) -> Vec<Effect> {
        self.t.update(self.now, e)
    }

    fn msg(&mut self, peer: u64, message: Message) -> Vec<Effect> {
        self.ev(Event::Message {
            peer: PeerKey(peer),
            message,
        })
    }

    /// Started, with one unchoking seed connected; returns the effects of
    /// the unchoke (the first requests).
    fn with_seed(&mut self, reserved: Reserved) -> Vec<Effect> {
        self.ev(Event::Start);
        self.ev(Event::Announced {
            peers: vec![addr(1)],
            interval: None,
        });
        self.ev(Event::PeerConnected {
            peer: PeerKey(1),
            addr: addr(1),
            reserved,
        });
        self.msg(1, Message::Bitfield(Bitfield::full(PIECES).to_bytes()));
        self.msg(1, Message::Unchoke)
    }

    /// Answers every request in `fx` (and the ones that follow) until no
    /// request is left; returns every effect seen.
    fn serve(&mut self, fx: Vec<Effect>) -> Vec<Effect> {
        let mut all = Vec::new();
        let mut pending: Vec<BlockRequest> = requests(&fx);
        all.extend(fx);
        while let Some(r) = pending.pop() {
            let fx = self.msg(
                1,
                Message::Piece {
                    piece: r.piece,
                    offset: r.offset,
                    data: Bytes::from(vec![1u8; r.length as usize]),
                },
            );
            pending.extend(requests(&fx));
            all.extend(fx);
            let fx = self.ev(Event::BlockWritten {
                block: BlockRef {
                    piece: r.piece,
                    offset: r.offset,
                    length: r.length,
                },
            });
            for e in &fx {
                if let Effect::HashPiece { piece } = e {
                    let fx = self.ev(Event::PieceHashed {
                        piece: *piece,
                        ok: true,
                    });
                    pending.extend(requests(&fx));
                    all.extend(fx);
                }
            }
            pending.extend(requests(&fx));
            all.extend(fx);
        }
        all
    }
}

fn requests(effects: &[Effect]) -> Vec<BlockRequest> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Send {
                message: Message::Request(r),
                ..
            } => Some(*r),
            _ => None,
        })
        .collect()
}

fn requested_pieces(effects: &[Effect]) -> Vec<u32> {
    let mut pieces: Vec<u32> = requests(effects).iter().map(|r| r.piece).collect();
    pieces.dedup();
    pieces
}

fn announces(effects: &[Effect]) -> Vec<(AnnounceEvent, u64)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Announce { event, left, .. } => Some((*event, *left)),
            _ => None,
        })
        .collect()
}

fn finished(effects: &[Effect]) -> bool {
    effects.contains(&Effect::Alert(Alert::Finished))
}

#[test]
fn skipped_pieces_are_never_requested() {
    use Priority::{Normal, Skip};
    let mut h = Harness::new(config(), &[Skip, Normal, Skip, Normal]);
    let fx = h.with_seed(Reserved::new());
    let all = h.serve(fx);
    let mut pieces = requested_pieces(&all);
    pieces.sort_unstable();
    pieces.dedup();
    assert_eq!(pieces, vec![1, 3]);
    assert!(finished(&all));
    let stats = h.t.stats();
    assert_eq!(stats.status, TorrentStatus::Finished);
    assert_eq!(stats.wanted_left, 0);
    assert_eq!(stats.pieces_have, 2);
    // Half of the torrent is still missing: no `completed` announce.
    assert!(
        !announces(&all)
            .iter()
            .any(|(e, _)| *e == AnnounceEvent::Completed)
    );
    // Seeding what we have; the seed is kept (we may want more later), and
    // we are no longer interested in it.
    assert!(!all.contains(&Effect::Disconnect { peer: PeerKey(1) }));
    assert!(all.contains(&Effect::Send {
        peer: PeerKey(1),
        message: Message::NotInterested
    }));
}

#[test]
fn wanting_more_resumes_the_download_and_completes() {
    use Priority::{Normal, Skip};
    let mut h = Harness::new(config(), &[Normal, Skip, Skip, Skip]);
    let fx = h.with_seed(Reserved::new());
    let all = h.serve(fx);
    assert!(finished(&all));
    let fx = h.ev(Event::SetPriorities {
        priorities: vec![Normal; PIECES as usize],
    });
    assert!(fx.contains(&Effect::Alert(Alert::StateChanged(
        TorrentStatus::Downloading
    ))));
    assert!(fx.contains(&Effect::Send {
        peer: PeerKey(1),
        message: Message::Interested
    }));
    assert!(!requests(&fx).is_empty(), "requests go out at once");
    let all = h.serve(fx);
    assert!(finished(&all));
    assert_eq!(
        announces(&all),
        vec![(AnnounceEvent::Completed, 0)],
        "completed once every piece is in"
    );
    // Both complete now: the seed is dropped.
    assert!(all.contains(&Effect::Disconnect { peer: PeerKey(1) }));
}

#[test]
fn skipping_everything_left_finishes_at_once() {
    let mut h = Harness::new(config(), &[]);
    let fx = h.with_seed(Reserved::new());
    assert!(!requests(&fx).is_empty());
    let fx = h.ev(Event::SetPriorities {
        priorities: vec![Priority::Skip; PIECES as usize],
    });
    assert!(finished(&fx));
    assert_eq!(h.t.status(), TorrentStatus::Finished);
    // Wrong length: ignored.
    let fx = h.ev(Event::SetPriorities {
        priorities: vec![Priority::High],
    });
    assert!(fx.is_empty());
}

#[test]
fn without_seeding_a_partial_finish_leaves_and_rejoins_the_swarm() {
    use Priority::{Normal, Skip};
    let mut c = config();
    c.seed = false;
    let mut h = Harness::new(c, &[Normal, Skip, Skip, Skip]);
    let fx = h.with_seed(Reserved::new());
    let all = h.serve(fx);
    assert!(finished(&all));
    let left = layout().total_length() - u64::from(2 * BLOCK_SIZE);
    assert!(announces(&all).contains(&(AnnounceEvent::Stopped, left)));
    assert!(all.contains(&Effect::Disconnect { peer: PeerKey(1) }));
    let fx = h.ev(Event::SetPriorities {
        priorities: vec![Normal; PIECES as usize],
    });
    assert_eq!(announces(&fx), vec![(AnnounceEvent::Started, left)]);
    assert_eq!(h.t.status(), TorrentStatus::Downloading);
}

#[test]
fn high_priority_pieces_come_first() {
    use Priority::{High, Low, Normal};
    let mut h = Harness::new(config(), &[Low, Normal, Low, High]);
    let fx = h.with_seed(Reserved::new());
    let all = h.serve(fx);
    let order = requested_pieces(&all);
    let mut seen = Vec::new();
    for p in order {
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    assert_eq!(seen, vec![3, 1, 0, 2]);
}

#[test]
fn urgent_pieces_are_requested_first_even_when_skipped() {
    let mut h = Harness::new(config(), &[Priority::Skip; PIECES as usize]);
    let fx = h.with_seed(Reserved::new());
    assert_eq!(h.t.status(), TorrentStatus::Finished, "nothing wanted");
    assert!(requests(&fx).is_empty());
    let fx = h.ev(Event::SetUrgent { pieces: vec![2, 0] });
    assert_eq!(h.t.status(), TorrentStatus::Downloading);
    assert_eq!(requested_pieces(&fx), vec![2]);
    let all = h.serve(fx);
    let mut seen = Vec::new();
    for p in requested_pieces(&all) {
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    assert_eq!(seen, vec![2, 0]);
    assert!(finished(&all));
    assert!(h.t.have().get(0) && h.t.have().get(2));
    assert!(!h.t.have().get(1));
    assert!(h.t.picker().urgent().is_empty());
}

#[test]
fn suggested_pieces_are_preferred() {
    let mut h = Harness::new(config(), &[]);
    h.ev(Event::Start);
    h.ev(Event::Announced {
        peers: vec![addr(1)],
        interval: None,
    });
    let reserved = Reserved::new().with_fast(true);
    h.ev(Event::PeerConnected {
        peer: PeerKey(1),
        addr: addr(1),
        reserved,
    });
    h.msg(1, Message::HaveAll);
    h.msg(1, Message::Suggest { piece: 3 });
    // Out of range: ignored.
    h.msg(1, Message::Suggest { piece: 99 });
    let fx = h.msg(1, Message::Unchoke);
    assert_eq!(requested_pieces(&fx), vec![3]);
}

proptest! {
    /// Whatever the priorities, only wanted pieces are requested and the
    /// torrent finishes with exactly those.
    #[test]
    fn only_wanted_pieces_are_downloaded(prios in prop::collection::vec(0usize..4, PIECES as usize)) {
        let prios: Vec<Priority> = prios.into_iter().map(|i| Priority::ALL[i]).collect();
        let mut h = Harness::new(config(), &prios);
        let fx = h.with_seed(Reserved::new());
        let all = h.serve(fx);
        for p in requested_pieces(&all) {
            prop_assert!(prios[p as usize].is_wanted());
        }
        prop_assert_eq!(h.t.status(), TorrentStatus::Finished);
        for (p, prio) in (0u32..).zip(&prios) {
            prop_assert_eq!(h.t.have().get(p), prio.is_wanted());
        }
    }
}
