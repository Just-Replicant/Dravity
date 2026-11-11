//! The BEP 14 message format (sans-IO).
//!
//! ```
//! use dravity_core::InfoHash;
//! use dravity_lsd::codec::{encode, parse, IPV4_GROUP};
//!
//! let hash = InfoHash::new([0xab; 20]);
//! let msg = encode(IPV4_GROUP, 6881, &[hash], Some("c00k1e"));
//! let back = parse(&msg).unwrap();
//! assert_eq!(back.port, 6881);
//! assert_eq!(back.info_hashes, [hash]);
//! assert_eq!(back.cookie.as_deref(), Some("c00k1e"));
//! ```

use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use dravity_core::InfoHash;

/// The IPv4 multicast group and port.
pub const IPV4_GROUP: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 192, 152, 143), 6771));

/// The IPv6 multicast group and port.
pub const IPV6_GROUP: SocketAddr = SocketAddr::V6(SocketAddrV6::new(
    Ipv6Addr::new(0xff15, 0, 0, 0, 0, 0, 0xefc0, 0x988f),
    6771,
    0,
    0,
));

/// Largest message accepted (BEP 14 messages fit one unfragmented
/// datagram).
pub const MAX_MESSAGE_LEN: usize = 1400;

/// Most info hashes in one message, so it stays below
/// [`MAX_MESSAGE_LEN`].
pub const MAX_HASHES_PER_MESSAGE: usize = 20;

/// A parsed `BT-SEARCH` announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    /// The sender's BitTorrent listen port.
    pub port: u16,
    /// Torrents the sender is in (v1 info hashes, 40 hex digits each).
    pub info_hashes: Vec<InfoHash>,
    /// The sender's cookie, used to ignore our own announces.
    pub cookie: Option<String>,
}

/// Why a datagram is not a valid announce.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LsdError {
    /// Longer than [`MAX_MESSAGE_LEN`].
    #[error("LSD message too long")]
    TooLong,
    /// Not text, or not a `BT-SEARCH * HTTP/1.1` request.
    #[error("not a BT-SEARCH message")]
    NotSearch,
    /// No valid nonzero `Port` header.
    #[error("missing or invalid Port header")]
    BadPort,
    /// No valid `Infohash` header.
    #[error("no valid Infohash header")]
    NoInfoHash,
}

/// Encodes one announce of `hashes` (at most [`MAX_HASHES_PER_MESSAGE`]
/// are used) for `group`, with our listen `port`.
#[must_use]
pub fn encode(group: SocketAddr, port: u16, hashes: &[InfoHash], cookie: Option<&str>) -> Vec<u8> {
    let mut out = String::with_capacity(128 + 52 * hashes.len());
    let _ = write!(
        out,
        "BT-SEARCH * HTTP/1.1\r\nHost: {group}\r\nPort: {port}\r\n"
    );
    for hash in hashes.iter().take(MAX_HASHES_PER_MESSAGE) {
        let _ = write!(out, "Infohash: {}\r\n", hash.to_hex());
    }
    if let Some(cookie) = cookie {
        let _ = write!(out, "cookie: {cookie}\r\n");
    }
    out.push_str("\r\n\r\n");
    out.into_bytes()
}

/// Parses an announce. Unknown headers are ignored, and so are
/// `Infohash` values that are not 40 hex digits.
///
/// # Errors
/// See [`LsdError`].
pub fn parse(datagram: &[u8]) -> Result<Announce, LsdError> {
    if datagram.len() > MAX_MESSAGE_LEN {
        return Err(LsdError::TooLong);
    }
    let text = std::str::from_utf8(datagram).map_err(|_| LsdError::NotSearch)?;
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    if !first.eq_ignore_ascii_case("BT-SEARCH * HTTP/1.1") {
        return Err(LsdError::NotSearch);
    }
    let mut port = None;
    let mut info_hashes = Vec::new();
    let mut cookie = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "port" => port = value.parse::<u16>().ok().filter(|&p| p != 0),
            "infohash" => {
                if value.len() == 40
                    && let Ok(hash) = InfoHash::from_hex(value)
                    && !info_hashes.contains(&hash)
                {
                    info_hashes.push(hash);
                }
            }
            "cookie" => cookie = Some(value.to_owned()),
            _ => {}
        }
    }
    let port = port.ok_or(LsdError::BadPort)?;
    if info_hashes.is_empty() {
        return Err(LsdError::NoInfoHash);
    }
    Ok(Announce {
        port,
        info_hashes,
        cookie,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn encodes_bep_14_messages() {
        let h = InfoHash::new([0x01; 20]);
        let msg = String::from_utf8(encode(IPV4_GROUP, 51413, &[h], None)).unwrap();
        assert_eq!(
            msg,
            format!(
                "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 51413\r\nInfohash: {}\r\n\r\n\r\n",
                "01".repeat(20)
            )
        );
        let v6 = String::from_utf8(encode(IPV6_GROUP, 1, &[h], None)).unwrap();
        assert!(v6.contains("Host: [ff15::efc0:988f]:6771"), "{v6}");
        let many = vec![h; 50];
        let msg = encode(IPV4_GROUP, 1, &many, Some("x"));
        assert!(msg.len() <= MAX_MESSAGE_LEN);
    }

    #[test]
    fn parses_leniently_and_rejects_garbage() {
        let a = InfoHash::new([0xaa; 20]);
        let msg = format!(
            "bt-search * http/1.1\r\nhost: x\r\nPORT:  6881 \r\ninfohash: {}\r\nInfohash: zz\r\nInfohash: {}\r\nX-Other: 1\r\n\r\n\r\n",
            a.to_hex(),
            a.to_hex().to_uppercase()
        );
        let parsed = parse(msg.as_bytes()).unwrap();
        assert_eq!(parsed.port, 6881);
        assert_eq!(parsed.info_hashes, [a], "duplicates and junk dropped");
        assert_eq!(parsed.cookie, None);
        assert_eq!(parse(b"GET / HTTP/1.1\r\n\r\n"), Err(LsdError::NotSearch));
        assert_eq!(parse(&[0xff, 0xfe]), Err(LsdError::NotSearch));
        let no_port = format!("BT-SEARCH * HTTP/1.1\r\nInfohash: {}\r\n\r\n", a.to_hex());
        assert_eq!(parse(no_port.as_bytes()), Err(LsdError::BadPort));
        let zero = format!(
            "BT-SEARCH * HTTP/1.1\r\nPort: 0\r\nInfohash: {}\r\n\r\n",
            a.to_hex()
        );
        assert_eq!(parse(zero.as_bytes()), Err(LsdError::BadPort));
        assert_eq!(
            parse(b"BT-SEARCH * HTTP/1.1\r\nPort: 1\r\n\r\n"),
            Err(LsdError::NoInfoHash)
        );
        assert_eq!(parse(&vec![b'a'; 2000]), Err(LsdError::TooLong));
    }
}
