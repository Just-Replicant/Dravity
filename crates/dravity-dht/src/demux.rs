//! Telling DHT, uTP and UDP tracker datagrams apart, for one UDP socket
//! shared by all three.
//!
//! The first byte decides: a KRPC message is a bencoded dictionary
//! (`d`), a uTP packet starts with its type in the high nibble and
//! version 1 in the low one (`0x01`, `0x11`, … `0x41`), and a UDP tracker
//! response (BEP 15) starts with a big-endian action 0–3, so with `0x00`.

/// Who a datagram is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Datagram {
    /// A DHT (KRPC) message.
    Dht,
    /// A uTP packet.
    Utp,
    /// A UDP tracker response.
    Tracker,
    /// None of them; drop it.
    Unknown,
}

/// Classifies a received datagram by its first bytes.
///
/// ```
/// use dravity_dht::demux::{classify, Datagram};
///
/// assert_eq!(classify(b"d1:ad2:id20:aaaaaaaaaaaaaaaaaaaae1:q4:ping1:t2:aa1:y1:qe"), Datagram::Dht);
/// let mut syn = [0u8; 20];
/// syn[0] = 0x41; // ST_SYN, version 1
/// assert_eq!(classify(&syn), Datagram::Utp);
/// assert_eq!(classify(&[0, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0]), Datagram::Tracker);
/// ```
#[must_use]
pub fn classify(data: &[u8]) -> Datagram {
    match data {
        [b'd', .., b'e'] => Datagram::Dht,
        // uTP: 20-byte header, version 1, types 0..=4 (ST_DATA..ST_SYN).
        [first, ..] if data.len() >= 20 && first & 0x0f == 1 && first >> 4 <= 4 => Datagram::Utp,
        // BEP 15 responses: action 0 connect, 1 announce, 2 scrape, 3 error.
        [0, 0, 0, action, ..] if data.len() >= 8 && *action <= 3 => Datagram::Tracker,
        _ => Datagram::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind() {
        let mut utp = [0u8; 20];
        for t in 0..=4u8 {
            utp[0] = (t << 4) | 1;
            assert_eq!(classify(&utp), Datagram::Utp);
        }
        utp[0] = 0x51;
        assert_eq!(classify(&utp), Datagram::Unknown, "type 5 does not exist");
        assert_eq!(
            classify(&[0x01; 10]),
            Datagram::Unknown,
            "too short for uTP"
        );
        assert_eq!(classify(&[0, 0, 0, 3, 1, 2, 3, 4, b'x']), Datagram::Tracker);
        assert_eq!(classify(&[0, 0, 0, 4, 1, 2, 3, 4]), Datagram::Unknown);
        assert_eq!(classify(b"d"), Datagram::Unknown);
        assert_eq!(classify(b"de"), Datagram::Dht);
        assert_eq!(classify(&[]), Datagram::Unknown);
    }
}
