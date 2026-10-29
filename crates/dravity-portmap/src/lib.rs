//! Router port mapping for Dravity: NAT-PMP (RFC 6886) and UPnP IGD.
//!
//! Peers can only connect to us if the home router forwards our listening
//! port. [`PortMapper`] asks the router to do so, NAT-PMP first (one UDP
//! round trip to the default gateway) and UPnP IGD second, renews the
//! mappings at half their lifetime and removes them when stopped. The
//! external port is requested to equal the listen port. When NAT-PMP
//! returns a different one, [`Status::external_port`] reports it, so the
//! session announces the port peers can actually reach. Failure is normal (no router, feature disabled,
//! carrier-grade NAT) and only reported in [`Status`]; outgoing
//! connections work regardless.
//!
//! The protocol modules ([`natpmp`], [`upnp`]) are sans-IO and fuzzed; the
//! mapper task does the networking.

mod gateway;
mod http;
pub mod natpmp;
pub mod upnp;

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use gateway::{
    default_gateway, parse_proc_net_route, parse_route_get_default, parse_windows_route,
};
pub use http::Url;
pub use natpmp::{NatPmpError, Protocol};
pub use upnp::UpnpError;

/// Port mapping settings.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PortMapConfig {
    /// Try NAT-PMP.
    pub natpmp: bool,
    /// Try UPnP IGD.
    pub upnp: bool,
    /// NAT-PMP server; `None` uses the default gateway on port 5351.
    pub gateway: Option<SocketAddrV4>,
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
            natpmp: true,
            upnp: true,
            gateway: None,
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
    /// UPnP device error.
    #[error(transparent)]
    Upnp(#[from] UpnpError),
    /// Network error (including timeouts: no gateway answered).
    #[error(transparent)]
    Io(#[from] io::Error),
    /// No default gateway is known.
    #[error("no default gateway")]
    NoGateway,
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
    NatPmp(SocketAddrV4),
    Upnp(upnp::Service),
}

async fn run(
    config: PortMapConfig,
    mappings: Vec<Mapping>,
    status: watch::Sender<Status>,
    stop: CancellationToken,
) {
    loop {
        let (active, wait) = match map_all(&config, &mappings).await {
            Ok((active, next)) => {
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
                });
                (None, config.retry)
            }
        };
        let stopped = tokio::select! {
            () = stop.cancelled() => true,
            () = tokio::time::sleep(wait.max(Duration::from_secs(1))) => false,
        };
        if stopped {
            if let Some(active) = active {
                unmap_all(&config, &mappings, &active).await;
            }
            status.send_modify(|s| s.mapped.clear());
            return;
        }
    }
}

async fn map_all(
    config: &PortMapConfig,
    mappings: &[Mapping],
) -> Result<(Active, Status), PortMapError> {
    let mut last = PortMapError::NoGateway;
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
    let std::net::IpAddr::V4(local) = upnp::local_addr_towards(service.control.addr)? else {
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
