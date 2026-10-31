//! `.torrent` files for the tests: v1, v2-only and hybrid, over the same
//! files.

use std::borrow::Cow;
use std::path::Path;

use dravity_bencode::{Dict, Value};
use dravity_core::merkle;
use sha1::{Digest, Sha1};

/// Piece length of every test torrent.
pub(crate) const PIECE: u32 = 64 * 1024;

/// Which hashes a torrent carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    V1,
    Hybrid,
    V2,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::V1 => "v1",
            Self::Hybrid => "hybrid",
            Self::V2 => "v2",
        })
    }
}

/// `(path below the torrent's directory, content)`.
pub(crate) type File = (Vec<&'static str>, Vec<u8>);

fn bstr(b: &[u8]) -> Value<'static> {
    Value::Bytes(Cow::Owned(b.to_vec()))
}

fn dict(entries: Vec<(&[u8], Value<'static>)>) -> Value<'static> {
    Value::Dict(
        entries
            .into_iter()
            .map(|(k, v)| (Cow::Owned(k.to_vec()), v))
            .collect(),
    )
}

fn content(len: usize, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32 ^ seed).wrapping_mul(2_654_435_761).to_le_bytes()[2])
        .collect()
}

/// Two files of a few MiB (neither a multiple of the piece length) and a
/// small one, sorted by path.
pub(crate) fn files() -> Vec<File> {
    vec![
        (vec!["a", "big.bin"], content(3 * 1024 * 1024 + 12_345, 1)),
        (vec!["b.bin"], content(700_001, 2)),
        (vec!["c.txt"], b"hello from dravity\n".to_vec()),
    ]
}

/// A multi-file torrent named `payload` over `files` (sorted by path) that
/// announces to `tracker`.
pub(crate) fn make(files: &[File], kind: Kind, tracker: &str) -> Vec<u8> {
    let mut tree = Dict::new();
    let mut layers = Dict::new();
    let mut v1_files = Vec::new();
    let mut stream = Vec::new();
    let last = files.iter().rposition(|f| !f.1.is_empty());
    for (i, (path, data)) in files.iter().enumerate() {
        let mut leaf = vec![(&b"length"[..], Value::Int(data.len() as i64))];
        if let Some(h) = merkle::file_hashes(data, PIECE) {
            leaf.push((&b"pieces root"[..], bstr(&h.root)));
            if !h.layer.is_empty() {
                layers.insert(Cow::Owned(h.root.to_vec()), bstr(&h.layer.concat()));
            }
        }
        let mut node = &mut tree;
        for c in path {
            let Value::Dict(next) = node
                .entry(Cow::Owned(c.as_bytes().to_vec()))
                .or_insert_with(|| Value::Dict(Dict::new()))
            else {
                unreachable!()
            };
            node = next;
        }
        node.insert(Cow::Borrowed(&b""[..]), dict(leaf));

        stream.extend_from_slice(data);
        let segs = |p: &[&str]| Value::List(p.iter().map(|c| bstr(c.as_bytes())).collect());
        v1_files.push(dict(vec![
            (b"length", Value::Int(data.len() as i64)),
            (b"path", segs(path)),
        ]));
        // Hybrids align files to pieces with BEP 47 padding files.
        let rem = data.len() % PIECE as usize;
        if kind == Kind::Hybrid && rem != 0 && Some(i) != last {
            let pad = PIECE as usize - rem;
            stream.resize(stream.len() + pad, 0);
            v1_files.push(dict(vec![
                (b"attr", bstr(b"p")),
                (b"length", Value::Int(pad as i64)),
                (b"path", segs(&[".pad", &pad.to_string()])),
            ]));
        }
    }
    let pieces: Vec<u8> = stream
        .chunks(PIECE as usize)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let mut info = vec![
        (&b"name"[..], bstr(b"payload")),
        (b"piece length", Value::Int(i64::from(PIECE))),
    ];
    if kind != Kind::V1 {
        info.push((b"file tree", Value::Dict(tree)));
        info.push((b"meta version", Value::Int(2)));
    }
    if kind != Kind::V2 {
        info.push((b"files", Value::List(v1_files)));
        info.push((b"pieces", bstr(&pieces)));
    }
    let mut top = vec![
        (&b"announce"[..], bstr(tracker.as_bytes())),
        (b"info", dict(info)),
    ];
    if kind != Kind::V1 {
        top.push((b"piece layers", Value::Dict(layers)));
    }
    dict(top).to_vec()
}

/// The BEP 47 padding files of a hybrid over `files`: `(name below
/// `.pad`, length)`, as [`make`] lists them.
pub(crate) fn pads(files: &[File]) -> Vec<(String, usize)> {
    let last = files.iter().rposition(|f| !f.1.is_empty());
    files
        .iter()
        .enumerate()
        .filter_map(|(i, (_, data))| {
            let rem = data.len() % PIECE as usize;
            (rem != 0 && Some(i) != last).then(|| {
                let pad = PIECE as usize - rem;
                (pad.to_string(), pad)
            })
        })
        .collect()
}

/// Writes the zero-filled padding files of a hybrid over `files` below
/// `dir/payload/.pad`, for a client that reads them from disk instead of
/// treating them as zeros.
pub(crate) fn write_pads(dir: &Path, files: &[File]) {
    let pad_dir = dir.join("payload").join(".pad");
    std::fs::create_dir_all(&pad_dir).unwrap();
    for (name, len) in pads(files) {
        std::fs::write(pad_dir.join(name), vec![0; len]).unwrap();
    }
}

/// Writes `files` below `dir/payload`.
pub(crate) fn write_files(dir: &Path, files: &[File]) {
    for (path, data) in files {
        let p = path.iter().fold(dir.join("payload"), |p, c| p.join(c));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
}

/// Whether `dir/payload` holds exactly `files`.
pub(crate) fn same_files(dir: &Path, files: &[File]) -> Result<(), String> {
    for (path, data) in files {
        let p = path.iter().fold(dir.join("payload"), |p, c| p.join(c));
        match std::fs::read(&p) {
            Ok(got) if got == *data => {}
            Ok(got) => return Err(format!("{}: {} bytes differ", p.display(), got.len())),
            Err(e) => return Err(format!("{}: {e}", p.display())),
        }
    }
    Ok(())
}
