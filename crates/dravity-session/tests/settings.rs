//! Live settings: transactional updates, session-wide rate limits and the
//! torrent queue.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::panic
)]

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use dravity_core::create::{CreateOptions, CreateVersion, InputFile, TorrentCreator};
use dravity_core::{InfoHash, Metainfo};
use dravity_engine::Alert;
use dravity_session::{
    AddTorrentOptions, ListenConfig, Session, SessionAlert, SessionConfig, SettingsError,
    SettingsUpdate,
};
use tokio::sync::broadcast;

fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

fn torrent(name: &str, data: &[u8]) -> Metainfo {
    let mut c = TorrentCreator::new(
        name,
        vec![InputFile::new([name], data.len() as u64)],
        true,
        CreateOptions::new(CreateVersion::V1),
    )
    .unwrap();
    c.update(data).unwrap();
    Metainfo::from_bytes(&c.finish().unwrap()).unwrap()
}

fn config() -> SessionConfig {
    let mut c = SessionConfig::default();
    c.dht = None;
    c.port_mapping = None;
    c.listen = Some(ListenConfig::loopback(0));
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_millis(200);
    c
}

async fn until_finished(rx: &mut broadcast::Receiver<SessionAlert>, hash: InfoHash) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Ok(a) if a.info_hash == hash && a.alert == Alert::Finished => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("{e}"),
            }
        }
    })
    .await
    .expect("not finished");
}

/// A session seeding each `(name, data)` from `dir`.
async fn seed(dir: &Path, files: &[(&str, Vec<u8>)]) -> (Session, SocketAddr) {
    let session = Session::new(config()).unwrap();
    let mut alerts = session.subscribe();
    for (name, data) in files {
        std::fs::write(dir.join(name), data).unwrap();
        let m = torrent(name, data);
        let hash = m.info_hash();
        session.add_torrent(m, AddTorrentOptions::new(dir)).unwrap();
        until_finished(&mut alerts, hash).await;
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], session.listen_port().unwrap()));
    (session, addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn updates_are_all_or_nothing() {
    let session = Session::new(config()).unwrap();
    let before = session.settings();
    let mut bad = SettingsUpdate::default();
    bad.download_rate_limit = Some(Some(1 << 20));
    bad.max_connections = Some(Some(3));
    bad.upload_rate_limit = Some(Some(1024));
    assert!(matches!(
        session.apply_settings(bad),
        Err(SettingsError::Invalid {
            field: "upload_rate_limit",
            ..
        })
    ));
    assert_eq!(session.settings(), before, "nothing applied");
    let mut ok = SettingsUpdate::default();
    ok.download_rate_limit = Some(Some(1 << 20));
    ok.max_connections = Some(Some(7));
    ok.max_active_seeds = Some(Some(3));
    let now = session.apply_settings(ok).unwrap();
    assert_eq!(now, session.settings());
    assert_eq!(now.download_rate_limit, Some(1 << 20));
    assert_eq!(session.max_connections(), Some(7));
    session.set_max_connections(None);
    assert_eq!(session.settings().max_connections, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_wide_download_limit_paces_every_torrent() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(96 * 1024, 1);
    let (seed, addr) = seed(dir.path(), &[("paced.bin", data.clone())]).await;
    let mut c = config();
    c.download_rate_limit = Some(32 * 1024);
    let leecher = Session::new(c).unwrap();
    let mut alerts = leecher.subscribe();
    let out = tempfile::tempdir().unwrap();
    let m = torrent("paced.bin", &data);
    let hash = m.info_hash();
    let started = Instant::now();
    leecher
        .add_torrent(m, AddTorrentOptions::new(out.path()).with_peers([addr]))
        .unwrap();
    until_finished(&mut alerts, hash).await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(1800),
        "96 KiB at 32 KiB/s took {took:?}"
    );
    assert_eq!(std::fs::read(out.path().join("paced.bin")).unwrap(), data);
    leecher.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_runs_one_download_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (content(64 * 1024, 2), content(64 * 1024, 3));
    let (seed, addr) = seed(dir.path(), &[("a.bin", a.clone()), ("b.bin", b.clone())]).await;
    let mut c = config();
    c.max_active_downloads = Some(1);
    let leecher = Session::new(c).unwrap();
    let mut alerts = leecher.subscribe();
    let out = tempfile::tempdir().unwrap();
    let (ma, mb) = (torrent("a.bin", &a), torrent("b.bin", &b));
    let (ha, hb) = (ma.info_hash(), mb.info_hash());
    leecher
        .add_torrent(ma, AddTorrentOptions::new(out.path()).with_peers([addr]))
        .unwrap();
    leecher
        .add_torrent(mb, AddTorrentOptions::new(out.path()).with_peers([addr]))
        .unwrap();
    let summary = |h| {
        leecher
            .list()
            .into_iter()
            .find(|t| t.info_hash == h)
            .unwrap()
    };
    assert!(summary(ha).running && !summary(ha).queued);
    assert!(
        summary(hb).queued && !summary(hb).running,
        "{:?}",
        summary(hb)
    );
    until_finished(&mut alerts, ha).await;
    // `a` now seeds, which frees the download slot for `b`.
    until_finished(&mut alerts, hb).await;
    assert!(!summary(hb).queued);
    assert_eq!(std::fs::read(out.path().join("b.bin")).unwrap(), b);
    // Lifting the limit live leaves nothing queued; limiting seeds to one
    // queues the newer seed.
    let mut u = SettingsUpdate::default();
    u.max_active_downloads = Some(None);
    u.max_active_seeds = Some(Some(1));
    leecher.apply_settings(u).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !summary(hb).queued {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the second seed was not queued");
    assert!(summary(ha).running);
    leecher.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_settings_survive_a_restart() {
    let state = tempfile::tempdir().unwrap();
    let mut c = config();
    c.listen = None;
    c.state_dir = Some(state.path().to_path_buf());
    let expected = {
        let s = Session::new(c.clone()).unwrap();
        let mut update = SettingsUpdate::default();
        update.download_rate_limit = Some(Some(50_000));
        update.max_connections = Some(Some(7));
        update.max_active_downloads = Some(Some(2));
        let applied = s.apply_settings(update).unwrap();
        s.shutdown().await;
        applied
    };
    let s = Session::new(c.clone()).unwrap();
    // Not applied until restore: a fresh session starts from its config.
    assert_eq!(s.settings().download_rate_limit, None);
    let report = s.restore().await.unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(s.settings(), expected);
    s.shutdown().await;

    // A damaged settings file is reported, not fatal.
    std::fs::write(state.path().join("settings.bencode"), b"junk").unwrap();
    let s = Session::new(c).unwrap();
    let report = s.restore().await.unwrap();
    assert_eq!(report.failed.len(), 1, "{report:?}");
    s.shutdown().await;
}
