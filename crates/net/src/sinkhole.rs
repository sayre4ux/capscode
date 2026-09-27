//! Telemetry sinkhole host list .
//!
//! These hostnames are blocked at two independent, tool-config-agnostic layers:
//!
//! 1. **Capsule `/etc/hosts`** — each name is mapped to `0.0.0.0` by the capsule
//!    entrypoint, so a name-based beacon resolves to a dead address regardless of
//!    any `DO_NOT_TRACK`/telemetry env var the tool may or may not honour.
//! 2. **Gateway nftables `@sinkhole4` set** — the gateway entrypoint best-effort
//!    resolves each name (through the tunnel resolver, once up) and adds the
//!    resulting addresses to a drop set. This catches hard-coded-IP beacons that
//!    skip DNS. Because provider telemetry IPs rotate, this layer is defence in
//!    depth on top of the deterministic `/etc/hosts` layer, not a substitute.
//!
//! Neither layer reads a tool setting, so tool preferences cannot change them.

/// Known analytics / crash-reporting / editor-telemetry endpoints used by the
/// supported AI tools and their SDKs. Kept deliberately small and auditable.
pub const TELEMETRY_HOSTS: &[&str] = &[
    // Statsig — feature-flag + event pipeline embedded in several tool SDKs.
    "statsig.anthropic.com",
    "featuregates.org",
    "api.statsig.com",
    "events.statsig.com",
    "statsigapi.net",
    // Sentry — crash / error reporting.
    "sentry.io",
    "o4505.ingest.sentry.io",
    "ingest.sentry.io",
    // Editor / tool telemetry.
    "telemetry.vscode-cdn.net",
    "mobile.events.data.microsoft.com",
    "vortex.data.microsoft.com",
    "api.segment.io",
    "cdn.segment.com",
    "www.google-analytics.com",
    "analytics.google.com",
    // Chromium's own phone-home. The capsule ships a headless Chromium so the
    // agent can check its own web work; left alone it reports to the component
    // updater, the variations service, Safe Browsing and crash upload on every
    // launch. Launch flags can disable these, but the drop belongs here so it is
    // authoritative at the nft layer regardless of how the browser is invoked.
    "update.googleapis.com",
    "clientservices.googleapis.com",
    "clients2.google.com",
    "clients4.google.com",
    "safebrowsing.googleapis.com",
    "optimizationguide-pa.googleapis.com",
    "content-autofill.googleapis.com",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_is_nonempty_lowercase_and_unique() {
        assert!(!TELEMETRY_HOSTS.is_empty());
        let mut seen = std::collections::HashSet::new();
        for h in TELEMETRY_HOSTS {
            assert_eq!(*h, h.to_lowercase(), "host must be lowercase: {h}");
            assert!(
                !h.contains('/') && !h.contains(':'),
                "bare hostname only: {h}"
            );
            assert!(seen.insert(*h), "duplicate host: {h}");
        }
    }

    #[test]
    fn covers_the_named_vendors() {
        let joined = TELEMETRY_HOSTS.join(" ");
        assert!(joined.contains("statsig"));
        assert!(joined.contains("sentry"));
        assert!(joined.contains("data.microsoft.com")); // editor telemetry
    }
}
