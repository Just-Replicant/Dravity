//! HTTP tracker protocol (BEP 3, BEP 23 compact peers, BEP 7 `peers6`).

use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use dravity_bencode::Value;

use crate::error::TrackerError;
use crate::types::{AnnounceRequest, AnnounceResponse};

/// Maximum accepted HTTP response body.
pub const MAX_RESPONSE_LEN: usize = 2 * 1024 * 1024;

/// Appends the announce query to `base` (which may already have a query).
///
/// ```
/// use dravity_core::{InfoHash, PeerId};
/// use dravity_tracker::{AnnounceEvent, AnnounceRequest, http::announce_url};
///
/// let req = AnnounceRequest {
///     info_hash: InfoHash::new([0xab; 20]),
///     peer_id: PeerId::new(*b"-DV0001-123456789012"),
///     port: 6881,
///     uploaded: 0,
///     downloaded: 0,
///     left: 10,
///     event: AnnounceEvent::Started,
///     num_want: Some(50),
///     key: 0x1234,
/// };
/// let url = announce_url("http://t.example/announce?passkey=x", &req);
/// assert!(url.starts_with("http://t.example/announce?passkey=x&info_hash=%AB%AB"));
/// assert!(url.ends_with("&compact=1&event=started&numwant=50&key=00001234"));
/// ```
#[must_use]
pub fn announce_url(base: &str, req: &AnnounceRequest) -> String {
    let mut url = String::with_capacity(base.len() + 256);
    url.push_str(base);
    url.push(if base.contains('?') { '&' } else { '?' });
    url.push_str("info_hash=");
    percent_encode(req.info_hash.as_bytes(), &mut url);
    url.push_str("&peer_id=");
    percent_encode(req.peer_id.as_bytes(), &mut url);
    // Writing to a String cannot fail.
    let _ = write!(
        url,
        "&port={}&uploaded={}&downloaded={}&left={}&compact=1",
        req.port, req.uploaded, req.downloaded, req.left
    );
    if let Some(event) = req.event.as_http() {
        let _ = write!(url, "&event={event}");
    }
    if let Some(n) = req.num_want {
        let _ = write!(url, "&numwant={n}");
    }
    let _ = write!(url, "&key={:08x}", req.key);
    url
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
fn percent_encode(bytes: &[u8], out: &mut String) {
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
}

/// Parses a bencoded HTTP announce response.
///
/// # Errors
/// [`TrackerError::Failure`] when the tracker reports a `failure reason`;
/// [`TrackerError::Malformed`] when the body is not a valid response;
/// [`TrackerError::TooLarge`] above [`MAX_RESPONSE_LEN`].
pub fn parse_announce_response(body: &[u8]) -> Result<AnnounceResponse, TrackerError> {
    if body.len() > MAX_RESPONSE_LEN {
        return Err(TrackerError::TooLarge(MAX_RESPONSE_LEN));
    }
    let value = Value::decode(body).map_err(|_| TrackerError::Malformed("invalid bencode"))?;
    if value.as_dict().is_none() {
        return Err(TrackerError::Malformed("response is not a dictionary"));
    }
    if let Some(reason) = value.get(b"failure reason") {
        let text = reason
            .as_bytes()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        return Err(TrackerError::Failure(text));
    }
    let seconds = |key: &[u8]| {
        value
            .get(key)
            .and_then(Value::as_int)
            .and_then(|i| u64::try_from(i).ok())
            .map(Duration::from_secs)
    };
    let count = |key: &[u8]| {
        value
            .get(key)
            .and_then(Value::as_int)
            .and_then(|i| u32::try_from(i).ok())
    };
    let mut peers = Vec::new();
    match value.get(b"peers") {
        Some(Value::Bytes(compact)) => peers.extend(parse_compact_v4(compact)?),
        Some(Value::List(list)) => peers.extend(list.iter().filter_map(parse_dict_peer)),
        Some(_) => return Err(TrackerError::Malformed("`peers` has the wrong type")),
        None => {}
    }
    if let Some(Value::Bytes(compact)) = value.get(b"peers6") {
        peers.extend(parse_compact_v6(compact)?);
    }
    Ok(AnnounceResponse {
        interval: seconds(b"interval"),
        min_interval: seconds(b"min interval"),
        peers,
        seeders: count(b"complete"),
        leechers: count(b"incomplete"),
        warning: value
            .get(b"warning message")
            .and_then(Value::as_bytes)
            .map(|b| String::from_utf8_lossy(b).into_owned()),
    })
}

/// Parses BEP 23 compact IPv4 peers (6 bytes each).
///
/// # Errors
/// [`TrackerError::Malformed`] if the length is not a multiple of 6.
pub fn parse_compact_v4(bytes: &[u8]) -> Result<Vec<SocketAddr>, TrackerError> {
    let (chunks, rest) = bytes.as_chunks::<6>();
    if !rest.is_empty() {
        return Err(TrackerError::Malformed(
            "compact peers length not a multiple of 6",
        ));
    }
    Ok(chunks
        .iter()
        .map(|&[a, b, c, d, p0, p1]| {
            SocketAddr::from((Ipv4Addr::new(a, b, c, d), u16::from_be_bytes([p0, p1])))
        })
        .collect())
}

/// Parses BEP 7 compact IPv6 peers (18 bytes each).
///
/// # Errors
/// [`TrackerError::Malformed`] if the length is not a multiple of 18.
pub fn parse_compact_v6(bytes: &[u8]) -> Result<Vec<SocketAddr>, TrackerError> {
    let (chunks, rest) = bytes.as_chunks::<18>();
    if !rest.is_empty() {
        return Err(TrackerError::Malformed(
            "compact peers6 length not a multiple of 18",
        ));
    }
    Ok(chunks
        .iter()
        .filter_map(|c| {
            let (ip, port) = c.split_first_chunk::<16>()?;
            let port = u16::from_be_bytes(*port.first_chunk::<2>()?);
            Some(SocketAddr::from((Ipv6Addr::from(*ip), port)))
        })
        .collect())
}

/// A `{ip, port}` dictionary peer; entries with bad fields are skipped.
fn parse_dict_peer(v: &Value<'_>) -> Option<SocketAddr> {
    let ip = v.get(b"ip")?.as_str()?.parse().ok()?;
    let port = u16::try_from(v.get(b"port")?.as_int()?).ok()?;
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compact_and_dict_peers() {
        let body = b"d8:completei5e10:incompletei2e8:intervali1800e12:min intervali60e5:peers12:\x0a\x00\x00\x01\x1a\xe1\x7f\x00\x00\x01\x00\x506:peers618:\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x1a\xe1e";
        let r = parse_announce_response(body).unwrap();
        assert_eq!(r.interval, Some(Duration::from_secs(1800)));
        assert_eq!(r.min_interval, Some(Duration::from_secs(60)));
        assert_eq!((r.seeders, r.leechers), (Some(5), Some(2)));
        let peers: Vec<String> = r.peers.iter().map(ToString::to_string).collect();
        assert_eq!(peers, ["10.0.0.1:6881", "127.0.0.1:80", "[::1]:6881"]);

        let body = b"d8:intervali60e5:peersld2:ip9:127.0.0.14:porti7eed2:ip3:bad4:porti1eed2:ip3:::14:porti99999eeee";
        let r = parse_announce_response(body).unwrap();
        assert_eq!(r.peers, vec!["127.0.0.1:7".parse().unwrap()]);
    }

    #[test]
    fn reports_failures_and_garbage() {
        assert!(matches!(
            parse_announce_response(b"d14:failure reason6:no waye"),
            Err(TrackerError::Failure(m)) if m == "no way"
        ));
        assert!(matches!(
            parse_announce_response(b"<html>"),
            Err(TrackerError::Malformed(_))
        ));
        assert!(matches!(
            parse_announce_response(b"li1ee"),
            Err(TrackerError::Malformed(_))
        ));
        assert!(matches!(
            parse_announce_response(b"d5:peers5:abcdee"),
            Err(TrackerError::Malformed(_))
        ));
        assert!(matches!(
            parse_announce_response(b"d5:peersi1ee"),
            Err(TrackerError::Malformed(_))
        ));
        assert!(matches!(
            parse_announce_response(&vec![b'x'; MAX_RESPONSE_LEN + 1]),
            Err(TrackerError::TooLarge(_))
        ));
        // Negative intervals are ignored rather than wrapping.
        let r = parse_announce_response(b"d8:intervali-5e5:peers0:e").unwrap();
        assert_eq!(r.interval, None);
        assert!(r.peers.is_empty());
    }

    #[test]
    fn percent_encoding() {
        let mut s = String::new();
        percent_encode(b"aZ09-._~ /\xff", &mut s);
        assert_eq!(s, "aZ09-._~%20%2F%FF");
    }
}
