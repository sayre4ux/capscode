//! Dynamic model discovery for providers that publish a model catalog.
//!
//! The registry describes providers statically, which is right for vendors with
//! a handful of fixed model ids. OpenRouter is an aggregator: its catalog is
//! hundreds of ids that change weekly, so a static `default_model` reduced the
//! generated tool config (`~/.omp/agent/models.yml`) to a single entry.
//!
//! This module asks the provider for its catalog **on the host**, before the
//! capsule exists, and folds the result into `ProviderSpec::models`. The real
//! credential is handed to `curl` through a `--config` file on stdin — never
//! argv, never an environment variable — the same shape `init`'s credential
//! probe uses, so the key stays out of `ps`, the capsule, the generated config,
//! and every error path. Discovery is best-effort: any failure leaves the spec
//! untouched and the session launches on the static registry data.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use capscode_core::{AuthKind, AuthScheme, ProviderCredential, ProviderSpec};
use secrecy::ExposeSecret;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

use crate::init::well_formed_key;
use crate::ui::note;

/// Hosts whose `/models` endpoint CapsCode knows how to read. Gating on the
/// host rather than the registry id keeps a renamed or user-overlaid OpenRouter
/// entry working, and keeps every other provider on the static path.
const DISCOVERY_HOSTS: [&str; 1] = ["openrouter.ai"];

/// Refuse a catalog that is implausibly large rather than writing a multi-megabyte
/// config into the capsule. OpenRouter publishes ~500 ids today.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

const REQUEST_TIMEOUT_SECS: u64 = 10;

/// Replaces `spec.models` with the provider's live catalog when the provider
/// supports discovery. Best-effort: on any failure the spec is left exactly as
/// the registry defined it.
pub(crate) async fn augment_provider_models(
    spec: &mut ProviderSpec,
    credential: Option<&ProviderCredential>,
) {
    let Some(url) = discovery_url(spec) else {
        return;
    };
    let Some(credential) = credential else {
        return;
    };
    let body = match fetch_catalog(&url, spec.auth, credential).await {
        Ok(body) => body,
        Err(error) => {
            // `fetch_catalog` never puts the credential in its errors, so this
            // is safe to print; it still goes through the redacting `note`.
            note(format!(
                "{}: model discovery failed ({error}); using the registry default model",
                spec.id
            ));
            return;
        }
    };
    let discovered = match parse_model_ids(&body) {
        Ok(ids) if !ids.is_empty() => ids,
        Ok(_) => {
            note(format!(
                "{}: model discovery returned no usable models; using the registry default model",
                spec.id
            ));
            return;
        }
        Err(error) => {
            note(format!(
                "{}: model discovery response was unreadable ({error}); \
                 using the registry default model",
                spec.id
            ));
            return;
        }
    };
    let merged = merge_model_ids(spec, discovered);
    note(format!("{}: discovered {} models", spec.id, merged.len()));
    spec.models = merged;
}

/// The catalog endpoint for a discovery-capable provider, or `None` when the
/// provider is served entirely from the static registry.
fn discovery_url(spec: &ProviderSpec) -> Option<String> {
    let base = spec.base_url.trim_end_matches('/');
    let host = base.strip_prefix("https://")?;
    let host = host.split('/').next()?;
    // Strip an optional port; userinfo is already rejected at registry load.
    let host = host.split(':').next()?;
    if !DISCOVERY_HOSTS
        .iter()
        .any(|known| host.eq_ignore_ascii_case(known))
    {
        return None;
    }
    Some(format!("{base}/models"))
}

/// Model ids from an OpenAI-style `{"data": [{"id": ...}, ...]}` catalog.
/// Ids that could not appear in a real catalog are dropped rather than failing
/// the whole response — an aggregator adding one odd entry should not cost the
/// user the other several hundred.
fn parse_model_ids(body: &str) -> Result<Vec<String>> {
    let document: serde_json::Value =
        serde_json::from_str(body).context("response was not JSON")?;
    let data = document
        .get("data")
        .and_then(serde_json::Value::as_array)
        .context("response has no 'data' array")?;
    Ok(data
        .iter()
        .filter_map(|entry| entry.get("id").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|id| well_formed_model_id(id))
        .map(str::to_string)
        .collect())
}

/// A model id has to be safe to embed in a generated YAML/JSON config and to
/// pass as a `--model` argument, so the accepted alphabet is deliberately narrow.
fn well_formed_model_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':' | b'@')
        })
}

/// The provider's `default_model` first, then its static registry models, then
/// everything discovered — deduplicated, order preserved. `ProviderSpec::model_ids`
/// re-prepends `default_model`, so the launch model can never fall out of the
/// catalog even if the provider stops advertising it.
fn merge_model_ids(spec: &ProviderSpec, discovered: Vec<String>) -> Vec<String> {
    let mut ids: Vec<String> = Vec::with_capacity(discovered.len() + spec.models.len() + 1);
    ids.extend(spec.default_model.iter().cloned());
    ids.extend(spec.models.iter().cloned());
    ids.extend(discovered);
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    ids
}

/// Fixed `curl` arguments. The credential is *not* here by construction: it is
/// written to the config file on stdin, so nothing secret reaches argv.
fn catalog_curl_args(url: &str) -> Vec<String> {
    [
        "--disable",
        "--config",
        "-",
        "--silent",
        "--show-error",
        "--fail",
        "--proto",
        "=https",
        // No --location: a redirect must not be able to steer a credentialed
        // request at a host the registry never named.
        "--max-time",
        &REQUEST_TIMEOUT_SECS.to_string(),
        "--max-filesize",
        &MAX_RESPONSE_BYTES.to_string(),
        "--header",
        "accept: application/json",
        url,
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect()
}

/// The `curl` config carrying the credential. This is the only value that ever
/// holds the secret, and it is zeroized on drop.
fn catalog_curl_config(
    auth: AuthScheme,
    credential: &ProviderCredential,
) -> Result<Zeroizing<String>> {
    let secret = credential.secret.expose_secret();
    if !well_formed_key(secret) {
        // Refusing here is what keeps the secret out of a shell-quoting bug: a
        // key with a quote or newline could otherwise break out of the config line.
        bail!("credential is not safe to pass to the discovery client");
    }
    let (header, prefix) = if credential.kind == AuthKind::OAuth || auth == AuthScheme::Bearer {
        ("Authorization", "Bearer ")
    } else {
        ("x-api-key", "")
    };
    Ok(Zeroizing::new(format!(
        "header = \"{header}: {prefix}{secret}\"\n"
    )))
}

async fn fetch_catalog(
    url: &str,
    auth: AuthScheme,
    credential: &ProviderCredential,
) -> Result<String> {
    if !url.starts_with("https://") {
        bail!("model discovery requires an HTTPS provider URL");
    }
    let config = catalog_curl_config(auth, credential)?;

    let mut command = tokio::process::Command::new("curl");
    command
        .args(catalog_curl_args(url))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // curl's stderr can echo the request it built; drop it rather than risk
        // surfacing a header in a log line.
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().context("starting model discovery")?;
    let mut stdin = child
        .stdin
        .take()
        .context("opening model discovery input")?;
    stdin
        .write_all(config.as_bytes())
        .await
        .context("sending credential to model discovery")?;
    drop(stdin);
    let output = tokio::time::timeout(
        Duration::from_secs(REQUEST_TIMEOUT_SECS + 2),
        child.wait_with_output(),
    )
    .await
    .context("model discovery timed out")??;
    if !output.status.success() {
        bail!("provider did not return a model catalog");
    }
    if output.stdout.len() > MAX_RESPONSE_BYTES {
        bail!("model catalog was larger than {MAX_RESPONSE_BYTES} bytes");
    }
    String::from_utf8(output.stdout).context("model catalog was not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use capscode_core::{load_bundled_provider_registry, ApiStyle};
    use secrecy::SecretString;

    fn openrouter() -> ProviderSpec {
        load_bundled_provider_registry()
            .unwrap()
            .into_iter()
            .find(|spec| spec.id == "openrouter")
            .unwrap()
    }

    #[test]
    fn openrouter_is_the_only_bundled_provider_with_discovery() {
        let discoverable: Vec<String> = load_bundled_provider_registry()
            .unwrap()
            .into_iter()
            .filter(|spec| discovery_url(spec).is_some())
            .map(|spec| spec.id)
            .collect();
        assert_eq!(discoverable, ["openrouter"]);
        assert_eq!(
            discovery_url(&openrouter()).as_deref(),
            Some("https://openrouter.ai/api/v1/models")
        );
    }

    #[test]
    fn discovery_is_host_scoped_not_substring_matched() {
        let mut spec = openrouter();
        spec.base_url = "https://openrouter.ai.evil.example/api/v1".to_string();
        assert_eq!(discovery_url(&spec), None);
        spec.base_url = "http://openrouter.ai/api/v1".to_string();
        assert_eq!(discovery_url(&spec), None);
        spec.base_url = "https://openrouter.ai:8443/api/v1/".to_string();
        assert_eq!(
            discovery_url(&spec).as_deref(),
            Some("https://openrouter.ai:8443/api/v1/models")
        );
    }

    #[test]
    fn parses_ids_and_drops_unusable_entries() {
        let body = r#"{
            "data": [
                {"id": "moonshotai/kimi-k2.6", "name": "Kimi"},
                {"id": "  openai/gpt-5.5  "},
                {"id": ""},
                {"id": "bad id with spaces"},
                {"id": "quote\"injection"},
                {"name": "no id at all"},
                {"id": 42},
                {"id": "anthropic/claude-opus-4-8"}
            ]
        }"#;
        assert_eq!(
            parse_model_ids(body).unwrap(),
            [
                "moonshotai/kimi-k2.6",
                "openai/gpt-5.5",
                "anthropic/claude-opus-4-8"
            ]
        );
    }

    #[test]
    fn rejects_responses_that_are_not_a_model_catalog() {
        assert!(parse_model_ids("not json").is_err());
        assert!(parse_model_ids(r#"{"error":{"message":"nope"}}"#).is_err());
        assert!(parse_model_ids(r#"{"data":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn merge_keeps_the_default_model_first_and_deduplicates() {
        let mut spec = openrouter();
        spec.models = vec!["static/one".to_string(), "moonshotai/kimi-k2.6".to_string()];
        let merged = merge_model_ids(
            &spec,
            vec![
                "openai/gpt-5.5".to_string(),
                "moonshotai/kimi-k2.6".to_string(),
                "static/one".to_string(),
                "openai/gpt-5.5".to_string(),
                "z-ai/glm-5.2".to_string(),
            ],
        );
        assert_eq!(
            merged,
            [
                "moonshotai/kimi-k2.6",
                "static/one",
                "openai/gpt-5.5",
                "z-ai/glm-5.2"
            ]
        );
        spec.models = merged;
        // model_ids() is what the generated tool config consumes.
        assert_eq!(spec.model_ids()[0], "moonshotai/kimi-k2.6");
        assert_eq!(spec.model_ids().len(), 4);
    }

    #[tokio::test]
    async fn discovery_leaves_non_discovery_providers_untouched() {
        let mut spec = load_bundled_provider_registry()
            .unwrap()
            .into_iter()
            .find(|spec| spec.id == "anthropic")
            .unwrap();
        let before = spec.clone();
        augment_provider_models(
            &mut spec,
            Some(&ProviderCredential::api_key(SecretString::from(
                "sk-test-0123456789",
            ))),
        )
        .await;
        assert_eq!(spec, before);
    }

    #[tokio::test]
    async fn discovery_falls_back_to_the_registry_when_the_fetch_fails() {
        let mut spec = openrouter();
        // A malformed key never reaches the network: `catalog_curl_config`
        // refuses it, so this exercises the fallback without a live request.
        let before = spec.clone();
        augment_provider_models(
            &mut spec,
            Some(&ProviderCredential::api_key(SecretString::from(
                "has spaces and \" quotes",
            ))),
        )
        .await;
        assert_eq!(spec, before);
        assert_eq!(spec.model_ids(), ["moonshotai/kimi-k2.6"]);

        // No credential is also a fallback, not a failure.
        let mut spec = openrouter();
        augment_provider_models(&mut spec, None).await;
        assert_eq!(spec.model_ids(), ["moonshotai/kimi-k2.6"]);
    }

    #[test]
    fn the_secret_never_reaches_argv_or_errors() {
        let secret = "sk-or-v1-supersecretvalue";
        let credential = ProviderCredential::api_key(SecretString::from(secret));
        let args = catalog_curl_args("https://openrouter.ai/api/v1/models");
        assert!(
            args.iter().all(|arg| !arg.contains(secret)),
            "credential leaked into curl argv: {args:?}"
        );
        assert!(args.iter().any(|arg| arg == "--disable"));
        assert!(args.windows(2).any(|pair| pair == ["--config", "-"]));

        let config = catalog_curl_config(AuthScheme::Bearer, &credential).unwrap();
        assert_eq!(
            config.as_str(),
            format!("header = \"Authorization: Bearer {secret}\"\n")
        );
        // Exactly one header line: a credential can never smuggle a second one.
        assert_eq!(config.lines().count(), 1);

        let rejected = catalog_curl_config(
            AuthScheme::Bearer,
            &ProviderCredential::api_key(SecretString::from("bad\"key\nheader: x")),
        )
        .unwrap_err()
        .to_string();
        assert!(!rejected.contains("bad\""), "error echoed the credential");
        assert!(rejected.contains("not safe to pass"));
    }

    #[test]
    fn x_api_key_providers_use_their_own_header() {
        let credential = ProviderCredential::api_key(SecretString::from("sk-test-0123456789"));
        let config = catalog_curl_config(AuthScheme::XApiKey, &credential).unwrap();
        assert!(config.starts_with("header = \"x-api-key: sk-"));
        assert_eq!(openrouter().api_style, ApiStyle::OpenAI);
    }
}
