//! The LSD sockets: announce what the caller asks, report what is heard.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use dravity_core::InfoHash;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::codec::{self, IPV4_GROUP, IPV6_GROUP, MAX_HASHES_PER_MESSAGE, MAX_MESSAGE_LEN};

/// Messages accepted from one address per [`RATE_WINDOW`]; more are
/// dropped, so a noisy host cannot flood the session with dials.
const MAX_PER_SOURCE: u32 = 16;

/// The window [`MAX_PER_SOURCE`] counts over.
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Most sources tracked for rate limiting before the table is reset.
const MAX_SOURCES: usize = 1024;

/// Discoveries buffered for the caller; more are dropped.
const DISCOVERED_QUEUE: usize = 256;

/// LSD settings.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LsdConfig {
    /// How often each torrent is announced again (BEP 14 asks for at
    /// most once a minute; the default is five minutes).
    pub interval: Duration,
    /// Also use the IPv6 group.
    pub ipv6: bool,
    /// Unicast mode: bind here and send to these addresses instead of the
    /// multicast groups (isolated networks and tests).
    unicast: Option<(SocketAddr, Vec<SocketAddr>)>,
}

impl Default for LsdConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5 * 60),
            ipv6: true,
            unicast: None,
        }
    }
}

impl LsdConfig {
    /// Unicast mode: listen on `bind` and send announces to `targets`
    /// instead of the multicast groups.
    #[must_use]
    pub fn unicast(bind: SocketAddr, targets: Vec<SocketAddr>) -> Self {
        Self {
            unicast: Some((bind, targets)),
            ..Self::default()
        }
    }
}

/// A peer heard on the local network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Discovered {
    /// The torrent it announced.
    pub info_hash: InfoHash,
    /// Its address: the datagram's source IP and the announced port.
    pub peer: SocketAddr,
}

/// A running LSD service. Dropping it stops the task.
#[derive(Debug)]
pub struct Lsd {
    commands: mpsc::Sender<Vec<InfoHash>>,
    local: Vec<SocketAddr>,
}

/// One socket and where its announces go.
struct Endpoint {
    socket: UdpSocket,
    targets: Vec<SocketAddr>,
}

impl Lsd {
    /// Binds the sockets and starts the task. `port` is our BitTorrent
    /// listen port, which announces advertise.
    ///
    /// # Errors
    /// No socket could be bound (an IPv6 failure alone is only logged).
    pub fn start(config: &LsdConfig, port: u16) -> io::Result<(Self, mpsc::Receiver<Discovered>)> {
        let mut endpoints = Vec::new();
        if let Some((bind, targets)) = &config.unicast {
            let socket = std::net::UdpSocket::bind(bind)?;
            socket.set_nonblocking(true)?;
            endpoints.push(Endpoint {
                socket: UdpSocket::from_std(socket)?,
                targets: targets.clone(),
            });
        } else {
            endpoints.push(Endpoint {
                socket: multicast_v4()?,
                targets: vec![IPV4_GROUP],
            });
            if config.ipv6 {
                match multicast_v6() {
                    Ok(socket) => endpoints.push(Endpoint {
                        socket,
                        targets: vec![IPV6_GROUP],
                    }),
                    Err(e) => tracing::debug!(error = %e, "LSD over IPv6 unavailable"),
                }
            }
        }
        let local = endpoints
            .iter()
            .filter_map(|e| e.socket.local_addr().ok())
            .collect();
        let (commands, rx) = mpsc::channel(16);
        let (found_tx, found_rx) = mpsc::channel(DISCOVERED_QUEUE);
        let cookie = format!("{:016x}", fastrand::u64(..));
        tokio::spawn(run(endpoints, port, cookie, rx, found_tx));
        Ok((Self { commands, local }, found_rx))
    }

    /// Announces `hashes` now (in messages of at most
    /// [`MAX_HASHES_PER_MESSAGE`]). Dropped if the task is busy.
    pub fn announce(&self, hashes: Vec<InfoHash>) {
        if !hashes.is_empty() {
            let _ = self.commands.try_send(hashes);
        }
    }

    /// The bound local addresses.
    #[must_use]
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.local
    }
}

fn reusable(domain: Domain) -> io::Result<Socket> {
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    // Other LSD clients on this host bind the same port.
    socket.set_reuse_address(true)?;
    #[cfg(all(
        unix,
        not(any(target_os = "solaris", target_os = "illumos", target_os = "cygwin"))
    ))]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn multicast_v4() -> io::Result<UdpSocket> {
    let socket = reusable(Domain::IPV4)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, IPV4_GROUP.port())).into())?;
    if let IpAddr::V4(group) = IPV4_GROUP.ip() {
        socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)?;
    }
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(1)?;
    UdpSocket::from_std(socket.into())
}

fn multicast_v6() -> io::Result<UdpSocket> {
    let socket = reusable(Domain::IPV6)?;
    socket.set_only_v6(true)?;
    socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, IPV6_GROUP.port())).into())?;
    if let IpAddr::V6(group) = IPV6_GROUP.ip() {
        socket.join_multicast_v6(&group, 0)?;
    }
    socket.set_multicast_loop_v6(true)?;
    UdpSocket::from_std(socket.into())
}

/// Counts messages per source over [`RATE_WINDOW`].
#[derive(Default)]
struct RateLimit {
    seen: HashMap<IpAddr, (Instant, u32)>,
}

impl RateLimit {
    fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        if self.seen.len() >= MAX_SOURCES && !self.seen.contains_key(&ip) {
            self.seen.clear();
        }
        let entry = self.seen.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= RATE_WINDOW {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= MAX_PER_SOURCE
    }
}

async fn recv(socket: Option<&UdpSocket>, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    match socket {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending().await,
    }
}

async fn run(
    endpoints: Vec<Endpoint>,
    port: u16,
    cookie: String,
    mut commands: mpsc::Receiver<Vec<InfoHash>>,
    found: mpsc::Sender<Discovered>,
) {
    let mut limit = RateLimit::default();
    let mut buf_a = vec![0u8; MAX_MESSAGE_LEN + 1];
    let mut buf_b = vec![0u8; MAX_MESSAGE_LEN + 1];
    let a = endpoints.first().map(|e| &e.socket);
    let b = endpoints.get(1).map(|e| &e.socket);
    loop {
        let (n, from, which) = tokio::select! {
            command = commands.recv() => {
                let Some(hashes) = command else { return };
                for endpoint in &endpoints {
                    for target in &endpoint.targets {
                        for chunk in hashes.chunks(MAX_HASHES_PER_MESSAGE) {
                            let msg = codec::encode(*target, port, chunk, Some(&cookie));
                            if let Err(e) = endpoint.socket.send_to(&msg, target).await {
                                tracing::debug!(error = %e, %target, "LSD announce failed");
                            }
                        }
                    }
                }
                continue;
            }
            r = recv(a, &mut buf_a) => match r {
                Ok((n, from)) => (n, from, 0),
                Err(_) => continue,
            },
            r = recv(b, &mut buf_b) => match r {
                Ok((n, from)) => (n, from, 1),
                Err(_) => continue,
            },
        };
        let buf = if which == 0 { &buf_a } else { &buf_b };
        let Ok(announce) = codec::parse(buf.get(..n).unwrap_or_default()) else {
            continue;
        };
        if announce.cookie.as_deref() == Some(cookie.as_str()) {
            continue; // our own, looped back
        }
        if !limit.allow(from.ip(), Instant::now()) {
            continue;
        }
        let peer = SocketAddr::new(from.ip(), announce.port);
        for info_hash in announce.info_hashes {
            if found.try_send(Discovered { info_hash, peer }).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_rate_limited_per_window() {
        let mut limit = RateLimit::default();
        let ip: IpAddr = Ipv4Addr::LOCALHOST.into();
        let other: IpAddr = Ipv4Addr::new(10, 0, 0, 1).into();
        let t = Instant::now();
        for _ in 0..MAX_PER_SOURCE {
            assert!(limit.allow(ip, t));
        }
        assert!(!limit.allow(ip, t));
        assert!(limit.allow(other, t));
        assert!(limit.allow(ip, t + RATE_WINDOW));
    }
}
