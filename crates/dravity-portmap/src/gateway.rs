//! Finding the default gateway for NAT-PMP.

use std::net::Ipv4Addr;

/// The default IPv4 gateway in Linux's `/proc/net/route` format: the
/// `Gateway` column (little-endian hex) of the first up route with
/// destination `00000000`.
#[must_use]
pub fn parse_proc_net_route(text: &str) -> Option<Ipv4Addr> {
    text.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (destination, gateway, flags) = (fields.get(1)?, fields.get(2)?, fields.get(3)?);
        let flags = u16::from_str_radix(flags, 16).ok()?;
        // RTF_UP | RTF_GATEWAY
        if *destination != "00000000" || flags & 0x3 != 0x3 {
            return None;
        }
        let raw = u32::from_str_radix(gateway, 16).ok()?;
        Some(Ipv4Addr::from(raw.swap_bytes()))
    })
}

/// The default gateway: from the routing table on Linux, otherwise the
/// `.1` address of our subnet (the usual home router address).
#[must_use]
pub fn default_gateway() -> Option<Ipv4Addr> {
    if let Some(gw) = std::fs::read_to_string("/proc/net/route")
        .ok()
        .as_deref()
        .and_then(parse_proc_net_route)
    {
        return Some(gw);
    }
    // TEST-NET-1 is never on the LAN: the route to it is the default one.
    let local = crate::upnp::local_addr_towards(([192, 0, 2, 1], 9).into()).ok()?;
    let std::net::IpAddr::V4(v4) = local else {
        return None;
    };
    if !v4.is_private() {
        return None;
    }
    let [a, b, c, _] = v4.octets();
    Some(Ipv4Addr::new(a, b, c, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_default_route() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            eth0\t0002A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
            eth0\t00000000\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n";
        assert_eq!(
            parse_proc_net_route(table),
            Some(Ipv4Addr::new(192, 168, 2, 1))
        );
        assert_eq!(parse_proc_net_route("Iface\tDestination\n"), None);
    }
}
