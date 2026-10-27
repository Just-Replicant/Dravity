//! End-to-end downloads over loopback: a scripted seeder, an in-process HTTP
//! tracker, and a real `Session` writing to a temporary directory.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::panic
)]

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use dravity_bencode::{Dict, Value};
use dravity_core::{InfoHash, Metainfo, PeerId};
use dravity_engine::{Alert, Bitfield, TorrentStatus};
use dravity_session::{AddTorrentOptions, PeerCodec, Session, SessionAlert, SessionConfig};
use dravity_wire::{Handshake, Message};
use futures_util::{SinkExt, StreamExt};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_util::codec::Framed;

const PIECE_LEN: usize = 32 * 1024;

fn bytes(b: &[u8]) -> Value<'static> {
    Value::Bytes(Cow::Owned(b.to_vec()))
}

fn dict(entries: Vec<(&str, Value<'static>)>) -> Value<'static> {
    let mut d = Dict::new();
    for (k, v) in entries {
        d.insert(Cow::Owned(k.as_bytes().to_vec()), v);
    }
    Value::Dict(d)
}

/// A two-file torrent; returns (metainfo, content).
fn make_torrent(announce: Option<&str>, len_a: usize, len_b: usize) -> (Metainfo, Vec<u8>) {
    let content: Vec<u8> = (0..len_a + len_b)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[1])
        .collect();
    let pieces: Vec<u8> = content
        .chunks(PIECE_LEN)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let file = |name: &str, len: usize| {
        dict(vec![
            ("length", Value::Int(len as i64)),
            ("path", Value::List(vec![bytes(name.as_bytes())])),
        ])
    };
    let info = dict(vec![
        ("name", bytes(b"payload")),
        (
            "files",
            Value::List(vec![file("a.bin", len_a), file("b.bin", len_b)]),
        ),
        ("piece length", Value::Int(PIECE_LEN as i64)),
        ("pieces", bytes(&pieces)),
    ]);
    let mut top = vec![("info", info)];
    if let Some(url) = announce {
        top.push(("announce", bytes(url.as_bytes())));
    }
    let encoded = dict(top).to_vec();
    (Metainfo::from_bytes(&encoded).unwrap(), content)
}

/// A seeder that serves `content` to every connection. If `corrupt_once` is
/// set, the first block of piece 0 it serves is garbage.
async fn seeder(content: Arc<Vec<u8>>, info_hash: InfoHash, corrupt_once: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let corrupted = Arc::new(AtomicBool::new(!corrupt_once));
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(serve(
                sock,
                Arc::clone(&content),
                info_hash,
                Arc::clone(&corrupted),
            ));
        }
    });
    addr
}

async fn serve(
    mut sock: TcpStream,
    content: Arc<Vec<u8>>,
    info_hash: InfoHash,
    corrupted: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 68];
    if sock.read_exact(&mut buf).await.is_err() {
        return;
    }
    let theirs = Handshake::decode(&mut BytesMut::from(&buf[..]))
        .unwrap()
        .unwrap();
    assert_eq!(theirs.info_hash, info_hash);
    let ours = Handshake::new(info_hash, PeerId::new(*b"-TS0001-seedseedseed"));
    sock.write_all(&ours.to_bytes()).await.unwrap();
    let pieces = content.len().div_ceil(PIECE_LEN) as u32;
    let mut framed = Framed::new(sock, PeerCodec::default());
    if framed
        .send(Message::Bitfield(Bitfield::full(pieces).to_bytes()))
        .await
        .is_err()
    {
        return;
    }
    while let Some(Ok(message)) = framed.next().await {
        let reply = match message {
            Message::Interested => Message::Unchoke,
            Message::Request(r) => {
                let start = r.piece as usize * PIECE_LEN + r.offset as usize;
                let mut data = content[start..start + r.length as usize].to_vec();
                if r.piece == 0 && r.offset == 0 && !corrupted.swap(true, Ordering::SeqCst) {
                    data.iter_mut().for_each(|b| *b = !*b);
                }
                Message::Piece {
                    piece: r.piece,
                    offset: r.offset,
                    data: Bytes::from(data),
                }
            }
            _ => continue,
        };
        if framed.send(reply).await.is_err() {
            return;
        }
    }
}

/// An HTTP tracker that always returns `peers` (compact form).
async fn tracker(peers: Vec<SocketAddr>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/announce", listener.local_addr().unwrap());
    let mut compact = Vec::new();
    for p in &peers {
        let SocketAddr::V4(v4) = p else {
            panic!("IPv4 only")
        };
        compact.extend_from_slice(&v4.ip().octets());
        compact.extend_from_slice(&v4.port().to_be_bytes());
    }
    let body = dict(vec![
        ("interval", Value::Int(1800)),
        ("peers", bytes(&compact)),
    ])
    .to_vec();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });
    url
}

fn config() -> SessionConfig {
    let mut c = SessionConfig::default();
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_secs(1);
    c
}

/// Collects alerts for `info_hash` until `Finished` (or panics on timeout).
async fn until_finished(
    rx: &mut broadcast::Receiver<SessionAlert>,
    info_hash: InfoHash,
) -> Vec<Alert> {
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Ok(a) if a.info_hash == info_hash => {
                    let done = a.alert == Alert::Finished;
                    seen.push(a.alert);
                    if done {
                        return;
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("alert channel closed: {e}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; alerts so far: {seen:?}"));
    seen
}

fn assert_files(dir: &std::path::Path, content: &[u8], len_a: usize) {
    assert_eq!(
        std::fs::read(dir.join("payload/a.bin")).unwrap(),
        &content[..len_a]
    );
    assert_eq!(
        std::fs::read(dir.join("payload/b.bin")).unwrap(),
        &content[len_a..]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downloads_from_tracker_peer() {
    let (len_a, len_b) = (100_000, 150_001);
    let (probe, content) = make_torrent(None, len_a, len_b);
    let content = Arc::new(content);
    let seed = seeder(Arc::clone(&content), probe.info_hash(), false).await;
    let url = tracker(vec![seed]).await;
    let (metainfo, _) = make_torrent(Some(&url), len_a, len_b);
    // The announce URL is outside `info`, so the info-hash is unchanged.
    assert_eq!(metainfo.info_hash(), probe.info_hash());

    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let mut alerts = session.subscribe();
    let handle = session
        .add_torrent(metainfo, AddTorrentOptions::new(dir.path()))
        .unwrap();
    let seen = until_finished(&mut alerts, handle.info_hash()).await;
    assert!(seen.contains(&Alert::StateChanged(TorrentStatus::Checking)));
    assert!(seen.contains(&Alert::StateChanged(TorrentStatus::Downloading)));
    let stats = handle.stats().await.unwrap();
    assert_eq!(stats.status, TorrentStatus::Finished);
    assert_eq!(stats.bytes_have, (len_a + len_b) as u64);
    handle.stop().await;
    assert!(!handle.is_running());
    assert!(handle.stats().await.is_err());
    assert_files(dir.path(), &content, len_a);

    // Adding it again resumes from disk: everything verifies, no download.
    let (metainfo, _) = make_torrent(Some(&url), len_a, len_b);
    let handle = session
        .add_torrent(metainfo, AddTorrentOptions::new(dir.path()))
        .unwrap();
    let seen = until_finished(&mut alerts, handle.info_hash()).await;
    assert!(!seen.contains(&Alert::StateChanged(TorrentStatus::Downloading)));
    assert_eq!(handle.stats().await.unwrap().downloaded, 0);
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovers_from_corrupt_piece_with_explicit_peer() {
    let (len_a, len_b) = (40_000, 30_000);
    let (metainfo, content) = make_torrent(None, len_a, len_b);
    let content = Arc::new(content);
    let seed = seeder(Arc::clone(&content), metainfo.info_hash(), true).await;
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let mut alerts = session.subscribe();
    let handle = session
        .add_torrent(
            metainfo,
            AddTorrentOptions::new(dir.path()).with_peers([seed]),
        )
        .unwrap();
    assert!(matches!(
        session.add_torrent(
            make_torrent(None, len_a, len_b).0,
            AddTorrentOptions::new(dir.path())
        ),
        Err(dravity_session::SessionError::Duplicate(_))
    ));
    let seen = until_finished(&mut alerts, handle.info_hash()).await;
    assert!(seen.contains(&Alert::HashFailed { piece: 0 }), "{seen:?}");
    // Without trackers the announce fails but the download still works.
    assert!(seen.iter().any(|a| matches!(a, Alert::TrackerError { .. })));
    handle.stop().await;
    assert_files(dir.path(), &content, len_a);
}
