//! BEP 15 UDP tracker packet codec (sans-IO).
//!
//! A UDP announce is two round trips: `connect` obtains a connection id,
//! then `announce` uses it. Every response echoes the request's transaction
//! id, which callers must check (see [`parse_connect_response`] and
//! [`parse_announce_response`]).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use crate::error::TrackerError;
use crate::http::{parse_compact_v4, parse_compact_v6};
use crate::types::{AnnounceRequest, AnnounceResponse};

/// Magic constant identifying the protocol in a connect request.
pub const PROTOCOL_ID: u64 = 0x0417_2710_1980;

/// Largest datagram we read (enough for ~200 IPv6 peers or ~1,600 IPv4).
pub const MAX_PACKET_LEN: usize = 30 * 1024;

const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

/// Encodes a 16-byte connect request.
#[must_use]
pub fn connect_request(transaction_id: u32) -> [u8; 16] {
    let mut out = [0u8; 16];
    let (proto, rest) = out.split_at_mut(8);
    proto.copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    let (action, tid) = rest.split_at_mut(4);
    action.copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    tid.copy_from_slice(&transaction_id.to_be_bytes());
    out
}

/// Encodes a 98-byte announce request.
#[must_use]
pub fn announce_request(connection_id: u64, transaction_id: u32, req: &AnnounceRequest) -> Vec<u8> {
    let mut out = Vec::with_capacity(98);
    out.extend_from_slice(&connection_id.to_be_bytes());
    out.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    out.extend_from_slice(&transaction_id.to_be_bytes());
    out.extend_from_slice(req.info_hash.as_bytes());
    out.extend_from_slice(req.peer_id.as_bytes());
    out.extend_from_slice(&req.downloaded.to_be_bytes());
    out.extend_from_slice(&req.left.to_be_bytes());
    out.extend_from_slice(&req.uploaded.to_be_bytes());
    out.extend_from_slice(&req.event.as_udp().to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // IP: use the sender's.
    out.extend_from_slice(&req.key.to_be_bytes());
    let num_want = req
        .num_want
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(-1);
    out.extend_from_slice(&num_want.to_be_bytes());
    out.extend_from_slice(&req.port.to_be_bytes());
    out
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Checks the header shared by every response and returns the action.
fn header(packet: &[u8], transaction_id: u32) -> Result<u32, TrackerError> {
    let (Some(action), Some(tid)) = (u32_at(packet, 0), u32_at(packet, 4)) else {
        return Err(TrackerError::Malformed("UDP response shorter than 8 bytes"));
    };
    if tid != transaction_id {
        return Err(TrackerError::TransactionMismatch);
    }
    if action == ACTION_ERROR {
        let msg = packet.get(8..).unwrap_or_default();
        return Err(TrackerError::Failure(
            String::from_utf8_lossy(msg).into_owned(),
        ));
    }
    Ok(action)
}

/// Parses a connect response and returns the connection id.
///
/// # Errors
/// [`TrackerError::Failure`] for an error response,
/// [`TrackerError::TransactionMismatch`] for another request's answer, and
/// [`TrackerError::Malformed`] for a wrong action or length.
pub fn parse_connect_response(packet: &[u8], transaction_id: u32) -> Result<u64, TrackerError> {
    if header(packet, transaction_id)? != ACTION_CONNECT {
        return Err(TrackerError::Malformed("unexpected UDP action"));
    }
    packet
        .get(8..16)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or(TrackerError::Malformed("UDP connect response too short"))
}

/// Parses an announce response. `ipv6` selects 18-byte peers; BEP 15 sends
/// peers of the address family the request arrived over.
///
/// # Errors
/// As for [`parse_connect_response`], plus a peer list of the wrong length.
pub fn parse_announce_response(
    packet: &[u8],
    transaction_id: u32,
    ipv6: bool,
) -> Result<AnnounceResponse, TrackerError> {
    if header(packet, transaction_id)? != ACTION_ANNOUNCE {
        return Err(TrackerError::Malformed("unexpected UDP action"));
    }
    let (Some(interval), Some(leechers), Some(seeders)) =
        (u32_at(packet, 8), u32_at(packet, 12), u32_at(packet, 16))
    else {
        return Err(TrackerError::Malformed("UDP announce response too short"));
    };
    let peers = packet.get(20..).unwrap_or_default();
    let peers = if ipv6 {
        parse_compact_v6(peers)?
    } else {
        parse_compact_v4(peers)?
    };
    Ok(AnnounceResponse {
        interval: Some(Duration::from_secs(u64::from(interval))),
        min_interval: None,
        peers,
        seeders: Some(seeders),
        leechers: Some(leechers),
        warning: None,
    })
}

/// Encodes an announce response (used by tests and fuzzing).
#[must_use]
pub fn encode_announce_response(
    transaction_id: u32,
    interval: u32,
    leechers: u32,
    seeders: u32,
    peers: &[SocketAddr],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + peers.len() * 18);
    for word in [ACTION_ANNOUNCE, transaction_id, interval, leechers, seeders] {
        out.extend_from_slice(&word.to_be_bytes());
    }
    for peer in peers {
        match peer {
            SocketAddr::V4(a) => out.extend_from_slice(&Ipv4Addr::octets(a.ip())),
            SocketAddr::V6(a) => out.extend_from_slice(&Ipv6Addr::octets(a.ip())),
        }
        out.extend_from_slice(&peer.port().to_be_bytes());
    }
    out
}

/// Encodes a connect response (used by tests and fuzzing).
#[must_use]
pub fn encode_connect_response(transaction_id: u32, connection_id: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    let (head, id) = out.split_at_mut(8);
    let (action, tid) = head.split_at_mut(4);
    action.copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    tid.copy_from_slice(&transaction_id.to_be_bytes());
    id.copy_from_slice(&connection_id.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use dravity_core::{InfoHash, PeerId};

    use super::*;
    use crate::types::AnnounceEvent;

    fn req() -> AnnounceRequest {
        AnnounceRequest {
            info_hash: InfoHash::new([1; 20]),
            peer_id: PeerId::new([2; 20]),
            port: 6881,
            uploaded: 3,
            downloaded: 4,
            left: 5,
            event: AnnounceEvent::Started,
            num_want: None,
            key: 7,
        }
    }

    #[test]
    fn connect_roundtrip() {
        let r = connect_request(0xdead_beef);
        assert_eq!(&r[..8], &[0, 0, 0x04, 0x17, 0x27, 0x10, 0x19, 0x80]);
        let resp = encode_connect_response(0xdead_beef, 42);
        assert_eq!(parse_connect_response(&resp, 0xdead_beef).unwrap(), 42);
        assert!(parse_connect_response(&resp, 1).is_err());
        assert!(parse_connect_response(&resp[..12], 0xdead_beef).is_err());
    }

    #[test]
    fn announce_layout() {
        let p = announce_request(9, 10, &req());
        assert_eq!(p.len(), 98);
        assert_eq!(&p[80..84], &2u32.to_be_bytes(), "started");
        assert_eq!(&p[92..96], &(-1i32).to_be_bytes(), "default num_want");
        assert_eq!(&p[96..98], &6881u16.to_be_bytes());
    }

    #[test]
    fn announce_response_roundtrip() {
        let peers: Vec<SocketAddr> = vec!["1.2.3.4:5".parse().unwrap()];
        let pkt = encode_announce_response(3, 900, 1, 2, &peers);
        let r = parse_announce_response(&pkt, 3, false).unwrap();
        assert_eq!(r.peers, peers);
        assert_eq!(r.interval, Some(Duration::from_secs(900)));
        assert_eq!((r.seeders, r.leechers), (Some(2), Some(1)));
        assert!(parse_announce_response(&pkt, 3, true).is_err());
        let v6: Vec<SocketAddr> = vec!["[::1]:5".parse().unwrap()];
        let pkt = encode_announce_response(3, 900, 1, 2, &v6);
        assert_eq!(parse_announce_response(&pkt, 3, true).unwrap().peers, v6);
    }

    #[test]
    fn error_action() {
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&3u32.to_be_bytes());
        pkt.extend_from_slice(&8u32.to_be_bytes());
        pkt.extend_from_slice(b"go away");
        assert!(matches!(
            parse_announce_response(&pkt, 8, false),
            Err(TrackerError::Failure(m)) if m == "go away"
        ));
    }
}
