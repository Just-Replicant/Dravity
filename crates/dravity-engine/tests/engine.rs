//! Scripted event sequences against `TorrentState::update`.

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation, clippy::panic)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dravity_core::{BLOCK_SIZE, BlockRef, PieceLayout};
use dravity_engine::{
    Alert, AnnounceEvent, Bitfield, Config, Effect, Event, PeerKey, TorrentState, TorrentStatus,
};
use dravity_wire::{BlockRequest, Message, MetadataMessage, PexMessage, PexPeer, Reserved};
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
            reserved: Reserved::new(),
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
    let fx = h.ev(Event::PeerConnectFailed {
        addr: addr(1),
        retry: true,
    });
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
fn preamble_does_not_block_bitfield() {
    let mut h = Harness::started_with_peer(config());
    for message in [
        Message::KeepAlive,
        Message::Port(6881),
        Message::Extended {
            id: 0,
            payload: Bytes::new(),
        },
        Message::Unknown {
            id: 21,
            payload: Bytes::new(),
        },
    ] {
        assert!(
            !disconnects(&h.msg(1, message), 1),
            "preamble must not drop the peer"
        );
    }
    let fx = h.msg(1, Message::Bitfield(Bitfield::full(3).to_bytes()));
    assert!(!disconnects(&fx, 1));
    assert!(fx.contains(&Effect::Send {
        peer: PeerKey(1),
        message: Message::Interested
    }));
    // A second bitfield, or one after a real BEP 3 message, is still illegal.
    assert!(disconnects(
        &h.msg(1, Message::Bitfield(Bitfield::full(3).to_bytes())),
        1
    ));

    let mut h = Harness::started_with_peer(config());
    h.msg(1, Message::Unchoke);
    assert!(disconnects(&h.msg(1, Message::HaveAll), 1));
}

#[test]
fn failed_and_dropped_peers_are_retried() {
    let mut c = config();
    c.peer_retry = Duration::from_secs(15);
    let mut h = Harness::new(c);
    h.ev(Event::Start);
    h.ev(Event::AddPeers {
        peers: vec![addr(1)],
    });
    let fx = h.ev(Event::PeerConnectFailed {
        addr: addr(1),
        retry: true,
    });
    assert!(
        !fx.iter().any(|e| matches!(e, Effect::Connect { .. })),
        "a failure is not redialed in the same turn"
    );
    assert_eq!(h.t.stats().known_peers, 1);
    assert!(
        !h.advance(Duration::from_secs(14))
            .iter()
            .any(|e| matches!(e, Effect::Connect { addr: peer } if *peer == addr(1)))
    );
    assert!(
        h.advance(Duration::from_secs(1))
            .contains(&Effect::Connect { addr: addr(1) })
    );

    // A live peer that drops is dialed again; a protocol reject is not.
    let mut h = Harness::started_with_peer(config());
    h.ev(Event::PeerDisconnected { peer: PeerKey(1) });
    assert_eq!(h.t.stats().peers, 0);
    assert!(
        h.advance(h.t.config().peer_retry)
            .contains(&Effect::Connect { addr: addr(1) })
    );

    let mut h = Harness::started_with_peer(config());
    assert!(disconnects(&h.msg(1, Message::Have { piece: 99 }), 1));
    assert_eq!(h.t.stats().known_peers, 0);
    assert!(
        !h.advance(h.t.config().peer_retry)
            .iter()
            .any(|e| matches!(e, Effect::Connect { .. }))
    );

    // Adding the same address again dials immediately, ahead of the timer.
    let mut h = Harness::new(config());
    h.ev(Event::Start);
    h.ev(Event::AddPeers {
        peers: vec![addr(4)],
    });
    h.ev(Event::PeerConnectFailed {
        addr: addr(4),
        retry: true,
    });
    assert!(
        h.ev(Event::AddPeers {
            peers: vec![addr(4)],
        })
        .contains(&Effect::Connect { addr: addr(4) })
    );

    // A handshake reject is not retried.
    let mut h = Harness::new(config());
    h.ev(Event::Start);
    h.ev(Event::AddPeers {
        peers: vec![addr(2)],
    });
    h.ev(Event::PeerConnectFailed {
        addr: addr(2),
        retry: false,
    });
    assert_eq!(h.t.stats().known_peers, 0);
    assert!(
        !h.advance(h.t.config().peer_retry)
            .iter()
            .any(|e| matches!(e, Effect::Connect { .. }))
    );
}

#[test]
fn storage_failure_stops_the_torrent() {
    let mut h = Harness::started_with_peer(config());
    let fx = h.ev(Event::StorageFailed {
        error: "disk read failed".into(),
    });
    assert!(has_alert(
        &fx,
        |a| matches!(a, Alert::Error { message } if message.contains("disk read failed"))
    ));
    assert_eq!(h.t.status(), TorrentStatus::Stopped);
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
    c.initial_requests_per_peer = 100;
    c.endgame = false;
    let mut h = Harness::started_with_peer(c);
    h.ev(Event::Announced {
        peers: vec![addr(2)],
        interval: None,
    });
    h.ev(Event::PeerConnected {
        peer: PeerKey(2),
        addr: addr(2),
        reserved: Reserved::new(),
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
        reserved: Reserved::new(),
    });
    h.msg(2, Message::HaveAll);
    assert_eq!(requests(&h.msg(2, Message::Unchoke), 2), first);
}

#[test]
fn timeouts_and_keepalives() {
    let mut h = Harness::started_with_peer(config());
    h.msg(1, Message::HaveAll);
    let first = requests(&h.msg(1, Message::Unchoke), 1);
    // After the request timeout the same blocks are requested again, with
    // the AIMD window halved.
    let fx = h.advance(h.t.config().request_timeout);
    assert_eq!(requests(&fx, 1), first[..first.len() / 2]);
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
        reserved: Reserved::new(),
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

#[test]
fn pending_writes_throttle_requests() {
    let mut c = config();
    c.max_pending_writes = 2;
    let mut h = Harness::started_with_peer(c);
    h.msg(1, Message::HaveAll);
    let first = requests(&h.msg(1, Message::Unchoke), 1);
    assert_eq!(first.len(), 4);
    assert_eq!(requests(&h.msg(1, data_for(first[0])), 1).len(), 1);
    // Second unwritten block reaches the limit: no new request.
    assert!(requests(&h.msg(1, data_for(first[1])), 1).is_empty());
    assert_eq!(h.t.stats().pending_writes, 2);
    let fx = h.ev(Event::BlockWritten {
        block: block(first[0]),
    });
    assert_eq!(requests(&fx, 1).len(), 1, "resumes once a write completes");
}

#[test]
fn add_peers_connects_without_announcing() {
    let mut h = Harness::new(config());
    h.ev(Event::Start);
    let fx = h.ev(Event::AddPeers {
        peers: vec![addr(5)],
    });
    assert_eq!(fx, vec![Effect::Connect { addr: addr(5) }]);
}

#[test]
fn peers_added_while_checking_connect_afterwards() {
    let mut c = config();
    c.check_existing = true;
    let mut h = Harness::new(c);
    h.ev(Event::Start);
    assert!(
        h.ev(Event::AddPeers {
            peers: vec![addr(5)]
        })
        .is_empty()
    );
    h.ev(Event::PieceHashed {
        piece: 0,
        ok: false,
    });
    h.ev(Event::PieceHashed {
        piece: 1,
        ok: false,
    });
    let fx = h.ev(Event::PieceHashed {
        piece: 2,
        ok: false,
    });
    assert!(fx.contains(&Effect::Connect { addr: addr(5) }));
}

fn ext_handshake(entries: &str) -> Message {
    Message::Extended {
        id: 0,
        payload: Bytes::from(format!("d1:md{entries}ee").into_bytes()),
    }
}

fn extended() -> Reserved {
    Reserved::new().with_extension_protocol(true)
}

fn sent(effects: &[Effect], peer: u64) -> Vec<Message> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Send { peer: p, message } if *p == PeerKey(peer) => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// The single extended message sent to `peer`.
fn only_extended(effects: &[Effect], peer: u64) -> (u8, Bytes) {
    let mut messages = sent(effects, peer);
    messages.retain(|m| *m != Message::KeepAlive);
    match messages.as_slice() {
        [Message::Extended { id, payload }] => (*id, payload.clone()),
        other => panic!("expected one extended message, got {other:?}"),
    }
}

fn connect(h: &mut Harness, peer: u64, reserved: Reserved) -> Vec<Effect> {
    h.ev(Event::PeerConnected {
        peer: PeerKey(peer),
        addr: addr(peer as u16),
        reserved,
    })
}

#[test]
fn extension_handshake_and_metadata_serving() {
    let info = Bytes::from(vec![b'x'; 20_000]);
    let mut h = Harness::new(config());
    h.t = TorrentState::new(layout(), config()).with_info_bytes(info.clone());
    h.ev(Event::Start);
    // Plain peers get no extended messages.
    assert!(sent(&connect(&mut h, 1, Reserved::new()), 1).is_empty());
    let fx = connect(&mut h, 2, extended());
    let (_id, payload) = only_extended(&fx, 2);
    assert_eq!(_id, 0);
    let ours = dravity_wire::ExtendedHandshake::decode(&payload).unwrap();
    assert_eq!(ours.metadata_size, Some(20_000));
    let our_metadata_id = ours.extension_id("ut_metadata").unwrap();
    assert!(ours.extension_id("ut_pex").is_some());

    // The peer registers ut_metadata as id 3 and asks for both pieces.
    h.msg(2, ext_handshake("11:ut_metadatai3e"));
    let request = |piece| Message::Extended {
        id: our_metadata_id,
        payload: MetadataMessage::Request { piece }.encode().unwrap(),
    };
    let reply = |fx: &[Effect]| match sent(fx, 2).as_slice() {
        [Message::Extended { id: 3, payload }] => MetadataMessage::decode(payload).unwrap(),
        other => panic!("unexpected reply {other:?}"),
    };
    assert_eq!(
        reply(&h.msg(2, request(1))),
        MetadataMessage::Data {
            piece: 1,
            total_size: 20_000,
            data: info.slice(16_384..),
        }
    );
    assert_eq!(
        reply(&h.msg(2, request(2))),
        MetadataMessage::Reject { piece: 2 }
    );
    // Without info bytes every request is rejected.
    let mut bare = Harness::started_with_peer(config());
    bare.ev(Event::PeerConnected {
        peer: PeerKey(2),
        addr: addr(2),
        reserved: extended(),
    });
    bare.msg(2, ext_handshake("11:ut_metadatai3e"));
    let fx = bare.msg(2, request(0));
    assert!(matches!(
        sent(&fx, 2).as_slice(),
        [Message::Extended { id: 3, .. }]
    ));
}

#[test]
fn pex_adds_peers_and_reports_connections() {
    let mut c = config();
    c.max_peers = 10;
    let mut h = Harness::new(c);
    h.ev(Event::Start);
    h.ev(Event::AddPeers {
        peers: vec![addr(1), addr(2)],
    });
    connect(&mut h, 1, extended());
    connect(&mut h, 2, Reserved::new());
    h.msg(1, ext_handshake("6:ut_pexi7e"));
    // Peer 1 tells us about peer 3 (and an already connected peer).
    let pex = PexMessage {
        added: vec![
            PexPeer {
                addr: addr(3),
                flags: 0,
            },
            PexPeer {
                addr: addr(2),
                flags: 0,
            },
        ],
        dropped: vec![],
    };
    let fx = h.msg(
        1,
        Message::Extended {
            id: 2,
            payload: Bytes::from(pex.encode()),
        },
    );
    assert!(fx.contains(&Effect::Connect { addr: addr(3) }));
    assert!(!fx.contains(&Effect::Connect { addr: addr(2) }));

    // After the PEX interval peer 1 hears about peer 2 (not itself).
    let fx = h.advance(h.t.config().pex_interval);
    let (_id, payload) = only_extended(&fx, 1);
    assert_eq!(_id, 7);
    let msg = PexMessage::decode(&payload).unwrap();
    assert_eq!(
        msg.added.iter().map(|p| p.addr).collect::<Vec<_>>(),
        [addr(2)]
    );
    assert!(msg.dropped.is_empty());
    // Nothing changed: no message. Then peer 2 drops and is reported.
    assert!(sent(&h.advance(h.t.config().pex_interval), 1).is_empty());
    h.ev(Event::PeerDisconnected { peer: PeerKey(2) });
    h.msg(1, Message::KeepAlive); // stay clear of the peer timeout
    let fx = h.advance(h.t.config().pex_interval);
    let (_id, payload) = only_extended(&fx, 1);
    assert_eq!(PexMessage::decode(&payload).unwrap().dropped, [addr(2)]);
}

#[test]
fn pex_disabled_for_private_torrents() {
    let mut c = config();
    c.pex = false;
    let mut h = Harness::new(c);
    h.ev(Event::Start);
    let fx = connect(&mut h, 1, extended());
    let (_id, payload) = only_extended(&fx, 1);
    let ours = dravity_wire::ExtendedHandshake::decode(&payload).unwrap();
    assert_eq!(ours.extension_id("ut_pex"), None);
    h.msg(1, ext_handshake("6:ut_pexi7e"));
    let pex = PexMessage {
        added: vec![PexPeer {
            addr: addr(3),
            flags: 0,
        }],
        dropped: vec![],
    };
    let fx = h.msg(
        1,
        Message::Extended {
            id: 2,
            payload: Bytes::from(pex.encode()),
        },
    );
    assert!(!fx.contains(&Effect::Connect { addr: addr(3) }));
    let later = sent(&h.advance(Duration::from_secs(120)), 1);
    assert!(
        !later.iter().any(|m| matches!(m, Message::Extended { .. })),
        "{later:?}"
    );
}

#[test]
fn request_window_grows_and_respects_reqq() {
    // One big piece so the window, not the torrent, limits requests.
    let big = PieceLayout::new(64 * BLOCK_SIZE, u64::from(64 * BLOCK_SIZE)).unwrap();
    let mut c = config();
    c.max_requests_per_peer = 32;
    c.initial_requests_per_peer = 2;
    let mut h = Harness::new(c.clone());
    h.t = TorrentState::new(big, c);
    h.ev(Event::Start);
    connect(&mut h, 1, extended());
    h.msg(1, Message::HaveAll);
    let first = requests(&h.msg(1, Message::Unchoke), 1);
    assert_eq!(first.len(), 2);
    // Slow start: each answered block opens two more slots.
    let fx = h.msg(1, data_for(first[0]));
    assert_eq!(requests(&fx, 1).len(), 2);
    // A peer that accepts only 3 outstanding requests gets no more than that.
    h.msg(1, ext_handshake("e4:reqqi3"));
    let fx = h.msg(1, data_for(first[1]));
    assert_eq!(requests(&fx, 1).len(), 1);
    assert_eq!(h.t.stats().peers, 1);
}

#[test]
fn endgame_requests_twice_and_cancels_the_loser() {
    let mut c = config();
    c.max_requests_per_peer = 100;
    c.initial_requests_per_peer = 100;
    let mut h = Harness::started_with_peer(c);
    connect(&mut h, 2, Reserved::new());
    h.msg(1, Message::HaveAll);
    h.msg(2, Message::HaveAll);
    let all = requests(&h.msg(1, Message::Unchoke), 1);
    assert_eq!(all.len(), 6);
    // Everything is requested: peer 2 duplicates the in-flight blocks.
    let dup = requests(&h.msg(2, Message::Unchoke), 2);
    assert_eq!(dup, all);
    // Peer 2 wins the first block: peer 1 gets a cancel for it.
    let fx = h.msg(2, data_for(all[0]));
    assert!(fx.iter().any(|e| matches!(e, Effect::WriteBlock { .. })));
    assert!(sent(&fx, 1).contains(&Message::Cancel(all[0])));
    // The late copy from peer 1 is not written again.
    let fx = h.msg(1, data_for(all[0]));
    assert!(!fx.iter().any(|e| matches!(e, Effect::WriteBlock { .. })));
    // Peer 1 choking us leaves the blocks with peer 2: nothing new is asked.
    let fx = h.msg(1, Message::Choke);
    assert!(requests(&fx, 2).is_empty());
}

#[test]
fn idle_peers_steal_blocks_from_slow_peers() {
    let mut c = config();
    c.max_requests_per_peer = 100;
    c.initial_requests_per_peer = 100;
    c.endgame = false;
    c.steal_after = Duration::from_secs(5);
    let mut h = Harness::started_with_peer(c);
    connect(&mut h, 2, Reserved::new());
    h.msg(1, Message::HaveAll);
    h.msg(2, Message::HaveAll);
    let slow = requests(&h.msg(1, Message::Unchoke), 1);
    assert!(requests(&h.msg(2, Message::Unchoke), 2).is_empty());
    let stolen = requests(&h.advance(Duration::from_secs(5)), 2);
    assert_eq!(stolen, slow);
    assert!(requests(&h.advance(Duration::from_secs(1)), 2).is_empty());
}

#[test]
fn download_rate_limit_paces_requests() {
    let mut c = config();
    c.download_rate_limit = Some(u64::from(BLOCK_SIZE));
    let mut h = Harness::started_with_peer(c);
    h.msg(1, Message::HaveAll);
    let first = requests(&h.msg(1, Message::Unchoke), 1);
    assert_eq!(first.len(), 1, "one block per second");
    assert!(requests(&h.msg(1, data_for(first[0])), 1).is_empty());
    assert!(requests(&h.advance(Duration::from_millis(500)), 1).is_empty());
    assert_eq!(requests(&h.advance(Duration::from_millis(500)), 1).len(), 1);
}

#[test]
fn dht_lookups_are_scheduled() {
    let mut c = config();
    c.dht = true;
    let mut h = Harness::new(c);
    let fx = h.ev(Event::Start);
    assert!(fx.contains(&Effect::DhtLookup));
    // Only one lookup in flight.
    assert!(
        !h.advance(Duration::from_secs(1))
            .contains(&Effect::DhtLookup)
    );
    let fx = h.ev(Event::DhtPeers {
        peers: vec![addr(9)],
    });
    assert!(fx.contains(&Effect::Connect { addr: addr(9) }));
    let interval = h.t.config().dht_interval;
    assert!(!h.advance(interval / 2).contains(&Effect::DhtLookup));
    assert!(h.advance(interval / 2).contains(&Effect::DhtLookup));
    // Disabled by default.
    let mut h = Harness::new(config());
    assert!(!h.ev(Event::Start).contains(&Effect::DhtLookup));
}

#[test]
fn peers_handed_over_while_checking_start_after_the_check() {
    let mut c = config();
    c.check_existing = true;
    c.check_concurrency = 3;
    let mut h = Harness::new(c);
    h.t = TorrentState::new(layout(), h.t.config().clone()).with_started_announced(true);
    h.ev(Event::Start);
    assert_eq!(h.t.status(), TorrentStatus::Checking);
    let fx = connect(&mut h, 1, Reserved::new());
    assert!(!disconnects(&fx, 1));
    assert!(
        h.msg(1, Message::HaveAll).is_empty(),
        "no interest while checking"
    );
    h.ev(Event::PieceHashed { piece: 0, ok: true });
    h.ev(Event::PieceHashed {
        piece: 1,
        ok: false,
    });
    let fx = h.ev(Event::PieceHashed {
        piece: 2,
        ok: false,
    });
    let to_peer = sent(&fx, 1);
    assert!(
        matches!(to_peer.first(), Some(Message::Bitfield(_))),
        "{to_peer:?}"
    );
    assert!(to_peer.contains(&Message::Interested));
    // `started` was sent by the metadata fetch.
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Announce {
            event: AnnounceEvent::None,
            ..
        }
    )));
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
        Just(ext_handshake("11:ut_metadatai3e6:ut_pexi4ee4:reqqi2")),
        (0u8..4, proptest::collection::vec(any::<u8>(), 0..40)).prop_map(|(id, b)| {
            Message::Extended {
                id,
                payload: Bytes::from(b),
            }
        }),
        (0u32..2).prop_map(|piece| Message::Extended {
            id: 1,
            payload: MetadataMessage::Request { piece }.encode().unwrap(),
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
        let mut c = config();
        c.steal_after = Duration::from_secs(3);
        let mut h = Harness::new(c);
        h.t = TorrentState::new(layout(), h.t.config().clone())
            .with_info_bytes(Bytes::from(vec![1u8; 20_000]));
        h.ev(Event::Start);
        for step in steps {
            let fx = match step {
                Step::Msg(p, m) => h.msg(p, m),
                Step::Connect(p) => h.ev(Event::PeerConnected {
                    peer: PeerKey(p),
                    addr: addr(p as u16),
                    reserved: Reserved::new().with_extension_protocol(p % 2 == 1),
                }),
                Step::Disconnect(p) => h.ev(Event::PeerDisconnected { peer: PeerKey(p) }),
                Step::Tick(s) => h.advance(Duration::from_secs(s)),
                Step::Written(piece, b) => {
                    let Some(block) = layout().block(piece, b) else { continue };
                    h.ev(Event::BlockWritten { block })
                }
                Step::Hashed(piece, ok) => h.ev(Event::PieceHashed { piece, ok }),
            };
            let mut asked = std::collections::HashSet::new();
            for e in &fx {
                if let Effect::Send { peer, message: Message::Request(r) } = e {
                    prop_assert!(layout().is_valid_request(block(*r)));
                    prop_assert!((1..4).contains(&peer.0));
                    prop_assert!(asked.insert((*peer, *r)), "block requested twice from one peer");
                }
            }
            let s = h.t.stats();
            prop_assert!(s.pieces_have <= s.piece_count);
            prop_assert!(s.bytes_have <= s.total_length);
            prop_assert!(s.peers <= h.t.config().max_peers);
        }
    }
}
