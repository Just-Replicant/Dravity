//! Two LSD services find each other (unicast mode, so the test does not
//! depend on multicast routing).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::time::Duration;

use dravity_core::InfoHash;
use dravity_lsd::{Discovered, Lsd, LsdConfig};

fn free_udp() -> SocketAddr {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap()
}

#[tokio::test]
async fn announces_reach_the_other_side_but_not_ourselves() {
    let (addr_a, addr_b) = (free_udp(), free_udp());
    // A also sends to itself: its own cookie must filter that out.
    let (a, mut found_a) =
        Lsd::start(&LsdConfig::unicast(addr_a, vec![addr_b, addr_a]), 7000).unwrap();
    let (b, mut found_b) = Lsd::start(&LsdConfig::unicast(addr_b, vec![addr_a]), 8000).unwrap();
    assert_eq!(a.local_addrs(), [addr_a]);
    let hashes: Vec<InfoHash> = (0..25u8).map(|i| InfoHash::new([i; 20])).collect();
    a.announce(hashes.clone());
    let mut got = Vec::new();
    while got.len() < hashes.len() {
        let d: Discovered = tokio::time::timeout(Duration::from_secs(5), found_b.recv())
            .await
            .expect("announce not heard")
            .unwrap();
        assert_eq!(d.peer, SocketAddr::from(([127, 0, 0, 1], 7000)));
        got.push(d.info_hash);
    }
    assert_eq!(got, hashes, "split over two messages, in order");
    b.announce(vec![InfoHash::new([0xee; 20])]);
    let d = tokio::time::timeout(Duration::from_secs(5), found_a.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.peer.port(), 8000);
    assert_eq!(d.info_hash, InfoHash::new([0xee; 20]));
    // Nothing else arrived at A (its own announces were dropped).
    assert!(found_a.try_recv().is_err());
}

#[tokio::test]
async fn multicast_mode_starts_or_fails_cleanly() {
    // Multicast may be unavailable in a sandbox; either way it must not
    // panic, and a started service must accept announces.
    if let Ok((lsd, _rx)) = Lsd::start(&LsdConfig::default(), 6881) {
        assert!(!lsd.local_addrs().is_empty());
        lsd.announce(vec![InfoHash::new([1; 20])]);
    }
}
