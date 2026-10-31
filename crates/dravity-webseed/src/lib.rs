//! Web seeds (BEP 19, "GetRight style") for the Dravity BitTorrent engine.
//!
//! A web seed is an HTTP(S) server holding the torrent's files under their
//! torrent paths. The torrent lists such servers in `url-list` (a magnet
//! link in `ws`), and a client downloads byte ranges of the files with
//! ordinary `Range` requests and verifies them like any piece.
//!
//! * [`plan`] — sans-IO: the URL of each file ([`file_url`]) and the HTTP
//!   ranges that make up a range of the content stream ([`plan_range`]),
//!   padding files included as zeros.
//! * `WebSeedClient` — fetches a plan over HTTP(S) on Tokio (feature
//!   `client`, on by default), with bounded bodies and strict range checks.
//!
//! Responses are untrusted: a short, long or unranged body is an error, and
//! the data is still checked against the piece hashes by the session.

#[cfg(feature = "client")]
mod client;
mod error;
pub mod plan;

#[cfg(feature = "client")]
pub use client::{WebSeedClient, WebSeedConfig};
pub use error::WebSeedError;
pub use plan::{Segment, file_url, plan_range};
