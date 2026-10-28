//! Async announce over HTTP(S) and UDP.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustls_platform_verifier::BuilderVerifierExt;
use tokio::net::UdpSocket;
use tokio::sync::Mutex as AsyncMutex;
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

/// One UDP tracker socket and its BEP 15 connection id.
///
/// The id is only valid from the source port that performed `connect`, so
/// the socket is reused. [`UdpEndpoint::gate`] admits one announce at a time:
/// a datagram socket does not demultiplex responses, and a second `connect`
/// from the same port replaces the id the first announce just stored.
#[derive(Debug)]
struct UdpEndpoint {
    socket: Arc<UdpSocket>,
    id: Option<u64>,
    /// When `id` was stored, or when the socket was created if there is none.
    at: Instant,
    gate: Arc<AsyncMutex<()>>,
}

/// Announces to HTTP, HTTPS and UDP trackers.
///
/// HTTPS uses rustls with the `ring` provider and the operating system's
/// certificate verifier. UDP connection ids are cached per tracker address
/// for [`UDP_CONNECTION_TTL`] and sent from the socket that connected; clones
/// of a client share both, and take turns on that socket. If an announce with
/// a cached id fails (stale id, error response or no answer to a single
/// attempt) that id is dropped — a newer id stored for the same address is
/// left alone — and the announce is retried once with a fresh `connect` on
/// the same socket. An I/O error drops the socket and the announce is tried
/// once more from a new one.
#[derive(Debug, Clone)]
pub struct TrackerClient {
    http: reqwest::Client,
    config: ClientConfig,
    endpoints: Arc<Mutex<HashMap<SocketAddr, UdpEndpoint>>>,
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
            endpoints: Arc::default(),
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
        match self.announce_udp_at(addr, req).await {
            // The socket may be dead (a refused datagram sticks to a connected
            // UDP socket). Drop it and try once more from a new port.
            Err(TrackerError::Io(_)) => {
                self.drop_endpoint(addr);
                self.announce_udp_at(addr, req).await
            }
            other => other,
        }
    }

    /// One announce on the socket cached for `addr`, creating that socket if
    /// needed. The endpoint's gate is held for the whole exchange.
    async fn announce_udp_at(
        &self,
        addr: SocketAddr,
        req: &AnnounceRequest,
    ) -> Result<AnnounceResponse, TrackerError> {
        let (socket, gate) = self.udp_socket(addr).await?;
        let _hold = gate.lock().await;
        if let Some(connection_id) = self.cached_connection(addr) {
            match self
                .udp_announce_with(&socket, addr, connection_id, req, 0)
                .await
            {
                Err(e @ TrackerError::Io(_)) => return Err(self.fail_socket(addr, e)),
                Err(_) => self.forget_connection(addr, connection_id),
                ok => return ok,
            }
        }

        let tid = fastrand::u32(..);
        let packet = udp::connect_request(tid);
        let connection_id = match self
            .exchange(&socket, &packet, self.config.udp_retries, |p| {
                udp::parse_connect_response(p, tid)
            })
            .await
        {
            Ok(id) => id,
            Err(e @ TrackerError::Io(_)) => return Err(self.fail_socket(addr, e)),
            Err(e) => return Err(e),
        };
        self.remember_connection(addr, connection_id, &socket, &gate);
        match self
            .udp_announce_with(&socket, addr, connection_id, req, self.config.udp_retries)
            .await
        {
            Err(e @ TrackerError::Io(_)) => Err(self.fail_socket(addr, e)),
            other => other,
        }
    }

    /// The socket already connected to `addr`, or a new one, and the gate
    /// that serializes announces on it.
    ///
    /// Two clones that miss the cache at once may each bind a socket; the
    /// first one stored is kept, and the connection id always belongs to it.
    async fn udp_socket(
        &self,
        addr: SocketAddr,
    ) -> Result<(Arc<UdpSocket>, Arc<AsyncMutex<()>>), TrackerError> {
        if let Some((socket, gate)) = self.endpoint_handle(addr) {
            return Ok((socket, gate));
        }
        let bind = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let socket = Arc::new(UdpSocket::bind(bind).await?);
        socket.connect(addr).await?;
        let mut endpoints = self.lock_endpoints();
        if let Some(existing) = endpoints.get(&addr) {
            return Ok((Arc::clone(&existing.socket), Arc::clone(&existing.gate)));
        }
        let gate = Arc::new(AsyncMutex::new(()));
        self.evict_expired(&mut endpoints);
        endpoints.insert(
            addr,
            UdpEndpoint {
                socket: Arc::clone(&socket),
                id: None,
                at: Instant::now(),
                gate: Arc::clone(&gate),
            },
        );
        Ok((socket, gate))
    }

    fn endpoint_handle(&self, addr: SocketAddr) -> Option<(Arc<UdpSocket>, Arc<AsyncMutex<()>>)> {
        self.lock_endpoints()
            .get(&addr)
            .map(|endpoint| (Arc::clone(&endpoint.socket), Arc::clone(&endpoint.gate)))
    }

    /// Forgets `addr` after an I/O error so the next attempt binds a new socket.
    fn fail_socket(&self, addr: SocketAddr, err: TrackerError) -> TrackerError {
        self.drop_endpoint(addr);
        err
    }

    fn drop_endpoint(&self, addr: SocketAddr) {
        self.lock_endpoints().remove(&addr);
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

    fn lock_endpoints(&self) -> std::sync::MutexGuard<'_, HashMap<SocketAddr, UdpEndpoint>> {
        self.endpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn cached_connection(&self, addr: SocketAddr) -> Option<u64> {
        let mut endpoints = self.lock_endpoints();
        let endpoint = endpoints.get_mut(&addr)?;
        match endpoint.id {
            Some(id) if endpoint.at.elapsed() < UDP_CONNECTION_TTL => Some(id),
            Some(_) => {
                endpoint.id = None;
                None
            }
            None => None,
        }
    }

    fn evict_expired(&self, endpoints: &mut HashMap<SocketAddr, UdpEndpoint>) {
        endpoints.retain(|_, endpoint| {
            endpoint.id.is_none() || endpoint.at.elapsed() < UDP_CONNECTION_TTL
        });
        while endpoints.len() >= MAX_CACHED_CONNECTIONS {
            let Some(oldest) = endpoints
                .iter()
                .min_by_key(|(_, endpoint)| endpoint.at)
                .map(|(addr, _)| *addr)
            else {
                break;
            };
            endpoints.remove(&oldest);
        }
    }

    fn remember_connection(
        &self,
        addr: SocketAddr,
        id: u64,
        socket: &Arc<UdpSocket>,
        gate: &Arc<AsyncMutex<()>>,
    ) {
        let mut endpoints = self.lock_endpoints();
        endpoints.retain(|kept, endpoint| {
            *kept == addr || endpoint.id.is_none() || endpoint.at.elapsed() < UDP_CONNECTION_TTL
        });
        while endpoints.len() >= MAX_CACHED_CONNECTIONS {
            let Some(oldest) = endpoints
                .iter()
                .filter(|(candidate, _)| **candidate != addr)
                .min_by_key(|(_, endpoint)| endpoint.at)
                .map(|(candidate, _)| *candidate)
            else {
                break;
            };
            endpoints.remove(&oldest);
        }
        match endpoints.get_mut(&addr) {
            // The id was issued to this source port. A replacement socket
            // must not inherit it.
            Some(endpoint) if Arc::ptr_eq(&endpoint.socket, socket) => {
                endpoint.id = Some(id);
                endpoint.at = Instant::now();
            }
            Some(_) => {}
            None => {
                endpoints.insert(
                    addr,
                    UdpEndpoint {
                        socket: Arc::clone(socket),
                        id: Some(id),
                        at: Instant::now(),
                        gate: Arc::clone(gate),
                    },
                );
            }
        }
    }

    /// Drops `id` if it is still the one stored. A newer id from another
    /// clone is left in place.
    fn forget_connection(&self, addr: SocketAddr, id: u64) {
        let mut endpoints = self.lock_endpoints();
        if let Some(endpoint) = endpoints.get_mut(&addr) {
            if endpoint.id == Some(id) {
                endpoint.id = None;
            }
        }
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
