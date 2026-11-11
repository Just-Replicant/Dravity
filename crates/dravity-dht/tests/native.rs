//! `NativeDht` in a local swarm of nodes on loopback.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::net::SocketAddr;
use std::time::Duration;

use dravity_dht::krpc::{Body, Message, Query};
use dravity_dht::native::NativeDht;
use tokio::net::UdpSocket;

const TIMEOUT: Duration = Duration::from_millis(500);

async fn swarm(count: usize) -> Vec<NativeDht> {
    let mut nodes = Vec::new();
    for _ in 0..count {
        nodes.push(
            NativeDht::bind("127.0.0.1:0".parse().unwrap(), TIMEOUT)
                .await
                .unwrap(),
        );
    }
    let seed = nodes[0].local_addr().unwrap();
    for node in &nodes[1..] {
        node.bootstrap(&[seed]).await;
    }
    // A second pass lets early joiners learn about later ones.
    for node in &nodes[1..] {
        node.bootstrap(&[seed]).await;
    }
    nodes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_announced_on_one_node_are_found_from_another() {
    let nodes = swarm(24).await;
    assert!(
        nodes.iter().all(|n| n.table_len() >= 3),
        "every node knows some others"
    );
    let info_hash = [0x5a; 20];
    let (_, accepted) = nodes[7].announce(info_hash, Some(7777)).await;
    assert!(accepted >= 3, "accepted by {accepted} nodes");
    let found = nodes[19].get_peers(info_hash).await;
    let me = SocketAddr::from(([127, 0, 0, 1], 7777));
    assert_eq!(found.peers, vec![me]);
    assert!(!found.closest.is_empty());
    // With an implied port the datagram's source port is stored.
    let (_, accepted) = nodes[3].announce([0x77; 20], None).await;
    assert!(accepted >= 1);
    let implied = nodes[12].get_peers([0x77; 20]).await;
    assert_eq!(implied.peers, vec![nodes[3].local_addr().unwrap()]);
    // Unknown info-hash: no peers, but the lookup still converges.
    let none = nodes[5].get_peers([0x01; 20]).await;
    assert!(none.peers.is_empty());
    assert!(!none.closest.is_empty());
}

async fn exchange(socket: &UdpSocket, to: SocketAddr, query: Query) -> Body {
    let message = Message {
        transaction: b"aa".to_vec(),
        body: Body::Query { id: [9; 20], query },
    };
    socket.send_to(&message.encode(), to).await.unwrap();
    let mut buf = [0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let reply = Message::decode(&buf[..n]).unwrap();
    assert_eq!(reply.transaction, b"aa");
    reply.body
}

#[tokio::test]
async fn announces_need_a_token_issued_to_the_same_address() {
    let node = NativeDht::bind("127.0.0.1:0".parse().unwrap(), TIMEOUT)
        .await
        .unwrap();
    let to = node.local_addr().unwrap();
    let us = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let forged = Query::AnnouncePeer {
        info_hash: [1; 20],
        port: 6881,
        token: b"forged".to_vec(),
        implied_port: false,
    };
    let Body::Error { code, .. } = exchange(&us, to, forged).await else {
        panic!("a forged token is refused");
    };
    assert_eq!(code, 203);
    let Body::Response {
        token: Some(token),
        id,
        ..
    } = exchange(&us, to, Query::GetPeers { info_hash: [1; 20] }).await
    else {
        panic!("get_peers gives a token");
    };
    assert_eq!(id, node.id());
    // The querier is now in the node's table.
    assert_eq!(node.table_len(), 1);
    let ok = Query::AnnouncePeer {
        info_hash: [1; 20],
        port: 6881,
        token,
        implied_port: false,
    };
    assert!(matches!(exchange(&us, to, ok).await, Body::Response { .. }));
    let Body::Response { values, .. } =
        exchange(&us, to, Query::GetPeers { info_hash: [1; 20] }).await
    else {
        panic!("response");
    };
    assert_eq!(values, vec![SocketAddr::from(([127, 0, 0, 1], 6881))]);
    assert!(matches!(
        exchange(&us, to, Query::Ping).await,
        Body::Response { .. }
    ));
}

/// Joins the live DHT through the public bootstrap routers (needs the
/// internet: `cargo test -p dravity-dht --test native -- --ignored`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the internet"]
async fn live_bootstrap_reaches_the_public_dht() {
    let node = NativeDht::bind("0.0.0.0:0".parse().unwrap(), Duration::from_secs(2))
        .await
        .unwrap();
    let mut routers = Vec::new();
    for host in ["router.bittorrent.com:6881", "dht.transmissionbt.com:6881"] {
        if let Ok(addrs) = tokio::net::lookup_host(host).await {
            routers.extend(addrs.filter(SocketAddr::is_ipv4));
        }
    }
    let known = node.bootstrap(&routers).await;
    assert!(known >= 8, "routing table has {known} nodes");
    // Ubuntu ISOs are well seeded; any public hash with peers works too.
    let lookup = node.get_peers([0x11; 20]).await;
    assert!(!lookup.closest.is_empty(), "a lookup reached live nodes");
}
