//! Server-side request forgery (SSRF) checks for tracker URLs.
//!
//! An announce URL comes from a `.torrent` or a magnet's `tr=` and is
//! attacker-controlled. The rules follow libtorrent's `ssrf_mitigation`:
//!
//! * A tracker on `localhost` or a loopback address must use a path ending
//!   in `/announce`, so a torrent cannot make the client `GET` arbitrary
//!   local services (an admin page, a local API) with a tracker query
//!   string appended.
//! * A redirect may only go to `http` or `https`, never from `https` down
//!   to `http`, and at most [`MAX_REDIRECTS`] times.
//! * A redirect from a tracker that is not itself local must not land on a
//!   non-public address: neither a literal (`http://10.0.0.1/…`,
//!   `http://localhost/…`) nor a host name that resolves to one (enforced by
//!   the client's resolver, which this module cannot see).
//!
//! LAN trackers named directly by the torrent keep working: only the
//! *redirect* from a public tracker into the LAN is refused.
//!
//! These are pure functions over [`Url`]s; the client applies them.

use std::net::IpAddr;

use dravity_core::net::is_public;
use url::{Host, Url};

/// Most redirects followed for one announce.
pub const MAX_REDIRECTS: usize = 5;

/// Why a tracker URL or redirect is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Blocked {
    /// A loopback tracker whose path does not end in `/announce`.
    #[error("local tracker URL must end in /announce: {0}")]
    LocalPath(String),
    /// A redirect to a scheme other than `http` or `https`.
    #[error("redirect to unsupported scheme: {0}")]
    Scheme(String),
    /// A redirect from `https` to `http`.
    #[error("redirect downgrades https to http: {0}")]
    Downgrade(String),
    /// A redirect from a non-local tracker to a non-public address.
    #[error("redirect from a public tracker to a non-public address: {0}")]
    PrivateTarget(String),
    /// More than [`MAX_REDIRECTS`] redirects.
    #[error("too many tracker redirects")]
    TooManyRedirects,
}

/// Whether the URL's host is `localhost` (or a `*.localhost` name).
fn is_localhost_name(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost")
}

/// The IP address of an IP-literal host.
fn literal(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        Host::Domain(_) => None,
    }
}

/// Whether the URL points at the local machine: `localhost` or a loopback
/// literal.
#[must_use]
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => is_localhost_name(name),
        Some(_) => literal(url).is_some_and(|ip| match ip {
            IpAddr::V4(v4) => v4.is_loopback(),
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
            }
        }),
        None => false,
    }
}

/// Whether the URL is known to point at a non-public address: `localhost`
/// or a non-public IP literal. Host names are not resolved.
#[must_use]
pub fn is_non_public(url: &Url) -> bool {
    if is_loopback(url) {
        return true;
    }
    literal(url).is_some_and(|ip| !is_public(ip))
}

/// Checks an announce URL before the first request.
///
/// # Errors
/// [`Blocked::LocalPath`] for a loopback tracker whose path does not end in
/// `/announce`.
pub fn check_announce(url: &Url) -> Result<(), Blocked> {
    if is_loopback(url) && !url.path().ends_with("/announce") {
        return Err(Blocked::LocalPath(url.to_string()));
    }
    Ok(())
}

/// Checks one redirect hop. `origin` is the announce URL the torrent named,
/// `from` the URL that answered with the redirect, `to` its target and
/// `hops` how many redirects were already followed.
///
/// # Errors
/// The [`Blocked`] reason.
pub fn check_redirect(origin: &Url, from: &Url, to: &Url, hops: usize) -> Result<(), Blocked> {
    if hops >= MAX_REDIRECTS {
        return Err(Blocked::TooManyRedirects);
    }
    match (from.scheme(), to.scheme()) {
        (_, "http" | "https") if !(from.scheme() == "https" && to.scheme() == "http") => {}
        (_, "http" | "https") => return Err(Blocked::Downgrade(to.to_string())),
        _ => return Err(Blocked::Scheme(to.to_string())),
    }
    if !is_non_public(origin) && is_non_public(to) {
        return Err(Blocked::PrivateTarget(to.to_string()));
    }
    check_announce(to)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn local_trackers_need_an_announce_path() {
        for ok in [
            "http://127.0.0.1:6969/announce",
            "http://localhost/x/announce",
            "http://[::1]:80/announce",
            "http://10.0.0.5/admin",
            "https://tracker.example/anything",
        ] {
            assert_eq!(check_announce(&u(ok)), Ok(()), "{ok}");
        }
        for bad in [
            "http://127.0.0.1:8080/admin?x=1",
            "http://LOCALHOST./api",
            "http://foo.localhost/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(
                matches!(check_announce(&u(bad)), Err(Blocked::LocalPath(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn redirects_from_public_trackers_stay_public() {
        let origin = u("http://tracker.example/announce");
        let ok = u("https://other.example/announce");
        assert_eq!(check_redirect(&origin, &origin, &ok, 0), Ok(()));
        for bad in [
            "http://192.168.1.1/announce",
            "http://169.254.169.254/latest/meta-data",
            "http://localhost/announce",
            "http://[fd00::1]/announce",
        ] {
            assert!(
                matches!(
                    check_redirect(&origin, &origin, &u(bad), 0),
                    Err(Blocked::PrivateTarget(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn lan_trackers_may_redirect_within_the_lan() {
        let origin = u("http://10.0.0.2/announce");
        let to = u("http://10.0.0.3/announce");
        assert_eq!(check_redirect(&origin, &origin, &to, 1), Ok(()));
    }

    #[test]
    fn scheme_downgrade_and_hop_limit() {
        let https = u("https://t.example/announce");
        assert!(matches!(
            check_redirect(&https, &https, &u("http://t.example/announce"), 0),
            Err(Blocked::Downgrade(_))
        ));
        assert!(matches!(
            check_redirect(&https, &https, &u("ftp://t.example/announce"), 0),
            Err(Blocked::Scheme(_))
        ));
        assert_eq!(
            check_redirect(&https, &https, &https, MAX_REDIRECTS),
            Err(Blocked::TooManyRedirects)
        );
    }
}
