//! Native DHT traffic through a SOCKS5 UDP relay: no datagrams leave
//! until the association is up, host names are resolved by the proxy,
//! and two nodes can find each other through the relay.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dravity_core::InfoHash;
use dravity_dht::native::{NativeConfig, NativeDht};
use dravity_dht::{Contact, Dht, DhtConfig, Socks5Proxy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;

const TIMEOUT: Duration = Duration::from_millis(500);
const AUTH: (&str, &str) = ("me", "pass");

type Names = Arc<Mutex<Vec<String>>>;
type Sources = Arc<Mutex<Vec<SocketAddr>>>;

/// A SOCKS5 UDP proxy that supports many associations at once (one
/// "outside" socket each), optional username/password, a delayed
/// ASSOCIATE reply, and domain-address resolution via `names`.
struct FakeProxy {
    addr: SocketAddr,
    names: Names,
    /// Each association's network-facing address (the source the
    /// bootstrap victim sees).
    outside_addrs: Arc<Mutex<Vec<SocketAddr>>>,
}

struct FakeOpts {
    auth: bool,
    /// When set, ASSOCIATE replies wait until this becomes `true`.
    gate: Option<watch::Receiver<bool>>,
    /// Host names (SOCKS5 type 3) map to this address; the name is logged.
    resolve: HashMap<String, SocketAddr>,
}

impl Default for FakeOpts {
    fn default() -> Self {
        Self {
            auth: true,
            gate: None,
            resolve: HashMap::new(),
        }
    }
}

async fn socks5_udp_proxy(opts: FakeOpts) -> FakeProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let names: Names = Arc::default();
    let outside_addrs = Arc::new(Mutex::new(Vec::new()));
    let names_t = Arc::clone(&names);
    let outside_t = Arc::clone(&outside_addrs);
    tokio::spawn(async move {
        loop {
            let Ok((control, _)) = listener.accept().await else {
                break;
            };
            let names = Arc::clone(&names_t);
            let outside_addrs = Arc::clone(&outside_t);
            let gate = opts.gate.clone();
            let resolve = opts.resolve.clone();
            let auth = opts.auth;
            tokio::spawn(async move {
                handle_associate(control, auth, gate, resolve, names, outside_addrs).await;
            });
        }
    });
    FakeProxy {
        addr,
        names,
        outside_addrs,
    }
}

async fn handle_associate(
    mut control: tokio::net::TcpStream,
    auth: bool,
    gate: Option<watch::Receiver<bool>>,
    resolve: HashMap<String, SocketAddr>,
    names: Names,
    outside_addrs: Arc<Mutex<Vec<SocketAddr>>>,
) {
    if auth {
        let mut greet = [0u8; 4];
        if control.read_exact(&mut greet).await.is_err() {
            return;
        }
        if greet != [5, 2, 0, 2] {
            return;
        }
        if control.write_all(&[5, 2]).await.is_err() {
            return;
        }
        let mut creds = [0u8; 9];
        if control.read_exact(&mut creds).await.is_err() {
            return;
        }
        if &creds != b"\x01\x02me\x04pass" {
            let _ = control.write_all(&[1, 1]).await;
            return;
        }
        if control.write_all(&[1, 0]).await.is_err() {
            return;
        }
    } else {
        let mut greet = [0u8; 3];
        if control.read_exact(&mut greet).await.is_err() {
            return;
        }
        if greet != [5, 1, 0] {
            return;
        }
        if control.write_all(&[5, 0]).await.is_err() {
            return;
        }
    }
    let mut req = [0u8; 10];
    if control.read_exact(&mut req).await.is_err() {
        return;
    }
    if req[..4] != [5, 3, 0, 1] {
        return;
    }
    if let Some(mut gate) = gate {
        while !*gate.borrow() {
            if gate.changed().await.is_err() {
                return;
            }
        }
    }
    let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let outside = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    outside_addrs
        .lock()
        .unwrap()
        .push(outside.local_addr().unwrap());
    let port = relay.local_addr().unwrap().port().to_be_bytes();
    // 0.0.0.0: the client must use the proxy's TCP-peer address.
    if control
        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, port[0], port[1]])
        .await
        .is_err()
    {
        return;
    }
    let relay = Arc::new(relay);
    let outside = Arc::new(outside);
    let (client_tx, mut client_rx) = watch::channel(None);
    let relay_c = Arc::clone(&relay);
    let outside_c = Arc::clone(&outside);
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, client)) = relay_c.recv_from(&mut buf).await else {
                break;
            };
            let _ = client_tx.send(Some(client));
            let Some((target, payload)) = decode(&buf[..n], &resolve, &names) else {
                continue;
            };
            if outside_c.send_to(payload, target).await.is_err() {
                break;
            }
        }
    });
    let relay_s = Arc::clone(&relay);
    let outside_s = Arc::clone(&outside);
    tokio::spawn(async move {
        // Wait until we know the client's UDP address.
        let client = loop {
            if let Some(addr) = *client_rx.borrow() {
                break addr;
            }
            if client_rx.changed().await.is_err() {
                return;
            }
        };
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, from)) = outside_s.recv_from(&mut buf).await else {
                break;
            };
            let mut wrapped = dravity_core::socks5::udp_header(from);
            wrapped.extend_from_slice(&buf[..n]);
            if relay_s.send_to(&wrapped, client).await.is_err() {
                break;
            }
        }
    });
    // Keep the control connection open for the association's lifetime.
    let mut sink = [0u8; 8];
    let _ = control.read(&mut sink).await;
    std::future::pending::<()>().await;
}

fn decode<'a>(
    buf: &'a [u8],
    resolve: &HashMap<String, SocketAddr>,
    names: &Names,
) -> Option<(SocketAddr, &'a [u8])> {
    let atyp = *buf.get(3)?;
    if atyp == 3 {
        let len = usize::from(*buf.get(4)?);
        let host = buf.get(5..5 + len)?;
        let name = String::from_utf8(host.to_vec()).ok()?;
        names.lock().unwrap().push(name.clone());
        let dest = *resolve.get(&name)?;
        let payload = buf.get(5 + len + 2..)?;
        Some((dest, payload))
    } else {
        dravity_core::socks5::parse_udp_header(buf)
    }
}

fn proxy_cfg(addr: SocketAddr) -> Socks5Proxy {
    Socks5Proxy::new(addr.to_string(), Some((AUTH.0.into(), AUTH.1.into())))
}

fn native_cfg() -> NativeConfig {
    NativeConfig::with_timeout(TIMEOUT)
}

async fn direct_seed() -> NativeDht {
    NativeDht::bind_with("127.0.0.1:0".parse().unwrap(), native_cfg())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_sent_until_the_relay_is_up() {
    let victim = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let victim_addr = victim.local_addr().unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let sources: Sources = Arc::default();
    let count = Arc::clone(&received);
    let log = Arc::clone(&sources);
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((_, from)) = victim.recv_from(&mut buf).await else {
                break;
            };
            count.fetch_add(1, Ordering::SeqCst);
            log.lock().unwrap().push(from);
        }
    });

    let (gate_tx, gate_rx) = watch::channel(false);
    let mut opts = FakeOpts {
        gate: Some(gate_rx),
        ..FakeOpts::default()
    };
    opts.resolve.insert("dht.invalid".into(), victim_addr);
    let proxy = socks5_udp_proxy(opts).await;

    // A bind that is waiting for the ASSOCIATE reply must not produce a
    // UDP socket (and so cannot leak to the victim).
    let proxy_for_bind = proxy_cfg(proxy.addr);
    let pending = NativeDht::bind_relayed(&proxy_for_bind, native_cfg());
    let hold = tokio::time::timeout(Duration::from_millis(400), pending);
    // Meanwhile the facade starts, queues a lookup, and still must not leak.
    let mut c = DhtConfig::with_bootstrap(vec!["dht.invalid:6881".into()]);
    c.native = true;
    c.socks5 = Some(proxy_cfg(proxy.addr));
    let dht = Dht::new(&c).unwrap();
    let hash = InfoHash::new([0x11; 20]);
    let _ = tokio::time::timeout(Duration::from_millis(300), dht.get_peers(hash)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        received.load(Ordering::SeqCst),
        0,
        "no datagram reached the victim before the relay was up"
    );
    // The direct bind is still waiting.
    assert!(hold.await.is_err(), "bind_relayed waited for the relay");

    gate_tx.send(true).unwrap();
    // After the gate opens, traffic may flow; the victim should see
    // packets from the relay's outside socket only.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if received.load(Ordering::SeqCst) > 0 {
                break;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), dht.bootstrapped()).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("datagrams reach the victim once the relay is up");

    tokio::time::sleep(Duration::from_millis(50)).await;
    let seen = sources.lock().unwrap().clone();
    assert!(!seen.is_empty());
    let outsides = proxy.outside_addrs.lock().unwrap().clone();
    assert!(
        seen.iter().all(|s| outsides.contains(s)),
        "victim saw {seen:?}, relay outsides were {outsides:?}"
    );
    // Names were used, never a locally resolved address of dht.invalid
    // (which has none).
    assert!(
        proxy
            .names
            .lock()
            .unwrap()
            .iter()
            .any(|n| n == "dht.invalid"),
        "the proxy must have been asked to resolve the bootstrap name"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_associate_sends_nothing_and_lookups_are_empty() {
    let victim = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let victim_addr = victim.local_addr().unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&received);
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            if victim.recv(&mut buf).await.is_ok() {
                count.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    // Port 1 is not a SOCKS5 proxy; the association fails at once.
    let mut c = DhtConfig::with_bootstrap(vec![victim_addr.to_string()]);
    c.native = true;
    c.socks5 = Some(Socks5Proxy::new("127.0.0.1:1".into(), None));
    let dht = Dht::new(&c).unwrap();
    let hash = InfoHash::new([0x22; 20]);
    let peers = dht.get_peers(hash).await;
    assert!(peers.is_empty());
    assert!(dht.announce(hash, Some(1)).await.is_err());
    assert!(!dht.has_nodes().await);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(received.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_are_found_through_the_relay_and_names_are_not_resolved_locally() {
    let seed = direct_seed().await;
    let seed_addr = seed.local_addr().unwrap();
    let mut opts = FakeOpts::default();
    opts.resolve.insert("dht.invalid".into(), seed_addr);
    let proxy = socks5_udp_proxy(opts).await;
    let cfg = proxy_cfg(proxy.addr);

    let a = NativeDht::bind_relayed(&cfg, native_cfg()).await.unwrap();
    let b = NativeDht::bind_relayed(&cfg, native_cfg()).await.unwrap();
    let bootstrap = Contact::Name("dht.invalid".into(), seed_addr.port());
    assert!(a.bootstrap_contacts(std::slice::from_ref(&bootstrap)).await >= 1);
    assert!(b.bootstrap_contacts(&[bootstrap]).await >= 1);

    let info_hash = [0x5a; 20];
    let (_, accepted) = a.announce(info_hash, Some(6001)).await;
    assert!(accepted >= 1, "accepted by {accepted} nodes");
    let found = b.get_peers(info_hash).await;
    assert_eq!(found.peers, vec![SocketAddr::from(([127, 0, 0, 1], 6001))]);
    let names = proxy.names.lock().unwrap().clone();
    assert!(!names.is_empty(), "the bootstrap name must go to the proxy");
    assert!(
        names.iter().all(|n| n == "dht.invalid"),
        "only the bootstrap name went out, got {names:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dht_facade_runs_the_native_node_through_socks5() {
    let seed = direct_seed().await;
    let seed_addr = seed.local_addr().unwrap();
    let mut opts = FakeOpts::default();
    opts.resolve.insert("router.invalid".into(), seed_addr);
    let proxy = socks5_udp_proxy(opts).await;
    let config = || {
        let mut c = DhtConfig::with_bootstrap(vec![format!("router.invalid:{}", seed_addr.port())]);
        c.native = true;
        c.socks5 = Some(proxy_cfg(proxy.addr));
        c
    };
    let a = Dht::new(&config()).unwrap();
    let b = Dht::new(&config()).unwrap();
    assert!(a.bootstrapped().await);
    assert!(b.bootstrapped().await);
    assert!(a.has_nodes().await);
    let hash = InfoHash::new([0x42; 20]);
    a.announce(hash, Some(7001)).await.unwrap();
    assert_eq!(
        b.get_peers(hash).await,
        vec![SocketAddr::from(([127, 0, 0, 1], 7001))]
    );
    assert!(
        proxy
            .names
            .lock()
            .unwrap()
            .iter()
            .any(|n| n == "router.invalid")
    );
}
