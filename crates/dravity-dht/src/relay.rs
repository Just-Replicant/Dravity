//! A SOCKS5 UDP ASSOCIATE (RFC 1928 §7) for the native DHT.
//!
//! [`UdpAssociate::open`] returns only after the proxy has accepted the
//! association. The UDP socket is bound after that reply and talks only
//! to the relay, so no datagram is sent or accepted before the relay is
//! up (and none is sent at all if the association fails).

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use dravity_core::socks5;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

/// A SOCKS5 proxy that can relay native DHT datagrams.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Socks5Proxy {
    /// The proxy's `host:port`.
    pub server: String,
    /// RFC 1929 username and password, if the proxy needs them.
    pub auth: Option<(String, String)>,
}

impl Socks5Proxy {
    /// A proxy at `server` (`host:port`), with optional credentials.
    #[must_use]
    pub const fn new(server: String, auth: Option<(String, String)>) -> Self {
        Self { server, auth }
    }
}

impl fmt::Debug for Socks5Proxy {
    // Never print the password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Socks5Proxy")
            .field("server", &self.server)
            .field("auth", &self.auth.as_ref().map(|(user, _)| user))
            .finish()
    }
}

/// A destination a native DHT node can send a datagram to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Contact {
    /// An IP address (SOCKS5 address type 1 or 4).
    Addr(SocketAddr),
    /// A host name the SOCKS5 proxy resolves (address type 3), so the
    /// name never reaches the local resolver.
    Name(Arc<str>, u16),
}

impl Contact {
    /// Parses `host:port` or a socket address. A host that is not an IP
    /// becomes [`Contact::Name`].
    ///
    /// ```
    /// use dravity_dht::Contact;
    ///
    /// assert!(matches!(
    ///     Contact::parse("router.example:6881"),
    ///     Some(Contact::Name(n, 6881)) if &*n == "router.example"
    /// ));
    /// assert_eq!(
    ///     Contact::parse("127.0.0.1:6881"),
    ///     Some(Contact::Addr("127.0.0.1:6881".parse().unwrap()))
    /// );
    /// ```
    #[must_use]
    pub fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        if let Ok(addr) = entry.parse::<SocketAddr>() {
            return Some(Self::Addr(addr));
        }
        let (host, port) = entry.rsplit_once(':')?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port = port.parse().ok()?;
        if let Ok(ip) = host.parse::<IpAddr>() {
            Some(Self::Addr(SocketAddr::new(ip, port)))
        } else if !host.is_empty() && u8::try_from(host.len()).is_ok() {
            Some(Self::Name(Arc::from(host), port))
        } else {
            None
        }
    }

    /// [`Self::parse`] on every entry, skipping ones that are not
    /// `host:port`.
    #[must_use]
    pub fn parse_all(entries: &[String]) -> Vec<Self> {
        entries.iter().filter_map(|e| Self::parse(e)).collect()
    }
}

impl From<SocketAddr> for Contact {
    fn from(addr: SocketAddr) -> Self {
        Self::Addr(addr)
    }
}

/// Where a native node sends and receives datagrams: straight to the
/// peer, or through a completed SOCKS5 UDP association.
#[derive(Debug)]
pub(crate) enum Transport {
    /// A bound UDP socket.
    Direct(UdpSocket),
    /// A relay that exists only after UDP ASSOCIATE succeeded.
    Relayed(UdpAssociate),
}

impl Transport {
    pub(crate) async fn send_to(&self, payload: &[u8], dest: &Contact) -> io::Result<usize> {
        match self {
            Self::Direct(socket) => {
                let Contact::Addr(addr) = dest else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a direct DHT socket cannot send to a host name",
                    ));
                };
                socket.send_to(payload, addr).await
            }
            Self::Relayed(relay) => relay.send_to(payload, dest).await,
        }
    }

    pub(crate) async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Self::Direct(socket) => socket.recv_from(buf).await,
            Self::Relayed(relay) => relay.recv_from(buf).await,
        }
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Direct(socket) => socket.local_addr(),
            Self::Relayed(relay) => relay.local_addr(),
        }
    }
}

/// A completed SOCKS5 UDP ASSOCIATE. The UDP socket is created after the
/// proxy accepts, and is used only to talk to the relay.
#[derive(Debug)]
pub struct UdpAssociate {
    socket: UdpSocket,
    /// Closing this ends the association.
    _control: TcpStream,
}

fn unspecified(family_of: SocketAddr) -> SocketAddr {
    if family_of.is_ipv6() {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    }
}

fn refused(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionRefused, what.to_owned())
}

fn header(dest: &Contact) -> io::Result<Vec<u8>> {
    match dest {
        Contact::Addr(addr) => Ok(socks5::udp_header(*addr)),
        Contact::Name(host, port) => socks5::udp_header_domain(host, *port)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput)),
    }
}

impl UdpAssociate {
    /// Opens a TCP connection to `proxy`, completes UDP ASSOCIATE, then
    /// binds the UDP socket. Returns only once the relay is up.
    ///
    /// # Errors
    /// If the proxy cannot be reached, refuses the association, or sends
    /// a malformed reply.
    pub async fn open(proxy: &Socks5Proxy) -> io::Result<Self> {
        let mut control = TcpStream::connect(proxy.server.as_str()).await?;
        control
            .write_all(&socks5::greeting(proxy.auth.is_some()))
            .await?;
        let mut choice = [0u8; 2];
        control.read_exact(&mut choice).await?;
        match (choice, &proxy.auth) {
            ([5, 0], _) => {}
            ([5, 2], Some((user, pass))) => {
                let auth = socks5::auth(user, pass)
                    .ok_or_else(|| refused("SOCKS5 credentials longer than 255 bytes"))?;
                control.write_all(&auth).await?;
                let mut status = [0u8; 2];
                control.read_exact(&mut status).await?;
                if status[1] != 0 {
                    return Err(refused("SOCKS5 password refused"));
                }
            }
            _ => return Err(refused("no acceptable SOCKS5 method")),
        }
        // 0.0.0.0:0: we do not know our relay-facing address yet; the
        // proxy accepts datagrams from wherever they come.
        control
            .write_all(&socks5::request(
                socks5::UDP_ASSOCIATE,
                SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            ))
            .await?;
        let mut head = [0u8; 5];
        control.read_exact(&mut head).await?;
        if head[1] != 0 {
            return Err(refused(&format!("SOCKS5 code {}", head[1])));
        }
        let len =
            socks5::reply_len(&head).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        let mut reply = head.to_vec();
        reply.resize(len, 0);
        control
            .read_exact(reply.get_mut(5..).unwrap_or_default())
            .await?;
        let mut relay = socks5::reply_addr(&reply)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        // An unspecified relay address means "the proxy's own address".
        if relay.ip().is_unspecified() {
            relay.set_ip(control.peer_addr()?.ip());
        }
        // The UDP socket exists only after the association is accepted.
        let socket = UdpSocket::bind(unspecified(relay)).await?;
        socket.connect(relay).await?;
        Ok(Self {
            socket,
            _control: control,
        })
    }

    pub(crate) async fn send_to(&self, payload: &[u8], dest: &Contact) -> io::Result<usize> {
        let mut datagram = header(dest)?;
        datagram.extend_from_slice(payload);
        self.socket.send(&datagram).await
    }

    pub(crate) async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        // IPv6 header is 22 bytes; a domain header is at most 262.
        let mut datagram = vec![0u8; buf.len().saturating_add(262)];
        loop {
            let n = self.socket.recv(&mut datagram).await?;
            let Some((from, payload)) =
                socks5::parse_udp_header(datagram.get(..n).unwrap_or_default())
            else {
                continue;
            };
            let len = payload.len().min(buf.len());
            if let (Some(out), Some(src)) = (buf.get_mut(..len), payload.get(..len)) {
                out.copy_from_slice(src);
            }
            return Ok((len, from));
        }
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn contact_parses_names_and_addresses() {
        assert_eq!(
            Contact::parse("127.0.0.1:6881"),
            Some(Contact::Addr("127.0.0.1:6881".parse().unwrap()))
        );
        assert_eq!(
            Contact::parse("[::1]:6881"),
            Some(Contact::Addr("[::1]:6881".parse().unwrap()))
        );
        assert!(matches!(
            Contact::parse("router.example:25401"),
            Some(Contact::Name(n, 25401)) if &*n == "router.example"
        ));
        assert!(Contact::parse("no-port").is_none());
        assert!(Contact::parse("").is_none());
        assert!(Contact::parse(&format!("{}:1", "x".repeat(256))).is_none());
    }

    #[test]
    fn the_password_is_never_printed() {
        let p = Socks5Proxy::new("h:1".into(), Some(("me".into(), "secret".into())));
        assert!(!format!("{p:?}").contains("secret"));
        assert!(format!("{p:?}").contains("me"));
    }
}
