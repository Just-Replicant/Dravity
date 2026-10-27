//! Sans-IO torrent engine for Dravity.
//!
//! [`TorrentState`] holds everything about one torrent's download: the
//! [`Picker`] (rarest-first, per-block state), the peer table, tracker
//! scheduling and the initial data check. Drive it with
//! [`TorrentState::update`]: give it an [`Event`] and the current time, get
//! back [`Effect`]s to perform.
//!
//! The crate never performs I/O, spawns tasks or reads the clock.
//! `dravity-session` is the only production driver; tests script events
//! directly.
//!
//! Phase 1 scope: downloading only. Peers are never unchoked, so the engine
//! does not serve uploads yet (seeding and tit-for-tat are Phase 3). A peer
//! that drops is dialed again after [`Config::peer_retry`]; a `bitfield` is
//! still accepted when it follows a keep-alive, DHT `port`, or BEP 10
//! extended handshake.

mod bitfield;
pub mod picker;
mod state;
mod types;

pub use bitfield::Bitfield;
pub use picker::Picker;
pub use state::TorrentState;
pub use types::{Alert, AnnounceEvent, Config, Effect, Event, PeerKey, Stats, TorrentStatus};
