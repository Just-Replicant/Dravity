//! The facade's single error type.

use std::path::PathBuf;

/// Anything the facade can fail with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The session refused or failed an operation.
    #[error(transparent)]
    Session(#[from] dravity_session::SessionError),
    /// A `.torrent` file is not valid.
    #[error(transparent)]
    Metainfo(#[from] dravity_core::MetainfoError),
    /// A magnet link is not valid.
    #[error(transparent)]
    Magnet(#[from] dravity_core::MagnetError),
    /// A proxy URL is not valid or the proxy failed.
    #[error(transparent)]
    Proxy(#[from] dravity_session::proxy::ProxyError),
    /// A file could not be read.
    #[error("reading {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
}

/// `Result` with the facade's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
