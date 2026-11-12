//! The IP filter: refused before the handshake, never dialed, and
//! applied live to open connections.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dravity_core::Metainfo;
use dravity_core::create::{CreateOptions, InputFile, TorrentCreator};
use dravity_session::{AddTorrentOptions, IpFilter, ListenConfig, Session, SessionConfig};

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

fn loopback() -> IpFilter {
    IpFilter::parse("127.0.0.0/8\n::1\n").unwrap()
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

async fn wait_for(what: &str, mut f: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !f().await {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filtered_peer_is_never_dialed_until_the_filter_changes() {
    let (seed, addr, _d) = seed(None).await;
    let dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config(Some(loopback()))).unwrap();
    assert_eq!(leecher.ip_filter().len(), 2);
    let handle = leecher
        .add_torrent(
            torrent(),
            AddTorrentOptions::new(dir.path()).with_peers([addr]),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(leecher.connection_count(), 0);
    assert_eq!(handle.progress().await.unwrap().stats.bytes_have, 0);
    leecher.set_ip_filter(IpFilter::new());
    handle.add_peers(vec![addr]).await.unwrap();
    wait_for("download", async || {
        handle.progress().await.unwrap().stats.wanted_left == 0
    })
    .await;
    leecher.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listener_refuses_filtered_addresses_and_live_changes_close_peers() {
    let (seed, addr, _d) = seed(Some(loopback())).await;
    let dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config(None)).unwrap();
    let handle = leecher
        .add_torrent(
            torrent(),
            AddTorrentOptions::new(dir.path()).with_peers([addr]),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(handle.progress().await.unwrap().stats.bytes_have, 0);
    assert_eq!(seed.connection_count(), 0);
    leecher.shutdown().await;

    // Unfiltered, a connection opens; filtering live closes it.
    seed.set_ip_filter(IpFilter::new());
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(None);
    c.download_rate_limit = Some(16 * 1024);
    let slow = Session::new(c).unwrap();
    slow.add_torrent(
        torrent(),
        AddTorrentOptions::new(dir.path()).with_peers([addr]),
    )
    .unwrap();
    wait_for("connection", async || seed.connection_count() > 0).await;
    seed.set_ip_filter(loopback());
    wait_for("closed", async || seed.connection_count() == 0).await;
    slow.shutdown().await;
    seed.shutdown().await;
}
