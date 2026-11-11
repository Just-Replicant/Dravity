//! Pluggable peer transport.
//!
//! By default the session dials and accepts peers over real TCP and uTP
//! sockets. A [`Network`] in [`crate::SessionConfig::network`] replaces
//! both for peer connections: every dial goes through
//! [`Network::dial`] and incoming peers come from the listener
//! [`Network::listen`] returns. [`MemoryNetwork`] is an in-process
//! implementation for deterministic multi-peer tests, with fault
//! injection (unreachable addresses, connections cut mid-transfer).
//!
//! Trackers, web seeds, the DHT and LSD keep their own sockets; disable
//! them when a whole swarm should run in memory.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::sync::mpsc;

/// A byte stream to a peer.
pub trait PeerIo: AsyncRead + AsyncWrite + Send + Unpin + fmt::Debug + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + fmt::Debug + 'static> PeerIo for T {}

/// A boxed peer stream.
pub type BoxedStream = Box<dyn PeerIo>;

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Accepts incoming peer streams.
pub trait TransportListener: Send + fmt::Debug {
    /// The address peers dial to reach us.
    fn local_addr(&self) -> SocketAddr;

    /// The next incoming stream and the address it came from.
    fn accept(&mut self) -> BoxFuture<'_, io::Result<(BoxedStream, SocketAddr)>>;
}

/// Opens peer streams and the listener for incoming ones.
pub trait Network: Send + Sync + fmt::Debug + 'static {
    /// Opens a stream to `addr`.
    fn dial(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<BoxedStream>>;

    /// Starts listening. Called once, when the session is created.
    ///
    /// # Errors
    /// If the network cannot listen.
    fn listen(&self) -> io::Result<Box<dyn TransportListener>>;
}

/// Buffer of each direction of an in-memory connection.
const MEMORY_BUFFER: usize = 256 * 1024;

type Incoming = mpsc::UnboundedSender<(DuplexStream, SocketAddr)>;

#[derive(Debug, Default)]
struct Registry {
    listeners: HashMap<SocketAddr, Incoming>,
    unreachable: HashSet<SocketAddr>,
    next_port: u16,
}

/// An in-process network: hosts are IP addresses, connections are
/// [`tokio::io::duplex`] pipes.
#[derive(Debug, Clone, Default)]
pub struct MemoryNetwork(Arc<Mutex<Registry>>);

impl MemoryNetwork {
    /// An empty network.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A host on this network at `ip`, to put in
    /// [`crate::SessionConfig::network`].
    #[must_use]
    pub fn host(&self, ip: IpAddr) -> Arc<dyn Network> {
        Arc::new(MemoryHost {
            net: self.clone(),
            ip,
        })
    }

    /// Makes dials to `addr` fail (`true`) or work again (`false`), as if
    /// it were behind a NAT or firewall.
    pub fn set_unreachable(&self, addr: SocketAddr, unreachable: bool) {
        let mut r = self.registry();
        if unreachable {
            r.unreachable.insert(addr);
        } else {
            r.unreachable.remove(&addr);
        }
    }

    fn port(&self) -> u16 {
        let mut r = self.registry();
        r.next_port = r.next_port.wrapping_add(1).max(1);
        // Above the well-known range, like ephemeral ports.
        10_000u16.saturating_add(r.next_port)
    }
}

#[derive(Debug)]
struct MemoryHost {
    net: MemoryNetwork,
    ip: IpAddr,
}

impl Network for MemoryHost {
    fn dial(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<BoxedStream>> {
        Box::pin(async move {
            let from = SocketAddr::new(self.ip, self.net.port());
            let target = {
                let r = self.net.registry();
                if r.unreachable.contains(&addr) {
                    None
                } else {
                    r.listeners.get(&addr).cloned()
                }
            };
            let Some(target) = target else {
                return Err(io::ErrorKind::ConnectionRefused.into());
            };
            let (ours, theirs) = tokio::io::duplex(MEMORY_BUFFER);
            target
                .send((theirs, from))
                .map_err(|_| io::Error::from(io::ErrorKind::ConnectionRefused))?;
            Ok(Box::new(ours) as BoxedStream)
        })
    }

    fn listen(&self) -> io::Result<Box<dyn TransportListener>> {
        let addr = SocketAddr::new(self.ip, self.net.port());
        let (tx, rx) = mpsc::unbounded_channel();
        let mut r = self.net.registry();
        if r.listeners.contains_key(&addr) {
            return Err(io::ErrorKind::AddrInUse.into());
        }
        r.listeners.insert(addr, tx);
        Ok(Box::new(MemoryListener {
            net: self.net.clone(),
            addr,
            rx,
        }))
    }
}

#[derive(Debug)]
struct MemoryListener {
    net: MemoryNetwork,
    addr: SocketAddr,
    rx: mpsc::UnboundedReceiver<(DuplexStream, SocketAddr)>,
}

impl TransportListener for MemoryListener {
    fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    fn accept(&mut self) -> BoxFuture<'_, io::Result<(BoxedStream, SocketAddr)>> {
        Box::pin(async move {
            let (stream, from) = self
                .rx
                .recv()
                .await
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
            Ok((Box::new(stream) as BoxedStream, from))
        })
    }
}

impl Drop for MemoryListener {
    fn drop(&mut self) {
        self.net.registry().listeners.remove(&self.addr);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn hosts_dial_listeners_and_faults_apply() {
        let net = MemoryNetwork::new();
        let a = net.host("10.0.0.1".parse().unwrap());
        let b = net.host("10.0.0.2".parse().unwrap());
        let mut listener = b.listen().unwrap();
        let addr = listener.local_addr();
        assert_eq!(addr.ip(), "10.0.0.2".parse::<IpAddr>().unwrap());

        let mut out = a.dial(addr).await.unwrap();
        let (mut inc, from) = listener.accept().await.unwrap();
        assert_eq!(from.ip(), "10.0.0.1".parse::<IpAddr>().unwrap());
        out.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        inc.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        net.set_unreachable(addr, true);
        assert!(a.dial(addr).await.is_err());
        net.set_unreachable(addr, false);
        assert!(a.dial(addr).await.is_ok());
        drop(listener);
        assert_eq!(
            a.dial(addr).await.err().map(|e| e.kind()),
            Some(io::ErrorKind::ConnectionRefused)
        );
    }
}
