//! Sans-IO BitTorrent peer wire protocol for the Dravity engine.
//!
//! * [`Handshake`] / [`Reserved`] — the 68-byte handshake and extension flags.
//! * [`Message`] / [`MessageDecoder`] — BEP 3 messages, BEP 5 `port`, BEP 6
//!   fast messages, and BEP 10 extended frames, with streaming zero-copy
//!   decoding and bounded frame sizes.
//! * [`ExtendedHandshake`] — the bencode payload of extended message 0.
//!
//! The crate performs no I/O: feed it bytes from any transport (Tokio TCP,
//! uTP, io_uring) and write out what it encodes. Further message ids stay
//! [`Message::Unknown`]; see [`message`].

mod error;
mod extended;
mod handshake;
pub mod message;

pub use error::WireError;
pub use extended::{ExtendedError, ExtendedHandshake};
pub use handshake::{HANDSHAKE_LEN, Handshake, PROTOCOL, Reserved};
pub use message::{
    BlockRequest, DEFAULT_MAX_FRAME_LEN, Message, MessageDecoder, MessageId, bitfield_bit,
    check_bitfield,
};
