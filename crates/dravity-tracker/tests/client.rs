//! `TrackerClient` against in-process HTTP and UDP trackers.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::net::SocketAddr;
use std::time::Duration;

use dravity_core::{InfoHash, PeerId};
use dravity_tracker::{
    AnnounceEvent, AnnounceRequest, ClientConfig, CryptoSupport, ScrapeStats, TrackerClient,
    TrackerError, UdpProxy, udp,
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
        crypto: CryptoSupport::None,
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
    std::sync::Arc<std::sync::Mutex<Vec<u16>>>,
) {
    counting_udp_tracker_with(Duration::ZERO).await
}

/// Like [`counting_udp_tracker`], waiting `reply_delay` before each reply so
/// overlapping clients both send before either receives.
async fn counting_udp_tracker_with(
    reply_delay: Duration,
) -> (
    String,
    std::sync::Arc<std::sync::atomic::AtomicU32>,
    tokio::sync::mpsc::UnboundedSender<()>,
    std::sync::Arc<std::sync::Mutex<Vec<u16>>>,
) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let url = format!("udp://{}", server.local_addr().unwrap());
    let connects = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&connects);
    let ports = Arc::new(std::sync::Mutex::new(Vec::<u16>::new()));
    let ports_task = Arc::clone(&ports);
    let (rotate, mut rotations) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut current = 1000u64;
        loop {
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            ports_task.lock().unwrap().push(from.port());
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
            if !reply_delay.is_zero() {
                tokio::time::sleep(reply_delay).await;
            }
            server.send_to(&reply, from).await.unwrap();
        }
    });
    (url, connects, rotate, ports)
}

#[tokio::test]
async fn udp_connection_ids_are_cached_and_refreshed() {
    use std::sync::atomic::Ordering;
    let (url, connects, rotate, ports) = counting_udp_tracker().await;
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
    let ports = ports.lock().unwrap();
    let distinct: std::collections::HashSet<u16> = ports.iter().copied().collect();
    // The cached client reuses one socket; the other client binds its own.
    assert_eq!(distinct.len(), 2, "source ports: {ports:?}");
}

#[tokio::test]
async fn overlapping_udp_announces_share_one_socket() {
    use std::collections::HashSet;
    use std::sync::atomic::Ordering;
    // Replies wait so both clones send before either finishes, which used to
    // let them steal each other's datagrams or each open a connect.
    let (url, connects, rotate, ports) = counting_udp_tracker_with(Duration::from_millis(30)).await;
    let c = client();
    let first = c.clone();
    let second = c.clone();
    let req = request();
    let (left, right) = tokio::join!(first.announce(&url, &req), second.announce(&url, &req));
    left.unwrap();
    right.unwrap();
    assert_eq!(
        connects.load(Ordering::SeqCst),
        1,
        "second clone reused the id"
    );
    // The tracker forgets the id while two announces are in flight: one
    // reconnect, then the waiter uses that new id.
    rotate.send(()).unwrap();
    let first = c.clone();
    let second = c.clone();
    let (left, right) = tokio::join!(first.announce(&url, &req), second.announce(&url, &req));
    left.unwrap();
    right.unwrap();
    assert_eq!(
        connects.load(Ordering::SeqCst),
        2,
        "one reconnect for both clones"
    );
    let ports = ports.lock().unwrap();
    let distinct: HashSet<u16> = ports.iter().copied().collect();
    assert_eq!(distinct.len(), 1, "one source port: {ports:?}");
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

/// Serves HTTP requests forever: the first `redirects` answer `302` to
/// `location(n)`, later ones the announce body. Returns the request lines.
async fn redirecting_tracker(
    location: fn(SocketAddr, usize) -> String,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut n = 0;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let Ok(read) = sock.read(&mut buf).await else {
                    break;
                };
                if read == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..read]);
            }
            let line = String::from_utf8_lossy(&req)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            let response = if line.contains("/final/announce") {
                let body = b"d8:intervali60e5:peers0:e";
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    String::from_utf8_lossy(body)
                )
            } else {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    location(addr, n)
                )
            };
            n += 1;
            let _ = sock.write_all(response.as_bytes()).await;
        }
    });
    (addr, task)
}

#[tokio::test]
async fn ssrf_local_tracker_needs_announce_path() {
    let r = client()
        .announce("http://127.0.0.1:9/admin", &request())
        .await;
    assert!(matches!(r, Err(TrackerError::Blocked(_))), "{r:?}");
}

#[tokio::test]
async fn ssrf_redirect_within_the_lan_is_followed() {
    let (addr, task) = redirecting_tracker(|a, _| format!("http://{a}/final/announce")).await;
    let r = client()
        .announce(&format!("http://{addr}/announce"), &request())
        .await
        .unwrap();
    assert_eq!(r.interval, Some(Duration::from_secs(60)));
    task.abort();
}

#[tokio::test]
async fn ssrf_redirect_to_other_scheme_or_local_path_is_blocked() {
    // reqwest never follows a non-HTTP scheme; the redirect is returned.
    let (addr, task) = redirecting_tracker(|_, _| "file:///etc/passwd".to_owned()).await;
    let r = client()
        .announce(&format!("http://{addr}/announce"), &request())
        .await;
    assert!(
        matches!(
            r,
            Err(TrackerError::Blocked(_) | TrackerError::HttpStatus(302))
        ),
        "{r:?}"
    );
    task.abort();

    let (addr, task) = redirecting_tracker(|a, _| format!("http://{a}/admin")).await;
    let r = client()
        .announce(&format!("http://{addr}/announce"), &request())
        .await;
    assert!(matches!(r, Err(TrackerError::Blocked(_))), "{r:?}");
    task.abort();
}

#[tokio::test]
async fn ssrf_redirect_loop_is_cut_off() {
    let (addr, task) = redirecting_tracker(|a, n| format!("http://{a}/announce?hop={n}")).await;
    let r = client()
        .announce(&format!("http://{addr}/announce"), &request())
        .await;
    assert!(matches!(r, Err(TrackerError::Blocked(_))), "{r:?}");
    task.abort();
}

#[tokio::test]
async fn ssrf_checks_can_be_disabled() {
    let mut c = ClientConfig::default();
    c.ssrf_mitigation = false;
    c.http_timeout = Duration::from_secs(2);
    let r = TrackerClient::new(c)
        .unwrap()
        .announce("http://127.0.0.1:9/admin", &request())
        .await;
    assert!(!matches!(r, Err(TrackerError::Blocked(_))), "{r:?}");
}

#[tokio::test]
async fn http_scrape() {
    let (url, task) = http_tracker(
        "200 OK",
        b"d5:filesd20:\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11\x11d8:completei7e10:downloadedi90e10:incompletei2eeee",
    )
    .await;
    let stats = client().scrape(&url, &[0x11; 20]).await.unwrap();
    assert_eq!(
        stats,
        ScrapeStats {
            seeders: 7,
            leechers: 2,
            downloaded: 90
        }
    );
    let line = task.await.unwrap();
    assert!(line.starts_with("GET /scrape?info_hash=%11%11"), "{line}");
    // Missing from the answer, or a URL without `announce`: no scrape.
    let (url, _t) = http_tracker("200 OK", b"d5:filesdee").await;
    assert!(matches!(
        client().scrape(&url, &[0x11; 20]).await,
        Err(TrackerError::NoScrape)
    ));
    assert!(matches!(
        client()
            .scrape("http://127.0.0.1:1/tracker", &[0x11; 20])
            .await,
        Err(TrackerError::NoScrape)
    ));
}

#[tokio::test]
async fn udp_scrape_reuses_the_connection() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let url = format!("udp://{}", server.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (n, from) = server.recv_from(&mut buf).await.unwrap();
        assert_eq!(n, 16);
        let tid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
        server
            .send_to(&udp::encode_connect_response(tid, 55), from)
            .await
            .unwrap();
        for _ in 0..2 {
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 36, "one hash");
            assert_eq!(u64::from_be_bytes(buf[..8].try_into().unwrap()), 55);
            assert_eq!(&buf[16..36], &[0x22; 20]);
            let tid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            let stats = [ScrapeStats {
                seeders: 1,
                leechers: 2,
                downloaded: 3,
            }];
            server
                .send_to(&udp::encode_scrape_response(tid, &stats), from)
                .await
                .unwrap();
        }
    });
    let c = client();
    for _ in 0..2 {
        let stats = c.scrape(&url, &[0x22; 20]).await.unwrap();
        assert_eq!(stats.downloaded, 3);
    }
    task.await.unwrap();
}

/// A SOCKS5 proxy (no auth) that is also the tracker behind it: it checks
/// the CONNECT names the tracker host (remote DNS), then answers the
/// tunnelled HTTP announce itself.
#[tokio::test]
async fn http_announce_through_socks5_resolves_at_the_proxy() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut greet = [0u8; 2];
        sock.read_exact(&mut greet).await.unwrap();
        assert_eq!(greet[0], 5);
        let mut methods = vec![0u8; usize::from(greet[1])];
        sock.read_exact(&mut methods).await.unwrap();
        assert!(methods.contains(&0));
        sock.write_all(&[5, 0]).await.unwrap();
        let mut head = [0u8; 4];
        sock.read_exact(&mut head).await.unwrap();
        assert_eq!(head[..2], [5, 1], "CONNECT");
        assert_eq!(head[3], 3, "a domain name, not a locally resolved IP");
        let mut len = [0u8; 1];
        sock.read_exact(&mut len).await.unwrap();
        let mut name = vec![0u8; usize::from(len[0])];
        sock.read_exact(&mut name).await.unwrap();
        let mut port = [0u8; 2];
        sock.read_exact(&mut port).await.unwrap();
        sock.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await.unwrap();
            req.extend_from_slice(&buf[..n]);
        }
        let body = b"d8:intervali600e5:peers0:e";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        sock.write_all(head.as_bytes()).await.unwrap();
        sock.write_all(body).await.unwrap();
        (String::from_utf8(name).unwrap(), u16::from_be_bytes(port))
    });
    let mut c = ClientConfig::default();
    c.http_timeout = Duration::from_secs(5);
    c.http_proxy = Some(format!("socks5h://{proxy}"));
    let client = TrackerClient::new(c).unwrap();
    let r = client
        .announce("http://tracker.invalid:6969/announce", &request())
        .await
        .unwrap();
    assert_eq!(r.interval, Some(Duration::from_secs(600)));
    assert_eq!(task.await.unwrap(), ("tracker.invalid".to_owned(), 6969));
}

/// A SOCKS5 proxy with username/password that relays UDP (RFC 1928 §7):
/// datagrams from the client go to the address in their header, replies
/// come back wrapped. Returns the proxy address and every relayed target.
async fn socks5_udp_relay() -> (
    SocketAddr,
    std::sync::Arc<std::sync::Mutex<Vec<SocketAddr>>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let targets = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = std::sync::Arc::clone(&targets);
    tokio::spawn(async move {
        let (mut control, _) = listener.accept().await.unwrap();
        let mut greet = [0u8; 4];
        control.read_exact(&mut greet).await.unwrap();
        assert_eq!(greet, [5, 2, 0, 2], "offers username/password");
        control.write_all(&[5, 2]).await.unwrap();
        let mut auth = [0u8; 9];
        control.read_exact(&mut auth).await.unwrap();
        assert_eq!(&auth, b"\x01\x02me\x04pass");
        control.write_all(&[1, 0]).await.unwrap();
        let mut req = [0u8; 10];
        control.read_exact(&mut req).await.unwrap();
        assert_eq!(req[..4], [5, 3, 0, 1], "UDP ASSOCIATE");
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let outside = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Reply with 0.0.0.0: the client must use the proxy's address.
        let port = relay.local_addr().unwrap().port().to_be_bytes();
        control
            .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, port[0], port[1]])
            .await
            .unwrap();
        let mut buf = [0u8; 2048];
        loop {
            let (n, client) = relay.recv_from(&mut buf).await.unwrap();
            let (target, payload) = dravity_core::socks5::parse_udp_header(&buf[..n]).unwrap();
            log.lock().unwrap().push(target);
            outside.send_to(payload, target).await.unwrap();
            let mut answer = [0u8; 2048];
            let (m, from) = outside.recv_from(&mut answer).await.unwrap();
            let mut wrapped = dravity_core::socks5::udp_header(from);
            wrapped.extend_from_slice(&answer[..m]);
            relay.send_to(&wrapped, client).await.unwrap();
        }
    });
    (addr, targets)
}

#[tokio::test]
async fn udp_announce_through_a_socks5_relay() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tracker = server.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (_, from) = server.recv_from(&mut buf).await.unwrap();
        let tid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
        server
            .send_to(&udp::encode_connect_response(tid, 5), from)
            .await
            .unwrap();
        let (_, from) = server.recv_from(&mut buf).await.unwrap();
        let tid = u32::from_be_bytes(buf[12..16].try_into().unwrap());
        let peers: Vec<SocketAddr> = vec!["10.9.8.7:51413".parse().unwrap()];
        server
            .send_to(&udp::encode_announce_response(tid, 900, 1, 2, &peers), from)
            .await
            .unwrap();
    });
    let (proxy, targets) = socks5_udp_relay().await;
    let mut c = ClientConfig::default();
    c.udp_timeout = Duration::from_millis(500);
    c.udp_retries = 1;
    c.udp_proxy = Some(UdpProxy::new(
        proxy.to_string(),
        Some(("me".into(), "pass".into())),
    ));
    let r = TrackerClient::new(c)
        .unwrap()
        .announce(&format!("udp://{tracker}/announce"), &request())
        .await
        .unwrap();
    assert_eq!(
        r.peers,
        vec!["10.9.8.7:51413".parse::<SocketAddr>().unwrap()]
    );
    // Both the connect and the announce went through the relay.
    assert_eq!(*targets.lock().unwrap(), vec![tracker, tracker]);
}
