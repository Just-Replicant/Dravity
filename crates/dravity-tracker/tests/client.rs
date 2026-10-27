//! `TrackerClient` against in-process HTTP and UDP trackers.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::net::SocketAddr;
use std::time::Duration;

use dravity_core::{InfoHash, PeerId};
use dravity_tracker::{
    AnnounceEvent, AnnounceRequest, ClientConfig, TrackerClient, TrackerError, udp,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

fn request() -> AnnounceRequest {
    AnnounceRequest {
        info_hash: InfoHash::new([0x11; 20]),
        peer_id: PeerId::new(*b"-DV0001-abcdefghijkl"),
        port: 6881,
        uploaded: 0,
        downloaded: 0,
        left: 1000,
        event: AnnounceEvent::Started,
        num_want: Some(50),
        key: 1,
    }
}

fn client() -> TrackerClient {
    let mut c = ClientConfig::default();
    c.http_timeout = Duration::from_secs(5);
    c.udp_timeout = Duration::from_millis(200);
    c.udp_retries = 1;
    TrackerClient::new(c).unwrap()
}

/// Serves one HTTP request with `body`, returning the request line.
async fn http_tracker(
    status: &'static str,
    body: &'static [u8],
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/announce", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await.unwrap();
            req.extend_from_slice(&buf[..n]);
        }
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        sock.write_all(head.as_bytes()).await.unwrap();
        sock.write_all(body).await.unwrap();
        String::from_utf8_lossy(&req)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    });
    (url, task)
}

#[tokio::test]
async fn http_announce() {
    let (url, task) = http_tracker(
        "200 OK",
        b"d8:intervali900e5:peers6:\x7f\x00\x00\x01\x1a\xe1e",
    )
    .await;
    let r = client().announce(&url, &request()).await.unwrap();
    assert_eq!(r.interval, Some(Duration::from_secs(900)));
    assert_eq!(
        r.peers,
        vec!["127.0.0.1:6881".parse::<SocketAddr>().unwrap()]
    );
    let line = task.await.unwrap();
    assert!(line.starts_with("GET /announce?info_hash=%11%11"), "{line}");
    assert!(line.contains("&event=started"), "{line}");
}

#[tokio::test]
async fn http_failure_and_status() {
    let (url, _t) = http_tracker("200 OK", b"d14:failure reason4:nopee").await;
    assert!(matches!(
        client().announce(&url, &request()).await,
        Err(TrackerError::Failure(m)) if m == "nope"
    ));
    let (url, _t) = http_tracker("404 Not Found", b"").await;
    assert!(matches!(
        client().announce(&url, &request()).await,
        Err(TrackerError::HttpStatus(404))
    ));
}

#[tokio::test]
async fn udp_announce_ignores_stray_datagrams() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let url = format!("udp://{}/announce", server.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (n, from) = server.recv_from(&mut buf).await.unwrap();
        assert_eq!(n, 16);
        let tid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
        // A stray answer for another transaction comes first.
        server
            .send_to(&udp::encode_connect_response(tid.wrapping_add(1), 1), from)
            .await
            .unwrap();
        server
            .send_to(&udp::encode_connect_response(tid, 77), from)
            .await
            .unwrap();
        let (n, from) = server.recv_from(&mut buf).await.unwrap();
        assert_eq!(n, 98);
        assert_eq!(u64::from_be_bytes(buf[..8].try_into().unwrap()), 77);
        let tid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
        let peers: Vec<SocketAddr> = vec!["10.1.2.3:4000".parse().unwrap()];
        server
            .send_to(&udp::encode_announce_response(tid, 600, 3, 4, &peers), from)
            .await
            .unwrap();
    });
    let r = client().announce(&url, &request()).await.unwrap();
    assert_eq!(
        r.peers,
        vec!["10.1.2.3:4000".parse::<SocketAddr>().unwrap()]
    );
    assert_eq!(r.seeders, Some(4));
    task.await.unwrap();
}

#[tokio::test]
async fn udp_retries_then_times_out() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let url = format!("udp://{}", server.local_addr().unwrap());
    let started = std::time::Instant::now();
    let err = client().announce(&url, &request()).await.unwrap_err();
    assert!(matches!(err, TrackerError::Timeout), "{err}");
    // 200 ms + 400 ms with one retry.
    assert!(started.elapsed() >= Duration::from_millis(600));
    let mut buf = [0u8; 64];
    let mut attempts = 0;
    while let Ok(Ok(_)) =
        tokio::time::timeout(Duration::from_millis(50), server.recv_from(&mut buf)).await
    {
        attempts += 1;
    }
    assert_eq!(attempts, 2);
}

/// A UDP tracker that answers every packet, counting connects; announces
/// with a connection id other than the latest one get an error response.
async fn counting_udp_tracker() -> (
    String,
    std::sync::Arc<std::sync::atomic::AtomicU32>,
    tokio::sync::mpsc::UnboundedSender<()>,
) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let url = format!("udp://{}", server.local_addr().unwrap());
    let connects = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&connects);
    let (rotate, mut rotations) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut current = 1000u64;
        loop {
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            while rotations.try_recv().is_ok() {
                current += 1;
            }
            let tid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            let reply = if n == 16 {
                counter.fetch_add(1, Ordering::SeqCst);
                udp::encode_connect_response(tid, current).to_vec()
            } else if u64::from_be_bytes(buf[..8].try_into().unwrap()) == current {
                udp::encode_announce_response(tid, 600, 1, 1, &[])
            } else {
                let mut err = 3u32.to_be_bytes().to_vec();
                err.extend_from_slice(&tid.to_be_bytes());
                err.extend_from_slice(b"bad connection id");
                err
            };
            server.send_to(&reply, from).await.unwrap();
        }
    });
    (url, connects, rotate)
}

#[tokio::test]
async fn udp_connection_ids_are_cached_and_refreshed() {
    use std::sync::atomic::Ordering;
    let (url, connects, rotate) = counting_udp_tracker().await;
    let c = client();
    c.announce(&url, &request()).await.unwrap();
    c.clone().announce(&url, &request()).await.unwrap();
    assert_eq!(connects.load(Ordering::SeqCst), 1, "id reused by clones");
    // The tracker forgets the id: one failed announce, one reconnect.
    rotate.send(()).unwrap();
    c.announce(&url, &request()).await.unwrap();
    assert_eq!(connects.load(Ordering::SeqCst), 2);
    c.announce(&url, &request()).await.unwrap();
    assert_eq!(connects.load(Ordering::SeqCst), 2);
    // A different client has its own cache.
    client().announce(&url, &request()).await.unwrap();
    assert_eq!(connects.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn rejects_bad_urls() {
    let c = client();
    assert!(matches!(
        c.announce("wss://x/announce", &request()).await,
        Err(TrackerError::UnsupportedScheme(_))
    ));
    assert!(matches!(
        c.announce("not a url", &request()).await,
        Err(TrackerError::InvalidUrl(_))
    ));
    assert!(matches!(
        c.announce("udp://example.com/announce", &request()).await,
        Err(TrackerError::InvalidUrl(_))
    ));
}
