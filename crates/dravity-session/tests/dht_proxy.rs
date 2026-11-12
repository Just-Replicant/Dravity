//! The session turns a SOCKS5 `SessionConfig::proxy` into
//! `DhtConfig::socks5` when the native DHT is on, so DHT traffic goes
//! through the relay and bootstrap names are never resolved locally.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dravity_core::InfoHash;
use dravity_dht::DhtConfig;
use dravity_dht::native::NativeDht;
use dravity_session::proxy::Proxy;
use dravity_session::{ListenConfig, Session, SessionConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;

const TIMEOUT: Duration = Duration::from_millis(500);
const AUTH: (&str, &str) = ("me", "pass");

type Names = Arc<Mutex<Vec<String>>>;

struct FakeProxy {
    addr: SocketAddr,
    names: Names,
}

async fn socks5_udp_proxy(resolve: HashMap<String, SocketAddr>) -> FakeProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let names: Names = Arc::default();
    let names_t = Arc::clone(&names);
    tokio::spawn(async move {
        loop {
            let Ok((control, _)) = listener.accept().await else {
                break;
            };
            let names = Arc::clone(&names_t);
            let resolve = resolve.clone();
            tokio::spawn(async move {
                handle_associate(control, resolve, names).await;
            });
        }
    });
    FakeProxy { addr, names }
}

async fn handle_associate(
    mut control: tokio::net::TcpStream,
    resolve: HashMap<String, SocketAddr>,
    names: Names,
) {
    let mut greet = [0u8; 4];
    if control.read_exact(&mut greet).await.is_err() || greet != [5, 2, 0, 2] {
        return;
    }
    if control.write_all(&[5, 2]).await.is_err() {
        return;
    }
    let mut creds = [0u8; 9];
    if control.read_exact(&mut creds).await.is_err() || &creds != b"\x01\x02me\x04pass" {
        let _ = control.write_all(&[1, 1]).await;
        return;
    }
    if control.write_all(&[1, 0]).await.is_err() {
        return;
    }
    let mut req = [0u8; 10];
    if control.read_exact(&mut req).await.is_err() || req[..4] != [5, 3, 0, 1] {
        return;
    }
    let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let outside = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = relay.local_addr().unwrap().port().to_be_bytes();
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

fn session_config(proxy: SocketAddr, bootstrap: String) -> SessionConfig {
    let mut c = SessionConfig::default();
    // Loopback HTTP/UDP trackers in these hermetic tests are private origins.
    c.tracker.allow_private_trackers = true;
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_secs(1);
    c.listen = Some(ListenConfig::loopback(0));
    c.port_mapping = None;
    c.lsd = None;
    let mut dht = DhtConfig::with_bootstrap(vec![bootstrap]);
    dht.native = true;
    dht.bind_address = Some(Ipv4Addr::LOCALHOST);
    c.dht = Some(dht);
    c.proxy = Some(Proxy::parse(&format!("socks5://{}:{}@{proxy}", AUTH.0, AUTH.1)).unwrap());
    c
}

/// The session's native DHT goes through its SOCKS5 proxy: a bootstrap
/// name that does not resolve locally is sent as a domain address, and
/// an announce is stored on a loopback seed only the relay can reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_carries_the_native_dht_through_its_socks5_proxy() {
    let seed = NativeDht::bind("127.0.0.1:0".parse().unwrap(), TIMEOUT)
        .await
        .unwrap();
    let seed_addr = seed.local_addr().unwrap();
    let mut resolve = HashMap::new();
    resolve.insert("dht.invalid".into(), seed_addr);
    let proxy = socks5_udp_proxy(resolve).await;

    let bootstrap = format!("dht.invalid:{}", seed_addr.port());
    let a = Session::new(session_config(proxy.addr, bootstrap.clone())).unwrap();
    let b = Session::new(session_config(proxy.addr, bootstrap)).unwrap();
    let dht_a = a.dht().expect("native DHT is on");
    let dht_b = b.dht().expect("native DHT is on");
    assert!(
        dht_a.bootstrapped().await && dht_b.bootstrapped().await,
        "both session DHTs joined the seed through the proxy"
    );

    let hash = InfoHash::new([0x55; 20]);
    dht_a.announce(hash, Some(9001)).await.unwrap();
    assert_eq!(
        dht_b.get_peers(hash).await,
        vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 9001))]
    );
    let names = proxy.names.lock().unwrap().clone();
    assert!(
        names.iter().any(|n| n == "dht.invalid"),
        "the proxy must have been asked to resolve the bootstrap name, got {names:?}"
    );
}
