//! `WebSeedClient` against a tiny loopback HTTP server: range checks,
//! redirects and the non-public address guard.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_possible_truncation
)]

use std::error::Error as _;
use std::net::SocketAddr;
use std::sync::Arc;

use dravity_webseed::{Segment, WebSeedClient, WebSeedConfig, WebSeedError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How the server answers `/f`.
#[derive(Clone, Copy)]
enum Mode {
    /// Honour `Range` with `206` and a correct `Content-Range`.
    Ranges,
    /// Ignore `Range`: always `200` with the whole file.
    Whole,
    /// `206` with one byte more than asked for.
    Long,
    /// `206` with one byte less than asked for.
    Short,
    /// `206` without `Content-Range`.
    NoContentRange,
    /// `206` of the right length, from the next byte on, labelled honestly.
    Shifted,
    /// `206` of the right bytes, but a `Content-Range` total of another size.
    OtherTotal,
}

/// Serves `/f` (the given content) as `mode` says, redirects `/same` to
/// `/f` and `/away` to `http://localhost:<port>/f`, and answers 404 for
/// anything else.
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
                let redirect = match path.as_str() {
                    "/f" => None,
                    "/same" => Some("/f".to_owned()),
                    "/away" => Some(format!("http://localhost:{}/f", addr.port())),
                    _ => {
                        let _ = sock
                            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                            .await;
                        return;
                    }
                };
                if let Some(location) = redirect {
                    let head = format!(
                        "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
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
                let total = content.len();
                let partial = "206 Partial Content";
                let (status, body, content_range) = match mode {
                    Mode::Ranges => (partial, &content[a..=b], Some(format!("{a}-{b}/{total}"))),
                    Mode::Whole => ("200 OK", &content[..], None),
                    Mode::Long => (
                        partial,
                        &content[a..=b + 1],
                        Some(format!("{a}-{b}/{total}")),
                    ),
                    Mode::Short => (partial, &content[a..b], Some(format!("{a}-{b}/{total}"))),
                    Mode::NoContentRange => (partial, &content[a..=b], None),
                    Mode::Shifted => (
                        partial,
                        &content[a + 1..=b + 1],
                        Some(format!("{}-{}/{total}", a + 1, b + 1)),
                    ),
                    Mode::OtherTotal => (
                        partial,
                        &content[a..=b],
                        Some(format!("{a}-{b}/{}", total + 1)),
                    ),
                };
                let content_range = content_range
                    .map(|r| format!("content-range: bytes {r}\r\n"))
                    .unwrap_or_default();
                let head = format!(
                    "HTTP/1.1 {status}\r\n{content_range}content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
            });
        }
    });
    addr
}

const LEN: u64 = 100_000;

fn content() -> Arc<Vec<u8>> {
    Arc::new((0..LEN as u32).map(|i| (i % 253) as u8).collect())
}

fn segment(url: String, offset: u64, length: u64) -> Segment {
    Segment::Fetch {
        url,
        offset,
        length,
        file_length: LEN,
    }
}

fn fetch(addr: SocketAddr, path: &str, offset: u64, length: u64) -> Segment {
    segment(format!("http://{addr}{path}"), offset, length)
}

/// A client allowed to reach the loopback test server.
fn client() -> WebSeedClient {
    let mut config = WebSeedConfig::default();
    config.allow_private_addresses = true;
    WebSeedClient::new(&config).unwrap()
}

/// Every message in an error's source chain.
fn chain(e: &WebSeedError) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(" / ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
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
            fetch(addr, "/f", LEN - 1, 1),
        ])
        .await
        .unwrap();
    let mut want = data[10..70_010].to_vec();
    want.extend_from_slice(&[0; 5]);
    want.extend_from_slice(&data[..3]);
    want.push(data[LEN as usize - 1]);
    assert_eq!(got, want);
}

#[tokio::test]
async fn a_server_without_ranges_only_serves_file_starts() {
    let data = content();
    let addr = server(Arc::clone(&data), Mode::Whole).await;
    let c = client();
    // The start of the file: the first bytes of the 200 body.
    assert_eq!(
        c.fetch(&[fetch(addr, "/f", 0, 20)]).await.unwrap(),
        data[..20]
    );
    // The whole file.
    assert_eq!(
        c.fetch(&[fetch(addr, "/f", 0, LEN)]).await.unwrap(),
        data[..]
    );
    // Anything further in would mean discarding a prefix.
    assert!(matches!(
        c.fetch(&[fetch(addr, "/f", 5, 20)]).await,
        Err(WebSeedError::RangeIgnored)
    ));
    // A 200 for a file of another size is another file.
    let other = Segment::Fetch {
        url: format!("http://{addr}/f"),
        offset: 0,
        length: 20,
        file_length: LEN + 1,
    };
    assert!(matches!(
        c.fetch(&[other]).await,
        Err(WebSeedError::FileLength {
            expected,
            got: LEN
        }) if expected == LEN + 1
    ));
}

#[tokio::test]
async fn content_range_must_be_the_requested_range() {
    let data = content();
    let c = client();
    for (mode, header) in [
        (Mode::NoContentRange, String::new()),
        (Mode::Shifted, "bytes 101-200/100000".to_owned()),
        (Mode::OtherTotal, "bytes 100-199/100001".to_owned()),
    ] {
        let addr = server(Arc::clone(&data), mode).await;
        match c.fetch(&[fetch(addr, "/f", 100, 100)]).await {
            Err(WebSeedError::ContentRange { got, start, end }) => {
                assert_eq!((got, start, end), (header, 100, 199));
            }
            other => panic!("expected a Content-Range error, got {other:?}"),
        }
    }
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
        c.fetch(&[segment("ftp://h/f".into(), 0, 1)]).await,
        Err(WebSeedError::UnsupportedScheme(_))
    ));
    assert!(matches!(
        c.fetch(&[segment("not a url".into(), 0, 1)]).await,
        Err(WebSeedError::InvalidUrl(_))
    ));
    // A range past the end of the file is never requested.
    assert!(matches!(
        c.fetch(&[fetch(long, "/f", LEN - 1, 2)]).await,
        Err(WebSeedError::OutOfRange { .. })
    ));
}

#[tokio::test]
async fn redirects_stay_on_the_original_host() {
    let data = content();
    let addr = server(Arc::clone(&data), Mode::Ranges).await;
    let c = client();
    assert_eq!(
        c.fetch(&[fetch(addr, "/same", 0, 10)]).await.unwrap(),
        data[..10]
    );
    // `localhost` is another host name, even for the same machine.
    let e = c.fetch(&[fetch(addr, "/away", 0, 10)]).await.unwrap_err();
    assert!(
        chain(&e).contains("refused web seed redirect"),
        "{}",
        chain(&e)
    );
}

#[tokio::test]
async fn non_public_addresses_are_refused_by_default() {
    let data = content();
    let addr = server(Arc::clone(&data), Mode::Ranges).await;
    let c = WebSeedClient::new(&WebSeedConfig::default()).unwrap();
    for url in [
        format!("http://{addr}/f"),
        "http://169.254.169.254/latest/meta-data".to_owned(),
        "http://[::1]/f".to_owned(),
        "http://10.0.0.1/f".to_owned(),
        "http://100.64.0.1/f".to_owned(),
        "http://[fd00::1]/f".to_owned(),
        "http://0.0.0.0/f".to_owned(),
    ] {
        assert!(
            matches!(
                c.fetch(&[segment(url.clone(), 0, 1)]).await,
                Err(WebSeedError::ForbiddenAddress(_))
            ),
            "{url}"
        );
    }
    // A name is checked after resolution, at connect time.
    let e = c
        .fetch(&[segment(format!("http://localhost:{}/f", addr.port()), 0, 1)])
        .await
        .unwrap_err();
    assert!(
        chain(&e).contains("resolves only to non-public addresses"),
        "{}",
        chain(&e)
    );
}
