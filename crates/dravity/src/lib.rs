//! Dravity: a BitTorrent client library.
//!
//! This crate is the stable entry point. It re-exports the parts of the
//! internal crates a library user needs, so they can depend on `dravity`
//! alone while the internals are refactored freely:
//!
//! - [`ClientBuilder`] and [`Client`]: start a session and add torrents.
//! - [`prelude`]: the common types in one import.
//! - [`Error`]: one error type for the facade.
//! - [`session`], [`core`] and [`storage`]: the full underlying APIs.
//!
//! ```no_run
//! use dravity::prelude::*;
//!
//! # async fn run() -> dravity::Result<()> {
//! let client = Client::builder().listen_port(6881).build()?;
//! let torrent = client.add_magnet(
//!     "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
//!     "downloads",
//! )?;
//! let progress = torrent.progress().await?;
//! println!("{} bytes left", progress.stats.wanted_left);
//! client.shutdown().await;
//! # Ok(())
//! # }
//! ```

mod client;
mod error;

pub use client::{Client, ClientBuilder};
pub use error::{Error, Result};

/// The session layer ([`dravity_session`]): sessions, torrents, alerts,
/// settings, persistence, transports.
pub use dravity_session as session;

/// Torrent metadata, magnet links, hashes and torrent creation
/// ([`dravity_core`]).
pub use dravity_core as core;

/// Piece storage backends ([`dravity_storage`]).
pub use dravity_storage as storage;

/// The types most programs use.
pub mod prelude {
    pub use crate::{Client, ClientBuilder, Error, Result};
    pub use dravity_core::{InfoHash, Magnet, Metainfo};
    pub use dravity_session::{
        AddTorrentOptions, EncryptionPolicy, Session, SessionAlert, SessionConfig, TorrentHandle,
        TorrentProgress,
    };
}
