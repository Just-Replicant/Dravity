//! The SAM v3 client against a fake bridge: session create, naming
//! lookup, outbound connect and inbound accept, with a byte exchange.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use dravity_i2p::{DestinationSpec, SamConfig, SamError, Session};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};

struct SessionEntry {
    dest: String,
    pending_accepts: VecDeque<oneshot::Sender<Connector>>,
    pending_connects: VecDeque<Connector>,
}

struct Connector {
    stream: TcpStream,
    from_dest: String,
}

struct BridgeState {
    sessions: HashMap<String, SessionEntry>,
    /// Public dest -> session id.
    dests: HashMap<String, String>,
    /// Name -> public dest (plus `ME` is handled per session).
    names: HashMap<String, String>,
}

struct FakeSam {
    addr: std::net::SocketAddr,
    state: Arc<Mutex<BridgeState>>,
}

impl FakeSam {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(BridgeState {
            sessions: HashMap::new(),
            dests: HashMap::new(),
            names: HashMap::new(),
        }));
        let serve = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let serve = Arc::clone(&serve);
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, serve).await {
                        tracing_error_stub(e);
                    }
                });
            }
        });
        Self { addr, state }
    }

    fn config(&self) -> SamConfig {
        SamConfig::new(self.addr.to_string())
    }

    async fn set_name(&self, name: &str, dest: &str) {
        self.state
            .lock()
            .await
            .names
            .insert(name.to_owned(), dest.to_owned());
    }
}

// Integration tests have no tracing dependency; ignore handler errors.
fn tracing_error_stub(_: std::io::Error) {}

async fn handle(mut stream: TcpStream, state: Arc<Mutex<BridgeState>>) -> std::io::Result<()> {
    let hello = read_line(&mut stream).await?;
    if !hello.starts_with("HELLO VERSION") {
        write_line(&mut stream, "HELLO REPLY RESULT=NOVERSION").await?;
        return Ok(());
    }
    write_line(&mut stream, "HELLO REPLY RESULT=OK VERSION=3.1").await?;
    let line = read_line(&mut stream).await?;
    let fields = dravity_i2p::parse_fields(&line);
    if line.starts_with("SESSION CREATE") {
        session_create(stream, state, &fields).await
    } else if line.starts_with("STREAM CONNECT") {
        stream_connect(stream, state, &fields).await
    } else if line.starts_with("STREAM ACCEPT") {
        stream_accept(stream, state, &fields).await
    } else if line.starts_with("NAMING LOOKUP") {
        // A lookup on a fresh connection is unusual; refuse.
        write_line(
            &mut stream,
            "NAMING REPLY RESULT=I2P_ERROR MESSAGE=\"no session\"",
        )
        .await
    } else {
        Ok(())
    }
}

async fn session_create(
    mut stream: TcpStream,
    state: Arc<Mutex<BridgeState>>,
    fields: &HashMap<String, String>,
) -> std::io::Result<()> {
    let Some(id) = fields.get("ID").cloned() else {
        write_line(&mut stream, "SESSION STATUS RESULT=INVALID_ID").await?;
        return Ok(());
    };
    let dest = format!("DEST-{id}-{}", fastrand::u32(..));
    {
        let mut st = state.lock().await;
        if st.sessions.contains_key(&id) {
            write_line(&mut stream, "SESSION STATUS RESULT=DUPLICATED_ID").await?;
            return Ok(());
        }
        st.dests.insert(dest.clone(), id.clone());
        st.sessions.insert(
            id.clone(),
            SessionEntry {
                dest: dest.clone(),
                pending_accepts: VecDeque::new(),
                pending_connects: VecDeque::new(),
            },
        );
    }
    write_line(
        &mut stream,
        &format!("SESSION STATUS RESULT=OK DESTINATION={dest}"),
    )
    .await?;
    // Control connection: naming lookups until it closes (and the session dies).
    loop {
        let line = match read_line(&mut stream).await {
            Ok(line) => line,
            Err(_) => break,
        };
        if line.starts_with("NAMING LOOKUP") {
            let fields = dravity_i2p::parse_fields(&line);
            let name = fields.get("NAME").map(String::as_str).unwrap_or("");
            let answer = {
                let st = state.lock().await;
                if name == "ME" {
                    Some(dest.clone())
                } else {
                    st.names.get(name).cloned()
                }
            };
            match answer {
                Some(value) => {
                    write_line(
                        &mut stream,
                        &format!("NAMING REPLY RESULT=OK NAME={name} VALUE={value}"),
                    )
                    .await?;
                }
                None => {
                    write_line(
                        &mut stream,
                        &format!("NAMING REPLY RESULT=KEY_NOT_FOUND NAME={name}"),
                    )
                    .await?;
                }
            }
        }
    }
    let mut st = state.lock().await;
    st.sessions.remove(&id);
    st.dests.retain(|_, v| v != &id);
    Ok(())
}

async fn stream_connect(
    mut stream: TcpStream,
    state: Arc<Mutex<BridgeState>>,
    fields: &HashMap<String, String>,
) -> std::io::Result<()> {
    let Some(id) = fields.get("ID") else {
        write_line(&mut stream, "STREAM STATUS RESULT=INVALID_ID").await?;
        return Ok(());
    };
    let Some(dest) = fields.get("DESTINATION") else {
        write_line(&mut stream, "STREAM STATUS RESULT=INVALID_KEY").await?;
        return Ok(());
    };
    let (from_dest, target) = {
        let st = state.lock().await;
        let from = st.sessions.get(id).map(|s| s.dest.clone());
        let target = st.dests.get(dest).cloned();
        (from, target)
    };
    let Some(from_dest) = from_dest else {
        write_line(&mut stream, "STREAM STATUS RESULT=INVALID_ID").await?;
        return Ok(());
    };
    let Some(target) = target else {
        write_line(&mut stream, "STREAM STATUS RESULT=CANT_REACH_PEER").await?;
        return Ok(());
    };
    write_line(&mut stream, "STREAM STATUS RESULT=OK").await?;
    let connector = Connector { stream, from_dest };
    // Check-and-park under one lock so we cannot miss a concurrent accept.
    {
        let mut st = state.lock().await;
        let Some(session) = st.sessions.get_mut(&target) else {
            return Ok(());
        };
        if let Some(tx) = session.pending_accepts.pop_front() {
            let _ = tx.send(connector);
        } else {
            session.pending_connects.push_back(connector);
        }
    }
    Ok(())
}

async fn stream_accept(
    mut stream: TcpStream,
    state: Arc<Mutex<BridgeState>>,
    fields: &HashMap<String, String>,
) -> std::io::Result<()> {
    let Some(id) = fields.get("ID").cloned() else {
        write_line(&mut stream, "STREAM STATUS RESULT=INVALID_ID").await?;
        return Ok(());
    };
    {
        let st = state.lock().await;
        if !st.sessions.contains_key(&id) {
            drop(st);
            write_line(&mut stream, "STREAM STATUS RESULT=INVALID_ID").await?;
            return Ok(());
        }
    }
    write_line(&mut stream, "STREAM STATUS RESULT=OK").await?;
    // Check-and-park under one lock so we cannot miss a concurrent connect.
    let (parked, waiting) = {
        let mut st = state.lock().await;
        let Some(session) = st.sessions.get_mut(&id) else {
            return Ok(());
        };
        if let Some(c) = session.pending_connects.pop_front() {
            (Some(c), None)
        } else {
            let (tx, rx) = oneshot::channel();
            session.pending_accepts.push_back(tx);
            (None, Some(rx))
        }
    };
    let connector = if let Some(c) = parked {
        c
    } else {
        match waiting.expect("parked a oneshot").await {
            Ok(c) => c,
            Err(_) => return Ok(()),
        }
    };
    write_line(&mut stream, &connector.from_dest).await?;
    let mut other = connector.stream;
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut other).await;
    Ok(())
}

async fn write_line(stream: &mut TcpStream, line: &str) -> std::io::Result<()> {
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

async fn read_line(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut one = [0u8; 1];
    loop {
        stream.read_exact(&mut one).await?;
        match one[0] {
            b'\n' => break,
            b'\r' => {}
            b => buf.push(b),
        }
        if buf.len() > 16 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line too long",
            ));
        }
    }
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[tokio::test]
async fn session_create_returns_a_unique_destination() {
    let sam = FakeSam::start().await;
    let a = Session::create(&sam.config(), DestinationSpec::Transient)
        .await
        .unwrap();
    let b = Session::create(&sam.config(), DestinationSpec::Transient)
        .await
        .unwrap();
    assert!(!a.destination().is_empty());
    assert!(!a.id().is_empty());
    assert_ne!(a.destination(), b.destination());
    assert_ne!(a.id(), b.id());
}

#[tokio::test]
async fn naming_lookup_finds_a_registered_name() {
    let sam = FakeSam::start().await;
    let a = Session::create(&sam.config(), DestinationSpec::Transient)
        .await
        .unwrap();
    sam.set_name("alice.i2p", a.destination()).await;
    let b = Session::create(&sam.config(), DestinationSpec::Transient)
        .await
        .unwrap();
    assert_eq!(b.lookup("alice.i2p").await.unwrap(), a.destination());
    assert_eq!(a.lookup("ME").await.unwrap(), a.destination());
    let err = b.lookup("missing.i2p").await.unwrap_err();
    match err {
        SamError::Refused { result, .. } => assert_eq!(result, "KEY_NOT_FOUND"),
        other => panic!("expected KEY_NOT_FOUND, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_and_inbound_streams_exchange_bytes() {
    let sam = FakeSam::start().await;
    let a = Session::create(&sam.config(), DestinationSpec::Transient)
        .await
        .unwrap();
    let b = Session::create(&sam.config(), DestinationSpec::Transient)
        .await
        .unwrap();
    sam.set_name("alice.i2p", a.destination()).await;
    let dest = b.lookup("alice.i2p").await.unwrap();

    let exchange = async {
        let incoming = tokio::spawn({
            let a = a.clone();
            async move { a.accept().await }
        });
        let mut out = b.connect(&dest).await.unwrap();
        out.write_all(b"hello").await.unwrap();
        out.flush().await.unwrap();

        let (mut inc, from) = incoming.await.unwrap().unwrap();
        assert_eq!(from, b.destination());
        let mut buf = [0u8; 5];
        inc.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        inc.write_all(b"world").await.unwrap();
        inc.flush().await.unwrap();
        out.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("stream exchange timed out");
}

#[tokio::test]
async fn connect_to_an_unknown_destination_is_refused() {
    let sam = FakeSam::start().await;
    let a = Session::create(&sam.config(), DestinationSpec::Transient)
        .await
        .unwrap();
    let err = a.connect("no-such-dest").await.unwrap_err();
    match err {
        SamError::Refused { result, .. } => assert_eq!(result, "CANT_REACH_PEER"),
        other => panic!("expected CANT_REACH_PEER, got {other}"),
    }
}
