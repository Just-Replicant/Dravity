//! Tracker announces for the Dravity BitTorrent engine.
//!
//! * [`http`] — BEP 3 HTTP announce URL building and response parsing,
//!   including BEP 23 compact peers and BEP 7 `peers6`.
//! * [`udp`] — BEP 15 UDP tracker packet codec.
//! * [`TrackerClient`] — async announce over HTTP(S) or UDP on Tokio.
//!
//! Both codecs are sans-IO and fuzzed; only [`TrackerClient`] touches the
//! network. Responses are untrusted input: sizes are bounded and malformed
//! data is an error, never a panic.

mod client;
mod error;
pub mod http;
mod types;
pub mod udp;

pub use client::{ClientConfig, TrackerClient};
pub use error::TrackerError;
pub use types::{AnnounceEvent, AnnounceRequest, AnnounceResponse};
