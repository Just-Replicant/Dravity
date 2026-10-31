//! Async HTTP(S) fetches for web seeds.

use std::sync::Arc;
use std::time::Duration;

use rustls_platform_verifier::BuilderVerifierExt;

use crate::{Segment, WebSeedError};

/// Settings for a [`WebSeedClient`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WebSeedConfig {
    /// Total time allowed for one HTTP request.
    pub timeout: Duration,
    /// `User-Agent` header.
    pub user_agent: String,
}

impl Default for WebSeedConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            user_agent: concat!("Dravity/", env!("CARGO_PKG_VERSION")).to_owned(),
        }
    }
}

/// Downloads [`Segment`]s from web seeds.
///
/// HTTPS uses rustls with the `ring` provider and the operating system's
/// certificate verifier. Each fetched segment is one `GET` with a `Range`
/// header; the body must be exactly the bytes asked for (`206 Partial
/// Content`). A `200 OK` is accepted only for a range that starts the file,
/// and only its first bytes are read. Redirects are followed (at most ten).
/// Clones share one connection pool.
#[derive(Debug, Clone)]
pub struct WebSeedClient {
    http: reqwest::Client,
}

impl WebSeedClient {
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
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .timeout(config.timeout)
            .user_agent(config.user_agent.clone())
            .build()?;
        Ok(Self { http })
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
                } => self.fetch_one(url, *offset, *length, &mut out).await?,
            }
        }
        Ok(out)
    }

    async fn fetch_one(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        out: &mut Vec<u8>,
    ) -> Result<(), WebSeedError> {
        let parsed = reqwest::Url::parse(url).map_err(|_| WebSeedError::InvalidUrl(url.into()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(WebSeedError::UnsupportedScheme(parsed.scheme().to_owned()));
        }
        if length == 0 {
            return Ok(());
        }
        let last = offset
            .checked_add(length - 1)
            .ok_or(WebSeedError::OutOfRange {
                start: offset,
                end: u64::MAX,
            })?;
        let mut response = self
            .http
            .get(parsed)
            .header(reqwest::header::RANGE, format!("bytes={offset}-{last}"))
            .send()
            .await?;
        let status = response.status().as_u16();
        let exact = match status {
            206 => true,
            // The whole file: fine when we want its start.
            200 if offset == 0 => false,
            200 => return Err(WebSeedError::RangeIgnored),
            other => return Err(WebSeedError::HttpStatus(other)),
        };
        let mut got: u64 = 0;
        while let Some(chunk) = response.chunk().await? {
            let n = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            let take = n.min(length - got);
            let take_usize = usize::try_from(take).map_err(|_| too_long(take))?;
            out.extend_from_slice(chunk.get(..take_usize).unwrap_or_default());
            got += take;
            if take < n {
                if exact {
                    // More than the range asked for: not a trustworthy server.
                    return Err(WebSeedError::BodyLength {
                        expected: length,
                        got: length + 1,
                    });
                }
                break;
            }
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
        Ok(())
    }
}

fn too_long(length: u64) -> WebSeedError {
    WebSeedError::BodyLength {
        expected: length,
        got: 0,
    }
}
