//! Lookups and announces on a local test network.

#![allow(clippy::unwrap_used)]

use std::net::SocketAddr;

use dravity_core::InfoHash;
use dravity_dht::{Dht, DhtConfig, Testnet};

#[tokio::test(flavor = "multi_thread")]
async fn announce_then_lookup() {
    let net = Testnet::new(10).unwrap();
    let seeder = Dht::new(&net.config()).unwrap();
    let leecher = Dht::new(&net.config()).unwrap();
    assert!(seeder.bootstrapped().await);
    assert!(leecher.bootstrapped().await);
    assert!(
        seeder.has_nodes().await,
        "bootstrap left neighbors in the table"
    );
    let hash = InfoHash::new([0x42; 20]);
    assert!(
        leecher.get_peers(hash).await.is_empty(),
        "nobody announced yet"
    );

    seeder.announce(hash, Some(6881)).await.unwrap();
    let peers = leecher.get_peers(hash).await;
    let expected: SocketAddr = "127.0.0.1:6881".parse().unwrap();
    assert_eq!(peers, vec![expected], "distinct peers only");
    assert!(seeder.local_addr().await.ip().is_loopback());
    assert!(!seeder.to_bootstrap().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn isolated_node() {
    let mut config = DhtConfig::with_bootstrap(Vec::new());
    config.bind_address = Some(std::net::Ipv4Addr::LOCALHOST);
    let dht = Dht::new(&config).unwrap();
    assert!(!dht.has_nodes().await);
    assert!(!dht.bootstrapped().await);
    assert!(dht.get_peers(InfoHash::new([1; 20])).await.is_empty());
    assert!(dht.announce(InfoHash::new([1; 20]), None).await.is_err());
}
