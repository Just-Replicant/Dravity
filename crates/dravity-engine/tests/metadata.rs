//! Scripted magnet metadata fetches against `MetadataState::update`.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing
)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dravity_core::InfoHash;
use dravity_engine::{
    Alert, AnnounceEvent, Config, Effect, Event, MetadataState, PeerKey, TorrentStatus,
};
use dravity_wire::{ExtendedHandshake, Message, MetadataMessage, Reserved};

/// Our `ut_metadata` id (what peers send to us).
const OURS: u8 = 1;
/// The peers' `ut_metadata` id (what we send to them).
const THEIRS: u8 = 9;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 2], 7000 + n))
}

fn info() -> Bytes {
    // A bit more than two metadata pieces.
    Bytes::from(
        (0..40_000u32)
            .map(|i| (i * 7 % 251) as u8)
            .collect::<Vec<_>>(),
    )
}

struct Harness {
    m: MetadataState,
    now: Instant,
}

impl Harness {
    fn new(config: Config) -> Self {
        let mut h = Self {
            m: MetadataState::new(InfoHash::from_info_bytes(&info()), config),
            now: Instant::now(),
        };
        h.ev(Event::Start);
        h
    }

    fn ev(&mut self, e: Event) -> Vec<Effect> {
        self.m.update(self.now, e)
    }

    fn msg(&mut self, peer: u64, message: Message) -> Vec<Effect> {
        self.ev(Event::Message {
            peer: PeerKey(peer),
            message,
        })
    }

    /// Connects an extension-capable peer that offers `size` bytes.
    fn peer(&mut self, peer: u64, size: Option<u64>) -> Vec<Effect> {
        self.ev(Event::AddPeers {
            peers: vec![addr(peer as u16)],
        });
        let mut fx = self.ev(Event::PeerConnected {
            peer: PeerKey(peer),
            addr: addr(peer as u16),
            reserved: Reserved::new().with_extension_protocol(true),
        });
        let hs = ExtendedHandshake {
            extensions: vec![("ut_metadata".into(), THEIRS)],
            listen_port: None,
            client: None,
            your_ip: None,
            request_queue: None,
            metadata_size: size,
            ipv4: None,
            ipv6: None,
        };
        fx.extend(self.msg(
            peer,
            Message::Extended {
                id: 0,
                payload: Bytes::from(hs.encode().unwrap()),
            },
        ));
        fx
    }
}

/// `(peer, piece)` for every metadata request in `fx`.
fn requests(fx: &[Effect]) -> Vec<(u64, u32)> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::Send {
                peer,
                message:
                    Message::Extended {
                        id: THEIRS,
                        payload,
                    },
            } => match MetadataMessage::decode(payload).unwrap() {
                MetadataMessage::Request { piece } => Some((peer.0, piece)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn data(piece: u32, content: &[u8]) -> Message {
    let start = piece as usize * 16_384;
    let end = (start + 16_384).min(content.len());
    Message::Extended {
        id: OURS,
        payload: MetadataMessage::Data {
            piece,
            total_size: content.len() as u64,
            data: Bytes::copy_from_slice(&content[start..end]),
        }
        .encode()
        .unwrap(),
    }
}

fn received(fx: &[Effect]) -> Option<Bytes> {
    fx.iter().find_map(|e| match e {
        Effect::MetadataReceived { info } => Some(info.clone()),
        _ => None,
    })
}

#[test]
fn fetches_verifies_and_hands_over() {
    let mut h = Harness::new(Config::default());
    assert_eq!(h.m.status(), TorrentStatus::FetchingMetadata);
    let fx = h.peer(1, Some(40_000));
    // Our handshake goes out first, then requests (two in flight per peer).
    assert!(matches!(
        fx.first(),
        Some(Effect::Send {
            message: Message::Extended { id: 0, .. },
            ..
        })
    ));
    assert_eq!(requests(&fx), [(1, 0), (1, 1)]);
    let fx = h.peer(2, Some(40_000));
    assert_eq!(requests(&fx), [(2, 2)]);
    // The peer's availability is remembered for the hand-over.
    h.msg(1, Message::HaveAll);
    h.msg(1, Message::Unchoke);
    h.msg(2, Message::Have { piece: 3 });

    let content = info();
    assert!(received(&h.msg(1, data(0, &content))).is_none());
    assert!(received(&h.msg(2, data(2, &content))).is_none());
    let fx = h.msg(1, data(1, &content));
    assert_eq!(received(&fx), Some(content));
    assert!(fx.contains(&Effect::Alert(Alert::MetadataReceived)));
    assert!(h.m.is_complete());
    assert!(
        h.msg(1, Message::Choke).is_empty(),
        "done: events are ignored"
    );

    let handover = h.m.into_handover();
    assert!(handover.started_announced);
    let [one, two] = handover.peers.as_slice() else {
        panic!("{handover:?}");
    };
    assert_eq!((one.peer, one.addr), (PeerKey(1), addr(1)));
    assert!(matches!(one.replay[0], Message::Extended { id: 0, .. }));
    assert_eq!(one.replay[1..], [Message::HaveAll, Message::Unchoke]);
    assert_eq!(two.replay[1..], [Message::Have { piece: 3 }]);
}

#[test]
fn bad_metadata_drops_its_sources_and_starts_over() {
    let mut h = Harness::new(Config::default());
    h.peer(1, Some(40_000));
    let mut wrong = info().to_vec();
    wrong[0] ^= 0xff;
    h.msg(1, data(0, &wrong));
    h.msg(1, data(1, &wrong));
    let fx = h.msg(1, data(2, &wrong));
    assert!(received(&fx).is_none());
    assert!(fx.contains(&Effect::Disconnect { peer: PeerKey(1) }));
    // An honest peer then provides it from scratch.
    let fx = h.peer(2, Some(40_000));
    assert_eq!(requests(&fx), [(2, 0), (2, 1)]);
    let content = info();
    assert_eq!(requests(&h.msg(2, data(0, &content))), [(2, 2)]);
    h.msg(2, data(1, &content));
    assert_eq!(received(&h.msg(2, data(2, &content))), Some(content));
}

#[test]
fn rejects_timeouts_and_bad_sizes() {
    let mut config = Config::default();
    config.max_metadata_size = 50_000;
    let mut h = Harness::new(config);
    // Oversized and missing sizes are not trusted.
    assert!(requests(&h.peer(1, Some(60_000))).is_empty());
    assert!(requests(&h.peer(2, None)).is_empty());
    assert_eq!(requests(&h.peer(3, Some(40_000))), [(3, 0), (3, 1)]);
    // Peer 1 claims another size: never asked.
    let reject = |piece| Message::Extended {
        id: OURS,
        payload: MetadataMessage::Reject { piece }.encode().unwrap(),
    };
    let fx = h.msg(3, reject(0));
    assert_eq!(
        requests(&fx),
        [(3, 2)],
        "a reject moves that piece on and leaves the peer's other pieces"
    );
    // A fourth peer picks up the rejected piece.
    assert_eq!(requests(&h.peer(4, Some(40_000))), [(4, 0)]);
    // Every request times out. Each piece goes to the peer that did not fail it.
    h.now += Config::default().request_timeout;
    let fx = h.ev(Event::Tick);
    let again = requests(&fx);
    assert!(again.contains(&(3, 0)), "{again:?}");
    assert!(
        again.contains(&(4, 1)) && again.contains(&(4, 2)),
        "{again:?}"
    );
    assert!(!again.contains(&(3, 1)), "peer 3 just timed out on piece 1");
    assert!(!again.contains(&(4, 0)), "peer 4 just timed out on piece 0");
    assert!(
        fx.iter()
            .any(|e| matches!(e, Effect::Alert(Alert::Stats(_))))
    );
    let content = info();
    h.msg(4, data(1, &content));
    h.msg(4, data(2, &content));
    assert_eq!(received(&h.msg(3, data(0, &content))), Some(content));
}

#[test]
fn timeout_retries_the_only_peer_later() {
    let mut h = Harness::new(Config::default());
    assert_eq!(requests(&h.peer(1, Some(40_000))), [(1, 0), (1, 1)]);
    let timeout = Config::default().request_timeout;
    h.now += timeout;
    // Pieces 0 and 1 just timed out, so only the untouched piece is asked.
    assert_eq!(requests(&h.ev(Event::Tick)), [(1, 2)]);
    h.now += timeout;
    assert_eq!(requests(&h.ev(Event::Tick)), [(1, 0), (1, 1)]);
}

#[test]
fn stalled_metadata_size_is_abandoned() {
    let mut h = Harness::new(Config::default());
    h.peer(1, Some(100));
    h.now += Config::default().request_timeout;
    // Peer 2's size is ignored while 100 is still the adopted size.
    assert!(requests(&h.peer(2, Some(40_000))).is_empty());
    let fx = h.ev(Event::Tick);
    let asked = requests(&fx);
    assert!(asked.iter().all(|(peer, _)| *peer == 2), "{asked:?}");
    assert!(!asked.is_empty());
}

#[test]
fn bad_metadata_is_not_dialed_again() {
    let mut h = Harness::new(Config::default());
    h.peer(1, Some(40_000));
    let mut wrong = info().to_vec();
    wrong[0] ^= 0xff;
    h.msg(1, data(0, &wrong));
    h.msg(1, data(1, &wrong));
    h.msg(1, data(2, &wrong));
    let fx = h.ev(Event::AddPeers {
        peers: vec![addr(1)],
    });
    assert!(!fx.contains(&Effect::Connect { addr: addr(1) }), "{fx:?}");
}

#[test]
fn stop_after_metadata_finishes_the_task() {
    let mut h = Harness::new(Config::default());
    h.peer(1, Some(40_000));
    let content = info();
    h.msg(1, data(0, &content));
    h.msg(1, data(1, &content));
    assert!(received(&h.msg(1, data(2, &content))).is_some());
    let fx = h.ev(Event::Stop);
    assert_eq!(h.m.status(), TorrentStatus::Stopped);
    assert!(fx.contains(&Effect::Disconnect { peer: PeerKey(1) }));
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Announce {
            event: AnnounceEvent::Stopped,
            ..
        }
    )));
}

#[test]
fn failed_started_announce_is_retried() {
    let mut h = Harness::new(Config::default());
    h.ev(Event::AnnounceFailed {
        error: "down".into(),
    });
    h.now += Config::default().announce_retry;
    let fx = h.ev(Event::Tick);
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Announce {
            event: AnnounceEvent::Started,
            ..
        }
    )));
}

#[test]
fn trackers_dht_and_stop() {
    let mut config = Config::default();
    config.dht = true;
    let mut h = Harness::new(config);
    // Start already announced and asked the DHT; peers from both connect.
    let fx = h.ev(Event::DhtPeers {
        peers: vec![addr(5)],
        failed: false,
    });
    assert!(fx.contains(&Effect::Connect { addr: addr(5) }));
    let fx = h.ev(Event::Announced {
        peers: vec![addr(6)],
        interval: Some(Duration::from_secs(600)),
    });
    assert!(fx.contains(&Effect::Connect { addr: addr(6) }));
    h.ev(Event::PeerConnected {
        peer: PeerKey(5),
        addr: addr(5),
        reserved: Reserved::new(),
    });
    let fx = h.ev(Event::Stop);
    assert!(fx.contains(&Effect::Disconnect { peer: PeerKey(5) }));
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Announce {
            event: AnnounceEvent::Stopped,
            ..
        }
    )));
    assert_eq!(h.m.status(), TorrentStatus::Stopped);
    let handover = h.m.into_handover();
    assert!(handover.swarm.is_empty(), "stopped state has no peers");
}

#[test]
fn handover_keeps_dials_in_flight_and_a_failed_started() {
    let mut h = Harness::new(Config::default());
    h.ev(Event::AddPeers {
        peers: vec![addr(4), addr(5)],
    });
    let handover = h.m.into_handover();
    assert_eq!(handover.connecting, vec![addr(4), addr(5)]);
    assert!(handover.known.is_empty(), "{handover:?}");
    assert!(handover.started_announced);
    assert!(handover.started_pending);

    let mut failed = Harness::new(Config::default());
    failed.ev(Event::AnnounceFailed {
        error: "down".into(),
    });
    let handover = failed.m.into_handover();
    assert!(!handover.started_announced);
    assert!(!handover.started_pending);
}

#[test]
fn dht_and_pex_addresses_are_marked_for_a_private_handover() {
    let mut config = Config::default();
    config.dht = true;
    config.pex = true;
    let mut h = Harness::new(config);
    h.ev(Event::DhtPeers {
        peers: vec![addr(5)],
        failed: false,
    });
    h.ev(Event::Announced {
        peers: vec![addr(6)],
        interval: None,
    });
    let handover = h.m.into_handover();
    assert_eq!(handover.swarm, vec![addr(5)]);
    assert!(!handover.swarm.contains(&addr(6)));
    assert!(
        handover.known.contains(&addr(6)) || handover.connecting.contains(&addr(6)),
        "{handover:?}"
    );
}
