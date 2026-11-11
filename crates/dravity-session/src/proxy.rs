//! Outgoing peer connections through a proxy: SOCKS4/4a, SOCKS5 (with
//! username/password auth) and HTTP `CONNECT`.
//!
//! The byte-level request builders and reply parsers are plain functions,
//! tested without sockets; [`connect`] drives them over a `TcpStream`.
//! The SOCKS5 UDP ASSOCIATE datagram header is provided as a codec
//! ([`udp_header`], [`parse_udp_header`]) for a future UDP relay.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Longest HTTP `CONNECT` reply header we read before giving up.
const MAX_HTTP_REPLY: usize = 8 * 1024;

/// The proxy protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// SOCKS4 (IPv4 targets only; no authentication beyond a user id).
    Socks4,
    /// SOCKS5 (RFC 1928), optionally with RFC 1929 username/password.
    Socks5,
    /// HTTP `CONNECT` tunnelling, optionally with Basic auth.
    Http,
}

/// A proxy server and how to talk to it.
#[derive(Clone, PartialEq, Eq)]
pub struct Proxy {
    /// The protocol.
    pub kind: ProxyKind,
    /// The proxy server's `host:port`.
    pub server: String,
    /// Username and password, if the proxy needs them.
    pub auth: Option<(String, String)>,
}

impl fmt::Debug for Proxy {
    // Never print the password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy")
            .field("kind", &self.kind)
            .field("server", &self.server)
            .field("auth", &self.auth.as_ref().map(|(user, _)| user))
            .finish()
    }
}

/// Why a proxy could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// The proxy URL is malformed or uses an unknown scheme.
    #[error("invalid proxy URL: {0}")]
    InvalidUrl(String),
    /// SOCKS4 cannot reach IPv6 addresses.
    #[error("SOCKS4 cannot reach IPv6 address {0}")]
    Ipv6Unsupported(SocketAddr),
    /// A username or password longer than SOCKS5 allows (255 bytes).
    #[error("SOCKS5 credentials longer than 255 bytes")]
    CredentialsTooLong,
    /// The proxy refused, with its reply code or status line.
    #[error("proxy refused the connection: {0}")]
    Refused(String),
    /// The proxy sent something that is not a valid reply.
    #[error("malformed proxy reply")]
    Malformed,
    /// A socket error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Proxy {
    /// Parses `socks4://host:port`, `socks5://[user:pass@]host:port` or
    /// `http://[user:pass@]host:port`.
    ///
    /// ```
    /// use dravity_session::proxy::{Proxy, ProxyKind};
    ///
    /// let p = Proxy::parse("socks5://me:secret@127.0.0.1:1080").unwrap();
    /// assert_eq!(p.kind, ProxyKind::Socks5);
    /// assert_eq!(p.server, "127.0.0.1:1080");
    /// assert_eq!(p.auth, Some(("me".into(), "secret".into())));
    /// ```
    ///
    /// # Errors
    /// [`ProxyError::InvalidUrl`] for anything else.
    pub fn parse(url: &str) -> Result<Self, ProxyError> {
        let bad = || ProxyError::InvalidUrl(url.to_owned());
        let parsed = url::Url::parse(url).map_err(|_| bad())?;
        let kind = match parsed.scheme() {
            "socks4" | "socks4a" => ProxyKind::Socks4,
            "socks5" | "socks5h" => ProxyKind::Socks5,
            "http" => ProxyKind::Http,
            _ => return Err(bad()),
        };
        let host = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(bad)?;
        let port = parsed.port_or_known_default().ok_or_else(bad)?;
        if parsed.path() != "/" && !parsed.path().is_empty() {
            return Err(bad());
        }
        let auth = if parsed.username().is_empty() {
            None
        } else {
            let decode =
                |s: &str| percent_decode(s).ok_or_else(|| ProxyError::InvalidUrl(url.to_owned()));
            Some((
                decode(parsed.username())?,
                decode(parsed.password().unwrap_or(""))?,
            ))
        };
        Ok(Self {
            kind,
            server: format!("{host}:{port}"),
            auth,
        })
    }

    /// The proxy as a URL for the HTTP tracker and web seed clients.
    ///
    /// SOCKS proxies use the remote-DNS schemes (`socks5h://`,
    /// `socks4a://`) so tracker and web seed host names are resolved by the
    /// proxy and never leak to the local resolver. SOCKS4 carries no
    /// password, so credentials are dropped for it. Always `Some` today;
    /// the `Option` leaves room for kinds HTTP clients cannot use.
    ///
    /// ```
    /// use dravity_session::proxy::Proxy;
    ///
    /// let p = Proxy::parse("socks5://me:pw@127.0.0.1:1080").unwrap();
    /// assert_eq!(p.http_url().unwrap(), "socks5h://me:pw@127.0.0.1:1080");
    /// ```
    #[must_use]
    pub fn http_url(&self) -> Option<String> {
        let scheme = match self.kind {
            ProxyKind::Http => "http",
            ProxyKind::Socks5 => "socks5h",
            ProxyKind::Socks4 => return Some(format!("socks4a://{}", self.server)),
        };
        Some(match &self.auth {
            Some((user, pass)) => format!(
                "{scheme}://{}:{}@{}",
                percent_encode(user),
                percent_encode(pass),
                self.server
            ),
            None => format!("{scheme}://{}", self.server),
        })
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn percent_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// A SOCKS4 `CONNECT` request for `target`.
///
/// # Errors
/// [`ProxyError::Ipv6Unsupported`] for an IPv6 target.
pub fn socks4_request(target: SocketAddr, user: &str) -> Result<Vec<u8>, ProxyError> {
    let IpAddr::V4(ip) = target.ip() else {
        return Err(ProxyError::Ipv6Unsupported(target));
    };
    let mut out = vec![4, 1];
    out.extend_from_slice(&target.port().to_be_bytes());
    out.extend_from_slice(&ip.octets());
    out.extend_from_slice(user.as_bytes());
    out.push(0);
    Ok(out)
}

/// Checks an 8-byte SOCKS4 reply.
///
/// # Errors
/// [`ProxyError::Refused`] unless the code is 90 (granted);
/// [`ProxyError::Malformed`] for a short or wrong-version reply.
pub fn socks4_reply(reply: &[u8]) -> Result<(), ProxyError> {
    match reply {
        [0, 90, ..] if reply.len() == 8 => Ok(()),
        [0, code, ..] if reply.len() == 8 => {
            Err(ProxyError::Refused(format!("SOCKS4 code {code}")))
        }
        _ => Err(ProxyError::Malformed),
    }
}

/// The SOCKS5 greeting: offer "no auth", plus username/password if we
/// have credentials.
#[must_use]
pub fn socks5_greeting(with_auth: bool) -> Vec<u8> {
    if with_auth {
        vec![5, 2, 0, 2]
    } else {
        vec![5, 1, 0]
    }
}

/// The RFC 1929 username/password sub-negotiation.
///
/// # Errors
/// [`ProxyError::CredentialsTooLong`] if either is over 255 bytes.
pub fn socks5_auth(user: &str, pass: &str) -> Result<Vec<u8>, ProxyError> {
    let ulen = u8::try_from(user.len()).map_err(|_| ProxyError::CredentialsTooLong)?;
    let plen = u8::try_from(pass.len()).map_err(|_| ProxyError::CredentialsTooLong)?;
    let mut out = vec![1, ulen];
    out.extend_from_slice(user.as_bytes());
    out.push(plen);
    out.extend_from_slice(pass.as_bytes());
    Ok(out)
}

fn push_addr(out: &mut Vec<u8>, target: SocketAddr) {
    match target.ip() {
        IpAddr::V4(ip) => {
            out.push(1);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&target.port().to_be_bytes());
}

/// A SOCKS5 request: `command` 1 is CONNECT, 3 is UDP ASSOCIATE.
#[must_use]
pub fn socks5_request(command: u8, target: SocketAddr) -> Vec<u8> {
    let mut out = vec![5, command, 0];
    push_addr(&mut out, target);
    out
}

/// How many bytes the SOCKS5 reply whose first 5 bytes are `head` has
/// in total, or `None` if `head` is invalid.
#[must_use]
pub fn socks5_reply_len(head: &[u8; 5]) -> Option<usize> {
    let [5, _, 0, atyp, len] = *head else {
        return None;
    };
    match atyp {
        1 => Some(4 + 4 + 2),
        4 => Some(4 + 16 + 2),
        3 => Some(4 + 1 + usize::from(len) + 2),
        _ => None,
    }
}

/// Checks a SOCKS5 reply's status byte.
///
/// # Errors
/// [`ProxyError::Refused`] for a non-zero status.
pub fn socks5_status(head: &[u8; 5]) -> Result<(), ProxyError> {
    match head[1] {
        0 => Ok(()),
        code => Err(ProxyError::Refused(format!("SOCKS5 code {code}"))),
    }
}

/// The header that prefixes a datagram sent through a SOCKS5 UDP relay.
#[must_use]
pub fn udp_header(target: SocketAddr) -> Vec<u8> {
    let mut out = vec![0, 0, 0];
    push_addr(&mut out, target);
    out
}

/// Splits a datagram received from a SOCKS5 UDP relay into its source
/// address and payload. Fragmented and domain-addressed datagrams are
/// refused (`None`).
#[must_use]
pub fn parse_udp_header(datagram: &[u8]) -> Option<(SocketAddr, &[u8])> {
    match datagram {
        [0, 0, 0, 1, a, b, c, d, p1, p2, rest @ ..] => Some((
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(*a, *b, *c, *d)),
                u16::from_be_bytes([*p1, *p2]),
            ),
            rest,
        )),
        [0, 0, 0, 4, rest @ ..] => {
            let ip: [u8; 16] = rest.get(..16)?.try_into().ok()?;
            let port: [u8; 2] = rest.get(16..18)?.try_into().ok()?;
            Some((
                SocketAddr::new(IpAddr::from(ip), u16::from_be_bytes(port)),
                rest.get(18..)?,
            ))
        }
        _ => None,
    }
}

/// An HTTP `CONNECT` request for `target`.
#[must_use]
pub fn http_connect_request(target: SocketAddr, auth: Option<&(String, String)>) -> Vec<u8> {
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some((user, pass)) = auth {
        req.push_str("Proxy-Authorization: Basic ");
        req.push_str(&base64(format!("{user}:{pass}").as_bytes()));
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    req.into_bytes()
}

/// Checks the status line of an HTTP `CONNECT` reply (the header block,
/// up to and including the blank line).
///
/// # Errors
/// [`ProxyError::Refused`] for a non-2xx status; [`ProxyError::Malformed`]
/// if there is no valid status line.
pub fn http_connect_reply(header: &[u8]) -> Result<(), ProxyError> {
    let text = std::str::from_utf8(header).map_err(|_| ProxyError::Malformed)?;
    let line = text.lines().next().ok_or(ProxyError::Malformed)?;
    let mut parts = line.split_whitespace();
    let version = parts.next().ok_or(ProxyError::Malformed)?;
    let status = parts.next().ok_or(ProxyError::Malformed)?;
    if !version.starts_with("HTTP/1.") || status.len() != 3 {
        return Err(ProxyError::Malformed);
    }
    if status.starts_with('2') {
        Ok(())
    } else {
        Err(ProxyError::Refused(line.to_owned()))
    }
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let sym = |i: u32| char::from(ALPHABET.get((i & 63) as usize).copied().unwrap_or(b'='));
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        out.push(sym(n >> 18));
        out.push(sym(n >> 12));
        out.push(if chunk.len() > 1 { sym(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { sym(n) } else { '=' });
    }
    out
}

/// Opens a TCP connection to `target` through `proxy`.
///
/// # Errors
/// Any [`ProxyError`].
pub async fn connect(proxy: &Proxy, target: SocketAddr) -> Result<TcpStream, ProxyError> {
    let mut stream = TcpStream::connect(proxy.server.as_str()).await?;
    match proxy.kind {
        ProxyKind::Socks4 => {
            let user = proxy.auth.as_ref().map_or("", |(u, _)| u.as_str());
            stream.write_all(&socks4_request(target, user)?).await?;
            let mut reply = [0u8; 8];
            stream.read_exact(&mut reply).await?;
            socks4_reply(&reply)?;
        }
        ProxyKind::Socks5 => {
            stream
                .write_all(&socks5_greeting(proxy.auth.is_some()))
                .await?;
            let mut choice = [0u8; 2];
            stream.read_exact(&mut choice).await?;
            match (choice, &proxy.auth) {
                ([5, 0], _) => {}
                ([5, 2], Some((user, pass))) => {
                    stream.write_all(&socks5_auth(user, pass)?).await?;
                    let mut status = [0u8; 2];
                    stream.read_exact(&mut status).await?;
                    if status != [1, 0] {
                        return Err(ProxyError::Refused("SOCKS5 authentication".into()));
                    }
                }
                ([5, 0xff], _) => {
                    return Err(ProxyError::Refused("no acceptable SOCKS5 method".into()));
                }
                _ => return Err(ProxyError::Malformed),
            }
            stream.write_all(&socks5_request(1, target)).await?;
            let mut head = [0u8; 5];
            stream.read_exact(&mut head).await?;
            socks5_status(&head)?;
            let total = socks5_reply_len(&head).ok_or(ProxyError::Malformed)?;
            let mut rest = vec![0u8; total - head.len()];
            stream.read_exact(&mut rest).await?;
        }
        ProxyKind::Http => {
            stream
                .write_all(&http_connect_request(target, proxy.auth.as_ref()))
                .await?;
            // Read byte by byte so nothing after the header is consumed.
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                if header.len() >= MAX_HTTP_REPLY {
                    return Err(ProxyError::Malformed);
                }
                header.push(stream.read_u8().await?);
            }
            http_connect_reply(&header)?;
        }
    }
    Ok(stream)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn v4() -> SocketAddr {
        "10.1.2.3:6881".parse().unwrap()
    }

    #[test]
    fn urls_parse_and_bad_ones_are_refused() {
        let p = Proxy::parse("http://u%40x:p%3A@proxy.example:3128").unwrap();
        assert_eq!(p.kind, ProxyKind::Http);
        assert_eq!(p.auth, Some(("u@x".into(), "p:".into())));
        assert_eq!(
            p.http_url().unwrap(),
            "http://u%40x:p%3A@proxy.example:3128"
        );
        assert_eq!(
            Proxy::parse("socks4://h:1").unwrap().kind,
            ProxyKind::Socks4
        );
        assert_eq!(
            Proxy::parse("socks5://h:1").unwrap().http_url().unwrap(),
            "socks5h://h:1"
        );
        assert_eq!(
            Proxy::parse("socks4://u:x@h:1")
                .unwrap()
                .http_url()
                .unwrap(),
            "socks4a://h:1"
        );
        for bad in ["ftp://h:1", "socks5://h", "socks5://h:1/x", "nonsense"] {
            assert!(Proxy::parse(bad).is_err(), "{bad}");
        }
        assert!(
            !format!("{:?}", Proxy::parse("socks5://a:secret@h:1").unwrap()).contains("secret")
        );
    }

    #[test]
    fn socks4_bytes() {
        assert_eq!(
            socks4_request(v4(), "me").unwrap(),
            [4, 1, 0x1a, 0xe1, 10, 1, 2, 3, b'm', b'e', 0]
        );
        assert!(socks4_request("[::1]:1".parse().unwrap(), "").is_err());
        assert!(socks4_reply(&[0, 90, 0, 0, 0, 0, 0, 0]).is_ok());
        assert!(matches!(
            socks4_reply(&[0, 91, 0, 0, 0, 0, 0, 0]),
            Err(ProxyError::Refused(_))
        ));
        assert!(matches!(socks4_reply(&[0, 90]), Err(ProxyError::Malformed)));
    }

    #[test]
    fn socks5_bytes() {
        assert_eq!(socks5_greeting(false), [5, 1, 0]);
        assert_eq!(socks5_auth("a", "bc").unwrap(), [1, 1, b'a', 2, b'b', b'c']);
        assert!(socks5_auth(&"x".repeat(256), "").is_err());
        assert_eq!(
            socks5_request(1, v4()),
            [5, 1, 0, 1, 10, 1, 2, 3, 0x1a, 0xe1]
        );
        assert_eq!(socks5_reply_len(&[5, 0, 0, 1, 0]), Some(10));
        assert_eq!(socks5_reply_len(&[5, 0, 0, 4, 0]), Some(22));
        assert_eq!(socks5_reply_len(&[5, 0, 0, 3, 7]), Some(14));
        assert_eq!(socks5_reply_len(&[4, 0, 0, 1, 0]), None);
        assert!(socks5_status(&[5, 5, 0, 1, 0]).is_err());
    }

    #[test]
    fn udp_header_round_trips() {
        for target in [v4(), "[2001:db8::1]:51413".parse().unwrap()] {
            let mut datagram = udp_header(target);
            datagram.extend_from_slice(b"payload");
            assert_eq!(parse_udp_header(&datagram), Some((target, &b"payload"[..])));
        }
        // Fragments and truncated headers are refused.
        assert_eq!(parse_udp_header(&[0, 0, 1, 1, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(parse_udp_header(&[0, 0, 0, 4, 1]), None);
    }

    #[test]
    fn http_connect_bytes() {
        let auth = ("Aladdin".to_owned(), "open sesame".to_owned());
        let req = String::from_utf8(http_connect_request(v4(), Some(&auth))).unwrap();
        assert!(req.starts_with("CONNECT 10.1.2.3:6881 HTTP/1.1\r\n"));
        // RFC 7617's example credentials.
        assert!(req.contains("Proxy-Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==\r\n"));
        assert!(req.ends_with("\r\n\r\n"));
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert!(http_connect_reply(b"HTTP/1.1 200 Connection established\r\n\r\n").is_ok());
        assert!(matches!(
            http_connect_reply(b"HTTP/1.0 407 Proxy Authentication Required\r\n\r\n"),
            Err(ProxyError::Refused(_))
        ));
        assert!(matches!(
            http_connect_reply(b"SSH-2.0\r\n\r\n"),
            Err(ProxyError::Malformed)
        ));
    }
}
