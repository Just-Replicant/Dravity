//! BEP 52 (v2 and hybrid) metainfo tests. Fixtures are built here from file
//! contents, so every hash is derived from first principles.

#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::borrow::Cow;

use dravity_bencode::{Dict, Value};
use dravity_core::merkle::{self, Hash};
use dravity_core::{InfoHash, InfoHashV2, MetaVersion, Metainfo, MetainfoError};
use proptest::prelude::*;
use sha1::{Digest, Sha1};

fn bstr(b: &[u8]) -> Value<'static> {
    Value::Bytes(Cow::Owned(b.to_vec()))
}

fn dict(entries: Vec<(&[u8], Value<'static>)>) -> Value<'static> {
    let mut d = Dict::new();
    for (k, v) in entries {
        d.insert(Cow::Owned(k.to_vec()), v);
    }
    Value::Dict(d)
}

fn insert(v: &mut Value<'static>, key: &[u8], value: Option<Value<'static>>) {
    let Value::Dict(d) = v else {
        panic!("not a dict")
    };
    match value {
        Some(value) => d.insert(Cow::Owned(key.to_vec()), value),
        None => d.remove(key),
    };
}

/// One file of a fixture: path components and content.
struct F(Vec<&'static str>, Vec<u8>);

fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i * 31) as u8) ^ seed).collect()
}

/// Builds `(torrent bytes, content stream)`. `hybrid` adds v1 `files` with
/// BEP 47 padding and SHA-1 `pieces`. Files are sorted by path, as a file
/// tree orders them.
fn build(name: &str, mut files: Vec<F>, piece: u32, hybrid: bool) -> (Vec<u8>, Vec<u8>) {
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let single = files.len() == 1 && files[0].0 == [name];
    let mut tree = dict(vec![]);
    let mut layers = Dict::new();
    for F(path, data) in &files {
        let mut leaf = vec![(&b"length"[..], Value::Int(data.len() as i64))];
        if let Some(h) = merkle::file_hashes(data, piece) {
            leaf.push((&b"pieces root"[..], bstr(&h.root)));
            if !h.layer.is_empty() {
                layers.insert(Cow::Owned(h.root.to_vec()), bstr(&h.layer.concat()));
            }
        }
        let mut node = &mut tree;
        for c in path {
            let Value::Dict(d) = node else { unreachable!() };
            node = d
                .entry(Cow::Owned(c.as_bytes().to_vec()))
                .or_insert_with(|| dict(vec![]));
        }
        insert(node, b"", Some(dict(leaf)));
    }
    let mut stream = Vec::new();
    let mut v1_files = Vec::new();
    let last_data = files.iter().rposition(|f| !f.1.is_empty());
    for (i, F(path, data)) in files.iter().enumerate() {
        stream.extend_from_slice(data);
        v1_files.push(dict(vec![
            (b"length", Value::Int(data.len() as i64)),
            (
                b"path",
                Value::List(path.iter().map(|c| bstr(c.as_bytes())).collect()),
            ),
        ]));
        let rem = data.len() % piece as usize;
        if rem != 0 && Some(i) != last_data {
            let pad = piece as usize - rem;
            stream.resize(stream.len() + pad, 0);
            v1_files.push(dict(vec![
                (b"attr", bstr(b"p")),
                (b"length", Value::Int(pad as i64)),
                (
                    b"path",
                    Value::List(vec![bstr(b".pad"), bstr(pad.to_string().as_bytes())]),
                ),
            ]));
        }
    }
    let mut info = dict(vec![
        (b"file tree", tree),
        (b"meta version", Value::Int(2)),
        (b"name", bstr(name.as_bytes())),
        (b"piece length", Value::Int(i64::from(piece))),
    ]);
    if hybrid {
        let pieces: Vec<u8> = stream
            .chunks(piece as usize)
            .flat_map(|c| Sha1::digest(c).to_vec())
            .collect();
        insert(&mut info, b"pieces", Some(bstr(&pieces)));
        if single {
            insert(&mut info, b"length", Some(Value::Int(stream.len() as i64)));
        } else {
            insert(&mut info, b"files", Some(Value::List(v1_files)));
        }
    }
    let torrent = dict(vec![
        (b"announce", bstr(b"http://tracker.example/announce")),
        (b"info", info),
        (b"piece layers", Value::Dict(layers)),
    ]);
    (torrent.to_vec(), stream)
}

fn info_bytes(m: &Metainfo) -> Vec<u8> {
    m.info_bytes().to_vec()
}

fn every_piece_checks(m: &Metainfo, stream: &[u8]) -> bool {
    let info = m.info();
    (0..info.piece_count()).all(|p| {
        let r = info.layout().piece_range(p).unwrap();
        info.check_piece(p, &stream[r.start as usize..r.end as usize])
    })
}

fn sample() -> Vec<F> {
    vec![
        F(vec!["b", "big.bin"], content(100_000, 1)),
        F(vec!["a.txt"], content(5_000, 2)),
        F(vec!["c", "empty"], Vec::new()),
        F(vec!["c", "z.bin"], content(32_768, 3)),
    ]
}

#[test]
fn v2_only_single_file() {
    let data = content(70_000, 9);
    let (bytes, stream) = build(
        "movie.mkv",
        vec![F(vec!["movie.mkv"], data.clone())],
        32_768,
        false,
    );
    let m = Metainfo::from_bytes(&bytes).unwrap();
    let info = m.info();
    assert_eq!(info.version(), MetaVersion::V2);
    assert!(!info.is_multi_file());
    assert_eq!(info.piece_count(), 3);
    assert_eq!(info.total_length(), 70_000);
    assert!(info.piece_hashes().is_empty());
    let v2 = InfoHashV2::from_info_bytes(&info_bytes(&m));
    assert_eq!(m.info_hash_v2(), Some(v2));
    assert_eq!(m.info_hash_v1(), None);
    assert_eq!(m.info_hash(), v2.truncated());
    assert_eq!(m.swarm_hashes(), [v2.truncated()]);
    assert_eq!(
        info.pieces_root(0),
        Some(&merkle::file_hashes(&data, 32_768).unwrap().root)
    );
    assert!(info.missing_piece_layers().is_empty());
    assert!(every_piece_checks(&m, &stream));
    let mut bad = stream[..32_768].to_vec();
    bad[100] ^= 1;
    assert!(!info.check_piece(0, &bad));
    assert!(!info.check_piece(0, &stream[..100]));
    assert!(!info.check_piece(3, &[]));
}

#[test]
fn v2_only_files_are_padded_to_piece_boundaries() {
    let (bytes, stream) = build("set", sample(), 32_768, false);
    let m = Metainfo::from_bytes(&bytes).unwrap();
    let info = m.info();
    assert!(info.is_multi_file());
    let names: Vec<String> = info.files().iter().map(|f| f.path().to_string()).collect();
    assert_eq!(
        names,
        [
            "a.txt",
            ".pad/27768",
            "b/big.bin",
            ".pad/31072",
            "c/empty",
            "c/z.bin"
        ]
    );
    assert!(info.files()[1].is_padding() && info.files()[3].is_padding());
    for f in info
        .files()
        .iter()
        .filter(|f| !f.is_padding() && f.length() > 0)
    {
        assert_eq!(f.offset() % 32_768, 0, "{}", f.path());
    }
    // 1 + 4 + 1 pieces.
    assert_eq!(info.piece_count(), 6);
    assert_eq!(info.total_length(), stream.len() as u64);
    assert_eq!(info.pieces_root(1), None);
    assert_eq!(info.pieces_root(4), None);
    assert!(info.pieces_root(5).is_some());
    assert_eq!(info.piece_layers().len(), 1);
    assert!(every_piece_checks(&m, &stream));
}

#[test]
fn hybrid_torrents_check_both_hashes() {
    let (bytes, stream) = build("set", sample(), 32_768, true);
    let m = Metainfo::from_bytes(&bytes).unwrap();
    let info = m.info();
    assert_eq!(info.version(), MetaVersion::Hybrid);
    let raw = info_bytes(&m);
    assert_eq!(m.info_hash(), InfoHash::from_info_bytes(&raw));
    assert_eq!(m.info_hash_v1(), Some(InfoHash::from_info_bytes(&raw)));
    let v2 = InfoHashV2::from_info_bytes(&raw);
    assert_eq!(
        m.swarm_hashes(),
        [InfoHash::from_info_bytes(&raw), v2.truncated()]
    );
    assert_eq!(info.piece_hashes().len(), 6);
    assert!(every_piece_checks(&m, &stream));
    // Garbage in a padding file passes v2 (padding is not hashed there) but
    // not v1.
    let r = info.layout().piece_range(0).unwrap();
    let mut piece = stream[r.start as usize..r.end as usize].to_vec();
    *piece.last_mut().unwrap() = 1;
    assert!(!info.check_piece(0, &piece));

    let (single, stream) = build(
        "one",
        vec![F(vec!["one"], content(50_000, 4))],
        16_384,
        true,
    );
    let m = Metainfo::from_bytes(&single).unwrap();
    assert_eq!(m.info().version(), MetaVersion::Hybrid);
    assert!(!m.info().is_multi_file());
    assert!(every_piece_checks(&m, &stream));
}

#[test]
fn a_hybrid_piece_must_pass_merkle_as_well_as_sha1() {
    // v1 piece hashes of `honest` with the v2 tree of `other`: data that
    // matches SHA-1 but not the Merkle tree (a v1 collision, or a peer
    // serving a different v2 file) is rejected.
    let file = |seed| vec![F(vec!["one"], content(50_000, seed))];
    let (honest, stream) = build("one", file(4), 16_384, true);
    let (other, _) = build("one", file(5), 16_384, true);
    let honest = Value::decode(&honest).unwrap().into_owned();
    let mut forged = Value::decode(&other).unwrap().into_owned();
    let pieces = honest.get(b"info").unwrap().get(b"pieces").unwrap().clone();
    let mut info = forged.get(b"info").unwrap().clone();
    insert(&mut info, b"pieces", Some(pieces));
    insert(&mut forged, b"info", Some(info));
    let m = Metainfo::from_bytes(&forged.to_vec()).unwrap();
    let info = m.info();
    assert_eq!(info.version(), MetaVersion::Hybrid);
    assert!(info.missing_piece_layers().is_empty());
    for i in 0..info.piece_count() {
        let r = info.layout().piece_range(i).unwrap();
        let piece = &stream[r.start as usize..r.end as usize];
        let sha1: [u8; 20] = Sha1::digest(piece).into();
        assert_eq!(info.piece_hash(i), Some(&sha1), "piece {i} passes SHA-1");
        assert!(!info.check_piece(i, piece), "piece {i} fails Merkle");
    }
}

#[test]
fn hybrid_parts_must_agree() {
    let (bytes, _) = build("set", sample(), 32_768, true);
    let t = Value::decode(&bytes).unwrap().into_owned();
    let info = t.get(b"info").unwrap();
    // Rename a v1 file: the tree no longer matches.
    let mut renamed = info.clone();
    let Value::Dict(d) = &mut renamed else {
        unreachable!()
    };
    let Some(Value::List(files)) = d.get_mut(&b"files"[..]) else {
        unreachable!()
    };
    insert(
        &mut files[0],
        b"path",
        Some(Value::List(vec![bstr(b"other.txt")])),
    );
    let mut t2 = t.clone();
    insert(&mut t2, b"info", Some(renamed));
    assert!(matches!(
        Metainfo::from_bytes(&t2.to_vec()),
        Err(MetainfoError::HybridMismatch(_))
    ));
    // Drop the padding: files are no longer piece-aligned.
    let mut unpadded = info.clone();
    let Value::Dict(d) = &mut unpadded else {
        unreachable!()
    };
    let Some(Value::List(files)) = d.get_mut(&b"files"[..]) else {
        unreachable!()
    };
    files.retain(|f| f.get(b"attr").is_none());
    let total: usize = 5_000 + 100_000 + 32_768;
    let pieces: Vec<u8> = vec![0; total.div_ceil(32_768) * 20];
    insert(&mut unpadded, b"pieces", Some(bstr(&pieces)));
    let mut t3 = t.clone();
    insert(&mut t3, b"info", Some(unpadded));
    assert!(matches!(
        Metainfo::from_bytes(&t3.to_vec()),
        Err(MetainfoError::HybridMismatch(_))
    ));
}

#[test]
fn piece_layers_are_required_and_verified() {
    let (bytes, _) = build("set", sample(), 32_768, false);
    let mut t = Value::decode(&bytes).unwrap().into_owned();
    let mut missing = t.clone();
    insert(&mut missing, b"piece layers", None);
    assert!(matches!(
        Metainfo::from_bytes(&missing.to_vec()),
        Err(MetainfoError::MissingPieceLayer(_))
    ));
    let Some(Value::Dict(layers)) = t.get(b"piece layers").cloned() else {
        unreachable!()
    };
    let (root, layer) = layers.into_iter().next().unwrap();
    let mut layer = layer.as_bytes().unwrap().to_vec();
    layer[0] ^= 1;
    insert(
        &mut t,
        b"piece layers",
        Some(dict(vec![(&root, bstr(&layer))])),
    );
    assert!(matches!(
        Metainfo::from_bytes(&t.to_vec()),
        Err(MetainfoError::InvalidPieceLayer(_))
    ));
    layer.pop();
    insert(
        &mut t,
        b"piece layers",
        Some(dict(vec![(&root, bstr(&layer))])),
    );
    assert!(matches!(
        Metainfo::from_bytes(&t.to_vec()),
        Err(MetainfoError::InvalidPieceLayer(_))
    ));
}

#[test]
fn a_v2_magnet_needs_its_piece_layers() {
    let (bytes, stream) = build("set", sample(), 32_768, false);
    let full = Metainfo::from_bytes(&bytes).unwrap();
    let mut bare = Metainfo::from_info_bytes(full.info_bytes(), Vec::new()).unwrap();
    assert_eq!(bare.info_hash(), full.info_hash());
    let missing = bare.info().missing_piece_layers();
    assert_eq!(missing.len(), 1);
    assert_eq!(bare.info().piece_layer_len(&missing[0]), Some(4));
    let pieces = bare.info().pieces_with_root(&missing[0]);
    assert_eq!(pieces.len(), 1);
    assert_eq!(pieces[0].end - pieces[0].start, 4);
    assert!(bare.info().pieces_with_root(&[7; 32]).is_empty());
    assert!(!every_piece_checks(&bare, &stream));
    // Files of one piece verify with their root alone.
    let r = bare.info().layout().piece_range(0).unwrap();
    assert!(
        bare.info()
            .check_piece(0, &stream[r.start as usize..r.end as usize])
    );

    let (root, layer) = full.info().piece_layers()[0];
    let layer: Vec<Hash> = layer.to_vec();
    let mut wrong = layer.clone();
    wrong[2][0] ^= 1;
    assert!(bare.set_piece_layer(&root, wrong).is_err());
    assert!(bare.set_piece_layer(&[7; 32], layer.clone()).is_err());
    bare.set_piece_layer(&root, layer).unwrap();
    assert!(bare.info().missing_piece_layers().is_empty());
    assert!(every_piece_checks(&bare, &stream));
    assert_eq!(bare.info(), full.info());

    // A hybrid magnet verifies with SHA-1 while layers are missing.
    let (bytes, stream) = build("set", sample(), 32_768, true);
    let full = Metainfo::from_bytes(&bytes).unwrap();
    let bare = Metainfo::from_info_bytes(full.info_bytes(), Vec::new()).unwrap();
    assert_eq!(bare.info().missing_piece_layers().len(), 1);
    assert!(every_piece_checks(&bare, &stream));
}

#[test]
fn hostile_file_trees_are_rejected() {
    let leaf = |len: i64| dict(vec![(b"", dict(vec![(b"length", Value::Int(len))]))]);
    let root = [5u8; 32];
    let file = dict(vec![(
        b"",
        dict(vec![
            (b"length", Value::Int(10)),
            (b"pieces root", bstr(&root)),
        ]),
    )]);
    let info = |tree: Value<'static>, piece: i64| {
        dict(vec![
            (b"file tree", tree),
            (b"meta version", Value::Int(2)),
            (b"name", bstr(b"t")),
            (b"piece length", Value::Int(piece)),
        ])
    };
    let parse = |i: Value<'static>| Metainfo::from_info_bytes(&i.to_vec(), Vec::new());
    assert!(parse(info(dict(vec![(b"ok", file.clone())]), 16_384)).is_ok());
    assert!(matches!(
        parse(info(dict(vec![(b"..", file.clone())]), 16_384)),
        Err(MetainfoError::UnsafePath { .. })
    ));
    let mut both = file.clone();
    insert(&mut both, b"x", Some(file.clone()));
    assert!(matches!(
        parse(info(dict(vec![(b"d", both)]), 16_384)),
        Err(MetainfoError::PathConflict(_))
    ));
    assert!(matches!(
        parse(info(file.clone(), 16_384)),
        Err(MetainfoError::PathConflict(_))
    ));
    assert!(matches!(
        parse(info(dict(vec![(b"neg", leaf(-1))]), 16_384)),
        Err(MetainfoError::InvalidFileTree(_))
    ));
    assert!(matches!(
        parse(info(dict(vec![(b"noroot", leaf(5))]), 16_384)),
        Err(MetainfoError::InvalidFileTree(_))
    ));
    let short_root = dict(vec![(
        b"",
        dict(vec![
            (b"length", Value::Int(10)),
            (b"pieces root", bstr(&[1; 31])),
        ]),
    )]);
    assert!(matches!(
        parse(info(dict(vec![(b"r", short_root)]), 16_384)),
        Err(MetainfoError::InvalidFileTree(_))
    ));
    assert!(matches!(
        parse(info(dict(vec![(b"f", Value::Int(1))]), 16_384)),
        Err(MetainfoError::InvalidFileTree(_))
    ));
    assert!(matches!(
        parse(info(dict(vec![]), 16_384)),
        Err(MetainfoError::NoFiles)
    ));
    assert!(matches!(
        parse(info(dict(vec![(b"ok", file.clone())]), 3 * 16_384)),
        Err(MetainfoError::InvalidPieceLength(_))
    ));
    assert!(matches!(
        parse(info(dict(vec![(b"ok", file)]), 8_192)),
        Err(MetainfoError::InvalidPieceLength(_))
    ));
    // Empty files need no root.
    assert!(parse(info(dict(vec![(b"e", leaf(0))]), 16_384)).is_ok());
}

#[test]
fn web_seeds_come_from_url_list() {
    let (bytes, _) = build("one", vec![F(vec!["one"], content(10, 1))], 16_384, true);
    let mut t = Value::decode(&bytes).unwrap().into_owned();
    insert(&mut t, b"url-list", Some(bstr(b"http://seed.example/one")));
    let m = Metainfo::from_bytes(&t.to_vec()).unwrap();
    assert_eq!(m.web_seeds(), ["http://seed.example/one"]);
    insert(
        &mut t,
        b"url-list",
        Some(Value::List(vec![
            bstr(b"http://a/"),
            Value::Int(3),
            bstr(b""),
            bstr(b"http://a/"),
            bstr(b"ftp://b/"),
        ])),
    );
    let m = Metainfo::from_bytes(&t.to_vec()).unwrap();
    assert_eq!(m.web_seeds(), ["http://a/", "ftp://b/"]);
    let m = m.with_web_seeds([
        "http://c/".to_owned(),
        "http://a/".to_owned(),
        String::new(),
    ]);
    assert_eq!(m.web_seeds(), ["http://a/", "ftp://b/", "http://c/"]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn generated_v2_and_hybrid_torrents_verify(
        lengths in proptest::collection::vec(0usize..80_000, 1..6),
        shift in 0u32..3,
        hybrid in any::<bool>(),
    ) {
        prop_assume!(lengths.iter().any(|&l| l > 0));
        let piece = 16_384u32 << shift;
        let names = ["f0", "f1", "f2", "f3", "f4", "f5"];
        let files = lengths
            .iter()
            .enumerate()
            .map(|(i, &l)| F(vec!["d", names[i]], content(l, i as u8)))
            .collect();
        let (bytes, stream) = build("root", files, piece, hybrid);
        let m = Metainfo::from_bytes(&bytes).unwrap();
        prop_assert_eq!(m.info().total_length(), stream.len() as u64);
        prop_assert!(every_piece_checks(&m, &stream));
        let covered: u64 = (0..m.info().piece_count())
            .flat_map(|p| m.info().piece_file_slices(p))
            .map(|s| s.length)
            .sum();
        prop_assert_eq!(covered, m.info().total_length());
    }
}
