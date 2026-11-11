//! The facade end to end: two clients over the in-memory network.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use dravity::core::create::{CreateOptions, InputFile, TorrentCreator};
use dravity::prelude::*;
use dravity::session::transport::MemoryNetwork;

static DATA: [u8; 100_000] = [4; 100_000];

fn torrent_bytes() -> Vec<u8> {
    let mut c = TorrentCreator::new(
        "f.bin",
        vec![InputFile::new(["f.bin"], DATA.len() as u64)],
        true,
        CreateOptions::default(),
    )
    .unwrap();
    c.update(&DATA[..]).unwrap();
    c.finish().unwrap()
}

fn builder(net: &MemoryNetwork, host: u8) -> ClientBuilder {
    Client::builder()
        .dht(false)
        .lsd(false)
        .port_mapping(false)
        .network(net.host(IpAddr::from([10, 1, 0, host])))
        .config(|c| c.tick_interval = Duration::from_millis(50))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_share_a_torrent() {
    let net = MemoryNetwork::new();
    let seed_dir = tempfile::tempdir().unwrap();
    std::fs::write(seed_dir.path().join("f.bin"), &DATA[..]).unwrap();
    let file = seed_dir.path().join("t.torrent");
    std::fs::write(&file, torrent_bytes()).unwrap();

    let seed = builder(&net, 1).build().unwrap();
    seed.add_torrent_file(&file, seed_dir.path()).unwrap();
    let seed_addr = SocketAddr::from(([10, 1, 0, 1], seed.listen_port().unwrap()));

    let out = tempfile::tempdir().unwrap();
    let leecher = builder(&net, 2).build().unwrap();
    let handle = leecher
        .add_torrent_bytes(&torrent_bytes(), out.path())
        .unwrap();
    handle.add_peers([seed_addr]).await.unwrap();
    let start = Instant::now();
    while handle.progress().await.unwrap().stats.wanted_left > 0 {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(std::fs::read(out.path().join("f.bin")).unwrap(), DATA);
    leecher.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test]
async fn errors_come_through_one_type() {
    let net = MemoryNetwork::new();
    let client = builder(&net, 3).build().unwrap();
    assert!(matches!(
        client.add_magnet("not a magnet", "x"),
        Err(Error::Magnet(_))
    ));
    assert!(matches!(
        client.add_torrent_bytes(b"garbage", "x"),
        Err(Error::Metainfo(_))
    ));
    assert!(matches!(
        client.add_torrent_file(std::path::Path::new("/nonexistent.torrent"), "x"),
        Err(Error::Io { .. })
    ));
    assert!(matches!(
        Client::builder().proxy("ftp://x:1"),
        Err(Error::Proxy(_))
    ));
    let mut bad = builder(&net, 4);
    bad = bad.config(|c| c.peer_dscp = Some(99));
    assert!(matches!(bad.build(), Err(Error::Session(_))));
    client.shutdown().await;
}
