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
    let message = Message::new(b"aa".to_vec(), Body::Query { id: [9; 20], query });
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

/// `Dht` with `native` set runs `NativeDht` behind the same API the
/// session uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dht_facade_runs_the_native_node() {
    let nodes = swarm(6).await;
    let seed = nodes[0].local_addr().unwrap().to_string();
    let config = || {
        let mut c = dravity_dht::DhtConfig::with_bootstrap(vec![seed.clone()]);
        c.native = true;
        c.bind_address = Some(std::net::Ipv4Addr::LOCALHOST);
        c
    };
    let a = dravity_dht::Dht::new(&config()).unwrap();
    let b = dravity_dht::Dht::new(&config()).unwrap();
    assert!(a.bootstrapped().await);
    assert!(b.bootstrapped().await);
    assert!(a.has_nodes().await);
    assert!(!a.to_bootstrap().await.is_empty());
    let hash = dravity_core::InfoHash::new([0x42; 20]);
    a.announce(hash, Some(6001)).await.unwrap();
    assert_eq!(
        b.get_peers(hash).await,
        vec![SocketAddr::from(([127, 0, 0, 1], 6001))]
    );
    assert_eq!(a.local_addr().await.ip(), std::net::Ipv4Addr::LOCALHOST);
}

async fn node_at(ip: [u8; 4]) -> NativeDht {
    NativeDht::bind(SocketAddr::from((ip, 0)), TIMEOUT)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scrape_sample_and_items_work_across_the_swarm() {
    let nodes = swarm(16).await;
    let seed = nodes[0].local_addr().unwrap();
    let hash = [0x33; 20];
    // BEP 33 counts addresses: announce from distinct loopback IPs.
    for (i, is_seed) in [(2u8, true), (3, true), (4, true), (5, false), (6, false)] {
        let n = node_at([127, 0, 0, i]).await;
        n.bootstrap(&[seed]).await;
        let (_, accepted) = n.announce_as(hash, Some(6881), is_seed).await;
        assert!(accepted >= 1);
    }
    let scrape = nodes[9].scrape(hash).await;
    assert_eq!(scrape.seeds.estimate(), 3);
    assert_eq!(scrape.peers.estimate(), 2);

    // BEP 51: the announced hash shows up in samples near it.
    let samples = nodes[4].sample_infohashes(hash).await;
    assert!(samples.contains(&hash), "{samples:?}");

    // BEP 44 immutable: stored by one node, fetched (and hash-checked) by another.
    let (target, stored) = nodes[2].put_immutable(b"12:Hello World!").await;
    assert!(stored >= 1);
    assert_eq!(
        nodes[11].get_immutable(target).await.as_deref(),
        Some(&b"12:Hello World!"[..])
    );
    assert_eq!(nodes[11].get_immutable([0xee; 20]).await, None);

    // BEP 44 mutable: a newer sequence number replaces the value.
    let key = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
    let public = key.verifying_key().to_bytes();
    assert!(nodes[3].put_mutable(&key, b"salt", 1, b"3:one").await.1 >= 1);
    assert!(nodes[5].put_mutable(&key, b"salt", 2, b"3:two").await.1 >= 1);
    assert_eq!(
        nodes[13].get_mutable(&public, b"salt").await,
        Some((2, b"3:two".to_vec()))
    );
    // An older one is refused, so the value stays.
    nodes[6].put_mutable(&key, b"salt", 1, b"3:old").await;
    assert_eq!(
        nodes[14].get_mutable(&public, b"salt").await,
        Some((2, b"3:two".to_vec()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_nodes_query_but_are_not_remembered_or_answering() {
    let nodes = swarm(4).await;
    let seed = nodes[0].local_addr().unwrap();
    let mut config = dravity_dht::native::NativeConfig::default();
    config.timeout = TIMEOUT;
    config.read_only = true;
    let ro = NativeDht::bind_with("127.0.0.1:0".parse().unwrap(), config)
        .await
        .unwrap();
    assert!(ro.bootstrap(&[seed]).await >= 1, "it learns the network");
    let ro_addr = ro.local_addr().unwrap();
    for n in &nodes {
        assert!(
            !n.nodes().contains(&ro_addr),
            "BEP 43: not in routing tables"
        );
    }
    let us = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ping = Message::new(
        b"p1".to_vec(),
        Body::Query {
            id: [1; 20],
            query: Query::Ping,
        },
    );
    us.send_to(&ping.encode(), ro_addr).await.unwrap();
    let mut buf = [0u8; 512];
    assert!(
        tokio::time::timeout(Duration::from_millis(300), us.recv_from(&mut buf))
            .await
            .is_err(),
        "a read-only node answers nobody"
    );
}

#[tokio::test]
async fn a_bep42_id_is_taken_once_nodes_report_our_address() {
    // A fake node that tells every querier it is at a public address.
    let fake = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let fake_addr = fake.local_addr().unwrap();
    let public: SocketAddr = "124.31.75.21:6881".parse().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let (n, from) = fake.recv_from(&mut buf).await.unwrap();
            let q = Message::decode(&buf[..n]).unwrap();
            let mut reply = Message::new(
                q.transaction,
                Body::Response {
                    id: [0x10; 20],
                    nodes: Vec::new(),
                    values: Vec::new(),
                    token: None,
                },
            );
            reply.ext.ip = Some(public);
            fake.send_to(&reply.encode(), from).await.unwrap();
        }
    });
    let node = NativeDht::bind("127.0.0.1:0".parse().unwrap(), TIMEOUT)
        .await
        .unwrap();
    node.bootstrap(&[fake_addr]).await;
    assert_eq!(node.external_ip(), Some(public.ip()));
    assert!(dravity_dht::secure_id::is_secure(&node.id(), public.ip()));
    // The node answers with the querier's address (BEP 42 `ip`).
    let us = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ping = Message::new(
        b"p2".to_vec(),
        Body::Query {
            id: [1; 20],
            query: Query::Ping,
        },
    );
    us.send_to(&ping.encode(), node.local_addr().unwrap())
        .await
        .unwrap();
    let mut buf = [0u8; 512];
    let (n, _) = us.recv_from(&mut buf).await.unwrap();
    assert_eq!(
        Message::decode(&buf[..n]).unwrap().ext.ip,
        Some(us.local_addr().unwrap())
    );
}

/// A token issued to one loopback IP cannot be spent from another.
#[tokio::test]
async fn a_token_is_bound_to_the_querier_ip() {
    let node = NativeDht::bind("127.0.0.1:0".parse().unwrap(), TIMEOUT)
        .await
        .unwrap();
    let to = node.local_addr().unwrap();
    let holder = UdpSocket::bind("127.0.0.2:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let Body::Response {
        token: Some(token), ..
    } = exchange(
        &holder,
        to,
        Query::GetPeers {
            info_hash: [0x42; 20],
        },
    )
    .await
    else {
        panic!("get_peers gives a token");
    };
    let thief = UdpSocket::bind("127.0.0.3:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let steal = Query::AnnouncePeer {
        info_hash: [0x42; 20],
        port: 6881,
        token: token.clone(),
        implied_port: false,
    };
    let Body::Error { code, .. } = exchange(&thief, to, steal).await else {
        panic!("a borrowed token must be refused");
    };
    assert_eq!(code, 203);
    // The original holder can still use it.
    let ok = Query::AnnouncePeer {
        info_hash: [0x42; 20],
        port: 6881,
        token,
        implied_port: false,
    };
    assert!(matches!(
        exchange(&holder, to, ok).await,
        Body::Response { .. }
    ));
}

/// Unparsable and hostile datagrams must not take the node down, and a
/// later well-formed ping must still be answered. This is the offline
/// stand-in for public-network junk traffic.
#[tokio::test]
async fn garbage_and_truncated_queries_leave_the_node_answering() {
    let node = NativeDht::bind("127.0.0.1:0".parse().unwrap(), TIMEOUT)
        .await
        .unwrap();
    let to = node.local_addr().unwrap();
    let us = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    for payload in [
        Vec::new(),
        b"d".to_vec(),
        b"not bencode".to_vec(),
        b"d1:x1:ye".to_vec(),
        vec![0xff; 2048],
        b"d1:y1:q1:q4:pinge".to_vec(), // missing id and transaction
    ] {
        us.send_to(&payload, to).await.unwrap();
    }
    assert!(matches!(
        exchange(&us, to, Query::Ping).await,
        Body::Response { .. }
    ));
}

/// A bootstrap list with unresponsive addresses still joins through the
/// live ones, as a public bootstrap will when some routers are down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_skips_unresponsive_routers() {
    let seed = NativeDht::bind("127.0.0.1:0".parse().unwrap(), TIMEOUT)
        .await
        .unwrap();
    let seed_addr = seed.local_addr().unwrap();
    // Bound but never read: datagrams vanish, the address is unresponsive.
    let dead = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let dead_addr = dead.local_addr().unwrap();
    let node = NativeDht::bind("127.0.0.1:0".parse().unwrap(), TIMEOUT)
        .await
        .unwrap();
    let known = node.bootstrap(&[dead_addr, seed_addr]).await;
    assert!(
        known >= 1,
        "the live router must still be learned; known={known}"
    );
    assert!(node.nodes().contains(&seed_addr));
}
