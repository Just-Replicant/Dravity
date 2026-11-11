//! Core BitTorrent types for the Dravity engine.
//!
//! * [`InfoHash`], [`InfoHashV2`], [`PeerId`] — identifiers.
//! * [`Metainfo`] — validated `.torrent` parsing (v1, v2 and hybrid, BEP 52)
//!   with exact info-hashes.
//! * [`Magnet`] — magnet URI parsing and formatting.
//! * [`compact`] — compact IPv4/IPv6 peer endpoints (BEP 23, BEP 7).
//! * [`layout`] — piece, block and file arithmetic.
//! * [`merkle`] — BEP 52 SHA-256 Merkle trees (pieces roots, piece layers).
//! * [`path`] — rejection of unsafe file paths from untrusted metainfo.
//!
//! This crate is sans-IO: it never touches the network or filesystem.

pub mod compact;
mod encoding;
mod info_hash;
pub mod layout;
pub mod magnet;
pub mod merkle;
pub mod metainfo;
pub mod path;
mod v2;

pub use info_hash::{InfoHash, InfoHashV2, ParseHashError, PeerId};
pub use layout::{BLOCK_SIZE, BlockRef, PieceLayout};
pub use magnet::{Magnet, MagnetError};
pub use metainfo::{FileEntry, Info, Metainfo, MetainfoError};
pub use path::{PathError, SafePath};
pub use v2::MetaVersion;
