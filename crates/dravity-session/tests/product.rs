//! Phase 4 session features over loopback: file priorities, streaming,
//! pause/resume/remove, persistence and the watch folder.

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
use std::path::Path;
use std::time::Duration;

use dravity_bencode::{Dict, Value};
use dravity_core::{InfoHash, Magnet, Metainfo};
use dravity_engine::{Alert, Priority, TorrentStatus};
use dravity_session::{
    AddTorrentOptions, Durability, ListenConfig, PreallocateMode, Session, SessionAlert,
    SessionConfig, SessionError, WatchConfig,
};
use sha1::{Digest, Sha1};
use tokio::sync::broadcast;

const PIECE_LEN: usize = 32 * 1024;

fn bytes(b: &[u8]) -> Value<'static> {
    Value::Bytes(Cow::Owned(b.to_vec()))
}

fn dict(entries: Vec<(&str, Value<'static>)>) -> Value<'static> {
    let mut d = Dict::new();
    for (k, v) in entries {
        d.insert(Cow::Owned(k.as_bytes().to_vec()), v);
    }
    Value::Dict(d)
}

/// A two-file torrent (`payload/a.bin`, `payload/b.bin`); returns its
/// encoded form and content.
fn make_torrent(len_a: usize, len_b: usize) -> (Vec<u8>, Vec<u8>) {
    let content: Vec<u8> = (0..len_a + len_b)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[2])
        .collect();
    let pieces: Vec<u8> = content
        .chunks(PIECE_LEN)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let file = |name: &str, len: usize| {
        dict(vec![
            ("length", Value::Int(len as i64)),
            ("path", Value::List(vec![bytes(name.as_bytes())])),
        ])
    };
    let info = dict(vec![
        ("name", bytes(b"payload")),
        (
            "files",
            Value::List(vec![file("a.bin", len_a), file("b.bin", len_b)]),
        ),
        ("piece length", Value::Int(PIECE_LEN as i64)),
        ("pieces", bytes(&pieces)),
    ]);
    (dict(vec![("info", info)]).to_vec(), content)
}

fn metainfo(encoded: &[u8]) -> Metainfo {
    Metainfo::from_bytes(encoded).unwrap()
}

fn config() -> SessionConfig {
    let mut c = SessionConfig::default();
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_secs(1);
    c.dht = None;
    c.listen = Some(ListenConfig::loopback(0));
    c.port_mapping = None;
    c
}

fn write_files(dir: &Path, content: &[u8], len_a: usize) {
    std::fs::create_dir_all(dir.join("payload")).unwrap();
    std::fs::write(dir.join("payload/a.bin"), &content[..len_a]).unwrap();
    std::fs::write(dir.join("payload/b.bin"), &content[len_a..]).unwrap();
}

/// Waits for an alert of `info_hash` matching `f`.
async fn until(
    rx: &mut broadcast::Receiver<SessionAlert>,
    info_hash: InfoHash,
    f: impl Fn(&Alert) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Ok(a) if a.info_hash == info_hash && f(&a.alert) => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("alert channel closed: {e}"),
            }
        }
    })
    .await
    .expect("timed out waiting for an alert");
}

async fn until_finished(rx: &mut broadcast::Receiver<SessionAlert>, info_hash: InfoHash) {
    until(rx, info_hash, |a| *a == Alert::Finished).await;
}

/// A session seeding the torrent from a temporary directory.
struct Seed {
    session: Session,
    addr: SocketAddr,
    _dir: tempfile::TempDir,
}

async fn seed(encoded: &[u8], content: &[u8], len_a: usize) -> Seed {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path(), content, len_a);
    let session = Session::new(config()).unwrap();
    let port = session.listen_port().unwrap();
    let mut alerts = session.subscribe();
    let m = metainfo(encoded);
    let hash = m.info_hash();
    session
        .add_torrent(m, AddTorrentOptions::new(dir.path()))
        .unwrap();
    until_finished(&mut alerts, hash).await;
    Seed {
        session,
        addr: SocketAddr::from(([127, 0, 0, 1], port)),
        _dir: dir,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skipped_files_are_not_downloaded_until_wanted() {
    let (len_a, len_b) = (100_000, 120_000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let seed = seed(&encoded, &content, len_a).await;
    let dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config()).unwrap();
    let mut alerts = leecher.subscribe();
    let m = metainfo(&encoded);
    let hash = m.info_hash();
    let handle = leecher
        .add_torrent(
            m,
            AddTorrentOptions::new(dir.path())
                .with_peers([seed.addr])
                .with_file_priorities([Priority::Normal, Priority::Skip]),
        )
        .unwrap();
    until_finished(&mut alerts, hash).await;
    assert_eq!(
        std::fs::read(dir.path().join("payload/a.bin")).unwrap(),
        &content[..len_a]
    );
    let progress = handle.progress().await.unwrap();
    assert_eq!(progress.stats.status, TorrentStatus::Finished);
    assert_eq!(progress.stats.wanted_left, 0);
    let files = progress.files.unwrap();
    assert_eq!(files[0].have, len_a as u64);
    assert_eq!(files[1].priority, Priority::Skip);
    // Only the piece shared with a.bin touched b.bin.
    assert!(files[1].have < PIECE_LEN as u64, "{files:?}");
    assert!(progress.stats.bytes_have < (len_a + len_b) as u64);

    assert!(matches!(
        leecher
            .set_file_priorities(hash, vec![Priority::High])
            .await,
        Err(SessionError::FileCount {
            expected: 2,
            got: 1
        })
    ));
    leecher
        .set_file_priority(hash, 1, Priority::High)
        .await
        .unwrap();
    until_finished(&mut alerts, hash).await;
    assert_eq!(
        std::fs::read(dir.path().join("payload/b.bin")).unwrap(),
        &content[len_a..]
    );
    let summary = &leecher.list()[0];
    assert_eq!(
        summary.file_priorities,
        vec![Priority::Normal, Priority::High]
    );
    leecher.shutdown().await;
    seed.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_streams_from_any_position_while_skipped() {
    let (len_a, len_b) = (70_000, 200_000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let seed = seed(&encoded, &content, len_a).await;
    let dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config()).unwrap();
    let m = metainfo(&encoded);
    let handle = leecher
        .add_torrent(
            m,
            AddTorrentOptions::new(dir.path())
                .with_peers([seed.addr])
                .with_file_priorities([Priority::Skip, Priority::Skip]),
        )
        .unwrap();
    let mut stream = handle.open_file(1).await.unwrap();
    assert_eq!(stream.len(), len_b as u64);
    // Seek into the middle first, then read from the start.
    let middle = tokio::time::timeout(Duration::from_secs(30), stream.read_range(150_000, 20_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(middle, &content[len_a + 150_000..len_a + 170_000]);
    assert_eq!(stream.position(), 170_000);
    let head = stream.read_range(0, 1000).await.unwrap();
    assert_eq!(head, &content[len_a..len_a + 1000]);
    // Past the end: nothing.
    stream.seek(std::io::SeekFrom::End(5)).unwrap();
    assert!(stream.read_chunk(10).await.unwrap().is_empty());
    let mut buf = [0u8; 10];
    stream.seek(std::io::SeekFrom::End(-10)).unwrap();
    assert_eq!(stream.read(&mut buf).await.unwrap(), 10);
    assert_eq!(buf, content[content.len() - 10..]);
    // Only what was read (plus readahead) was fetched.
    let stats = handle.stats().await.unwrap();
    assert!(stats.pieces_have < stats.piece_count, "{stats:?}");
    assert!(matches!(
        handle.open_file(7).await,
        Err(SessionError::NoSuchFile(7))
    ));
    drop(stream);
    leecher.shutdown().await;
    seed.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_resume_and_remove() {
    let (len_a, len_b) = (50_000, 40_000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let seed = seed(&encoded, &content, len_a).await;
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let mut alerts = session.subscribe();
    let m = metainfo(&encoded);
    let hash = m.info_hash();
    let handle = session
        .add_torrent(
            m.clone(),
            AddTorrentOptions::new(dir.path())
                .with_peers([seed.addr])
                .paused(true),
        )
        .unwrap();
    assert!(!handle.is_running());
    let list = session.list();
    assert_eq!(list.len(), 1);
    assert!(list[0].paused && !list[0].running && list[0].has_metadata);
    assert_eq!(list[0].name.as_deref(), Some("payload"));
    assert!(matches!(
        session.add_torrent(m.clone(), AddTorrentOptions::new(dir.path())),
        Err(SessionError::Duplicate(_))
    ));

    let handle = session.resume(hash).unwrap();
    assert!(handle.is_running());
    until_finished(&mut alerts, hash).await;
    session.pause(hash).await.unwrap();
    assert!(!handle.is_running());
    assert!(session.torrent(hash).is_none());
    assert!(session.list()[0].paused);
    session.pause(hash).await.unwrap();
    let handle = session.resume(hash).unwrap();
    assert_eq!(
        handle.stats().await.unwrap().total_length,
        (len_a + len_b) as u64
    );

    session.remove(hash, true).await.unwrap();
    assert!(session.list().is_empty());
    assert!(!handle.is_running());
    assert!(!dir.path().join("payload").exists(), "files deleted");
    assert!(matches!(
        session.remove(hash, false).await,
        Err(SessionError::Unknown(_))
    ));
    assert!(matches!(
        session.resume(hash),
        Err(SessionError::Unknown(_))
    ));
    session.shutdown().await;
    seed.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_is_restored_from_its_state_directory() {
    let (len_a, len_b) = (60_000, 30_000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let m = metainfo(&encoded);
    let hash = m.info_hash();
    let state = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_files(data.path(), &content, len_a);
    let magnet =
        Magnet::parse("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=later")
            .unwrap();
    let mut c = config();
    c.state_dir = Some(state.path().to_path_buf());
    c.resume_dir = Some(state.path().join("resume"));
    {
        let session = Session::new(c.clone()).unwrap();
        let mut alerts = session.subscribe();
        session
            .add_torrent(
                m,
                AddTorrentOptions::new(data.path())
                    .with_file_priorities([Priority::High, Priority::Low]),
            )
            .unwrap();
        session
            .add_magnet(&magnet, AddTorrentOptions::new(data.path()).paused(true))
            .unwrap();
        until_finished(&mut alerts, hash).await;
        session.shutdown().await;
    }
    // Garbage next to the real state is reported, not fatal.
    std::fs::write(state.path().join("junk.state"), b"not bencode").unwrap();

    let session = Session::new(c).unwrap();
    let mut alerts = session.subscribe();
    let report = session.restore().await.unwrap();
    assert_eq!(report.restored.len(), 2, "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    let list = session.list();
    let torrent = list.iter().find(|t| t.info_hash == hash).unwrap();
    assert!(torrent.running && !torrent.paused);
    assert_eq!(torrent.file_priorities, vec![Priority::High, Priority::Low]);
    assert_eq!(torrent.download_dir, data.path());
    let magnet_entry = list
        .iter()
        .find(|t| Some(t.info_hash) == magnet.info_hash)
        .unwrap();
    assert!(magnet_entry.paused && !magnet_entry.has_metadata);
    assert_eq!(magnet_entry.name.as_deref(), Some("later"));
    until(&mut alerts, hash, |a| *a == Alert::Finished).await;
    // Restoring twice adds nothing.
    let again = session.restore().await.unwrap();
    assert!(again.restored.is_empty());

    // Removing forgets the state files.
    session.remove(hash, false).await.unwrap();
    session.shutdown().await;
    assert!(
        !state
            .path()
            .join(format!("{}.state", hash.to_hex()))
            .exists()
    );
    assert!(data.path().join("payload/a.bin").exists(), "files kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_watch_folder_adds_torrents_and_magnets() {
    let (encoded, _) = make_torrent(10_000, 10_000);
    let hash = metainfo(&encoded).info_hash();
    let watch_dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let mut watch = WatchConfig::new(watch_dir.path(), data.path());
    watch.interval = Duration::from_millis(30);
    watch.paused = true;
    let handle = session.watch(watch);
    std::fs::write(watch_dir.path().join("one.torrent"), &encoded).unwrap();
    std::fs::write(
        watch_dir.path().join("two.MAGNET"),
        "\n  magnet:?xt=urn:btih:89abcdef0123456789abcdef0123456789abcdef \n",
    )
    .unwrap();
    std::fs::write(watch_dir.path().join("bad.torrent"), b"nope").unwrap();
    std::fs::write(watch_dir.path().join("notes.txt"), b"ignored").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !(watch_dir.path().join("one.torrent.added").exists()
            && watch_dir.path().join("two.MAGNET.added").exists()
            && watch_dir.path().join("bad.torrent.invalid").exists())
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("watch folder files handled");
    let list = session.list();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|t| t.paused));
    assert!(list.iter().any(|t| t.info_hash == hash));
    assert!(watch_dir.path().join("notes.txt").exists());
    // The same torrent again is "added" (already there), not invalid.
    std::fs::write(watch_dir.path().join("again.torrent"), &encoded).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !watch_dir.path().join("again.torrent.added").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("duplicate handled");
    handle.stop().await;
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_ban_refuses_the_address_everywhere() {
    let (len_a, len_b) = (40_000, 30_000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let seed = seed(&encoded, &content, len_a).await;
    let loopback = std::net::IpAddr::from([127, 0, 0, 1]);
    assert!(seed.session.ban_ip(loopback));
    assert!(!seed.session.ban_ip(loopback), "already banned");
    // The v4-mapped form is the same address.
    assert!(!seed.session.ban_ip("::ffff:127.0.0.1".parse().unwrap()));
    assert_eq!(seed.session.banned_ips(), vec![loopback]);

    let dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config()).unwrap();
    let handle = leecher
        .add_torrent(
            metainfo(&encoded),
            AddTorrentOptions::new(dir.path()).with_peers([seed.addr]),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let progress = handle.progress().await.unwrap();
    assert_eq!(progress.stats.bytes_have, 0, "the banned leecher got data");

    assert!(seed.session.unban_ip(loopback));
    assert!(seed.session.banned_ips().is_empty());
    leecher.shutdown().await;
    seed.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_connection_cap_holds_dials_until_raised() {
    let (len_a, len_b) = (40_000, 30_000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let seed = seed(&encoded, &content, len_a).await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = config();
    c.max_connections = Some(0);
    let leecher = Session::new(c).unwrap();
    assert_eq!(leecher.max_connections(), Some(0));
    let mut alerts = leecher.subscribe();
    let m = metainfo(&encoded);
    let hash = m.info_hash();
    let handle = leecher
        .add_torrent(
            m,
            AddTorrentOptions::new(dir.path()).with_peers([seed.addr]),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(leecher.connection_count(), 0);
    assert_eq!(handle.progress().await.unwrap().stats.bytes_have, 0);
    // Raised at runtime: the deferred dial goes out and the download ends.
    leecher.set_max_connections(Some(4));
    until_finished(&mut alerts, hash).await;
    assert!(leecher.connection_count() <= 4);
    leecher.shutdown().await;
    seed.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeding_reads_go_through_the_shared_cache() {
    let (len_a, len_b) = (60_000, 50_000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let seed = seed(&encoded, &content, len_a).await;
    for _ in 0..2 {
        let dir = tempfile::tempdir().unwrap();
        let leecher = Session::new(config()).unwrap();
        let mut alerts = leecher.subscribe();
        let m = metainfo(&encoded);
        let hash = m.info_hash();
        leecher
            .add_torrent(
                m,
                AddTorrentOptions::new(dir.path()).with_peers([seed.addr]),
            )
            .unwrap();
        until_finished(&mut alerts, hash).await;
        leecher.shutdown().await;
    }
    let stats = seed.session.read_cache_stats().unwrap();
    assert!(stats.hits > 0, "{stats:?}");
    assert!(stats.bytes <= stats.budget);
    seed.session.shutdown().await;

    let mut c = config();
    c.read_cache = None;
    assert!(Session::new(c).unwrap().read_cache_stats().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partial_download_is_checked_only_where_data_can_be() {
    let (len_a, len_b) = (100_000, 120_000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let dir = tempfile::tempdir().unwrap();
    // Only a.bin is on disk; b.bin was never created.
    std::fs::create_dir_all(dir.path().join("payload")).unwrap();
    std::fs::write(dir.path().join("payload/a.bin"), &content[..len_a]).unwrap();
    let session = Session::new(config()).unwrap();
    let mut alerts = session.subscribe();
    let m = metainfo(&encoded);
    let hash = m.info_hash();
    let handle = session
        .add_torrent(m, AddTorrentOptions::new(dir.path()))
        .unwrap();
    until(&mut alerts, hash, |a| {
        *a == Alert::StateChanged(TorrentStatus::Downloading)
    })
    .await;
    let stats = handle.progress().await.unwrap().stats;
    // The pieces wholly inside a.bin were hashed and found; the rest were
    // ruled out by the prescan.
    assert_eq!(stats.pieces_have as usize, len_a / PIECE_LEN);
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preallocated_durable_downloads_complete() {
    let (len_a, len_b) = (70_000, 90_000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let seed = seed(&encoded, &content, len_a).await;
    for (prealloc, durability) in [
        (PreallocateMode::Full, Durability::PerPiece),
        (PreallocateMode::Sparse, Durability::OnComplete),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut c = config();
        c.preallocate = prealloc;
        c.durability = durability;
        let leecher = Session::new(c).unwrap();
        let mut alerts = leecher.subscribe();
        let m = metainfo(&encoded);
        let hash = m.info_hash();
        leecher
            .add_torrent(
                m,
                AddTorrentOptions::new(dir.path()).with_peers([seed.addr]),
            )
            .unwrap();
        until_finished(&mut alerts, hash).await;
        leecher.shutdown().await;
        let a = std::fs::read(dir.path().join("payload/a.bin")).unwrap();
        let b = std::fs::read(dir.path().join("payload/b.bin")).unwrap();
        assert_eq!(a, &content[..len_a], "{prealloc:?}");
        assert_eq!(b, &content[len_a..], "{prealloc:?}");
    }
    seed.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_select_only_magnet_downloads_just_its_files() {
    let (len_a, len_b) = (100_000, 120_000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let seed = seed(&encoded, &content, len_a).await;
    let hash = metainfo(&encoded).info_hash();
    let dir = tempfile::tempdir().unwrap();
    let leecher = Session::new(config()).unwrap();
    let mut alerts = leecher.subscribe();
    let magnet = Magnet::parse(&format!("magnet:?xt=urn:btih:{hash}&so=1")).unwrap();
    let handle = leecher
        .add_magnet(
            &magnet,
            AddTorrentOptions::new(dir.path()).with_peers([seed.addr]),
        )
        .unwrap();
    until_finished(&mut alerts, hash).await;
    assert_eq!(
        std::fs::read(dir.path().join("payload/b.bin")).unwrap(),
        &content[len_a..]
    );
    let files = handle.progress().await.unwrap().files.unwrap();
    assert_eq!(files[0].priority, Priority::Skip);
    assert!(files[0].have < PIECE_LEN as u64, "{files:?}");
    assert_eq!(
        leecher.list()[0].file_priorities,
        vec![Priority::Skip, Priority::Normal]
    );
    leecher.shutdown().await;
    seed.session.shutdown().await;
}
