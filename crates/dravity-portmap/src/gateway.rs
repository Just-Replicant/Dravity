//! Finding the default gateway for NAT-PMP.
//!
//! The NAT-PMP server is the next hop of the IPv4 default route: Linux reads
//! `/proc/net/route`, Windows parses `route print -4`, and macOS parses
//! `route -n get default`. That hop is used as-is, including on CGNAT
//! (100.64.0.0/10), which [`Ipv4Addr::is_private`](std::net::Ipv4Addr::is_private)
//! does not treat as local. Guessing the `.1` address of the local `/24`
//! sends a host such as 10.1.2.5/8 to 10.1.2.1. When the route cannot be
//! read, NAT-PMP is skipped; UPnP can still find a gateway with SSDP.

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

    #[cfg(windows)]
    #[test]
    fn live_route_print_matches_the_parser() {
        let Some(text) = command_stdout("route", &["print", "-4", "0.0.0.0"]) else {
            return;
        };
        assert_eq!(default_gateway(), parse_windows_route(&text));
    }
}
