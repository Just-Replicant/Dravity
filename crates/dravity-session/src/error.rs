//! Session errors.

use dravity_core::InfoHash;

/// Errors from the session API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// The torrent is already in the session.
    #[error("torrent {0} is already added")]
    Duplicate(InfoHash),
    /// The torrent is not (or no longer) running.
    #[error("torrent {0} is not running")]
    NotRunning(InfoHash),
    /// Storage could not be set up.
    #[error(transparent)]
    Storage(#[from] dravity_storage::StorageError),
    /// The tracker client could not be created.
    #[error(transparent)]
    Tracker(#[from] dravity_tracker::TrackerError),
    /// The listening sockets could not be set up.
    #[error("cannot listen for peers: {0}")]
    Listen(#[source] std::io::Error),
    /// The DHT node could not be started.
    #[error(transparent)]
    Dht(#[from] dravity_dht::DhtError),
    /// The magnet link has no v1 info-hash (`urn:btih`); v2-only magnets
    /// are not supported yet.
    #[error("magnet link has no v1 (btih) info-hash")]
    MagnetWithoutV1Hash,
    /// A custom storage backend was given for a magnet link, whose size is
    /// unknown until the metadata arrives.
    #[error("custom storage is not supported for magnet links")]
    StorageForMagnet,
}
