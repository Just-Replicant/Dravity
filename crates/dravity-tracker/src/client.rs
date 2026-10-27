//! Async announce over HTTP(S) and UDP.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustls_platform_verifier::BuilderVerifierExt;
use tokio::net::UdpSocket;
use url::Url;

use crate::error::TrackerError;
use crate::types::{AnnounceRequest, AnnounceResponse};
use crate::{http, udp};

/// Timeouts and retry policy for [`TrackerClient`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClientConfig {
    /// Total time allowed for one HTTP announce.
    pub http_timeout: Duration,
    /// Initial UDP response timeout; doubles on each retry (BEP 15 uses 15 s).
    pub udp_timeout: Duration,
    /// UDP retransmissions after the first attempt.
    pub udp_retries: u32,
    /// `User-Agent` for HTTP announces.
    pub user_agent: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            http_timeout: Duration::from_secs(30),
            udp_timeout: Duration::from_secs(15),
            udp_retries: 2,
            user_agent: concat!("Dravity/", env!("CARGO_PKG_VERSION")).to_owned(),
        }
    }
}

/// How long a UDP connection id may be reused (BEP 15: one minute).
pub const UDP_CONNECTION_TTL: Duration = Duration::from_secs(60);

/// Most cached UDP connection ids; the oldest are dropped beyond this.
const MAX_CACHED_CONNECTIONS: usize = 256;

/// Announces to HTTP, HTTPS and UDP trackers.
///
/// HTTPS uses rustls with the `ring` provider and the operating system's
/// certificate verifier. UDP connection ids are cached per tracker address
/// for [`UDP_CONNECTION_TTL`]; clones of a client share the cache. If an
/// announce with a cached id fails (stale id, error response or no answer
/// to a single attempt) the id is dropped and the announce is retried once
/// with a fresh `connect`.
#[derive(Debug, Clone)]
pub struct TrackerClient {
    http: reqwest::Client,
    config: ClientConfig,
    connections: Arc<Mutex<HashMap<SocketAddr, (u64, Instant)>>>,
}

impl TrackerClient {
    /// Builds a client.
    ///
    /// # Errors
    /// [`TrackerError::Tls`] or [`TrackerError::Http`] if the TLS or HTTP
    /// stack cannot be initialised (e.g. no usable system certificate store).
    pub fn new(config: ClientConfig) -> Result<Self, TrackerError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_platform_verifier()?
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .timeout(config.http_timeout)
            .user_agent(config.user_agent.clone())
            .build()?;
        Ok(Self {
            http,
            config,
            connections: Arc::default(),
        })
    }

    /// The client configuration.
    #[must_use]
    pub const fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Announces `req` to the tracker at `url`.
    ///
    /// # Errors
    /// Any [`TrackerError`]: bad URL, transport failure, timeout, tracker
    /// failure reason, or malformed response.
    pub async fn announce(
        &self,
        url: &str,
        req: &AnnounceRequest,
    ) -> Result<AnnounceResponse, TrackerError> {
        let parsed = Url::parse(url).map_err(|_| TrackerError::InvalidUrl(url.to_owned()))?;
        match parsed.scheme() {
            "http" | "https" => self.announce_http(url, req).await,
            "udp" => self.announce_udp(&parsed, req).await,
            other => Err(TrackerError::UnsupportedScheme(other.to_owned())),
        }
    }

    async fn announce_http(
        &self,
        base: &str,
        req: &AnnounceRequest,
    ) -> Result<AnnounceResponse, TrackerError> {
        let mut response = self.http.get(http::announce_url(base, req)).send().await?;
        if !response.status().is_success() {
            return Err(TrackerError::HttpStatus(response.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > http::MAX_RESPONSE_LEN {
                return Err(TrackerError::TooLarge(http::MAX_RESPONSE_LEN));
            }
            body.extend_from_slice(&chunk);
        }
        http::parse_announce_response(&body)
    }

    async fn announce_udp(
        &self,
        url: &Url,
        req: &AnnounceRequest,
    ) -> Result<AnnounceResponse, TrackerError> {
        let host = url
            .host_str()
            .ok_or_else(|| TrackerError::InvalidUrl(url.to_string()))?;
        let port = url
            .port()
            .ok_or_else(|| TrackerError::InvalidUrl(url.to_string()))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let addr = tokio::net::lookup_host((host, port))
            .await?
            .next()
            .ok_or_else(|| TrackerError::Resolve(host.to_owned()))?;
        let bind = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let socket = UdpSocket::bind(bind).await?;
        socket.connect(addr).await?;

        if let Some(connection_id) = self.cached_connection(addr) {
            match self
                .udp_announce_with(&socket, addr, connection_id, req, 0)
                .await
            {
                Err(TrackerError::Io(e)) => return Err(TrackerError::Io(e)),
                Err(_) => self.forget_connection(addr),
                ok => return ok,
            }
        }

        let tid = fastrand::u32(..);
        let packet = udp::connect_request(tid);
        let connection_id = self
            .exchange(&socket, &packet, self.config.udp_retries, |p| {
                udp::parse_connect_response(p, tid)
            })
            .await?;
        self.remember_connection(addr, connection_id);
        self.udp_announce_with(&socket, addr, connection_id, req, self.config.udp_retries)
            .await
    }

    async fn udp_announce_with(
        &self,
        socket: &UdpSocket,
        addr: SocketAddr,
        connection_id: u64,
        req: &AnnounceRequest,
        retries: u32,
    ) -> Result<AnnounceResponse, TrackerError> {
        let tid = fastrand::u32(..);
        let packet = udp::announce_request(connection_id, tid, req);
        self.exchange(socket, &packet, retries, |p| {
            udp::parse_announce_response(p, tid, addr.is_ipv6())
        })
        .await
    }

    fn lock_connections(&self) -> std::sync::MutexGuard<'_, HashMap<SocketAddr, (u64, Instant)>> {
        self.connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn cached_connection(&self, addr: SocketAddr) -> Option<u64> {
        let mut cache = self.lock_connections();
        match cache.get(&addr) {
            Some(&(id, at)) if at.elapsed() < UDP_CONNECTION_TTL => Some(id),
            Some(_) => {
                cache.remove(&addr);
                None
            }
            None => None,
        }
    }

    fn remember_connection(&self, addr: SocketAddr, id: u64) {
        let mut cache = self.lock_connections();
        cache.retain(|_, (_, at)| at.elapsed() < UDP_CONNECTION_TTL);
        while cache.len() >= MAX_CACHED_CONNECTIONS {
            let Some(oldest) = cache.iter().min_by_key(|(_, (_, at))| *at).map(|(a, _)| *a) else {
                break;
            };
            cache.remove(&oldest);
        }
        cache.insert(addr, (id, Instant::now()));
    }

    fn forget_connection(&self, addr: SocketAddr) {
        self.lock_connections().remove(&addr);
    }

    /// Sends `packet` and waits for a response `parse` accepts, retrying with
    /// exponential back-off. Datagrams with the wrong transaction id are
    /// ignored (they may be late answers to an earlier attempt).
    async fn exchange<T>(
        &self,
        socket: &UdpSocket,
        packet: &[u8],
        retries: u32,
        parse: impl Fn(&[u8]) -> Result<T, TrackerError>,
    ) -> Result<T, TrackerError> {
        let mut buf = vec![0u8; udp::MAX_PACKET_LEN];
        let mut timeout = self.config.udp_timeout;
        for _ in 0..=retries {
            socket.send(packet).await?;
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let Ok(received) = tokio::time::timeout_at(deadline, socket.recv(&mut buf)).await
                else {
                    break;
                };
                let n = received?;
                match parse(buf.get(..n).unwrap_or_default()) {
                    Err(TrackerError::TransactionMismatch) => {}
                    result => return result,
                }
            }
            timeout = timeout.saturating_mul(2);
        }
        Err(TrackerError::Timeout)
    }
}
