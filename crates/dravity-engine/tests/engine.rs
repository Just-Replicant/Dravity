//! Scripted event sequences against `TorrentState::update`.

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dravity_core::{BLOCK_SIZE, BlockRef, PieceLayout};
use dravity_engine::{
    Alert, AnnounceEvent, Bitfield, Config, Effect, Event, PeerKey, TorrentState, TorrentStatus,
};
use dravity_wire::{BlockRequest, Message};
use proptest::prelude::*;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], 6000 + n))
}

fn config() -> Config {
    let mut c = Config::default();
    c.check_existing = false;
    c.max_requests_per_peer = 4;
    c
}

/// 3 pieces of 2 blocks each, last piece shorter.
fn layout() -> PieceLayout {
    PieceLayout::new(2 * BLOCK_SIZE, u64::from(5 * BLOCK_SIZE) + 10).unwrap()
}

struct Harness {
    t: TorrentState,
    now: Instant,
}

impl Harness {
    fn new(config: Config) -> Self {
        Self {
            t: TorrentState::new(layout(), config),
            now: Instant::now(),
        }
    }

    fn ev(&mut self, e: Event) -> Vec<Effect> {
        self.t.update(self.now, e)
    }

    fn advance(&mut self, d: Duration) -> Vec<Effect> {
        self.now += d;
        self.ev(Event::Tick)
    }

    fn msg(&mut self, peer: u64, message: Message) -> Vec<Effect> {
        self.ev(Event::Message {
            peer: PeerKey(peer),
            message,
        })
    }

    /// Start, get one peer from the tracker and connect it.
    fn started_with_peer(config: Config) -> Self {
        let mut h = Self::new(config);
        h.ev(Event::Start);
        h.ev(Event::Announced {
            peers: vec![addr(1)],
            interval: None,
        });
        h.ev(Event::PeerConnected {
            peer: PeerKey(1),
            addr: addr(1),
        });
        h
    }
}

fn requests(effects: &[Effect], peer: u64) -> Vec<BlockRequest> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Send {
                peer: p,
                message: Message::Request(r),
            } if *p == PeerKey(peer) => Some(*r),
            _ => None,
        })
        .collect()
}

fn has_alert(effects: &[Effect], f: impl Fn(&Alert) -> bool) -> bool {
    effects
        .iter()
        .any(|e| matches!(e, Effect::Alert(a) if f(a)))
}

fn disconnects(effects: &[Effect], peer: u64) -> bool {
    effects.contains(&Effect::Disconnect {
        peer: PeerKey(peer),
    })
}

fn block(r: BlockRequest) -> BlockRef {
    BlockRef {
        piece: r.piece,
        offset: r.offset,
        length: r.length,
    }
}

fn data_for(r: BlockRequest) -> Message {
    Message::Piece {
        piece: r.piece,
        offset: r.offset,
        data: Bytes::from(vec![7u8; r.length as usize]),
    }
}

#[test]
fn start_announces_and_connects_tracker_peers() {
    let mut c = config();
    c.max_peers = 2;
    let mut h = Harness::new(c);
    let fx = h.ev(Event::Start);
    assert!(fx.contains(&Effect::Alert(Alert::StateChanged(
        TorrentStatus::Downloading
    ))));
    assert!(fx.contains(&Effect::Announce {
        event: AnnounceEvent::Started,
        uploaded: 0,
        downloaded: 0,
        left: layout().total_length(),
    }));
    let fx = h.ev(Event::Announced {
        peers: vec![
            addr(1),
            addr(1),
            addr(2),
            addr(3),
            SocketAddr::from(([1, 1, 1, 1], 0)),
        ],
        interval: Some(Duration::from_secs(600)),
    });
    let connects: Vec<_> = fx
        .iter()
        .filter_map(|e| match e {
            Effect::Connect { addr } => Some(*addr),
            _ => None,
        })
        .collect();
    assert_eq!(connects, vec![addr(1), addr(2)]);
    // A failed attempt frees a slot for the queued address.
    let fx = h.ev(Event::PeerConnectFailed { addr: addr(1) });
    assert_eq!(fx, vec![Effect::Connect { addr: addr(3) }]);
    assert_eq!(h.t.stats().connecting, 2);
}

#[test]
fn downloads_every_piece_and_finishes() {
    let mut h = Harness::started_with_peer(config());
    let fx = h.msg(1, Message::Bitfield(Bitfield::full(3).to_bytes()));
    assert!(fx.contains(&Effect::Send {
        peer: PeerKey(1),
        message: Message::Interested
    }));
    assert!(requests(&fx, 1).is_empty(), "still choked");
    let mut pending = requests(&h.msg(1, Message::Unchoke), 1);
    assert_eq!(pending.len(), 4, "pipeline depth");
    let mut hashed = Vec::new();
    let mut finished = false;
    while let Some(r) = pending.pop() {
        let fx = h.msg(1, data_for(r));
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::WriteBlock { block: b, .. } if *b == block(r)))
        );
        pending.extend(requests(&fx, 1));
        for e in h.ev(Event::BlockWritten { block: block(r) }) {
            if let Effect::HashPiece { piece } = e {
                hashed.push(piece);
                let fx = h.ev(Event::PieceHashed { piece, ok: true });
                assert!(has_alert(&fx, |a| *a == Alert::PieceCompleted { piece }));
                assert!(fx.contains(&Effect::Send {
                    peer: PeerKey(1),
                    message: Message::Have { piece }
                }));
                pending.extend(requests(&fx, 1));
                if has_alert(&fx, |a| *a == Alert::Finished) {
                    finished = true;
                    assert!(fx.iter().any(|e| matches!(
                        e,
                        Effect::Announce {
                            event: AnnounceEvent::Completed,
                            left: 0,
                            ..
                        }
                    )));
                    assert!(fx.contains(&Effect::Send {
                        peer: PeerKey(1),
                        message: Message::NotInterested
                    }));
                }
            }
        }
    }
    hashed.sort_unstable();
    assert_eq!(hashed, vec![0, 1, 2]);
    assert!(finished);
    let s = h.t.stats();
    assert_eq!(s.status, TorrentStatus::Finished);
    assert_eq!(s.bytes_have, layout().total_length());
    assert_eq!(s.downloaded, layout().total_length());
}

#[test]
fn hash_failure_redownloads_piece() {
    let mut c = config();
    c.max_requests_per_peer = 2;
    let mut h = Harness::started_with_peer(c);
    h.msg(1, Message::HaveAll);
    let piece0 = requests(&h.msg(1, Message::Unchoke), 1);
    assert!(piece0.iter().all(|r| r.piece == 0) && piece0.len() == 2);
    let mut to_hash = None;
    let mut next = Vec::new();
    for r in &piece0 {
        next.extend(requests(&h.msg(1, data_for(*r)), 1));
        for e in h.ev(Event::BlockWritten { block: block(*r) }) {
            if let Effect::HashPiece { piece } = e {
                to_hash = Some(piece);
            }
        }
    }
    assert_eq!(to_hash, Some(0));
    assert!(next.iter().all(|r| r.piece == 1));
    let fx = h.ev(Event::PieceHashed {
        piece: 0,
        ok: false,
    });
    assert!(has_alert(&fx, |a| *a == Alert::HashFailed { piece: 0 }));
    assert_eq!(h.t.have().count(), 0);
    // Once the pipeline has room, the failed piece is requested again first.
    let mut again = Vec::new();
    for r in next {
        again.extend(requests(&h.msg(1, data_for(r)), 1));
    }
    assert_eq!(again, piece0);
}

#[test]
fn protocol_violations_disconnect() {
    // Bitfield of the wrong length.
    let mut h = Harness::started_with_peer(config());
    assert!(disconnects(
        &h.msg(1, Message::Bitfield(Bytes::from_static(&[0xff, 0]))),
        1
    ));
    assert_eq!(h.t.stats().peers, 0);

    // Bitfield after another message.
    let mut h = Harness::started_with_peer(config());
    h.msg(1, Message::Unchoke);
    assert!(disconnects(
        &h.msg(1, Message::Bitfield(Bitfield::full(3).to_bytes())),
        1
    ));

    // Have for a piece that does not exist.
    let mut h = Harness::started_with_peer(config());
    assert!(disconnects(&h.msg(1, Message::Have { piece: 3 }), 1));

    // Messages from unknown peers are ignored.
    let mut h = Harness::started_with_peer(config());
    assert!(h.msg(42, Message::Unchoke).is_empty());
}

#[test]
fn choke_releases_requests_to_other_peers() {
    let mut c = config();
    c.max_requests_per_peer = 100;
    let mut h = Harness::started_with_peer(c);
    h.ev(Event::Announced {
        peers: vec![addr(2)],
        interval: None,
    });
    h.ev(Event::PeerConnected {
        peer: PeerKey(2),
        addr: addr(2),
    });
    h.msg(1, Message::HaveAll);
    h.msg(2, Message::HaveAll);
    assert_eq!(requests(&h.msg(1, Message::Unchoke), 1).len(), 6);
    assert!(requests(&h.msg(2, Message::Unchoke), 2).is_empty());
    h.msg(1, Message::Choke);
    // Released blocks go to peer 2 on its next opportunity.
    assert_eq!(requests(&h.advance(Duration::from_secs(1)), 2).len(), 6);
}

#[test]
fn disconnect_releases_requests() {
    let mut h = Harness::started_with_peer(config());
    h.msg(1, Message::HaveAll);
    let first = requests(&h.msg(1, Message::Unchoke), 1);
    h.ev(Event::PeerDisconnected { peer: PeerKey(1) });
    h.ev(Event::PeerConnected {
        peer: PeerKey(2),
        addr: addr(2),
    });
    h.msg(2, Message::HaveAll);
    assert_eq!(requests(&h.msg(2, Message::Unchoke), 2), first);
}

#[test]
fn timeouts_and_keepalives() {
    let mut h = Harness::started_with_peer(config());
    h.msg(1, Message::HaveAll);
    let first = requests(&h.msg(1, Message::Unchoke), 1);
    // After the request timeout the same blocks are requested again.
    let fx = h.advance(h.t.config().request_timeout);
    assert_eq!(requests(&fx, 1), first);
    let fx = h.advance(h.t.config().keepalive_interval);
    assert!(fx.contains(&Effect::Send {
        peer: PeerKey(1),
        message: Message::KeepAlive
    }));
    let fx = h.advance(h.t.config().peer_timeout);
    assert!(disconnects(&fx, 1));
    assert_eq!(h.t.stats().peers, 0);
}

#[test]
fn reannounces_on_interval_and_retries_failures() {
    let mut h = Harness::new(config());
    h.ev(Event::Start);
    h.ev(Event::AnnounceFailed {
        error: "boom".into(),
    });
    let fx = h.advance(h.t.config().announce_retry);
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Announce {
            event: AnnounceEvent::None,
            ..
        }
    )));
    // An in-flight announce is not duplicated.
    let fx = h.advance(Duration::from_secs(3600));
    assert!(!fx.iter().any(|e| matches!(e, Effect::Announce { .. })));
    h.ev(Event::Announced {
        peers: vec![],
        interval: Some(Duration::from_secs(1)),
    });
    // Tracker intervals are clamped to the minimum.
    assert!(
        !h.advance(Duration::from_secs(2))
            .iter()
            .any(|e| matches!(e, Effect::Announce { .. }))
    );
    let fx = h.advance(h.t.config().min_announce_interval);
    assert!(fx.iter().any(|e| matches!(e, Effect::Announce { .. })));
}

#[test]
fn check_existing_resumes_partial_download() {
    let mut c = config();
    c.check_existing = true;
    c.check_concurrency = 2;
    let mut h = Harness::new(c);
    let fx = h.ev(Event::Start);
    assert!(fx.contains(&Effect::Alert(Alert::StateChanged(TorrentStatus::Checking))));
    let hashes = |fx: &[Effect]| -> Vec<u32> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::HashPiece { piece } => Some(*piece),
                _ => None,
            })
            .collect()
    };
    assert_eq!(hashes(&fx), vec![0, 1]);
    assert_eq!(
        hashes(&h.ev(Event::PieceHashed { piece: 0, ok: true })),
        vec![2]
    );
    assert!(
        hashes(&h.ev(Event::PieceHashed {
            piece: 2,
            ok: false
        }))
        .is_empty()
    );
    let fx = h.ev(Event::PieceHashed { piece: 1, ok: true });
    assert!(fx.contains(&Effect::Alert(Alert::StateChanged(
        TorrentStatus::Downloading
    ))));
    assert!(fx.contains(&Effect::Announce {
        event: AnnounceEvent::Started,
        uploaded: 0,
        downloaded: 0,
        left: u64::from(BLOCK_SIZE) + 10,
    }));
    // Our bitfield is sent to new peers.
    h.ev(Event::Announced {
        peers: vec![addr(1)],
        interval: None,
    });
    let fx = h.ev(Event::PeerConnected {
        peer: PeerKey(1),
        addr: addr(1),
    });
    assert_eq!(
        fx,
        vec![Effect::Send {
            peer: PeerKey(1),
            message: Message::Bitfield(Bytes::from_static(&[0b1100_0000]))
        }]
    );
}

#[test]
fn complete_on_disk_finishes_without_announcing() {
    let mut c = config();
    c.check_existing = true;
    let mut h = Harness::new(c);
    h.ev(Event::Start);
    h.ev(Event::PieceHashed { piece: 0, ok: true });
    h.ev(Event::PieceHashed { piece: 1, ok: true });
    let fx = h.ev(Event::PieceHashed { piece: 2, ok: true });
    assert!(has_alert(&fx, |a| *a == Alert::Finished));
    assert!(!fx.iter().any(|e| matches!(e, Effect::Announce { .. })));
    assert_eq!(h.t.status(), TorrentStatus::Finished);
}

#[test]
fn empty_torrent_is_finished_immediately() {
    let layout = PieceLayout::new(BLOCK_SIZE, 0).unwrap();
    let mut t = TorrentState::new(layout, Config::default());
    let fx = t.update(Instant::now(), Event::Start);
    assert!(has_alert(&fx, |a| *a == Alert::Finished));
}

#[test]
fn stop_disconnects_and_announces_stopped() {
    let mut h = Harness::started_with_peer(config());
    let fx = h.ev(Event::Stop);
    assert!(disconnects(&fx, 1));
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Announce {
            event: AnnounceEvent::Stopped,
            ..
        }
    )));
    assert_eq!(h.t.status(), TorrentStatus::Stopped);
    assert!(
        h.ev(Event::Tick).is_empty(),
        "stopped torrents ignore events"
    );
}

#[test]
fn write_failure_is_fatal() {
    let mut h = Harness::started_with_peer(config());
    h.msg(1, Message::HaveAll);
    let r = requests(&h.msg(1, Message::Unchoke), 1)[0];
    h.msg(1, data_for(r));
    let fx = h.ev(Event::WriteFailed {
        block: block(r),
        error: "disk full".into(),
    });
    assert!(has_alert(
        &fx,
        |a| matches!(a, Alert::Error { message } if message.contains("disk full"))
    ));
    assert_eq!(h.t.status(), TorrentStatus::Stopped);
}

#[test]
fn unrequested_and_duplicate_blocks_are_not_written_twice() {
    let mut h = Harness::started_with_peer(config());
    h.msg(1, Message::HaveAll);
    let r = requests(&h.msg(1, Message::Unchoke), 1)[0];
    let writes = |fx: &[Effect]| {
        fx.iter()
            .filter(|e| matches!(e, Effect::WriteBlock { .. }))
            .count()
    };
    assert_eq!(writes(&h.msg(1, data_for(r))), 1);
    assert_eq!(writes(&h.msg(1, data_for(r))), 0);
    let bogus = Message::Piece {
        piece: 0,
        offset: 5,
        data: Bytes::from_static(b"xyz"),
    };
    assert_eq!(writes(&h.msg(1, bogus)), 0);
}

fn arb_message() -> impl Strategy<Value = Message> {
    prop_oneof![
        Just(Message::KeepAlive),
        Just(Message::Choke),
        Just(Message::Unchoke),
        Just(Message::Interested),
        Just(Message::HaveAll),
        Just(Message::HaveNone),
        (0u32..5).prop_map(|piece| Message::Have { piece }),
        proptest::collection::vec(any::<u8>(), 0..3)
            .prop_map(|b| Message::Bitfield(Bytes::from(b))),
        (0u32..4, 0u32..3, 0u32..3).prop_map(|(piece, block, extra)| Message::Piece {
            piece,
            offset: block * BLOCK_SIZE,
            data: Bytes::from(vec![0; (BLOCK_SIZE - extra) as usize]),
        }),
    ]
}

#[derive(Debug, Clone)]
enum Step {
    Msg(u64, Message),
    Connect(u64),
    Disconnect(u64),
    Tick(u64),
    Written(u32, u32),
    Hashed(u32, bool),
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (1u64..4, arb_message()).prop_map(|(p, m)| Step::Msg(p, m)),
        1 => (1u64..4).prop_map(Step::Connect),
        1 => (1u64..4).prop_map(Step::Disconnect),
        1 => (0u64..60).prop_map(Step::Tick),
        2 => (0u32..4, 0u32..3).prop_map(|(p, b)| Step::Written(p, b)),
        1 => (0u32..4, any::<bool>()).prop_map(|(p, ok)| Step::Hashed(p, ok)),
    ]
}

proptest! {
    #[test]
    fn arbitrary_event_sequences_keep_invariants(steps in proptest::collection::vec(arb_step(), 0..200)) {
        let mut h = Harness::new(config());
        h.ev(Event::Start);
        for step in steps {
            let fx = match step {
                Step::Msg(p, m) => h.msg(p, m),
                Step::Connect(p) => h.ev(Event::PeerConnected { peer: PeerKey(p), addr: addr(p as u16) }),
                Step::Disconnect(p) => h.ev(Event::PeerDisconnected { peer: PeerKey(p) }),
                Step::Tick(s) => h.advance(Duration::from_secs(s)),
                Step::Written(piece, b) => {
                    let Some(block) = layout().block(piece, b) else { continue };
                    h.ev(Event::BlockWritten { block })
                }
                Step::Hashed(piece, ok) => h.ev(Event::PieceHashed { piece, ok }),
            };
            for e in &fx {
                if let Effect::Send { peer, message: Message::Request(r) } = e {
                    prop_assert!(layout().is_valid_request(block(*r)));
                    prop_assert!((1..4).contains(&peer.0));
                }
            }
            let s = h.t.stats();
            prop_assert!(s.pieces_have <= s.piece_count);
            prop_assert!(s.bytes_have <= s.total_length);
            prop_assert!(s.peers <= h.t.config().max_peers);
        }
    }
}
