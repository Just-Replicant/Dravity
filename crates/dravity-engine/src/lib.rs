//! Sans-IO torrent engine for Dravity.
//!
//! [`TorrentState`] holds everything about one torrent's download: the
//! [`Picker`] (rarest-first, per-block state), the peer table, tracker and
//! DHT scheduling and the initial data check. [`MetadataState`] fetches the
//! `info` dictionary of a magnet link from peers (BEP 9) before a
//! `TorrentState` can exist. Drive either with `update`: give it an
//! [`Event`] and the current time, get back [`Effect`]s to perform.
//!
//! The crate never performs I/O, spawns tasks or reads the clock.
//! `dravity-session` is the only production driver; tests script events
//! directly.
//!
//! Swarm behavior (Phase 2):
//!
//! * **Extension protocol** (BEP 10): an extension handshake is sent to
//!   peers that support it; `ut_metadata` requests are answered from the
//!   torrent's `info` bytes and `ut_pex` peers are exchanged (BEP 11).
//! * **AIMD request window**: each peer's pipeline depth starts at
//!   [`Config::initial_requests_per_peer`], grows while blocks arrive and
//!   halves `min(window, reqq)` on a request timeout, and is capped by
//!   [`Config::max_requests_per_peer`].
//! * **Piece stealing and endgame**: an idle peer re-requests a block that a
//!   slow peer has held for [`Config::steal_after`]; once every block is
//!   requested any in-flight block may be requested twice. The copy that
//!   arrives second is cancelled.
//! * **Rate limit**: [`Config::download_rate_limit`] paces requests with a
//!   [`TokenBucket`].
//! * **DHT**: with [`Config::dht`] the engine schedules
//!   [`Effect::DhtLookup`]s; the driver runs them.
//!
//! The engine is still download-only: peers are never unchoked (seeding and
//! tit-for-tat are Phase 3).

mod bitfield;
mod ext;
mod metadata;
pub mod picker;
mod pool;
mod rate;
mod state;
mod types;
mod window;

pub use bitfield::Bitfield;
pub use metadata::{Handover, HandoverPeer, MetadataState};
pub use picker::Picker;
pub use rate::TokenBucket;
pub use state::TorrentState;
pub use types::{Alert, AnnounceEvent, Config, Effect, Event, PeerKey, Stats, TorrentStatus};
