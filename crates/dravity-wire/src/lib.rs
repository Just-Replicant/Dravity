//! Sans-IO BitTorrent peer wire protocol for the Dravity engine.
//!
//! * [`Handshake`] / [`Reserved`] — the 68-byte handshake and extension flags.
//! * [`Message`] / [`MessageDecoder`] — BEP 3 messages (plus BEP 5 `port`)
//!   with streaming, zero-copy decoding and bounded frame sizes.
//!
//! The crate performs no I/O: feed it bytes from any transport (Tokio TCP,
//! uTP, io_uring) and write out what it encodes. BEP 6 and BEP 10 messages
//! will be added in later phases; see [`message`] for how.

mod error;
mod handshake;
pub mod message;

pub use error::WireError;
pub use handshake::{HANDSHAKE_LEN, Handshake, PROTOCOL, Reserved};
pub use message::{BlockRequest, DEFAULT_MAX_FRAME_LEN, Message, MessageDecoder, MessageId};
