//! The uTP packet format (BEP 29).
//!
//! ```text
//! 0       4       8               16              24              32
//! +-------+-------+---------------+---------------+---------------+
//! | type  | ver   | extension     | connection_id                 |
//! +-------+-------+---------------+---------------+---------------+
//! | timestamp_microseconds                                        |
//! +---------------+---------------+---------------+---------------+
//! | timestamp_difference_microseconds                             |
//! +---------------+---------------+---------------+---------------+
//! | wnd_size                                                      |
//! +---------------+---------------+---------------+---------------+
//! | seq_nr                        | ack_nr                        |
//! +---------------+---------------+---------------+---------------+
//! ```
//!
//! An extension chain follows the header: each entry is the next
//! extension's type, the entry's length and its bytes. Selective ACK
//! (type 1) is understood; other extensions are skipped.

use bytes::{BufMut, Bytes, BytesMut};

/// Fixed header length.
pub const HEADER_LEN: usize = 20;
/// The only protocol version.
pub const VERSION: u8 = 1;
const EXT_NONE: u8 = 0;
const EXT_SACK: u8 = 1;

/// Packet type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketType {
    /// Payload data.
    Data,
    /// Last packet of a stream.
    Fin,
    /// Acknowledgement without data.
    State,
    /// Forcibly terminates a connection.
    Reset,
    /// Opens a connection.
    Syn,
}

impl PacketType {
    const fn to_wire(self) -> u8 {
        match self {
            Self::Data => 0,
            Self::Fin => 1,
            Self::State => 2,
            Self::Reset => 3,
            Self::Syn => 4,
        }
    }

    const fn from_wire(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Data,
            1 => Self::Fin,
            2 => Self::State,
            3 => Self::Reset,
            4 => Self::Syn,
            _ => return None,
        })
    }
}

/// Why bytes are not a uTP packet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PacketError {
    /// Shorter than a header or an extension it announces.
    #[error("truncated packet")]
    Truncated,
    /// Not version 1.
    #[error("unsupported uTP version {0}")]
    Version(u8),
    /// Unknown packet type.
    #[error("unknown uTP packet type {0}")]
    Type(u8),
    /// A selective ACK whose length is not a non-zero multiple of 4.
    #[error("invalid selective ack length {0}")]
    SackLength(usize),
}

/// One uTP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Packet type.
    pub ty: PacketType,
    /// Connection id (the receiver's id for this connection).
    pub conn_id: u16,
    /// Sender's clock, microseconds (wraps).
    pub timestamp: u32,
    /// Sender's last measured one-way delay of our packets, microseconds.
    pub timestamp_diff: u32,
    /// Bytes the sender can still receive.
    pub wnd_size: u32,
    /// Sequence number.
    pub seq_nr: u16,
    /// Last in-order sequence number received.
    pub ack_nr: u16,
    /// Selective ACK bitmask: bit `i` acknowledges `ack_nr + 2 + i`
    /// (least significant bit of each byte first). Empty when absent.
    pub sack: Vec<u8>,
    /// Payload (data and fin packets).
    pub payload: Bytes,
}

impl Packet {
    /// A packet of type `ty` with every other field zero.
    #[must_use]
    pub fn new(ty: PacketType, conn_id: u16) -> Self {
        Self {
            ty,
            conn_id,
            timestamp: 0,
            timestamp_diff: 0,
            wnd_size: 0,
            seq_nr: 0,
            ack_nr: 0,
            sack: Vec::new(),
            payload: Bytes::new(),
        }
    }

    /// Encoded length.
    #[must_use]
    pub fn len(&self) -> usize {
        HEADER_LEN + self.sack_len().map_or(0, |n| n + 2) + self.payload.len()
    }

    /// Never true: a header is always present.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// SACK bytes on the wire, padded to a multiple of 4 (at most 252).
    fn sack_len(&self) -> Option<usize> {
        (!self.sack.is_empty()).then(|| self.sack.len().div_ceil(4).min(63) * 4)
    }

    /// Whether the selective ACK acknowledges `seq`.
    #[must_use]
    pub fn sack_has(&self, seq: u16) -> bool {
        let i = usize::from(seq.wrapping_sub(self.ack_nr).wrapping_sub(2));
        self.sack
            .get(i / 8)
            .is_some_and(|byte| byte & (1 << (i % 8)) != 0)
    }

    /// Appends the encoding to `out`.
    pub fn encode_into(&self, out: &mut BytesMut) {
        out.reserve(self.len());
        out.put_u8((self.ty.to_wire() << 4) | VERSION);
        let sack_len = self.sack_len();
        out.put_u8(if sack_len.is_some() {
            EXT_SACK
        } else {
            EXT_NONE
        });
        out.put_u16(self.conn_id);
        out.put_u32(self.timestamp);
        out.put_u32(self.timestamp_diff);
        out.put_u32(self.wnd_size);
        out.put_u16(self.seq_nr);
        out.put_u16(self.ack_nr);
        if let Some(len) = sack_len {
            out.put_u8(EXT_NONE);
            // At most 252: fits.
            out.put_u8(u8::try_from(len).unwrap_or(252));
            let bytes = self
                .sack
                .get(..len.min(self.sack.len()))
                .unwrap_or_default();
            out.put_slice(bytes);
            out.put_bytes(0, len - bytes.len());
        }
        out.put_slice(&self.payload);
    }

    /// The encoding.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(self.len());
        self.encode_into(&mut out);
        out.freeze()
    }

    /// Decodes a whole datagram.
    ///
    /// # Errors
    /// [`PacketError`] if `data` is not a valid packet.
    pub fn decode(data: &[u8]) -> Result<Self, PacketError> {
        let header: &[u8; HEADER_LEN] = data
            .get(..HEADER_LEN)
            .and_then(|h| h.try_into().ok())
            .ok_or(PacketError::Truncated)?;
        let [
            b0,
            ext,
            c0,
            c1,
            t0,
            t1,
            t2,
            t3,
            d0,
            d1,
            d2,
            d3,
            w0,
            w1,
            w2,
            w3,
            s0,
            s1,
            a0,
            a1,
        ] = *header;
        if b0 & 0x0f != VERSION {
            return Err(PacketError::Version(b0 & 0x0f));
        }
        let ty = PacketType::from_wire(b0 >> 4).ok_or(PacketError::Type(b0 >> 4))?;
        let mut sack = Vec::new();
        let mut next = ext;
        let mut rest = data.get(HEADER_LEN..).unwrap_or_default();
        while next != EXT_NONE {
            let [kind, len, tail @ ..] = rest else {
                return Err(PacketError::Truncated);
            };
            let len = usize::from(*len);
            let body = tail.get(..len).ok_or(PacketError::Truncated)?;
            if next == EXT_SACK {
                if len == 0 || len % 4 != 0 {
                    return Err(PacketError::SackLength(len));
                }
                sack = body.to_vec();
            }
            next = *kind;
            rest = tail.get(len..).unwrap_or_default();
        }
        Ok(Self {
            ty,
            conn_id: u16::from_be_bytes([c0, c1]),
            timestamp: u32::from_be_bytes([t0, t1, t2, t3]),
            timestamp_diff: u32::from_be_bytes([d0, d1, d2, d3]),
            wnd_size: u32::from_be_bytes([w0, w1, w2, w3]),
            seq_nr: u16::from_be_bytes([s0, s1]),
            ack_nr: u16::from_be_bytes([a0, a1]),
            sack,
            payload: Bytes::copy_from_slice(rest),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout() {
        let mut p = Packet::new(PacketType::Syn, 0x1234);
        p.timestamp = 0x0102_0304;
        p.timestamp_diff = 5;
        p.wnd_size = 0x10_0000;
        p.seq_nr = 7;
        p.ack_nr = 0xffff;
        let bytes = p.encode();
        assert_eq!(
            &bytes[..],
            &[
                0x41, 0, 0x12, 0x34, 1, 2, 3, 4, 0, 0, 0, 5, 0, 0x10, 0, 0, 0, 7, 0xff, 0xff
            ]
        );
        assert_eq!(Packet::decode(&bytes), Ok(p));
    }

    #[test]
    fn sack_is_padded_and_read() {
        let mut p = Packet::new(PacketType::State, 1);
        p.ack_nr = 10;
        p.sack = vec![0b0000_0101];
        let bytes = p.encode();
        assert_eq!(bytes.len(), HEADER_LEN + 6);
        assert_eq!(&bytes[HEADER_LEN..], &[0, 4, 5, 0, 0, 0]);
        let back = Packet::decode(&bytes).unwrap();
        assert!(back.sack_has(12) && !back.sack_has(13) && back.sack_has(14));
        assert!(!back.sack_has(11) && !back.sack_has(10) && !back.sack_has(9));
    }

    #[test]
    fn unknown_extensions_are_skipped_and_bad_ones_refused() {
        let mut bytes = Packet::new(PacketType::Data, 3).encode().to_vec();
        bytes[1] = 9; // unknown extension of 2 bytes, then payload
        bytes.extend_from_slice(&[0, 2, 0xaa, 0xbb, b'h', b'i']);
        let p = Packet::decode(&bytes).unwrap();
        assert_eq!(&p.payload[..], b"hi");
        assert!(p.sack.is_empty());

        let mut bad = Packet::new(PacketType::Data, 3).encode().to_vec();
        bad[1] = EXT_SACK;
        bad.extend_from_slice(&[0, 3, 1, 2, 3]);
        assert_eq!(Packet::decode(&bad), Err(PacketError::SackLength(3)));
        bad.truncate(HEADER_LEN + 1);
        assert_eq!(Packet::decode(&bad), Err(PacketError::Truncated));
        assert_eq!(Packet::decode(&[0x41; 5]), Err(PacketError::Truncated));
        let mut v2 = Packet::new(PacketType::Data, 3).encode().to_vec();
        v2[0] = 0x02;
        assert_eq!(Packet::decode(&v2), Err(PacketError::Version(2)));
        v2[0] = 0x71;
        assert_eq!(Packet::decode(&v2), Err(PacketError::Type(7)));
    }
}
