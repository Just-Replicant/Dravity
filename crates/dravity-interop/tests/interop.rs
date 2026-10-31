//! Dravity against qBittorrent (libtorrent) and Transmission on loopback.
//!
//! Opt-in: every test is `#[ignore]`d and also needs `DRAVITY_INTEROP=1`
//! and the client binary on `PATH`:
//!
//! ```text
//! DRAVITY_INTEROP=1 cargo test -p dravity-interop -- --ignored --test-threads=1
//! ```
//!
//! Each case starts a fresh client with DHT, PEX and local discovery off, a
//! tracker of the test's own, and moves a three-file torrent (about 3.7
//! MiB) one way: the client seeds and Dravity downloads ("from"), or
//! Dravity seeds and the client downloads ("to"). The received files are
//! compared byte for byte. Dravity's view of the connection (TCP or uTP,
//! encrypted or not) is sampled mid-transfer and printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    missing_docs
)]

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use dravity_core::Metainfo;
use dravity_session::{
    AddTorrentOptions, EncryptionPolicy, ListenConfig, Session, SessionConfig, TorrentHandle,
    Transport,
};
use support::clients::{Client, Crypto, Qbittorrent, Transmission, Transports, installed};
use support::http::Tracker;
use support::torrent::{self, Kind};

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    Qbittorrent,
    Transmission,
}

impl Which {
    const fn binary(self) -> &'static str {
        match self {
            Self::Qbittorrent => "qbittorrent-nox",
            Self::Transmission => "transmission-daemon",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// The client seeds, Dravity downloads.
    FromClient,
    /// Dravity seeds, the client downloads.
    ToClient,
}

/// The MSE policy on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mse {
    /// Dravity `Disabled`, the client plaintext only (or tolerating MSE).
    Disabled,
    /// Dravity `Enabled`, the client preferring MSE.
    Enabled,
    /// Both require it.
    Required,
}

#[derive(Debug, Clone, Copy)]
struct Case {
    client: Which,
    kind: Kind,
    direction: Direction,
    mse: Mse,
    /// The transports the client uses (Dravity has both, and prefers uTP
    /// only when the client uses uTP alone).
    net: Transports,
}

impl std::fmt::Display for Case {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let dir = match self.direction {
            Direction::FromClient => "client -> dravity",
            Direction::ToClient => "dravity -> client",
        };
        write!(
            f,
            "{:?} {} {dir}, mse {}, {}",
            self.client,
            self.kind,
            match self.mse {
                Mse::Disabled => "disabled",
                Mse::Enabled => "enabled",
                Mse::Required => "required",
            },
            match self.net {
                Transports::Both => "TCP+uTP",
                Transports::TcpOnly => "TCP",
                Transports::UtpOnly => "uTP",
            }
        )
    }
}

/// What Dravity saw of one transfer.
#[derive(Debug)]
struct Seen {
    transport: Transport,
    encrypted: bool,
}

/// Whether the interop tests should run against `which` here.
fn enabled(which: Which) -> bool {
    if std::env::var("DRAVITY_INTEROP").as_deref() != Ok("1") {
        eprintln!("skipping: set DRAVITY_INTEROP=1 to run interop tests");
        return false;
    }
    if !installed(which.binary()) {
        eprintln!("skipping: {} is not installed", which.binary());
        return false;
    }
    true
}

fn dravity(case: &Case) -> Session {
    let mut c = SessionConfig::default();
    c.tick_interval = Duration::from_millis(100);
    c.stop_announce_timeout = Duration::from_secs(1);
    c.dht = None;
    c.port_mapping = None;
    c.listen = Some(ListenConfig::loopback(0));
    c.prefer_utp = case.net == Transports::UtpOnly;
    c.encryption = match case.mse {
        Mse::Disabled => EncryptionPolicy::Disabled,
        Mse::Enabled => EncryptionPolicy::Enabled,
        Mse::Required => EncryptionPolicy::Required,
    };
    // Slow enough (about 1.5 s) to see the connection mid-transfer.
    c.engine.upload_rate_limit = Some(2_500_000);
    c.engine.download_rate_limit = Some(2_500_000);
    c.engine.request_timeout = Duration::from_secs(10);
    Session::new(c).unwrap()
}

async fn start_client(case: &Case, dir: &Path) -> Result<Client, String> {
    let crypto = match case.mse {
        Mse::Disabled => Crypto::Plaintext,
        Mse::Enabled => Crypto::Prefer,
        Mse::Required => Crypto::Require,
    };
    Ok(match case.client {
        Which::Qbittorrent => Client::Qbittorrent(Qbittorrent::start(dir, crypto, case.net).await?),
        Which::Transmission => {
            Client::Transmission(Transmission::start(dir, crypto, case.net).await?)
        }
    })
}

/// Polls `handle` until `done`, remembering the first peer connection seen.
async fn watch(
    handle: &TorrentHandle,
    timeout: Duration,
    mut done: impl AsyncFnMut() -> Result<bool, String>,
) -> Result<Option<(Transport, bool)>, String> {
    let start = Instant::now();
    let mut seen = None;
    loop {
        if seen.is_none() {
            let t = Instant::now();
            let peers = handle.peers().await.map_err(|e| e.to_string())?;
            if std::env::var_os("DRAVITY_INTEROP_DEBUG").is_some() {
                eprintln!("peers() took {:?}: {}", t.elapsed(), peers.len());
            }
            if let Some(p) = peers.first() {
                seen = Some((p.transport, p.encrypted));
            }
        }
        if done().await? {
            return Ok(seen);
        }
        if start.elapsed() > timeout {
            return Err(format!("timed out; connection seen: {seen:?}"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn run(case: Case) -> Result<Seen, String> {
    let tracker = Tracker::start().await;
    let files = torrent::files();
    let bytes = torrent::make(&files, case.kind, &tracker.url());
    let m = Metainfo::from_bytes(&bytes).map_err(|e| e.to_string())?;
    // How each client names the torrent.
    let hash = match m.info_hash_v1() {
        Some(v1) => v1.to_hex(),
        None => m.info_hash_v2().map(|h| h.to_hex()).unwrap_or_default(),
    };
    let swarm = m.info_hash();
    let tmp = tempfile::tempdir().unwrap();
    let client = start_client(&case, &tmp.path().join("client")).await?;
    let session = dravity(&case);
    let port = session.listen_port().ok_or("no listen port")?;
    let dravity_addr = SocketAddr::from(([127, 0, 0, 1], port));
    let ours = tmp.path().join("dravity");
    let theirs = tmp.path().join("client-data");
    std::fs::create_dir_all(&theirs).unwrap();
    let start = Instant::now();
    let result = match case.direction {
        Direction::FromClient => {
            torrent::write_files(&theirs, &files);
            if case.kind == Kind::Hybrid && client.needs_pad_files() {
                torrent::write_pads(&theirs, &files);
            }
            client.add(&bytes, &theirs).await?;
            client.wait_done(&hash, Duration::from_secs(30)).await?;
            let handle = session
                .add_torrent(
                    m.clone(),
                    AddTorrentOptions::new(&ours).with_peers([client.peer()]),
                )
                .map_err(|e| e.to_string())?;
            let pieces = m.info().piece_count();
            let seen = watch(&handle, TRANSFER_TIMEOUT, async || {
                let stats = handle.stats().await.map_err(|e| e.to_string())?;
                Ok(stats.pieces_have == pieces)
            })
            .await;
            seen.and_then(|seen| torrent::same_files(&ours, &files).map(|()| seen))
        }
        Direction::ToClient => {
            torrent::write_files(&ours, &files);
            let handle = session
                .add_torrent(m.clone(), AddTorrentOptions::new(&ours))
                .map_err(|e| e.to_string())?;
            // Seeding, and announced, before the client asks the tracker.
            let wait = Instant::now();
            while !(handle.stats().await.map_err(|e| e.to_string())?.pieces_have
                == m.info().piece_count()
                && tracker.peers(swarm.as_bytes()).contains(&dravity_addr))
            {
                if wait.elapsed() > Duration::from_secs(20) {
                    return Err("dravity never seeded and announced".to_owned());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            client.add(&bytes, &theirs).await?;
            // The client dials us where it can be told to; otherwise we
            // dial it (the upload is the same either way).
            if !client.add_peer(&hash, dravity_addr).await {
                handle
                    .add_peers([client.peer()])
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let seen = watch(&handle, TRANSFER_TIMEOUT, async || {
                let p = client.progress(&hash).await;
                if std::env::var_os("DRAVITY_INTEROP_DEBUG").is_some() {
                    eprintln!("{:.2}s {p:?}", start.elapsed().as_secs_f64());
                }
                Ok(p.is_some_and(|p| p.done))
            })
            .await;
            seen.and_then(|seen| torrent::same_files(&theirs, &files).map(|()| seen))
        }
    };
    let secs = start.elapsed().as_secs_f64();
    session.shutdown().await;
    let version = client.version().to_owned();
    drop(client);
    let (transport, encrypted) = result
        .map_err(|e| format!("{version}: {e}"))?
        .ok_or_else(|| format!("{version}: done, but no connection was ever seen"))?;
    eprintln!(
        "interop: {version:<34} {case}: ok in {secs:.1}s over {transport:?}, {}",
        if encrypted { "encrypted" } else { "plaintext" }
    );
    Ok(Seen {
        transport,
        encrypted,
    })
}

/// Runs `cases` one after another (those whose description contains
/// `DRAVITY_INTEROP_ONLY`, if set); fails with every failure at the end.
/// `RUST_LOG` turns on Dravity's logs.
async fn run_all(cases: &[Case]) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let only = std::env::var("DRAVITY_INTEROP_ONLY").unwrap_or_default();
    let mut failures = Vec::new();
    for case in cases.iter().filter(|c| c.to_string().contains(&only)) {
        let outcome = run(*case).await.and_then(|seen| check(case, &seen));
        if let Err(e) = outcome {
            eprintln!("interop: FAILED {case}: {e}");
            failures.push(format!("{case}: {e}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Whether the connection Dravity saw is the one `case` asks for.
fn check(case: &Case, seen: &Seen) -> Result<(), String> {
    match case.mse {
        Mse::Required if !seen.encrypted => return Err("not encrypted".to_owned()),
        Mse::Disabled if seen.encrypted => return Err("encrypted".to_owned()),
        _ => {}
    }
    let want = match case.net {
        Transports::Both => None,
        Transports::TcpOnly => Some(Transport::Tcp),
        Transports::UtpOnly => Some(Transport::Utp),
    };
    match want {
        Some(want) if want != seen.transport => {
            Err(format!("over {:?}, not {want:?}", seen.transport))
        }
        _ => Ok(()),
    }
}

fn cases(client: Which, kinds: &[Kind], mse: Mse, net: Transports) -> Vec<Case> {
    kinds
        .iter()
        .flat_map(|&kind| {
            [Direction::FromClient, Direction::ToClient].map(|direction| Case {
                client,
                kind,
                direction,
                mse,
                net,
            })
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qbittorrent-nox and DRAVITY_INTEROP=1"]
async fn qbittorrent_v1_hybrid_and_v2_both_ways() {
    if !enabled(Which::Qbittorrent) {
        return;
    }
    let kinds = [Kind::V1, Kind::Hybrid, Kind::V2];
    run_all(&cases(
        Which::Qbittorrent,
        &kinds,
        Mse::Enabled,
        Transports::Both,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qbittorrent-nox and DRAVITY_INTEROP=1"]
async fn qbittorrent_with_mse_required() {
    if !enabled(Which::Qbittorrent) {
        return;
    }
    let kinds = [Kind::V1, Kind::Hybrid, Kind::V2];
    run_all(&cases(
        Which::Qbittorrent,
        &kinds,
        Mse::Required,
        Transports::Both,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qbittorrent-nox and DRAVITY_INTEROP=1"]
async fn qbittorrent_in_plaintext() {
    if !enabled(Which::Qbittorrent) {
        return;
    }
    let kinds = [Kind::V1, Kind::V2];
    run_all(&cases(
        Which::Qbittorrent,
        &kinds,
        Mse::Disabled,
        Transports::Both,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qbittorrent-nox and DRAVITY_INTEROP=1"]
async fn qbittorrent_over_tcp() {
    if !enabled(Which::Qbittorrent) {
        return;
    }
    let kinds = [Kind::V1, Kind::V2];
    run_all(&cases(
        Which::Qbittorrent,
        &kinds,
        Mse::Enabled,
        Transports::TcpOnly,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qbittorrent-nox and DRAVITY_INTEROP=1"]
async fn qbittorrent_over_utp() {
    if !enabled(Which::Qbittorrent) {
        return;
    }
    let kinds = [Kind::V1, Kind::Hybrid, Kind::V2];
    run_all(&cases(
        Which::Qbittorrent,
        &kinds,
        Mse::Enabled,
        Transports::UtpOnly,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs transmission-daemon and DRAVITY_INTEROP=1"]
async fn transmission_v1_and_hybrid_both_ways() {
    if !enabled(Which::Transmission) {
        return;
    }
    let kinds = [Kind::V1, Kind::Hybrid];
    run_all(&cases(
        Which::Transmission,
        &kinds,
        Mse::Enabled,
        Transports::Both,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs transmission-daemon and DRAVITY_INTEROP=1"]
async fn transmission_with_mse_required() {
    if !enabled(Which::Transmission) {
        return;
    }
    let kinds = [Kind::V1, Kind::Hybrid];
    run_all(&cases(
        Which::Transmission,
        &kinds,
        Mse::Required,
        Transports::Both,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs transmission-daemon and DRAVITY_INTEROP=1"]
async fn transmission_in_plaintext() {
    if !enabled(Which::Transmission) {
        return;
    }
    let kinds = [Kind::V1];
    run_all(&cases(
        Which::Transmission,
        &kinds,
        Mse::Disabled,
        Transports::Both,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs transmission-daemon and DRAVITY_INTEROP=1"]
async fn transmission_over_tcp() {
    if !enabled(Which::Transmission) {
        return;
    }
    let kinds = [Kind::V1, Kind::Hybrid];
    run_all(&cases(
        Which::Transmission,
        &kinds,
        Mse::Enabled,
        Transports::TcpOnly,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs transmission-daemon and DRAVITY_INTEROP=1"]
async fn transmission_over_utp() {
    if !enabled(Which::Transmission) {
        return;
    }
    let kinds = [Kind::V1, Kind::Hybrid];
    run_all(&cases(
        Which::Transmission,
        &kinds,
        Mse::Enabled,
        Transports::UtpOnly,
    ))
    .await;
}
