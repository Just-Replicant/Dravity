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
use dravity_core::{InfoHash, Magnet, Metainfo, PeerId};
use dravity_dht::{Dht, Testnet};
use dravity_engine::{Alert, Bitfield, TorrentStatus};
use dravity_session::{
    AddTorrentOptions, ListenConfig, PeerCodec, Session, SessionAlert, SessionConfig,
};
use dravity_wire::{
    ExtendedHandshake, Handshake, METADATA_PIECE_LEN, Message, MetadataMessage, PexMessage,
    PexPeer, Reserved,
};
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

/// What a scripted remote peer does.
#[derive(Clone, Default)]
struct PeerScript {
    /// Content to serve; `None` advertises no pieces.
    content: Option<Arc<Vec<u8>>>,
    /// Bencoded `info` dictionary to serve over `ut_metadata` (BEP 9).
    info: Option<Bytes>,
    /// Addresses to announce over `ut_pex` (BEP 11).
    pex: Vec<SocketAddr>,
    /// Garble the first block of piece 0 once.
    corrupt_once: bool,
}

/// A seeder that serves `content` to every connection. If `corrupt_once` is
/// set, the first block of piece 0 it serves is garbage.
async fn seeder(content: Arc<Vec<u8>>, info_hash: InfoHash, corrupt_once: bool) -> SocketAddr {
    spawn_peer(
        info_hash,
        PeerScript {
            content: Some(content),
            corrupt_once,
            ..PeerScript::default()
        },
    )
    .await
}

/// Listens on loopback and runs `script` for every connection.
async fn spawn_peer(info_hash: InfoHash, script: PeerScript) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let corrupted = Arc::new(AtomicBool::new(!script.corrupt_once));
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(serve(
                sock,
                script.clone(),
                info_hash,
                Arc::clone(&corrupted),
            ));
        }
    });
    addr
}

/// Our extension ids, as announced to the client.
const SEED_UT_METADATA: u8 = 3;
const SEED_UT_PEX: u8 = 4;

async fn serve(
    mut sock: TcpStream,
    script: PeerScript,
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
    let extended = theirs.reserved.extension_protocol();
    let ours = Handshake::new(info_hash, PeerId::new(*b"-TS0001-seedseedseed"))
        .with_reserved(Reserved::new().with_extension_protocol(true));
    sock.write_all(&ours.to_bytes()).await.unwrap();
    let mut framed = Framed::new(sock, PeerCodec::default());
    if extended {
        let mut extensions = vec![("ut_metadata".to_owned(), SEED_UT_METADATA)];
        if !script.pex.is_empty() {
            extensions.push(("ut_pex".to_owned(), SEED_UT_PEX));
        }
        let hs = ExtendedHandshake {
            extensions,
            listen_port: None,
            client: Some(b"test seeder".to_vec()),
            your_ip: None,
            request_queue: Some(250),
            metadata_size: script.info.as_ref().map(|i| i.len() as u64),
            ipv4: None,
            ipv6: None,
        };
        let payload = Bytes::from(hs.encode().unwrap());
        if framed
            .send(Message::Extended { id: 0, payload })
            .await
            .is_err()
        {
            return;
        }
    }
    let availability = match &script.content {
        Some(content) => {
            let pieces = content.len().div_ceil(PIECE_LEN) as u32;
            Message::Bitfield(Bitfield::full(pieces).to_bytes())
        }
        None => Message::KeepAlive,
    };
    if framed.send(availability).await.is_err() {
        return;
    }
    // Extension ids the client wants us to use.
    let mut their_metadata = None;
    let mut their_pex = None;
    while let Some(Ok(message)) = framed.next().await {
        let reply = match message {
            Message::Interested => Message::Unchoke,
            Message::Request(r) => {
                let Some(content) = &script.content else {
                    continue;
                };
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
            Message::Extended { id: 0, payload } => {
                let hs = ExtendedHandshake::decode(&payload).unwrap();
                for (name, id) in hs.extensions {
                    match name.as_str() {
                        "ut_metadata" => their_metadata = Some(id),
                        "ut_pex" => their_pex = Some(id),
                        _ => {}
                    }
                }
                match their_pex.filter(|_| !script.pex.is_empty()) {
                    Some(id) => {
                        let pex = PexMessage {
                            added: script
                                .pex
                                .iter()
                                .map(|&addr| PexPeer { addr, flags: 0 })
                                .collect(),
                            dropped: Vec::new(),
                        };
                        Message::Extended {
                            id,
                            payload: Bytes::from(pex.encode()),
                        }
                    }
                    None => continue,
                }
            }
            Message::Extended {
                id: SEED_UT_METADATA,
                payload,
            } => {
                let (Some(id), Ok(MetadataMessage::Request { piece })) =
                    (their_metadata, MetadataMessage::decode(&payload))
                else {
                    continue;
                };
                let answer = match &script.info {
                    Some(info) => {
                        let start = (piece * METADATA_PIECE_LEN) as usize;
                        let end = (start + METADATA_PIECE_LEN as usize).min(info.len());
                        MetadataMessage::Data {
                            piece,
                            total_size: info.len() as u64,
                            data: info.slice(start..end),
                        }
                    }
                    None => MetadataMessage::Reject { piece },
                };
                Message::Extended {
                    id,
                    payload: answer.encode().unwrap(),
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
    // Hermetic: no public DHT bootstrap, loopback only, any free port.
    c.dht = None;
    c.listen = Some(ListenConfig::loopback(0));
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
    // A torrent with no trackers still downloads from the explicit peer,
    // and does not report a tracker error.
    assert!(
        !seen.iter().any(|a| matches!(a, Alert::TrackerError { .. })),
        "{seen:?}"
    );
    handle.stop().await;
    assert_files(dir.path(), &content, len_a);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_metadata_stops_instead_of_hanging() {
    let garbage = Bytes::from(vec![b'x'; 64]);
    let hash = InfoHash::from_info_bytes(&garbage);
    let seed = spawn_peer(
        hash,
        PeerScript {
            info: Some(garbage),
            ..PeerScript::default()
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let mut alerts = session.subscribe();
    let handle = session
        .add_magnet(
            &Magnet::new(hash),
            AddTorrentOptions::new(dir.path()).with_peers([seed]),
        )
        .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let alert = alerts.recv().await.expect("alerts closed");
            if alert.info_hash == hash {
                if let Alert::Error { message } = alert.alert {
                    return message;
                }
            }
        }
    })
    .await
    .expect("invalid metadata left the torrent running");
    assert!(message.contains("invalid metadata"), "{message}");
    tokio::time::timeout(Duration::from_secs(5), handle.stop())
        .await
        .expect("stop hung after metadata failure");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn magnet_fetches_metadata_then_downloads() {
    let (len_a, len_b) = (70_000, 20_000);
    let (metainfo, content) = make_torrent(None, len_a, len_b);
    let hash = metainfo.info_hash();
    let seed = spawn_peer(
        hash,
        PeerScript {
            content: Some(Arc::new(content.clone())),
            info: Some(Bytes::copy_from_slice(metainfo.info_bytes())),
            ..PeerScript::default()
        },
    )
    .await;
    let url = tracker(vec![seed]).await;
    let mut magnet = Magnet::new(hash);
    magnet.trackers.push(url);

    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let mut alerts = session.subscribe();
    let handle = session
        .add_magnet(&magnet, AddTorrentOptions::new(dir.path()))
        .unwrap();
    let seen = until_finished(&mut alerts, hash).await;
    assert!(
        seen.contains(&Alert::StateChanged(TorrentStatus::FetchingMetadata)),
        "{seen:?}"
    );
    let received = seen
        .iter()
        .position(|a| *a == Alert::MetadataReceived)
        .expect("metadata alert");
    let downloading = seen
        .iter()
        .position(|a| *a == Alert::StateChanged(TorrentStatus::Downloading))
        .expect("downloading");
    assert!(received < downloading);
    assert_eq!(
        handle.stats().await.unwrap().bytes_have,
        (len_a + len_b) as u64
    );
    handle.stop().await;
    assert_files(dir.path(), &content, len_a);

    // Unsupported inputs are rejected up front.
    let v2_only = Magnet::parse(&format!("magnet:?xt=urn:btmh:1220{}", "ab".repeat(32))).unwrap();
    assert!(matches!(
        session.add_magnet(&v2_only, AddTorrentOptions::new(dir.path())),
        Err(dravity_session::SessionError::MagnetWithoutV1Hash)
    ));
    let storage = Arc::new(dravity_storage::MemoryStorage::new(1));
    assert!(matches!(
        session.add_magnet(
            &magnet,
            AddTorrentOptions::new(dir.path()).with_storage(storage)
        ),
        Err(dravity_session::SessionError::StorageForMagnet)
    ));
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn magnet_finds_peers_in_the_dht() {
    let (len_a, len_b) = (33_000, 1);
    let (metainfo, content) = make_torrent(None, len_a, len_b);
    let hash = metainfo.info_hash();
    let seed = spawn_peer(
        hash,
        PeerScript {
            content: Some(Arc::new(content.clone())),
            info: Some(Bytes::copy_from_slice(metainfo.info_bytes())),
            ..PeerScript::default()
        },
    )
    .await;
    let net = Testnet::new(8).unwrap();
    let announcer = Dht::new(&net.config()).unwrap();
    announcer.announce(hash, Some(seed.port())).await.unwrap();

    let mut c = config();
    c.dht = Some(net.config());
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(c).unwrap();
    assert!(session.dht().is_some());
    let mut alerts = session.subscribe();
    let handle = session
        .add_magnet(&Magnet::new(hash), AddTorrentOptions::new(dir.path()))
        .unwrap();
    until_finished(&mut alerts, hash).await;
    handle.stop().await;
    assert_files(dir.path(), &content, len_a);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_learned_through_pex() {
    let (len_a, len_b) = (50_000, 10_000);
    let (metainfo, content) = make_torrent(None, len_a, len_b);
    let hash = metainfo.info_hash();
    let seed = seeder(Arc::new(content.clone()), hash, false).await;
    // The only peer we are told about has nothing but knows the seeder.
    let gossip = spawn_peer(
        hash,
        PeerScript {
            pex: vec![seed],
            ..PeerScript::default()
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let mut alerts = session.subscribe();
    let handle = session
        .add_torrent(
            metainfo,
            AddTorrentOptions::new(dir.path()).with_peers([gossip]),
        )
        .unwrap();
    until_finished(&mut alerts, hash).await;
    handle.stop().await;
    assert_files(dir.path(), &content, len_a);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fastresume_skips_the_check_until_files_change() {
    let (len_a, len_b) = (64_000, 32_000);
    let (metainfo, content) = make_torrent(None, len_a, len_b);
    let hash = metainfo.info_hash();
    let seed = seeder(Arc::new(content.clone()), hash, false).await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = config();
    c.resume_dir = Some(dir.path().join(".resume"));
    let session = Session::new(c).unwrap();
    let mut alerts = session.subscribe();
    let add = |session: &Session| {
        session
            .add_torrent(
                make_torrent(None, len_a, len_b).0,
                AddTorrentOptions::new(dir.path()).with_peers([seed]),
            )
            .unwrap()
    };

    let handle = add(&session);
    let seen = until_finished(&mut alerts, hash).await;
    assert!(seen.contains(&Alert::StateChanged(TorrentStatus::Checking)));
    handle.stop().await;
    let resume = dir.path().join(format!(".resume/{}.resume", hash.to_hex()));
    assert!(resume.exists());

    // Nothing changed: trusted without hashing.
    let handle = add(&session);
    let seen = until_finished(&mut alerts, hash).await;
    assert!(
        !seen.contains(&Alert::StateChanged(TorrentStatus::Checking)),
        "{seen:?}"
    );
    assert_eq!(handle.stats().await.unwrap().downloaded, 0);
    handle.stop().await;

    // A touched file invalidates the resume data: full check again.
    std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("payload/b.bin"))
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    let handle = add(&session);
    let seen = until_finished(&mut alerts, hash).await;
    assert!(seen.contains(&Alert::StateChanged(TorrentStatus::Checking)));
    assert_eq!(handle.stats().await.unwrap().downloaded, 0);
    handle.stop().await;
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tier_falls_back_to_the_next_tracker() {
    let (len_a, len_b) = (20_000, 20_000);
    let (probe, content) = make_torrent(None, len_a, len_b);
    let seed = seeder(Arc::new(content.clone()), probe.info_hash(), false).await;
    let good = tracker(vec![seed]).await;
    // A port nobody listens on.
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}/announce", l.local_addr().unwrap())
    };
    let mut magnet = Magnet::new(probe.info_hash());
    magnet.trackers = vec![dead, good];
    // Same tier: build a torrent with announce-list [[dead, good]].
    let info = Value::decode(probe.info_bytes()).unwrap().into_owned();
    let tier = Value::List(
        magnet
            .trackers
            .iter()
            .map(|u| bytes(u.as_bytes()))
            .collect(),
    );
    let encoded = dict(vec![
        ("info", info),
        ("announce-list", Value::List(vec![tier])),
    ])
    .to_vec();
    let metainfo = Metainfo::from_bytes(&encoded).unwrap();
    assert_eq!(metainfo.trackers().len(), 1);
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let mut alerts = session.subscribe();
    let handle = session
        .add_torrent(metainfo, AddTorrentOptions::new(dir.path()))
        .unwrap();
    let seen = until_finished(&mut alerts, handle.info_hash()).await;
    assert!(
        !seen.iter().any(|a| matches!(a, Alert::TrackerError { .. })),
        "one tracker of the tier answered: {seen:?}"
    );
    handle.stop().await;
    assert_files(dir.path(), &content, len_a);
}

/// Writes `content` where a torrent from `make_torrent` expects it.
fn write_files(dir: &std::path::Path, content: &[u8], len_a: usize) {
    std::fs::create_dir_all(dir.join("payload")).unwrap();
    std::fs::write(dir.join("payload/a.bin"), &content[..len_a]).unwrap();
    std::fs::write(dir.join("payload/b.bin"), &content[len_a..]).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_seeds_to_another_session() {
    let (len_a, len_b) = (90_000, 70_000);
    let (metainfo, content) = make_torrent(None, len_a, len_b);
    let hash = metainfo.info_hash();
    let seed_dir = tempfile::tempdir().unwrap();
    write_files(seed_dir.path(), &content, len_a);
    let seeder = Session::new(config()).unwrap();
    let port = seeder.listen_port().expect("seeder listens");
    let mut seeder_alerts = seeder.subscribe();
    let seeding = seeder
        .add_torrent(metainfo, AddTorrentOptions::new(seed_dir.path()))
        .unwrap();
    // Complete on disk: verified, then seeding.
    until_finished(&mut seeder_alerts, hash).await;

    let leech_dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config()).unwrap();
    let mut alerts = leecher.subscribe();
    let leeching = leecher
        .add_torrent(
            make_torrent(None, len_a, len_b).0,
            AddTorrentOptions::new(leech_dir.path())
                .with_peers([SocketAddr::from(([127, 0, 0, 1], port))]),
        )
        .unwrap();
    let seen = until_finished(&mut alerts, hash).await;
    assert!(
        seen.contains(&Alert::StateChanged(TorrentStatus::Downloading)),
        "{seen:?}"
    );
    assert_files(leech_dir.path(), &content, len_a);
    let stats = seeding.stats().await.unwrap();
    assert_eq!(stats.status, TorrentStatus::Finished, "still seeding");
    assert!(stats.uploaded >= (len_a + len_b) as u64, "{stats:?}");
    assert_eq!(
        leeching.stats().await.unwrap().downloaded,
        (len_a + len_b) as u64
    );
    leecher.shutdown().await;
    seeder.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_connections_for_unknown_torrents_are_closed() {
    let (metainfo, _) = make_torrent(None, 1000, 1000);
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let port = session.listen_port().unwrap();
    let handle = session
        .add_torrent(metainfo, AddTorrentOptions::new(dir.path()))
        .unwrap();
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let hello = Handshake::new(InfoHash::new([0xee; 20]), PeerId::new([3; 20]));
    sock.write_all(&hello.to_bytes()).await.unwrap();
    let mut buf = [0u8; 68];
    let read = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf))
        .await
        .expect("closed promptly");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");

    // The right info-hash gets our handshake back.
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let hello = Handshake::new(handle.info_hash(), PeerId::new([3; 20]));
    sock.write_all(&hello.to_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let mut reply = BytesMut::from(&buf[..]);
    let theirs = Handshake::decode(&mut reply).unwrap().unwrap();
    assert_eq!(theirs.info_hash, handle.info_hash());
    assert_eq!(theirs.peer_id, session.peer_id());
    assert!(theirs.reserved.fast());
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_listener_nothing_is_accepted() {
    let mut c = config();
    c.listen = None;
    let session = Session::new(c).unwrap();
    assert_eq!(session.listen_port(), None);
}
