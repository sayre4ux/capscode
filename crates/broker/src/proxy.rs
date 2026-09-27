//! Per-request proxy handler: route, allowlist-gate (default deny), strip +
//! inject auth, normalize, and forward with streaming preserved.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode, Uri};
use secrecy::SecretString;

use capscode_core::BrokerError;

use crate::route::{normalize_and_inject, Route};
use crate::upstream::Upstream;

/// Response body type the broker emits to the capsule side. Upstream bodies are
/// forwarded frame-by-frame; error bodies are small fixed strings.
pub(crate) type RespBody = BoxBody<Bytes, std::io::Error>;

/// Everything a request handler needs, shared across all connections. All three
/// tables are derived from the providers manifest at startup and are immutable
/// thereafter — the capsule cannot add a route, an allowlisted host, or a key.
pub(crate) struct Inner {
    /// vhost → route, parsed from `CAPSCODE_PROVIDERS`. An unknown vhost has no
    /// entry and is rejected before any upstream connection (default-deny).
    pub routes: HashMap<String, Route>,
    /// Default-deny allowlist of real upstream authorities — exactly the set of
    /// authorities named by `routes`, and nothing else.
    pub allow_hosts: HashSet<String>,
    /// Real provider credentials keyed by `key_env`. A route whose `key_env` has
    /// no entry here is forwarded without a credential (upstream 401s).
    pub creds: HashMap<String, SecretString>,
    pub upstream: Upstream,
}

fn text(status: StatusCode, msg: &'static str) -> Response<RespBody> {
    let body = Full::new(Bytes::from_static(msg.as_bytes()))
        .map_err(|never: Infallible| match never {})
        .boxed();
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain")
        .body(body)
        .expect("static error response is always valid")
}

/// Resolve the inbound virtual host from the Host header, falling back to the
/// TLS SNI captured at accept time.
fn inbound_authority(req: &Request<Incoming>, sni: Option<&str>) -> Option<String> {
    if let Some(h) = req.headers().get(hyper::header::HOST) {
        if let Ok(s) = h.to_str() {
            return Some(s.to_string());
        }
    }
    req.uri()
        .authority()
        .map(|a| a.to_string())
        .or_else(|| sni.map(|s| s.to_string()))
}

/// Decide the route and enforce the default-deny allowlist. Returns an owned copy
/// of the route plus the optional resolved credential, or a `BrokerError` — and
/// crucially performs **no** upstream connection, so a rejected request never
/// dials out. The credential is `None` when the route's `key_env` has no key; the
/// request is still forwarded (upstream rejects it), it is never dialed for an
/// un-allowlisted authority.
fn authorize<'a>(
    inner: &'a Inner,
    vhost: &str,
) -> Result<(Route, Option<&'a SecretString>), BrokerError> {
    // Strip the port, then look the vhost up in the manifest-derived table.
    // Unknown vhost ⇒ no route ⇒ HostNotAllowed (default-deny) before any dial.
    let host = vhost.split(':').next().unwrap_or(vhost);
    let route = inner
        .routes
        .get(host)
        .ok_or_else(|| BrokerError::HostNotAllowed(vhost.to_string()))?;

    // Default-deny defense-in-depth: the resolved upstream authority must be in
    // the allowlist (which is itself built only from the manifest authorities).
    if !inner.allow_hosts.contains(&route.authority) {
        return Err(BrokerError::HostNotAllowed(route.authority.clone()));
    }

    let credential = inner.creds.get(&route.key_env);
    Ok((route.clone(), credential))
}

/// Handle a single proxied request. Always resolves to an HTTP response (errors
/// become status codes); never logs a key or a header value.
pub(crate) async fn handle(
    inner: std::sync::Arc<Inner>,
    sni: Option<String>,
    req: Request<Incoming>,
) -> Result<Response<RespBody>, Infallible> {
    let vhost = match inbound_authority(&req, sni.as_deref()) {
        Some(v) => v,
        None => {
            tracing::warn!("rejected request with no Host/SNI");
            return Ok(text(StatusCode::BAD_REQUEST, "missing host"));
        }
    };

    let (route, credential) = match authorize(&inner, &vhost) {
        Ok(pair) => pair,
        Err(BrokerError::HostNotAllowed(h)) => {
            // Reject BEFORE any upstream connection is opened. Log the host only
            // (never a header value).
            tracing::warn!(host = %h, "host not allowlisted — rejected (default deny)");
            return Ok(text(StatusCode::FORBIDDEN, "host not allowed"));
        }
        Err(e) => {
            tracing::error!(error = %e, "authorization failed");
            return Ok(text(StatusCode::BAD_GATEWAY, "authorization failed"));
        }
    };

    // Rebuild the request for the upstream: rewrite the URI path/query and apply
    // the single normalize+inject step (strip credentials and transport headers,
    // preserve other client headers, rewrite Host, and inject the real key).
    let (mut parts, body) = req.into_parts();

    // Origin-form target (path + query only), with the manifest `base_path`
    // prefixed. The upstream connection is already dialed to `route.authority`,
    // and the Host header (set in normalize_and_inject) carries the authority —
    // this is what real providers expect, not absolute-form.
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let target = format!("{}{}", route.base_path, path_and_query);
    parts.uri = match target.parse::<Uri>() {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "failed to build upstream uri");
            return Ok(text(StatusCode::BAD_REQUEST, "bad request target"));
        }
    };

    if let Err(e) = normalize_and_inject(&mut parts.headers, &route, credential) {
        tracing::error!(error = %e, "header normalization failed");
        return Ok(text(StatusCode::BAD_GATEWAY, "header normalization failed"));
    }

    tracing::info!(
        authority = %route.authority,
        method = %parts.method,
        "proxying request",
    );

    let upstream_req = Request::from_parts(parts, body);
    match inner.upstream.send(&route.authority, upstream_req).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            // Forward the response streaming: map the body error type, box it,
            // never collect.
            let body = body.map_err(std::io::Error::other).boxed();
            Ok(Response::from_parts(parts, body))
        }
        Err(e) => {
            tracing::error!(error = %e, "upstream request failed");
            Ok(text(StatusCode::BAD_GATEWAY, "upstream error"))
        }
    }
}
