//! Local Service Discovery (BEP 14) for Dravity.
//!
//! Peers on the same network find each other by multicasting
//! `BT-SEARCH` messages to `239.192.152.143:6771` (and
//! `[ff15::efc0:988f]:6771`). [`codec`] encodes and parses them without
//! I/O; [`Lsd`] runs the sockets: it announces the info hashes it is
//! given and reports the peers it hears about.
//!
//! Private torrents (BEP 27) must not use LSD; the caller decides which
//! hashes to announce and which discoveries to accept.

pub mod codec;
mod service;

pub use codec::{Announce, LsdError};
pub use service::{Discovered, Lsd, LsdConfig};
