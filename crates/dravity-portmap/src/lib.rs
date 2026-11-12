//! Router port mapping for Dravity: PCP (RFC 6887), NAT-PMP (RFC 6886)
//! and UPnP IGD.
//!
//! Peers can only connect to us if the home router forwards our listening
//! port. [`PortMapper`] asks the router to do so, PCP first, then NAT-PMP
//! (both one UDP round trip to the default gateway's port 5351) and UPnP
//! IGD last, renews the
//! mappings at half their lifetime and removes them when stopped. The
//! external port is requested to equal the listen port. When NAT-PMP
//! returns a different one, [`Status::external_port`] reports it, so the
//! session announces the port peers can actually reach. Failure is normal (no router, feature disabled,
//! carrier-grade NAT) and only reported in [`Status`]; outgoing
//! connections work regardless.
//!
//! The protocol modules ([`pcp`], [`natpmp`], [`upnp`]) are sans-IO; the
//! mapper task does the networking.

mod gateway;
mod http;
pub mod natpmp;
pub mod pcp;
pub mod upnp;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use gateway::{
    default_gateway, default_gateway_v6, parse_proc_net_ipv6_route, parse_proc_net_route,
    parse_route_get_default, parse_windows_route,
};
pub use http::Url;
pub use natpmp::{NatPmpError, Protocol};
pub use pcp::PcpError;
pub use upnp::UpnpError;

/// Port mapping settings.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PortMapConfig {
    /// Try PCP (before NAT-PMP, on the same gateway port).
    pub pcp: bool,
    /// Try NAT-PMP.
    pub natpmp: bool,
    /// Try UPnP IGD.
    pub upnp: bool,
    /// NAT-PMP server; `None` uses the default gateway on port 5351.
    pub gateway: Option<SocketAddrV4>,
    /// Also ask the IPv6 default router for PCP firewall pinholes, so
    /// peers can reach our global IPv6 address through a router that
    /// blocks unsolicited inbound IPv6 (RFC 6887 §10.3; nothing is
    /// translated, so the port stays ours).
    pub ipv6: bool,
    /// PCP server for pinholes; `None` uses the IPv6 default router on
    /// port 5351 (Linux).
    pub gateway_v6: Option<SocketAddrV6>,
    /// UPnP device description URL; `None` discovers it with SSDP.
    pub upnp_location: Option<String>,
    /// Requested mapping lifetime; mappings are renewed at half of it.
    pub lifetime: Duration,
    /// Mapping description shown in the router's UI (UPnP).
    pub description: String,
    /// Time allowed for one discovery or request.
    pub timeout: Duration,
    /// Wait before trying again after every method failed.
    pub retry: Duration,
}

impl Default for PortMapConfig {
    fn default() -> Self {
        Self {
            pcp: true,
            natpmp: true,
            upnp: true,
            gateway: None,
            ipv6: true,
            gateway_v6: None,
            upnp_location: None,
            lifetime: Duration::from_secs(2 * 3600),
            description: "Dravity".to_owned(),
            timeout: Duration::from_secs(3),
            retry: Duration::from_secs(5 * 60),
        }
    }
}

/// A port to forward to us (the external port is requested to match).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Mapping {
    /// Protocol.
    pub protocol: Protocol,
    /// Our listening port.
    pub port: u16,
}

/// How mappings were made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// PCP.
    Pcp,
    /// NAT-PMP.
    NatPmp,
    /// UPnP IGD.
    Upnp,
}

/// One active mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapped {
    /// What was asked for.
    pub mapping: Mapping,
    /// Public port the router forwards (may differ from the requested one).
    pub external_port: u16,
}

/// What the mapper achieved so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Status {
    /// Method in use; `None` until a mapping succeeded.
    pub method: Option<Method>,
    /// Router's public address, if it told us.
    pub external_ip: Option<Ipv4Addr>,
    /// Active mappings.
    pub mapped: Vec<Mapped>,
    /// Why mapping failed last, if it did.
    pub error: Option<String>,
    /// IPv6 firewall pinholes open on the router (PCP), independent of
    /// the IPv4 mappings above.
    pub pinholes: Vec<Mapped>,
    /// Why the IPv6 pinholes failed last, if they did (`None` with
    /// [`PortMapConfig::ipv6`] off).
    pub pinhole_error: Option<String>,
}

impl Status {
    /// Public port the router forwards for `protocol`, if a mapping is
    /// active and the router said which (UPnP mappings report the requested
    /// port; `0` means unknown and is ignored).
    #[must_use]
    pub fn external_port(&self, protocol: Protocol) -> Option<u16> {
        self.mapped
            .iter()
            .find(|m| m.mapping.protocol == protocol && m.external_port != 0)
            .map(|m| m.external_port)
    }
}

/// Why a mapping attempt failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PortMapError {
    /// NAT-PMP gateway error.
    #[error(transparent)]
    NatPmp(#[from] NatPmpError),
    /// PCP gateway error.
    #[error(transparent)]
    Pcp(#[from] PcpError),
    /// UPnP device error.
    #[error(transparent)]
    Upnp(#[from] UpnpError),
    /// Network error (including timeouts: no gateway answered).
    #[error(transparent)]
    Io(#[from] io::Error),
    /// No default gateway is known.
    #[error("no default gateway")]
    NoGateway,
    /// No global IPv6 address to open a pinhole for.
    #[error("no global IPv6 address")]
    NoGlobalAddress,
    /// No UPnP internet gateway answered, or it has no port-mapping service.
    #[error("no UPnP internet gateway found")]
    NoDevice,
}

/// Maps ports in the background until stopped.
#[derive(Debug)]
pub struct PortMapper {
    stop: CancellationToken,
    task: JoinHandle<()>,
    status: watch::Receiver<Status>,
}

impl PortMapper {
    /// Starts mapping `mappings`. Must be called in a Tokio runtime.
    #[must_use]
    pub fn start(config: PortMapConfig, mappings: Vec<Mapping>) -> Self {
        let stop = CancellationToken::new();
        let (tx, status) = watch::channel(Status::default());
        let task = tokio::spawn(run(config, mappings, tx, stop.clone()));
        Self { stop, task, status }
    }

    /// The current status.
    #[must_use]
    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    /// A receiver that sees every status change.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// Removes the mappings (bounded by the request timeout) and stops.
    pub async fn stop(mut self) {
        self.stop.cancel();
        let _ = (&mut self.task).await;
    }
}

impl Drop for PortMapper {
    fn drop(&mut self) {
        // Without `stop` the task still removes what it can.
        self.stop.cancel();
    }
}

/// Mappings made by one method, for renewing and removing.
#[derive(Debug)]
enum Active {
    /// The server and the request each mapping was made with (renewals
    /// and the delete reuse its nonce).
    Pcp(SocketAddrV4, Vec<pcp::MapRequest>),
    NatPmp(SocketAddrV4),
    Upnp(upnp::Service),
}

/// IPv6 pinholes made with PCP: the server, the socket address requests
/// leave from, and each request (renewals and deletes reuse its nonce).
#[derive(Debug)]
struct Pinholes {
    server: SocketAddr,
    from: SocketAddr,
    requests: Vec<pcp::MapRequest>,
}

async fn run(
    config: PortMapConfig,
    mappings: Vec<Mapping>,
    status: watch::Sender<Status>,
    stop: CancellationToken,
) {
    loop {
        let (pinholes, pinhole_status) = if config.ipv6 {
            match map_pinholes(&config, &mappings).await {
                Ok((pinholes, mapped)) => (Some(pinholes), (mapped, None)),
                Err(e) => {
                    tracing::debug!(error = %e, "IPv6 pinholes failed");
                    (None, (Vec::new(), Some(e.to_string())))
                }
            }
        } else {
            (None, (Vec::new(), None))
        };
        let (active, mut wait) = match map_all(&config, &mappings).await {
            Ok((active, mut next)) => {
                (next.pinholes, next.pinhole_error) = pinhole_status;
                status.send_replace(next);
                // Renew at half the lifetime (RFC 6886 §3.3).
                (Some(active), config.lifetime / 2)
            }
            Err(e) => {
                tracing::debug!(error = %e, "port mapping failed");
                status.send_modify(|s| {
                    s.method = None;
                    s.mapped.clear();
                    s.error = Some(e.to_string());
                    (s.pinholes, s.pinhole_error) = pinhole_status;
                });
                (None, config.retry)
            }
        };
        if pinholes.is_some() {
            // Pinholes must be renewed in time even while IPv4 retries.
            wait = wait.min(config.lifetime / 2);
        }
        let stopped = tokio::select! {
            () = stop.cancelled() => true,
            () = tokio::time::sleep(wait.max(Duration::from_secs(1))) => false,
        };
        if stopped {
            if let Some(active) = active {
                unmap_all(&config, &mappings, &active).await;
            }
            if let Some(pinholes) = pinholes {
                close_pinholes(&pinholes).await;
            }
            status.send_modify(|s| {
                s.mapped.clear();
                s.pinholes.clear();
            });
            return;
        }
    }
}

async fn map_all(
    config: &PortMapConfig,
    mappings: &[Mapping],
) -> Result<(Active, Status), PortMapError> {
    let mut last = PortMapError::NoGateway;
    if config.pcp {
        match map_pcp(config, mappings).await {
            Ok(ok) => return Ok(ok),
            Err(e) => {
                tracing::debug!(error = %e, "PCP failed; trying NAT-PMP");
                last = e;
            }
        }
    }
    if config.natpmp {
        match map_natpmp(config, mappings).await {
            Ok(ok) => return Ok(ok),
            Err(e) => last = e,
        }
    }
    if config.upnp {
        match map_upnp(config, mappings).await {
            Ok(ok) => return Ok(ok),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn natpmp_server(config: &PortMapConfig) -> Result<SocketAddrV4, PortMapError> {
    config
        .gateway
        .or_else(|| default_gateway().map(|gw| SocketAddrV4::new(gw, natpmp::SERVER_PORT)))
        .ok_or(PortMapError::NoGateway)
}

/// One NAT-PMP exchange with retransmissions.
async fn natpmp_exchange(
    server: SocketAddrV4,
    request: natpmp::Request,
) -> Result<natpmp::Response, PortMapError> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.connect(server).await?;
    let bytes = request.encode();
    let mut buf = [0u8; 64];
    for delay in natpmp::RETRIES {
        socket.send(&bytes).await?;
        let deadline = tokio::time::Instant::now() + delay;
        while let Ok(received) = tokio::time::timeout_at(deadline, socket.recv(&mut buf)).await {
            let n = received?;
            let response = natpmp::Response::decode(buf.get(..n).unwrap_or_default())?;
            if response.answers(&request) {
                return Ok(response);
            }
        }
    }
    Err(io::Error::from(io::ErrorKind::TimedOut).into())
}

/// One PCP exchange with retransmissions (RFC 6887 §8.1.1, cut short like
/// NAT-PMP's).
async fn pcp_exchange(
    server: SocketAddrV4,
    request: &pcp::MapRequest,
) -> Result<pcp::MapResponse, PortMapError> {
    pcp_exchange_from(
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V4(server),
        request,
    )
    .await
}

/// [`pcp_exchange`] from a chosen local address (a pinhole request must
/// come from the address it opens, RFC 6887 §8.3).
async fn pcp_exchange_from(
    from: SocketAddr,
    server: SocketAddr,
    request: &pcp::MapRequest,
) -> Result<pcp::MapResponse, PortMapError> {
    let socket = UdpSocket::bind(from).await?;
    socket.connect(server).await?;
    let bytes = request.encode();
    let mut buf = [0u8; 1100];
    for delay in natpmp::RETRIES {
        socket.send(&bytes).await?;
        let deadline = tokio::time::Instant::now() + delay;
        while let Ok(received) = tokio::time::timeout_at(deadline, socket.recv(&mut buf)).await {
            let n = received?;
            let response = pcp::decode(buf.get(..n).unwrap_or_default())?;
            if response.answers(request) {
                return Ok(response);
            }
        }
    }
    Err(io::Error::from(io::ErrorKind::TimedOut).into())
}

/// A fresh 12-byte nonce. Not cryptographic: it only has to differ
/// between mappings and be hard to guess off-path (RFC 6887 §11.1).
fn nonce() -> [u8; 12] {
    use std::hash::{BuildHasher, Hasher};
    let mut out = [0u8; 12];
    for chunk in out.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        let bytes = h.finish().to_le_bytes();
        let n = chunk.len();
        chunk.copy_from_slice(bytes.get(..n).unwrap_or_default());
    }
    out
}

async fn map_pcp(
    config: &PortMapConfig,
    mappings: &[Mapping],
) -> Result<(Active, Status), PortMapError> {
    let server = natpmp_server(config)?;
    let client = upnp::local_addr_towards(SocketAddr::V4(server))?;
    let lifetime = u32::try_from(config.lifetime.as_secs()).unwrap_or(u32::MAX);
    let mut status = Status {
        method: Some(Method::Pcp),
        ..Status::default()
    };
    let mut requests = Vec::new();
    for mapping in mappings {
        let request = pcp::MapRequest {
            nonce: nonce(),
            protocol: mapping.protocol,
            internal_port: mapping.port,
            external_port: mapping.port,
            external_ip: pcp::any_external(client),
            client_ip: client,
            lifetime,
        };
        let response = pcp_exchange(server, &request).await?;
        if let IpAddr::V4(ip) = response.external_ip
            && !ip.is_unspecified()
        {
            status.external_ip = Some(ip);
        }
        status.mapped.push(Mapped {
            mapping: *mapping,
            external_port: response.external_port,
        });
        requests.push(request);
    }
    tracing::info!(?status, "ports mapped with PCP");
    Ok((Active::Pcp(server, requests), status))
}

/// The PCP server for pinholes.
fn pinhole_server(config: &PortMapConfig) -> Result<SocketAddrV6, PortMapError> {
    config
        .gateway_v6
        .or_else(|| {
            default_gateway_v6()
                .map(|(router, scope)| SocketAddrV6::new(router, natpmp::SERVER_PORT, 0, scope))
        })
        .ok_or(PortMapError::NoGateway)
}

/// A well-known public IPv6 address, only used to ask the kernel which
/// source address it would pick: `connect` on UDP sends nothing.
const GLOBAL_PROBE: SocketAddrV6 = SocketAddrV6::new(
    Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888),
    53,
    0,
    0,
);

fn source_towards(dest: SocketAddrV6) -> io::Result<Ipv6Addr> {
    let socket = std::net::UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))?;
    socket.connect(dest)?;
    match socket.local_addr()?.ip() {
        IpAddr::V6(ip) => Ok(ip),
        IpAddr::V4(_) => Err(io::Error::from(io::ErrorKind::AddrNotAvailable)),
    }
}

/// Link-local (`fe80::/10`) or unspecified: no use to a remote peer.
const fn unreachable_from_afar(ip: Ipv6Addr) -> bool {
    ip.is_unspecified() || ip.segments()[0] & 0xffc0 == 0xfe80
}

/// Our address for pinholes: the source towards the router, or, when that
/// is link-local (the router usually is), the source towards the internet.
fn pinhole_client(server: SocketAddrV6) -> Result<Ipv6Addr, PortMapError> {
    let near = source_towards(server)?;
    if !unreachable_from_afar(near) {
        return Ok(near);
    }
    let far = source_towards(GLOBAL_PROBE).map_err(|_| PortMapError::NoGlobalAddress)?;
    if unreachable_from_afar(far) {
        return Err(PortMapError::NoGlobalAddress);
    }
    Ok(far)
}

async fn map_pinholes(
    config: &PortMapConfig,
    mappings: &[Mapping],
) -> Result<(Pinholes, Vec<Mapped>), PortMapError> {
    let server = pinhole_server(config)?;
    let client = pinhole_client(server)?;
    let from = SocketAddr::from((client, 0));
    let lifetime = u32::try_from(config.lifetime.as_secs()).unwrap_or(u32::MAX);
    let mut mapped = Vec::new();
    let mut requests = Vec::new();
    for mapping in mappings {
        let request = pcp::MapRequest {
            nonce: nonce(),
            protocol: mapping.protocol,
            internal_port: mapping.port,
            external_port: mapping.port,
            external_ip: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            client_ip: IpAddr::V6(client),
            lifetime,
        };
        let response = pcp_exchange_from(from, SocketAddr::V6(server), &request).await?;
        mapped.push(Mapped {
            mapping: *mapping,
            external_port: response.external_port,
        });
        requests.push(request);
    }
    tracing::info!(%client, ?mapped, "IPv6 pinholes open with PCP");
    Ok((
        Pinholes {
            server: SocketAddr::V6(server),
            from,
            requests,
        },
        mapped,
    ))
}

async fn close_pinholes(pinholes: &Pinholes) {
    for request in &pinholes.requests {
        let delete = pcp::MapRequest {
            lifetime: 0,
            external_port: 0,
            ..*request
        };
        if let Err(e) = pcp_exchange_from(pinholes.from, pinholes.server, &delete).await {
            tracing::debug!(port = request.internal_port, error = %e, "closing pinhole failed");
        }
    }
}

async fn map_natpmp(
    config: &PortMapConfig,
    mappings: &[Mapping],
) -> Result<(Active, Status), PortMapError> {
    let server = natpmp_server(config)?;
    let lifetime = u32::try_from(config.lifetime.as_secs()).unwrap_or(u32::MAX);
    let mut status = Status {
        method: Some(Method::NatPmp),
        ..Status::default()
    };
    if let natpmp::Response::ExternalAddress { addr, .. } =
        natpmp_exchange(server, natpmp::Request::ExternalAddress).await?
    {
        status.external_ip = Some(addr);
    }
    for mapping in mappings {
        let request = natpmp::Request::Map {
            protocol: mapping.protocol,
            internal_port: mapping.port,
            external_port: mapping.port,
            lifetime,
        };
        if let natpmp::Response::Map { external_port, .. } =
            natpmp_exchange(server, request).await?
        {
            if external_port != mapping.port {
                tracing::info!(
                    protocol = ?mapping.protocol,
                    listen_port = mapping.port,
                    external_port,
                    "NAT-PMP external port differs from the listen port; announcing the external one"
                );
            }
            status.mapped.push(Mapped {
                mapping: *mapping,
                external_port,
            });
        }
    }
    tracing::info!(?status, "ports mapped with NAT-PMP");
    Ok((Active::NatPmp(server), status))
}

async fn discover_upnp(config: &PortMapConfig) -> Result<Url, PortMapError> {
    if let Some(location) = &config.upnp_location {
        return Url::parse(location).ok_or(PortMapError::NoDevice);
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket
        .send_to(&upnp::search_request(), upnp::SSDP_ADDR)
        .await?;
    let mut buf = vec![0u8; 2048];
    let deadline = tokio::time::Instant::now() + config.timeout;
    while let Ok(received) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
        let (n, _) = received?;
        if let Some(url) = upnp::parse_search_response(buf.get(..n).unwrap_or_default()) {
            return Ok(url);
        }
    }
    Err(PortMapError::NoDevice)
}

async fn soap(
    config: &PortMapConfig,
    service: &upnp::Service,
    action: &upnp::Action,
) -> Result<String, PortMapError> {
    let headers = [
        ("Content-Type", "text/xml; charset=\"utf-8\""),
        ("SOAPAction", action.soap_action.as_str()),
    ];
    let response = http::request(
        &service.control,
        "POST",
        &headers,
        action.body.as_bytes(),
        config.timeout,
    )
    .await?;
    Ok(upnp::check_response(response.status, &response.body)?)
}

async fn map_upnp(
    config: &PortMapConfig,
    mappings: &[Mapping],
) -> Result<(Active, Status), PortMapError> {
    let location = discover_upnp(config).await?;
    let description = http::request(&location, "GET", &[], &[], config.timeout).await?;
    let xml = String::from_utf8_lossy(&description.body);
    let service = upnp::parse_description(&xml, &location).ok_or(PortMapError::NoDevice)?;
    let IpAddr::V4(local) = upnp::local_addr_towards(service.control.addr)? else {
        return Err(PortMapError::NoDevice);
    };
    let mut status = Status {
        method: Some(Method::Upnp),
        ..Status::default()
    };
    let lease = u32::try_from(config.lifetime.as_secs()).unwrap_or(u32::MAX);
    for mapping in mappings {
        let internal = SocketAddrV4::new(local, mapping.port);
        let add = |lease| {
            upnp::add_port_mapping(
                &service,
                mapping.protocol,
                mapping.port,
                internal,
                lease,
                &config.description,
            )
        };
        match soap(config, &service, &add(lease)).await {
            Ok(_) => {}
            // Old routers only take permanent mappings; we remove them on
            // stop.
            Err(PortMapError::Upnp(UpnpError::Fault { code, .. }))
                if code == upnp::ONLY_PERMANENT_LEASES =>
            {
                soap(config, &service, &add(0)).await?;
            }
            Err(e) => return Err(e),
        }
        // The device is not asked which external port it chose, so a
        // rewrite stays invisible here. Announcing a different port is
        // left to a later phase.
        status.mapped.push(Mapped {
            mapping: *mapping,
            external_port: mapping.port,
        });
    }
    if let Ok(body) = soap(config, &service, &upnp::get_external_ip(&service)).await {
        status.external_ip = upnp::parse_external_ip(&body);
    }
    tracing::info!(?status, "ports mapped with UPnP");
    Ok((Active::Upnp(service), status))
}

async fn unmap_all(config: &PortMapConfig, mappings: &[Mapping], active: &Active) {
    for mapping in mappings {
        let result = match active {
            Active::Pcp(server, requests) => {
                let Some(request) = requests
                    .iter()
                    .find(|r| r.protocol == mapping.protocol && r.internal_port == mapping.port)
                else {
                    continue;
                };
                let delete = pcp::MapRequest {
                    lifetime: 0,
                    external_port: 0,
                    ..*request
                };
                pcp_exchange(*server, &delete).await.map(drop)
            }
            Active::NatPmp(server) => {
                let request = natpmp::Request::Map {
                    protocol: mapping.protocol,
                    internal_port: mapping.port,
                    external_port: 0,
                    lifetime: 0,
                };
                natpmp_exchange(*server, request).await.map(drop)
            }
            Active::Upnp(service) => {
                let delete = upnp::delete_port_mapping(service, mapping.protocol, mapping.port);
                soap(config, service, &delete).await.map(drop)
            }
        };
        if let Err(e) = result {
            tracing::debug!(?mapping, error = %e, "removing port mapping failed");
        }
    }
}

/// Fuzzing entry point for the HTTP response parser; not a stable API.
#[doc(hidden)]
#[must_use]
pub fn fuzz_http_response(data: &[u8]) -> bool {
    http::parse_response(data).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{Mapped, Mapping, Protocol, Status};

    #[test]
    fn the_external_port_is_the_routers_choice_and_zero_is_unknown() {
        let mapped = |protocol, port, external_port| Mapped {
            mapping: Mapping { protocol, port },
            external_port,
        };
        let status = Status {
            mapped: vec![
                mapped(Protocol::Udp, 6881, 6881),
                mapped(Protocol::Tcp, 6881, 40_000),
            ],
            ..Status::default()
        };
        assert_eq!(status.external_port(Protocol::Tcp), Some(40_000));
        assert_eq!(status.external_port(Protocol::Udp), Some(6881));
        let unknown = Status {
            mapped: vec![mapped(Protocol::Tcp, 6881, 0)],
            ..Status::default()
        };
        assert_eq!(unknown.external_port(Protocol::Tcp), None);
        assert_eq!(Status::default().external_port(Protocol::Tcp), None);
    }
}
