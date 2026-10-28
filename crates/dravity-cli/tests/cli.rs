//! Runs the `dravity` binary against generated torrents and a loopback seeder.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation
)]

use std::borrow::Cow;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::Command;

use bytes::{Bytes, BytesMut};
use dravity_bencode::{Dict, Value};
use dravity_core::{InfoHash, Metainfo, PeerId};
use dravity_wire::{Handshake, Message, MessageDecoder};
use sha1::{Digest, Sha1};

const PIECE_LEN: usize = 16 * 1024;

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

/// Writes a single-file torrent to `dir/test.torrent`.
fn write_torrent(dir: &Path, content: &[u8]) -> (std::path::PathBuf, InfoHash) {
    let pieces: Vec<u8> = content
        .chunks(PIECE_LEN)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let info = dict(vec![
        ("name", bytes(b"hello.bin")),
        ("length", Value::Int(content.len() as i64)),
        ("piece length", Value::Int(PIECE_LEN as i64)),
        ("pieces", bytes(&pieces)),
    ]);
    let encoded = dict(vec![
        ("announce", bytes(b"udp://127.0.0.1:9/announce")),
        ("info", info),
    ])
    .to_vec();
    let path = dir.join("test.torrent");
    std::fs::write(&path, &encoded).unwrap();
    (path, Metainfo::from_bytes(&encoded).unwrap().info_hash())
}

/// A blocking single-connection seeder on a background thread.
fn seeder(content: Vec<u8>, info_hash: InfoHash) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut hs = [0u8; 68];
        sock.read_exact(&mut hs).unwrap();
        let ours = Handshake::new(info_hash, PeerId::new(*b"-TS0001-seedseedseed"));
        sock.write_all(&ours.to_bytes()).unwrap();
        let pieces = content.len().div_ceil(PIECE_LEN) as u32;
        let mut bits = vec![0u8; pieces.div_ceil(8) as usize];
        for i in 0..pieces as usize {
            bits[i / 8] |= 0x80 >> (i % 8);
        }
        let mut out = BytesMut::new();
        Message::Bitfield(Bytes::from(bits))
            .encode(&mut out)
            .unwrap();
        sock.write_all(&out).unwrap();
        let mut decoder = MessageDecoder::new();
        let mut buf = BytesMut::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match sock.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            while let Ok(Some(message)) = decoder.decode(&mut buf) {
                let reply = match message {
                    Message::Interested => Message::Unchoke,
                    Message::Request(r) => {
                        let start = r.piece as usize * PIECE_LEN + r.offset as usize;
                        Message::Piece {
                            piece: r.piece,
                            offset: r.offset,
                            data: Bytes::copy_from_slice(
                                &content[start..start + r.length as usize],
                            ),
                        }
                    }
                    _ => continue,
                };
                let mut out = BytesMut::new();
                reply.encode(&mut out).unwrap();
                if sock.write_all(&out).is_err() {
                    return;
                }
            }
        }
    });
    addr
}

fn content() -> Vec<u8> {
    (0..50_000u32).map(|i| (i % 253) as u8).collect()
}

#[test]
fn info_describes_torrent() {
    let dir = tempfile::tempdir().unwrap();
    let (torrent, hash) = write_torrent(dir.path(), &content());
    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("info")
        .arg(&torrent)
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("name:       hello.bin"), "{text}");
    assert!(text.contains(&hash.to_string()), "{text}");
    assert!(text.contains("pieces:     4 x 16.0 KiB"), "{text}");
    assert!(text.contains("udp://127.0.0.1:9/announce"), "{text}");
}

#[test]
fn download_from_explicit_peer() {
    let dir = tempfile::tempdir().unwrap();
    let data = content();
    let (torrent, hash) = write_torrent(dir.path(), &data);
    let peer = seeder(data.clone(), hash);
    let out_dir = dir.path().join("out");
    let started = std::time::Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("download")
        .arg(&torrent)
        .arg("--output")
        .arg(&out_dir)
        .arg("--peer")
        .arg(peer.to_string())
        .args(["--no-dht", "--download-limit", "16K", "--quiet"])
        .output()
        .unwrap();
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "16 KiB/s should pace 50 KiB; finished in {:?}",
        started.elapsed()
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(out_dir.join("hello.bin")).unwrap(), data);
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello.bin: complete"));
    // Fastresume data lands in <output>/.dravity by default.
    let resume = out_dir.join(format!(".dravity/{}.resume", hash.to_hex()));
    assert!(resume.exists());
}

#[test]
fn resume_dir_and_no_resume() {
    let dir = tempfile::tempdir().unwrap();
    let data = content();
    let (torrent, hash) = write_torrent(dir.path(), &data);
    let peer = seeder(data.clone(), hash);
    let out_dir = dir.path().join("out");
    let resume_dir = dir.path().join("resume");
    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("download")
        .arg(&torrent)
        .arg("--output")
        .arg(&out_dir)
        .arg("--peer")
        .arg(peer.to_string())
        .arg("--resume-dir")
        .arg(&resume_dir)
        .args(["--no-dht", "--quiet"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        resume_dir
            .join(format!("{}.resume", hash.to_hex()))
            .exists()
    );
    assert!(!out_dir.join(".dravity").exists());

    let peer = seeder(data, hash);
    let out_dir = dir.path().join("out2");
    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("download")
        .arg(&torrent)
        .arg("--output")
        .arg(&out_dir)
        .arg("--peer")
        .arg(peer.to_string())
        .args(["--no-dht", "--no-resume", "--quiet"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out_dir.join(".dravity").exists());
}

#[test]
fn info_describes_magnet() {
    let uri = format!(
        "magnet:?xt=urn:btih:{}&dn=hello.bin&tr=udp%3A%2F%2F127.0.0.1%3A9&x.pe=127.0.0.1:7000",
        "ab".repeat(20)
    );
    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("info")
        .arg(&uri)
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("name:       hello.bin"), "{text}");
    assert!(text.contains(&"ab".repeat(20)), "{text}");
    assert!(text.contains("tracker:    udp://127.0.0.1:9"), "{text}");
    assert!(text.contains("peer:       127.0.0.1:7000"), "{text}");

    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .args(["info", "magnet:?dn=nothing"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("error: parsing the magnet link"));
}

#[test]
fn rejects_bad_rate() {
    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .args(["download", "x.torrent", "--download-limit", "fast"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid rate"));
}

#[test]
fn reports_bad_torrent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.torrent");
    std::fs::write(&path, b"not bencode").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("info")
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("error: parsing"));
}
