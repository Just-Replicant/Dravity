//! One task per peer connection: handshake, then read → event and
//! command → batched write. The task never touches torrent state.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::BytesMut;
use dravity_core::{InfoHash, PeerId};
use dravity_engine::PeerKey;
use dravity_wire::{Handshake, Message, Reserved};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, FramedParts};

use crate::codec::PeerCodec;
use crate::torrent::Input;

/// Commands from the torrent owner to a peer task.
#[derive(Debug)]
pub(crate) enum PeerCommand {
    Send(Message),
}

/// Static parameters of a peer connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PeerParams {
    pub(crate) key: PeerKey,
    pub(crate) addr: SocketAddr,
    pub(crate) info_hash: InfoHash,
    pub(crate) our_id: PeerId,
    pub(crate) connect_timeout: Duration,
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

/// Extensions we advertise: BEP 10 (metadata exchange, PEX).
pub(crate) fn our_reserved() -> Reserved {
    Reserved::new().with_extension_protocol(true)
}

/// Writes our handshake and reads the peer's. Returns the peer's handshake
/// and any bytes that arrived after it.
pub(crate) async fn handshake<S>(
    stream: &mut S,
    info_hash: InfoHash,
    our_id: PeerId,
) -> Result<(Handshake, BytesMut), HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(
            &Handshake::new(info_hash, our_id)
                .with_reserved(our_reserved())
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

/// Connects, handshakes and runs the connection until either side closes.
pub(crate) async fn run(
    params: PeerParams,
    events: mpsc::Sender<Input>,
    mut commands: mpsc::Receiver<PeerCommand>,
) {
    let PeerParams {
        key,
        addr,
        info_hash,
        our_id,
        connect_timeout,
    } = params;
    let setup = async {
        let mut stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (theirs, leftover) = handshake(&mut stream, info_hash, our_id).await?;
        Ok::<_, HandshakeError>((stream, theirs.reserved, leftover))
    };
    let (stream, reserved, leftover) = match tokio::time::timeout(connect_timeout, setup).await {
        Ok(Ok(ok)) => ok,
        Ok(Err(e)) => {
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
        Err(_) => {
            tracing::debug!(%addr, error = %HandshakeError::Timeout, "peer connection failed");
            let _ = events
                .send(Input::ConnectFailed {
                    key,
                    addr,
                    retry: true,
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
            handshake(&mut a, hash, ours).await,
            Err(HandshakeError::WrongInfoHash)
        ));

        let (mut a, mut b) = tokio::io::duplex(1024);
        b.write_all(&Handshake::new(hash, ours).to_bytes())
            .await
            .unwrap();
        assert!(matches!(
            handshake(&mut a, hash, ours).await,
            Err(HandshakeError::SelfConnection)
        ));

        let (mut a, mut b) = tokio::io::duplex(1024);
        let mut bytes = Handshake::new(hash, PeerId::new([9; 20]))
            .to_bytes()
            .to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 1, 1]); // unchoke right behind it
        b.write_all(&bytes).await.unwrap();
        let (hs, rest) = handshake(&mut a, hash, ours).await.unwrap();
        assert_eq!(hs.peer_id, PeerId::new([9; 20]));
        assert_eq!(&rest[..], &[0, 0, 0, 1, 1]);

        let (mut a, b) = tokio::io::duplex(1024);
        drop(b);
        assert!(handshake(&mut a, hash, ours).await.is_err());
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
            connect_timeout: Duration::from_secs(5),
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
