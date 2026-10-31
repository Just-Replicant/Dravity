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
use dravity_core::merkle;
use dravity_core::{InfoHash, InfoHashV2};
use dravity_engine::{
    Alert, AnnounceEvent, Config, DialHints, Effect, Event, MetadataState, PeerKey, PeerLink,
    TorrentStatus,
};
use dravity_wire::{ExtendedHandshake, HashRequest, Message, MetadataMessage, Reserved};

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
        Self::with(MetadataState::new(
            InfoHash::from_info_bytes(&info()),
            config,
        ))
    }

    fn with(m: MetadataState) -> Self {
        let mut h = Self {
            m,
            now: Instant::now(),
        };
        h.ev(Event::Start);
        h
    }

    /// Feeds all three pieces of `content` from one peer.
    fn fetch(&mut self, content: &[u8]) -> Vec<Effect> {
        self.peer(1, Some(content.len() as u64));
        let mut fx = Vec::new();
        for piece in 0..3 {
            fx.extend(self.msg(1, data(piece, content)));
        }
        fx
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
        self.peer_with(peer, size, Reserved::new().with_extension_protocol(true))
    }

    /// Like [`Self::peer`], with the handshake's `reserved` bytes.
    fn peer_with(&mut self, peer: u64, size: Option<u64>, reserved: Reserved) -> Vec<Effect> {
        self.ev(Event::AddPeers {
            peers: vec![addr(peer as u16)],
        });
        let mut fx = self.ev(Event::PeerConnected {
            peer: PeerKey(peer),
            addr: addr(peer as u16),
            reserved,
            link: PeerLink::default(),
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
        Effect::MetadataReceived { info, .. } => Some(info.clone()),
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
    assert!(
        !fx.contains(&Effect::Connect {
            addr: addr(1),
            hints: DialHints::default()
        }),
        "{fx:?}"
    );
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
    assert!(fx.contains(&Effect::Connect {
        addr: addr(5),
        hints: DialHints::default()
    }));
    let fx = h.ev(Event::Announced {
        peers: vec![addr(6)],
        interval: Some(Duration::from_secs(600)),
    });
    assert!(fx.contains(&Effect::Connect {
        addr: addr(6),
        hints: DialHints::default()
    }));
    h.ev(Event::PeerConnected {
        peer: PeerKey(5),
        addr: addr(5),
        reserved: Reserved::new(),
        link: PeerLink::default(),
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

#[test]
fn accepted_peers_fetch_metadata_and_are_marked_incoming() {
    let mut h = Harness::new(Config::default());
    // An accepted connection was never dialed: no pool entry is needed.
    let fx = h.ev(Event::PeerAccepted {
        peer: PeerKey(7),
        addr: SocketAddr::from(([10, 0, 9, 1], 51_000)),
        reserved: Reserved::new().with_extension_protocol(true),
        link: PeerLink::default(),
    });
    assert!(matches!(
        fx.first(),
        Some(Effect::Send {
            message: Message::Extended { id: 0, .. },
            ..
        })
    ));
    let hs = ExtendedHandshake {
        extensions: vec![("ut_metadata".into(), THEIRS)],
        listen_port: None,
        client: None,
        your_ip: None,
        request_queue: None,
        metadata_size: Some(40_000),
        ipv4: None,
        ipv6: None,
    };
    let fx = h.msg(
        7,
        Message::Extended {
            id: 0,
            payload: Bytes::from(hs.encode().unwrap()),
        },
    );
    assert_eq!(requests(&fx), [(7, 0), (7, 1)]);
    let fx = h.peer(1, Some(40_000));
    assert_eq!(requests(&fx), [(1, 2)]);
    assert_eq!(h.m.stats().peers, 2);

    let content = info();
    h.msg(7, data(0, &content));
    h.msg(1, data(2, &content));
    assert_eq!(received(&h.msg(7, data(1, &content))), Some(content));
    let handover = h.m.into_handover();
    let incoming: Vec<_> = handover
        .peers
        .iter()
        .map(|p| (p.peer, p.incoming))
        .collect();
    assert_eq!(incoming, [(PeerKey(1), false), (PeerKey(7), true)]);
}

#[test]
fn v2_magnets_verify_with_sha256() {
    let content = info();
    let v2 = InfoHashV2::from_info_bytes(&content);
    let mut h = Harness::with(MetadataState::new_v2(v2, Config::default()));
    assert_eq!(h.m.info_hash(), v2.truncated());
    assert_eq!(received(&h.fetch(&content)), Some(content.clone()));

    // A wrong v2 hash rejects metadata whose truncated hash we asked for.
    let mut h = Harness::with(MetadataState::new_v2(
        InfoHashV2::new([3; 32]),
        Config::default(),
    ));
    assert_eq!(received(&h.fetch(&content)), None);
    assert!(!h.m.is_complete());
}

#[test]
fn hybrid_magnets_check_both_hashes() {
    let content = info();
    let v1 = InfoHash::from_info_bytes(&content);
    let good = MetadataState::new(v1, Config::default())
        .with_info_hash_v2(InfoHashV2::from_info_bytes(&content));
    assert_eq!(
        received(&Harness::with(good).fetch(&content)),
        Some(content.clone())
    );
    let bad = MetadataState::new(v1, Config::default()).with_info_hash_v2(InfoHashV2::new([4; 32]));
    assert_eq!(received(&Harness::with(bad).fetch(&content)), None);
}

/// A v2-only `info` with one file of `len` bytes in 16 KiB pieces, and the
/// file's piece layer.
fn v2_info(len: usize) -> (Bytes, merkle::FileHashes) {
    let data: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
    let tree = merkle::file_hashes(&data, 16_384).unwrap();
    let mut info = format!("d9:file treed5:a.bind0:d6:lengthi{len}e11:pieces root32:").into_bytes();
    info.extend_from_slice(&tree.root);
    info.extend_from_slice(b"eee12:meta versioni2e4:name5:a.bin12:piece lengthi16384ee");
    (Bytes::from(info), tree)
}

/// Every hash request in `fx`.
fn hash_requests(fx: &[Effect]) -> Vec<(u64, HashRequest)> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::Send {
                peer,
                message: Message::HashRequest(r),
            } => Some((peer.0, *r)),
            _ => None,
        })
        .collect()
}

fn answer(tree: &merkle::FileHashes, r: HashRequest) -> Message {
    let proof = merkle::layer_proof(
        &tree.layer,
        16_384,
        r.base_layer,
        r.index as usize,
        r.length as usize,
        r.proof_layers,
    )
    .unwrap();
    Message::Hashes {
        request: r,
        hashes: proof
            .base
            .iter()
            .chain(&proof.uncles)
            .flatten()
            .copied()
            .collect::<Vec<u8>>()
            .into(),
    }
}

#[test]
fn v2_magnets_fetch_piece_layers_with_hash_requests() {
    let (info, tree) = v2_info(5 * 16_384 + 100);
    assert_eq!(tree.layer.len(), 6);
    let v2 = InfoHashV2::from_info_bytes(&info);
    let mut h = Harness::with(MetadataState::new_v2(v2, Config::default()));
    h.peer(1, Some(info.len() as u64));
    let fx = h.msg(1, data(0, &info));
    // The info is verified, but the layer is missing: no hand-over yet.
    assert_eq!(received(&fx), None);
    assert!(!h.m.is_complete());
    let asked = hash_requests(&fx);
    assert_eq!(asked.len(), 1);
    let (peer, r) = asked[0];
    assert_eq!(peer, 1);
    assert_eq!(r.pieces_root, tree.root);
    assert_eq!(
        (r.base_layer, r.index, r.length, r.proof_layers),
        (0, 0, 8, 2)
    );

    // A wrong proof drops the peer and the block goes to the next one.
    let mut bad = tree.clone();
    bad.layer[2][0] ^= 1;
    let fx = h.msg(1, answer(&bad, r));
    assert!(fx.contains(&Effect::Disconnect { peer: PeerKey(1) }));
    assert_eq!(received(&fx), None);
    let fx = h.peer(2, Some(info.len() as u64));
    let asked = hash_requests(&fx);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].0, 2);

    // A reject frees the block; a correct answer completes the metadata.
    h.msg(2, Message::HashReject(asked[0].1));
    let fx = h.peer(3, Some(info.len() as u64));
    let (peer, r) = hash_requests(&fx)[0];
    assert_eq!(peer, 3);
    let fx = h.msg(3, answer(&tree, r));
    let layers = fx
        .iter()
        .find_map(|e| match e {
            Effect::MetadataReceived {
                info: got,
                piece_layers,
            } => {
                assert_eq!(*got, info);
                Some(piece_layers.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(layers, vec![(tree.root, tree.layer.clone())]);
    assert!(h.m.is_complete());
}

#[test]
fn large_piece_layers_are_fetched_in_blocks_with_uncle_proofs() {
    // 600 pieces pad to 1024: two blocks of 512 hashes, each needing one
    // uncle hash to reach the pieces root.
    let (info, tree) = v2_info(600 * 16_384);
    assert_eq!(tree.layer.len(), 600);
    let v2 = InfoHashV2::from_info_bytes(&info);
    let mut h = Harness::with(MetadataState::new_v2(v2, Config::default()));
    h.peer(1, Some(info.len() as u64));
    let fx = h.msg(1, data(0, &info));
    let asked = hash_requests(&fx);
    let wanted: Vec<_> = asked
        .iter()
        .map(|(_, r)| (r.base_layer, r.index, r.length, r.proof_layers))
        .collect();
    assert_eq!(wanted, vec![(0, 0, 512, 9), (0, 512, 512, 9)]);

    // Each answer carries 512 base hashes and exactly one uncle.
    let good = answer(&tree, asked[0].1);
    let Message::Hashes { hashes, .. } = &good else {
        panic!("expected hashes");
    };
    assert_eq!(hashes.len(), 513 * 32);

    // A tampered uncle fails the proof even though every base hash is right.
    let mut forged = hashes.to_vec();
    let last = forged.len() - 1;
    forged[last] ^= 1;
    let fx = h.msg(
        1,
        Message::Hashes {
            request: asked[0].1,
            hashes: forged.into(),
        },
    );
    assert!(fx.contains(&Effect::Disconnect { peer: PeerKey(1) }));

    // A second peer answers both blocks and completes the layer.
    let fx = h.peer(2, Some(info.len() as u64));
    let asked = hash_requests(&fx);
    assert_eq!(asked.len(), 2);
    let mut done = None;
    for (peer, r) in asked {
        assert_eq!(peer, 2);
        let fx = h.msg(2, answer(&tree, r));
        assert!(!fx.contains(&Effect::Disconnect { peer: PeerKey(2) }));
        done = done.or_else(|| {
            fx.iter().find_map(|e| match e {
                Effect::MetadataReceived { piece_layers, .. } => Some(piece_layers.clone()),
                _ => None,
            })
        });
    }
    assert_eq!(done, Some(vec![(tree.root, tree.layer.clone())]));
    assert!(h.m.is_complete());
}

/// A hybrid `info` (v1 and v2) with one file of `len` bytes in 16 KiB
/// pieces, and the file's piece layer.
fn hybrid_info(len: usize) -> (Bytes, merkle::FileHashes) {
    use sha1::{Digest, Sha1};
    let data: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
    let tree = merkle::file_hashes(&data, 16_384).unwrap();
    let pieces: Vec<u8> = data
        .chunks(16_384)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let mut info = format!("d9:file treed5:a.bind0:d6:lengthi{len}e11:pieces root32:").into_bytes();
    info.extend_from_slice(&tree.root);
    info.extend_from_slice(
        format!(
            "eee6:lengthi{len}e12:meta versioni2e4:name5:a.bin12:piece lengthi16384e6:pieces{}:",
            pieces.len()
        )
        .as_bytes(),
    );
    info.extend_from_slice(&pieces);
    info.push(b'e');
    (Bytes::from(info), tree)
}

/// The piece layers of the `MetadataReceived` in `fx`, if any.
fn received_layers(fx: &[Effect]) -> Option<Vec<(merkle::Hash, Vec<merkle::Hash>)>> {
    fx.iter().find_map(|e| match e {
        Effect::MetadataReceived { piece_layers, .. } => Some(piece_layers.clone()),
        _ => None,
    })
}

fn v2_peer() -> Reserved {
    Reserved::new().with_extension_protocol(true).with_v2(true)
}

#[test]
fn hybrid_magnets_fetch_piece_layers_from_v2_peers() {
    let (info, tree) = hybrid_info(5 * 16_384 + 100);
    assert_eq!(tree.layer.len(), 6);
    // A `btih` magnet: the SHA-1 info-hash checks the metadata.
    let mut h = Harness::with(MetadataState::new(
        InfoHash::from_info_bytes(&info),
        Config::default(),
    ));
    h.peer(1, Some(info.len() as u64));
    let fx = h.msg(1, data(0, &info));
    // Verified, but the hand-over waits for the layer. Peer 1 does not
    // advertise BEP 52, so it is not asked.
    assert_eq!(received(&fx), None);
    assert!(hash_requests(&fx).is_empty());
    assert!(!h.m.is_complete());

    let fx = h.peer_with(2, Some(info.len() as u64), v2_peer());
    let asked = hash_requests(&fx);
    assert_eq!(asked.len(), 1);
    let (peer, r) = asked[0];
    assert_eq!((peer, r.pieces_root), (2, tree.root));

    // A forged proof drops the peer like for a v2-only magnet.
    let mut bad = tree.clone();
    bad.layer[0][0] ^= 1;
    let fx = h.msg(2, answer(&bad, r));
    assert!(fx.contains(&Effect::Disconnect { peer: PeerKey(2) }));
    assert_eq!(received(&fx), None);

    let fx = h.peer_with(3, Some(info.len() as u64), v2_peer());
    let (peer, r) = hash_requests(&fx)[0];
    assert_eq!(peer, 3);
    let fx = h.msg(3, answer(&tree, r));
    assert_eq!(received(&fx), Some(info));
    assert_eq!(
        received_layers(&fx),
        Some(vec![(tree.root, tree.layer.clone())])
    );
    assert!(h.m.is_complete());
}

#[test]
fn hybrid_magnets_start_on_sha1_when_no_peer_sends_layers() {
    let (info, _) = hybrid_info(5 * 16_384 + 100);
    let config = Config::default();
    let timeout = config.piece_layer_timeout;
    let mut h = Harness::with(MetadataState::new(InfoHash::from_info_bytes(&info), config));
    h.peer_with(1, Some(info.len() as u64), v2_peer());
    let fx = h.msg(1, data(0, &info));
    let (_, r) = hash_requests(&fx)[0];
    // The peer rejects the request; nobody else has the layer.
    h.msg(1, Message::HashReject(r));
    h.now += timeout - Duration::from_millis(1);
    assert_eq!(received(&h.ev(Event::Tick)), None);
    h.now += Duration::from_millis(1);
    let fx = h.ev(Event::Tick);
    assert_eq!(received(&fx), Some(info));
    assert_eq!(received_layers(&fx), Some(Vec::new()));
    assert!(h.m.is_complete());
    // Nothing more is asked once the metadata is handed over.
    assert!(hash_requests(&h.ev(Event::Tick)).is_empty());
}

#[test]
fn v2_only_magnets_wait_for_their_layers() {
    let (info, _) = v2_info(5 * 16_384 + 100);
    let config = Config::default();
    let timeout = config.piece_layer_timeout;
    let mut h = Harness::with(MetadataState::new_v2(
        InfoHashV2::from_info_bytes(&info),
        config,
    ));
    h.peer(1, Some(info.len() as u64));
    h.msg(1, data(0, &info));
    h.now += timeout * 2;
    assert_eq!(received(&h.ev(Event::Tick)), None);
    assert!(!h.m.is_complete());
}

#[test]
fn small_v2_files_need_no_hash_requests() {
    let (info, tree) = v2_info(10_000);
    assert!(tree.layer.is_empty());
    let mut h = Harness::with(MetadataState::new_v2(
        InfoHashV2::from_info_bytes(&info),
        Config::default(),
    ));
    h.peer(1, Some(info.len() as u64));
    let fx = h.msg(1, data(0, &info));
    assert!(hash_requests(&fx).is_empty());
    assert_eq!(received(&fx), Some(info));
}

#[test]
fn hash_requests_are_rejected_before_the_layers_are_known() {
    let mut h = Harness::new(Config::default());
    h.peer(1, None);
    let r = HashRequest {
        pieces_root: [1; 32],
        base_layer: 0,
        index: 0,
        length: 2,
        proof_layers: 0,
    };
    let fx = h.msg(1, Message::HashRequest(r));
    assert!(fx.contains(&Effect::Send {
        peer: PeerKey(1),
        message: Message::HashReject(r)
    }));
}

#[test]
fn dial_hints_and_links_survive_the_handover() {
    let mut config = Config::default();
    config.pex = true;
    let mut h = Harness::new(config);
    h.ev(Event::AddPeers {
        peers: vec![addr(1)],
    });
    let link = PeerLink {
        utp: true,
        mse: Some(true),
    };
    h.ev(Event::PeerConnected {
        peer: PeerKey(1),
        addr: addr(1),
        reserved: Reserved::new().with_extension_protocol(true),
        link,
    });
    let pex = dravity_wire::PexMessage {
        added: vec![dravity_wire::PexPeer {
            addr: addr(7),
            flags: Some(dravity_wire::PEX_ENCRYPTION),
        }],
        dropped: Vec::new(),
    };
    let fx = h.msg(
        1,
        Message::Extended {
            id: 2,
            payload: Bytes::from(pex.encode()),
        },
    );
    let pex_hints = DialHints {
        encryption: Some(true),
        utp: Some(false),
    };
    assert!(fx.contains(&Effect::Connect {
        addr: addr(7),
        hints: pex_hints
    }));
    let handover = h.m.into_handover();
    assert_eq!(handover.peers[0].link, link);
    assert_eq!(
        handover.hints,
        vec![(addr(1), DialHints::from_link(link)), (addr(7), pex_hints)]
    );
}
