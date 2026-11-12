//! Peer extras: GeoIP countries in the peer list, DSCP marking, and web
//! seed back-off that survives a restart.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dravity_core::Metainfo;
use dravity_core::create::{CreateOptions, InputFile, TorrentCreator};
use dravity_session::persist::TorrentRecord;
use dravity_session::{AddTorrentOptions, GeoIp, ListenConfig, Session, SessionConfig};

static DATA: [u8; 300_000] = [5; 300_000];

fn torrent(web_seeds: Vec<String>) -> Metainfo {
    let mut options = CreateOptions::default();
    options.web_seeds = web_seeds;
    let mut c = TorrentCreator::new(
        "f.bin",
        vec![InputFile::new(["f.bin"], DATA.len() as u64)],
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
async fn peers_carry_their_country_and_connections_are_marked() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.bin"), &DATA[..]).unwrap();
    let mut sc = config();
    sc.peer_dscp = Some(8);
    let seed = Session::new(sc).unwrap();
    seed.add_torrent(torrent(Vec::new()), AddTorrentOptions::new(dir.path()))
        .unwrap();
    let addr = SocketAddr::from(([127, 0, 0, 1], seed.listen_port().unwrap()));

    let mut c = config();
    c.peer_dscp = Some(46);
    c.geoip = Some(Arc::new(
        GeoIp::parse("127.0.0.0,127.255.255.255,XL\n").unwrap(),
    ));
    c.download_rate_limit = Some(16 * 1024);
    let out = tempfile::tempdir().unwrap();
    let leecher = Session::new(c).unwrap();
    let handle = leecher
        .add_torrent(
            torrent(Vec::new()),
            AddTorrentOptions::new(out.path()).with_peers([addr]),
        )
        .unwrap();
    wait_for("a peer with a country", async || {
        handle
            .peers()
            .await
            .unwrap()
            .iter()
            .any(|p| p.country.map(|c| c.to_string()).as_deref() == Some("XL"))
    })
    .await;
    leecher.shutdown().await;
    seed.shutdown().await;

    let mut bad = config();
    bad.peer_dscp = Some(64);
    assert!(Session::new(bad).is_err());
}

fn record(state: &std::path::Path) -> TorrentRecord {
    let file = std::fs::read_dir(state)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "state"))
        .expect("a state file");
    TorrentRecord::decode(&std::fs::read(file).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_seed_back_off_survives_a_restart() {
    // Nothing listens on this port: every fetch fails.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/f.bin", closed.local_addr().unwrap());
    drop(closed);
    let state = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let mut c = config();
    c.listen = None;
    c.state_dir = Some(state.path().to_path_buf());
    c.web_seed_retry = Duration::from_secs(3600);
    if let Some(w) = c.web_seed.as_mut() {
        w.allow_private_addresses = true;
    }
    {
        let session = Session::new(c.clone()).unwrap();
        session
            .add_torrent(
                torrent(vec![url.clone()]),
                AddTorrentOptions::new(out.path()),
            )
            .unwrap();
        let dir = state.path().to_path_buf();
        wait_for("web seed failure saved", async || {
            std::fs::read_dir(&dir).unwrap().any(|e| {
                let p = e.unwrap().path();
                p.extension().is_some_and(|e| e == "state")
                    && std::fs::read(&p).is_ok_and(|b| b.windows(9).any(|w| w == b"web-seeds"))
            })
        })
        .await;
        session.shutdown().await;
    }
    let saved = record(state.path());
    let stat = saved.web_seeds.first().expect("a web seed stat");
    assert_eq!(stat.url, url);
    assert!(stat.failures >= 1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(stat.retry_at > now + 3000, "back-off was not kept");

    // After a restart the back-off is still in force: no new attempt, so
    // the failure count does not grow, and the record keeps it.
    let session = Session::new(c).unwrap();
    session.restore().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    session.shutdown().await;
    let again = record(state.path());
    assert_eq!(
        again.web_seeds.first().map(|s| s.failures),
        Some(stat.failures)
    );
}
