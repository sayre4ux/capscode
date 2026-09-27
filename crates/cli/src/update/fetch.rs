//! HTTPS fetches for the update channel: an allowlist of GitHub hosts, manual
//! redirect handling so every hop is checked, and hard response-size caps.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Hosts the channel may be fetched from. `raw.githubusercontent.com` serves
/// `channel.json`; `github.com` serves release assets and answers with a redirect
/// to GitHub's asset storage. GitHub has served those redirects from
/// `objects.githubusercontent.com` and, since 2025, `release-assets.githubusercontent.com`.
pub(crate) const ALLOWED_HOSTS: [&str; 4] = [
    "github.com",
    "raw.githubusercontent.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const MAX_REDIRECTS: usize = 5;

/// One HTTP response, before redirects are followed.
pub(crate) struct RawResponse {
    pub status: u16,
    pub location: Option<String>,
    pub content_length: Option<u64>,
    pub body: Box<dyn Read + Send>,
}

/// Sends a single GET without following redirects.
pub(crate) trait Transport: Send + Sync {
    fn get(&self, url: &str) -> Result<RawResponse>;
}

/// Fetches a bounded body from an allowlisted HTTPS URL.
pub(crate) trait Fetch: Send + Sync {
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>>;
}

pub(crate) struct Fetcher<T> {
    transport: T,
}

impl<T: Transport> Fetcher<T> {
    pub(crate) fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T: Transport> Fetch for Fetcher<T> {
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>> {
        let mut current = checked_url(url)?;
        for _ in 0..=MAX_REDIRECTS {
            let response = self.transport.get(&current)?;
            match response.status {
                200 => {
                    if response
                        .content_length
                        .is_some_and(|length| length > limit as u64)
                    {
                        bail!(
                            "{} is larger than the {limit}-byte limit",
                            host_of(&current)
                        );
                    }
                    return read_capped(response.body, limit)
                        .with_context(|| format!("reading {}", host_of(&current)));
                }
                301 | 302 | 303 | 307 | 308 => {
                    let location = response.location.with_context(|| {
                        format!("{} sent a redirect without a location", host_of(&current))
                    })?;
                    current = redirect_target(&current, &location)?;
                }
                status => bail!("{} answered HTTP {status}", host_of(&current)),
            }
        }
        bail!("too many redirects while fetching {}", host_of(url))
    }
}

/// Accept only `https://<allowlisted host>/<path>` with no credentials or custom port.
pub(crate) fn checked_url(url: &str) -> Result<String> {
    if url.len() > 4096 || !url.bytes().all(|byte| byte.is_ascii_graphic()) {
        bail!("refusing an update URL with unexpected characters");
    }
    let uri: ureq::http::Uri = url
        .parse()
        .context("refusing an update URL that does not parse")?;
    if uri.scheme_str() != Some("https") {
        bail!("refusing a non-HTTPS update URL");
    }
    let authority = uri
        .authority()
        .context("refusing an update URL without a host")?;
    if authority.as_str().contains('@') {
        bail!("refusing an update URL with embedded credentials");
    }
    if authority.port_u16().is_some_and(|port| port != 443) {
        bail!("refusing an update URL with a non-standard port");
    }
    let host = authority.host().to_ascii_lowercase();
    if !ALLOWED_HOSTS.contains(&host.as_str()) {
        bail!("refusing to fetch updates from {host}; only GitHub hosts are allowed");
    }
    Ok(url.to_owned())
}

/// Resolve a `Location` header against the current URL and check the result.
pub(crate) fn redirect_target(current: &str, location: &str) -> Result<String> {
    let next = if location.starts_with('/') && !location.starts_with("//") {
        let uri: ureq::http::Uri = current.parse().context("parsing the current update URL")?;
        let authority = uri.authority().context("current update URL has no host")?;
        format!("https://{}{location}", authority.host())
    } else {
        location.to_owned()
    };
    checked_url(&next).context("refusing an update redirect")
}

/// Read at most `limit` bytes; a longer body is an error, never a truncation.
pub(crate) fn read_capped(body: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    body.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .context("reading the response body")?;
    if bytes.len() > limit {
        bail!("the response is larger than the {limit}-byte limit");
    }
    Ok(bytes)
}

fn host_of(url: &str) -> String {
    url.parse::<ureq::http::Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_owned))
        .unwrap_or_else(|| "the update server".to_owned())
}

/// The real transport: ureq over rustls, verifying certificates against the
/// Mozilla root store, sending no headers beyond `Host` and `User-Agent`.
pub(crate) struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub(crate) fn new(total: Duration, connect: Duration) -> Self {
        // DECISION: Mozilla's roots (webpki-roots, already built for the broker) rather than
        // the OS store, so no new TLS crates enter the graph and a locally installed
        // interception root cannot vouch for GitHub. Proxies that intercept TLS fail closed.
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(ureq::tls::RootCerts::WebPki)
            .unversioned_rustls_crypto_provider(provider)
            .build();
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent(format!("capscode/{}", env!("CARGO_PKG_VERSION")))
            .accept(ureq::config::AutoHeaderValue::None)
            .accept_encoding(ureq::config::AutoHeaderValue::None)
            .timeout_global(Some(total))
            .timeout_connect(Some(connect))
            .tls_config(tls)
            .build();
        Self {
            agent: config.new_agent(),
        }
    }
}

impl Transport for UreqTransport {
    fn get(&self, url: &str) -> Result<RawResponse> {
        let response = self
            .agent
            .get(url)
            .call()
            .with_context(|| format!("contacting {}", host_of(url)))?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let location = header("location");
        let content_length = header("content-length").and_then(|value| value.parse().ok());
        let status = response.status().as_u16();
        Ok(RawResponse {
            status,
            location,
            content_length,
            body: Box::new(response.into_body().into_reader()),
        })
    }
}

/// The production fetcher with the given overall time budget.
pub(crate) fn https_fetcher(total: Duration) -> Fetcher<UreqTransport> {
    Fetcher::new(UreqTransport::new(
        total,
        total.min(Duration::from_secs(10)),
    ))
}
