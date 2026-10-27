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
}
