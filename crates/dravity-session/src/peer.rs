//! One task per peer connection: handshake, then read → event and
//! command → batched write. The task never touches torrent state.
//!
//! Dialed peers are connected and handshaken here ([`run`]); accepted
//! peers arrive already handshaken from the listener ([`run_accepted`]).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use dravity_core::admit::ConnectionSlot;
use dravity_core::{InfoHash, PeerId};
use dravity_engine::{DialHints, PeerKey};
use dravity_mse::{MseError, Policy};
use dravity_utp::UtpSocket;
use dravity_wire::{Handshake, Message, Reserved};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, FramedParts};

use crate::codec::PeerCodec;
use crate::rate::RateLimits;
use crate::stream::{PeerStream, RawStream};
use crate::torrent::Input;

/// Commands from the torrent owner to a peer task.
#[derive(Debug)]
pub(crate) enum PeerCommand {
    Send(Message),
}

/// Static parameters of a peer connection.
#[derive(Debug, Clone)]
pub(crate) struct PeerParams {
    pub(crate) key: PeerKey,
    pub(crate) addr: SocketAddr,
    pub(crate) info_hash: InfoHash,
    pub(crate) our_id: PeerId,
    pub(crate) reserved: Reserved,
    pub(crate) connect_timeout: Duration,
    /// The uTP socket for this address family, if uTP is available.
    pub(crate) utp: Option<Arc<UtpSocket>>,
    pub(crate) prefer_utp: bool,
    pub(crate) encryption: Policy,
    /// What PEX flags or earlier connections say about the peer.
    pub(crate) hints: DialHints,
    /// The session-wide rate limits.
    pub(crate) rates: RateLimits,
    /// Dial through this proxy (TCP only) instead of directly.
    pub(crate) proxy: Option<Arc<crate::proxy::Proxy>>,
    /// DSCP mark for the TCP connection.
    pub(crate) dscp: Option<u8>,
    /// Dial over this network instead of sockets.
    pub(crate) network: Option<Arc<dyn crate::transport::Network>>,
}

/// Why a handshake failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HandshakeError {
    #[error("timed out")]
    Timeout,
    #[error("connection closed during handshake")]
    Eof,
    #[error("peer is serving a different torrent")]
    WrongInfoHash,
    #[error("connected to ourselves")]
    SelfConnection,
    #[error("torrent is too busy to take the connection")]
    Busy,
    #[error("peer did not encrypt, and encryption is required")]
    EncryptionRequired,
    #[error("encryption handshake failed: {0}")]
    Mse(#[from] MseError),
    #[error(transparent)]
    Wire(#[from] dravity_wire::WireError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl HandshakeError {
    /// Transient failures are dialed again. A peer that is the wrong torrent,
    /// ourselves, or speaking garbage is not.
    pub(crate) fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::Eof | Self::Io(_) | Self::Mse(MseError::Io(_))
        )
    }
}

/// Extensions we advertise: BEP 10 (metadata exchange, PEX), when enabled
/// the fast extension (BEP 6) and, for a torrent with a v2 info-hash, BEP
/// 52 (v2: hash requests).
///
/// The v2 bit is per torrent, as libtorrent sets it: libtorrent 2.x drops
/// a plaintext connection whose handshake sets it for a v1-only torrent.
pub(crate) fn our_reserved(fast: bool, v2: bool) -> Reserved {
    Reserved::new()
        .with_extension_protocol(true)
        .with_v2(v2)
        .with_fast(fast)
}

/// Writes our handshake and reads the peer's. Returns the peer's handshake
/// and any bytes that arrived after it.
pub(crate) async fn handshake<S>(
    stream: &mut S,
    info_hash: InfoHash,
    our_id: PeerId,
    reserved: Reserved,
) -> Result<(Handshake, BytesMut), HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(
            &Handshake::new(info_hash, our_id)
                .with_reserved(reserved)
                .to_bytes(),
        )
        .await?;
    let mut buf = BytesMut::with_capacity(1024);
    let theirs = loop {
        if let Some(hs) = Handshake::decode(&mut buf)? {
            break hs;
        }
        if stream.read_buf(&mut buf).await? == 0 {
            return Err(HandshakeError::Eof);
        }
    };
    if theirs.info_hash != info_hash {
        return Err(HandshakeError::WrongInfoHash);
    }
    if theirs.peer_id == our_id {
        return Err(HandshakeError::SelfConnection);
    }
    Ok((theirs, buf))
}

/// How far a dial attempt got before it failed.
#[derive(Debug)]
enum Failed {
    /// No connection at all (refused, unreachable, or timed out).
    Connect(HandshakeError),
    /// Connected, then the encryption or BitTorrent handshake failed.
    Handshake(HandshakeError),
}

/// Opens a raw connection over one transport.
async fn connect(transport: Transport, params: &PeerParams) -> std::io::Result<RawStream> {
    if let Some(network) = &params.network {
        return Ok(RawStream::Boxed(network.dial(params.addr).await?));
    }
    if let Some(proxy) = &params.proxy {
        let tcp = crate::proxy::connect(proxy, params.addr)
            .await
            .map_err(|e| match e {
                crate::proxy::ProxyError::Io(io) => io,
                other => std::io::Error::new(std::io::ErrorKind::ConnectionRefused, other),
            })?;
        tcp.set_nodelay(true)?;
        crate::qos::mark(&tcp, params.dscp, params.addr);
        return Ok(RawStream::Tcp(tcp));
    }
    Ok(match (transport, &params.utp) {
        (Transport::Utp, Some(utp)) => RawStream::Utp(utp.connect(params.addr).await?),
        _ => {
            let tcp = TcpStream::connect(params.addr).await?;
            tcp.set_nodelay(true)?;
            crate::qos::mark(&tcp, params.dscp, params.addr);
            RawStream::Tcp(tcp)
        }
    })
}

/// Connects over one transport, runs the MSE handshake if `encrypt`, then
/// the BitTorrent handshake, all within the connect timeout.
async fn open(
    transport: Transport,
    params: &PeerParams,
    encrypt: bool,
) -> Result<(PeerStream, Handshake, BytesMut), Failed> {
    let deadline = tokio::time::Instant::now() + params.connect_timeout;
    let raw = tokio::time::timeout_at(deadline, connect(transport, params))
        .await
        .map_err(|_| Failed::Connect(HandshakeError::Timeout))?
        .map_err(|e| Failed::Connect(e.into()))?;
    tokio::time::timeout_at(deadline, async {
        let mut stream = if encrypt {
            let methods = params.encryption.methods();
            let skey = params.info_hash.as_bytes();
            PeerStream::mse(dravity_mse::initiate(raw, skey, methods).await?)
        } else {
            PeerStream::plain(raw)
        };
        let (theirs, leftover) = handshake(
            &mut stream,
            params.info_hash,
            params.our_id,
            params.reserved,
        )
        .await?;
        Ok((stream, theirs, leftover))
    })
    .await
    .map_err(|_| Failed::Handshake(HandshakeError::Timeout))?
    .map_err(Failed::Handshake)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Tcp,
    Utp,
}

/// A successful dial.
struct Dialed {
    stream: PeerStream,
    theirs: Handshake,
    leftover: BytesMut,
    /// What the dial showed about MSE support: `Some(true)` if it completed,
    /// `Some(false)` if the peer refused it and took a plain handshake,
    /// `None` if MSE was not tried.
    mse: Option<bool>,
}

/// The transports to try, in order: uTP first when the peer is known to
/// speak it (PEX flag or an earlier uTP connection), TCP first when it is
/// known not to, otherwise as configured. The other one is the fallback.
const fn transports(utp: bool, hint: Option<bool>, prefer_utp: bool) -> &'static [Transport] {
    if !utp {
        return &[Transport::Tcp];
    }
    let utp_first = match hint {
        Some(known) => known,
        None => prefer_utp,
    };
    if utp_first {
        &[Transport::Utp, Transport::Tcp]
    } else {
        &[Transport::Tcp, Transport::Utp]
    }
}

/// Whether to open with MSE (`true`) or a plain handshake, and whether the
/// other may follow on the same transport if the peer accepts the
/// connection and then fails the first.
///
/// [`Policy::Enabled`] starts with MSE unless the peer is known not to do
/// it (refused it before, or PEX said so), and then falls back to the
/// other. [`Policy::Required`] and [`Policy::Disabled`] have no fallback.
fn encryption_order(policy: Policy, hint: Option<bool>) -> &'static [bool] {
    match policy {
        Policy::Disabled => &[false],
        Policy::Required => &[true],
        _ if hint == Some(false) => &[false, true],
        _ => &[true, false],
    }
}

/// Tries the preferred transport, then the other one if the first could
/// not connect at all (a peer that answered and refused is not retried).
/// On each transport, tries the encryption order of
/// [`encryption_order`]; each attempt gets the full connect timeout.
async fn dial(params: &PeerParams) -> Result<Dialed, HandshakeError> {
    let attempts = encryption_order(params.encryption, params.hints.encryption);
    let mut last = HandshakeError::Timeout;
    for &transport in transports(params.utp.is_some(), params.hints.utp, params.prefer_utp) {
        let mut refused_mse = false;
        let mut result = Err(Failed::Connect(HandshakeError::Timeout));
        for (i, &encrypt) in attempts.iter().enumerate() {
            result = open(transport, params, encrypt).await;
            match &result {
                Err(Failed::Handshake(e)) if i + 1 < attempts.len() => {
                    tracing::trace!(addr = %params.addr, ?transport, encrypt, error = %e, "dial failed; trying the other handshake");
                    refused_mse |= encrypt;
                }
                _ => break,
            }
        }
        match result {
            Ok((stream, theirs, leftover)) => {
                let mse = if stream.uses_mse() {
                    Some(true)
                } else if refused_mse {
                    Some(false)
                } else {
                    None
                };
                return Ok(Dialed {
                    stream,
                    theirs,
                    leftover,
                    mse,
                });
            }
            Err(Failed::Connect(e) | Failed::Handshake(e)) if !e.retryable() => return Err(e),
            Err(Failed::Connect(e) | Failed::Handshake(e)) => last = e,
        }
        tracing::trace!(addr = %params.addr, ?transport, error = %last, "dial attempt failed");
    }
    Err(last)
}

/// Connects, handshakes and runs the connection until either side closes.
///
/// `slot` is the connection's place under the session-wide cap, held until
/// the task ends.
pub(crate) async fn run(
    params: PeerParams,
    events: mpsc::Sender<Input>,
    commands: mpsc::Receiver<PeerCommand>,
    slot: ConnectionSlot,
) {
    let _slot = slot;
    let (key, addr) = (params.key, params.addr);
    let (stream, reserved, leftover, mse) = match dial(&params).await {
        Ok(Dialed {
            stream,
            theirs,
            leftover,
            mse,
        }) => {
            tracing::debug!(
                %addr,
                transport = stream.transport(),
                encrypted = stream.encrypted(),
                "peer connected"
            );
            (stream, theirs.reserved, leftover, mse)
        }
        Err(e) => {
            tracing::debug!(%addr, error = %e, "peer connection failed");
            let _ = events
                .send(Input::ConnectFailed {
                    key,
                    addr,
                    retry: e.retryable(),
                })
                .await;
            return;
        }
    };
    if events
        .send(Input::Connected {
            key,
            addr,
            reserved,
            transport: stream.kind(),
            encrypted: stream.encrypted(),
            mse,
        })
        .await
        .is_err()
    {
        return;
    }
    serve(
        key,
        addr,
        stream,
        leftover,
        &events,
        commands,
        &params.rates,
    )
    .await;
}

/// Runs a connection the listener accepted and handshook. The owner
/// already told the engine about it.
pub(crate) async fn run_accepted(
    (key, addr): (PeerKey, SocketAddr),
    stream: PeerStream,
    leftover: BytesMut,
    events: mpsc::Sender<Input>,
    commands: mpsc::Receiver<PeerCommand>,
    slot: ConnectionSlot,
    rates: RateLimits,
) {
    let _slot = slot;
    serve(key, addr, stream, leftover, &events, commands, &rates).await;
}

/// Payload bytes of a piece message (what the rate limits count).
fn payload(message: &Message) -> usize {
    match message {
        Message::Piece { data, .. } => data.len(),
        _ => 0,
    }
}

/// Moves messages between an established connection and the owner until
/// either side closes.
async fn serve<S>(
    key: PeerKey,
    addr: SocketAddr,
    stream: S,
    leftover: BytesMut,
    events: &mpsc::Sender<Input>,
    mut commands: mpsc::Receiver<PeerCommand>,
    rates: &RateLimits,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Bytes that arrived with the handshake must be decoded before the
    // next socket read: `from_parts` marks a non-empty buffer readable.
    let mut parts = FramedParts::new::<Message>(stream, PeerCodec::default());
    parts.read_buf = leftover;
    let mut framed = Framed::from_parts(parts);

    let reason: String = loop {
        tokio::select! {
            message = framed.next() => match message {
                Some(Ok(message)) => {
                    let bytes = payload(&message);
                    if events.send(Input::Message { key, message }).await.is_err() {
                        break "session closed".into();
                    }
                    // Holding the next read lets flow control slow the peer.
                    if bytes > 0 {
                        rates.down.acquire(bytes).await;
                    }
                }
                Some(Err(e)) => break e.to_string(),
                None => break "peer closed the connection".into(),
            },
            command = commands.recv() => match command {
                Some(PeerCommand::Send(message)) => {
                    // Batch everything already queued into one flush.
                    rates.up.acquire(payload(&message)).await;
                    let mut result = framed.feed(message).await;
                    while result.is_ok() {
                        match commands.try_recv() {
                            Ok(PeerCommand::Send(m)) => {
                                rates.up.acquire(payload(&m)).await;
                                result = framed.feed(m).await;
                            }
                            Err(_) => break,
                        }
                    }
                    if let Err(e) = result.and(framed.flush().await) {
                        break e.to_string();
                    }
                }
                // The owner dropped us (engine disconnect): no event needed.
                None => return,
            },
        }
    };
    tracing::debug!(%addr, %reason, "peer disconnected");
    let _ = events.send(Input::Disconnected { key }).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dial_order_follows_hints() {
        use Transport::{Tcp, Utp};
        assert_eq!(transports(false, Some(true), true), [Tcp]);
        assert_eq!(transports(true, None, true), [Utp, Tcp]);
        assert_eq!(transports(true, None, false), [Tcp, Utp]);
        assert_eq!(transports(true, Some(true), false), [Utp, Tcp]);
        assert_eq!(transports(true, Some(false), true), [Tcp, Utp]);

        assert_eq!(encryption_order(Policy::Disabled, Some(true)), [false]);
        assert_eq!(encryption_order(Policy::Required, Some(false)), [true]);
        assert_eq!(encryption_order(Policy::Enabled, None), [true, false]);
        assert_eq!(encryption_order(Policy::Enabled, Some(true)), [true, false]);
        assert_eq!(
            encryption_order(Policy::Enabled, Some(false)),
            [false, true]
        );
    }

    #[tokio::test]
    async fn handshake_checks_info_hash_and_self() {
        let ours = PeerId::new([1; 20]);
        let hash = InfoHash::new([2; 20]);

        let (mut a, mut b) = tokio::io::duplex(1024);
        let other = Handshake::new(InfoHash::new([3; 20]), PeerId::new([4; 20]));
        b.write_all(&other.to_bytes()).await.unwrap();
        assert!(matches!(
            handshake(&mut a, hash, ours, Reserved::new()).await,
            Err(HandshakeError::WrongInfoHash)
        ));

        let (mut a, mut b) = tokio::io::duplex(1024);
        b.write_all(&Handshake::new(hash, ours).to_bytes())
            .await
            .unwrap();
        assert!(matches!(
            handshake(&mut a, hash, ours, Reserved::new()).await,
            Err(HandshakeError::SelfConnection)
        ));

        let (mut a, mut b) = tokio::io::duplex(1024);
        let mut bytes = Handshake::new(hash, PeerId::new([9; 20]))
            .to_bytes()
            .to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 1, 1]); // unchoke right behind it
        b.write_all(&bytes).await.unwrap();
        let (hs, rest) = handshake(&mut a, hash, ours, Reserved::new())
            .await
            .unwrap();
        assert_eq!(hs.peer_id, PeerId::new([9; 20]));
        assert_eq!(&rest[..], &[0, 0, 0, 1, 1]);

        let (mut a, b) = tokio::io::duplex(1024);
        drop(b);
        assert!(
            handshake(&mut a, hash, ours, Reserved::new())
                .await
                .is_err()
        );
    }

    #[test]
    fn only_transient_handshake_errors_are_retried() {
        assert!(HandshakeError::Timeout.retryable());
        assert!(HandshakeError::Eof.retryable());
        assert!(HandshakeError::Io(std::io::Error::other("reset")).retryable());
        assert!(!HandshakeError::WrongInfoHash.retryable());
        assert!(!HandshakeError::SelfConnection.retryable());
        assert!(!HandshakeError::Wire(dravity_wire::WireError::InvalidProtocol).retryable());
    }

    /// Regression: a message sent in the same segment as the handshake must
    /// be delivered without waiting for more socket data.
    #[tokio::test]
    async fn delivers_message_coalesced_with_handshake() {
        let hash = InfoHash::new([5; 20]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut theirs = [0u8; 68];
            sock.read_exact(&mut theirs).await.unwrap();
            let mut out = Handshake::new(hash, PeerId::new([6; 20]))
                .to_bytes()
                .to_vec();
            out.extend_from_slice(&[0, 0, 0, 1, 1]);
            sock.write_all(&out).await.unwrap();
            // Keep the connection open without sending anything else.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let (events_tx, mut events) = mpsc::channel(8);
        let (_cmd_tx, cmd_rx) = mpsc::channel(8);
        let params = PeerParams {
            key: PeerKey(1),
            addr,
            info_hash: hash,
            our_id: PeerId::new([7; 20]),
            reserved: our_reserved(true, true),
            connect_timeout: Duration::from_secs(5),
            utp: None,
            prefer_utp: false,
            encryption: Policy::Disabled,
            hints: DialHints::default(),
            rates: RateLimits::new(None, None),
            proxy: None,
            dscp: None,
            network: None,
        };
        tokio::spawn(run(
            params,
            events_tx,
            cmd_rx,
            dravity_core::admit::ConnectionLimit::default()
                .try_admit()
                .unwrap(),
        ));
        assert!(matches!(events.recv().await, Some(Input::Connected { .. })));
        let next = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
        assert!(
            matches!(
                next,
                Ok(Some(Input::Message {
                    message: Message::Unchoke,
                    ..
                }))
            ),
            "{next:?}"
        );
        server.abort();
    }
}
