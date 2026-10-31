//! BEP 52 end to end: v2-only and hybrid torrents between two sessions over
//! loopback, v2 magnet links, both swarm ids of a hybrid torrent, and v2
//! torrents surviving a restart. Also BEP 19 web seeds (a loopback HTTP
//! server), whose padding-aware ranges are easiest to show on these
//! piece-aligned multi-file torrents.

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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::BytesMut;
use dravity_bencode::{Dict, Value};
use dravity_core::merkle;
use dravity_core::{InfoHash, Magnet, MetaVersion, Metainfo, PeerId};
use dravity_engine::{Alert, TorrentStatus};
use dravity_session::{AddTorrentOptions, ListenConfig, Session, SessionAlert, SessionConfig};
use dravity_wire::Handshake;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

const PIECE: u32 = 32 * 1024;

fn bstr(b: &[u8]) -> Value<'static> {
    Value::Bytes(Cow::Owned(b.to_vec()))
}

fn dict(entries: Vec<(&[u8], Value<'static>)>) -> Value<'static> {
    Value::Dict(
        entries
            .into_iter()
            .map(|(k, v)| (Cow::Owned(k.to_vec()), v))
            .collect(),
    )
}

/// `(relative path, content)` of one file below the torrent's directory.
type File = (Vec<&'static str>, Vec<u8>);

fn content(len: usize, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32 ^ seed).wrapping_mul(2_654_435_761).to_le_bytes()[2])
        .collect()
}

/// A multi-file torrent named `payload` over `files` (sorted by path),
/// v2-only or hybrid. Returns the encoded `.torrent`.
fn make(files: &mut [File], hybrid: bool, web_seed: Option<&str>) -> Vec<u8> {
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut tree = Dict::new();
    let mut layers = Dict::new();
    let mut v1_files = Vec::new();
    let mut stream = Vec::new();
    let last = files.iter().rposition(|f| !f.1.is_empty());
    for (i, (path, data)) in files.iter().enumerate() {
        let mut leaf = vec![(&b"length"[..], Value::Int(data.len() as i64))];
        if let Some(h) = merkle::file_hashes(data, PIECE) {
            leaf.push((&b"pieces root"[..], bstr(&h.root)));
            if !h.layer.is_empty() {
                layers.insert(Cow::Owned(h.root.to_vec()), bstr(&h.layer.concat()));
            }
        }
        let mut node = &mut tree;
        for c in path {
            let Value::Dict(next) = node
                .entry(Cow::Owned(c.as_bytes().to_vec()))
                .or_insert_with(|| Value::Dict(Dict::new()))
            else {
                unreachable!()
            };
            node = next;
        }
        node.insert(Cow::Borrowed(&b""[..]), dict(leaf));

        stream.extend_from_slice(data);
        let segs = |p: &[&str]| Value::List(p.iter().map(|c| bstr(c.as_bytes())).collect());
        v1_files.push(dict(vec![
            (b"length", Value::Int(data.len() as i64)),
            (b"path", segs(path)),
        ]));
        let rem = data.len() % PIECE as usize;
        if rem != 0 && Some(i) != last {
            let pad = PIECE as usize - rem;
            stream.resize(stream.len() + pad, 0);
            v1_files.push(dict(vec![
                (b"attr", bstr(b"p")),
                (b"length", Value::Int(pad as i64)),
                (b"path", segs(&[".pad", &pad.to_string()])),
            ]));
        }
    }
    let mut info = vec![
        (&b"file tree"[..], Value::Dict(tree)),
        (b"meta version", Value::Int(2)),
        (b"name", bstr(b"payload")),
        (b"piece length", Value::Int(i64::from(PIECE))),
    ];
    if hybrid {
        let pieces: Vec<u8> = stream
            .chunks(PIECE as usize)
            .flat_map(|c| Sha1::digest(c).to_vec())
            .collect();
        info.push((b"files", Value::List(v1_files)));
        info.push((b"pieces", bstr(&pieces)));
    }
    let mut top = vec![
        (&b"info"[..], dict(info)),
        (b"piece layers", Value::Dict(layers)),
    ];
    if let Some(url) = web_seed {
        top.push((b"url-list", bstr(url.as_bytes())));
    }
    dict(top).to_vec()
}

fn write_files(dir: &Path, files: &[File]) {
    for (path, data) in files {
        let mut p = dir.join("payload");
        for c in path {
            p.push(c);
        }
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
}

fn assert_files(dir: &Path, files: &[File]) {
    // Storage creates files as data arrives, so an empty file never appears
    // (as for v1 torrents).
    for (path, data) in files.iter().filter(|f| !f.1.is_empty()) {
        let mut p = dir.join("payload");
        for c in path {
            p.push(c);
        }
        assert_eq!(&std::fs::read(&p).unwrap(), data, "{}", p.display());
    }
    assert!(
        !dir.join("payload/.pad").exists(),
        "padding is never written"
    );
}

fn sample() -> Vec<File> {
    vec![
        (vec!["video.bin"], content(150_000, 1)),
        (vec!["notes.txt"], content(4_000, 2)),
        (vec!["extra", "empty"], Vec::new()),
        (vec!["extra", "tail.bin"], content(40_000, 3)),
    ]
}

/// Files no larger than one piece: their pieces roots are all a v2 magnet
/// link needs.
fn small() -> Vec<File> {
    vec![
        (vec!["a.bin"], content(20_000, 4)),
        (vec!["b.bin"], content(PIECE as usize, 5)),
    ]
}

fn config() -> SessionConfig {
    let mut c = SessionConfig::default();
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_secs(1);
    c.dht = None;
    c.listen = Some(ListenConfig::loopback(0));
    c.port_mapping = None;
    c
}

/// Waits for an alert of `info_hash` matching `done`; returns all seen.
async fn until(
    rx: &mut broadcast::Receiver<SessionAlert>,
    info_hash: InfoHash,
    done: impl Fn(&Alert) -> bool,
) -> Vec<Alert> {
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Ok(a) if a.info_hash == info_hash => {
                    let stop = done(&a.alert);
                    seen.push(a.alert);
                    if stop {
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

/// A session seeding `torrent` from `files`; returns it and its port.
async fn seed(torrent: &[u8], files: &[File], dir: &Path) -> (Session, u16) {
    write_files(dir, files);
    let m = Metainfo::from_bytes(torrent).unwrap();
    let hash = m.info_hash();
    let session = Session::new(config()).unwrap();
    let port = session.listen_port().unwrap();
    let mut alerts = session.subscribe();
    session.add_torrent(m, AddTorrentOptions::new(dir)).unwrap();
    until(&mut alerts, hash, |a| *a == Alert::Finished).await;
    (session, port)
}

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

async fn download_between_sessions(hybrid: bool) {
    let mut files = sample();
    let torrent = make(&mut files, hybrid, None);
    let m = Metainfo::from_bytes(&torrent).unwrap();
    let expected = if hybrid {
        MetaVersion::Hybrid
    } else {
        MetaVersion::V2
    };
    assert_eq!(m.info().version(), expected);
    let hash = m.info_hash();
    let seed_dir = tempfile::tempdir().unwrap();
    let (seeder, port) = seed(&torrent, &files, seed_dir.path()).await;

    let leech_dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config()).unwrap();
    let mut alerts = leecher.subscribe();
    let handle = leecher
        .add_torrent(
            m,
            AddTorrentOptions::new(leech_dir.path()).with_peers([local(port)]),
        )
        .unwrap();
    let seen = until(&mut alerts, hash, |a| *a == Alert::Finished).await;
    assert!(
        !seen.iter().any(|a| matches!(a, Alert::HashFailed { .. })),
        "{seen:?}"
    );
    assert_files(leech_dir.path(), &files);
    let stats = handle.stats().await.unwrap();
    assert_eq!(stats.status, TorrentStatus::Finished);
    assert_eq!(stats.pieces_have, stats.piece_count);
    leecher.shutdown().await;
    seeder.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_v2_only_torrent_moves_between_sessions() {
    download_between_sessions(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hybrid_torrent_moves_between_sessions() {
    download_between_sessions(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hybrid_torrent_answers_to_both_swarm_ids() {
    let mut files = sample();
    let torrent = make(&mut files, true, None);
    let m = Metainfo::from_bytes(&torrent).unwrap();
    let ids = m.swarm_hashes();
    assert_eq!(ids.len(), 2);
    let dir = tempfile::tempdir().unwrap();
    let (session, port) = seed(&torrent, &files, dir.path()).await;
    for id in ids {
        let mut sock = TcpStream::connect(local(port)).await.unwrap();
        sock.write_all(&Handshake::new(id, PeerId::new([7; 20])).to_bytes())
            .await
            .unwrap();
        let mut buf = [0u8; 68];
        tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut buf))
            .await
            .expect("handshake reply")
            .unwrap();
        let theirs = Handshake::decode(&mut BytesMut::from(&buf[..]))
            .unwrap()
            .unwrap();
        assert_eq!(theirs.info_hash, id, "replies with the id we asked for");
        assert!(theirs.reserved.v2(), "advertises BEP 52 support");
    }
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_v2_magnet_link_fetches_metadata_and_downloads() {
    let mut files = small();
    let torrent = make(&mut files, false, None);
    let m = Metainfo::from_bytes(&torrent).unwrap();
    let v2 = m.info_hash_v2().unwrap();
    let hash = m.info_hash();
    let seed_dir = tempfile::tempdir().unwrap();
    let (seeder, port) = seed(&torrent, &files, seed_dir.path()).await;

    let leecher = Session::new(config()).unwrap();
    let mut alerts = leecher.subscribe();
    let leech_dir = tempfile::tempdir().unwrap();
    let magnet = Magnet::parse(&format!(
        "magnet:?xt=urn:btmh:1220{v2}&x.pe=127.0.0.1:{port}&ws=http%3A%2F%2Fseed.invalid%2F"
    ))
    .unwrap();
    let handle = leecher
        .add_magnet(&magnet, AddTorrentOptions::new(leech_dir.path()))
        .unwrap();
    assert_eq!(handle.info_hash(), hash, "the truncated v2 hash");
    let seen = until(&mut alerts, hash, |a| *a == Alert::Finished).await;
    assert!(seen.contains(&Alert::MetadataReceived), "{seen:?}");
    assert_files(leech_dir.path(), &files);
    let known = leecher.metainfo(hash).unwrap();
    assert_eq!(known.info_hash_v2(), Some(v2));
    assert_eq!(known.web_seeds(), ["http://seed.invalid/"]);
    leecher.shutdown().await;
    seeder.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_v2_magnet_fetches_piece_layers_and_downloads_multi_piece_files() {
    // `video.bin` spans 5 pieces and `tail.bin` 2: their piece layers are
    // not in the metadata and come from the seed with BEP 52 hash requests.
    let mut files = sample();
    let torrent = make(&mut files, false, None);
    let m = Metainfo::from_bytes(&torrent).unwrap();
    assert_eq!(m.info().piece_layers().len(), 2);
    let v2 = m.info_hash_v2().unwrap();
    let hash = m.info_hash();
    let seed_dir = tempfile::tempdir().unwrap();
    let (seeder, port) = seed(&torrent, &files, seed_dir.path()).await;

    let leecher = Session::new(config()).unwrap();
    let mut alerts = leecher.subscribe();
    let leech_dir = tempfile::tempdir().unwrap();
    let magnet = Magnet::parse(&format!(
        "magnet:?xt=urn:btmh:1220{v2}&x.pe=127.0.0.1:{port}"
    ))
    .unwrap();
    let handle = leecher
        .add_magnet(&magnet, AddTorrentOptions::new(leech_dir.path()))
        .unwrap();
    let seen = until(&mut alerts, hash, |a| *a == Alert::Finished).await;
    assert!(seen.contains(&Alert::MetadataReceived), "{seen:?}");
    assert!(
        !seen
            .iter()
            .any(|a| matches!(a, Alert::HashFailed { .. } | Alert::Error { .. })),
        "{seen:?}"
    );
    assert_files(leech_dir.path(), &files);
    let known = leecher.metainfo(hash).unwrap();
    assert!(known.info().missing_piece_layers().is_empty());
    assert_eq!(known.info().piece_layers(), m.info().piece_layers());
    let stats = handle.stats().await.unwrap();
    assert_eq!(stats.pieces_have, stats.piece_count);
    leecher.shutdown().await;
    seeder.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_torrents_keep_their_piece_layers_across_a_restart() {
    let mut files = sample();
    let torrent = make(&mut files, false, Some("http://seed.invalid/files/"));
    let m = Metainfo::from_bytes(&torrent).unwrap();
    let hash = m.info_hash();
    let state = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_files(data.path(), &files);
    let mut c = config();
    c.state_dir = Some(state.path().to_path_buf());
    {
        let session = Session::new(c.clone()).unwrap();
        let mut alerts = session.subscribe();
        session
            .add_torrent(m.clone(), AddTorrentOptions::new(data.path()))
            .unwrap();
        until(&mut alerts, hash, |a| *a == Alert::Finished).await;
        session.shutdown().await;
    }
    let session = Session::new(c).unwrap();
    let mut alerts = session.subscribe();
    let report = session.restore().await.unwrap();
    assert_eq!(report.restored, [hash], "{report:?}");
    let restored = session.metainfo(hash).unwrap();
    assert_eq!(restored.info(), m.info());
    assert!(restored.info().missing_piece_layers().is_empty());
    assert_eq!(restored.web_seeds(), m.web_seeds());
    until(&mut alerts, hash, |a| *a == Alert::Finished).await;
    session.shutdown().await;
}

/// A tiny HTTP server for `root`: `GET /<path>` with a `Range` header gets
/// `206` and those bytes, without one the whole file; anything missing (or
/// everything, with `broken`) gets `404`. Counts requests.
async fn http_server(root: PathBuf, broken: bool) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let root = root.clone();
            let count = Arc::clone(&count);
            tokio::spawn(async move {
                // Keep-alive: answer requests until the client hangs up.
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                loop {
                    let end = loop {
                        if let Some(i) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    };
                    let text = String::from_utf8_lossy(&req[..end]).to_string();
                    req.drain(..end);
                    count.fetch_add(1, Ordering::Relaxed);
                    let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let file = std::fs::read(root.join(path.trim_start_matches('/')));
                    let (status, body) = match file {
                        Ok(data) if !broken => {
                            let range = text.lines().find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("range: bytes=")
                                    .map(str::to_owned)
                            });
                            match range {
                                Some(r) => {
                                    let (a, b) = r.trim().split_once('-').unwrap();
                                    let (a, b): (usize, usize) =
                                        (a.parse().unwrap(), b.parse().unwrap());
                                    ("206 Partial Content", data[a..=b].to_vec())
                                }
                                None => ("200 OK", data),
                            }
                        }
                        _ => ("404 Not Found", Vec::new()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n",
                        body.len()
                    );
                    if sock.write_all(head.as_bytes()).await.is_err()
                        || sock.write_all(&body).await.is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (addr, hits)
}

async fn download_from_web_seed(hybrid: bool) {
    let mut files = sample();
    let served = tempfile::tempdir().unwrap();
    write_files(served.path(), &files);
    let (addr, hits) = http_server(served.path().to_path_buf(), false).await;
    let torrent = make(&mut files, hybrid, Some(&format!("http://{addr}/")));
    let m = Metainfo::from_bytes(&torrent).unwrap();
    assert_eq!(m.web_seeds(), [format!("http://{addr}/")]);
    let hash = m.info_hash();

    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let mut alerts = session.subscribe();
    let handle = session
        .add_torrent(m, AddTorrentOptions::new(dir.path()))
        .unwrap();
    let seen = until(&mut alerts, hash, |a| *a == Alert::Finished).await;
    assert!(
        !seen.iter().any(|a| matches!(a, Alert::HashFailed { .. })),
        "{seen:?}"
    );
    assert_files(dir.path(), &files);
    assert!(hits.load(Ordering::Relaxed) > 0);
    let stats = handle.stats().await.unwrap();
    assert_eq!(stats.pieces_have, stats.piece_count);
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_v2_only_torrent_downloads_from_a_web_seed_alone() {
    download_from_web_seed(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hybrid_torrent_downloads_from_a_web_seed_alone() {
    download_from_web_seed(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_web_seed_does_not_stop_a_download_from_peers() {
    let mut files = sample();
    let (addr, hits) = http_server(PathBuf::from("/nonexistent"), true).await;
    let torrent = make(&mut files, true, Some(&format!("http://{addr}/")));
    let m = Metainfo::from_bytes(&torrent).unwrap();
    let hash = m.info_hash();
    let seed_dir = tempfile::tempdir().unwrap();
    let (seeder, port) = seed(&torrent, &files, seed_dir.path()).await;

    let dir = tempfile::tempdir().unwrap();
    let mut c = config();
    c.web_seed_retry = Duration::from_millis(20);
    let leecher = Session::new(c).unwrap();
    let mut alerts = leecher.subscribe();
    let handle = leecher
        .add_torrent(m, AddTorrentOptions::new(dir.path()))
        .unwrap();
    // The seed fails, is retried after its back-off, and fails again.
    tokio::time::timeout(Duration::from_secs(10), async {
        while hits.load(Ordering::Relaxed) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the web seed is retried");
    assert_eq!(handle.stats().await.unwrap().pieces_have, 0);

    handle.add_peers([local(port)]).await.unwrap();
    let seen = until(&mut alerts, hash, |a| *a == Alert::Finished).await;
    assert!(
        !seen.iter().any(|a| matches!(a, Alert::HashFailed { .. })),
        "{seen:?}"
    );
    assert_files(dir.path(), &files);
    leecher.shutdown().await;
    seeder.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_seeds_can_be_turned_off() {
    let mut files = small();
    let served = tempfile::tempdir().unwrap();
    write_files(served.path(), &files);
    let (addr, hits) = http_server(served.path().to_path_buf(), false).await;
    let torrent = make(&mut files, false, Some(&format!("http://{addr}/")));
    let m = Metainfo::from_bytes(&torrent).unwrap();
    let mut c = config();
    c.web_seed = None;
    let session = Session::new(c).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let handle = session
        .add_torrent(m, AddTorrentOptions::new(dir.path()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    let stats = handle.stats().await.unwrap();
    assert_eq!(stats.pieces_have, 0);
    session.shutdown().await;
}

/// Percent-decodes a query value.
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// An HTTP tracker that records every announced `info_hash` and returns
/// `peer` only to announces for `only`.
async fn recording_tracker(
    only: InfoHash,
    peer: SocketAddr,
) -> (String, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/announce", listener.local_addr().unwrap());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let SocketAddr::V4(v4) = peer else {
        panic!("IPv4 only")
    };
    let mut compact = v4.ip().octets().to_vec();
    compact.extend_from_slice(&v4.port().to_be_bytes());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            let compact = compact.clone();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).to_string();
                let target = text.split_whitespace().nth(1).unwrap_or("").to_owned();
                let hash = target
                    .split(['?', '&'])
                    .find_map(|kv| kv.strip_prefix("info_hash="))
                    .map(unescape)
                    .unwrap_or_default();
                let peers = if hash == only.as_bytes() {
                    compact
                } else {
                    Vec::new()
                };
                log.lock().unwrap().push(hash);
                let body = dict(vec![
                    (b"interval", Value::Int(1800)),
                    (b"peers", bstr(&peers)),
                ])
                .to_vec();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });
    (url, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hybrid_torrent_announces_both_ids_to_trackers() {
    let mut files = sample();
    let torrent = make(&mut files, true, None);
    let base = Metainfo::from_bytes(&torrent).unwrap();
    let ids = base.swarm_hashes();
    assert_eq!(ids.len(), 2);
    let seed_dir = tempfile::tempdir().unwrap();
    let (seeder, port) = seed(&torrent, &files, seed_dir.path()).await;
    // The tracker hands out the seed only in the v2 swarm.
    let (url, seen) = recording_tracker(ids[1], local(port)).await;
    let m = Metainfo::from_info_bytes(base.info_bytes(), vec![vec![url]]).unwrap();
    let hash = m.info_hash();

    let dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config()).unwrap();
    let mut alerts = leecher.subscribe();
    leecher
        .add_torrent(m, AddTorrentOptions::new(dir.path()))
        .unwrap();
    until(&mut alerts, hash, |a| *a == Alert::Finished).await;
    assert_files(dir.path(), &files);
    let announced = seen.lock().unwrap().clone();
    for id in &ids {
        assert!(
            announced.iter().any(|h| h == id.as_bytes()),
            "{id} announced: {announced:?}"
        );
    }
    leecher.shutdown().await;
    seeder.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hybrid_magnet_answers_to_its_v2_id_once_the_metadata_arrives() {
    let mut files = sample();
    let torrent = make(&mut files, true, None);
    let m = Metainfo::from_bytes(&torrent).unwrap();
    let ids = m.swarm_hashes();
    let seed_dir = tempfile::tempdir().unwrap();
    let (seeder, port) = seed(&torrent, &files, seed_dir.path()).await;

    let leecher = Session::new(config()).unwrap();
    let leech_port = leecher.listen_port().unwrap();
    let mut alerts = leecher.subscribe();
    let dir = tempfile::tempdir().unwrap();
    let magnet = Magnet::parse(&format!(
        "magnet:?xt=urn:btih:{}&x.pe=127.0.0.1:{port}",
        ids[0]
    ))
    .unwrap();
    leecher
        .add_magnet(&magnet, AddTorrentOptions::new(dir.path()))
        .unwrap();
    until(&mut alerts, ids[0], |a| *a == Alert::Finished).await;
    // No restart: the v2 swarm id now reaches the same torrent.
    let mut sock = TcpStream::connect(local(leech_port)).await.unwrap();
    sock.write_all(&Handshake::new(ids[1], PeerId::new([8; 20])).to_bytes())
        .await
        .unwrap();
    let mut buf = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut buf))
        .await
        .expect("handshake reply")
        .unwrap();
    let theirs = Handshake::decode(&mut BytesMut::from(&buf[..]))
        .unwrap()
        .unwrap();
    assert_eq!(theirs.info_hash, ids[1]);
    leecher.shutdown().await;
    seeder.shutdown().await;
}
