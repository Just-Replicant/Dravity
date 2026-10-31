//! Web seed errors.

/// Why a web seed request failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WebSeedError {
    /// The URL could not be parsed or has no host.
    #[error("invalid web seed URL {0:?}")]
    InvalidUrl(String),
    /// Only `http` and `https` web seeds are supported.
    #[error("unsupported web seed scheme {0:?}")]
    UnsupportedScheme(String),
    /// The range is outside the torrent's content.
    #[error("range {start}..{end} is outside the content")]
    OutOfRange {
        /// First byte.
        start: u64,
        /// One past the last byte.
        end: u64,
    },
    /// The server answered with an unexpected status.
    #[error("web seed answered HTTP {0}")]
    HttpStatus(u16),
    /// The server ignored the `Range` header for a range that does not start
    /// the file (BEP 19 servers must support ranges).
    #[error("web seed ignored the Range header")]
    RangeIgnored,
    /// The body was shorter or longer than the range asked for.
    #[error("web seed sent {got} bytes, expected {expected}")]
    BodyLength {
        /// Bytes asked for.
        expected: u64,
        /// Bytes received (a long body stops counting one past `expected`).
        got: u64,
    },
    /// The TLS configuration could not be built.
    #[cfg(feature = "client")]
    #[error("TLS setup failed: {0}")]
    Tls(#[from] rustls::Error),
    /// Transport failure, timeout, or a bad redirect.
    #[cfg(feature = "client")]
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}
