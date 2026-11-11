//! Async HTTP(S) fetches for web seeds.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::{Attempt, Policy};
use rustls_platform_verifier::BuilderVerifierExt;

use crate::{Segment, WebSeedError, is_public};

/// Redirects followed per request.
const MAX_REDIRECTS: usize = 5;

/// Settings for a [`WebSeedClient`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WebSeedConfig {
    /// Total time allowed for one HTTP request.
    pub timeout: Duration,
    /// `User-Agent` header.
    pub user_agent: String,
    /// Let web seeds reach loopback, private, link-local and other
    /// non-public addresses (see [`is_public`]). Off by default: a torrent
    /// must not make the client probe its network. Turn on for tests and
    /// LAN web seeds.
    pub allow_private_addresses: bool,
    /// Fetch through this HTTP proxy (`http://[user:pass@]host:port`).
    pub http_proxy: Option<String>,
}

impl Default for WebSeedConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            user_agent: concat!("Dravity/", env!("CARGO_PKG_VERSION")).to_owned(),
            allow_private_addresses: false,
            http_proxy: None,
        }
    }
}

/// Downloads [`Segment`]s from web seeds.
///
/// HTTPS uses rustls with the `ring` provider and the operating system's
/// certificate verifier. Each fetched segment is one `GET` with a `Range`
/// header. Clones share one connection pool.
///
/// Responses are checked before any byte is returned:
/// - `206 Partial Content` must carry `Content-Range: bytes S-E/T` with
///   exactly the requested `S` and `E`, and `T` (unless `*`) equal to the
///   file's length; the body must be exactly `E - S + 1` bytes.
/// - `200 OK` (a server ignoring `Range`) is the whole file: accepted only
///   for a range that starts the file, whose first bytes are then the
///   requested ones; a `Content-Length` other than the file's length is
///   refused. A `200` for a range further in would mean downloading and
///   discarding everything before it, so it is refused.
///
/// Network safety (the URLs come from the torrent):
/// - Redirects (at most five) stay on the original host, keep the scheme or
///   upgrade `http` to `https` (never downgrade), and keep the port (any
///   port on an upgrade to `https` is only allowed if it is 443 or the
///   original port).
/// - Unless [`WebSeedConfig::allow_private_addresses`] is set, an IP
///   literal must be public ([`is_public`]), and host names are resolved
///   by a resolver that drops non-public addresses, so the connection goes
///   to an address that was checked (no DNS rebinding between check and
///   connect). Proxies from the environment are not used, since a proxy
///   would resolve the name itself.
#[derive(Debug, Clone)]
pub struct WebSeedClient {
    http: reqwest::Client,
    allow_private: bool,
}

/// Resolves with the system resolver and keeps only public addresses.
#[derive(Debug)]
struct PublicResolver;

/// A host whose every address is non-public.
#[derive(Debug, thiserror::Error)]
#[error("web seed host {0} resolves only to non-public addresses")]
struct NotPublic(String);

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| is_public(a.ip()))
                .collect();
            if found.is_empty() {
                return Err(NotPublic(host).into());
            }
            let addrs: Addrs = Box::new(found.into_iter());
            Ok(addrs)
        })
    }
}

/// A redirect the policy refuses.
#[derive(Debug, thiserror::Error)]
#[error("refused web seed redirect from {from} to {to}")]
struct RefusedRedirect {
    from: Url,
    to: Url,
}

/// Whether a redirect from `from` (the original URL) to `to` stays on the
/// same host without a downgrade.
fn same_origin(from: &Url, to: &Url) -> bool {
    let same_host = from.host_str().is_some() && from.host_str() == to.host_str();
    let ports = (from.port_or_known_default(), to.port_or_known_default());
    let scheme_ok = match (from.scheme(), to.scheme()) {
        ("http", "http") | ("https", "https") => ports.0 == ports.1,
        ("http", "https") => ports.1 == Some(443) || ports.0 == ports.1,
        _ => false,
    };
    same_host && scheme_ok
}

fn redirect(attempt: Attempt<'_>) -> reqwest::redirect::Action {
    let previous = attempt.previous();
    if previous.len() > MAX_REDIRECTS {
        return attempt.error("too many web seed redirects");
    }
    match previous.first() {
        Some(from) if !same_origin(from, attempt.url()) => {
            let error = RefusedRedirect {
                from: from.clone(),
                to: attempt.url().clone(),
            };
            attempt.error(error)
        }
        _ => attempt.follow(),
    }
}

impl WebSeedClient {
    /// Whether this client may connect to non-public addresses.
    #[must_use]
    pub const fn allows_private(&self) -> bool {
        self.allow_private
    }

    /// Checks `url` before any request: `http` or `https`, a host, and
    /// (unless [`WebSeedConfig::allow_private_addresses`] is set) a public
    /// IP literal. Host names are not resolved here.
    ///
    /// # Errors
    /// [`WebSeedError::InvalidUrl`], [`WebSeedError::UnsupportedScheme`], or
    /// [`WebSeedError::ForbiddenAddress`].
    pub fn validate_url(&self, url: &str) -> Result<Url, WebSeedError> {
        let parsed = Url::parse(url).map_err(|_| WebSeedError::InvalidUrl(url.into()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(WebSeedError::UnsupportedScheme(parsed.scheme().to_owned()));
        }
        match parsed.host() {
            Some(url::Host::Ipv4(ip)) if !self.allow_private && !is_public(IpAddr::V4(ip)) => {
                Err(WebSeedError::ForbiddenAddress(IpAddr::V4(ip)))
            }
            Some(url::Host::Ipv6(ip)) if !self.allow_private && !is_public(IpAddr::V6(ip)) => {
                Err(WebSeedError::ForbiddenAddress(IpAddr::V6(ip)))
            }
            Some(_) => Ok(parsed),
            None => Err(WebSeedError::InvalidUrl(url.into())),
        }
    }

    /// Builds a client.
    ///
    /// # Errors
    /// [`WebSeedError::Tls`] or [`WebSeedError::Http`] if the TLS or HTTP
    /// stack cannot be initialised.
    pub fn new(config: &WebSeedConfig) -> Result<Self, WebSeedError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_platform_verifier()?
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut builder = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .timeout(config.timeout)
            .user_agent(config.user_agent.clone())
            .redirect(Policy::custom(redirect));
        builder = match &config.http_proxy {
            Some(url) => builder.proxy(reqwest::Proxy::all(url.as_str())?),
            None => builder.no_proxy(),
        };
        if !config.allow_private_addresses {
            builder = builder.dns_resolver(Arc::new(PublicResolver));
        }
        Ok(Self {
            http: builder.build()?,
            allow_private: config.allow_private_addresses,
        })
    }

    /// Fetches `segments` and returns their bytes, concatenated in order.
    ///
    /// # Errors
    /// Any [`WebSeedError`]: an unsupported URL, a transport failure or
    /// timeout, an unexpected status, or a body of the wrong length.
    pub async fn fetch(&self, segments: &[Segment]) -> Result<Vec<u8>, WebSeedError> {
        let total: u64 = segments.iter().map(Segment::len).sum();
        let mut out = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
        for segment in segments {
            match segment {
                Segment::Zeros { length } => {
                    let length = usize::try_from(*length).map_err(|_| too_long(*length))?;
                    out.resize(out.len() + length, 0);
                }
                Segment::Fetch {
                    url,
                    offset,
                    length,
                    file_length,
                } => {
                    let range = Wanted {
                        offset: *offset,
                        length: *length,
                        file_length: *file_length,
                    };
                    self.fetch_one(url, range, &mut out).await?;
                }
            }
        }
        Ok(out)
    }

    async fn fetch_one(
        &self,
        url: &str,
        wanted: Wanted,
        out: &mut Vec<u8>,
    ) -> Result<(), WebSeedError> {
        let Wanted {
            offset,
            length,
            file_length,
        } = wanted;
        let parsed = self.validate_url(url)?;
        if length == 0 {
            return Ok(());
        }
        let last = offset
            .checked_add(length - 1)
            .filter(|&last| last < file_length)
            .ok_or(WebSeedError::OutOfRange {
                start: offset,
                end: offset.saturating_add(length),
            })?;
        let mut response = self
            .http
            .get(parsed)
            .header(reqwest::header::RANGE, format!("bytes={offset}-{last}"))
            .send()
            .await?;
        let status = response.status().as_u16();
        let exact = match status {
            206 => {
                let header = response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                    .unwrap_or_default();
                let matches = parse_content_range(&header).is_some_and(|(s, e, total)| {
                    s == offset && e == last && total.is_none_or(|t| t == file_length)
                });
                if !matches {
                    return Err(WebSeedError::ContentRange {
                        got: header,
                        start: offset,
                        end: last,
                    });
                }
                true
            }
            // The whole file: its first bytes are the range when it starts
            // the file.
            200 if offset == 0 => {
                if let Some(got) = response.content_length()
                    && got != file_length
                {
                    return Err(WebSeedError::FileLength {
                        expected: file_length,
                        got,
                    });
                }
                // Asking for the whole file: nothing may follow.
                length == file_length
            }
            200 => return Err(WebSeedError::RangeIgnored),
            other => return Err(WebSeedError::HttpStatus(other)),
        };
        let mut got: u64 = 0;
        let mut chunks = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            let n = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            let take = n.min(length - got);
            got += take;
            if take < n && exact {
                // More than the range asked for: not a trustworthy server.
                return Err(WebSeedError::BodyLength {
                    expected: length,
                    got: length + 1,
                });
            }
            let take = usize::try_from(take).map_err(|_| too_long(take))?;
            chunks.push(chunk.slice(..take));
            if got == length && !exact {
                break;
            }
        }
        if got != length {
            return Err(WebSeedError::BodyLength {
                expected: length,
                got,
            });
        }
        // Only a complete, checked range reaches the caller.
        for chunk in chunks {
            out.extend_from_slice(&chunk);
        }
        Ok(())
    }
}

/// The part of a file a [`Segment::Fetch`] asks for.
#[derive(Debug, Clone, Copy)]
struct Wanted {
    offset: u64,
    length: u64,
    file_length: u64,
}

/// Parses `Content-Range: bytes first-last/total` (RFC 9110 §14.4), with
/// `total` possibly `*`. Returns `(first, last, total)`; `None` for anything
/// else, including the `bytes */total` form of a `416`.
fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let (unit, rest) = value.trim().split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, total) = rest.trim().split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let digits = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    let (first, last) = (digits(first)?, digits(last)?);
    let total = match total {
        "*" => None,
        t => Some(digits(t)?),
    };
    (first <= last && total.is_none_or(|t| last < t)).then_some((first, last, total))
}

fn too_long(length: u64) -> WebSeedError {
    WebSeedError::BodyLength {
        expected: length,
        got: 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(
            parse_content_range("bytes 0-99/1000"),
            Some((0, 99, Some(1000)))
        );
        assert_eq!(parse_content_range("Bytes 5-5/*"), Some((5, 5, None)));
        for bad in [
            "",
            "bytes */1000",
            "bytes 9-1/1000",
            "bytes 0-1000/1000",
            "bytes -1-5/10",
            "bytes 0-+5/10",
            "items 0-1/10",
            "bytes 0-1",
            "bytes 1-2/x",
            "bytes 99999999999999999999-1/2",
        ] {
            assert_eq!(parse_content_range(bad), None, "{bad}");
        }
    }

    #[test]
    fn private_literals_are_refused_unless_allowed() {
        let strict = WebSeedClient::new(&WebSeedConfig::default()).unwrap();
        assert!(matches!(
            strict.validate_url("http://169.254.169.254/latest"),
            Err(WebSeedError::ForbiddenAddress(_))
        ));
        assert!(matches!(
            strict.validate_url("http://[::1]/x"),
            Err(WebSeedError::ForbiddenAddress(_))
        ));
        assert!(strict.validate_url("http://93.184.215.14/x").is_ok());
        assert!(strict.validate_url("https://cdn.example/a").is_ok());
        assert!(matches!(
            strict.validate_url("ftp://cdn.example/a"),
            Err(WebSeedError::UnsupportedScheme(_))
        ));
        let open = WebSeedClient::new(&WebSeedConfig {
            allow_private_addresses: true,
            ..WebSeedConfig::default()
        })
        .unwrap();
        assert!(open.validate_url("http://127.0.0.1/x").is_ok());
        assert!(open.allows_private());
    }

    #[test]
    fn redirects_stay_on_the_original_host() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert!(same_origin(
            &u("http://cdn.example/a"),
            &u("http://cdn.example/b")
        ));
        assert!(same_origin(
            &u("http://cdn.example/a"),
            &u("https://cdn.example/a")
        ));
        assert!(same_origin(
            &u("http://cdn.example:8080/a"),
            &u("https://cdn.example:8080/a")
        ));
        assert!(!same_origin(
            &u("https://cdn.example/a"),
            &u("http://cdn.example/a")
        ));
        assert!(!same_origin(
            &u("http://cdn.example/a"),
            &u("http://other.example/a")
        ));
        assert!(!same_origin(
            &u("http://cdn.example/a"),
            &u("http://cdn.example:8080/a")
        ));
        assert!(!same_origin(
            &u("http://cdn.example/a"),
            &u("http://169.254.169.254/")
        ));
        assert!(!same_origin(
            &u("http://cdn.example/a"),
            &u("ftp://cdn.example/a")
        ));
    }
}
