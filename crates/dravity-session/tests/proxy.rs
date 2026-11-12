//! Dialing peers through SOCKS4, SOCKS5 and HTTP CONNECT proxies.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dravity_core::Metainfo;
use dravity_core::create::{CreateOptions, InputFile, TorrentCreator};
use dravity_session::proxy::{Proxy, ProxyKind};
use dravity_session::{AddTorrentOptions, IpFilter, ListenConfig, Session, SessionConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static DATA: [u8; 300_000] = [3; 300_000];

fn torrent() -> Metainfo {
    let mut c = TorrentCreator::new(
        "f.bin",
        vec![InputFile::new(["f.bin"], DATA.len() as u64)],
        true,
        CreateOptions::default(),
    )
    .unwrap();
    c.update(&DATA[..]).unwrap();
    Metainfo::from_bytes(&c.finish().unwrap()).unwrap()
}

fn config(filter: Option<IpFilter>) -> SessionConfig {
    let mut c = SessionConfig::default();
    // Loopback HTTP/UDP trackers in these hermetic tests are private origins.
    c.tracker.allow_private_trackers = true;
    c.dht = None;
    c.port_mapping = None;
    c.listen = Some(ListenConfig::loopback(0));
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_millis(100);
    c.ip_filter = filter.map(Arc::new);
    c
}

async fn seed(filter: Option<IpFilter>) -> (Session, SocketAddr, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.bin"), &DATA[..]).unwrap();
    let s = Session::new(config(filter)).unwrap();
    s.add_torrent(torrent(), AddTorrentOptions::new(dir.path()))
        .unwrap();
    let addr = SocketAddr::from(([127, 0, 0, 1], s.listen_port().unwrap()));
    (s, addr, dir)
}

/// A tiny proxy for tests: speaks one protocol, checks credentials,
/// records each target and relays bytes. Returns its address.
async fn fake_proxy(kind: ProxyKind, targets: Arc<Mutex<Vec<SocketAddr>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut client, _) = listener.accept().await.unwrap();
            let targets = Arc::clone(&targets);
            tokio::spawn(async move {
                let Some(target) = handshake(kind, &mut client).await else {
                    return;
                };
                targets.lock().unwrap().push(target);
                let mut upstream = TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    addr
}

async fn handshake(kind: ProxyKind, c: &mut TcpStream) -> Option<SocketAddr> {
    match kind {
        ProxyKind::Socks4 => {
            let mut head = [0u8; 8];
            c.read_exact(&mut head).await.ok()?;
            while c.read_u8().await.ok()? != 0 {}
            c.write_all(&[0, 90, 0, 0, 0, 0, 0, 0]).await.ok()?;
            let ip = std::net::Ipv4Addr::new(head[4], head[5], head[6], head[7]);
            Some(SocketAddr::from((
                ip,
                u16::from_be_bytes([head[2], head[3]]),
            )))
        }
        ProxyKind::Socks5 => {
            let mut greet = [0u8; 2];
            c.read_exact(&mut greet).await.ok()?;
            let mut methods = vec![0u8; usize::from(greet[1])];
            c.read_exact(&mut methods).await.ok()?;
            assert!(methods.contains(&2), "client offered no password auth");
            c.write_all(&[5, 2]).await.ok()?;
            let field = async |c: &mut TcpStream| {
                let len = c.read_u8().await.ok()?;
                let mut v = vec![0u8; usize::from(len)];
                c.read_exact(&mut v).await.ok()?;
                Some(v)
            };
            assert_eq!(c.read_u8().await.ok()?, 1);
            let user = field(c).await?;
            let pass = field(c).await?;
            if (user.as_slice(), pass.as_slice()) != (b"me".as_slice(), b"secret".as_slice()) {
                c.write_all(&[1, 1]).await.ok()?;
                return None;
            }
            c.write_all(&[1, 0]).await.ok()?;
            let mut req = [0u8; 10];
            c.read_exact(&mut req).await.ok()?;
            assert_eq!(&req[..4], &[5, 1, 0, 1]);
            c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.ok()?;
            let ip = std::net::Ipv4Addr::new(req[4], req[5], req[6], req[7]);
            Some(SocketAddr::from((ip, u16::from_be_bytes([req[8], req[9]]))))
        }
        ProxyKind::Http => {
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(c.read_u8().await.ok()?);
            }
            let text = String::from_utf8(header).ok()?;
            let target = text
                .strip_prefix("CONNECT ")?
                .split(' ')
                .next()?
                .parse()
                .ok()?;
            c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .ok()?;
            Some(target)
        }
    }
}

async fn download_through(proxy: Proxy) -> (bool, Vec<SocketAddr>) {
    let (seed, addr, _d) = seed(None).await;
    let targets = Arc::new(Mutex::new(Vec::new()));
    let server = fake_proxy(proxy.kind, Arc::clone(&targets)).await;
    let mut c = config(None);
    c.proxy = Some(Proxy {
        server: server.to_string(),
        ..proxy
    });
    let dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(c).unwrap();
    let handle = leecher
        .add_torrent(
            torrent(),
            AddTorrentOptions::new(dir.path()).with_peers([addr]),
        )
        .unwrap();
    let start = Instant::now();
    let mut done = false;
    while start.elapsed() < Duration::from_secs(5) {
        if handle.progress().await.unwrap().stats.wanted_left == 0 {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    leecher.shutdown().await;
    seed.shutdown().await;
    let seen = targets.lock().unwrap().clone();
    let only_seed = seen.iter().all(|t| *t == addr);
    assert!(only_seed, "unexpected targets {seen:?}");
    (done, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downloads_through_each_kind_of_proxy() {
    for (kind, auth) in [
        (
            ProxyKind::Socks5,
            Some(("me".to_owned(), "secret".to_owned())),
        ),
        (ProxyKind::Socks4, None),
        (ProxyKind::Http, Some(("u".to_owned(), "p".to_owned()))),
    ] {
        let (done, seen) = download_through(Proxy {
            kind,
            server: String::new(),
            auth,
        })
        .await;
        assert!(done, "{kind:?}: download did not finish");
        assert!(!seen.is_empty(), "{kind:?}: proxy saw no connection");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_proxy_means_no_connection() {
    let (done, seen) = download_through(Proxy {
        kind: ProxyKind::Socks5,
        server: String::new(),
        auth: Some(("me".to_owned(), "wrong".to_owned())),
    })
    .await;
    assert!(!done);
    assert!(seen.is_empty());
}
