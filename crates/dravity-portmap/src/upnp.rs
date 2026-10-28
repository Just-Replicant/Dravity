//! UPnP Internet Gateway Device port mapping: SSDP discovery, the device
//! description and the `WANIPConnection` / `WANPPPConnection` SOAP actions.
//!
//! Everything here is sans-IO text processing; [`crate::PortMapper`] does
//! the networking. XML is read with a tolerant tag scanner: device
//! descriptions are small and machine-generated, and a full XML parser
//! would be a large dependency for three fields.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use crate::http::Url;
use crate::natpmp::Protocol;

/// SSDP multicast group and port.
pub const SSDP_ADDR: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);

/// Services that can map ports, most capable first.
const SERVICES: [&str; 3] = [
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];

/// The SSDP `M-SEARCH` for Internet gateways.
#[must_use]
pub fn search_request() -> Vec<u8> {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_ADDR}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n\
         ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n"
    )
    .into_bytes()
}

/// The description URL (`LOCATION`) of an SSDP answer, if it is one.
#[must_use]
pub fn parse_search_response(data: &[u8]) -> Option<Url> {
    let text = std::str::from_utf8(data).ok()?;
    let mut lines = text.split("\r\n");
    if !lines.next()?.starts_with("HTTP/1.1 200") {
        return None;
    }
    lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("location")
            .then(|| Url::parse(value))
            .flatten()
    })
}

/// A service that accepts port-mapping actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Service type URN (the SOAP namespace).
    pub service_type: String,
    /// Where to post actions.
    pub control: Url,
}

/// The text of every `<tag>...</tag>` in `xml`, ignoring namespace
/// prefixes and attributes.
fn elements<'a>(xml: &'a str, tag: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    let mut rest = xml;
    std::iter::from_fn(move || {
        loop {
            let start = rest.find('<')?;
            rest = rest.get(start + 1..)?;
            let end = rest.find('>')?;
            let open = rest.get(..end)?;
            rest = rest.get(end + 1..)?;
            if open.starts_with('/') || open.ends_with('/') {
                continue;
            }
            let name = open.split_whitespace().next().unwrap_or_default();
            let local = name.rsplit(':').next().unwrap_or(name);
            if local != tag {
                continue;
            }
            let close = rest.find("</")?;
            let text = rest.get(..close)?;
            return Some(text.trim());
        }
    })
}

/// Finds the port-mapping service in a device description.
#[must_use]
pub fn parse_description(xml: &str, location: &Url) -> Option<Service> {
    // `URLBase` (UPnP 1.0) overrides the description's own location.
    let base = elements(xml, "URLBase")
        .next()
        .and_then(Url::parse)
        .unwrap_or_else(|| location.clone());
    let mut found: Vec<(usize, Service)> = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<service>").or_else(|| rest.find("<service ")) {
        let after = rest.get(start..)?;
        let end = after.find("</service>")?;
        let block = after.get(..end)?;
        rest = after.get(end + 10..)?;
        let Some(service_type) = elements(block, "serviceType").next() else {
            continue;
        };
        let Some(rank) = SERVICES.iter().position(|s| *s == service_type) else {
            continue;
        };
        let Some(control) = elements(block, "controlURL")
            .next()
            .and_then(|c| base.join(c))
        else {
            continue;
        };
        found.push((
            rank,
            Service {
                service_type: service_type.to_owned(),
                control,
            },
        ));
    }
    found
        .into_iter()
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, s)| s)
}

const fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Tcp => "TCP",
        Protocol::Udp => "UDP",
    }
}

fn envelope(service: &Service, action: &str, args: &[(&str, String)]) -> String {
    let mut body = String::new();
    for (name, value) in args {
        body.push_str(&format!("<{name}>{value}</{name}>"));
    }
    format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
         <u:{action} xmlns:u=\"{}\">{body}</u:{action}></s:Body></s:Envelope>",
        service.service_type
    )
}

/// A SOAP action: the `SOAPAction` header value and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// `SOAPAction` header value (quoted).
    pub soap_action: String,
    /// XML body.
    pub body: String,
}

/// `AddPortMapping` for `internal` (our LAN address) with `lease` seconds
/// (0: permanent, which some routers require).
#[must_use]
pub fn add_port_mapping(
    service: &Service,
    protocol: Protocol,
    external_port: u16,
    internal: SocketAddrV4,
    lease: u32,
    description: &str,
) -> Action {
    let args = [
        ("NewRemoteHost", String::new()),
        ("NewExternalPort", external_port.to_string()),
        ("NewProtocol", protocol_name(protocol).to_owned()),
        ("NewInternalPort", internal.port().to_string()),
        ("NewInternalClient", internal.ip().to_string()),
        ("NewEnabled", "1".to_owned()),
        ("NewPortMappingDescription", escape(description)),
        ("NewLeaseDuration", lease.to_string()),
    ];
    Action {
        soap_action: format!("\"{}#AddPortMapping\"", service.service_type),
        body: envelope(service, "AddPortMapping", &args),
    }
}

/// `DeletePortMapping`.
#[must_use]
pub fn delete_port_mapping(service: &Service, protocol: Protocol, external_port: u16) -> Action {
    let args = [
        ("NewRemoteHost", String::new()),
        ("NewExternalPort", external_port.to_string()),
        ("NewProtocol", protocol_name(protocol).to_owned()),
    ];
    Action {
        soap_action: format!("\"{}#DeletePortMapping\"", service.service_type),
        body: envelope(service, "DeletePortMapping", &args),
    }
}

/// `GetExternalIPAddress`.
#[must_use]
pub fn get_external_ip(service: &Service) -> Action {
    Action {
        soap_action: format!("\"{}#GetExternalIPAddress\"", service.service_type),
        body: envelope(service, "GetExternalIPAddress", &[]),
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A failed SOAP action.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum UpnpError {
    /// The device returned a UPnP error (for example 718: conflict, 725:
    /// only permanent leases supported).
    #[error("UPnP error {code}: {description}")]
    Fault {
        /// UPnP error code.
        code: u16,
        /// Device's description.
        description: String,
    },
    /// An HTTP error without a UPnP fault.
    #[error("HTTP status {0}")]
    Status(u16),
}

/// UPnP error: only permanent leases are supported.
pub const ONLY_PERMANENT_LEASES: u16 = 725;

/// Checks a SOAP response; returns its body on success.
///
/// # Errors
/// [`UpnpError`] for a fault or an HTTP error status.
pub fn check_response(status: u16, body: &[u8]) -> Result<String, UpnpError> {
    let text = String::from_utf8_lossy(body).into_owned();
    if status == 200 {
        return Ok(text);
    }
    match elements(&text, "errorCode")
        .next()
        .and_then(|c| c.parse().ok())
    {
        Some(code) => Err(UpnpError::Fault {
            code,
            description: elements(&text, "errorDescription")
                .next()
                .unwrap_or_default()
                .to_owned(),
        }),
        None => Err(UpnpError::Status(status)),
    }
}

/// The address in a `GetExternalIPAddress` response.
#[must_use]
pub fn parse_external_ip(body: &str) -> Option<Ipv4Addr> {
    elements(body, "NewExternalIPAddress").next()?.parse().ok()
}

/// Our address on the interface that reaches `gateway` (no packet is sent).
///
/// # Errors
/// If no route to `gateway` exists.
pub fn local_addr_towards(gateway: SocketAddr) -> std::io::Result<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0))?;
    socket.connect(gateway)?;
    Ok(socket.local_addr()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
 <device>
  <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
  <deviceList><device>
   <serviceList>
    <service>
     <serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType>
     <controlURL>/ctl/CmnIfCfg</controlURL>
    </service>
   </serviceList>
   <deviceList><device>
    <serviceList>
     <service>
      <serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>
      <controlURL>/ctl/PPP</controlURL>
     </service>
     <service>
      <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
      <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>
      <controlURL>ctl/IPConn</controlURL>
     </service>
    </serviceList>
   </device></deviceList>
  </device></deviceList>
 </device>
</root>"#;

    #[test]
    fn discovery_and_description() {
        let answer = b"HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\n\
            Location: http://192.168.1.1:5000/rootDesc.xml\r\nST: urn:x\r\n\r\n";
        let location = parse_search_response(answer).unwrap();
        assert_eq!(location.path, "/rootDesc.xml");
        assert!(parse_search_response(b"NOTIFY * HTTP/1.1\r\n\r\n").is_none());
        let service = parse_description(DESCRIPTION, &location).unwrap();
        assert_eq!(
            service.service_type,
            "urn:schemas-upnp-org:service:WANIPConnection:1"
        );
        assert_eq!(service.control.path, "/ctl/IPConn");
        assert!(parse_description("<root/>", &location).is_none());
        assert!(
            String::from_utf8(search_request())
                .unwrap()
                .contains("ssdp:discover")
        );
    }

    #[test]
    fn actions_and_responses() {
        let service = Service {
            service_type: SERVICES[1].to_owned(),
            control: Url::parse("http://192.168.1.1:5000/ctl").unwrap(),
        };
        let add = add_port_mapping(
            &service,
            Protocol::Tcp,
            6881,
            "192.168.1.20:6881".parse().unwrap(),
            3600,
            "Dravity <tcp>",
        );
        assert_eq!(
            add.soap_action,
            "\"urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping\""
        );
        for part in [
            "<NewExternalPort>6881</NewExternalPort>",
            "<NewProtocol>TCP</NewProtocol>",
            "<NewInternalClient>192.168.1.20</NewInternalClient>",
            "<NewLeaseDuration>3600</NewLeaseDuration>",
            "Dravity &lt;tcp&gt;",
        ] {
            assert!(add.body.contains(part), "{part} in {}", add.body);
        }
        let del = delete_port_mapping(&service, Protocol::Udp, 6881);
        assert!(del.body.contains("<NewProtocol>UDP</NewProtocol>"));

        let fault = br#"<s:Envelope><s:Body><s:Fault><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
            <errorCode>725</errorCode><errorDescription>OnlyPermanentLeasesSupported</errorDescription>
            </UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;
        assert_eq!(
            check_response(500, fault),
            Err(UpnpError::Fault {
                code: ONLY_PERMANENT_LEASES,
                description: "OnlyPermanentLeasesSupported".into()
            })
        );
        assert_eq!(check_response(404, b""), Err(UpnpError::Status(404)));
        let ok = "<s:Envelope><s:Body><u:GetExternalIPAddressResponse xmlns:u=\"x\">\
            <NewExternalIPAddress>203.0.113.9</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>";
        let body = check_response(200, ok.as_bytes()).unwrap();
        assert_eq!(
            parse_external_ip(&body),
            Some(Ipv4Addr::new(203, 0, 113, 9))
        );
    }
}
