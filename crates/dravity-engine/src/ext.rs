//! BEP 10 extension protocol glue shared by the torrent and metadata states.

use std::net::{IpAddr, SocketAddr};

use bytes::Bytes;
use dravity_wire::{
    ExtendedHandshake, Holepunch, LT_DONTHAVE, LT_TEX, Message, PexMessage, PexPeer,
    TrackerExchange, UT_HOLEPUNCH, UT_METADATA, UT_PEX,
};

use crate::types::Config;

/// Extended message id peers use to send us `ut_metadata` messages.
pub(crate) const OUR_UT_METADATA: u8 = 1;
/// Extended message id peers use to send us `ut_pex` messages.
pub(crate) const OUR_UT_PEX: u8 = 2;
/// Extended message id peers use to send us BEP 54 `lt_donthave`.
pub(crate) const OUR_LT_DONTHAVE: u8 = 3;
/// Extended message id peers use to send us `lt_tex` tracker lists.
pub(crate) const OUR_LT_TEX: u8 = 4;
/// Extended message id peers use to send us BEP 55 `ut_holepunch`.
pub(crate) const OUR_UT_HOLEPUNCH: u8 = 5;
/// Most addresses remembered as "learned over PEX from a peer that can
/// relay a holepunch".
pub(crate) const MAX_HOLEPUNCH_SOURCES: usize = 1_000;
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
    /// Id to use when sending `lt_donthave` to the peer.
    pub(crate) lt_donthave: Option<u8>,
    /// Id to use when sending `lt_tex` to the peer.
    pub(crate) lt_tex: Option<u8>,
    /// BEP 21: the peer does not want to download.
    pub(crate) upload_only: bool,
    /// Id to use when sending `ut_holepunch` to the peer.
    pub(crate) ut_holepunch: Option<u8>,
}

impl PeerExtensions {
    /// Applies a (possibly repeated) handshake. Id 0 disables an extension.
    pub(crate) fn update(&mut self, hs: &ExtendedHandshake) {
        let id = |name| hs.extension_id(name).filter(|&id| id != 0);
        self.ut_metadata = id(UT_METADATA);
        self.ut_pex = id(UT_PEX);
        self.lt_donthave = id(LT_DONTHAVE);
        self.lt_tex = id(LT_TEX);
        self.ut_holepunch = id(UT_HOLEPUNCH);
        if let Some(upload_only) = hs.upload_only {
            self.upload_only = upload_only;
        }
        if hs.metadata_size.is_some() {
            self.metadata_size = hs.metadata_size;
        }
        if hs.request_queue.is_some() {
            self.reqq = hs.request_queue.filter(|&q| q > 0);
        }
    }
}

/// Our extension handshake to the peer at `peer`: the extensions we speak,
/// our listening port (`p`), how many requests we queue (`reqq`), and the
/// address we see the peer at (`yourip`, which helps it learn its public
/// address behind NAT).
/// `upload_only` (BEP 21) tells the peer we will not download from it.
pub(crate) fn our_handshake(
    config: &Config,
    metadata_size: Option<u64>,
    peer: SocketAddr,
    upload_only: bool,
) -> Message {
    let mut extensions = vec![
        (UT_METADATA.to_owned(), OUR_UT_METADATA),
        (LT_DONTHAVE.to_owned(), OUR_LT_DONTHAVE),
    ];
    if config.pex {
        extensions.push((UT_PEX.to_owned(), OUR_UT_PEX));
    }
    if config.tracker_exchange {
        extensions.push((LT_TEX.to_owned(), OUR_LT_TEX));
    }
    if config.holepunch {
        extensions.push((UT_HOLEPUNCH.to_owned(), OUR_UT_HOLEPUNCH));
    }
    extensions.sort();
    let your_ip = match peer.ip() {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) => v4.octets().to_vec(),
            None => ip.octets().to_vec(),
        },
    };
    let hs = ExtendedHandshake {
        extensions,
        listen_port: config.listen_port,
        client: Some(
            concat!("Dravity ", env!("CARGO_PKG_VERSION"))
                .as_bytes()
                .to_vec(),
        ),
        your_ip: Some(your_ip),
        request_queue: u32::try_from(config.max_upload_queue)
            .ok()
            .filter(|&q| q > 0),
        metadata_size,
        ipv4: None,
        ipv6: None,
        upload_only: upload_only.then_some(true),
    };
    Message::Extended {
        id: HANDSHAKE_ID,
        // Only `metadata_size` can fail to encode, and it comes from a
        // buffer length, which always fits in an `i64`.
        payload: Bytes::from(hs.encode().unwrap_or_default()),
    }
}

/// Peers to add from a received PEX message, with their flags (at most
/// [`PEX_MAX_ACCEPTED`]); `None` if the payload is malformed.
pub(crate) fn pex_added(payload: &[u8]) -> Option<Vec<PexPeer>> {
    let msg = PexMessage::decode(payload).ok()?;
    Some(msg.added.into_iter().take(PEX_MAX_ACCEPTED).collect())
}

/// A PEX message announcing `added` (with BEP 11 flags) and `dropped`.
pub(crate) fn pex_message(id: u8, added: &[(SocketAddr, u8)], dropped: &[SocketAddr]) -> Message {
    let msg = PexMessage {
        added: added
            .iter()
            .map(|&(addr, flags)| PexPeer {
                addr,
                flags: Some(flags),
            })
            .collect(),
        dropped: dropped.to_vec(),
    };
    Message::Extended {
        id,
        payload: Bytes::from(msg.encode()),
    }
}

/// An `lt_tex` message listing `trackers`.
pub(crate) fn tex_message(id: u8, trackers: &[String]) -> Message {
    let msg = TrackerExchange {
        added: trackers.to_vec(),
    };
    Message::Extended {
        id,
        payload: Bytes::from(msg.encode()),
    }
}

/// A `ut_holepunch` message.
pub(crate) fn holepunch_message(id: u8, msg: Holepunch) -> Message {
    Message::Extended {
        id,
        payload: Bytes::from(msg.encode()),
    }
}
