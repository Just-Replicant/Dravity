//! Core BitTorrent types for the Dravity engine.
//!
//! * [`InfoHash`], [`InfoHashV2`], [`PeerId`] — identifiers.
//! * [`Metainfo`] — validated `.torrent` parsing with exact v1 info-hash.
//! * [`Magnet`] — magnet URI parsing and formatting.
//! * [`compact`] — compact IPv4/IPv6 peer endpoints (BEP 23, BEP 7).
//! * [`layout`] — piece, block and file arithmetic.
//! * [`path`] — rejection of unsafe file paths from untrusted metainfo.
//!
//! This crate is sans-IO: it never touches the network or filesystem.

pub mod compact;
mod encoding;
mod info_hash;
pub mod layout;
pub mod magnet;
pub mod metainfo;
pub mod path;

pub use info_hash::{InfoHash, InfoHashV2, ParseHashError, PeerId};
pub use layout::{BLOCK_SIZE, BlockRef, PieceLayout};
pub use magnet::{Magnet, MagnetError};
pub use metainfo::{FileEntry, Info, Metainfo, MetainfoError};
pub use path::{PathError, SafePath};
