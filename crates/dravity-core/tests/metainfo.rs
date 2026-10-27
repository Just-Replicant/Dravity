//! Metainfo parsing tests. All fixtures are generated here, so the suite needs
//! no binary files and every expectation is derived from first principles.

#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::panic
)]

use std::borrow::Cow;
use std::path::PathBuf;

use dravity_bencode::{Dict, Value};
use dravity_core::layout::FileSlice;
use dravity_core::{InfoHash, Metainfo, MetainfoError, PathError};
use proptest::prelude::*;

fn b(s: &str) -> Value<'static> {
    Value::Bytes(Cow::Owned(s.as_bytes().to_vec()))
}

fn dict(entries: Vec<(&str, Value<'static>)>) -> Value<'static> {
    let mut d = Dict::new();
    for (k, v) in entries {
        d.insert(Cow::Owned(k.as_bytes().to_vec()), v);
    }
    Value::Dict(d)
}

fn list(items: Vec<Value<'static>>) -> Value<'static> {
    Value::List(items)
}

fn int(i: i64) -> Value<'static> {
    Value::Int(i)
}

fn pieces(n: u64) -> Value<'static> {
    let bytes: Vec<u8> = (0..n * 20).map(|i| (i % 251) as u8).collect();
    Value::Bytes(Cow::Owned(bytes))
}

fn file(path: &[&str], len: i64) -> Value<'static> {
    dict(vec![
        ("length", int(len)),
        ("path", list(path.iter().map(|p| b(p)).collect())),
    ])
}

fn single_info(name: &str, len: u64, piece_len: u64) -> Value<'static> {
    dict(vec![
        ("name", b(name)),
        ("length", int(len as i64)),
        ("piece length", int(piece_len as i64)),
        ("pieces", pieces(len.div_ceil(piece_len))),
    ])
}

fn multi_info(
    name: &str,
    files: Vec<Value<'static>>,
    total: u64,
    piece_len: u64,
) -> Value<'static> {
    dict(vec![
        ("name", b(name)),
        ("files", list(files)),
        ("piece length", int(piece_len as i64)),
        ("pieces", pieces(total.div_ceil(piece_len))),
    ])
}

fn torrent(info: Value<'static>) -> Vec<u8> {
    dict(vec![
        ("announce", b("http://tracker.example/announce")),
        ("info", info),
    ])
    .to_vec()
}

fn parse(info: Value<'static>) -> Result<Metainfo, MetainfoError> {
    Metainfo::from_bytes(&torrent(info))
}

fn with(info: Value<'static>, key: &str, value: Option<Value<'static>>) -> Value<'static> {
    let Value::Dict(mut d) = info else {
        unreachable!()
    };
    match value {
        Some(v) => d.insert(Cow::Owned(key.as_bytes().to_vec()), v),
        None => d.remove(key.as_bytes()),
    };
    Value::Dict(d)
}

#[test]
fn single_file_torrent() {
    let info = single_info("video.mkv", 100_000, 32_768);
    let m = parse(info.clone()).unwrap();
    assert_eq!(m.info_hash(), InfoHash::from_info_bytes(&info.to_vec()));
    assert_eq!(m.info_bytes(), info.to_vec());
    let i = m.info();
    assert_eq!(i.name(), "video.mkv");
    assert!(!i.is_multi_file());
    assert!(!i.is_private());
    assert_eq!(i.total_length(), 100_000);
    assert_eq!(i.piece_count(), 4);
    assert_eq!(i.layout().piece_size(3), Some(100_000 - 3 * 32_768));
    assert_eq!(i.files().len(), 1);
    assert_eq!(i.file_path(0), Some(PathBuf::from("video.mkv")));
    assert_eq!(i.piece_hash(0).unwrap()[1], 1);
    assert_eq!(i.piece_hash(4), None);
    assert_eq!(m.announce(), Some("http://tracker.example/announce"));
    assert_eq!(
        m.trackers(),
        [["http://tracker.example/announce".to_owned()]]
    );
}

#[test]
fn multi_file_torrent_with_metadata() {
    let files = vec![
        file(&["a.txt"], 10),
        dict(vec![
            ("attr", b("p")),
            ("length", int(6)),
            ("path", list(vec![b(".pad"), b("6")])),
        ]),
        file(&["sub", "b.bin"], 20),
        file(&["sub", "empty"], 0),
    ];
    let info = with(
        multi_info("ignored", files, 36, 16),
        "name.utf-8",
        Some(b("Album")),
    );
    let info = with(info, "private", Some(int(1)));
    let info = with(info, "source", Some(b("TRK")));
    let bytes = dict(vec![
        ("announce", b("http://a/1")),
        (
            "announce-list",
            list(vec![
                list(vec![b("http://a/1"), b("http://a/2")]),
                list(vec![b("udp://b/")]),
            ]),
        ),
        (
            "comment",
            Value::Bytes(Cow::Owned(b"bad \xff utf8".to_vec())),
        ),
        ("created by", b("dravity-tests")),
        ("creation date", int(1_700_000_000)),
        ("info", info),
    ])
    .to_vec();
    let m = Metainfo::from_bytes(&bytes).unwrap();
    let i = m.info();
    assert_eq!(i.name(), "Album");
    assert!(i.is_multi_file());
    assert!(i.is_private());
    assert_eq!(i.source(), Some("TRK"));
    assert_eq!(i.total_length(), 36);
    assert_eq!(i.piece_count(), 3);
    let offsets: Vec<u64> = i.files().iter().map(|f| f.offset()).collect();
    assert_eq!(offsets, [0, 10, 16, 36]);
    assert!(i.files()[1].is_padding());
    assert_eq!(
        i.file_path(2),
        Some(PathBuf::from("Album").join("sub").join("b.bin"))
    );
    let slices: Vec<FileSlice> = i.piece_file_slices(0).collect();
    assert_eq!(
        slices,
        [
            FileSlice {
                file_index: 0,
                file_offset: 0,
                length: 10
            },
            FileSlice {
                file_index: 1,
                file_offset: 0,
                length: 6
            },
        ]
    );
    assert_eq!(m.announce_list().len(), 2);
    assert_eq!(m.trackers()[0], ["http://a/1", "http://a/2"]);
    assert_eq!(m.comment(), Some("bad \u{fffd} utf8"));
    assert_eq!(m.created_by(), Some("dravity-tests"));
    assert_eq!(m.creation_date(), Some(1_700_000_000));
}

#[test]
fn info_hash_covers_original_bytes_not_reencoding() {
    // `name` before `length`: unsorted, so canonical re-encoding would differ.
    let mut info = b"d4:name1:a6:lengthi3e12:piece lengthi16384e6:pieces20:".to_vec();
    info.extend_from_slice(&[7u8; 20]);
    info.push(b'e');
    let mut t = b"d4:info".to_vec();
    t.extend_from_slice(&info);
    t.push(b'e');
    let m = Metainfo::from_bytes(&t).unwrap();
    assert_eq!(m.info_hash(), InfoHash::from_info_bytes(&info));
    let canonical = Value::decode(&info).unwrap().to_vec();
    assert_ne!(canonical, info);
    assert_ne!(m.info_hash(), InfoHash::from_info_bytes(&canonical));
}

#[test]
fn lenient_optional_fields() {
    let info = single_info("x", 1, 16384);
    let bytes = dict(vec![
        (
            "announce-list",
            list(vec![
                b("not-a-tier"),
                list(vec![int(5), b("http://ok/")]),
                list(vec![]),
            ]),
        ),
        ("creation date", b("yesterday")),
        ("info", info),
    ])
    .to_vec();
    let m = Metainfo::from_bytes(&bytes).unwrap();
    assert_eq!(m.announce_list(), [["http://ok/".to_owned()]]);
    assert_eq!(m.creation_date(), None);
    assert_eq!(m.announce(), None);
}

fn unsafe_path(r: Result<Metainfo, MetainfoError>) -> PathError {
    match r {
        Err(MetainfoError::UnsafePath { source, .. }) => source,
        other => panic!("expected UnsafePath, got {other:?}"),
    }
}

#[test]
fn rejects_path_traversal_and_absolute_paths() {
    let multi = |path: &[&str]| parse(multi_info("root", vec![file(path, 1)], 1, 16384));
    assert_eq!(
        unsafe_path(multi(&["..", "etc", "passwd"])),
        PathError::Traversal("..".into())
    );
    assert_eq!(
        unsafe_path(multi(&["ok", ".."])),
        PathError::Traversal("..".into())
    );
    assert_eq!(
        unsafe_path(multi(&["/etc/passwd"])),
        PathError::Absolute("/etc/passwd".into())
    );
    assert_eq!(
        unsafe_path(multi(&["C:\\Windows"])),
        PathError::Absolute("C:\\Windows".into())
    );
    assert_eq!(
        unsafe_path(multi(&["a/../../b"])),
        PathError::Separator("a/../../b".into())
    );
    assert_eq!(unsafe_path(multi(&["a", ""])), PathError::EmptyComponent);
    assert_eq!(unsafe_path(multi(&[])), PathError::Empty);
    assert_eq!(
        unsafe_path(multi(&["nul\0byte"])),
        PathError::ControlCharacter("nul\0byte".into())
    );
    assert_eq!(
        unsafe_path(parse(single_info("..", 1, 16384))),
        PathError::Traversal("..".into())
    );
    assert_eq!(
        unsafe_path(parse(single_info("/abs", 1, 16384))),
        PathError::Absolute("/abs".into())
    );
    // `path.utf-8` is validated too and takes precedence.
    let f = dict(vec![
        ("length", int(1)),
        ("path", list(vec![b("fine")])),
        ("path.utf-8", list(vec![b("..")])),
    ]);
    assert_eq!(
        unsafe_path(parse(multi_info("r", vec![f], 1, 16384))),
        PathError::Traversal("..".into())
    );
}

#[test]
fn rejects_duplicate_and_conflicting_paths() {
    let dup = parse(multi_info(
        "r",
        vec![file(&["a"], 1), file(&["a"], 1)],
        2,
        16384,
    ));
    assert_eq!(dup.unwrap_err(), MetainfoError::DuplicatePath("a".into()));
    let conflict = parse(multi_info(
        "r",
        vec![file(&["a"], 1), file(&["a", "b"], 1)],
        2,
        16384,
    ));
    assert_eq!(
        conflict.unwrap_err(),
        MetainfoError::PathConflict("a".into())
    );
    let conflict = parse(multi_info(
        "r",
        vec![file(&["a", "b"], 1), file(&["a"], 1)],
        2,
        16384,
    ));
    assert_eq!(
        conflict.unwrap_err(),
        MetainfoError::PathConflict("a".into())
    );
}

#[test]
fn rejects_structural_problems() {
    let base = single_info("x", 100, 16384);
    let cases: Vec<(Value<'static>, MetainfoError)> = vec![
        (
            with(base.clone(), "pieces", Some(b("short"))),
            MetainfoError::InvalidPiecesLength(5),
        ),
        (
            with(base.clone(), "pieces", Some(pieces(2))),
            MetainfoError::PieceCountMismatch {
                expected: 1,
                actual: 2,
            },
        ),
        (
            with(base.clone(), "piece length", Some(int(0))),
            MetainfoError::InvalidPieceLength(0),
        ),
        (
            with(base.clone(), "piece length", Some(int(1 << 33))),
            MetainfoError::InvalidPieceLength(1 << 33),
        ),
        (
            with(base.clone(), "files", Some(list(vec![file(&["a"], 1)]))),
            MetainfoError::AmbiguousMode,
        ),
        (
            with(base.clone(), "length", None),
            MetainfoError::MissingLength,
        ),
        (
            with(
                with(base.clone(), "length", None),
                "files",
                Some(list(vec![])),
            ),
            MetainfoError::NoFiles,
        ),
        (
            with(base.clone(), "pieces", None),
            MetainfoError::MissingPieces,
        ),
        (
            with(
                with(base.clone(), "pieces", None),
                "meta version",
                Some(int(2)),
            ),
            MetainfoError::UnsupportedV2Only,
        ),
        (
            with(base.clone(), "private", Some(int(2))),
            MetainfoError::InvalidPrivate(2),
        ),
        (
            with(base.clone(), "private", Some(int(-1))),
            MetainfoError::InvalidPrivate(-1),
        ),
    ];
    for (info, expected) in cases {
        assert_eq!(parse(info).unwrap_err(), expected);
    }
    // Type errors and malformed bencode surface as Bencode errors.
    assert!(matches!(
        parse(with(base.clone(), "length", Some(int(-1)))),
        Err(MetainfoError::Bencode(_))
    ));
    assert!(matches!(
        parse(with(base.clone(), "name", None)),
        Err(MetainfoError::Bencode(_))
    ));
    assert!(matches!(
        Metainfo::from_bytes(b"d4:infoi1ee"),
        Err(MetainfoError::InfoNotDict)
    ));
    assert!(matches!(
        Metainfo::from_bytes(b"de"),
        Err(MetainfoError::Bencode(_))
    ));
    let mut trailing = torrent(base);
    trailing.push(b'x');
    assert!(matches!(
        Metainfo::from_bytes(&trailing),
        Err(MetainfoError::Bencode(_))
    ));
}

#[test]
fn zero_length_torrent_has_no_pieces() {
    let m = parse(single_info("empty", 0, 16_384)).unwrap();
    assert_eq!(m.info().total_length(), 0);
    assert_eq!(m.info().piece_count(), 0);
    assert_eq!(m.info().piece_hash(0), None);
    assert_eq!(m.info().files()[0].length(), 0);
    let public = parse(with(single_info("a.txt", 1, 1), "private", Some(int(0)))).unwrap();
    assert!(!public.info().is_private());
}

#[test]
fn hybrid_torrent_parses_v1_part() {
    let info = with(single_info("h", 10, 16384), "meta version", Some(int(2)));
    let info = with(info, "file tree", Some(dict(vec![("h", dict(vec![]))])));
    assert_eq!(parse(info).unwrap().info().total_length(), 10);
}

proptest! {
    #[test]
    fn generated_multi_file_torrents_parse(
        lengths in proptest::collection::vec(0u64..100_000, 1..12),
        piece_shift in 14u32..20,
    ) {
        let total: u64 = lengths.iter().sum();
        prop_assume!(total > 0);
        let piece_len = 1u64 << piece_shift;
        let files = lengths
            .iter()
            .enumerate()
            .map(|(i, &l)| file(&["dir", &format!("f{i}")], l as i64))
            .collect();
        let info = multi_info("root", files, total, piece_len);
        let m = parse(info.clone()).unwrap();
        prop_assert_eq!(m.info_hash(), InfoHash::from_info_bytes(&info.to_vec()));
        prop_assert_eq!(m.info().total_length(), total);
        let covered: u64 = (0..m.info().piece_count())
            .flat_map(|p| m.info().piece_file_slices(p))
            .map(|s| s.length)
            .sum();
        prop_assert_eq!(covered, total);
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = Metainfo::from_bytes(&bytes);
    }

    #[test]
    fn mutated_torrents_never_panic(idx in any::<prop::sample::Index>(), byte in any::<u8>()) {
        let mut t = torrent(multi_info("r", vec![file(&["a"], 5), file(&["b", "c"], 40_000)], 40_005, 16384));
        let i = idx.index(t.len());
        t[i] = byte;
        let _ = Metainfo::from_bytes(&t);
    }
}
