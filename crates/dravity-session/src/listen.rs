//! Accepting incoming peer connections.
//!
//! The session binds one TCP socket per address family (IPv6 with
//! `IPV6_V6ONLY`, so both families can share a port on every platform),
//! and with uTP enabled a UDP socket on the same port for each. One accept
//! task per socket reads the remote handshake, routes the connection
//! to the torrent named by its info-hash, answers with our handshake and
//! hands the stream to that torrent's task, which runs it like a dialed
//! peer. Unknown info-hashes, connections from ourselves and handshakes that
//! do not finish in time are dropped. The number of handshakes in progress
//! is bounded by [`ListenConfig::max_pending`].

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::BytesMut;
use dravity_core::{InfoHash, PeerId};
use dravity_utp::UtpSocket;
use dravity_wire::{Handshake, Reserved};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::peer::HandshakeError;
use crate::stream::PeerStream;
use crate::torrent::{Accepted, Command};

/// Where to accept incoming peer connections.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListenConfig {
    /// TCP port for both address families; 0 picks a free port. If the port
    /// is taken, a free port is used instead (and reported to trackers).
    pub port: u16,
    /// IPv4 address to bind; `None` disables IPv4.
    pub ipv4: Option<Ipv4Addr>,
    /// IPv6 address to bind; `None` disables IPv6. A host without IPv6
    /// only logs the failure.
    pub ipv6: Option<Ipv6Addr>,
    /// Incoming handshakes in progress at once; more connections are closed
    /// right away.
    pub max_pending: usize,
    /// Also speak uTP (BEP 29) on UDP sockets bound to the same port;
    /// `None` uses TCP only.
    pub utp: Option<dravity_utp::Config>,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            port: 6881,
            ipv4: Some(Ipv4Addr::UNSPECIFIED),
            ipv6: Some(Ipv6Addr::UNSPECIFIED),
            max_pending: 32,
            utp: Some(dravity_utp::Config::default()),
        }
    }
}

impl ListenConfig {
    /// Listen on `port` on every interface.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            port,
            ..Self::default()
        }
    }

    /// Listen on loopback only (`127.0.0.1` and `::1`); for tests.
    #[must_use]
    pub fn loopback(port: u16) -> Self {
        Self {
            port,
            ipv4: Some(Ipv4Addr::LOCALHOST),
            ipv6: Some(Ipv6Addr::LOCALHOST),
            ..Self::default()
        }
    }
}

/// Bound, not yet accepting, sockets.
#[derive(Debug)]
pub(crate) struct Bound {
    pub(crate) sockets: Vec<std::net::TcpListener>,
    /// uTP sockets, on the TCP port of their family.
    pub(crate) udp: Vec<std::net::UdpSocket>,
    pub(crate) utp: dravity_utp::Config,
    /// The port every socket is bound to.
    pub(crate) port: u16,
}

/// Binds the sockets of `config`.
///
/// IPv4 is bound first; IPv6 then uses the same port. A taken port falls
/// back to a free one.
///
/// # Errors
/// The bind error if no socket could be bound.
pub(crate) fn bind(config: &ListenConfig) -> io::Result<Bound> {
    let mut sockets = Vec::new();
    let mut port = config.port;
    let mut first_error = None;
    let addrs = config
        .ipv4
        .map(|ip| SocketAddr::from((ip, 0)))
        .into_iter()
        .chain(config.ipv6.map(|ip| SocketAddr::from((ip, 0))));
    for mut addr in addrs {
        addr.set_port(port);
        let bound = match bind_one(addr) {
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && sockets.is_empty() && port != 0 => {
                tracing::warn!(port, "listen port in use, using a free port");
                addr.set_port(0);
                bind_one(addr)
            }
            other => other,
        };
        match bound {
            Ok(socket) => {
                port = socket.local_addr()?.port();
                sockets.push(socket);
            }
            Err(e) => {
                tracing::warn!(%addr, error = %e, "cannot listen");
                first_error.get_or_insert(e);
            }
        }
    }
    if sockets.is_empty() {
        return Err(first_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address to bind")));
    }
    let mut udp = Vec::new();
    if config.utp.is_some() {
        for tcp in &sockets {
            let addr = tcp.local_addr()?;
            match bind_udp(addr) {
                Ok(socket) => udp.push(socket),
                // TCP still works; this family just has no uTP.
                Err(e) => tracing::warn!(%addr, error = %e, "cannot bind uTP socket"),
            }
        }
    }
    Ok(Bound {
        sockets,
        udp,
        utp: config.utp.clone().unwrap_or_default(),
        port,
    })
}

fn bind_udp(addr: SocketAddr) -> io::Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

/// The session's uTP sockets, one per address family.
#[derive(Debug, Clone, Default)]
pub(crate) struct UtpSockets {
    v4: Option<Arc<UtpSocket>>,
    v6: Option<Arc<UtpSocket>>,
}

impl UtpSockets {
    /// The socket that can reach `addr`.
    pub(crate) fn for_addr(&self, addr: SocketAddr) -> Option<&Arc<UtpSocket>> {
        if addr.is_ipv4() {
            self.v4.as_ref()
        } else {
            self.v6.as_ref()
        }
    }
}

fn bind_one(addr: SocketAddr) -> io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    // Rebinding right after a restart must not wait for TIME_WAIT. Windows'
    // SO_REUSEADDR would let another process steal the port instead.
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    Ok(socket.into())
}

/// Torrents incoming connections can be routed to.
pub(crate) type Registry = Arc<Mutex<HashMap<InfoHash, mpsc::Sender<Command>>>>;

/// What an accept task needs.
#[derive(Debug, Clone)]
pub(crate) struct Acceptor {
    pub(crate) registry: Registry,
    pub(crate) our_id: PeerId,
    pub(crate) reserved: Reserved,
    pub(crate) handshake_timeout: Duration,
    pub(crate) pending: Arc<Semaphore>,
}

/// Starts one accept task per socket; they end when `stop` is cancelled.
/// Returns the uTP sockets for dialing.
///
/// # Errors
/// If a socket cannot be registered with the Tokio runtime.
pub(crate) fn start(
    bound: Bound,
    acceptor: &Acceptor,
    stop: &CancellationToken,
) -> io::Result<UtpSockets> {
    for socket in bound.sockets {
        let listener = TcpListener::from_std(socket)?;
        tracing::info!(addr = %listener.local_addr()?, "accepting peers over TCP");
        tokio::spawn(accept_loop(listener, acceptor.clone(), stop.clone()));
    }
    let mut utp = UtpSockets::default();
    for socket in bound.udp {
        let socket = Arc::new(UtpSocket::from_std(socket, bound.utp.clone())?);
        let addr = socket.local_addr();
        tracing::info!(%addr, "accepting peers over uTP");
        tokio::spawn(accept_utp(
            Arc::clone(&socket),
            acceptor.clone(),
            stop.clone(),
        ));
        if addr.is_ipv4() {
            utp.v4 = Some(socket);
        } else {
            utp.v6 = Some(socket);
        }
    }
    Ok(utp)
}

async fn accept_utp(socket: Arc<UtpSocket>, acceptor: Acceptor, stop: CancellationToken) {
    loop {
        let accepted = tokio::select! {
            () = stop.cancelled() => return,
            accepted = socket.accept() => accepted,
        };
        let Ok((stream, addr)) = accepted else {
            return;
        };
        handle(PeerStream::Utp(stream), addr, &acceptor);
    }
}

/// Handshakes and routes one accepted connection in its own task, within
/// the pending-handshake budget.
fn handle(stream: PeerStream, addr: SocketAddr, acceptor: &Acceptor) {
    let Ok(permit) = Arc::clone(&acceptor.pending).try_acquire_owned() else {
        tracing::debug!(%addr, "too many incoming handshakes; closing");
        return;
    };
    let acceptor = acceptor.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let transport = stream.transport();
        if let Err(e) = route(stream, addr, &acceptor).await {
            tracing::debug!(%addr, transport, error = %e, "incoming connection dropped");
        }
    });
}

async fn accept_loop(listener: TcpListener, acceptor: Acceptor, stop: CancellationToken) {
    loop {
        let accepted = tokio::select! {
            () = stop.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let (stream, addr) = match accepted {
            Ok(ok) => ok,
            Err(e) => {
                // Usually EMFILE or a connection reset before accept; back
                // off briefly instead of spinning.
                tracing::debug!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if let Err(e) = stream.set_nodelay(true) {
            tracing::debug!(%addr, error = %e, "set_nodelay failed");
        }
        handle(PeerStream::Tcp(stream), addr, &acceptor);
    }
}

/// Handshakes an accepted connection and hands it to its torrent.
async fn route(
    mut stream: PeerStream,
    addr: SocketAddr,
    acceptor: &Acceptor,
) -> Result<(), HandshakeError> {
    let lookup = |hash: &InfoHash| {
        acceptor
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(hash)
            .filter(|tx| !tx.is_closed())
            .cloned()
    };
    let (theirs, leftover, torrent) = tokio::time::timeout(
        acceptor.handshake_timeout,
        accept_handshake(&mut stream, acceptor.our_id, acceptor.reserved, lookup),
    )
    .await
    .map_err(|_| HandshakeError::Timeout)??;
    let accepted = Accepted {
        stream,
        addr,
        reserved: theirs.reserved,
        leftover,
    };
    // Never wait on a busy torrent: the peer can connect again later.
    torrent
        .try_send(Command::Incoming(accepted))
        .map_err(|_| HandshakeError::Busy)
}

/// Reads the remote handshake, finds its torrent with `lookup`, then sends
/// ours. Returns the remote handshake, bytes after it and the torrent.
pub(crate) async fn accept_handshake<S, T>(
    stream: &mut S,
    our_id: PeerId,
    reserved: Reserved,
    lookup: impl Fn(&InfoHash) -> Option<T>,
) -> Result<(Handshake, BytesMut, T), HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = BytesMut::with_capacity(1024);
    let theirs = loop {
        if let Some(hs) = Handshake::decode(&mut buf)? {
            break hs;
        }
        if stream.read_buf(&mut buf).await? == 0 {
            return Err(HandshakeError::Eof);
        }
    };
    let torrent = lookup(&theirs.info_hash).ok_or(HandshakeError::WrongInfoHash)?;
    if theirs.peer_id == our_id {
        return Err(HandshakeError::SelfConnection);
    }
    stream
        .write_all(
            &Handshake::new(theirs.info_hash, our_id)
                .with_reserved(reserved)
                .to_bytes(),
        )
        .await?;
    Ok((theirs, buf, torrent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accept_handshake_routes_known_torrents_only() {
        let ours = PeerId::new([1; 20]);
        let known = InfoHash::new([2; 20]);
        let lookup = |h: &InfoHash| (*h == known).then_some(7u8);

        let (mut a, mut b) = tokio::io::duplex(1024);
        let mut hello = Handshake::new(known, PeerId::new([3; 20]))
            .to_bytes()
            .to_vec();
        hello.extend_from_slice(&[0, 0, 0, 1, 2]); // interested, coalesced
        b.write_all(&hello).await.unwrap();
        let (hs, rest, torrent) = accept_handshake(&mut a, ours, Reserved::new(), lookup)
            .await
            .unwrap();
        assert_eq!((hs.peer_id, torrent), (PeerId::new([3; 20]), 7));
        assert_eq!(&rest[..], &[0, 0, 0, 1, 2]);
        let mut reply = BytesMut::new();
        while Handshake::decode(&mut reply).unwrap().is_none() {
            b.read_buf(&mut reply).await.unwrap();
        }

        let (mut a, mut b) = tokio::io::duplex(1024);
        b.write_all(&Handshake::new(InfoHash::new([9; 20]), PeerId::new([3; 20])).to_bytes())
            .await
            .unwrap();
        assert!(matches!(
            accept_handshake(&mut a, ours, Reserved::new(), lookup).await,
            Err(HandshakeError::WrongInfoHash)
        ));

        let (mut a, mut b) = tokio::io::duplex(1024);
        b.write_all(&Handshake::new(known, ours).to_bytes())
            .await
            .unwrap();
        assert!(matches!(
            accept_handshake(&mut a, ours, Reserved::new(), lookup).await,
            Err(HandshakeError::SelfConnection)
        ));
    }

    #[test]
    fn binds_both_families_on_one_port_and_falls_back_when_taken() {
        let first = bind(&ListenConfig::loopback(0)).unwrap();
        assert!(first.port != 0);
        let ports: Vec<u16> = first
            .sockets
            .iter()
            .map(|s| s.local_addr().unwrap().port())
            .collect();
        assert!(ports.iter().all(|p| *p == first.port), "{ports:?}");
        // The same port again: a free one is used instead.
        let second = bind(&ListenConfig {
            ipv6: None,
            ..ListenConfig::loopback(first.port)
        })
        .unwrap();
        assert_ne!(second.port, first.port);
        assert!(
            bind(&ListenConfig {
                ipv4: None,
                ipv6: None,
                ..ListenConfig::default()
            })
            .is_err()
        );
    }
}
