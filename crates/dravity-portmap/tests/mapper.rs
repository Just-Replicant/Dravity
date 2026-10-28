//! The mapper against a fake NAT-PMP gateway and a fake UPnP device.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dravity_portmap::{Mapping, Method, PortMapConfig, PortMapper, Protocol, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

fn mappings() -> Vec<Mapping> {
    vec![
        Mapping {
            protocol: Protocol::Tcp,
            port: 6881,
        },
        Mapping {
            protocol: Protocol::Udp,
            port: 6881,
        },
    ]
}

fn config() -> PortMapConfig {
    let mut c = PortMapConfig::default();
    c.timeout = Duration::from_secs(2);
    c.retry = Duration::from_secs(1);
    c
}

/// Answers NAT-PMP requests, mapping internal port p to p + 1000, and
/// records every request.
async fn fake_natpmp() -> (SocketAddrV4, Arc<Mutex<Vec<Vec<u8>>>>) {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let std::net::SocketAddr::V4(addr) = socket.local_addr().unwrap() else {
        unreachable!()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            let req = buf[..n].to_vec();
            log.lock().unwrap().push(req.clone());
            let mut out = vec![0, 128 + req[1], 0, 0, 0, 0, 0, 7];
            if req[1] == 0 {
                out.extend_from_slice(&[203, 0, 113, 9]);
            } else {
                let internal = u16::from_be_bytes([req[4], req[5]]);
                out.extend_from_slice(&req[4..6]);
                out.extend_from_slice(&(internal + 1000).to_be_bytes());
                out.extend_from_slice(&req[8..12]);
            }
            socket.send_to(&out, from).await.unwrap();
        }
    });
    (addr, seen)
}

async fn wait_for(mapper: &PortMapper, done: impl Fn(&Status) -> bool) -> Status {
    let mut rx = mapper.subscribe();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let status = rx.borrow_and_update().clone();
            if done(&status) {
                return status;
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("mapper status")
}

#[tokio::test]
async fn natpmp_maps_reports_and_removes() {
    let (gateway, seen) = fake_natpmp().await;
    let mut c = config();
    c.upnp = false;
    c.gateway = Some(gateway);
    let mapper = PortMapper::start(c, mappings());
    let status = wait_for(&mapper, |s| s.method.is_some()).await;
    assert_eq!(status.method, Some(Method::NatPmp));
    assert_eq!(status.external_ip, Some(Ipv4Addr::new(203, 0, 113, 9)));
    assert_eq!(status.mapped.len(), 2);
    assert!(status.mapped.iter().all(|m| m.external_port == 7881));
    mapper.stop().await;
    let seen = seen.lock().unwrap();
    // External address, two maps, two deletes (lifetime 0).
    assert_eq!(seen.len(), 5, "{seen:?}");
    for delete in &seen[3..] {
        assert_eq!(&delete[6..12], &[0, 0, 0, 0, 0, 0]);
    }
    assert_eq!(seen[1][1], 2, "TCP first");
    assert_eq!(&seen[1][8..12], &7200u32.to_be_bytes());
}

#[derive(Default)]
struct Device {
    /// SOAP actions received, by `SOAPAction` header.
    actions: Mutex<Vec<String>>,
    /// Refuse leases other than 0 with error 725.
    permanent_only: bool,
}

async fn fake_upnp(device: Arc<Device>) -> String {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let description = format!(
        "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\">\
         <URLBase>http://{addr}/</URLBase><device><serviceList>\
         <service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>\
         <controlURL>/l3f</controlURL></service></serviceList><deviceList><device>\
         <serviceList><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
         <controlURL>/ctl/IPConn</controlURL></service></serviceList></device></deviceList>\
         </device></root>"
    );
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let device = Arc::clone(&device);
            let description = description.clone();
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buf = [0u8; 4096];
                // Read the head and the Content-Length body.
                let (head, body) = loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    data.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&data).into_owned();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len: usize = text
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .map_or(0, |v| v.trim().parse().unwrap());
                        if text.len() >= end + 4 + len {
                            break (text[..end].to_owned(), text[end + 4..].to_owned());
                        }
                    }
                };
                let (status, reply) = if head.starts_with("GET /desc.xml ") {
                    ("200 OK", description)
                } else if head.starts_with("POST /ctl/IPConn ") {
                    let action = head
                        .lines()
                        .find_map(|l| l.strip_prefix("SOAPAction: "))
                        .unwrap()
                        .to_owned();
                    device.actions.lock().unwrap().push(action.clone());
                    if action.contains("#GetExternalIPAddress") {
                        (
                            "200 OK",
                            "<s:Envelope><s:Body><u:GetExternalIPAddressResponse>\
                             <NewExternalIPAddress>198.51.100.4</NewExternalIPAddress>\
                             </u:GetExternalIPAddressResponse></s:Body></s:Envelope>"
                                .to_owned(),
                        )
                    } else if device.permanent_only
                        && action.contains("#AddPortMapping")
                        && !body.contains("<NewLeaseDuration>0<")
                    {
                        (
                            "500 Internal Server Error",
                            "<s:Envelope><s:Body><s:Fault><detail><UPnPError>\
                             <errorCode>725</errorCode><errorDescription>OnlyPermanentLeasesSupported\
                             </errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
                                .to_owned(),
                        )
                    } else {
                        ("200 OK", "<s:Envelope/>".to_owned())
                    }
                } else {
                    ("404 Not Found", String::new())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{reply}",
                    reply.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    format!("http://{addr}/desc.xml")
}

async fn upnp_round_trip(device: Arc<Device>, natpmp: bool) -> Vec<String> {
    let mut c = config();
    c.upnp_location = Some(fake_upnp(Arc::clone(&device)).await);
    c.natpmp = natpmp;
    if natpmp {
        // Nothing listens there: NAT-PMP fails and UPnP is tried.
        let unused = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let std::net::SocketAddr::V4(addr) = unused.local_addr().unwrap() else {
            unreachable!()
        };
        drop(unused);
        c.gateway = Some(addr);
    }
    let mapper = PortMapper::start(c, mappings());
    let status = wait_for(&mapper, |s| s.method.is_some()).await;
    assert_eq!(status.method, Some(Method::Upnp));
    assert_eq!(status.external_ip, Some(Ipv4Addr::new(198, 51, 100, 4)));
    assert_eq!(status.mapped.len(), 2);
    mapper.stop().await;
    let actions = device.actions.lock().unwrap().clone();
    actions
        .iter()
        .map(|a| {
            a.rsplit('#')
                .next()
                .unwrap()
                .trim_end_matches('"')
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn upnp_maps_and_removes() {
    let actions = upnp_round_trip(Arc::new(Device::default()), false).await;
    assert_eq!(
        actions,
        [
            "AddPortMapping",
            "AddPortMapping",
            "GetExternalIPAddress",
            "DeletePortMapping",
            "DeletePortMapping"
        ]
    );
}

#[tokio::test]
async fn upnp_falls_back_to_permanent_leases_after_natpmp_fails() {
    let device = Arc::new(Device {
        permanent_only: true,
        ..Device::default()
    });
    let actions = upnp_round_trip(device, true).await;
    assert_eq!(actions.iter().filter(|a| *a == "AddPortMapping").count(), 4);
    assert_eq!(
        actions.iter().filter(|a| *a == "DeletePortMapping").count(),
        2
    );
}

#[tokio::test]
async fn failure_is_reported_and_retried() {
    let mut c = config();
    c.upnp = false;
    let unused = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let std::net::SocketAddr::V4(addr) = unused.local_addr().unwrap() else {
        unreachable!()
    };
    drop(unused);
    c.gateway = Some(addr);
    let mapper = PortMapper::start(c, mappings());
    let status = wait_for(&mapper, |s| s.error.is_some()).await;
    assert_eq!(status.method, None);
    assert!(status.mapped.is_empty());
    mapper.stop().await;
}
