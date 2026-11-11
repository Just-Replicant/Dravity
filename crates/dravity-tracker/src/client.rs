//! Async announce over HTTP(S) and UDP.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dravity_core::net::is_public;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::{Attempt, Policy};
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::net::UdpSocket;
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

use crate::error::TrackerError;
use crate::types::{AnnounceRequest, AnnounceResponse, ScrapeStats};
use crate::{guard, http, udp};

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
    /// Apply the SSRF checks of [`crate::guard`] (default `true`): local
    /// trackers need an `/announce` path, and redirects stay on `http(s)`,
    /// never downgrade and never move from a non-local tracker to a
    /// non-public address (literal or resolved).
    pub ssrf_mitigation: bool,
    /// Send HTTP(S) announces and scrapes through this HTTP proxy
    /// (`http://[user:pass@]host:port`). UDP trackers are not proxied.
    pub http_proxy: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            http_timeout: Duration::from_secs(30),
            udp_timeout: Duration::from_secs(15),
            udp_retries: 2,
            user_agent: concat!("Dravity/", env!("CARGO_PKG_VERSION")).to_owned(),
            ssrf_mitigation: true,
            http_proxy: None,
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
    trusted: Arc<TrustedHosts>,
}

/// A request that needs a UDP connection id.
#[derive(Debug, Clone, Copy)]
enum UdpRequest<'a> {
    Announce(&'a AnnounceRequest),
    Scrape(&'a [[u8; 20]]),
}

/// The answer to a [`UdpRequest`].
#[derive(Debug)]
enum UdpAnswer {
    Announce(AnnounceResponse),
    Scrape(Vec<ScrapeStats>),
}

/// Most host names remembered as announce origins; the set is cleared when
/// it grows past this (names are re-added on their next announce).
const MAX_TRUSTED_HOSTS: usize = 1024;

/// Host names that appeared in an announce URL itself (not in a redirect).
///
/// Only these may resolve to non-public addresses: a LAN tracker named by
/// the torrent works, but a public tracker cannot redirect to a name that
/// resolves into the LAN (and DNS rebinding cannot swap the address after
/// a check, because the filter runs in the resolver the connection uses).
#[derive(Debug, Default)]
struct TrustedHosts(Mutex<HashSet<String>>);

impl TrustedHosts {
    fn insert(&self, host: &str) {
        let mut set = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if set.len() >= MAX_TRUSTED_HOSTS {
            set.clear();
        }
        set.insert(host.to_ascii_lowercase());
    }

    fn contains(&self, host: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&host.to_ascii_lowercase())
    }
}

/// The system resolver, keeping only public addresses for names that are
/// not announce origins.
#[derive(Debug)]
struct GuardedResolver {
    trusted: Arc<TrustedHosts>,
}

/// A redirect target whose every address is non-public.
#[derive(Debug, thiserror::Error)]
#[error("tracker host {0} resolves only to non-public addresses")]
struct NotPublic(String);

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let trusted = self.trusted.contains(&host);
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| trusted || is_public(a.ip()))
                .collect();
            if found.is_empty() {
                return Err(NotPublic(host).into());
            }
            let addrs: Addrs = Box::new(found.into_iter());
            Ok(addrs)
        })
    }
}

/// The redirect policy: [`guard::check_redirect`] on every hop.
fn redirect(attempt: Attempt<'_>) -> reqwest::redirect::Action {
    let previous = attempt.previous();
    let (Some(origin), Some(from)) = (previous.first(), previous.last()) else {
        return attempt.follow();
    };
    let hops = previous.len().saturating_sub(1);
    match guard::check_redirect(origin, from, attempt.url(), hops) {
        Ok(()) => attempt.follow(),
        Err(blocked) => attempt.error(blocked),
    }
}

/// The [`guard::Blocked`] reason inside a reqwest error, if there is one.
fn blocked_reason(error: &reqwest::Error) -> Option<guard::Blocked> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = source {
        if let Some(blocked) = e.downcast_ref::<guard::Blocked>() {
            return Some(blocked.clone());
        }
        source = e.source();
    }
    None
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
        let trusted = Arc::new(TrustedHosts::default());
        let mut builder = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .timeout(config.http_timeout)
            .user_agent(config.user_agent.clone());
        if config.ssrf_mitigation {
            builder = builder
                .redirect(Policy::custom(redirect))
                .dns_resolver(Arc::new(GuardedResolver {
                    trusted: Arc::clone(&trusted),
                }));
        } else {
            builder = builder.redirect(Policy::limited(guard::MAX_REDIRECTS));
        }
        if let Some(url) = &config.http_proxy {
            builder = builder.proxy(reqwest::Proxy::all(url.as_str())?);
        }
        let http = builder.build()?;
        Ok(Self {
            http,
            config,
            endpoints: Arc::default(),
            trusted,
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
            "udp" => match self.udp_request(&parsed, UdpRequest::Announce(req)).await? {
                UdpAnswer::Announce(response) => Ok(response),
                UdpAnswer::Scrape(_) => Err(TrackerError::Malformed("unexpected UDP action")),
            },
            other => Err(TrackerError::UnsupportedScheme(other.to_owned())),
        }
    }

    /// Scrapes the tracker whose announce URL is `url` for `info_hash`
    /// (BEP 48 over HTTP, BEP 15 over UDP).
    ///
    /// # Errors
    /// [`TrackerError::NoScrape`] for an HTTP tracker whose URL does not
    /// end in `announce…`, a torrent missing from the answer, and every
    /// error [`Self::announce`] can return.
    pub async fn scrape(
        &self,
        url: &str,
        info_hash: &[u8; 20],
    ) -> Result<ScrapeStats, TrackerError> {
        let parsed = Url::parse(url).map_err(|_| TrackerError::InvalidUrl(url.to_owned()))?;
        match parsed.scheme() {
            "http" | "https" => {
                let scrape = http::scrape_url(url, &[*info_hash]).ok_or(TrackerError::NoScrape)?;
                let body = self.get(url, &scrape).await?;
                http::parse_scrape_response(&body)?
                    .into_iter()
                    .find(|(hash, _)| hash == info_hash)
                    .map(|(_, stats)| stats)
                    .ok_or(TrackerError::NoScrape)
            }
            "udp" => {
                let hashes = [*info_hash];
                match self
                    .udp_request(&parsed, UdpRequest::Scrape(&hashes))
                    .await?
                {
                    UdpAnswer::Scrape(stats) => {
                        stats.into_iter().next().ok_or(TrackerError::NoScrape)
                    }
                    UdpAnswer::Announce(_) => Err(TrackerError::Malformed("unexpected UDP action")),
                }
            }
            other => Err(TrackerError::UnsupportedScheme(other.to_owned())),
        }
    }

    async fn announce_http(
        &self,
        base: &str,
        req: &AnnounceRequest,
    ) -> Result<AnnounceResponse, TrackerError> {
        let url = http::announce_url(base, req);
        let body = self.get(base, &url).await?;
        http::parse_announce_response(&body)
    }

    /// GETs `url` (built from the announce URL `base`) through the SSRF
    /// guard and returns the body, at most [`http::MAX_RESPONSE_LEN`]. The
    /// guard judges `base`: a scrape URL differs from its announce URL only
    /// in `announce` becoming `scrape`, on the same host.
    async fn get(&self, base: &str, url: &str) -> Result<Vec<u8>, TrackerError> {
        if self.config.ssrf_mitigation {
            let parsed = Url::parse(base).map_err(|_| TrackerError::InvalidUrl(base.to_owned()))?;
            guard::check_announce(&parsed)?;
            if let Some(url::Host::Domain(name)) = parsed.host() {
                self.trusted.insert(name);
            }
        }
        let mut response =
            self.http.get(url).send().await.map_err(|e| {
                blocked_reason(&e).map_or(TrackerError::Http(e), TrackerError::Blocked)
            })?;
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
        Ok(body)
    }

    async fn udp_request(&self, url: &Url, req: UdpRequest<'_>) -> Result<UdpAnswer, TrackerError> {
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
        match self.udp_request_at(addr, req).await {
            // The socket may be dead (a refused datagram sticks to a connected
            // UDP socket). Drop it and try once more from a new port.
            Err(TrackerError::Io(_)) => {
                self.drop_endpoint(addr);
                self.udp_request_at(addr, req).await
            }
            other => other,
        }
    }

    /// One request on the socket cached for `addr`, creating that socket if
    /// needed. The endpoint's gate is held for the whole exchange.
    async fn udp_request_at(
        &self,
        addr: SocketAddr,
        req: UdpRequest<'_>,
    ) -> Result<UdpAnswer, TrackerError> {
        let (socket, gate) = self.udp_socket(addr).await?;
        let _hold = gate.lock().await;
        if let Some(connection_id) = self.cached_connection(addr) {
            match self
                .udp_request_with(&socket, addr, connection_id, req, 0)
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
            .udp_request_with(&socket, addr, connection_id, req, self.config.udp_retries)
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

    async fn udp_request_with(
        &self,
        socket: &UdpSocket,
        addr: SocketAddr,
        connection_id: u64,
        req: UdpRequest<'_>,
        retries: u32,
    ) -> Result<UdpAnswer, TrackerError> {
        let tid = fastrand::u32(..);
        match req {
            UdpRequest::Announce(req) => {
                let packet = udp::announce_request(connection_id, tid, req);
                self.exchange(socket, &packet, retries, |p| {
                    udp::parse_announce_response(p, tid, addr.is_ipv6()).map(UdpAnswer::Announce)
                })
                .await
            }
            UdpRequest::Scrape(hashes) => {
                let packet = udp::scrape_request(connection_id, tid, hashes);
                let count = hashes.len().min(udp::MAX_SCRAPE_HASHES);
                self.exchange(socket, &packet, retries, |p| {
                    udp::parse_scrape_response(p, tid, count).map(UdpAnswer::Scrape)
                })
                .await
            }
        }
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
        if let Some(endpoint) = endpoints.get_mut(&addr)
            && endpoint.id == Some(id)
        {
            endpoint.id = None;
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
