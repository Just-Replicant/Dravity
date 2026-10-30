//! The HTTP API against real sessions over loopback, through
//! `tower::ServiceExt::oneshot` (no HTTP server needed).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::panic
)]

use std::borrow::Cow;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use dravity_bencode::{Dict, Value};
use dravity_core::{InfoHash, Metainfo};
use dravity_engine::Alert;
use dravity_http::HttpConfig;
use dravity_session::{AddTorrentOptions, ListenConfig, Session, SessionAlert, SessionConfig};
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use sha1::{Digest, Sha1};
use tokio::sync::broadcast;
use tower::ServiceExt;

const PIECE_LEN: usize = 32 * 1024;

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

/// A two-file torrent (`payload/a.bin`, `payload/movie.mp4`).
fn make_torrent(len_a: usize, len_b: usize) -> (Vec<u8>, Vec<u8>) {
    let content: Vec<u8> = (0..len_a + len_b)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[2])
        .collect();
    let pieces: Vec<u8> = content
        .chunks(PIECE_LEN)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let file = |name: &str, len: usize| {
        dict(vec![
            ("length", Value::Int(len as i64)),
            ("path", Value::List(vec![bytes(name.as_bytes())])),
        ])
    };
    let info = dict(vec![
        ("name", bytes(b"payload")),
        (
            "files",
            Value::List(vec![file("a.bin", len_a), file("movie.mp4", len_b)]),
        ),
        ("piece length", Value::Int(PIECE_LEN as i64)),
        ("pieces", bytes(&pieces)),
    ]);
    (dict(vec![("info", info)]).to_vec(), content)
}

fn config() -> SessionConfig {
    let mut c = SessionConfig::default();
    c.tick_interval = Duration::from_millis(50);
    c.stop_announce_timeout = Duration::from_secs(1);
    c.dht = None;
    c.listen = Some(ListenConfig::loopback(0));
    c.port_mapping = None;
    c
}

async fn until(
    rx: &mut broadcast::Receiver<SessionAlert>,
    info_hash: InfoHash,
    f: impl Fn(&Alert) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Ok(a) if a.info_hash == info_hash && f(&a.alert) => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("alert channel closed: {e}"),
            }
        }
    })
    .await
    .expect("timed out waiting for an alert");
}

struct Seed {
    session: Session,
    addr: SocketAddr,
    _dir: tempfile::TempDir,
}

async fn seed(encoded: &[u8], content: &[u8], len_a: usize) -> Seed {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path(), content, len_a);
    let session = Session::new(config()).unwrap();
    let port = session.listen_port().unwrap();
    let mut alerts = session.subscribe();
    let m = Metainfo::from_bytes(encoded).unwrap();
    let hash = m.info_hash();
    session
        .add_torrent(m, AddTorrentOptions::new(dir.path()))
        .unwrap();
    until(&mut alerts, hash, |a| *a == Alert::Finished).await;
    Seed {
        session,
        addr: SocketAddr::from(([127, 0, 0, 1], port)),
        _dir: dir,
    }
}

fn write_files(dir: &Path, content: &[u8], len_a: usize) {
    std::fs::create_dir_all(dir.join("payload")).unwrap();
    std::fs::write(dir.join("payload/a.bin"), &content[..len_a]).unwrap();
    std::fs::write(dir.join("payload/movie.mp4"), &content[len_a..]).unwrap();
}

struct Response {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Response {
    fn json(&self) -> Json {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&self.body)))
    }

    fn header(&self, name: header::HeaderName) -> &str {
        self.headers.get(name).unwrap().to_str().unwrap()
    }
}

async fn call(app: &Router, request: Request<Body>) -> Response {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Response {
        status,
        headers,
        body,
    }
}

fn request(method: Method, uri: &str) -> axum::http::request::Builder {
    Request::builder().method(method).uri(uri)
}

async fn get(app: &Router, uri: &str) -> Response {
    call(app, request(Method::GET, uri).body(Body::empty()).unwrap()).await
}

async fn send_json(app: &Router, method: Method, uri: &str, body: &Json) -> Response {
    let request = request(method, uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    call(app, request).await
}

async fn post(app: &Router, uri: &str) -> Response {
    call(app, request(Method::POST, uri).body(Body::empty()).unwrap()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_need_the_token_when_one_is_set() {
    let session = Session::new(config()).unwrap();
    let app = dravity_http::router(session, HttpConfig::new("unused").with_token("s3cret"));

    let denied = get(&app, "/api/v1/session").await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
    assert!(
        denied
            .header(header::WWW_AUTHENTICATE)
            .starts_with("Bearer")
    );
    let wrong = request(Method::GET, "/metrics")
        .header(header::AUTHORIZATION, "Bearer s3cre")
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&app, wrong).await.status, StatusCode::UNAUTHORIZED);

    let allowed = request(Method::GET, "/api/v1/session")
        .header(header::AUTHORIZATION, "Bearer s3cret")
        .body(Body::empty())
        .unwrap();
    let allowed = call(&app, allowed).await;
    assert_eq!(allowed.status, StatusCode::OK);
    let body = allowed.json();
    assert_eq!(body["torrents"], 0);
    assert_eq!(body["peer_id"].as_str().unwrap().len(), 40);
    assert!(body["listen_port"].as_u64().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_torrent_is_added_downloaded_streamed_and_removed_through_the_api() {
    let (len_a, len_b) = (PIECE_LEN * 2, PIECE_LEN * 3 + 1000);
    let (encoded, content) = make_torrent(len_a, len_b);
    let hash = Metainfo::from_bytes(&encoded).unwrap().info_hash().to_hex();
    let seed = seed(&encoded, &content, len_a).await;

    let leech_dir = tempfile::tempdir().unwrap();
    let leech = Session::new(config()).unwrap();
    let mut alerts = leech.subscribe();
    let app = dravity_http::router(leech.clone(), HttpConfig::new(leech_dir.path()));

    // Upload paused, so nothing starts before the priorities are set.
    let upload = request(Method::POST, "/api/v1/torrents?paused=true")
        .header(header::CONTENT_TYPE, "application/x-bittorrent")
        .body(Body::from(encoded.clone()))
        .unwrap();
    let added = call(&app, upload).await;
    assert_eq!(added.status, StatusCode::CREATED, "{:?}", added.json());
    assert_eq!(added.json()["info_hash"], hash);

    let list = get(&app, "/api/v1/torrents").await.json();
    assert_eq!(list[0]["state"], "paused");
    assert_eq!(list[0]["name"], "payload");
    let detail = get(&app, &format!("/api/v1/torrents/{hash}")).await.json();
    assert_eq!(detail["stats"], Json::Null);
    assert_eq!(
        detail["files"][1]["path"],
        Path::new("payload")
            .join("movie.mp4")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(detail["files"][1]["have"], Json::Null);

    let set = send_json(
        &app,
        Method::PUT,
        &format!("/api/v1/torrents/{hash}/files"),
        &json!({"priorities": ["high", "low"]}),
    )
    .await;
    assert_eq!(set.status, StatusCode::NO_CONTENT);
    let one = send_json(
        &app,
        Method::PUT,
        &format!("/api/v1/torrents/{hash}/files/1"),
        &json!({"priority": "normal"}),
    )
    .await;
    assert_eq!(one.status, StatusCode::NO_CONTENT);

    let paused_head = call(
        &app,
        request(
            Method::HEAD,
            &format!("/api/v1/torrents/{hash}/files/1/content"),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(paused_head.status, StatusCode::CONFLICT);

    let resumed = post(&app, &format!("/api/v1/torrents/{hash}/resume")).await;
    assert_eq!(resumed.status, StatusCode::NO_CONTENT);
    let peers = send_json(
        &app,
        Method::POST,
        &format!("/api/v1/torrents/{hash}/peers"),
        &json!({"peers": [seed.addr.to_string()]}),
    )
    .await;
    assert_eq!(peers.status, StatusCode::NO_CONTENT);

    // A player can ask for the length before any ranged read. The body
    // stays empty, including when the bytes are not on disk yet.
    let head = call(
        &app,
        request(
            Method::HEAD,
            &format!("/api/v1/torrents/{hash}/files/1/content"),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());
    assert_eq!(head.header(header::CONTENT_TYPE), "video/mp4");
    assert_eq!(head.header(header::ACCEPT_RANGES), "bytes");
    assert_eq!(head.header(header::CONTENT_LENGTH), len_b.to_string());
    assert!(!head.headers.contains_key(header::CONTENT_RANGE));

    let head_range = request(
        Method::HEAD,
        &format!("/api/v1/torrents/{hash}/files/1/content"),
    )
    .header(header::RANGE, "bytes=40000-")
    .body(Body::empty())
    .unwrap();
    let head_range = call(&app, head_range).await;
    assert_eq!(head_range.status, StatusCode::PARTIAL_CONTENT);
    assert!(head_range.body.is_empty());
    assert_eq!(
        head_range.header(header::CONTENT_LENGTH),
        (len_b - 40_000).to_string()
    );
    assert_eq!(
        head_range.header(header::CONTENT_RANGE),
        format!("bytes 40000-{}/{len_b}", len_b - 1)
    );

    let head_outside = request(
        Method::HEAD,
        &format!("/api/v1/torrents/{hash}/files/0/content"),
    )
    .header(header::RANGE, format!("bytes={len_a}-"))
    .body(Body::empty())
    .unwrap();
    let head_outside = call(&app, head_outside).await;
    assert_eq!(head_outside.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(head_outside.body.is_empty());
    assert_eq!(
        head_outside.header(header::CONTENT_RANGE),
        format!("bytes */{len_a}")
    );

    // Streaming works while (or after) the file downloads.
    let ranged = request(
        Method::GET,
        &format!("/api/v1/torrents/{hash}/files/1/content"),
    )
    .header(header::RANGE, "bytes=40000-")
    .body(Body::empty())
    .unwrap();
    let ranged = tokio::time::timeout(Duration::from_secs(30), call(&app, ranged))
        .await
        .expect("streaming timed out");
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.header(header::CONTENT_TYPE), "video/mp4");
    assert_eq!(ranged.header(header::ACCEPT_RANGES), "bytes");
    assert_eq!(
        ranged.header(header::CONTENT_RANGE),
        format!("bytes 40000-{}/{len_b}", len_b - 1)
    );
    assert_eq!(ranged.body, &content[len_a + 40_000..]);

    let hash_id: InfoHash = hash.parse().unwrap();
    until(&mut alerts, hash_id, |a| *a == Alert::Finished).await;
    let whole = get(&app, &format!("/api/v1/torrents/{hash}/files/0/content")).await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.body, &content[..len_a]);
    let outside = request(
        Method::GET,
        &format!("/api/v1/torrents/{hash}/files/0/content"),
    )
    .header(header::RANGE, format!("bytes={len_a}-"))
    .body(Body::empty())
    .unwrap();
    let outside = call(&app, outside).await;
    assert_eq!(outside.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        outside.header(header::CONTENT_RANGE),
        format!("bytes */{len_a}")
    );

    let detail = get(&app, &format!("/api/v1/torrents/{hash}")).await.json();
    assert_eq!(detail["state"], "running");
    assert_eq!(detail["stats"]["status"], "finished");
    assert_eq!(detail["stats"]["wanted_left"], 0);
    assert_eq!(detail["files"][0]["priority"], "high");
    assert_eq!(detail["files"][0]["have"], len_a);

    let metrics = get(&app, "/metrics").await;
    assert_eq!(metrics.status, StatusCode::OK);
    assert!(
        metrics
            .header(header::CONTENT_TYPE)
            .starts_with("text/plain; version=0.0.4")
    );
    let text = String::from_utf8(metrics.body).unwrap();
    assert!(
        text.contains("dravity_torrents{state=\"running\"} 1\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "dravity_torrent_have_bytes{{info_hash=\"{hash}\"}} {}\n",
            len_a + len_b
        )),
        "{text}"
    );

    assert_eq!(
        post(&app, &format!("/api/v1/torrents/{hash}/pause"))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    let not_running = get(&app, &format!("/api/v1/torrents/{hash}/files/0/content")).await;
    assert_eq!(not_running.status, StatusCode::CONFLICT);

    let removed = call(
        &app,
        request(
            Method::DELETE,
            &format!("/api/v1/torrents/{hash}?delete_files=true"),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert!(!leech_dir.path().join("payload").exists());
    let gone = get(&app, &format!("/api/v1/torrents/{hash}")).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert!(gone.json()["error"].as_str().unwrap().contains(&hash));

    leech.shutdown().await;
    seed.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_requests_are_rejected_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let app = dravity_http::router(session.clone(), HttpConfig::new(dir.path()));

    let bad_hash = get(&app, "/api/v1/torrents/xyz").await;
    assert_eq!(bad_hash.status, StatusCode::BAD_REQUEST);
    let unknown = get(&app, &format!("/api/v1/torrents/{}", "ab".repeat(20))).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    let garbage = request(Method::POST, "/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/x-bittorrent")
        .body(Body::from("not bencode"))
        .unwrap();
    let garbage = call(&app, garbage).await;
    assert_eq!(garbage.status, StatusCode::BAD_REQUEST);
    assert!(
        garbage.json()["error"]
            .as_str()
            .unwrap()
            .contains(".torrent")
    );

    let hash = "cd".repeat(20);
    let magnet = json!({"magnet": format!("magnet:?xt=urn:btih:{hash}&dn=thing"), "paused": true});
    let added = send_json(&app, Method::POST, "/api/v1/torrents", &magnet).await;
    assert_eq!(added.status, StatusCode::CREATED);
    let again = send_json(&app, Method::POST, "/api/v1/torrents", &magnet).await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    let list = get(&app, "/api/v1/torrents").await.json();
    assert_eq!(list[0]["name"], "thing");
    assert_eq!(list[0]["has_metadata"], false);

    // The metadata is unknown: no files, no priorities.
    let priorities = send_json(
        &app,
        Method::PUT,
        &format!("/api/v1/torrents/{hash}/files/0"),
        &json!({"priority": "high"}),
    )
    .await;
    assert_eq!(priorities.status, StatusCode::CONFLICT);
    let bad_priority = send_json(
        &app,
        Method::PUT,
        &format!("/api/v1/torrents/{hash}/files"),
        &json!({"priorities": ["urgent"]}),
    )
    .await;
    assert!(bad_priority.status.is_client_error());

    let unknown_field = send_json(
        &app,
        Method::POST,
        "/api/v1/torrents",
        &json!({"magnet": format!("magnet:?xt=urn:btih:{}", "ef".repeat(20)), "evil": 1}),
    )
    .await;
    assert_eq!(unknown_field.status, StatusCode::BAD_REQUEST);
    assert!(
        unknown_field.json()["error"]
            .as_str()
            .unwrap()
            .contains("evil")
    );
    let bad_magnet = send_json(
        &app,
        Method::POST,
        "/api/v1/torrents",
        &json!({"magnet": "http://example.com"}),
    )
    .await;
    assert_eq!(bad_magnet.status, StatusCode::BAD_REQUEST);

    let peers = send_json(
        &app,
        Method::POST,
        &format!("/api/v1/torrents/{hash}/peers"),
        &json!({"peers": ["127.0.0.1:1"]}),
    )
    .await;
    assert_eq!(peers.status, StatusCode::CONFLICT, "paused: not running");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alerts_arrive_as_server_sent_events() {
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(config()).unwrap();
    let app = dravity_http::router(session.clone(), HttpConfig::new(dir.path()));
    let (encoded, _) = make_torrent(PIECE_LEN, PIECE_LEN);
    let hash = Metainfo::from_bytes(&encoded).unwrap().info_hash().to_hex();

    let response = app
        .clone()
        .oneshot(
            request(
                Method::GET,
                &format!("/api/v1/events?stats=false&info_hash={hash}"),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut body = response.into_body();

    // Adding the torrent (no peers, no data) makes it check and download.
    let upload = request(Method::POST, "/api/v1/torrents")
        .body(Body::from(encoded))
        .unwrap();
    assert_eq!(call(&app, upload).await.status, StatusCode::CREATED);

    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !text.contains("\"status\":\"downloading\"") {
            let frame = body.frame().await.unwrap().unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(&String::from_utf8_lossy(&data));
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no state change event: {text}"));
    assert!(text.contains("event: state_changed\n"), "{text}");
    assert!(
        text.contains(&format!("\"info_hash\":\"{hash}\"")),
        "{text}"
    );
    assert!(!text.contains("event: stats"), "{text}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_stops_on_shutdown_with_an_event_stream_open() {
    let session = Session::new(config()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(dravity_http::serve(
        listener,
        session.clone(),
        HttpConfig::new("unused"),
        async {
            let _ = stop_rx.await;
        },
    ));

    // A raw HTTP/1.1 client holding an event stream open.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET /api/v1/events HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut head = [0u8; 64];
    let n = client.read(&mut head).await.unwrap();
    assert!(head[..n].starts_with(b"HTTP/1.1 200"));

    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();
    session.shutdown().await;
}
