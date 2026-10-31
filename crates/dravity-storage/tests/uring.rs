//! The io_uring backend (`--features uring`, Linux) against the same
//! storage rules as the blocking one. Every test skips (passes, with a note
//! on stderr) when this kernel or sandbox does not allow io_uring.

#![cfg(all(feature = "uring", target_os = "linux"))]
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::print_stderr
)]

use std::borrow::Cow;
use std::sync::Arc;

use bytes::Bytes;
use dravity_bencode::{Dict, Value};
use dravity_core::Metainfo;
use dravity_storage::{BlockingIo, FileIo, FileStorage, Storage, UringIo, verify_piece};
use sha1::{Digest, Sha1};

/// The io_uring backend, or `None` (after saying why) when unavailable.
fn uring() -> Option<UringIo> {
    match UringIo::start() {
        Ok(io) => Some(io),
        Err(e) => {
            eprintln!("skipping: io_uring is unavailable here: {e}");
            None
        }
    }
}

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

/// Three files (one empty) over 16 KiB pieces.
fn torrent() -> (Metainfo, Vec<u8>) {
    let files: [(&str, usize); 3] = [("a.bin", 40_000), ("empty", 0), ("b.bin", 70_001)];
    let content: Vec<u8> = (0..110_001usize).map(|i| (i * 31 % 251) as u8).collect();
    let pieces: Vec<u8> = content
        .chunks(16_384)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let list = files
        .iter()
        .map(|(name, len)| {
            dict(vec![
                ("length", Value::Int(*len as i64)),
                ("path", Value::List(vec![bytes(name.as_bytes())])),
            ])
        })
        .collect();
    let info = dict(vec![
        ("files", Value::List(list)),
        ("name", bytes(b"t")),
        ("piece length", Value::Int(16_384)),
        ("pieces", bytes(&pieces)),
    ]);
    let encoded = dict(vec![("info", info)]).to_vec();
    (Metainfo::from_bytes(&encoded).unwrap(), content)
}

#[test]
fn uring_storage_writes_reads_and_verifies() {
    let Some(io) = uring() else { return };
    assert_eq!(io.name(), "io_uring");
    let (m, content) = torrent();
    let dir = tempfile::tempdir().unwrap();
    let storage = FileStorage::new(dir.path(), m.info())
        .unwrap()
        .with_io(Arc::new(io));
    assert_eq!(storage.io().name(), "io_uring");
    // Ranges that cross file boundaries.
    for chunk in (0..content.len()).step_by(9_999) {
        let end = (chunk + 9_999).min(content.len());
        storage
            .write_at(chunk as u64, &content[chunk..end])
            .unwrap();
    }
    storage.flush().unwrap();
    for p in 0..m.info().piece_count() {
        assert!(verify_piece(&storage, m.info(), p).unwrap(), "piece {p}");
    }
    let mut back = vec![0; content.len()];
    storage.read_at(0, &mut back).unwrap();
    assert_eq!(back, content);
    // The same files read through the blocking backend.
    let blocking = FileStorage::new(dir.path(), m.info()).unwrap();
    let mut again = vec![0; content.len()];
    blocking.read_at(0, &mut again).unwrap();
    assert_eq!(again, content);
    assert_eq!(
        std::fs::read(dir.path().join("t/b.bin")).unwrap(),
        content[40_000..]
    );
}

#[test]
fn uring_reads_report_missing_data() {
    let Some(io) = uring() else { return };
    let (m, _) = torrent();
    let dir = tempfile::tempdir().unwrap();
    let storage = FileStorage::new(dir.path(), m.info())
        .unwrap()
        .with_io(Arc::new(io));
    // Never written: the file does not exist.
    let mut buf = vec![0; 100];
    assert!(storage.read_at(0, &mut buf).unwrap_err().is_missing_data());
    // Written short: reading past its end fails instead of returning zeros.
    storage.write_at(0, &[1; 10]).unwrap();
    assert!(storage.read_at(0, &mut buf).is_err());
}

#[test]
fn uring_serves_many_threads_at_once() {
    let Some(io) = uring() else { return };
    let (m, content) = torrent();
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(
        FileStorage::new(dir.path(), m.info())
            .unwrap()
            .with_io(Arc::new(io)),
    );
    let content = Arc::new(content);
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let storage = Arc::clone(&storage);
            let content = Arc::clone(&content);
            std::thread::spawn(move || {
                for start in (t * 1000..content.len()).step_by(8 * 1000) {
                    let end = (start + 1000).min(content.len());
                    storage
                        .write_at(start as u64, &content[start..end])
                        .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let mut back = vec![0; content.len()];
    storage.read_at(0, &mut back).unwrap();
    assert_eq!(back, *content);
}

#[test]
fn an_unusable_ring_is_an_error_not_a_panic() {
    // Zero entries is refused by every kernel (EINVAL), or io_uring is
    // missing altogether: either way the caller gets an error to fall back
    // on.
    assert!(UringIo::start_with(0).is_err());
    let fallback: Arc<dyn FileIo> = match UringIo::start_with(0) {
        Ok(io) => Arc::new(io),
        Err(_) => Arc::new(BlockingIo),
    };
    assert_eq!(fallback.name(), "blocking");
}

#[test]
fn uring_flushes_through_the_ring() {
    let Some(io) = uring() else { return };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let file = io.adopt(file).unwrap();
    file.write_all_at(Bytes::from_static(b"durable"), 3)
        .unwrap();
    file.sync_data().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"\0\0\0durable");
    // Reads append to the buffer they are given and hand it back.
    let back = file.read_exact_at(b"x".to_vec(), 4, 5).unwrap();
    assert_eq!(back, b"xrabl");
    let mut slice = [0; 3];
    file.read_slice_at(&mut slice, 0).unwrap();
    assert_eq!(slice, [0; 3]);
    // A read-only descriptor still flushes, as with `File::sync_data`.
    let read_only = io.adopt(std::fs::File::open(&path).unwrap()).unwrap();
    read_only.sync_data().unwrap();
    let blocking = BlockingIo
        .adopt(std::fs::File::open(&path).unwrap())
        .unwrap();
    blocking.sync_data().unwrap();
}

#[test]
fn uring_closes_files_when_the_cache_drops_them() {
    let Some(io) = uring() else { return };
    let (m, content) = torrent();
    let dir = tempfile::tempdir().unwrap();
    // One open file at a time: every switch between the two files evicts
    // (flushes and closes) the other on the ring.
    let storage = FileStorage::new(dir.path(), m.info())
        .unwrap()
        .with_io(Arc::new(io))
        .with_max_open_files(1);
    let fds = || std::fs::read_dir("/proc/self/fd").map(Iterator::count).ok();
    let before = fds();
    for _ in 0..50 {
        storage
            .write_bytes(0, Bytes::copy_from_slice(&content))
            .unwrap();
        assert_eq!(storage.open_files(), 1);
    }
    assert_eq!(storage.read_vec(0, content.len()).unwrap(), content);
    drop(storage);
    // Closing happens on the ring thread right after the drop.
    let mut after = fds();
    for _ in 0..100 {
        if after <= before {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        after = fds();
    }
    assert!(
        after <= before,
        "descriptors leaked: {before:?} -> {after:?}"
    );
}

#[test]
fn owned_buffers_round_trip_across_files() {
    let Some(io) = uring() else { return };
    let (m, content) = torrent();
    let dir = tempfile::tempdir().unwrap();
    let storage = FileStorage::new(dir.path(), m.info())
        .unwrap()
        .with_io(Arc::new(io));
    let data = Bytes::copy_from_slice(&content);
    for chunk in (0..content.len()).step_by(7_777) {
        let end = (chunk + 7_777).min(content.len());
        storage
            .write_bytes(chunk as u64, data.slice(chunk..end))
            .unwrap();
    }
    for chunk in (0..content.len()).step_by(9_999) {
        let end = (chunk + 9_999).min(content.len());
        assert_eq!(
            storage.read_vec(chunk as u64, end - chunk).unwrap(),
            content[chunk..end]
        );
    }
}
