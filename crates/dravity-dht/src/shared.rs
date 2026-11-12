//! One UDP socket shared by the DHT, uTP and UDP tracker clients.
//!
//! Incoming datagrams are classified with [`crate::demux::classify`] and
//! pushed onto the matching branch. Each branch can also send through the
//! same socket. The receive task stops when the last [`SharedUdp`] handle
//! is dropped.
//!
//! The session does not yet bind DHT, uTP and UDP trackers to one of
//! these sockets (they still use a socket each, usually on the same
//! port via `SO_REUSEADDR`). This type is the building block for that
//! wiring.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::demux::{Datagram, classify};

/// Capacity of each classified receive queue.
const BRANCH_QUEUE: usize = 256;

/// Largest UDP payload we accept (theoretical IPv4 maximum).
const MAX_DATAGRAM: usize = 65_535;

/// A UDP socket whose incoming datagrams are split into DHT, uTP and
/// tracker branches.
#[derive(Debug)]
pub struct SharedUdp {
    socket: Arc<UdpSocket>,
    _recv: AbortOnDrop,
}

#[derive(Debug)]
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The three classified sides of a [`SharedUdp`].
#[derive(Debug)]
pub struct Branches {
    /// Datagrams classified as KRPC.
    pub dht: UdpBranch,
    /// Datagrams classified as uTP.
    pub utp: UdpBranch,
    /// Datagrams classified as BEP 15 UDP tracker responses.
    pub tracker: UdpBranch,
}

/// One classified stream over a shared socket.
#[derive(Debug)]
pub struct UdpBranch {
    socket: Arc<UdpSocket>,
    rx: mpsc::Receiver<(Vec<u8>, SocketAddr)>,
}

impl SharedUdp {
    /// Wraps an already bound socket and starts the receive task.
    #[must_use]
    pub fn wrap(socket: UdpSocket) -> (Self, Branches) {
        let socket = Arc::new(socket);
        let (dht_tx, dht_rx) = mpsc::channel(BRANCH_QUEUE);
        let (utp_tx, utp_rx) = mpsc::channel(BRANCH_QUEUE);
        let (tracker_tx, tracker_rx) = mpsc::channel(BRANCH_QUEUE);
        let recv = tokio::spawn(recv_loop(Arc::clone(&socket), dht_tx, utp_tx, tracker_tx));
        let branch = |rx| UdpBranch {
            socket: Arc::clone(&socket),
            rx,
        };
        (
            Self {
                socket: Arc::clone(&socket),
                _recv: AbortOnDrop(recv),
            },
            Branches {
                dht: branch(dht_rx),
                utp: branch(utp_rx),
                tracker: branch(tracker_rx),
            },
        )
    }

    /// Binds `addr` and starts demultiplexing.
    ///
    /// # Errors
    /// If the socket cannot be bound.
    pub async fn bind(addr: SocketAddr) -> io::Result<(Self, Branches)> {
        Ok(Self::wrap(UdpSocket::bind(addr).await?))
    }

    /// The local address of the shared socket.
    ///
    /// # Errors
    /// If the socket has been closed.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

impl UdpBranch {
    /// The local address of the shared socket.
    ///
    /// # Errors
    /// If the socket has been closed.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Receives the next datagram classified for this branch.
    ///
    /// # Errors
    /// [`io::ErrorKind::NotConnected`] if the shared socket has been
    /// dropped and the queue is empty.
    pub async fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (data, from) = self.rx.recv().await.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "shared UDP socket is closed")
        })?;
        let n = data.len().min(buf.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), data.get(..n)) {
            dst.copy_from_slice(src);
        }
        Ok((n, from))
    }

    /// Sends `buf` to `addr` on the shared socket.
    ///
    /// # Errors
    /// The underlying socket's send error.
    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.socket.send_to(buf, addr).await
    }
}

async fn recv_loop(
    socket: Arc<UdpSocket>,
    dht: mpsc::Sender<(Vec<u8>, SocketAddr)>,
    utp: mpsc::Sender<(Vec<u8>, SocketAddr)>,
    tracker: mpsc::Sender<(Vec<u8>, SocketAddr)>,
) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(pair) => pair,
            // A transient receive error (ICMP on a connected path, etc.)
            // must not kill the demultiplexer.
            Err(_) => continue,
        };
        let Some(data) = buf.get(..n) else {
            continue;
        };
        let tx = match classify(data) {
            Datagram::Dht => &dht,
            Datagram::Utp => &utp,
            Datagram::Tracker => &tracker,
            Datagram::Unknown => continue,
        };
        // A full branch queue drops the datagram, as a real UDP socket
        // would when its receive buffer is full.
        let _ = tx.try_send((data.to_vec(), from));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn pair() -> (UdpSocket, (SharedUdp, Branches)) {
        let shared = SharedUdp::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        (peer, shared)
    }

    #[tokio::test]
    async fn datagrams_reach_the_matching_branch() {
        let (peer, (shared, mut branches)) = pair().await;
        let to = shared.local_addr().unwrap();

        let krpc = b"de";
        peer.send_to(krpc, to).await.unwrap();
        let mut buf = [0u8; 64];
        let (n, from) =
            tokio::time::timeout(Duration::from_secs(2), branches.dht.recv_from(&mut buf))
                .await
                .expect("dht")
                .unwrap();
        assert_eq!(&buf[..n], krpc);
        assert_eq!(from, peer.local_addr().unwrap());

        let mut utp = [0u8; 20];
        utp[0] = 0x41;
        peer.send_to(&utp, to).await.unwrap();
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), branches.utp.recv_from(&mut buf))
            .await
            .expect("utp")
            .unwrap();
        assert_eq!(&buf[..n], &utp);

        let tracker = [0, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0];
        peer.send_to(&tracker, to).await.unwrap();
        let (n, _) =
            tokio::time::timeout(Duration::from_secs(2), branches.tracker.recv_from(&mut buf))
                .await
                .expect("tracker")
                .unwrap();
        assert_eq!(&buf[..n], &tracker);

        // Garbage is dropped, not delivered to any branch.
        peer.send_to(&[0xff; 8], to).await.unwrap();
        let leftover =
            tokio::time::timeout(Duration::from_millis(50), branches.dht.recv_from(&mut buf)).await;
        assert!(leftover.is_err(), "unknown datagrams must be dropped");
    }

    #[tokio::test]
    async fn a_branch_sends_through_the_shared_socket() {
        let (peer, (shared, branches)) = pair().await;
        let to = peer.local_addr().unwrap();
        let payload = b"hello-shared";
        branches.dht.send_to(payload, to).await.unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("reply")
            .unwrap();
        assert_eq!(&buf[..n], payload);
        assert_eq!(from, shared.local_addr().unwrap());
    }
}
