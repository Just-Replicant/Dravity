//! Server-side request forgery (SSRF) checks for tracker URLs.
//!
//! An announce URL comes from a `.torrent` or a magnet's `tr=` and is
//! attacker-controlled. The rules follow libtorrent's `ssrf_mitigation`:
//!
//! * Non-public trackers are denied unless explicitly allowed, and allowed
//!   private origins must use a final path segment beginning with `announce`.
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
    /// A non-public tracker origin was not explicitly allowed.
    #[error("non-public tracker origin is not allowed: {0}")]
    PrivateOrigin(String),
    /// An allowed private tracker does not have an announce-shaped path.
    #[error("non-public tracker path must end in an announce segment: {0}")]
    PrivatePath(String),
    /// A redirect to a scheme other than `http` or `https`.
    #[error("redirect to unsupported scheme: {0}")]
    Scheme(String),
    /// A redirect from `https` to `http`.
    #[error("redirect downgrades https to http: {0}")]
    Downgrade(String),
    /// A redirect from a non-local tracker to a non-public address.
    #[error("redirect from a public tracker to a non-public address: {0}")]
    PrivateTarget(String),
    /// A proxy would resolve a name outside the guarded resolver.
    #[error("tracker proxy resolution cannot be verified safely: {0}")]
    ProxyResolution(String),
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

/// Whether two URLs have the same scheme, host and effective port.
#[must_use]
pub fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str().map(str::to_ascii_lowercase)
            == right.host_str().map(str::to_ascii_lowercase)
        && left.port_or_known_default() == right.port_or_known_default()
}

fn has_announce_path(url: &Url) -> bool {
    url.path_segments()
        .and_then(Iterator::last)
        .is_some_and(|segment| segment.starts_with("announce"))
}

/// Checks an announce URL before the first request.
///
/// # Errors
/// [`Blocked::LocalPath`] for a loopback tracker whose path does not end in
/// `/announce`.
pub fn check_announce(url: &Url, allow_private: bool) -> Result<(), Blocked> {
    // This guard prevents HTTP request forgery. UDP tracker packets have a
    // fixed BEP 15 binary shape and no path or headers that can be turned
    // into requests to an HTTP service; LAN UDP trackers therefore remain
    // usable without weakening the HTTP policy.
    if !matches!(url.scheme(), "http" | "https") {
        return Ok(());
    }
    if is_non_public(url) && !allow_private {
        return Err(Blocked::PrivateOrigin(url.to_string()));
    }
    if allow_private && !has_announce_path(url) {
        return Err(Blocked::PrivatePath(url.to_string()));
    }
    Ok(())
}

/// Checks one redirect hop. `origin` is the announce URL the torrent named,
/// `from` the URL that answered with the redirect, `to` its target and
/// `hops` how many redirects were already followed.
///
/// # Errors
/// The [`Blocked`] reason.
pub fn check_redirect(
    origin: &Url,
    from: &Url,
    to: &Url,
    hops: usize,
    origin_allows_private: bool,
) -> Result<(), Blocked> {
    if hops >= MAX_REDIRECTS {
        return Err(Blocked::TooManyRedirects);
    }
    match (from.scheme(), to.scheme()) {
        (_, "http" | "https") if !(from.scheme() == "https" && to.scheme() == "http") => {}
        (_, "http" | "https") => return Err(Blocked::Downgrade(to.to_string())),
        _ => return Err(Blocked::Scheme(to.to_string())),
    }
    let same_origin = same_origin(origin, to);
    if origin_allows_private && !same_origin {
        return Err(Blocked::PrivateTarget(to.to_string()));
    }
    if is_non_public(to) && !(origin_allows_private && same_origin) {
        return Err(Blocked::PrivateTarget(to.to_string()));
    }
    check_announce(to, origin_allows_private && same_origin)
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
            "http://localhost/x/announce.php",
            "http://[::1]:80/announce.cgi",
        ] {
            assert_eq!(check_announce(&u(ok), true), Ok(()), "{ok}");
        }
        for bad in [
            "http://127.0.0.1:8080/admin?x=1",
            "http://LOCALHOST./api",
            "http://foo.localhost/",
            "http://[::ffff:127.0.0.1]/",
            "http://10.0.0.5/admin",
        ] {
            assert!(
                matches!(check_announce(&u(bad), true), Err(Blocked::PrivatePath(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn redirects_from_public_trackers_stay_public() {
        let origin = u("http://tracker.example/announce");
        let ok = u("https://other.example/announce");
        assert_eq!(check_redirect(&origin, &origin, &ok, 0, false), Ok(()));
        for bad in [
            "http://192.168.1.1/announce",
            "http://169.254.169.254/latest/meta-data",
            "http://localhost/announce",
            "http://[fd00::1]/announce",
        ] {
            assert!(
                matches!(
                    check_redirect(&origin, &origin, &u(bad), 0, false),
                    Err(Blocked::PrivateTarget(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn private_redirect_trust_is_exact_origin_scoped() {
        let origin = u("http://10.0.0.2/announce");
        let same = u("http://10.0.0.2/announce.php");
        assert_eq!(check_redirect(&origin, &origin, &same, 1, true), Ok(()));
        let to = u("http://10.0.0.3/announce");
        assert!(matches!(
            check_redirect(&origin, &origin, &to, 1, true),
            Err(Blocked::PrivateTarget(_))
        ));
    }

    #[test]
    fn scheme_downgrade_and_hop_limit() {
        let https = u("https://t.example/announce");
        assert!(matches!(
            check_redirect(&https, &https, &u("http://t.example/announce"), 0, false),
            Err(Blocked::Downgrade(_))
        ));
        assert!(matches!(
            check_redirect(&https, &https, &u("ftp://t.example/announce"), 0, false),
            Err(Blocked::Scheme(_))
        ));
        assert_eq!(
            check_redirect(&https, &https, &https, MAX_REDIRECTS, false),
            Err(Blocked::TooManyRedirects)
        );
    }
}
