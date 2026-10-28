//! Filesystem and memory backends against generated torrents.

#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing
)]

use std::borrow::Cow;

use dravity_bencode::{Dict, Value};
use dravity_core::{BLOCK_SIZE, BlockRef, Metainfo};
use dravity_storage::{
    FileStamp, FileStorage, MemoryStorage, ResumeData, Storage, StorageError, load_resume,
    read_block, remove_resume, save_resume, verify_piece, write_block,
};
use sha1::{Digest, Sha1};

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

/// A torrent over `files` (path components, contents, padding flag).
fn torrent(files: &[(&[&str], Vec<u8>, bool)], piece_len: usize) -> (Metainfo, Vec<u8>) {
    let content: Vec<u8> = files.iter().flat_map(|f| f.1.clone()).collect();
    let pieces: Vec<u8> = content
        .chunks(piece_len)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let file_list = files
        .iter()
        .map(|(path, data, pad)| {
            let mut e = vec![
                ("length", Value::Int(data.len() as i64)),
                (
                    "path",
                    Value::List(path.iter().map(|p| bytes(p.as_bytes())).collect()),
                ),
            ];
            if *pad {
                e.push(("attr", bytes(b"p")));
            }
            dict(e)
        })
        .collect();
    let info = dict(vec![
        ("name", bytes(b"root")),
        ("files", Value::List(file_list)),
        ("piece length", Value::Int(piece_len as i64)),
        ("pieces", bytes(&pieces)),
    ]);
    let bytes = dict(vec![("info", info)]).to_vec();
    (Metainfo::from_bytes(&bytes).unwrap(), content)
}

fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn write_all_blocks(storage: &dyn Storage, m: &Metainfo, content: &[u8]) {
    let layout = m.info().layout();
    for piece in 0..layout.piece_count() {
        for b in layout.blocks(piece) {
            let start = (layout.piece_offset(piece).unwrap() + u64::from(b.offset)) as usize;
            write_block(
                storage,
                layout,
                b,
                &content[start..start + b.length as usize],
            )
            .unwrap();
        }
    }
}

#[test]
fn file_storage_spans_files_and_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let (m, data) = torrent(
        &[
            (&["a.bin"], content(20_000, 1), false),
            (&["sub", "b.bin"], content(3, 2), false),
            (&["empty"], Vec::new(), false),
            (&["sub", "deeper", "c.bin"], content(40_000, 3), false),
        ],
        2 * BLOCK_SIZE as usize,
    );
    let s = FileStorage::new(dir.path(), m.info()).unwrap();
    assert_eq!(s.len(), data.len() as u64);
    // Nothing verifies before writing, and nothing is created by checking.
    assert!(!verify_piece(&s, m.info(), 0).unwrap());
    assert!(!dir.path().join("root").exists());
    write_all_blocks(&s, &m, &data);
    s.flush().unwrap();
    for piece in 0..m.info().piece_count() {
        assert!(verify_piece(&s, m.info(), piece).unwrap(), "piece {piece}");
    }
    assert_eq!(
        std::fs::read(dir.path().join("root/a.bin")).unwrap(),
        data[..20_000]
    );
    assert_eq!(
        std::fs::read(dir.path().join("root/sub/b.bin")).unwrap(),
        data[20_000..20_003]
    );
    assert_eq!(
        std::fs::read(dir.path().join("root/sub/deeper/c.bin")).unwrap(),
        data[20_003..]
    );
    // Reads span files too.
    let mut buf = vec![0; 100];
    s.read_at(19_950, &mut buf).unwrap();
    assert_eq!(buf, data[19_950..20_050]);
}

#[test]
fn read_block_returns_written_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let (m, data) = torrent(
        &[
            (&["a"], content(BLOCK_SIZE as usize + 100, 4), false),
            (&["b"], content(BLOCK_SIZE as usize, 5), false),
        ],
        2 * BLOCK_SIZE as usize,
    );
    let s = FileStorage::new(dir.path(), m.info()).unwrap();
    let layout = m.info().layout();
    let block = BlockRef {
        piece: 0,
        offset: BLOCK_SIZE,
        length: BLOCK_SIZE,
    };
    // Never written: missing data, not a hard error.
    assert!(read_block(&s, layout, block).unwrap_err().is_missing_data());
    write_all_blocks(&s, &m, &data);
    // Spans the boundary between `a` and `b`.
    let got = read_block(&s, layout, block).unwrap();
    assert_eq!(got, data[BLOCK_SIZE as usize..2 * BLOCK_SIZE as usize]);
    let last = layout.blocks(1).last().unwrap();
    let got = read_block(&s, layout, last).unwrap();
    assert_eq!(got.len(), last.length as usize);
    // Unaligned or oversized requests are refused.
    for bad in [
        BlockRef {
            piece: 0,
            offset: 1,
            length: BLOCK_SIZE + 1,
        },
        BlockRef {
            piece: 9,
            offset: 0,
            length: 1,
        },
    ] {
        assert!(matches!(
            read_block(&s, layout, bad),
            Err(StorageError::InvalidBlock(_))
        ));
    }
}

#[test]
fn detects_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let (m, data) = torrent(&[(&["x"], content(50_000, 9), false)], BLOCK_SIZE as usize);
    let s = FileStorage::new(dir.path(), m.info()).unwrap();
    write_all_blocks(&s, &m, &data);
    s.write_at(BLOCK_SIZE as u64 + 5, b"!").unwrap();
    assert!(verify_piece(&s, m.info(), 0).unwrap());
    assert!(!verify_piece(&s, m.info(), 1).unwrap());
    assert!(verify_piece(&s, m.info(), 2).unwrap());
}

#[test]
fn padding_files_are_never_created() {
    let dir = tempfile::tempdir().unwrap();
    let (m, data) = torrent(
        &[
            (&["a"], content(1000, 1), false),
            (&[".pad", "1000"], vec![0; 1000], true),
            (&["b"], content(500, 2), false),
        ],
        1024,
    );
    let s = FileStorage::new(dir.path(), m.info()).unwrap();
    write_all_blocks(&s, &m, &data);
    for piece in 0..m.info().piece_count() {
        assert!(verify_piece(&s, m.info(), piece).unwrap());
    }
    assert!(!dir.path().join("root/.pad").exists());
}

#[test]
fn fd_cache_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..10).map(|i| format!("f{i}")).collect();
    let specs: Vec<(Vec<&str>, Vec<u8>)> = names
        .iter()
        .map(|n| (vec![n.as_str()], content(100, 4)))
        .collect();
    let files: Vec<(&[&str], Vec<u8>, bool)> = specs
        .iter()
        .map(|(p, d)| (p.as_slice(), d.clone(), false))
        .collect();
    let (m, data) = torrent(&files, 256);
    let s = FileStorage::new(dir.path(), m.info())
        .unwrap()
        .with_max_open_files(3);
    write_all_blocks(&s, &m, &data);
    assert!(s.open_files() <= 3);
    // Evicted files are synced too; flush must not skip them.
    s.flush().unwrap();
    for piece in 0..m.info().piece_count() {
        assert!(verify_piece(&s, m.info(), piece).unwrap());
    }
}

#[cfg(unix)]
#[test]
fn refuses_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("root")).unwrap();
    let (m, data) = torrent(&[(&["a"], content(10, 1), false)], 16);
    let s = FileStorage::new(dir.path(), m.info()).unwrap();
    let err = s.write_at(0, &data).unwrap_err();
    assert!(matches!(err, StorageError::Symlink(_)), "{err}");
    assert!(!elsewhere.path().join("a").exists());
}

#[test]
fn rejects_out_of_range_and_bad_blocks() {
    let s = MemoryStorage::new(10);
    assert!(matches!(
        s.write_at(8, b"abc"),
        Err(StorageError::OutOfBounds { .. })
    ));
    assert!(matches!(
        s.read_at(u64::MAX, &mut [0]),
        Err(StorageError::OutOfBounds { .. })
    ));
    let (m, _) = torrent(&[(&["a"], content(10, 1), false)], 16);
    let layout = m.info().layout();
    let block = BlockRef {
        piece: 0,
        offset: 0,
        length: 10,
    };
    assert!(matches!(
        write_block(&s, layout, block, b"short"),
        Err(StorageError::InvalidBlock(_))
    ));
    assert!(matches!(
        write_block(&s, layout, BlockRef { piece: 1, ..block }, &[0; 10]),
        Err(StorageError::InvalidBlock(_))
    ));
    assert!(verify_piece(&s, m.info(), 5).is_err());
}

#[test]
fn memory_storage_verifies() {
    let (m, data) = torrent(&[(&["a"], content(70_000, 5), false)], 32_768);
    let s = MemoryStorage::new(m.info().total_length());
    write_all_blocks(&s, &m, &data);
    assert_eq!(s.to_vec(), data);
    for piece in 0..m.info().piece_count() {
        assert!(verify_piece(&s, m.info(), piece).unwrap());
    }
}

#[test]
fn stamps_vouch_for_resume_data() {
    let dir = tempfile::tempdir().unwrap();
    let (m, data) = torrent(
        &[
            (&["a.bin"], content(20_000, 1), false),
            (&["pad"], vec![0; 100], true),
            (&["b.bin"], content(5_000, 2), false),
        ],
        2 * BLOCK_SIZE as usize,
    );
    let s = FileStorage::new(dir.path(), m.info()).unwrap();
    assert_eq!(
        s.stamps().unwrap(),
        Some(vec![FileStamp::Missing; 3]),
        "nothing written yet"
    );
    write_all_blocks(&s, &m, &data);
    s.flush().unwrap();
    let stamps = s.stamps().unwrap().unwrap();
    assert!(matches!(stamps[0], FileStamp::Present { len: 20_000, .. }));
    assert_eq!(stamps[1], FileStamp::Missing, "padding is never created");

    let path = dir.path().join("state/x.resume");
    assert_eq!(load_resume(&path).unwrap(), None);
    let resume = ResumeData {
        info_hash: m.info_hash(),
        piece_count: m.info().piece_count(),
        have: vec![0b1000_0000],
        files: stamps.clone(),
    };
    save_resume(&path, &resume).unwrap();
    let loaded = load_resume(&path).unwrap().unwrap();
    assert_eq!(loaded, resume);
    assert!(!dir.path().join("state/x.resume.tmp").exists());
    assert!(loaded.is_valid_for(
        &m.info_hash(),
        m.info().piece_count(),
        &s.stamps().unwrap().unwrap()
    ));

    // Touching a file invalidates the stamps.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("root/b.bin"))
        .unwrap();
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
        .unwrap();
    assert!(!loaded.is_valid_for(
        &m.info_hash(),
        m.info().piece_count(),
        &s.stamps().unwrap().unwrap()
    ));

    std::fs::write(&path, b"garbage").unwrap();
    assert!(load_resume(&path).is_err());
    remove_resume(&path).unwrap();
    remove_resume(&path).unwrap();
    assert_eq!(load_resume(&path).unwrap(), None);
    assert_eq!(MemoryStorage::new(1).stamps().unwrap(), None);
}
