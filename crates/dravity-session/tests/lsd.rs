//! Local Service Discovery between two sessions (unicast mode, so the
//! test does not depend on multicast routing).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation
)]

use std::net::SocketAddr;
use std::time::Duration;

use dravity_core::Metainfo;
use dravity_core::create::{CreateOptions, CreateVersion, InputFile, TorrentCreator};
use dravity_engine::Alert;
use dravity_session::{AddTorrentOptions, ListenConfig, LsdConfig, Session, SessionConfig};

const LEN: usize = 100_000;

fn content() -> Vec<u8> {
    (0..LEN).map(|i| (i % 251) as u8).collect()
}

fn torrent(private: bool) -> Metainfo {
    let mut options = CreateOptions::new(CreateVersion::V1);
    options.private = private;
    let mut c = TorrentCreator::new(
        "lsd.bin",
        vec![InputFile::new(["lsd.bin"], LEN as u64)],
        true,
        options,
    )
    .unwrap();
    c.update(&content()).unwrap();
    Metainfo::from_bytes(&c.finish().unwrap()).unwrap()
}

fn free_udp() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn config(bind: SocketAddr, target: SocketAddr) -> SessionConfig {
    let mut c = SessionConfig::default();
    // Loopback HTTP/UDP trackers in these hermetic tests are private origins.
    c.tracker.allow_private_trackers = true;
    c.dht = None;
    c.port_mapping = None;
    c.listen = Some(ListenConfig::loopback(0));
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_millis(200);
    let mut lsd = LsdConfig::unicast(bind, vec![target]);
    lsd.interval = Duration::from_millis(200);
    c.lsd = Some(lsd);
    c
}

/// A seed and a leecher that know nothing of each other but LSD.
type Pair = (
    Session,
    Session,
    tokio::sync::broadcast::Receiver<dravity_session::SessionAlert>,
    tempfile::TempDir,
    tempfile::TempDir,
);

async fn pair(private: bool) -> Pair {
    let (a, b) = (free_udp(), free_udp());
    let seed_dir = tempfile::tempdir().unwrap();
    std::fs::write(seed_dir.path().join("lsd.bin"), content()).unwrap();
    let seed = Session::new(config(a, b)).unwrap();
    let mut seed_alerts = seed.subscribe();
    let m = torrent(private);
    let hash = m.info_hash();
    seed.add_torrent(m.clone(), AddTorrentOptions::new(seed_dir.path()))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(a) = seed_alerts.recv().await
                && a.info_hash == hash
                && a.alert == Alert::Finished
            {
                return;
            }
        }
    })
    .await
    .unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config(b, a)).unwrap();
    let alerts = leecher.subscribe();
    leecher
        .add_torrent(m, AddTorrentOptions::new(leech_dir.path()))
        .unwrap();
    (seed, leecher, alerts, seed_dir, leech_dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leecher_finds_a_seed_on_the_local_network() {
    let (seed, leecher, mut alerts, _s, leech_dir) = pair(false).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(a) = alerts.recv().await
                && a.alert == Alert::Finished
            {
                return;
            }
        }
    })
    .await
    .expect("the LSD peer was not used");
    assert_eq!(
        std::fs::read(leech_dir.path().join("lsd.bin")).unwrap(),
        content()
    );
    leecher.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_torrents_stay_off_lsd() {
    let (seed, leecher, _alerts, _s, _l) = pair(true).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let handle = leecher.torrent(torrent(true).info_hash()).unwrap();
    assert!(handle.peers().await.unwrap().is_empty());
    leecher.shutdown().await;
    seed.shutdown().await;
}
