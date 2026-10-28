//! A Tokio UDP socket multiplexing uTP connections.
//!
//! One driver task per [`UtpSocket`] owns the UDP socket: it routes
//! datagrams to connections by `(remote address, connection id)`, runs
//! their timers and sends what they produce. [`UtpStream`]s share their
//! [`Connection`] with the driver behind a mutex and wake it when they
//! write. The driver ends once the socket and every stream are dropped and
//! their connections have closed.

use std::collections::HashMap;
use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc};

use crate::conn::{Config, ConnError, Connection};
use crate::packet::{Packet, PacketType};

/// Largest datagram read.
const MAX_DATAGRAM: usize = 64 * 1024;

type Key = (SocketAddr, u16);

/// A connection shared between its stream and the driver.
#[derive(Debug)]
struct Slot {
    conn: Connection,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    /// The stream (or the pending connect) is gone.
    detached: bool,
}

impl Slot {
    fn wake(&mut self) {
        let failed = self.conn.is_closed();
        if self.conn.readable() > 0 || self.conn.is_eof() || failed {
            if let Some(w) = self.read_waker.take() {
                w.wake();
            }
        }
        if self.conn.writable() > 0 || self.conn.is_connected() || failed {
            if let Some(w) = self.write_waker.take() {
                w.wake();
            }
        }
    }
}

type SlotRef = Arc<Mutex<Slot>>;

fn lock(slot: &SlotRef) -> MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
struct Shared {
    udp: UdpSocket,
    config: Config,
    conns: Mutex<HashMap<Key, SlotRef>>,
    /// Connections with something to send since the driver last looked.
    dirty: Mutex<Vec<Key>>,
    wake: Notify,
    /// The [`UtpSocket`] was dropped: accept no more connections.
    closed: AtomicBool,
}

impl Shared {
    fn conns(&self) -> MutexGuard<'_, HashMap<Key, SlotRef>> {
        self.conns.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn mark(&self, key: Key) {
        self.dirty
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(key);
        self.wake.notify_one();
    }
}

/// A UDP socket speaking uTP.
#[derive(Debug)]
pub struct UtpSocket {
    shared: Arc<Shared>,
    incoming: tokio::sync::Mutex<mpsc::Receiver<(UtpStream, SocketAddr)>>,
    local_addr: SocketAddr,
}

impl UtpSocket {
    /// Binds `addr` and starts the driver. Must be called in a Tokio
    /// runtime.
    ///
    /// # Errors
    /// The bind error.
    pub async fn bind(addr: SocketAddr, config: Config) -> io::Result<Self> {
        Self::from_udp(UdpSocket::bind(addr).await?, config)
    }

    /// Uses an already bound (non-blocking) socket.
    ///
    /// # Errors
    /// If the socket cannot be registered with the runtime.
    pub fn from_std(socket: std::net::UdpSocket, config: Config) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Self::from_udp(UdpSocket::from_std(socket)?, config)
    }

    fn from_udp(udp: UdpSocket, config: Config) -> io::Result<Self> {
        let local_addr = udp.local_addr()?;
        let shared = Arc::new(Shared {
            udp,
            config,
            conns: Mutex::new(HashMap::new()),
            dirty: Mutex::new(Vec::new()),
            wake: Notify::new(),
            closed: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(drive(Arc::clone(&shared), tx));
        Ok(Self {
            shared,
            incoming: tokio::sync::Mutex::new(rx),
            local_addr,
        })
    }

    /// The bound address.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Opens a connection to `addr`; resolves once the remote answered the
    /// SYN.
    ///
    /// # Errors
    /// [`io::ErrorKind::TimedOut`] if it never answered,
    /// [`io::ErrorKind::ConnectionRefused`] if it reset the connection.
    pub async fn connect(&self, addr: SocketAddr) -> io::Result<UtpStream> {
        let (key, slot) = {
            let mut conns = self.shared.conns();
            let mut id = fastrand::u16(..);
            while conns.contains_key(&(addr, id)) {
                id = fastrand::u16(..);
            }
            let conn = Connection::connect(self.shared.config.clone(), Instant::now(), id);
            let slot = Arc::new(Mutex::new(Slot {
                conn,
                read_waker: None,
                write_waker: None,
                detached: false,
            }));
            conns.insert((addr, id), Arc::clone(&slot));
            ((addr, id), slot)
        };
        let stream = UtpStream {
            shared: Arc::clone(&self.shared),
            key,
            slot,
        };
        self.shared.mark(key);
        poll_fn(|cx| {
            let mut slot = lock(&stream.slot);
            if slot.conn.is_connected() {
                return Poll::Ready(Ok(()));
            }
            if let Some(e) = slot.conn.error() {
                return Poll::Ready(Err(match e {
                    ConnError::Reset => io::Error::from(io::ErrorKind::ConnectionRefused),
                    ConnError::TimedOut => io::Error::from(io::ErrorKind::TimedOut),
                }));
            }
            slot.write_waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await?;
        Ok(stream)
    }

    /// Waits for the next incoming connection.
    ///
    /// # Errors
    /// If the driver stopped.
    pub async fn accept(&self) -> io::Result<(UtpStream, SocketAddr)> {
        self.incoming
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| io::Error::other("uTP socket driver stopped"))
    }
}

impl Drop for UtpSocket {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.wake.notify_one();
    }
}

/// One uTP connection, as a byte stream.
#[derive(Debug)]
pub struct UtpStream {
    shared: Arc<Shared>,
    key: Key,
    slot: SlotRef,
}

impl UtpStream {
    /// The remote address.
    #[must_use]
    pub const fn peer_addr(&self) -> SocketAddr {
        self.key.0
    }
}

fn conn_error(e: ConnError) -> io::Error {
    match e {
        ConnError::Reset => io::Error::from(io::ErrorKind::ConnectionReset),
        ConnError::TimedOut => io::Error::from(io::ErrorKind::TimedOut),
    }
}

impl AsyncRead for UtpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut slot = lock(&self.slot);
        if slot.conn.readable() > 0 {
            let n = slot.conn.read(buf.initialize_unfilled());
            buf.advance(n);
            drop(slot);
            // A window update may be due.
            self.shared.mark(self.key);
            return Poll::Ready(Ok(()));
        }
        if slot.conn.is_eof() {
            return Poll::Ready(Ok(()));
        }
        if let Some(e) = slot.conn.error() {
            return Poll::Ready(Err(conn_error(e)));
        }
        if slot.conn.is_closed() {
            return Poll::Ready(Ok(()));
        }
        slot.read_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut slot = lock(&self.slot);
        if let Some(e) = slot.conn.error() {
            return Poll::Ready(Err(conn_error(e)));
        }
        if slot.conn.is_closed() {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        let n = slot.conn.write(buf);
        if n == 0 && !buf.is_empty() {
            slot.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        drop(slot);
        self.shared.mark(self.key);
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Written bytes are already queued with the connection, which sends
        // them as its windows allow.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        lock(&self.slot).conn.close();
        self.shared.mark(self.key);
        Poll::Ready(Ok(()))
    }
}

impl Drop for UtpStream {
    fn drop(&mut self) {
        let mut slot = lock(&self.slot);
        slot.conn.close();
        slot.detached = true;
        drop(slot);
        self.shared.mark(self.key);
    }
}

/// The driver: datagrams in, timers, datagrams out.
async fn drive(shared: Arc<Shared>, accepted: mpsc::Sender<(UtpStream, SocketAddr)>) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut out: Vec<(SocketAddr, Bytes)> = Vec::new();
    loop {
        let deadline = next_deadline(&shared);
        let sleep = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at.into()).await,
                None => std::future::pending().await,
            }
        };
        let mut touched: Vec<Key> = Vec::new();
        tokio::select! {
            received = shared.udp.recv_from(&mut buf) => match received {
                Ok((len, from)) => {
                    let data = buf.get(..len).unwrap_or_default();
                    if let Some(key) = on_datagram(&shared, &accepted, data, from, &mut out) {
                        touched.push(key);
                    }
                }
                // ICMP errors (Windows reports them on recv) and the like
                // concern one remote, not the socket.
                Err(e) => tracing::trace!(error = %e, "uTP recv failed"),
            },
            () = shared.wake.notified() => {}
            () = sleep => {}
        }
        touched.append(&mut shared.dirty.lock().unwrap_or_else(PoisonError::into_inner));
        let now = Instant::now();
        let slots: Vec<(Key, SlotRef)> = {
            let conns = shared.conns();
            // Timers: every connection whose deadline passed.
            for (key, slot) in conns.iter() {
                if lock(slot).conn.poll_timeout().is_some_and(|t| t <= now) {
                    touched.push(*key);
                }
            }
            touched.sort_unstable();
            touched.dedup();
            touched
                .iter()
                .filter_map(|k| conns.get(k).map(|s| (*k, Arc::clone(s))))
                .collect()
        };
        let mut finished = Vec::new();
        for (key, slot) in slots {
            let mut slot = lock(&slot);
            slot.conn.on_timeout(now);
            while let Some(packet) = slot.conn.poll_transmit(now) {
                out.push((key.0, packet.encode()));
            }
            slot.wake();
            // Closed and done lingering, or abandoned with everything sent
            // (the peer never closed its side).
            let done = (slot.conn.is_closed() && !slot.conn.is_lingering())
                || (slot.conn.is_fin_acked() && !slot.conn.is_closed());
            if slot.detached && done {
                finished.push(key);
            }
        }
        if !finished.is_empty() {
            let mut conns = shared.conns();
            for key in finished {
                conns.remove(&key);
            }
        }
        for (to, datagram) in out.drain(..) {
            if let Err(e) = shared.udp.send_to(&datagram, to).await {
                tracing::trace!(%to, error = %e, "uTP send failed");
            }
        }
        if shared.closed.load(Ordering::Acquire) && shared.conns().is_empty() {
            return;
        }
    }
}

fn next_deadline(shared: &Shared) -> Option<Instant> {
    shared
        .conns()
        .values()
        .filter_map(|slot| lock(slot).conn.poll_timeout())
        .min()
}

/// Routes one datagram; returns the connection it touched.
fn on_datagram(
    shared: &Arc<Shared>,
    accepted: &mpsc::Sender<(UtpStream, SocketAddr)>,
    data: &[u8],
    from: SocketAddr,
    out: &mut Vec<(SocketAddr, Bytes)>,
) -> Option<Key> {
    let packet = Packet::decode(data).ok()?;
    let now = Instant::now();
    if packet.ty == PacketType::Syn {
        let key = (from, packet.conn_id.wrapping_add(1));
        if let Some(slot) = shared.conns().get(&key) {
            lock(slot).conn.on_syn();
            return Some(key);
        }
        if shared.closed.load(Ordering::Acquire) {
            out.push((from, reset(&packet)));
            return None;
        }
        let Ok(permit) = accepted.try_reserve() else {
            // Nobody is accepting fast enough: refuse.
            out.push((from, reset(&packet)));
            return None;
        };
        let conn = Connection::accept(shared.config.clone(), now, &packet);
        let slot = Arc::new(Mutex::new(Slot {
            conn,
            read_waker: None,
            write_waker: None,
            detached: false,
        }));
        shared.conns().insert(key, Arc::clone(&slot));
        permit.send((
            UtpStream {
                shared: Arc::clone(shared),
                key,
                slot,
            },
            from,
        ));
        return Some(key);
    }
    let key = (from, packet.conn_id);
    let Some(slot) = shared.conns().get(&key).cloned() else {
        if packet.ty != PacketType::Reset {
            out.push((from, reset(&packet)));
        }
        return None;
    };
    lock(&slot).conn.on_packet(now, &packet);
    Some(key)
}

/// A reset answering `to`.
fn reset(to: &Packet) -> Bytes {
    let mut p = Packet::new(PacketType::Reset, to.conn_id);
    if to.ty == PacketType::Syn {
        p.conn_id = to.conn_id;
    }
    p.ack_nr = to.seq_nr;
    let mut out = BytesMut::new();
    p.encode_into(&mut out);
    out.freeze()
}
