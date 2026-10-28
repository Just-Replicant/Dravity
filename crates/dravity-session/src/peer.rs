//! One task per peer connection: handshake, then read → event and
//! command → batched write. The task never touches torrent state.
//!
//! Dialed peers are connected and handshaken here ([`run`]); accepted
//! peers arrive already handshaken from the listener ([`run_accepted`]).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use dravity_core::{InfoHash, PeerId};
use dravity_engine::PeerKey;
use dravity_utp::UtpSocket;
use dravity_wire::{Handshake, Message, Reserved};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, FramedParts};

use crate::codec::PeerCodec;
use crate::stream::PeerStream;
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
    #[error(transparent)]
    Wire(#[from] dravity_wire::WireError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl HandshakeError {
    /// Transient failures are dialed again. A peer that is the wrong torrent,
    /// ourselves, or speaking garbage is not.
    pub(crate) fn retryable(&self) -> bool {
        matches!(self, Self::Timeout | Self::Eof | Self::Io(_))
    }
}

/// Extensions we advertise: BEP 10 (metadata exchange, PEX) and, when
/// enabled, the fast extension (BEP 6).
pub(crate) fn our_reserved(fast: bool) -> Reserved {
    Reserved::new()
        .with_extension_protocol(true)
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

/// Opens a connection and handshakes over one transport.
async fn open(
    transport: Transport,
    params: &PeerParams,
) -> Result<(PeerStream, Handshake, BytesMut), HandshakeError> {
    let mut stream = match (transport, &params.utp) {
        (Transport::Utp, Some(utp)) => PeerStream::Utp(utp.connect(params.addr).await?),
        _ => {
            let tcp = TcpStream::connect(params.addr).await?;
            tcp.set_nodelay(true)?;
            PeerStream::Tcp(tcp)
        }
    };
    let (theirs, leftover) = handshake(
        &mut stream,
        params.info_hash,
        params.our_id,
        params.reserved,
    )
    .await?;
    Ok((stream, theirs, leftover))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Tcp,
    Utp,
}

/// Tries the preferred transport, then the other one if the first could
/// not connect at all (a peer that answered and refused is not retried).
/// Each attempt gets the full connect timeout.
async fn dial(params: &PeerParams) -> Result<(PeerStream, Handshake, BytesMut), HandshakeError> {
    let order = match (params.utp.is_some(), params.prefer_utp) {
        (false, _) => &[Transport::Tcp][..],
        (true, false) => &[Transport::Tcp, Transport::Utp][..],
        (true, true) => &[Transport::Utp, Transport::Tcp][..],
    };
    let mut last = HandshakeError::Timeout;
    for &transport in order {
        match tokio::time::timeout(params.connect_timeout, open(transport, params)).await {
            Ok(Ok(ok)) => return Ok(ok),
            Ok(Err(e)) if !e.retryable() => return Err(e),
            Ok(Err(e)) => last = e,
            Err(_) => last = HandshakeError::Timeout,
        }
        tracing::trace!(addr = %params.addr, ?transport, error = %last, "dial attempt failed");
    }
    Err(last)
}

/// Connects, handshakes and runs the connection until either side closes.
pub(crate) async fn run(
    params: PeerParams,
    events: mpsc::Sender<Input>,
    commands: mpsc::Receiver<PeerCommand>,
) {
    let (key, addr) = (params.key, params.addr);
    let (stream, reserved, leftover) = match dial(&params).await {
        Ok((stream, theirs, leftover)) => {
            tracing::debug!(%addr, transport = stream.transport(), "peer connected");
            (stream, theirs.reserved, leftover)
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
        })
        .await
        .is_err()
    {
        return;
    }
    serve(key, addr, stream, leftover, &events, commands).await;
}

/// Runs a connection the listener accepted and handshook. The owner
/// already told the engine about it.
pub(crate) async fn run_accepted(
    key: PeerKey,
    addr: SocketAddr,
    stream: PeerStream,
    leftover: BytesMut,
    events: mpsc::Sender<Input>,
    commands: mpsc::Receiver<PeerCommand>,
) {
    serve(key, addr, stream, leftover, &events, commands).await;
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
                    if events.send(Input::Message { key, message }).await.is_err() {
                        break "session closed".into();
                    }
                }
                Some(Err(e)) => break e.to_string(),
                None => break "peer closed the connection".into(),
            },
            command = commands.recv() => match command {
                Some(PeerCommand::Send(message)) => {
                    // Batch everything already queued into one flush.
                    let mut result = framed.feed(message).await;
                    while result.is_ok() {
                        match commands.try_recv() {
                            Ok(PeerCommand::Send(m)) => result = framed.feed(m).await,
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
            reserved: our_reserved(true),
            connect_timeout: Duration::from_secs(5),
            utp: None,
            prefer_utp: false,
        };
        tokio::spawn(run(params, events_tx, cmd_rx));
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
