//! Whole swarms over the in-memory transport: no sockets, and faults
//! injected on demand.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use dravity_core::Metainfo;
use dravity_core::create::{CreateOptions, InputFile, TorrentCreator};
use dravity_session::transport::MemoryNetwork;
use dravity_session::{AddTorrentOptions, Session, SessionConfig, TorrentHandle};

static DATA: [u8; 400_000] = [9; 400_000];

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

fn config(net: &MemoryNetwork, host: u8) -> SessionConfig {
    let mut c = SessionConfig::default();
    c.dht = None;
    c.port_mapping = None;
    c.lsd = None;
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_millis(100);
    c.network = Some(net.host(IpAddr::from([10, 0, 0, host])));
    c
}

fn addr(s: &Session, host: u8) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, host], s.listen_port().unwrap()))
}

async fn finished(h: &TorrentHandle) -> bool {
    h.progress().await.unwrap().stats.wanted_left == 0
}

async fn wait_for(what: &str, mut f: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !f().await {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_swarm_downloads_entirely_in_memory() {
    let net = MemoryNetwork::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.bin"), &DATA[..]).unwrap();
    let seed = Session::new(config(&net, 1)).unwrap();
    let seed_handle = seed
        .add_torrent(torrent(), AddTorrentOptions::new(dir.path()))
        .unwrap();
    let seed_addr = addr(&seed, 1);

    let mut leechers = Vec::new();
    for host in 2..=4u8 {
        let out = tempfile::tempdir().unwrap();
        let s = Session::new(config(&net, host)).unwrap();
        let h = s
            .add_torrent(
                torrent(),
                AddTorrentOptions::new(out.path()).with_peers([seed_addr]),
            )
            .unwrap();
        leechers.push((s, h, out));
    }
    for (_, h, out) in &leechers {
        wait_for("leecher finished", async || finished(h).await).await;
        assert_eq!(std::fs::read(out.path().join("f.bin")).unwrap(), DATA);
    }
    // Every peer address the seed saw is on the memory network.
    let peers = seed_handle.peers().await.unwrap();
    assert!(
        peers
            .iter()
            .all(|p| p.info.addr.ip().to_string().starts_with("10.0.0."))
    );
    for (s, _, _) in leechers {
        s.shutdown().await;
    }
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_seed_blocks_until_the_fault_is_lifted() {
    let net = MemoryNetwork::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.bin"), &DATA[..]).unwrap();
    let seed = Session::new(config(&net, 1)).unwrap();
    seed.add_torrent(torrent(), AddTorrentOptions::new(dir.path()))
        .unwrap();
    let seed_addr = addr(&seed, 1);
    net.set_unreachable(seed_addr, true);

    let out = tempfile::tempdir().unwrap();
    let leecher = Session::new(config(&net, 2)).unwrap();
    let h = leecher
        .add_torrent(
            torrent(),
            AddTorrentOptions::new(out.path()).with_peers([seed_addr]),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.progress().await.unwrap().stats.bytes_have, 0);
    assert_eq!(leecher.connection_count(), 0);

    net.set_unreachable(seed_addr, false);
    h.add_peers([seed_addr]).await.unwrap();
    wait_for("download after the fault", async || finished(&h).await).await;
    leecher.shutdown().await;
    seed.shutdown().await;
}
