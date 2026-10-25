//! The 68-byte BitTorrent handshake.

use bytes::{Buf, BufMut, BytesMut};
use dravity_core::{InfoHash, PeerId};

use crate::error::WireError;

/// Protocol identifier sent in every handshake.
pub const PROTOCOL: &[u8; 19] = b"BitTorrent protocol";

/// Encoded handshake length: 1 + 19 + 8 + 20 + 20.
pub const HANDSHAKE_LEN: usize = 68;

/// The 8 reserved handshake bytes, used to advertise extensions.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Reserved([u8; 8]);

impl Reserved {
    /// Byte/bit of the BEP 10 extension protocol flag.
    const EXTENSION_PROTOCOL: (usize, u8) = (5, 0x10);
    /// Byte/bit of the BEP 6 fast extension flag.
    const FAST: (usize, u8) = (7, 0x04);
    /// Byte/bit of the BEP 5 DHT flag.
    const DHT: (usize, u8) = (7, 0x01);

    /// No extensions.
    #[must_use]
    pub const fn new() -> Self {
        Self([0; 8])
    }

    /// Wraps raw bytes.
    #[must_use]
    pub const fn from_bytes(b: [u8; 8]) -> Self {
        Self(b)
    }

    /// Raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 8] {
        &self.0
    }

    fn get(&self, (byte, mask): (usize, u8)) -> bool {
        self.0.get(byte).is_some_and(|b| b & mask != 0)
    }

    fn set(mut self, (byte, mask): (usize, u8), on: bool) -> Self {
        if let Some(b) = self.0.get_mut(byte) {
            if on {
                *b |= mask;
            } else {
                *b &= !mask;
            }
        }
        self
    }

    /// Whether the peer supports the BEP 10 extension protocol.
    #[must_use]
    pub fn extension_protocol(&self) -> bool {
        self.get(Self::EXTENSION_PROTOCOL)
    }

    /// Sets the BEP 10 extension protocol flag.
    #[must_use]
    pub fn with_extension_protocol(self, on: bool) -> Self {
        self.set(Self::EXTENSION_PROTOCOL, on)
    }

    /// Whether the peer supports the BEP 6 fast extension.
    #[must_use]
    pub fn fast(&self) -> bool {
        self.get(Self::FAST)
    }

    /// Sets the BEP 6 fast extension flag.
    #[must_use]
    pub fn with_fast(self, on: bool) -> Self {
        self.set(Self::FAST, on)
    }

    /// Whether the peer runs a BEP 5 DHT node.
    #[must_use]
    pub fn dht(&self) -> bool {
        self.get(Self::DHT)
    }

    /// Sets the BEP 5 DHT flag.
    #[must_use]
    pub fn with_dht(self, on: bool) -> Self {
        self.set(Self::DHT, on)
    }
}

impl std::fmt::Debug for Reserved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Reserved({:016x})", u64::from_be_bytes(self.0))
    }
}

/// A peer handshake.
///
/// ```
/// use bytes::BytesMut;
/// use dravity_core::{InfoHash, PeerId};
/// use dravity_wire::{Handshake, Reserved};
///
/// let hs = Handshake::new(InfoHash::new([1; 20]), PeerId::new([2; 20]))
///     .with_reserved(Reserved::new().with_extension_protocol(true));
/// let mut buf = BytesMut::new();
/// hs.encode(&mut buf);
/// assert_eq!(buf.len(), 68);
/// assert_eq!(Handshake::decode(&mut buf).unwrap(), Some(hs));
/// assert!(buf.is_empty());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handshake {
    /// Extension flags.
    pub reserved: Reserved,
    /// Torrent being requested.
    pub info_hash: InfoHash,
    /// Sender's peer id.
    pub peer_id: PeerId,
}

impl Handshake {
    /// Creates a handshake with no extension flags.
    #[must_use]
    pub const fn new(info_hash: InfoHash, peer_id: PeerId) -> Self {
        Self {
            reserved: Reserved::new(),
            info_hash,
            peer_id,
        }
    }

    /// Replaces the reserved bytes.
    #[must_use]
    pub const fn with_reserved(mut self, reserved: Reserved) -> Self {
        self.reserved = reserved;
        self
    }

    /// Appends the 68-byte encoding to `dst`.
    pub fn encode(&self, dst: &mut impl BufMut) {
        dst.put_u8(19);
        dst.put_slice(PROTOCOL);
        dst.put_slice(self.reserved.as_bytes());
        dst.put_slice(self.info_hash.as_bytes());
        dst.put_slice(self.peer_id.as_bytes());
    }

    /// Returns the 68-byte encoding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; HANDSHAKE_LEN] {
        let mut out = [0u8; HANDSHAKE_LEN];
        self.encode(&mut out.as_mut_slice());
        out
    }

    /// Streaming decode: consumes a handshake from the front of `src` when all
    /// 68 bytes are present, returns `Ok(None)` if more bytes are needed.
    ///
    /// The protocol prefix is checked as soon as bytes arrive so that a
    /// non-BitTorrent peer is rejected without waiting for 68 bytes.
    ///
    /// # Errors
    /// [`WireError::InvalidProtocol`] if the prefix is not `\x13BitTorrent protocol`.
    pub fn decode(src: &mut BytesMut) -> Result<Option<Self>, WireError> {
        let prefix_len = src.len().min(20);
        let expected = [&[19u8][..], &PROTOCOL[..]].concat();
        if src.get(..prefix_len) != expected.get(..prefix_len) {
            return Err(WireError::InvalidProtocol);
        }
        if src.len() < HANDSHAKE_LEN {
            return Ok(None);
        }
        src.advance(20);
        let mut reserved = [0u8; 8];
        src.copy_to_slice(&mut reserved);
        let mut info_hash = [0u8; 20];
        src.copy_to_slice(&mut info_hash);
        let mut peer_id = [0u8; 20];
        src.copy_to_slice(&mut peer_id);
        Ok(Some(Self {
            reserved: Reserved::from_bytes(reserved),
            info_hash: InfoHash::new(info_hash),
            peer_id: PeerId::new(peer_id),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_bits_match_beps() {
        let r = Reserved::new()
            .with_extension_protocol(true)
            .with_fast(true)
            .with_dht(true);
        assert_eq!(r.as_bytes(), &[0, 0, 0, 0, 0, 0x10, 0, 0x05]);
        assert!(r.extension_protocol() && r.fast() && r.dht());
        let r = r.with_fast(false);
        assert!(!r.fast() && r.dht());
        assert_eq!(format!("{r:?}"), "Reserved(0000000000100001)");
    }

    #[test]
    fn rejects_wrong_protocol_early() {
        let mut buf = BytesMut::from(&b"\x13BitTorrent"[..]);
        assert_eq!(Handshake::decode(&mut buf), Ok(None));
        let mut buf = BytesMut::from(&b"GET / HTTP/1.1"[..]);
        assert_eq!(Handshake::decode(&mut buf), Err(WireError::InvalidProtocol));
        let mut buf = BytesMut::new();
        assert_eq!(Handshake::decode(&mut buf), Ok(None));
    }

    #[test]
    fn decodes_in_fragments() {
        let hs = Handshake::new(
            InfoHash::new([9; 20]),
            PeerId::new(*b"-DV0001-abcdefghijkl"),
        );
        let bytes = hs.to_bytes();
        let mut buf = BytesMut::new();
        for (i, b) in bytes.iter().enumerate() {
            buf.extend_from_slice(&[*b]);
            let r = Handshake::decode(&mut buf).unwrap();
            assert_eq!(r.is_some(), i == bytes.len() - 1);
            if let Some(got) = r {
                assert_eq!(got, hs);
            }
        }
    }
}
