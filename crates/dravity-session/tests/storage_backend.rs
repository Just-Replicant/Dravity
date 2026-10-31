//! `SessionConfig::storage_backend`: io_uring when the `uring` feature is on
//! and the kernel allows it, blocking I/O otherwise, with the same results.

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
use std::time::Duration;

use dravity_bencode::{Dict, Value};
use dravity_core::Metainfo;
use dravity_engine::Alert;
use dravity_session::{AddTorrentOptions, ListenConfig, Session, SessionConfig, StorageBackend};
use sha1::{Digest, Sha1};

const PIECE: usize = 16 * 1024;

fn bstr(b: &[u8]) -> Value<'static> {
    Value::Bytes(Cow::Owned(b.to_vec()))
}

fn dict(entries: Vec<(&str, Value<'static>)>) -> Value<'static> {
    let mut d = Dict::new();
    for (k, v) in entries {
        d.insert(Cow::Owned(k.as_bytes().to_vec()), v);
    }
    Value::Dict(d)
}

/// A two-file torrent named `pair`.
fn torrent() -> (Metainfo, Vec<(&'static str, Vec<u8>)>) {
    let files = vec![
        (
            "one.bin",
            (0..70_000u32).map(|i| (i % 241) as u8).collect::<Vec<u8>>(),
        ),
        ("two.bin", (0..33_333u32).map(|i| (i % 239) as u8).collect()),
    ];
    let stream: Vec<u8> = files.iter().flat_map(|f| f.1.clone()).collect();
    let pieces: Vec<u8> = stream
        .chunks(PIECE)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let list = files
        .iter()
        .map(|(name, data)| {
            dict(vec![
                ("length", Value::Int(data.len() as i64)),
                ("path", Value::List(vec![bstr(name.as_bytes())])),
            ])
        })
        .collect();
    let info = dict(vec![
        ("files", Value::List(list)),
        ("name", bstr(b"pair")),
        ("piece length", Value::Int(PIECE as i64)),
        ("pieces", bstr(&pieces)),
    ]);
    let encoded = dict(vec![("info", info)]).to_vec();
    (Metainfo::from_bytes(&encoded).unwrap(), files)
}

fn config(backend: StorageBackend) -> SessionConfig {
    let mut c = SessionConfig::default();
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_secs(1);
    c.dht = None;
    c.listen = Some(ListenConfig::loopback(0));
    c.port_mapping = None;
    c.storage_backend = backend;
    c
}

/// Whether this build and this machine run io_uring.
fn uring_here() -> bool {
    #[cfg(all(feature = "uring", target_os = "linux"))]
    {
        dravity_storage::UringIo::start().is_ok()
    }
    #[cfg(not(all(feature = "uring", target_os = "linux")))]
    {
        false
    }
}

async fn wait_finished(session: &Session, m: &Metainfo) {
    let mut alerts = session.subscribe();
    let hash = m.info_hash();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match alerts.recv().await {
                Ok(a) if a.info_hash == hash && a.alert == Alert::Finished => return,
                Ok(a) if a.info_hash == hash => {
                    assert!(
                        !matches!(a.alert, Alert::HashFailed { .. }),
                        "{:?}",
                        a.alert
                    );
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("{e}"),
            }
        }
    })
    .await
    .expect("finished in time");
}

/// Seeds with `seed_backend`, downloads with `leech_backend`, checks files.
async fn round_trip(seed_backend: StorageBackend, leech_backend: StorageBackend) {
    let (m, files) = torrent();
    let seed_dir = tempfile::tempdir().unwrap();
    for (name, data) in &files {
        std::fs::create_dir_all(seed_dir.path().join("pair")).unwrap();
        std::fs::write(seed_dir.path().join("pair").join(name), data).unwrap();
    }
    let seeder = Session::new(config(seed_backend)).unwrap();
    let port = seeder.listen_port().unwrap();
    let seeding = wait_finished(&seeder, &m);
    seeder
        .add_torrent(m.clone(), AddTorrentOptions::new(seed_dir.path()))
        .unwrap();
    seeding.await;

    let leech_dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config(leech_backend)).unwrap();
    let done = wait_finished(&leecher, &m);
    leecher
        .add_torrent(
            m.clone(),
            AddTorrentOptions::new(leech_dir.path())
                .with_peers([SocketAddr::from(([127, 0, 0, 1], port))]),
        )
        .unwrap();
    done.await;
    for (name, data) in &files {
        assert_eq!(
            &std::fs::read(leech_dir.path().join("pair").join(name)).unwrap(),
            data
        );
    }
    leecher.shutdown().await;
    seeder.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn io_uring_is_used_when_available_and_falls_back_otherwise() {
    let session = Session::new(config(StorageBackend::IoUring)).unwrap();
    let expected = if uring_here() {
        StorageBackend::IoUring
    } else {
        StorageBackend::Blocking
    };
    assert_eq!(session.storage_backend(), expected);
    let blocking = Session::new(config(StorageBackend::Blocking)).unwrap();
    assert_eq!(blocking.storage_backend(), StorageBackend::Blocking);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn torrents_move_between_io_uring_and_blocking_sessions() {
    // Without io_uring both sides quietly use blocking I/O: still correct.
    round_trip(StorageBackend::IoUring, StorageBackend::Blocking).await;
    round_trip(StorageBackend::Blocking, StorageBackend::IoUring).await;
}
