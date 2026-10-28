//! BEP 11 `ut_pex`: peer exchange.
//!
//! Each message lists peers the sender connected to (`added`, `added6`, with
//! one flag byte each in `added.f` / `added6.f`) and peers it dropped
//! (`dropped`, `dropped6`) since its previous message, in compact form.

use std::borrow::Cow;
use std::net::SocketAddr;

use dravity_bencode::{Dict, Value};
use dravity_core::compact;

use crate::ExtendedError;

/// Extension name in the BEP 10 handshake `m` dictionary.
pub const UT_PEX: &str = "ut_pex";

/// Most entries decoded from one list. Outgoing messages stay at 50
/// (BEP 11); this cap bounds memory for hostile messages and extra entries
/// are ignored.
pub const MAX_PEX_ENTRIES: usize = 200;

/// `added.f` flag: the peer prefers encryption.
pub const PEX_ENCRYPTION: u8 = 0x01;
/// `added.f` flag: the peer is a seed or partial seed.
pub const PEX_SEED: u8 = 0x02;
/// `added.f` flag: the peer supports uTP.
pub const PEX_UTP: u8 = 0x04;
/// `added.f` flag: the peer supports holepunching.
pub const PEX_HOLEPUNCH: u8 = 0x08;
/// `added.f` flag: the sender connected to this peer itself (outgoing).
pub const PEX_REACHABLE: u8 = 0x10;

/// A peer added in a PEX message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PexPeer {
    /// Peer endpoint.
    pub addr: SocketAddr,
    /// `added.f` flags (`PEX_*`), `0` when absent.
    pub flags: u8,
}

/// One `ut_pex` message.
///
/// ```
/// use dravity_wire::{PexMessage, PexPeer};
///
/// let msg = PexMessage {
///     added: vec![PexPeer { addr: "10.0.0.1:6881".parse().unwrap(), flags: 0x10 }],
///     dropped: vec!["[::1]:51413".parse().unwrap()],
/// };
/// assert_eq!(PexMessage::decode(&msg.encode()).unwrap(), msg);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PexMessage {
    /// Newly connected peers (IPv4 first, then IPv6).
    pub added: Vec<PexPeer>,
    /// Peers the sender disconnected from (IPv4 first, then IPv6).
    pub dropped: Vec<SocketAddr>,
}

impl PexMessage {
    /// Parses an extended-message payload. Lists are truncated to
    /// [`MAX_PEX_ENTRIES`]; a missing or short flags string means no flags.
    ///
    /// # Errors
    /// [`ExtendedError`] when the payload is not a dictionary or a list is
    /// not a byte string of whole compact entries.
    pub fn decode(payload: &[u8]) -> Result<Self, ExtendedError> {
        let value = Value::decode(payload)?;
        let Some(dict) = value.as_dict() else {
            return Err(ExtendedError::NotADict);
        };
        let bytes = |field: &'static str| -> Result<&[u8], ExtendedError> {
            match dict.get(field.as_bytes()) {
                None => Ok(&[]),
                Some(v) => v.as_bytes().ok_or(ExtendedError::BadType { field }),
            }
        };
        let list = |field: &'static str, v6: bool| -> Result<Vec<SocketAddr>, ExtendedError> {
            let raw = bytes(field)?;
            let entry = if v6 { compact::V6_LEN } else { compact::V4_LEN };
            let raw = raw
                .get(..raw.len().min(MAX_PEX_ENTRIES * entry))
                .unwrap_or(raw);
            let parsed = if v6 {
                compact::parse_v6(raw)
            } else {
                compact::parse_v4(raw)
            };
            parsed.map_err(|_| ExtendedError::BadType { field })
        };
        let mut added = Vec::new();
        for (field, flags_field, v6) in [("added", "added.f", false), ("added6", "added6.f", true)]
        {
            let flags = bytes(flags_field)?;
            added.extend(
                list(field, v6)?
                    .into_iter()
                    .enumerate()
                    .map(|(i, addr)| PexPeer {
                        addr,
                        flags: flags.get(i).copied().unwrap_or(0),
                    }),
            );
        }
        let mut dropped = list("dropped", false)?;
        dropped.extend(list("dropped6", true)?);
        Ok(Self { added, dropped })
    }

    /// Encodes the extended-message payload (canonical bencode).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let (mut a4, mut a6, mut f4, mut f6) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for peer in &self.added {
            compact::push(peer.addr, &mut a4, &mut a6);
            if peer.addr.is_ipv4() {
                f4.push(peer.flags);
            } else {
                f6.push(peer.flags);
            }
        }
        let (mut d4, mut d6) = (Vec::new(), Vec::new());
        for addr in &self.dropped {
            compact::push(*addr, &mut d4, &mut d6);
        }
        let mut dict = Dict::new();
        for (key, value) in [
            (b"added".as_slice(), a4),
            (b"added.f", f4),
            (b"added6", a6),
            (b"added6.f", f6),
            (b"dropped", d4),
            (b"dropped6", d6),
        ] {
            dict.insert(Cow::Borrowed(key), Value::Bytes(Cow::Owned(value)));
        }
        Value::Dict(dict).to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_flags_and_both_families() {
        let payload = b"d5:added12:\x0a\x00\x00\x01\x1a\xe1\x0a\x00\x00\x02\x1a\xe27:added.f1:\x127:dropped6:\x01\x02\x03\x04\x00\x50e";
        let msg = PexMessage::decode(payload).unwrap();
        assert_eq!(msg.added.len(), 2);
        assert_eq!(msg.added[0].flags, PEX_SEED | PEX_REACHABLE);
        assert_eq!(msg.added[1].flags, 0, "short flags string means no flags");
        assert_eq!(msg.dropped, ["1.2.3.4:80".parse::<SocketAddr>().unwrap()]);
        assert_eq!(PexMessage::decode(b"de").unwrap(), PexMessage::default());
    }

    #[test]
    fn rejects_bad_lists_and_caps_entries() {
        assert!(matches!(
            PexMessage::decode(b"d5:added5:abcdee"),
            Err(ExtendedError::BadType { field: "added" })
        ));
        assert!(matches!(
            PexMessage::decode(b"d7:droppedi1ee"),
            Err(ExtendedError::BadType { field: "dropped" })
        ));
        let many = PexMessage {
            added: (0..300u16)
                .map(|p| PexPeer {
                    addr: SocketAddr::from(([10, 0, 0, 1], p + 1)),
                    flags: 0,
                })
                .collect(),
            dropped: Vec::new(),
        };
        assert_eq!(
            PexMessage::decode(&many.encode()).unwrap().added.len(),
            MAX_PEX_ENTRIES
        );
    }
}
