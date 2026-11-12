//! BEP 32: the native DHT on IPv6, and a dual-stack facade that joins
//! both an IPv4 and an IPv6 swarm.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use dravity_core::InfoHash;
use dravity_dht::native::NativeDht;
use dravity_dht::{Dht, DhtConfig};

const TIMEOUT: Duration = Duration::from_millis(500);

async fn swarm6(count: usize) -> Vec<NativeDht> {
    let mut nodes = Vec::new();
    for _ in 0..count {
        nodes.push(
            NativeDht::bind("[::1]:0".parse().unwrap(), TIMEOUT)
                .await
                .unwrap(),
        );
    }
    let seed = nodes[0].local_addr().unwrap();
    for node in &nodes[1..] {
        node.bootstrap(&[seed]).await;
    }
    for node in &nodes[1..] {
        node.bootstrap(&[seed]).await;
    }
    nodes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ipv6_peers_announced_on_one_node_are_found_from_another() {
    let nodes = swarm6(12).await;
    assert!(
        nodes.iter().all(|n| n.table_len() >= 3),
        "every IPv6 node knows some others"
    );
    assert!(
        nodes.iter().all(|n| n.local_addr().unwrap().is_ipv6()),
        "every node is bound on IPv6"
    );
    let info_hash = [0x6a; 20];
    let (_, accepted) = nodes[3].announce(info_hash, Some(7777)).await;
    assert!(accepted >= 3, "accepted by {accepted} nodes");
    let found = nodes[9].get_peers(info_hash).await;
    let me = SocketAddr::from((Ipv6Addr::LOCALHOST, 7777));
    assert_eq!(found.peers, vec![me]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dual_stack_facade_finds_ipv4_and_ipv6_peers() {
    let seed4 = NativeDht::bind("127.0.0.1:0".parse().unwrap(), TIMEOUT)
        .await
        .unwrap();
    let seed6 = NativeDht::bind("[::1]:0".parse().unwrap(), TIMEOUT)
        .await
        .unwrap();
    let bootstrap = vec![
        seed4.local_addr().unwrap().to_string(),
        seed6.local_addr().unwrap().to_string(),
    ];
    let config = || {
        let mut c = DhtConfig::with_bootstrap(bootstrap.clone());
        c.native = true;
        c.ipv6 = true;
        c.bind_address = Some(Ipv4Addr::LOCALHOST);
        c.bind_address_v6 = Some(Ipv6Addr::LOCALHOST);
        c
    };
    let a = Dht::new(&config()).unwrap();
    let b = Dht::new(&config()).unwrap();
    assert!(a.bootstrapped().await);
    assert!(b.bootstrapped().await);
    assert!(a.has_nodes().await);
    assert_eq!(a.local_addr().await.ip(), Ipv4Addr::LOCALHOST);
    let addr6 = a.local_addr_v6().await.expect("IPv6 node is running");
    assert_eq!(addr6.ip(), Ipv6Addr::LOCALHOST);

    let hash = InfoHash::new([0x32; 20]);
    a.announce(hash, Some(8001)).await.unwrap();
    let peers = b.get_peers(hash).await;
    assert!(
        peers.contains(&SocketAddr::from((Ipv4Addr::LOCALHOST, 8001))),
        "IPv4 peer missing from {peers:?}"
    );
    assert!(
        peers.contains(&SocketAddr::from((Ipv6Addr::LOCALHOST, 8001))),
        "IPv6 peer missing from {peers:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the internet"]
async fn live_ipv6_bootstrap_reaches_the_public_dht() {
    let node = NativeDht::bind("[::]:0".parse().unwrap(), Duration::from_secs(2))
        .await
        .unwrap();
    let mut routers = Vec::new();
    for host in ["router.bittorrent.com:6881", "dht.transmissionbt.com:6881"] {
        if let Ok(addrs) = tokio::net::lookup_host(host).await {
            routers.extend(addrs.filter(SocketAddr::is_ipv6));
        }
    }
    assert!(
        !routers.is_empty(),
        "the public routers have no AAAA records from this network"
    );
    let known = node.bootstrap(&routers).await;
    assert!(known >= 1, "IPv6 routing table has {known} nodes");
}
