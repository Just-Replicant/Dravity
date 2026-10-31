//! Message Stream Encryption between two sessions, for every pair of
//! policies, over TCP and uTP.

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
use dravity_session::{
    AddTorrentOptions, EncryptionPolicy, ListenConfig, Session, SessionConfig, TorrentHandle,
    Transport,
};
use sha1::{Digest, Sha1};

const PIECE: usize = 16 * 1024;
const LEN: usize = 200_000;

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

fn torrent() -> (Metainfo, Vec<u8>) {
    let content: Vec<u8> = (0..LEN as u32)
        .map(|i| (i.wrapping_mul(31) >> 3) as u8)
        .collect();
    let pieces: Vec<u8> = content
        .chunks(PIECE)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let info = dict(vec![
        ("length", Value::Int(LEN as i64)),
        ("name", bstr(b"secret.bin")),
        ("piece length", Value::Int(PIECE as i64)),
        ("pieces", bstr(&pieces)),
    ]);
    let encoded = dict(vec![("info", info)]).to_vec();
    (Metainfo::from_bytes(&encoded).unwrap(), content)
}

fn config(policy: EncryptionPolicy, utp: bool) -> SessionConfig {
    let mut c = SessionConfig::default();
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_secs(1);
    c.connect_timeout = Duration::from_secs(3);
    c.dht = None;
    c.listen = Some(ListenConfig::loopback(0));
    c.port_mapping = None;
    c.prefer_utp = utp;
    c.encryption = policy;
    // Slow enough that the connection can be observed mid-download.
    c.engine.upload_rate_limit = Some(150_000);
    c
}

/// The first connected peer's (transport, encrypted), polling `handle`.
async fn first_peer(handle: &TorrentHandle, within: Duration) -> Option<(Transport, bool)> {
    tokio::time::timeout(within, async {
        loop {
            if let Some(p) = handle.peers().await.unwrap().first() {
                return (p.transport, p.encrypted);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .ok()
}

/// Seeds with `seed`, downloads with `leech`. Returns the leecher's view of
/// the connection, or `None` if the two never connected.
async fn exchange(
    seed: EncryptionPolicy,
    leech: EncryptionPolicy,
    utp: bool,
) -> Option<(Transport, bool)> {
    let (m, content) = torrent();
    let hash = m.info_hash();
    let seed_dir = tempfile::tempdir().unwrap();
    std::fs::write(seed_dir.path().join("secret.bin"), &content).unwrap();
    let seeder = Session::new(config(seed, false)).unwrap();
    let port = seeder.listen_port().unwrap();
    let mut seeder_alerts = seeder.subscribe();
    seeder
        .add_torrent(m.clone(), AddTorrentOptions::new(seed_dir.path()))
        .unwrap();
    loop {
        let a = seeder_alerts.recv().await.unwrap();
        if a.info_hash == hash && a.alert == Alert::Finished {
            break;
        }
    }

    let leech_dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config(leech, utp)).unwrap();
    let mut alerts = leecher.subscribe();
    let handle = leecher
        .add_torrent(
            m,
            AddTorrentOptions::new(leech_dir.path())
                .with_peers([SocketAddr::from(([127, 0, 0, 1], port))]),
        )
        .unwrap();
    let seen = first_peer(&handle, Duration::from_secs(2)).await;
    if seen.is_some() {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let a = alerts.recv().await.unwrap();
                if a.info_hash == hash && a.alert == Alert::Finished {
                    break;
                }
            }
        })
        .await
        .expect("download finished");
        assert_eq!(
            std::fs::read(leech_dir.path().join("secret.bin")).unwrap(),
            content
        );
    }
    leecher.shutdown().await;
    seeder.shutdown().await;
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_policy_pair_over_tcp() {
    use EncryptionPolicy::{Disabled, Enabled, Required};
    // (seeder, leecher, expected: None = no connection, Some(encrypted)).
    let cases = [
        (Disabled, Disabled, Some(false)),
        (Disabled, Enabled, Some(false)),
        (Disabled, Required, None),
        (Enabled, Disabled, Some(false)),
        (Enabled, Enabled, Some(true)),
        (Enabled, Required, Some(true)),
        (Required, Disabled, None),
        (Required, Enabled, Some(true)),
        (Required, Required, Some(true)),
    ];
    let runs = cases.iter().map(|&(seed, leech, expected)| async move {
        let seen = exchange(seed, leech, false).await;
        (seed, leech, expected, seen)
    });
    for (seed, leech, expected, seen) in futures_util::future::join_all(runs).await {
        assert_eq!(
            seen.map(|(_, encrypted)| encrypted),
            expected,
            "seeder {seed}, leecher {leech}"
        );
        if let Some((transport, _)) = seen {
            assert_eq!(transport, Transport::Tcp);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encryption_over_utp() {
    use EncryptionPolicy::{Enabled, Required};
    for (seed, leech) in [(Required, Required), (Enabled, Enabled)] {
        let seen = exchange(seed, leech, true).await;
        assert_eq!(
            seen,
            Some((Transport::Utp, true)),
            "seeder {seed}, leecher {leech}"
        );
    }
}
