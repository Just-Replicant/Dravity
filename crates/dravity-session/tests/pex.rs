//! PEX flags (BEP 11) carry what a peer learned about another's encryption
//! support, and the receiver dials accordingly.
//!
//! A swarm of three: `A` seeds, `B` downloads from `A`, and `C` only knows
//! `B`. `B` shares `A` over PEX with the flags its own dial observed.

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
use dravity_session::{AddTorrentOptions, EncryptionPolicy, ListenConfig, Session, SessionConfig};
use sha1::{Digest, Sha1};

const PIECE: usize = 16 * 1024;
const LEN: usize = 400_000;

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
    // Slow enough that the swarm stays connected while PEX runs.
    c.engine.upload_rate_limit = Some(60_000);
    c.engine.pex_interval = Duration::from_millis(200);
    c
}

/// Whether `C` ends up connected to `A`, and if so encrypted.
async fn swarm(a: EncryptionPolicy, c: EncryptionPolicy) -> Option<bool> {
    let (m, content) = torrent();
    let hash = m.info_hash();
    let a_dir = tempfile::tempdir().unwrap();
    std::fs::write(a_dir.path().join("secret.bin"), &content).unwrap();
    let a_session = Session::new(config(a, false)).unwrap();
    let a_addr = SocketAddr::from(([127, 0, 0, 1], a_session.listen_port().unwrap()));
    let mut alerts = a_session.subscribe();
    a_session
        .add_torrent(m.clone(), AddTorrentOptions::new(a_dir.path()))
        .unwrap();
    loop {
        let a = alerts.recv().await.unwrap();
        if a.info_hash == hash && a.alert == Alert::Finished {
            break;
        }
    }

    let b_dir = tempfile::tempdir().unwrap();
    let b_session = Session::new(config(EncryptionPolicy::Enabled, false)).unwrap();
    let b_addr = SocketAddr::from(([127, 0, 0, 1], b_session.listen_port().unwrap()));
    let b = b_session
        .add_torrent(
            m.clone(),
            AddTorrentOptions::new(b_dir.path()).with_peers([a_addr]),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while b.peers().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("B connects to A");

    let c_dir = tempfile::tempdir().unwrap();
    let c_session = Session::new(config(c, false)).unwrap();
    let c_handle = c_session
        .add_torrent(m, AddTorrentOptions::new(c_dir.path()).with_peers([b_addr]))
        .unwrap();
    // Several PEX rounds; C's dial to A, if any, finishes well within this.
    let seen = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let peers = c_handle.peers().await.unwrap();
            if let Some(p) = peers.iter().find(|p| p.info.addr == a_addr) {
                return p.encrypted;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .ok();
    c_session.shutdown().await;
    b_session.shutdown().await;
    a_session.shutdown().await;
    seen
}

#[tokio::test(flavor = "multi_thread")]
async fn required_dials_a_peer_flagged_as_encrypting() {
    // B's MSE handshake with A succeeds, so A is shared with flag 0x01.
    assert_eq!(
        swarm(EncryptionPolicy::Enabled, EncryptionPolicy::Required).await,
        Some(true)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn required_skips_a_peer_flagged_as_plain() {
    // A refuses MSE, so B falls back to plain and shares A without 0x01: C
    // never dials it rather than failing a required handshake.
    assert_eq!(
        swarm(EncryptionPolicy::Disabled, EncryptionPolicy::Required).await,
        None
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn enabled_dials_a_plain_peer_in_plaintext() {
    // C opens with a plain handshake (no MSE attempt to fall back from).
    assert_eq!(
        swarm(EncryptionPolicy::Disabled, EncryptionPolicy::Enabled).await,
        Some(false)
    );
}
