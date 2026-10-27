//! Sans-IO BitTorrent peer wire protocol for the Dravity engine.
//!
//! * [`Handshake`] / [`Reserved`] — the 68-byte handshake and extension flags.
//! * [`Message`] / [`MessageDecoder`] — BEP 3 messages, BEP 5 `port`, BEP 6
//!   fast messages, and BEP 10 extended frames, with streaming zero-copy
//!   decoding and bounded frame sizes.
//! * [`ExtendedHandshake`] — the bencode payload of extended message 0.
//! * [`MetadataMessage`] — BEP 9 `ut_metadata` (metadata for magnet links).
//! * [`PexMessage`] — BEP 11 `ut_pex` peer exchange.
//!
//! The crate performs no I/O: feed it bytes from any transport (Tokio TCP,
//! uTP, io_uring) and write out what it encodes. Further message ids stay
//! [`Message::Unknown`]; see [`message`].

mod error;
mod extended;
mod handshake;
pub mod message;
mod metadata;
mod pex;

pub use error::WireError;
pub use extended::{ExtendedError, ExtendedHandshake};
pub use handshake::{HANDSHAKE_LEN, Handshake, PROTOCOL, Reserved};
pub use message::{
    BlockRequest, DEFAULT_MAX_FRAME_LEN, Message, MessageDecoder, MessageId, bitfield_bit,
    check_bitfield,
};
pub use metadata::{METADATA_PIECE_LEN, MetadataMessage, UT_METADATA, metadata_piece_count};
pub use pex::{
    MAX_PEX_ENTRIES, PEX_ENCRYPTION, PEX_HOLEPUNCH, PEX_REACHABLE, PEX_SEED, PEX_UTP, PexMessage,
    PexPeer, UT_PEX,
};
