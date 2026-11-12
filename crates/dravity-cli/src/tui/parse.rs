//! Input parsing for TUI prompts: ports, IPs, caps, peer lists and paths.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

/// Parses a listen port: `0` lets the OS pick a free port, `1..=65535` is
/// used as given. Empty or non-numeric input is refused.
pub(crate) fn port(text: &str) -> Result<u16, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("type a port (0 picks a free one, 1–65535)".to_owned());
    }
    text.parse::<u16>()
        .map_err(|_| format!("`{text}` is not a port (0–65535)"))
}

/// Parses an IP address (v4 or v6), not a host:port.
pub(crate) fn ip(text: &str) -> Result<IpAddr, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("type an IP address".to_owned());
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Ok(ip);
    }
    if text.parse::<SocketAddr>().is_ok() || text.contains(':') && !text.contains("::") {
        return Err(format!(
            "`{text}` looks like host:port; type an IP address only"
        ));
    }
    Err(format!("`{text}` is not an IP address"))
}

/// Parses a session cap: empty, `unlimited`, `none`, `off` or `-` lifts
/// the cap (`None`). A non-negative integer sets it (`Some(0)` is a cap
/// of zero, which queues or holds everything).
pub(crate) fn cap(text: &str) -> Result<Option<usize>, String> {
    let text = text.trim();
    if text.is_empty()
        || matches!(
            text.to_ascii_lowercase().as_str(),
            "unlimited" | "none" | "off" | "-"
        )
    {
        return Ok(None);
    }
    text.parse::<usize>()
        .map(Some)
        .map_err(|_| format!("`{text}` is not a count (a number, or empty to lift the cap)"))
}

/// Splits a peer list on commas or whitespace. Each entry is a `host:port`
/// (or a parsed [`SocketAddr`]); DNS happens later, when the session is
/// asked to connect. Empty input is an empty list.
pub(crate) fn peer_list(text: &str) -> Result<Vec<String>, String> {
    let mut peers = Vec::new();
    for part in text.split([',', ' ', '\t', '\n', '\r']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if !looks_like_host_port(part) {
            return Err(format!("`{part}` is not host:port"));
        }
        peers.push(part.to_owned());
    }
    Ok(peers)
}

/// Whether `s` is a [`SocketAddr`] or a `host:port` (IPv6 in brackets).
fn looks_like_host_port(s: &str) -> bool {
    if s.parse::<SocketAddr>().is_ok() {
        return true;
    }
    if let Some(rest) = s.strip_prefix('[') {
        let Some((host, port)) = rest.split_once("]:") else {
            return false;
        };
        return !host.is_empty() && is_port(port);
    }
    // A hostname or IPv4 with a trailing port; refuse a bare IPv6.
    match s.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(':') => is_port(port),
        _ => false,
    }
}

fn is_port(s: &str) -> bool {
    s.parse::<u16>().is_ok()
}

/// A filter-file path. Empty means "clear the filter". The file is read
/// when the session applies it, not here.
pub(crate) fn filter_path(text: &str) -> Result<Option<PathBuf>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let path = Path::new(text);
    if text.contains('\0') {
        return Err("the path contains a NUL byte".to_owned());
    }
    Ok(Some(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::{cap, filter_path, ip, peer_list, port};

    #[test]
    fn port_accepts_zero_and_rejects_empty_or_out_of_range() {
        assert_eq!(port("0"), Ok(0));
        assert_eq!(port(" 6881 "), Ok(6881));
        assert_eq!(port("65535"), Ok(65535));
        assert!(port("").is_err());
        assert!(port("  ").is_err());
        assert!(port("65536").is_err());
        assert!(port("-1").is_err());
        assert!(port("port").is_err());
        assert!(port("6881x").is_err());
    }

    #[test]
    fn ip_accepts_v4_and_v6_and_rejects_a_port() {
        assert_eq!(
            ip("127.0.0.1").unwrap(),
            "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(
            ip("  ::1  ").unwrap(),
            "::1".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(
            ip("2001:db8::1").unwrap(),
            "2001:db8::1".parse::<std::net::IpAddr>().unwrap()
        );
        assert!(ip("").is_err());
        assert!(ip("localhost").is_err());
        assert!(ip("127.0.0.1:6881").is_err(), "a port is not an IP");
        let err = ip("10.0.0.1:80").unwrap_err();
        assert!(err.contains("host:port"), "{err}");
        assert!(ip("999.0.0.1").is_err());
    }

    #[test]
    fn cap_treats_empty_and_words_as_unlimited_and_zero_as_a_cap() {
        assert_eq!(cap(""), Ok(None));
        assert_eq!(cap("  "), Ok(None));
        assert_eq!(cap("unlimited"), Ok(None));
        assert_eq!(cap("NONE"), Ok(None));
        assert_eq!(cap("off"), Ok(None));
        assert_eq!(cap("-"), Ok(None));
        assert_eq!(cap("0"), Ok(Some(0)));
        assert_eq!(cap("12"), Ok(Some(12)));
        assert!(cap("-1").is_err());
        assert!(cap("3.5").is_err());
        assert!(cap("many").is_err());
    }

    #[test]
    fn peer_list_splits_on_commas_or_space_and_checks_host_port() {
        assert_eq!(peer_list("").unwrap(), Vec::<String>::new());
        assert_eq!(
            peer_list("10.0.0.2:6881, example.org:80  [2001:db8::1]:1").unwrap(),
            vec![
                "10.0.0.2:6881".to_owned(),
                "example.org:80".to_owned(),
                "[2001:db8::1]:1".to_owned(),
            ]
        );
        assert!(peer_list("10.0.0.2").is_err(), "no port");
        assert!(peer_list("10.0.0.2:99999").is_err());
        assert!(
            peer_list("2001:db8::1").is_err(),
            "bare IPv6 needs brackets"
        );
        assert!(peer_list("host:port").is_err());
    }

    #[test]
    fn filter_path_empty_clears_and_a_path_is_kept() {
        assert_eq!(filter_path(""), Ok(None));
        assert_eq!(
            filter_path(" /tmp/ipfilter.dat ").unwrap().as_deref(),
            Some(std::path::Path::new("/tmp/ipfilter.dat"))
        );
    }
}
