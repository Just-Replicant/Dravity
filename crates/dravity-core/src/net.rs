//! Address classification shared by every component that connects to a
//! host named by untrusted input (tracker URLs, web seeds, peers).
//!
//! A torrent's `announce`/`url-list` (or a magnet's `tr`/`ws`) is
//! attacker-controlled. Without a check a torrent could make the client
//! probe its LAN, a router's admin page or a cloud metadata endpoint
//! (`169.254.169.254`), a server-side request forgery (SSRF).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Whether `ip` is a publicly routable unicast address.
///
/// Refused: unspecified and "this network" (`0.0.0.0/8`, `::`), loopback
/// (`127.0.0.0/8`, `::1`), RFC 1918 private (`10/8`, `172.16/12`,
/// `192.168/16`), CGNAT (`100.64.0.0/10`), link-local (`169.254/16`,
/// `fe80::/10`), IETF protocol assignments (`192.0.0.0/24`), benchmarking
/// (`198.18.0.0/15`), documentation (`192.0.2/24`, `198.51.100/24`,
/// `203.0.113/24`, `2001:db8::/32`), multicast, reserved (`240.0.0.0/4`,
/// broadcast), unique-local (`fc00::/7`), discard-only (`100::/64`), and
/// IPv4-mapped or -compatible IPv6 addresses whose IPv4 address is refused.
///
/// ```
/// use dravity_core::net::is_public;
/// assert!(is_public("93.184.215.14".parse().unwrap()));
/// assert!(!is_public("169.254.169.254".parse().unwrap()));
/// assert!(!is_public("::ffff:10.0.0.1".parse().unwrap()));
/// ```
#[must_use]
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b & 0xfe) == 18))
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let [s0, s1, s2, s3, s4, s5, _, _] = ip.segments();
    // IPv4-compatible (deprecated) `::a.b.c.d`.
    if [s0, s1, s2, s3, s4, s5] == [0; 6] && !ip.is_unspecified() && !ip.is_loopback() {
        let [.., a, b, c, d] = ip.octets();
        return is_public_v4(Ipv4Addr::new(a, b, c, d));
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s0 & 0xfe00) == 0xfc00
        || (s0 & 0xffc0) == 0xfe80
        || (s0 == 0x2001 && s1 == 0x0db8)
        || [s0, s1, s2, s3] == [0x0100, 0, 0, 0])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        for public in ["1.1.1.1", "93.184.215.14", "100.128.0.1", "2606:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
        for private in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.169.254",
            "192.0.0.8",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "100::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::10.0.0.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
    }
}
