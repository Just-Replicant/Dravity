//! Phase 3 engine behavior: seeding, choking, the BEP 6 fast extension and
//! peer bans, scripted against `TorrentState::update`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::panic
)]

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dravity_core::{BLOCK_SIZE, BlockRef, InfoHash, PieceLayout};
use dravity_engine::{
    Alert, AnnounceEvent, Bitfield, Config, Effect, Event, PeerKey, TorrentState, TorrentStatus,
};
use dravity_wire::{BlockRequest, ExtendedHandshake, Message, PexMessage, Reserved};

/// 3 pieces of 2 blocks each, last piece shorter.
fn layout() -> PieceLayout {
    PieceLayout::new(2 * BLOCK_SIZE, u64::from(5 * BLOCK_SIZE) + 10).unwrap()
}

/// Peer `n` lives on its own IPv4 address, so bans hit one peer only.
fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, n as u8, 1], 6881))
}

fn config() -> Config {
    let mut c = Config::default();
    c.check_existing = false;
    c
}

fn fast() -> Reserved {
    Reserved::new().with_fast(true)
}

fn req(piece: u32, offset: u32, length: u32) -> BlockRequest {
    BlockRequest {
        piece,
        offset,
        length,
    }
}

fn block(r: BlockRequest) -> BlockRef {
    BlockRef {
        piece: r.piece,
        offset: r.offset,
        length: r.length,
    }
}

struct Harness {
    t: TorrentState,
    now: Instant,
}

impl Harness {
    /// A torrent with `have` pieces, started.
    fn with_have(config: Config, have: &[u32]) -> Self {
        let mut bits = Bitfield::new(3);
        for &p in have {
            bits.set(p);
        }
        let mut h = Self {
            t: TorrentState::with_have(layout(), bits, config)
                .with_info_hash(InfoHash::new([0xaa; 20])),
            now: Instant::now(),
        };
        h.ev(Event::Start);
        h
    }

    fn seed(config: Config) -> Self {
        Self::with_have(config, &[0, 1, 2])
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

    fn accept(&mut self, peer: u64, reserved: Reserved) -> Vec<Effect> {
        self.ev(Event::PeerAccepted {
            peer: PeerKey(peer),
            addr: addr(peer as u16),
            reserved,
        })
    }

    fn connect(&mut self, peer: u64, reserved: Reserved) -> Vec<Effect> {
        self.ev(Event::PeerConnected {
            peer: PeerKey(peer),
            addr: addr(peer as u16),
            reserved,
        })
    }

    /// Answers every read in `fx` and returns the resulting effects.
    fn complete_reads(&mut self, fx: &[Effect]) -> Vec<Effect> {
        let mut out = Vec::new();
        for e in fx {
            if let Effect::ReadBlock { peer, block } = e {
                out.extend(self.ev(Event::BlockRead {
                    peer: *peer,
                    block: *block,
                    data: Bytes::from(vec![1u8; block.length as usize]),
                }));
            }
        }
        out
    }
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

fn reads(effects: &[Effect]) -> Vec<(u64, BlockRef)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::ReadBlock { peer, block } => Some((peer.0, *block)),
            _ => None,
        })
        .collect()
}

fn pieces_sent(effects: &[Effect], peer: u64) -> Vec<(u32, u32)> {
    sent(effects, peer)
        .into_iter()
        .filter_map(|m| match m {
            Message::Piece { piece, offset, .. } => Some((piece, offset)),
            _ => None,
        })
        .collect()
}

fn requests(effects: &[Effect], peer: u64) -> Vec<BlockRequest> {
    sent(effects, peer)
        .into_iter()
        .filter_map(|m| match m {
            Message::Request(r) => Some(r),
            _ => None,
        })
        .collect()
}

fn disconnects(effects: &[Effect], peer: u64) -> bool {
    effects.contains(&Effect::Disconnect {
        peer: PeerKey(peer),
    })
}

#[test]
fn a_seed_serves_interested_peers() {
    let mut h = Harness::seed(config());
    assert_eq!(h.t.status(), TorrentStatus::Finished);
    let fx = h.accept(1, Reserved::new());
    assert_eq!(
        sent(&fx, 1),
        [Message::Bitfield(Bitfield::full(3).to_bytes())]
    );
    // Interest is answered with an unchoke right away (free slot).
    assert!(sent(&h.msg(1, Message::Interested), 1).contains(&Message::Unchoke));
    let fx = h.msg(1, Message::Request(req(0, 0, BLOCK_SIZE)));
    assert_eq!(reads(&fx), [(1, block(req(0, 0, BLOCK_SIZE)))]);
    let fx = h.complete_reads(&fx);
    assert_eq!(pieces_sent(&fx, 1), [(0, 0)]);
    assert_eq!(h.t.stats().uploaded, u64::from(BLOCK_SIZE));
    // The short last block too.
    let fx = h.msg(1, Message::Request(req(2, 0, BLOCK_SIZE + 10)));
    assert!(reads(&fx).is_empty(), "longer than a block");
    let fx = h.msg(1, Message::Request(req(2, BLOCK_SIZE, 10)));
    assert_eq!(pieces_sent(&h.complete_reads(&fx), 1), [(2, BLOCK_SIZE)]);
    let stats = h.t.stats();
    assert_eq!((stats.unchoked, stats.interested), (1, 1));
}

#[test]
fn requests_we_cannot_serve_are_rejected_only_with_the_fast_extension() {
    let mut c = config();
    // No allowed fast set: a choked peer may download nothing.
    c.allowed_fast = 0;
    let mut h = Harness::with_have(c, &[0]);
    assert_eq!(h.t.status(), TorrentStatus::Downloading);
    h.accept(1, Reserved::new());
    let fx = h.accept(2, fast());
    // Fast peers learn our pieces too, and get the allowed fast set.
    assert_eq!(sent(&fx, 2)[0], Message::Bitfield(h.t.have().to_bytes()));
    for peer in [1, 2] {
        // Choked: nothing is served.
        let fx = h.msg(peer, Message::Request(req(0, 0, BLOCK_SIZE)));
        assert!(reads(&fx).is_empty());
        let rejected = sent(&fx, peer).contains(&Message::Reject(req(0, 0, BLOCK_SIZE)));
        assert_eq!(rejected, peer == 2);
    }
    for peer in [1, 2] {
        h.msg(peer, Message::Interested);
    }
    // Unchoked, but we do not have piece 1 and piece 9 does not exist.
    for bad in [
        req(1, 0, BLOCK_SIZE),
        req(9, 0, BLOCK_SIZE),
        req(0, 2 * BLOCK_SIZE - 1, 2),
    ] {
        let fx = h.msg(2, Message::Request(bad));
        assert!(reads(&fx).is_empty(), "{bad:?}");
        assert_eq!(sent(&fx, 2), [Message::Reject(bad)]);
        assert!(sent(&h.msg(1, Message::Request(bad)), 1).is_empty());
    }
}

#[test]
fn have_all_and_have_none_go_to_fast_peers() {
    let mut h = Harness::seed(config());
    assert_eq!(sent(&h.accept(1, fast()), 1)[0], Message::HaveAll);
    let mut h = Harness::with_have(config(), &[]);
    assert_eq!(sent(&h.accept(1, fast()), 1), [Message::HaveNone]);
    // Without the extension, nothing at all is sent for no pieces.
    assert!(sent(&h.accept(2, Reserved::new()), 2).is_empty());
}

#[test]
fn cancels_drop_queued_and_in_flight_reads() {
    let mut c = config();
    c.max_pending_reads = 1;
    let mut h = Harness::seed(c);
    h.accept(1, fast());
    h.msg(1, Message::Interested);
    let first = req(0, 0, BLOCK_SIZE);
    let second = req(0, BLOCK_SIZE, BLOCK_SIZE);
    let fx = h.msg(1, Message::Request(first));
    assert_eq!(reads(&fx).len(), 1);
    // One read at a time: the second waits in the queue.
    assert!(reads(&h.msg(1, Message::Request(second))).is_empty());
    // A duplicate request is ignored.
    assert!(sent(&h.msg(1, Message::Request(second)), 1).is_empty());
    assert_eq!(h.t.stats().pending_reads, 1);
    // Cancelling the queued one rejects it (BEP 6).
    assert_eq!(
        sent(&h.msg(1, Message::Cancel(second)), 1),
        [Message::Reject(second)]
    );
    // Cancelling the one being read drops the data when it arrives.
    assert_eq!(
        sent(&h.msg(1, Message::Cancel(first)), 1),
        [Message::Reject(first)]
    );
    let fx = h.complete_reads(&fx);
    assert!(pieces_sent(&fx, 1).is_empty());
    assert_eq!(h.t.stats().pending_reads, 0);
    assert_eq!(h.t.stats().uploaded, 0);
    // A cancel for something never asked is ignored.
    assert!(sent(&h.msg(1, Message::Cancel(first)), 1).is_empty());
}

#[test]
fn reads_are_bounded_and_shared_between_peers() {
    let mut c = config();
    c.max_pending_reads = 2;
    let mut h = Harness::seed(c);
    for p in [1, 2] {
        h.accept(p, Reserved::new());
        h.msg(p, Message::Interested);
    }
    let mut fx = Vec::new();
    for r in [
        req(0, 0, BLOCK_SIZE),
        req(0, BLOCK_SIZE, BLOCK_SIZE),
        req(1, 0, BLOCK_SIZE),
    ] {
        fx.extend(h.msg(1, Message::Request(r)));
    }
    fx.extend(h.msg(2, Message::Request(req(1, BLOCK_SIZE, BLOCK_SIZE))));
    assert_eq!(reads(&fx).len(), 2, "disk backpressure");
    // Completing one read lets the next peer in turn go first.
    let first = Effect::ReadBlock {
        peer: PeerKey(1),
        block: block(req(0, 0, BLOCK_SIZE)),
    };
    let next = h.complete_reads(&[first]);
    assert_eq!(reads(&next), [(2, block(req(1, BLOCK_SIZE, BLOCK_SIZE)))]);
}

#[test]
fn upload_rate_limit_paces_reads() {
    let mut c = config();
    c.upload_rate_limit = Some(u64::from(BLOCK_SIZE));
    let mut h = Harness::seed(c);
    h.accept(1, Reserved::new());
    h.msg(1, Message::Interested);
    let mut fx = Vec::new();
    for r in [
        req(0, 0, BLOCK_SIZE),
        req(0, BLOCK_SIZE, BLOCK_SIZE),
        req(1, 0, BLOCK_SIZE),
    ] {
        fx.extend(h.msg(1, Message::Request(r)));
    }
    assert_eq!(reads(&fx).len(), 1, "one block per second");
    assert!(reads(&h.advance(Duration::from_millis(100))).is_empty());
    assert_eq!(reads(&h.advance(Duration::from_secs(1))).len(), 1);
}

#[test]
fn choking_rejects_pending_requests_except_allowed_fast() {
    let mut c = config();
    c.upload_slots = 1;
    c.optimistic_unchoke = false;
    let mut h = Harness::seed(c);
    let fx = h.accept(1, fast());
    let allowed: Vec<u32> = sent(&fx, 1)
        .into_iter()
        .filter_map(|m| match m {
            Message::AllowedFast { piece } => Some(piece),
            _ => None,
        })
        .collect();
    assert!(!allowed.is_empty() && allowed.len() <= 3, "{allowed:?}");
    let fast_piece = allowed[0];
    let other = (0..3).find(|p| !allowed.contains(p));
    h.msg(1, Message::Interested);
    let max_reads = h.t.config().max_pending_reads;
    assert!(max_reads > 2);
    let kept = req(fast_piece, 0, BLOCK_SIZE);
    h.msg(1, Message::Request(kept));
    let dropped = other.map(|p| req(p, 0, BLOCK_SIZE));
    let mut fx = Vec::new();
    if let Some(r) = dropped {
        fx = h.msg(1, Message::Request(r));
    }
    // Peer 1 stops being interested: we choke it and reject what is not
    // in its allowed fast set.
    let choke = h.msg(1, Message::NotInterested);
    let msgs = sent(&choke, 1);
    assert_eq!(msgs[0], Message::Choke);
    if let Some(r) = dropped {
        assert!(msgs.contains(&Message::Reject(r)));
        assert!(pieces_sent(&h.complete_reads(&fx), 1).is_empty());
    }
    assert!(!msgs.contains(&Message::Reject(kept)));
    // An allowed fast piece is still served while choked.
    let fx = h.msg(1, Message::Request(req(fast_piece, BLOCK_SIZE, BLOCK_SIZE)));
    assert_eq!(reads(&fx).len(), 1);
}

#[test]
fn allowed_fast_set_follows_bep_6_for_ipv4_only() {
    let mut h = Harness::seed(config());
    let at = SocketAddr::from(([80, 4, 4, 200], 6881));
    let fx = h.ev(Event::PeerAccepted {
        peer: PeerKey(1),
        addr: at,
        reserved: fast(),
    });
    let mut expected = dravity_wire::allowed_fast_set(
        &InfoHash::new([0xaa; 20]),
        std::net::Ipv4Addr::new(80, 4, 4, 200),
        3,
        10,
    );
    expected.sort_unstable();
    let mut got: Vec<u32> = sent(&fx, 1)
        .into_iter()
        .filter_map(|m| match m {
            Message::AllowedFast { piece } => Some(piece),
            _ => None,
        })
        .collect();
    got.sort_unstable();
    assert_eq!(got, expected);
    let v6 = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 6881));
    let fx = h.ev(Event::PeerAccepted {
        peer: PeerKey(2),
        addr: v6,
        reserved: fast(),
    });
    assert_eq!(sent(&fx, 2), [Message::HaveAll]);
}

/// A downloading torrent (no pieces) with fast peer 1 that has everything.
fn leech_with_fast_seed(config: Config) -> Harness {
    let mut h = Harness::with_have(config, &[]);
    h.connect(1, fast());
    h.msg(1, Message::HaveAll);
    h
}

#[test]
fn allowed_fast_pieces_are_requested_while_choked() {
    let mut h = leech_with_fast_seed(config());
    let fx = h.msg(1, Message::AllowedFast { piece: 2 });
    let asked = requests(&fx, 1);
    assert!(
        !asked.is_empty() && asked.iter().all(|r| r.piece == 2),
        "{asked:?}"
    );
    // A rejected allowed fast piece is released and not asked again while
    // choked.
    let mut fx = Vec::new();
    for r in &asked {
        fx.extend(h.msg(1, Message::Reject(*r)));
    }
    assert!(requests(&fx, 1).is_empty());
    // An out-of-range allowed fast index is ignored.
    assert!(requests(&h.msg(1, Message::AllowedFast { piece: 7 }), 1).is_empty());
    // Unchoked: every piece, including the rejected one, is wanted again.
    let fx = h.msg(1, Message::Unchoke);
    assert!(requests(&fx, 1).iter().any(|r| r.piece == 2));
}

#[test]
fn a_fast_choke_keeps_requests_until_rejected() {
    let mut c = config();
    c.endgame = false;
    let mut h = leech_with_fast_seed(c);
    let asked = requests(&h.msg(1, Message::Unchoke), 1);
    assert_eq!(asked.len(), 4);
    h.msg(1, Message::Choke);
    // Nothing was released: a second peer only gets the other blocks.
    h.connect(2, Reserved::new());
    h.msg(2, Message::HaveAll);
    let taken = requests(&h.msg(2, Message::Unchoke), 2);
    assert_eq!(taken.len(), 2);
    assert!(taken.iter().all(|r| !asked.contains(r)), "{taken:?}");
    // The reject hands the block over; peer 1 is not asked again.
    let fx = h.msg(1, Message::Reject(asked[0]));
    assert!(requests(&fx, 1).is_empty());
    let fx = h.advance(Duration::from_secs(1));
    assert_eq!(requests(&fx, 2), [asked[0]]);
}

#[test]
fn fast_messages_without_the_extension_are_ignored() {
    let mut c = config();
    c.endgame = false;
    let mut h = Harness::with_have(c, &[]);
    h.connect(1, Reserved::new());
    h.msg(1, Message::HaveAll);
    assert!(requests(&h.msg(1, Message::AllowedFast { piece: 0 }), 1).is_empty());
    let asked = requests(&h.msg(1, Message::Unchoke), 1);
    // A `reject` from a peer without the extension does not release blocks.
    h.msg(1, Message::Reject(asked[0]));
    h.connect(2, Reserved::new());
    h.msg(2, Message::HaveAll);
    assert!(!requests(&h.msg(2, Message::Unchoke), 2).contains(&asked[0]));
}

/// Downloads `blocks` blocks from peer `p` (which sent `have all`) to give
/// it a rate.
fn feed(h: &mut Harness, p: u64, blocks: usize) {
    let asked = requests(&h.msg(p, Message::Unchoke), p);
    for r in asked.into_iter().take(blocks) {
        h.msg(
            p,
            Message::Piece {
                piece: r.piece,
                offset: r.offset,
                data: Bytes::from(vec![0; r.length as usize]),
            },
        );
    }
}

fn unchoked(fx: &[Effect], peer: u64) -> Option<bool> {
    sent(fx, peer).into_iter().rev().find_map(|m| match m {
        Message::Unchoke => Some(true),
        Message::Choke => Some(false),
        _ => None,
    })
}

#[test]
fn tit_for_tat_unchokes_the_peers_that_upload_to_us() {
    let mut c = config();
    c.upload_slots = 1;
    c.optimistic_unchoke = false;
    let mut h = Harness::with_have(c, &[0]);
    for p in 1..=3 {
        h.connect(p, Reserved::new());
    }
    h.msg(3, Message::HaveAll);
    // Everyone wants piece 0; the free slot goes to peer 1 straight away.
    assert_eq!(unchoked(&h.msg(1, Message::Interested), 1), Some(true));
    assert_eq!(unchoked(&h.msg(2, Message::Interested), 2), None);
    h.msg(3, Message::Interested);
    // Peer 3 sends us data; peer 1 does not.
    feed(&mut h, 3, 2);
    let fx = h.advance(Duration::from_secs(10));
    assert_eq!(unchoked(&fx, 3), Some(true));
    assert_eq!(unchoked(&fx, 1), Some(false));
    assert_eq!(unchoked(&fx, 2), None, "still choked");
    assert_eq!(h.t.stats().unchoked, 1);
}

#[test]
fn optimistic_unchoke_rotates() {
    let mut c = config();
    c.upload_slots = 0;
    let mut h = Harness::with_have(c, &[0]);
    let mut fx = Vec::new();
    for p in 1..=2 {
        h.connect(p, Reserved::new());
        fx.extend(h.msg(p, Message::Interested));
    }
    fx.extend(h.advance(Duration::from_secs(1)));
    // No regular slots: peer 1 (lowest key, equal wait) is optimistic.
    assert_eq!(unchoked(&fx, 1), Some(true));
    assert_eq!(unchoked(&fx, 2), None);
    assert_eq!(h.t.stats().unchoked, 1);
    // Thirty seconds later peer 2 gets its turn.
    let mut fx = Vec::new();
    for _ in 0..3 {
        fx.extend(h.advance(Duration::from_secs(10)));
        for p in 1..=2 {
            h.msg(p, Message::KeepAlive);
        }
    }
    assert_eq!(unchoked(&fx, 2), Some(true));
    assert_eq!(unchoked(&fx, 1), Some(false));
    assert_eq!(h.t.stats().unchoked, 1);
}

#[test]
fn snubbing_peers_lose_their_slot() {
    let mut c = config();
    c.upload_slots = 1;
    c.optimistic_unchoke = false;
    let mut h = Harness::with_have(c, &[0]);
    h.connect(1, Reserved::new());
    h.connect(2, Reserved::new());
    h.msg(1, Message::HaveAll);
    h.msg(1, Message::Interested);
    h.msg(2, Message::Interested);
    // Peer 1 unchokes us and sends one block, then nothing.
    feed(&mut h, 1, 1);
    let fx = h.advance(Duration::from_secs(10));
    assert_ne!(unchoked(&fx, 1), Some(false));
    let mut fx = Vec::new();
    for _ in 0..7 {
        fx.extend(h.advance(Duration::from_secs(10)));
        for p in 1..=2 {
            h.msg(p, Message::KeepAlive);
        }
    }
    assert_eq!(unchoked(&fx, 1), Some(false), "snubbed");
    assert_eq!(unchoked(&fx, 2), Some(true));
}

#[test]
fn a_seed_keeps_stable_slots_and_drops_other_seeds() {
    let mut c = config();
    c.upload_slots = 1;
    c.optimistic_unchoke = false;
    let mut h = Harness::seed(c);
    h.accept(1, Reserved::new());
    h.accept(2, Reserved::new());
    h.msg(1, Message::Interested);
    h.msg(2, Message::Interested);
    // Equal (zero) uploads: the round keeps the peer already unchoked.
    let fx = h.advance(Duration::from_secs(10));
    assert_eq!(unchoked(&fx, 1), None);
    assert_eq!(unchoked(&fx, 2), None);
    assert_eq!(h.t.stats().unchoked, 1);
    // Another seed is pointless to a seed.
    h.accept(3, Reserved::new());
    assert!(disconnects(&h.msg(3, Message::HaveAll), 3));
    h.accept(4, Reserved::new());
    assert!(disconnects(
        &h.msg(4, Message::Bitfield(Bitfield::full(3).to_bytes())),
        4
    ));
}

#[test]
fn finishing_drops_seeds_and_keeps_leechers() {
    let mut h = Harness::with_have(config(), &[0, 1]);
    h.connect(1, Reserved::new());
    h.msg(1, Message::HaveAll);
    h.connect(2, Reserved::new());
    h.msg(
        2,
        Message::Bitfield({
            let mut b = Bitfield::new(3);
            b.set(0);
            b.to_bytes()
        }),
    );
    let asked = requests(&h.msg(1, Message::Unchoke), 1);
    assert!(asked.iter().all(|r| r.piece == 2));
    for r in &asked {
        h.msg(
            1,
            Message::Piece {
                piece: r.piece,
                offset: r.offset,
                data: Bytes::from(vec![0; r.length as usize]),
            },
        );
        h.ev(Event::BlockWritten { block: block(*r) });
    }
    let fx = h.ev(Event::PieceHashed { piece: 2, ok: true });
    assert!(fx.contains(&Effect::Alert(Alert::Finished)));
    assert!(disconnects(&fx, 1), "the seed is dropped");
    assert!(!disconnects(&fx, 2), "the leecher stays");
    assert_eq!(h.t.status(), TorrentStatus::Finished);
    assert_eq!(h.t.stats().peers, 1);
}

#[test]
fn seed_false_leaves_the_swarm_when_done() {
    let mut c = config();
    c.seed = false;
    let mut h = Harness::seed(c);
    let fx = h.accept(1, Reserved::new());
    assert!(disconnects(&fx, 1));
    let fx = h.advance(Duration::from_secs(3600));
    assert!(!fx.iter().any(|e| matches!(e, Effect::Announce { .. })));
}

/// Downloads all of piece 0 from `senders` (alternating), then fails it.
fn fail_piece_zero(h: &mut Harness, senders: &[u64]) -> Vec<Effect> {
    let blocks = [req(0, 0, BLOCK_SIZE), req(0, BLOCK_SIZE, BLOCK_SIZE)];
    for p in senders {
        h.msg(*p, Message::HaveAll);
        h.msg(*p, Message::Unchoke);
    }
    for (i, r) in blocks.iter().enumerate() {
        let p = senders[i % senders.len()];
        h.msg(
            p,
            Message::Piece {
                piece: r.piece,
                offset: r.offset,
                data: Bytes::from(vec![0; r.length as usize]),
            },
        );
    }
    h.ev(Event::PieceHashed {
        piece: 0,
        ok: false,
    })
}

#[test]
fn the_only_sender_of_a_bad_piece_is_banned() {
    let mut c = config();
    c.hash_fail_ban = 2;
    c.max_requests_per_peer = 2;
    c.initial_requests_per_peer = 2;
    let mut h = Harness::with_have(c, &[]);
    h.connect(1, Reserved::new());
    let fx = fail_piece_zero(&mut h, &[1]);
    let ip: IpAddr = addr(1).ip();
    assert!(fx.contains(&Effect::Alert(Alert::PeerBanned { ip })));
    assert!(disconnects(&fx, 1));
    assert_eq!(h.t.stats().banned, 1);
    // Never dialed again, from any port, and refused when it connects.
    let fx = h.ev(Event::Announced {
        peers: vec![addr(1), SocketAddr::new(ip, 9999), addr(5)],
        interval: None,
    });
    assert!(fx.contains(&Effect::Connect { addr: addr(5) }));
    assert!(
        !fx.iter()
            .any(|e| matches!(e, Effect::Connect { addr } if addr.ip() == ip))
    );
    let fx = h.ev(Event::PeerAccepted {
        peer: PeerKey(7),
        addr: SocketAddr::new(ip, 40_000),
        reserved: Reserved::new(),
    });
    assert!(disconnects(&fx, 7));
}

#[test]
fn by_default_one_bad_piece_is_forgiven_and_a_second_bans() {
    let mut c = config();
    c.max_requests_per_peer = 2;
    c.initial_requests_per_peer = 2;
    let mut h = Harness::with_have(c, &[]);
    h.connect(1, Reserved::new());
    let fx = fail_piece_zero(&mut h, &[1]);
    assert!(!disconnects(&fx, 1));
    assert_eq!(h.t.stats().banned, 0);
    // The piece is downloaded again from the same peer and fails again.
    for r in [req(0, 0, BLOCK_SIZE), req(0, BLOCK_SIZE, BLOCK_SIZE)] {
        h.msg(
            1,
            Message::Piece {
                piece: r.piece,
                offset: r.offset,
                data: Bytes::from(vec![0; r.length as usize]),
            },
        );
    }
    let fx = h.ev(Event::PieceHashed {
        piece: 0,
        ok: false,
    });
    assert!(disconnects(&fx, 1));
    assert_eq!(h.t.stats().banned, 1);
}

#[test]
fn shared_bad_pieces_need_two_strikes() {
    let mut c = config();
    c.hash_fail_ban = 2;
    c.max_requests_per_peer = 1;
    c.initial_requests_per_peer = 1;
    c.endgame = false;
    let mut h = Harness::with_have(c, &[]);
    h.connect(1, Reserved::new());
    h.connect(2, Reserved::new());
    let fx = fail_piece_zero(&mut h, &[1, 2]);
    assert!(
        !fx.iter()
            .any(|e| matches!(e, Effect::Alert(Alert::PeerBanned { .. })))
    );
    assert_eq!(h.t.stats().banned, 0);
    assert_eq!(h.t.stats().peers, 2);
}

#[test]
fn hash_fail_ban_zero_never_bans() {
    let mut c = config();
    c.hash_fail_ban = 0;
    c.max_requests_per_peer = 2;
    c.initial_requests_per_peer = 2;
    let mut h = Harness::with_have(c, &[]);
    h.connect(1, Reserved::new());
    let fx = fail_piece_zero(&mut h, &[1]);
    assert!(!disconnects(&fx, 1));
    assert_eq!(h.t.stats().banned, 0);
}

fn our_handshake(fx: &[Effect], peer: u64) -> ExtendedHandshake {
    sent(fx, peer)
        .into_iter()
        .find_map(|m| match m {
            Message::Extended { id: 0, payload } => {
                Some(ExtendedHandshake::decode(&payload).unwrap())
            }
            _ => None,
        })
        .expect("extension handshake")
}

#[test]
fn extension_handshake_advertises_port_queue_and_your_ip() {
    let mut c = config();
    c.listen_port = Some(51_413);
    let mut h = Harness::with_have(c, &[]);
    let fx = h.connect(1, Reserved::new().with_extension_protocol(true));
    let hs = our_handshake(&fx, 1);
    assert_eq!(hs.listen_port, Some(51_413));
    assert_eq!(hs.request_queue, Some(250));
    assert_eq!(hs.your_ip, Some(vec![10, 0, 1, 1]));
}

#[test]
fn incoming_peers_are_shared_by_their_listen_port() {
    let mut h = Harness::with_have(config(), &[]);
    let ext = Reserved::new().with_extension_protocol(true);
    // Peer 1 (dialed) speaks PEX; peers 2 and 3 connected to us.
    h.connect(1, ext);
    h.accept(2, ext);
    h.accept(3, ext);
    h.msg(
        1,
        Message::Extended {
            id: 0,
            payload: Bytes::from_static(b"d1:md6:ut_pexi5eee"),
        },
    );
    // Peer 2 names its listen port; peer 3 does not.
    h.msg(
        2,
        Message::Extended {
            id: 0,
            payload: Bytes::from_static(b"d1:mde1:pi7000ee"),
        },
    );
    let fx = h.advance(h.t.config().pex_interval);
    let pex = sent(&fx, 1)
        .into_iter()
        .find_map(|m| match m {
            Message::Extended { id: 5, payload } => Some(PexMessage::decode(&payload).unwrap()),
            _ => None,
        })
        .expect("pex message");
    let added: Vec<SocketAddr> = pex.added.iter().map(|p| p.addr).collect();
    assert_eq!(added, [SocketAddr::new(addr(2).ip(), 7000)]);
}

#[test]
fn incoming_peers_are_refused_while_idle_and_when_full() {
    let mut c = config();
    c.max_peers = 1;
    let mut t = TorrentState::new(layout(), c);
    let now = Instant::now();
    let fx = t.update(
        now,
        Event::PeerAccepted {
            peer: PeerKey(1),
            addr: addr(1),
            reserved: Reserved::new(),
        },
    );
    assert!(disconnects(&fx, 1), "not started");
    t.update(now, Event::Start);
    let fx = t.update(
        now,
        Event::PeerAccepted {
            peer: PeerKey(2),
            addr: addr(2),
            reserved: Reserved::new(),
        },
    );
    assert!(!disconnects(&fx, 2));
    let fx = t.update(
        now,
        Event::PeerAccepted {
            peer: PeerKey(3),
            addr: addr(3),
            reserved: Reserved::new(),
        },
    );
    assert!(disconnects(&fx, 3), "max_peers");
}

#[test]
fn a_failed_upload_read_stops_the_torrent() {
    let mut h = Harness::seed(config());
    h.accept(1, Reserved::new());
    h.msg(1, Message::Interested);
    let fx = h.msg(1, Message::Request(req(0, 0, BLOCK_SIZE)));
    let (_, b) = reads(&fx)[0];
    let fx = h.ev(Event::ReadFailed {
        peer: PeerKey(1),
        block: b,
        error: "gone".into(),
    });
    assert!(
        fx.iter()
            .any(|e| matches!(e, Effect::Alert(Alert::Error { .. })))
    );
    assert_eq!(h.t.status(), TorrentStatus::Stopped);
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Announce {
            event: AnnounceEvent::Stopped,
            ..
        }
    )));
}
