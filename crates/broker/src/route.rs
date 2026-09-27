//! Route representation, header normalization, and strip/inject
//! Routes are parsed from the non-secret providers manifest
//! (see [`crate::manifest`]); this module owns only the
//! per-request header rewrite that every forwarded request goes through.

use std::collections::HashSet;

use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};

use capscode_core::BrokerError;

/// How the real provider credential is injected on the upstream request. Which
/// scheme a route uses is taken from the manifest `inject` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InjectScheme {
    /// `Authorization: Bearer <secret>` (OpenAI/Moonshot API keys, Anthropic
    /// subscription OAuth tokens, x.ai, …).
    Bearer,
    /// `x-api-key: <secret>` (Anthropic API keys and other x-api-key providers).
    XApiKey,
}

/// A resolved route from a broker virtual host to a real provider upstream,
/// built from one manifest entry. Owned (not `&'static`) because the set of
/// providers is data-driven, not compiled in.
#[derive(Clone, Debug)]
pub(crate) struct Route {
    /// The real provider API authority (host). This is what the allowlist gates
    /// and the upstream TLS connection is dialed to.
    pub authority: String,
    /// Path prefix prepended to the forwarded request path (`""`, `/anthropic`,
    /// `/v1`, …).
    pub base_path: String,
    /// The injection scheme, taken from the manifest `inject` field.
    pub inject: InjectScheme,
    /// The `keys.env` variable name whose value is this route's real credential.
    pub key_env: String,
}

/// Inbound authentication headers that must be stripped so the capsule's
/// sentinel key (or any injected credential) can never reach upstream.
const STRIPPED_AUTH_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "api-key",
    "proxy-authorization",
    "cookie",
    "anthropic-api-key",
    "openai-api-key",
];

/// Hop-by-hop headers that must not be forwarded; hyper reconstructs framing.
/// Headers named by `Connection` are removed dynamically.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

fn connection_tokens(headers: &HeaderMap) -> HashSet<HeaderName> {
    let mut tokens = HashSet::new();
    for value in headers.get_all(hyper::header::CONNECTION).iter() {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for token in value
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
                tokens.insert(name);
            }
        }
    }
    tokens
}

fn is_dropped_name(name: &HeaderName, connection_tokens: &HashSet<HeaderName>) -> bool {
    let n = name.as_str();
    STRIPPED_AUTH_HEADERS.contains(&n)
        || HOP_BY_HOP_HEADERS.contains(&n)
        || n.starts_with("proxy-")
        || connection_tokens.contains(name)
}

/// Rewrites only broker-owned and transport headers before forwarding:
/// inbound credentials, hop-by-hop and proxy headers are removed; Host is set
/// to the upstream authority; and the provider credential is injected. Every
/// other client header, including User-Agent and SDK headers, keeps its value.
///
/// This runs unconditionally for every request that is actually forwarded — the
/// only code path that reaches upstream — so normalization can never be skipped.
/// The client's `anthropic-beta` header is deliberately NOT stripped: for a
/// subscription OAuth token, Claude Code supplies its own OAuth beta value and
/// the broker must preserve it.
///
/// `credential` is `None` when the manifest lists a provider whose `key_env` had
/// no matching key in `keys.env`. Such a request is still normalized and
/// forwarded, but carries **no** credential — the upstream rejects it (401),
/// which is the intended "unroutable, fails auth downstream" behavior. No header
/// *value* is logged here.
pub(crate) fn normalize_and_inject(
    headers: &mut HeaderMap,
    route: &Route,
    credential: Option<&SecretString>,
) -> Result<(), BrokerError> {
    let connection_tokens = connection_tokens(headers);
    // Collect first, then remove — can't mutate the map mid-iteration.
    let to_remove: Vec<HeaderName> = headers
        .keys()
        .filter(|name| is_dropped_name(name, &connection_tokens) || name.as_str() == "host")
        .cloned()
        .collect();
    for name in to_remove {
        headers.remove(&name);
    }

    // Set Host to the real upstream authority.
    headers.insert(
        hyper::header::HOST,
        HeaderValue::from_str(&route.authority).map_err(|e| {
            BrokerError::Upstream(route.authority.clone(), format!("host header: {e}"))
        })?,
    );

    // Inject the real credential under the manifest's scheme, if we have one. The
    // header value is marked sensitive so any hyper-side debug rendering redacts
    // it, and it is never logged by us.
    if let Some(credential) = credential {
        let secret = credential.expose_secret();
        let (name, value_str) = match route.inject {
            InjectScheme::XApiKey => (HeaderName::from_static("x-api-key"), secret.to_string()),
            InjectScheme::Bearer => (hyper::header::AUTHORIZATION, format!("Bearer {secret}")),
        };
        let mut value = HeaderValue::from_str(&value_str).map_err(|_| {
            BrokerError::Upstream(route.authority.clone(), "invalid key bytes".to_string())
        })?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;

    fn bearer_route(authority: &str) -> Route {
        Route {
            authority: authority.to_string(),
            base_path: String::new(),
            inject: InjectScheme::Bearer,
            key_env: "SOME_KEY".to_string(),
        }
    }

    fn xapikey_route(authority: &str) -> Route {
        Route {
            authority: authority.to_string(),
            base_path: String::new(),
            inject: InjectScheme::XApiKey,
            key_env: "ANTHROPIC_API_KEY".to_string(),
        }
    }

    /// A base set of inbound headers carrying a sentinel key + fingerprints.
    fn inbound() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("sk-capscode-proxied"));
        h.insert("authorization", HeaderValue::from_static("Bearer sentinel"));
        h.insert(
            "user-agent",
            HeaderValue::from_static("claude-cli/1.2.3 node/20"),
        );
        h.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.9"));
        h.insert("x-stainless-os", HeaderValue::from_static("MacOS"));
        // Claude Code's OAuth beta header must always survive to upstream.
        h.insert(
            "anthropic-beta",
            HeaderValue::from_static("oauth-2025-04-20"),
        );
        h
    }

    #[test]
    fn xapikey_injection_strips_auth_and_preserves_client_headers() {
        let route = xapikey_route("api.anthropic.com");
        let cred = SecretString::from("sk-REAL-upstream".to_string());
        let mut h = inbound();

        normalize_and_inject(&mut h, &route, Some(&cred)).unwrap();

        // Injected real key present under x-api-key; sentinel gone.
        assert_eq!(h.get("x-api-key").unwrap(), "sk-REAL-upstream");
        assert!(h.get("x-api-key").unwrap().is_sensitive());
        // authorization was an inbound auth header → stripped, not injected.
        assert!(h.get("authorization").is_none());
        // User-Agent and SDK companion headers pass through; Host is rewritten.
        assert_eq!(h.get("user-agent").unwrap(), "claude-cli/1.2.3 node/20");
        assert_eq!(h.get("x-forwarded-for").unwrap(), "10.0.0.9");
        assert_eq!(h.get("x-stainless-os").unwrap(), "MacOS");
        assert_eq!(h.get("host").unwrap(), "api.anthropic.com");
        // anthropic-beta preserved verbatim.
        assert_eq!(h.get("anthropic-beta").unwrap(), "oauth-2025-04-20");
    }

    #[test]
    fn bearer_injection_replaces_inbound_authorization() {
        let route = bearer_route("api.x.ai");
        let cred = SecretString::from("sk-grok-real".to_string());
        let mut h = inbound();

        normalize_and_inject(&mut h, &route, Some(&cred)).unwrap();

        assert_eq!(h.get("authorization").unwrap(), "Bearer sk-grok-real");
        assert!(h.get("authorization").unwrap().is_sensitive());
        // No x-api-key on a bearer route.
        assert!(h.get("x-api-key").is_none());
        assert_eq!(h.get("host").unwrap(), "api.x.ai");
    }

    #[test]
    fn missing_credential_injects_nothing_but_still_strips_sentinel() {
        // A manifest provider whose key_env had no key: forwarded with NO
        // credential at all (upstream will 401), but the inbound sentinel is
        // still stripped and the request is still normalized.
        let route = bearer_route("api.moonshot.ai");
        let mut h = inbound();

        normalize_and_inject(&mut h, &route, None).unwrap();

        // Neither auth header carries anything — no injection happened...
        assert!(h.get("authorization").is_none());
        assert!(h.get("x-api-key").is_none());
        // ...yet normalization still ran: the original UA survives and Host is set.
        assert_eq!(h.get("user-agent").unwrap(), "claude-cli/1.2.3 node/20");
        assert_eq!(h.get("host").unwrap(), "api.moonshot.ai");
        assert_eq!(h.get("x-forwarded-for").unwrap(), "10.0.0.9");
    }

    #[test]
    fn preserves_client_headers_and_removes_only_broker_or_transport_headers() {
        let route = bearer_route("api.example.com");
        let mut incoming = HeaderMap::new();
        incoming.insert("user-agent", HeaderValue::from_static("tool/3.4 node/22"));
        incoming.insert("x-stainless-os", HeaderValue::from_static("linux"));
        incoming.insert("x-forwarded-for", HeaderValue::from_static("192.0.2.1"));
        incoming.insert("forwarded", HeaderValue::from_static("for=192.0.2.1"));
        incoming.insert("traceparent", HeaderValue::from_static("00-abc-def-01"));
        incoming.insert("content-type", HeaderValue::from_static("application/json"));
        incoming.insert("authorization", HeaderValue::from_static("Bearer sentinel"));
        incoming.insert("x-api-key", HeaderValue::from_static("sentinel"));
        incoming.insert("cookie", HeaderValue::from_static("session=sentinel"));
        incoming.insert("host", HeaderValue::from_static("tool.broker.local"));
        incoming.insert("connection", HeaderValue::from_static("x-connection-token"));
        incoming.insert("x-connection-token", HeaderValue::from_static("remove-me"));
        incoming.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        incoming.insert("proxy-custom", HeaderValue::from_static("remove-me"));
        let mut expected = incoming.clone();
        for name in [
            "authorization",
            "x-api-key",
            "cookie",
            "host",
            "connection",
            "x-connection-token",
            "keep-alive",
            "proxy-custom",
        ] {
            expected.remove(name);
        }
        expected.insert("host", HeaderValue::from_static("api.example.com"));

        normalize_and_inject(&mut incoming, &route, None).unwrap();

        assert_eq!(incoming, expected);
        assert_eq!(incoming.get("user-agent").unwrap(), "tool/3.4 node/22");
        assert_eq!(incoming.get("x-stainless-os").unwrap(), "linux");
        assert_eq!(incoming.get("x-forwarded-for").unwrap(), "192.0.2.1");
        assert_eq!(incoming.get("forwarded").unwrap(), "for=192.0.2.1");
        assert_eq!(incoming.get("traceparent").unwrap(), "00-abc-def-01");
    }
}
