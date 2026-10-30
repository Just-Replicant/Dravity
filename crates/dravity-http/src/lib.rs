//! HTTP API for a Dravity [`Session`].
//!
//! [`router`] builds an [axum] router that drives the session through its
//! public API only (commands in, alerts out), like the CLI does. [`serve`]
//! runs it on a listener until a shutdown signal. Any client that speaks
//! JSON can use it.
//!
//! | Method | Path | |
//! |---|---|---|
//! | `GET` | `/api/v1/session` | Peer id, listen port, DHT, port mapping |
//! | `GET` | `/api/v1/torrents` | Every torrent, running or not |
//! | `POST` | `/api/v1/torrents` | Add: a `.torrent` body (`?download_dir=&paused=`) or JSON `{"magnet": …, "download_dir", "paused", "file_priorities", "peers"}` |
//! | `GET` | `/api/v1/torrents/{info_hash}` | One torrent with live stats and files |
//! | `DELETE` | `/api/v1/torrents/{info_hash}?delete_files=true` | Remove (optionally with its files) |
//! | `POST` | `/api/v1/torrents/{info_hash}/pause`, `/resume` | Pause or resume |
//! | `PUT` | `/api/v1/torrents/{info_hash}/files` | `{"priorities": ["high", "skip", …]}` |
//! | `PUT` | `/api/v1/torrents/{info_hash}/files/{index}` | `{"priority": "low"}` |
//! | `GET`, `HEAD` | `/api/v1/torrents/{info_hash}/files/{index}/content` | Stream a file while it downloads (`Range` supported). `HEAD` returns the same status and headers and no body |
//! | `POST` | `/api/v1/torrents/{info_hash}/peers` | `{"peers": ["1.2.3.4:6881"]}` |
//! | `GET` | `/api/v1/events?stats=false&info_hash=…` | Alerts as server-sent events |
//! | `GET` | `/metrics` | Prometheus text format |
//!
//! Errors are `{"error": "…"}` with a 4xx/5xx status. Info-hashes are 40
//! hex digits; priorities are `skip`, `low`, `normal` and `high`.
//!
//! # Security
//!
//! The API can add torrents that write anywhere the process may write, and
//! delete their files. [`HttpConfig`] binds to `127.0.0.1` by default; set
//! [`HttpConfig::token`] before exposing it, and put a TLS proxy in front of
//! it for remote use (this crate speaks plain HTTP/1.1).
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use dravity_http::HttpConfig;
//! use dravity_session::{Session, SessionConfig};
//!
//! let session = Session::new(SessionConfig::default())?;
//! let config = HttpConfig::new("downloads").with_token("change me");
//! let listener = tokio::net::TcpListener::bind(config.bind).await?;
//! dravity_http::serve(listener, session, config, std::future::pending()).await?;
//! # Ok(()) }
//! ```

mod dto;
mod error;
pub mod metrics;
pub mod range;
mod routes;

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};
use dravity_session::Session;
pub use dto::PriorityDto;
pub use error::ApiError;
use tokio::sync::watch;

/// Default API port.
pub const DEFAULT_PORT: u16 = 9091;

/// Settings of the HTTP API.
#[derive(Clone)]
#[non_exhaustive]
pub struct HttpConfig {
    /// Address to listen on (`127.0.0.1:9091` by default).
    pub bind: SocketAddr,
    /// Bearer token every request must carry (`Authorization: Bearer …`);
    /// `None` disables authentication.
    pub token: Option<String>,
    /// Where torrents added without a `download_dir` go.
    pub download_dir: PathBuf,
    /// Largest request body (a `.torrent` upload), in bytes.
    pub max_body: usize,
}

impl std::fmt::Debug for HttpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpConfig")
            .field("bind", &self.bind)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("download_dir", &self.download_dir)
            .field("max_body", &self.max_body)
            .finish()
    }
}

impl HttpConfig {
    /// Loopback-only, no token, 16 MiB bodies, downloads into `dir`.
    pub fn new(download_dir: impl Into<PathBuf>) -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)),
            token: None,
            download_dir: download_dir.into(),
            max_body: 16 * 1024 * 1024,
        }
    }

    /// Listens on `bind`.
    #[must_use]
    pub const fn with_bind(mut self, bind: SocketAddr) -> Self {
        self.bind = bind;
        self
    }

    /// Requires `token` as a bearer token.
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
}

#[derive(Debug, Clone)]
struct AppState {
    session: Session,
    config: Arc<HttpConfig>,
    /// Becomes `true` when the server shuts down; ends event streams.
    closing: watch::Receiver<bool>,
}

/// The API as a router, for embedding or testing. Its event streams end
/// only when the session is dropped; [`serve`] also ends them on shutdown.
pub fn router(session: Session, config: HttpConfig) -> Router {
    let (tx, closing) = watch::channel(false);
    // Kept open forever: the receiver must not see a closed channel.
    std::mem::forget(tx);
    build(session, config, closing)
}

fn build(session: Session, config: HttpConfig, closing: watch::Receiver<bool>) -> Router {
    let max_body = config.max_body;
    let state = AppState {
        session,
        config: Arc::new(config),
        closing,
    };
    Router::new()
        .route("/api/v1/session", get(routes::session_info))
        .route("/api/v1/torrents", get(routes::list).post(routes::add))
        .route(
            "/api/v1/torrents/{info_hash}",
            get(routes::detail).delete(routes::remove),
        )
        .route("/api/v1/torrents/{info_hash}/pause", post(routes::pause))
        .route("/api/v1/torrents/{info_hash}/resume", post(routes::resume))
        .route(
            "/api/v1/torrents/{info_hash}/files",
            put(routes::set_priorities),
        )
        .route(
            "/api/v1/torrents/{info_hash}/files/{index}",
            put(routes::set_priority),
        )
        .route(
            "/api/v1/torrents/{info_hash}/files/{index}/content",
            get(routes::content).head(routes::content),
        )
        .route(
            "/api/v1/torrents/{info_hash}/peers",
            post(routes::add_peers),
        )
        .route("/api/v1/events", get(routes::events))
        .route("/metrics", get(routes::metrics))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            routes::auth,
        ))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state)
}

/// Why the server stopped.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HttpError {
    /// Accepting connections failed.
    #[error("HTTP server failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Serves the API on `listener` until `shutdown` completes, then lets open
/// requests finish (event streams end when the session shuts down).
///
/// # Errors
/// [`HttpError::Io`] if the server fails.
pub async fn serve(
    listener: tokio::net::TcpListener,
    session: Session,
    config: HttpConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), HttpError> {
    if let Ok(addr) = listener.local_addr() {
        if config.token.is_none() && !addr.ip().is_loopback() {
            tracing::warn!(%addr, "HTTP API reachable from the network without a token");
        }
        tracing::info!(%addr, "HTTP API listening");
    }
    let (closing_tx, closing) = watch::channel(false);
    let shutdown = async move {
        shutdown.await;
        let _ = closing_tx.send(true);
    };
    axum::serve(listener, build(session, config, closing))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
