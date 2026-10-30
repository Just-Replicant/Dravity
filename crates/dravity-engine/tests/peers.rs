//! Peer snapshots (`TorrentState::peers`, `MetadataState::peers`).

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use dravity_core::{BLOCK_SIZE, BlockRef, InfoHash, PieceLayout};
use dravity_engine::{Bitfield, Config, Effect, Event, MetadataState, PeerKey, TorrentState};
use dravity_wire::{Message, Reserved};

const PIECES: u32 = 4;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 2], 7000 + n))
}

fn config() -> Config {
    let mut c = Config::default();
    c.check_existing = false;
    c
}

fn layout() -> PieceLayout {
    PieceLayout::new(2 * BLOCK_SIZE, u64::from(PIECES * 2 * BLOCK_SIZE)).unwrap()
}

fn first_request(effects: &[Effect]) -> Option<BlockRef> {
    effects.iter().find_map(|e| match e {
        Effect::Send {
            message: Message::Request(r),
            ..
        } => Some(BlockRef {
            piece: r.piece,
            offset: r.offset,
            length: r.length,
        }),
        _ => None,
    })
}

#[test]
fn a_torrent_lists_its_peers_with_flags_and_byte_totals() {
    let now = Instant::now();
    let mut t = TorrentState::new(layout(), config());
    assert!(t.peers().is_empty());
    t.update(now, Event::Start);
    // Connected out of address order: the snapshot is sorted.
    t.update(
        now,
        Event::PeerAccepted {
            peer: PeerKey(2),
            addr: addr(2),
            reserved: Reserved::default(),
        },
    );
    t.update(
        now,
        Event::PeerConnected {
            peer: PeerKey(1),
            addr: addr(1),
            reserved: Reserved::default(),
        },
    );
    t.update(
        now,
        Event::Message {
            peer: PeerKey(1),
            message: Message::Bitfield(Bitfield::full(PIECES).to_bytes()),
        },
    );
    let fx = t.update(
        now,
        Event::Message {
            peer: PeerKey(1),
            message: Message::Unchoke,
        },
    );
    let block = first_request(&fx).unwrap();
    t.update(
        now,
        Event::Message {
            peer: PeerKey(1),
            message: Message::Piece {
                piece: block.piece,
                offset: block.offset,
                data: Bytes::from(vec![0u8; block.length as usize]),
            },
        },
    );

    let peers = t.peers();
    assert_eq!(peers.len(), 2);
    let seed = peers.first().unwrap();
    assert_eq!(seed.key, PeerKey(1));
    assert_eq!(seed.addr, addr(1));
    assert!(!seed.incoming);
    assert_eq!((seed.pieces_have, seed.piece_count), (PIECES, PIECES));
    assert_eq!(seed.downloaded, u64::from(block.length));
    assert_eq!(seed.uploaded, 0);
    assert!(seed.am_interested);
    assert!(!seed.peer_choking);
    assert!(seed.am_choking);

    let other = peers.get(1).unwrap();
    assert!(other.incoming);
    assert_eq!(other.pieces_have, 0);
    assert_eq!(other.downloaded, 0);
    assert!(other.peer_choking);

    t.update(now, Event::PeerDisconnected { peer: PeerKey(2) });
    assert_eq!(t.peers().len(), 1);
}

#[test]
fn a_magnet_lists_its_peers_before_the_size_is_known() {
    let now = Instant::now();
    let info_hash: InfoHash = "cd".repeat(20).parse().unwrap();
    let mut m = MetadataState::new(info_hash, config());
    m.update(now, Event::Start);
    m.update(
        now,
        Event::PeerConnected {
            peer: PeerKey(7),
            addr: addr(7),
            reserved: Reserved::default(),
        },
    );
    let peers = m.peers();
    assert_eq!(peers.len(), 1);
    let peer = peers.first().unwrap();
    assert_eq!(peer.key, PeerKey(7));
    assert_eq!((peer.pieces_have, peer.piece_count), (0, 0));
    m.update(now, Event::PeerDisconnected { peer: PeerKey(7) });
    assert!(m.peers().is_empty());
}
