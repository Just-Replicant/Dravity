//! BEP 10 extension protocol glue shared by the torrent and metadata states.

use std::net::SocketAddr;

use bytes::Bytes;
use dravity_wire::{ExtendedHandshake, Message, PexMessage, PexPeer, UT_METADATA, UT_PEX};

/// Extended message id peers use to send us `ut_metadata` messages.
pub(crate) const OUR_UT_METADATA: u8 = 1;
/// Extended message id peers use to send us `ut_pex` messages.
pub(crate) const OUR_UT_PEX: u8 = 2;
/// Extended message id of the handshake.
pub(crate) const HANDSHAKE_ID: u8 = 0;
/// Incoming PEX addresses kept from one message. The codec allows 200 per
/// list; outgoing messages stay at [`PEX_MAX_SENT`].
pub(crate) const PEX_MAX_ACCEPTED: usize = 200;
/// PEX entries sent in one list (BEP 11).
pub(crate) const PEX_MAX_SENT: usize = 50;

/// What a peer told us in its extension handshake.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PeerExtensions {
    /// Id to use when sending `ut_metadata` to the peer.
    pub(crate) ut_metadata: Option<u8>,
    /// Id to use when sending `ut_pex` to the peer.
    pub(crate) ut_pex: Option<u8>,
    /// Size of the `info` dictionary the peer can serve.
    pub(crate) metadata_size: Option<u64>,
    /// How many outstanding requests the peer accepts.
    pub(crate) reqq: Option<u32>,
}

impl PeerExtensions {
    /// Applies a (possibly repeated) handshake. Id 0 disables an extension.
    pub(crate) fn update(&mut self, hs: &ExtendedHandshake) {
        let id = |name| hs.extension_id(name).filter(|&id| id != 0);
        self.ut_metadata = id(UT_METADATA);
        self.ut_pex = id(UT_PEX);
        if hs.metadata_size.is_some() {
            self.metadata_size = hs.metadata_size;
        }
        if hs.request_queue.is_some() {
            self.reqq = hs.request_queue.filter(|&q| q > 0);
        }
    }
}

/// Our extension handshake.
pub(crate) fn our_handshake(metadata_size: Option<u64>, pex: bool) -> Message {
    let mut extensions = vec![(UT_METADATA.to_owned(), OUR_UT_METADATA)];
    if pex {
        extensions.push((UT_PEX.to_owned(), OUR_UT_PEX));
    }
    let hs = ExtendedHandshake {
        extensions,
        listen_port: None,
        client: Some(
            concat!("Dravity ", env!("CARGO_PKG_VERSION"))
                .as_bytes()
                .to_vec(),
        ),
        your_ip: None,
        request_queue: None,
        metadata_size,
        ipv4: None,
        ipv6: None,
    };
    Message::Extended {
        id: HANDSHAKE_ID,
        // Only `metadata_size` can fail to encode, and it comes from a
        // buffer length, which always fits in an `i64`.
        payload: Bytes::from(hs.encode().unwrap_or_default()),
    }
}

/// Peers to add from a received PEX message (at most
/// [`PEX_MAX_ACCEPTED`]); `None` if the payload is malformed.
pub(crate) fn pex_added(payload: &[u8]) -> Option<Vec<SocketAddr>> {
    let msg = PexMessage::decode(payload).ok()?;
    Some(
        msg.added
            .into_iter()
            .take(PEX_MAX_ACCEPTED)
            .map(|p| p.addr)
            .collect(),
    )
}

/// A PEX message announcing `added` and `dropped`.
pub(crate) fn pex_message(id: u8, added: &[SocketAddr], dropped: &[SocketAddr]) -> Message {
    let msg = PexMessage {
        added: added
            .iter()
            .map(|&addr| PexPeer {
                addr,
                // We dialed every peer we know, so each one is reachable.
                flags: dravity_wire::PEX_REACHABLE,
            })
            .collect(),
        dropped: dropped.to_vec(),
    };
    Message::Extended {
        id,
        payload: Bytes::from(msg.encode()),
    }
}
