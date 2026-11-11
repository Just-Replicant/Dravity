//! Addresses for web seeds.
//!
//! The driver presents each web seed (BEP 19) to the engine as an incoming
//! peer. Its HTTP server's address would be a poor peer address: two web
//! seeds on one host and port (`http://cdn/a`, `http://cdn/b`) would collide
//! (the engine keeps one connection per address), a real BitTorrent peer at
//! that address would be skipped while the web seed is connected, and a
//! disconnect would schedule a BitTorrent dial to port 80 or 443.
//!
//! Instead each web seed gets a synthetic address in `100::/64`, the IPv6
//! discard-only prefix (RFC 6666): it is never routed, so it cannot belong
//! to a real peer, and nothing sent there would arrive anywhere. The
//! engine never dials, retries, shares (PEX) or pools these addresses,
//! whoever names them; trackers and the DHT only ever get our own port.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

/// The address web seed number `index` of a torrent is reported under:
/// `[100::<index>]:0`, distinct for every index.
///
/// ```
/// use dravity_engine::{is_web_seed_addr, web_seed_addr};
/// assert_ne!(web_seed_addr(0), web_seed_addr(1));
/// assert!(is_web_seed_addr(web_seed_addr(7)));
/// assert!(!is_web_seed_addr("[2001:db8::1]:80".parse().unwrap()));
/// ```
#[must_use]
pub fn web_seed_addr(index: u32) -> SocketAddr {
    let [a, b, c, d] = index.to_be_bytes();
    let ip = Ipv6Addr::new(
        0x0100,
        0,
        0,
        0,
        0,
        0,
        u16::from_be_bytes([a, b]),
        u16::from_be_bytes([c, d]),
    );
    SocketAddr::new(IpAddr::V6(ip), 0)
}

/// Whether `addr` lies in `100::/64`, where web seed addresses live. Such
/// an address is never a dialable peer.
#[must_use]
pub fn is_web_seed_addr(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V6(ip) => matches!(ip.segments(), [0x0100, 0, 0, 0, ..]),
        IpAddr::V4(_) => false,
    }
}
