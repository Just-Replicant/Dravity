//! Tracker errors.

/// Errors from announcing to a tracker.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TrackerError {
    /// The announce URL could not be parsed or has no host.
    #[error("invalid tracker URL `{0}`")]
    InvalidUrl(String),
    /// The URL scheme is not `http`, `https`, or `udp`.
    #[error("unsupported tracker scheme `{0}`")]
    UnsupportedScheme(String),
    /// The tracker replied with a `failure reason`.
    #[error("tracker failure: {0}")]
    Failure(String),
    /// The response is not a valid announce response.
    #[error("malformed tracker response: {0}")]
    Malformed(&'static str),
    /// A UDP response carried another request's transaction id.
    #[error("UDP transaction id mismatch")]
    TransactionMismatch,
    /// The response body exceeded the size limit.
    #[error("tracker response larger than {0} bytes")]
    TooLarge(usize),
    /// Non-success HTTP status.
    #[error("tracker returned HTTP status {0}")]
    HttpStatus(u16),
    /// HTTP transport error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// TLS configuration error.
    #[error("TLS setup failed: {0}")]
    Tls(#[from] rustls::Error),
    /// Socket error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// No answer within the configured time.
    #[error("tracker timed out")]
    Timeout,
    /// The host name did not resolve to any address.
    #[error("could not resolve tracker host `{0}`")]
    Resolve(String),
}
