//! Finding the default gateway for NAT-PMP.
//!
//! The NAT-PMP server is the next hop of the IPv4 default route: Linux reads
//! `/proc/net/route`, Windows parses `route print -4`, and macOS parses
//! `route -n get default`. That hop is used as-is, including on CGNAT
//! (100.64.0.0/10), which [`Ipv4Addr::is_private`](std::net::Ipv4Addr::is_private)
//! does not treat as local. Guessing the `.1` address of the local `/24`
//! sends a host such as 10.1.2.5/8 to 10.1.2.1. When the route cannot be
//! read, NAT-PMP is skipped; UPnP can still find a gateway with SSDP.

use std::net::{Ipv4Addr, Ipv6Addr};

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

/// The default IPv4 gateway in `route print -4` output: the `Gateway` of a
/// `0.0.0.0/0.0.0.0` route with the lowest metric. A `0.0.0.0/1` split
/// (mask `128.0.0.0`, often `On-link`) is not the default route.
#[must_use]
pub fn parse_windows_route(text: &str) -> Option<Ipv4Addr> {
    let mut best: Option<(u32, Ipv4Addr)> = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(dest) = fields.first().copied() else {
            continue;
        };
        let Some(mask) = fields.get(1).copied() else {
            continue;
        };
        let Some(gateway) = fields.get(2).copied() else {
            continue;
        };
        let Some(metric) = fields.get(4).copied() else {
            continue;
        };
        if dest != "0.0.0.0" || mask != "0.0.0.0" {
            continue;
        }
        let (Ok(gateway), Ok(metric)) = (gateway.parse::<Ipv4Addr>(), metric.parse::<u32>()) else {
            continue;
        };
        if gateway.is_unspecified() {
            continue;
        }
        if best.is_none_or(|(lowest, _)| metric < lowest) {
            best = Some((metric, gateway));
        }
    }
    best.map(|(_, gateway)| gateway)
}

/// The gateway line of `route -n get default` (macOS and other BSDs).
#[must_use]
pub fn parse_route_get_default(text: &str) -> Option<Ipv4Addr> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next() != Some("gateway:") {
            return None;
        }
        let gateway = parts.next()?.parse::<Ipv4Addr>().ok()?;
        (!gateway.is_unspecified()).then_some(gateway)
    })
}

/// The IPv6 default router in Linux's `/proc/net/ipv6_route` format: the
/// next hop and interface name of the first up gateway route to `::/0`.
///
/// ```
/// use std::net::Ipv6Addr;
///
/// let table = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 \
///     fe800000000000000000000000000001 00000400 00000001 00000000 00450003 eth0\n";
/// let (router, iface) = dravity_portmap::parse_proc_net_ipv6_route(table).unwrap();
/// assert_eq!(router, "fe80::1".parse::<Ipv6Addr>().unwrap());
/// assert_eq!(iface, "eth0");
/// ```
#[must_use]
pub fn parse_proc_net_ipv6_route(text: &str) -> Option<(Ipv6Addr, String)> {
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (dest, plen, next_hop, flags, iface) = (
            fields.first()?,
            fields.get(1)?,
            fields.get(4)?,
            fields.get(8)?,
            fields.get(9)?,
        );
        let flags = u32::from_str_radix(flags, 16).ok()?;
        // RTF_UP | RTF_GATEWAY
        if dest.len() != 32
            || dest.bytes().any(|b| b != b'0')
            || *plen != "00"
            || flags & 0x3 != 0x3
        {
            return None;
        }
        let router = Ipv6Addr::from(u128::from_str_radix(next_hop, 16).ok()?);
        (!router.is_unspecified()).then(|| (router, (*iface).to_owned()))
    })
}

/// The IPv6 default router with the interface index a link-local address
/// needs as its scope, or `None` when there is no IPv6 default route we
/// can read (only Linux is read today).
#[must_use]
pub fn default_gateway_v6() -> Option<(Ipv6Addr, u32)> {
    #[cfg(target_os = "linux")]
    {
        let table = std::fs::read_to_string("/proc/net/ipv6_route").ok()?;
        let (router, iface) = parse_proc_net_ipv6_route(&table)?;
        if iface.contains(['/', '.']) {
            return None;
        }
        let index = std::fs::read_to_string(format!("/sys/class/net/{iface}/ifindex")).ok()?;
        Some((router, index.trim().parse().ok()?))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The default gateway, or `None` when this host has no IPv4 default route
/// we can read.
#[must_use]
pub fn default_gateway() -> Option<Ipv4Addr> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/net/route")
            .ok()
            .as_deref()
            .and_then(parse_proc_net_route)
    }
    #[cfg(windows)]
    {
        command_stdout("route", &["print", "-4", "0.0.0.0"])
            .as_deref()
            .and_then(parse_windows_route)
    }
    #[cfg(target_os = "macos")]
    {
        command_stdout("/sbin/route", &["-n", "get", "default"])
            .as_deref()
            .and_then(parse_route_get_default)
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        None
    }
}

/// stdout of `program` when it exits successfully.
#[cfg(any(windows, target_os = "macos"))]
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A windowed parent should not flash a console for `route.exe`.
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
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

    #[test]
    fn windows_default_is_the_lowest_metric_full_route() {
        // 10.1.2.5/8 must not be sent to 10.1.2.1. The `0.0.0.0/1` row is a
        // VPN split, not the default. CGNAT (100.64.0.0/10) is a real next hop.
        let table = "\
Network Destination        Netmask          Gateway       Interface  Metric
          0.0.0.0          0.0.0.0      100.64.0.1       100.64.1.5     30
          0.0.0.0          0.0.0.0       10.0.0.1         10.1.2.5     25
          0.0.0.0        128.0.0.0         On-link          10.0.3.2      5
          0.0.0.0          0.0.0.0          0.0.0.0         10.1.2.5      1
";
        assert_eq!(parse_windows_route(table), Some(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(
            parse_windows_route(
                "0.0.0.0          0.0.0.0      100.64.0.1       100.64.1.5     20\n"
            ),
            Some(Ipv4Addr::new(100, 64, 0, 1))
        );
        assert_eq!(parse_windows_route("no routes\n"), None);
    }

    #[test]
    fn route_get_default_reads_the_gateway_line() {
        let text = "\
   route to: default
destination: default
       mask: default
    gateway: 10.0.0.1
  interface: en0
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING>
";
        assert_eq!(
            parse_route_get_default(text),
            Some(Ipv4Addr::new(10, 0, 0, 1))
        );
        assert_eq!(
            parse_route_get_default("destination: default\n    gateway: link#14\n"),
            None
        );
    }

    #[test]
    fn reads_the_ipv6_default_router() {
        // The on-link /64, a loopback host route, then the default route.
        let table = "\
fe800000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001 enp0s3
00000000000000000000000000000001 80 00000000000000000000000000000000 00 00000000000000000000000000000000 00000000 0000000a 00000000 80200001 lo
00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200 lo
00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe80000000000000021122fffe334455 00000400 00000001 00000000 00450003 wlan0
";
        let (router, iface) = parse_proc_net_ipv6_route(table).unwrap();
        assert_eq!(
            router,
            "fe80::211:22ff:fe33:4455".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(iface, "wlan0");
        // The unreachable `lo` default (no RTF_GATEWAY) is not a router.
        assert_eq!(
            parse_proc_net_ipv6_route(
                table
                    .lines()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .as_str()
            ),
            None
        );
        assert_eq!(parse_proc_net_ipv6_route("garbage\n"), None);
    }

    #[cfg(windows)]
    #[test]
    fn live_route_print_matches_the_parser() {
        let Some(text) = command_stdout("route", &["print", "-4", "0.0.0.0"]) else {
            return;
        };
        assert_eq!(default_gateway(), parse_windows_route(&text));
    }
}
