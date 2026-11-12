//! Tracker scrapes (BEP 15) through a torrent handle and on a schedule.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::time::Duration;

use dravity_core::Metainfo;
use dravity_core::create::{CreateOptions, InputFile, TorrentCreator};
use dravity_session::{
    AddTorrentOptions, ScrapeOutcome, ScrapeStats, Session, SessionConfig, TrackerStatus,
};
use dravity_tracker::udp;
use tokio::net::UdpSocket;

const STATS: ScrapeStats = ScrapeStats {
    seeders: 12,
    leechers: 34,
    downloaded: 567,
};

/// A UDP tracker that answers connect, announce (no peers) and scrape.
async fn tracker() -> String {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let url = format!("udp://{}", socket.local_addr().unwrap());
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                return;
            };
            if n < 16 {
                continue;
            }
            let action = u32::from_be_bytes(buf[8..12].try_into().unwrap());
            let tid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            let reply = match action {
                0 => udp::encode_connect_response(tid, 9).to_vec(),
                1 => udp::encode_announce_response(tid, 1800, 0, 0, &[]),
                2 => udp::encode_scrape_response(tid, &vec![STATS; (n - 16) / 20]),
                _ => continue,
            };
            let _ = socket.send_to(&reply, from).await;
        }
    });
    url
}

fn torrent(tracker: &str) -> Metainfo {
    let mut options = CreateOptions::default();
    options.trackers = vec![vec![tracker.to_owned()]];
    let mut c = TorrentCreator::new(
        "x.bin",
        vec![InputFile::new(["x.bin"], 1000)],
        true,
        options,
    )
    .unwrap();
    c.update(&[5; 1000]).unwrap();
    Metainfo::from_bytes(&c.finish().unwrap()).unwrap()
}

fn config() -> SessionConfig {
    let mut c = SessionConfig::default();
    // Loopback HTTP/UDP trackers in these hermetic tests are private origins.
    c.tracker.allow_private_trackers = true;
    c.dht = None;
    c.listen = None;
    c.port_mapping = None;
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_millis(200);
    c
}

fn scraped(list: &[TrackerStatus]) -> bool {
    list.iter()
        .any(|t| t.scrape == Some(ScrapeOutcome::Ok(STATS)) && t.scraped.is_some())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handle_scrapes_on_request() {
    let url = tracker().await;
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let handle = session
        .add_torrent(torrent(&url), AddTorrentOptions::new(dir.path()))
        .unwrap();
    assert!(handle.trackers().await.unwrap()[0].scrape.is_none());
    let list = handle.scrape().await.unwrap();
    assert!(scraped(&list), "{list:?}");
    // The result stays in the tracker list.
    assert!(scraped(&handle.trackers().await.unwrap()));
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrapes_run_on_a_schedule_and_failures_are_reported() {
    let url = tracker().await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = config();
    c.scrape_interval = Some(Duration::from_secs(60));
    let session = Session::new(c).unwrap();
    let handle = session
        .add_torrent(torrent(&url), AddTorrentOptions::new(dir.path()))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !scraped(&handle.trackers().await.unwrap()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no scheduled scrape");
    session.shutdown().await;

    // An HTTP tracker without `announce` in its path cannot be scraped.
    let session = Session::new(config()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let handle = session
        .add_torrent(
            torrent("http://127.0.0.1:9/tracker"),
            AddTorrentOptions::new(dir.path()),
        )
        .unwrap();
    let list = handle.scrape().await.unwrap();
    assert!(
        matches!(list[0].scrape, Some(ScrapeOutcome::Failed { .. })),
        "{list:?}"
    );
    session.shutdown().await;
}
