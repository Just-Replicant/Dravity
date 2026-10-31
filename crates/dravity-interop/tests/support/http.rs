//! A minimal HTTP/1.1 client for the clients' local control APIs, and an
//! HTTP tracker.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A response: status, headers (lower-case names) and body.
#[derive(Debug)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Response {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Sends one request (`Connection: close`) and reads the whole response.
pub(crate) async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<Response> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    // Read until the response is complete, not until the server closes:
    // some servers (qBittorrent's) keep the socket open a while anyway.
    let mut raw = Vec::new();
    let mut buf = [0; 16 * 1024];
    loop {
        if complete(&raw) {
            break;
        }
        let n = tokio::time::timeout(Duration::from_secs(30), stream.read(&mut buf))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "HTTP response timed out"))??;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    parse(&raw)
}

/// Whether `raw` holds a whole response (by `Content-Length`, or the last
/// chunk of a chunked body). Without either, the server's close ends it.
fn complete(raw: &[u8]) -> bool {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let body = &raw[split + 4..];
    let header = |name: &str| {
        head.split("\r\n")
            .filter_map(|l| l.split_once(':'))
            .find(|(n, _)| n.trim() == name)
            .map(|(_, v)| v.trim().to_owned())
    };
    if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        return body.ends_with(b"0\r\n\r\n");
    }
    header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|len| body.len() >= len)
}

fn parse(raw: &[u8]) -> io::Result<Response> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP response");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(bad)?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad())?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(bad)?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    if chunked {
        body = unchunk(&body).ok_or_else(bad)?;
    }
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn unchunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line = data.windows(2).position(|w| w == b"\r\n")?;
        let size = std::str::from_utf8(&data[..line]).ok()?;
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        data = &data[line + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(data.get(..size)?);
        data = data.get(size + 2..)?;
    }
}

/// `application/x-www-form-urlencoded` escaping.
pub(crate) fn form_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Standard base64 (for Transmission's `metainfo`).
pub(crate) fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// An HTTP tracker on loopback that hands every announcer the other peers
/// of the same info-hash (compact, IPv4).
#[derive(Debug, Clone)]
pub(crate) struct Tracker {
    pub(crate) addr: SocketAddr,
    swarms: Arc<Mutex<HashMap<Vec<u8>, Vec<SocketAddr>>>>,
}

impl Tracker {
    pub(crate) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let swarms: Arc<Mutex<HashMap<Vec<u8>, Vec<SocketAddr>>>> = Arc::default();
        let shared = Arc::clone(&swarms);
        tokio::spawn(async move {
            while let Ok((mut sock, from)) = listener.accept().await {
                let swarms = Arc::clone(&shared);
                tokio::spawn(async move {
                    let mut buf = vec![0; 8192];
                    let mut len = 0;
                    while !buf[..len].windows(4).any(|w| w == b"\r\n\r\n") && len < buf.len() {
                        match sock.read(&mut buf[len..]).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => len += n,
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..len]).into_owned();
                    let target = head.split_whitespace().nth(1).unwrap_or("");
                    let query = target.split_once('?').map_or("", |(_, q)| q);
                    let mut info_hash = None;
                    let mut port = None;
                    for pair in query.split('&') {
                        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                        match k {
                            "info_hash" => info_hash = Some(percent_decode(v)),
                            "port" => port = v.parse::<u16>().ok(),
                            _ => {}
                        }
                    }
                    if std::env::var_os("DRAVITY_INTEROP_DEBUG").is_some() {
                        eprintln!("tracker: announce from {from}: {target}");
                    }
                    let body = match (info_hash, port) {
                        (Some(hash), Some(port)) => {
                            let me = SocketAddr::new(from.ip(), port);
                            let mut swarms = swarms.lock().unwrap();
                            let peers = swarms.entry(hash).or_default();
                            if !peers.contains(&me) {
                                peers.push(me);
                            }
                            let mut compact = Vec::new();
                            for p in peers.iter().filter(|p| **p != me) {
                                if let SocketAddr::V4(v4) = p {
                                    compact.extend_from_slice(&v4.ip().octets());
                                    compact.extend_from_slice(&v4.port().to_be_bytes());
                                }
                            }
                            let mut body = format!(
                                "d8:intervali2e12:min intervali1e5:peers{}:",
                                compact.len()
                            )
                            .into_bytes();
                            body.extend_from_slice(&compact);
                            body.push(b'e');
                            body
                        }
                        _ => b"d14:failure reason11:bad requeste".to_vec(),
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });
        Self { addr, swarms }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}/announce", self.addr)
    }

    /// Peers that announced `info_hash` (20 bytes).
    pub(crate) fn peers(&self, info_hash: &[u8]) -> Vec<SocketAddr> {
        self.swarms
            .lock()
            .unwrap()
            .get(info_hash)
            .cloned()
            .unwrap_or_default()
    }
}
