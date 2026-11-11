//! UDP tracker traffic through a SOCKS5 UDP relay (RFC 1928 §7).
//!
//! A [`UdpLink`] is what the client sends announces on: either a UDP
//! socket connected to the tracker, or one connected to the relay a
//! SOCKS5 proxy assigned with UDP ASSOCIATE. The association lives as long
//! as its TCP control connection, which the link keeps open.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use dravity_core::socks5;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

/// A SOCKS5 proxy that relays UDP tracker requests.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UdpProxy {
    /// The proxy's `host:port`.
    pub server: String,
    /// RFC 1929 username and password, if the proxy needs them.
    pub auth: Option<(String, String)>,
}

impl UdpProxy {
    /// A proxy at `server` (`host:port`), with optional credentials.
    #[must_use]
    pub const fn new(server: String, auth: Option<(String, String)>) -> Self {
        Self { server, auth }
    }
}

impl fmt::Debug for UdpProxy {
    // Never print the password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdpProxy")
            .field("server", &self.server)
            .field("auth", &self.auth.as_ref().map(|(user, _)| user))
            .finish()
    }
}

/// Where announces to one tracker go.
#[derive(Debug)]
pub(crate) struct UdpLink {
    socket: UdpSocket,
    relayed: Option<Relayed>,
}

#[derive(Debug)]
struct Relayed {
    /// The tracker, which every reply must come from.
    target: SocketAddr,
    header: Vec<u8>,
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

impl UdpLink {
    /// A socket connected straight to `target`.
    pub(crate) async fn direct(target: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(unspecified(target)).await?;
        socket.connect(target).await?;
        Ok(Self {
            socket,
            relayed: None,
        })
    }

    /// A UDP association with `proxy` for datagrams to `target`.
    pub(crate) async fn relayed(proxy: &UdpProxy, target: SocketAddr) -> io::Result<Self> {
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
        // We do not know our relay-facing address in advance: 0.0.0.0:0
        // lets the proxy accept our datagrams from wherever they come.
        control
            .write_all(&socks5::request(socks5::UDP_ASSOCIATE, unspecified(target)))
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
        let socket = UdpSocket::bind(unspecified(relay)).await?;
        socket.connect(relay).await?;
        Ok(Self {
            socket,
            relayed: Some(Relayed {
                target,
                header: socks5::udp_header(target),
                _control: control,
            }),
        })
    }

    pub(crate) async fn send(&self, payload: &[u8]) -> io::Result<()> {
        match &self.relayed {
            None => self.socket.send(payload).await.map(drop),
            Some(relayed) => {
                let mut datagram = relayed.header.clone();
                datagram.extend_from_slice(payload);
                self.socket.send(&datagram).await.map(drop)
            }
        }
    }

    /// The next datagram from the tracker; relayed datagrams from anyone
    /// else, or fragmented ones, are skipped.
    pub(crate) async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(relayed) = &self.relayed else {
            return self.socket.recv(buf).await;
        };
        let mut datagram = vec![0u8; buf.len() + 22];
        loop {
            let n = self.socket.recv(&mut datagram).await?;
            let Some((from, payload)) =
                socks5::parse_udp_header(datagram.get(..n).unwrap_or_default())
            else {
                continue;
            };
            if !same_host(from, relayed.target) {
                continue;
            }
            let len = payload.len().min(buf.len());
            if let (Some(out), Some(src)) = (buf.get_mut(..len), payload.get(..len)) {
                out.copy_from_slice(src);
            }
            return Ok(len);
        }
    }
}

/// `a` and `b` are one endpoint, an IPv4 address and its IPv6-mapped form
/// alike (relays differ in which they report).
fn same_host(a: SocketAddr, b: SocketAddr) -> bool {
    let canon = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 @ IpAddr::V4(_) => v4,
    };
    a.port() == b.port() && canon(a.ip()) == canon(b.ip())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn mapped_and_plain_ipv4_are_one_host() {
        let plain: SocketAddr = "192.0.2.1:6969".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:192.0.2.1]:6969".parse().unwrap();
        assert!(same_host(plain, mapped));
        assert!(!same_host(plain, "192.0.2.1:80".parse().unwrap()));
    }

    #[test]
    fn the_password_is_never_printed() {
        let p = UdpProxy::new("h:1".into(), Some(("me".into(), "secret".into())));
        assert!(!format!("{p:?}").contains("secret"));
    }
}
