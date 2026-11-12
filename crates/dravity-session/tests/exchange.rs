//! Tracker exchange (`lt_tex`) between two sessions over loopback.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use dravity_core::Metainfo;
use dravity_core::create::{CreateOptions, InputFile, TorrentCreator};
use dravity_session::{AddTorrentOptions, ListenConfig, Session, SessionConfig};

static DATA: [u8; 40_000] = [7; 40_000];

fn torrent(trackers: &[&str], private: bool) -> Metainfo {
    let mut options = CreateOptions::default();
    options.trackers = trackers.iter().map(|t| vec![(*t).to_owned()]).collect();
    options.private = private;
    let mut c = TorrentCreator::new(
        "tex.bin",
        vec![InputFile::new(["tex.bin"], DATA.len() as u64)],
        true,
        options,
    )
    .unwrap();
    c.update(&DATA[..]).unwrap();
    Metainfo::from_bytes(&c.finish().unwrap()).unwrap()
}

fn config() -> SessionConfig {
    let mut c = SessionConfig::default();
    // Loopback HTTP/UDP trackers in these hermetic tests are private origins.
    c.tracker.allow_private_trackers = true;
    c.dht = None;
    c.port_mapping = None;
    c.listen = Some(ListenConfig::loopback(0));
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_millis(100);
    c
}

/// The trackers the leecher knows after downloading from a seed whose
/// copy of the torrent lists `udp://127.0.0.1:9/announce`.
async fn learned(private: bool) -> Vec<String> {
    let seed_dir = tempfile::tempdir().unwrap();
    std::fs::write(seed_dir.path().join("tex.bin"), &DATA[..]).unwrap();
    let seed = Session::new(config()).unwrap();
    seed.add_torrent(
        torrent(&["udp://127.0.0.1:9/announce"], private),
        AddTorrentOptions::new(seed_dir.path()),
    )
    .unwrap();
    let addr = SocketAddr::from(([127, 0, 0, 1], seed.listen_port().unwrap()));
    let dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config()).unwrap();
    let m = torrent(&[], private);
    let handle = leecher
        .add_torrent(m, AddTorrentOptions::new(dir.path()).with_peers([addr]))
        .unwrap();
    let start = Instant::now();
    let mut urls = Vec::new();
    while start.elapsed() < Duration::from_secs(5) {
        urls = handle
            .trackers()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.url)
            .collect();
        if !urls.is_empty() || (private && start.elapsed() > Duration::from_secs(2)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    leecher.shutdown().await;
    seed.shutdown().await;
    urls
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_shares_its_trackers() {
    assert_eq!(learned(false).await, ["udp://127.0.0.1:9/announce"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_torrents_never_exchange_trackers() {
    assert!(learned(true).await.is_empty());
}
