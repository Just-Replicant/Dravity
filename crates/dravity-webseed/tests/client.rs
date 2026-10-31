//! `WebSeedClient` against a tiny loopback HTTP server.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;

use dravity_webseed::{Segment, WebSeedClient, WebSeedConfig, WebSeedError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How the server answers.
#[derive(Clone, Copy)]
enum Mode {
    /// Honour `Range` with `206`.
    Ranges,
    /// Ignore `Range`: always `200` with the whole file.
    Whole,
    /// `206` with one byte more than asked for.
    Long,
    /// `206` with one byte less than asked for.
    Short,
}

/// Serves `/f` (the given content) and answers 404 for anything else.
async fn server(content: Arc<Vec<u8>>, mode: Mode) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let content = Arc::clone(&content);
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    req.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&req).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("").to_owned();
                if path != "/f" {
                    let _ = sock
                        .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    return;
                }
                let range = text
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("range: bytes=")
                            .map(str::to_owned)
                    })
                    .unwrap();
                let (a, b) = range.trim().split_once('-').unwrap();
                let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
                let (status, body) = match mode {
                    Mode::Ranges => ("206 Partial Content", content[a..=b].to_vec()),
                    Mode::Whole => ("200 OK", content.to_vec()),
                    Mode::Long => ("206 Partial Content", content[a..=b + 1].to_vec()),
                    Mode::Short => ("206 Partial Content", content[a..b].to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });
    addr
}

fn content() -> Arc<Vec<u8>> {
    Arc::new((0..100_000u32).map(|i| (i % 253) as u8).collect())
}

fn fetch(addr: SocketAddr, path: &str, offset: u64, length: u64) -> Segment {
    Segment::Fetch {
        url: format!("http://{addr}{path}"),
        offset,
        length,
    }
}

fn client() -> WebSeedClient {
    WebSeedClient::new(&WebSeedConfig::default()).unwrap()
}

#[tokio::test]
async fn ranges_and_zeros_are_concatenated() {
    let data = content();
    let addr = server(Arc::clone(&data), Mode::Ranges).await;
    let got = client()
        .fetch(&[
            fetch(addr, "/f", 10, 70_000),
            Segment::Zeros { length: 5 },
            fetch(addr, "/f", 0, 3),
        ])
        .await
        .unwrap();
    let mut want = data[10..70_010].to_vec();
    want.extend_from_slice(&[0; 5]);
    want.extend_from_slice(&data[..3]);
    assert_eq!(got, want);
}

#[tokio::test]
async fn a_server_without_ranges_only_serves_file_starts() {
    let data = content();
    let addr = server(Arc::clone(&data), Mode::Whole).await;
    let c = client();
    assert_eq!(
        c.fetch(&[fetch(addr, "/f", 0, 20)]).await.unwrap(),
        data[..20]
    );
    assert!(matches!(
        c.fetch(&[fetch(addr, "/f", 5, 20)]).await,
        Err(WebSeedError::RangeIgnored)
    ));
}

#[tokio::test]
async fn wrong_bodies_and_statuses_are_errors() {
    let data = content();
    let long = server(Arc::clone(&data), Mode::Long).await;
    let short = server(Arc::clone(&data), Mode::Short).await;
    let c = client();
    assert!(matches!(
        c.fetch(&[fetch(long, "/f", 0, 100)]).await,
        Err(WebSeedError::BodyLength { expected: 100, .. })
    ));
    assert!(matches!(
        c.fetch(&[fetch(short, "/f", 0, 100)]).await,
        Err(WebSeedError::BodyLength {
            expected: 100,
            got: 99
        })
    ));
    assert!(matches!(
        c.fetch(&[fetch(long, "/missing", 0, 1)]).await,
        Err(WebSeedError::HttpStatus(404))
    ));
    assert!(matches!(
        c.fetch(&[Segment::Fetch {
            url: "ftp://h/f".into(),
            offset: 0,
            length: 1
        }])
        .await,
        Err(WebSeedError::UnsupportedScheme(_))
    ));
    assert!(matches!(
        c.fetch(&[Segment::Fetch {
            url: "not a url".into(),
            offset: 0,
            length: 1
        }])
        .await,
        Err(WebSeedError::InvalidUrl(_))
    ));
}
