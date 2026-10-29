//! Runs the `dravity` binary against generated torrents and a loopback seeder.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::panic
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
        .args([
            "--no-dht",
            "--no-port-mapping",
            "--download-limit",
            "16K",
            "--quiet",
        ])
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
        .args(["--no-dht", "--no-port-mapping", "--quiet"])
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
        .args(["--no-dht", "--no-port-mapping", "--no-resume", "--quiet"])
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

/// Reads `child`'s stderr until the "accepting peers on port N" line.
fn listen_port(child: &mut std::process::Child) -> u16 {
    use std::io::BufRead;
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { return };
            if let Some(port) = line.strip_prefix("accepting peers on port ") {
                let _ = tx.send(port.trim().parse::<u16>().unwrap());
            }
        }
    });
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .expect("listen port line")
}

/// A `--seed` process serves a second download, which dials it over uTP.
#[test]
fn seed_serves_another_download() {
    let dir = tempfile::tempdir().unwrap();
    let data = content();
    let (torrent, _) = write_torrent(dir.path(), &data);
    let seed_dir = dir.path().join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();
    std::fs::write(seed_dir.join("hello.bin"), &data).unwrap();
    let mut seeder = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("download")
        .arg(&torrent)
        .arg("--output")
        .arg(&seed_dir)
        .args([
            "--seed",
            "--port",
            "0",
            "--no-dht",
            "--no-port-mapping",
            "--no-resume",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let port = listen_port(&mut seeder);

    let out_dir = dir.path().join("out");
    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("download")
        .arg(&torrent)
        .arg("--output")
        .arg(&out_dir)
        .arg("--peer")
        .arg(format!("127.0.0.1:{port}"))
        .args([
            "--no-dht",
            "--no-port-mapping",
            "--prefer-utp",
            "--port",
            "0",
            "--quiet",
        ])
        .output()
        .unwrap();
    seeder.kill().unwrap();
    let _ = seeder.wait();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(out_dir.join("hello.bin")).unwrap(), data);
}

#[test]
fn info_numbers_files_and_bad_selections_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (torrent, _) = write_torrent(dir.path(), &content());
    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("info")
        .arg(&torrent)
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("  [0] "), "{text}");

    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("download")
        .arg(&torrent)
        .args([
            "--files",
            "3",
            "--no-dht",
            "--no-port-mapping",
            "--no-listen",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("file index 3 out of range"), "{stderr}");

    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .args([
            "download",
            &format!("magnet:?xt=urn:btih:{}", "ab".repeat(20)),
        ])
        .args(["--file-priority", "0=high"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("need a .torrent file"));

    let out = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("download")
        .arg(&torrent)
        .args(["--file-priority", "0=urgent"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown priority"));
}

/// A minimal HTTP/1.1 GET; returns the status line and body.
fn http_get(addr: SocketAddr, path: &str, token: &str) -> (String, String) {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    (
        head.lines().next().unwrap_or("").to_owned(),
        body.to_owned(),
    )
}

/// A child process killed when dropped, so a failing test leaves none
/// behind.
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `dravity serve` and returns it with its HTTP address.
fn start_serve(dir: &Path) -> (Daemon, SocketAddr, std::sync::mpsc::Receiver<String>) {
    use std::io::BufRead;
    let mut child = Command::new(env!("CARGO_BIN_EXE_dravity"))
        .arg("serve")
        .arg("--download-dir")
        .arg(dir.join("downloads"))
        .arg("--watch")
        .arg(dir.join("watch"))
        .args([
            "--http",
            "127.0.0.1:0",
            "--port",
            "0",
            "--no-dht",
            "--no-port-mapping",
        ])
        .env("DRAVITY_TOKEN", "t0ken")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let child = Daemon(child);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut addr = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut lines = Vec::new();
    while addr.is_none() && std::time::Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
            if let Some(rest) = line.strip_prefix("HTTP API on http://") {
                addr = rest.strip_suffix("/api/v1/").and_then(|a| a.parse().ok());
            }
            lines.push(line);
        }
    }
    let addr = addr.unwrap_or_else(|| panic!("no HTTP address in {lines:?}"));
    // Lines seen before the address go back in front of the rest.
    let (tx2, rx2) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in lines {
            let _ = tx2.send(line);
        }
        while let Ok(line) = rx.recv() {
            if tx2.send(line).is_err() {
                return;
            }
        }
    });
    (child, addr, rx2)
}

#[cfg(unix)]
fn interrupt(daemon: &mut Daemon) {
    let child = &mut daemon.0;
    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "serve exited with {status}");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    panic!("serve did not stop on SIGINT");
}

/// `serve` takes torrents from the watch folder, shows them over HTTP and
/// restores them after a restart.
#[cfg(unix)]
#[test]
fn serve_watches_serves_the_api_and_restores_its_torrents() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, addr, _lines) = start_serve(dir.path());

    let (status, _) = http_get(addr, "/api/v1/torrents", "wrong");
    assert!(status.contains(" 401 "), "{status}");
    let (status, body) = http_get(addr, "/api/v1/torrents", "t0ken");
    assert!(status.contains(" 200 "), "{status}");
    assert_eq!(body, "[]");

    let (torrent, hash) = write_torrent(dir.path(), &content());
    std::fs::copy(&torrent, dir.path().join("watch/new.torrent")).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let (_, body) = http_get(addr, "/api/v1/torrents", "t0ken");
        if body.contains(&hash.to_hex()) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "not added: {body}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(dir.path().join("watch/new.torrent.added").exists());
    let (_, metrics) = http_get(addr, "/metrics", "t0ken");
    assert!(
        metrics.contains("dravity_torrents{state=\"running\"} 1"),
        "{metrics}"
    );
    interrupt(&mut child);

    let (mut child, addr, lines) = start_serve(dir.path());
    let restored = lines
        .iter()
        .take_while(|l| !l.starts_with("HTTP API"))
        .any(|l| l == "restored 1 torrent(s)");
    assert!(restored, "the torrent was not restored");
    let (_, body) = http_get(addr, "/api/v1/torrents", "t0ken");
    assert!(body.contains(&hash.to_hex()), "{body}");
    interrupt(&mut child);
}
