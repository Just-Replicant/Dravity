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
    c.pcp = false;
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

/// Answers PCP MAP requests, mapping internal port p to p + 2000 on
/// 198.51.100.4, and records every request.
async fn fake_pcp() -> (SocketAddrV4, Arc<Mutex<Vec<Vec<u8>>>>) {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let std::net::SocketAddr::V4(addr) = socket.local_addr().unwrap() else {
        unreachable!()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        let mut buf = [0u8; 1100];
        loop {
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            let req = buf[..n].to_vec();
            log.lock().unwrap().push(req.clone());
            let internal = u16::from_be_bytes([req[40], req[41]]);
            let mut out = vec![2, 0x81, 0, 0];
            out.extend_from_slice(&req[4..8]);
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&[0; 12]);
            out.extend_from_slice(&req[24..42]);
            out.extend_from_slice(&(internal + 2000).to_be_bytes());
            out.extend_from_slice(&Ipv4Addr::new(198, 51, 100, 4).to_ipv6_mapped().octets());
            socket.send_to(&out, from).await.unwrap();
        }
    });
    (addr, seen)
}

type TimedRequests = Arc<Mutex<Vec<(tokio::time::Instant, Vec<u8>)>>>;

async fn wait_for_requests(seen: &TimedRequests, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if seen.lock().unwrap().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("PCP requests");
}

/// Grants TCP two seconds and UDP eight seconds so renewal must use the
/// shortest response lifetime, not the configured request lifetime.
async fn fake_timed_pcp_v4() -> (SocketAddrV4, TimedRequests) {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let std::net::SocketAddr::V4(addr) = socket.local_addr().unwrap() else {
        unreachable!()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        let mut buf = [0u8; 1100];
        loop {
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            let req = buf[..n].to_vec();
            log.lock()
                .unwrap()
                .push((tokio::time::Instant::now(), req.clone()));
            let requested = u32::from_be_bytes(req[4..8].try_into().unwrap());
            let granted: u32 = if requested == 0 {
                0
            } else if req[36] == 6 {
                2
            } else {
                8
            };
            let internal = u16::from_be_bytes([req[40], req[41]]);
            let mut out = vec![2, 0x81, 0, 0];
            out.extend_from_slice(&granted.to_be_bytes());
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&[0; 12]);
            out.extend_from_slice(&req[24..42]);
            out.extend_from_slice(&(internal + 2000).to_be_bytes());
            out.extend_from_slice(&Ipv4Addr::new(198, 51, 100, 4).to_ipv6_mapped().octets());
            socket.send_to(&out, from).await.unwrap();
        }
    });
    (addr, seen)
}

#[tokio::test]
async fn pcp_maps_reports_and_deletes_with_the_same_nonce() {
    let (gateway, seen) = fake_pcp().await;
    let mut c = config();
    c.upnp = false;
    c.gateway = Some(gateway);
    let mapper = PortMapper::start(c, mappings());
    let status = wait_for(&mapper, |s| s.method.is_some()).await;
    assert_eq!(status.method, Some(Method::Pcp));
    assert_eq!(status.external_ip, Some(Ipv4Addr::new(198, 51, 100, 4)));
    assert!(status.mapped.iter().all(|m| m.external_port == 8881));
    mapper.stop().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "two maps, two deletes");
    assert!(seen.iter().all(|r| r.len() == 60 && r[0] == 2 && r[1] == 1));
    assert_eq!(seen[0][36], 6, "TCP first");
    assert_eq!(&seen[0][4..8], &7200u32.to_be_bytes());
    for (map, delete) in seen[..2].iter().zip(&seen[2..]) {
        assert_eq!(&delete[4..8], &[0, 0, 0, 0], "lifetime 0 deletes");
        assert_eq!(&map[24..36], &delete[24..36], "same nonce");
    }
    assert_ne!(&seen[0][24..36], &seen[1][24..36], "a nonce per mapping");
}

#[tokio::test]
async fn pcp_renews_with_retained_identity_at_the_minimum_granted_lifetime() {
    let (gateway, seen) = fake_timed_pcp_v4().await;
    let mut c = config();
    c.ipv6 = false;
    c.natpmp = false;
    c.upnp = false;
    c.gateway = Some(gateway);
    c.lifetime = Duration::from_secs(20);
    let mapper = PortMapper::start(c, mappings());
    let status = wait_for(&mapper, |s| s.mapped.len() == 2).await;
    assert!(status.mapped.iter().all(|m| m.external_port == 8881));

    wait_for_requests(&seen, 4).await;
    mapper.stop().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 6, "two maps, two renewals, two deletes");
    let elapsed = seen[2].0.duration_since(seen[0].0);
    assert!(
        (Duration::from_millis(700)..Duration::from_secs(4)).contains(&elapsed),
        "renewed after {elapsed:?}, expected half the two-second grant"
    );
    let assigned_ip = Ipv4Addr::new(198, 51, 100, 4).to_ipv6_mapped().octets();
    for index in 0..2 {
        let open = &seen[index].1;
        let renew = &seen[index + 2].1;
        let delete = &seen[index + 4].1;
        assert_eq!(&renew[24..42], &open[24..42], "retained MAP identity");
        assert_eq!(&delete[24..42], &open[24..42], "retained delete identity");
        assert_eq!(&renew[42..44], &8881u16.to_be_bytes());
        assert_eq!(&delete[42..44], &8881u16.to_be_bytes());
        assert_eq!(&renew[44..60], &assigned_ip);
        assert_eq!(&delete[44..60], &assigned_ip);
        assert_eq!(&renew[4..8], &20u32.to_be_bytes());
        assert_eq!(&delete[4..8], &[0; 4]);
    }
}

#[tokio::test]
async fn a_nat_pmp_only_gateway_is_used_after_pcp_is_refused() {
    let (gateway, seen) = fake_natpmp().await;
    let mut c = config();
    c.upnp = false;
    c.gateway = Some(gateway);
    let mapper = PortMapper::start(c, mappings());
    let status = wait_for(&mapper, |s| s.method.is_some()).await;
    assert_eq!(status.method, Some(Method::NatPmp));
    mapper.stop().await;
    assert_eq!(seen.lock().unwrap()[0].len(), 60, "PCP was tried first");
}

/// Answers PCP MAP requests on `[::1]` with the requested port, and records
/// every request.
async fn fake_pcp_v6() -> (std::net::SocketAddrV6, Arc<Mutex<Vec<Vec<u8>>>>) {
    let socket = UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let std::net::SocketAddr::V6(addr) = socket.local_addr().unwrap() else {
        unreachable!()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        let mut buf = [0u8; 1100];
        loop {
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            let req = buf[..n].to_vec();
            log.lock().unwrap().push(req.clone());
            let mut out = vec![2, 0x81, 0, 0];
            out.extend_from_slice(&req[4..8]);
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&[0; 12]);
            out.extend_from_slice(&req[24..42]);
            out.extend_from_slice(&req[40..42]);
            out.extend_from_slice(&req[8..24]);
            socket.send_to(&out, from).await.unwrap();
        }
    });
    (addr, seen)
}

async fn fake_timed_pcp_v6() -> (std::net::SocketAddrV6, TimedRequests) {
    let socket = UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let std::net::SocketAddr::V6(addr) = socket.local_addr().unwrap() else {
        unreachable!()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        let mut buf = [0u8; 1100];
        loop {
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            let req = buf[..n].to_vec();
            log.lock()
                .unwrap()
                .push((tokio::time::Instant::now(), req.clone()));
            let requested = u32::from_be_bytes(req[4..8].try_into().unwrap());
            let granted: u32 = if requested == 0 {
                0
            } else if req[36] == 6 {
                2
            } else {
                8
            };
            let internal = u16::from_be_bytes([req[40], req[41]]);
            let mut out = vec![2, 0x81, 0, 0];
            out.extend_from_slice(&granted.to_be_bytes());
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&[0; 12]);
            out.extend_from_slice(&req[24..42]);
            out.extend_from_slice(&(internal + 3000).to_be_bytes());
            out.extend_from_slice(
                &"2001:db8::5"
                    .parse::<std::net::Ipv6Addr>()
                    .unwrap()
                    .octets(),
            );
            socket.send_to(&out, from).await.unwrap();
        }
    });
    (addr, seen)
}

#[tokio::test]
async fn ipv6_pinholes_open_from_our_address_and_close_on_stop() {
    let (gateway, seen) = fake_pcp_v6().await;
    let mut c = config();
    c.pcp = false;
    c.natpmp = false;
    c.upnp = false;
    c.gateway_v6 = Some(gateway);
    let mapper = PortMapper::start(c, mappings());
    let status = wait_for(&mapper, |s| s.pinholes.len() == 2).await;
    assert_eq!(status.pinhole_error, None);
    assert!(status.mapped.is_empty(), "IPv4 is independent: {status:?}");
    assert!(status.error.is_some());
    assert!(status.pinholes.iter().all(|m| m.external_port == 6881));
    mapper.stop().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "two pinholes, two deletes");
    for (open, close) in seen[..2].iter().zip(&seen[2..]) {
        assert_eq!(open[1], 1, "MAP");
        assert_eq!(
            &open[8..24],
            &std::net::Ipv6Addr::LOCALHOST.octets(),
            "the client address is the one requests come from"
        );
        assert_eq!(&open[44..60], &[0; 16], "any external address");
        assert_eq!(&close[4..8], &[0, 0, 0, 0], "lifetime 0 closes");
        assert_eq!(&close[24..36], &open[24..36], "same nonce");
    }
    assert_eq!(seen[0][36], 6, "TCP first");
}

#[tokio::test]
async fn ipv6_pinholes_renew_and_close_with_the_retained_identity() {
    let (gateway, seen) = fake_timed_pcp_v6().await;
    let mut c = config();
    c.pcp = false;
    c.natpmp = false;
    c.upnp = false;
    c.gateway_v6 = Some(gateway);
    c.lifetime = Duration::from_secs(20);
    c.retry = Duration::from_secs(10);
    let mapper = PortMapper::start(c, mappings());
    let status = wait_for(&mapper, |s| s.pinholes.len() == 2).await;
    assert!(status.pinholes.iter().all(|m| m.external_port == 9881));

    wait_for_requests(&seen, 4).await;
    mapper.stop().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 6, "two opens, two renewals, two closes");
    let elapsed = seen[2].0.duration_since(seen[0].0);
    assert!(
        (Duration::from_millis(700)..Duration::from_secs(4)).contains(&elapsed),
        "renewed after {elapsed:?}, expected half the two-second grant"
    );
    let assigned_ip = "2001:db8::5"
        .parse::<std::net::Ipv6Addr>()
        .unwrap()
        .octets();
    for index in 0..2 {
        let open = &seen[index].1;
        let renew = &seen[index + 2].1;
        let close = &seen[index + 4].1;
        assert_eq!(&renew[24..42], &open[24..42], "retained MAP identity");
        assert_eq!(&close[24..42], &open[24..42], "retained close identity");
        assert_eq!(&renew[42..44], &9881u16.to_be_bytes());
        assert_eq!(&close[42..44], &9881u16.to_be_bytes());
        assert_eq!(&renew[44..60], &assigned_ip);
        assert_eq!(&close[44..60], &assigned_ip);
        assert_eq!(&renew[4..8], &20u32.to_be_bytes());
        assert_eq!(&close[4..8], &[0; 4]);
    }
}

#[tokio::test]
async fn without_ipv6_no_pinhole_is_tried() {
    let (gateway, seen) = fake_pcp_v6().await;
    let (v4, _) = fake_natpmp().await;
    let mut c = config();
    c.pcp = false;
    c.upnp = false;
    c.gateway = Some(v4);
    c.ipv6 = false;
    c.gateway_v6 = Some(gateway);
    let mapper = PortMapper::start(c, mappings());
    let status = wait_for(&mapper, |s| s.method.is_some()).await;
    assert!(status.pinholes.is_empty());
    assert_eq!(status.pinhole_error, None);
    mapper.stop().await;
    assert!(seen.lock().unwrap().is_empty());
}
