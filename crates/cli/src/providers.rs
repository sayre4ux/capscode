use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use anyhow::{bail, Context, Result};
use capscode_core::{
    load_bundled_provider_registry, merge_provider_registries, Config, ProviderCredential,
    ProviderDefaults, ProviderSpec, Tool,
};
#[cfg(test)]
use capscode_core::{ApiStyle, AuthScheme};
use secrecy::SecretString;

#[derive(Clone)]
pub(crate) struct ProviderResolution {
    pub spec: ProviderSpec,
    pub credential: Option<ProviderCredential>,
    pub credential_source: Option<String>,
}

impl ProviderResolution {
    pub(crate) fn credential_available(&self) -> bool {
        self.credential.is_some()
    }
}

impl std::fmt::Debug for ProviderResolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderResolution")
            .field("spec", &self.spec)
            .field("credential_available", &self.credential_available())
            .field("credential_source", &self.credential_source)
            .finish()
    }
}

pub(crate) fn registry_file_path() -> PathBuf {
    Config::config_home().join("providers.yaml")
}

pub(crate) fn load_registry() -> Result<Vec<ProviderSpec>> {
    load_registry_at(&registry_file_path())
}

pub(crate) fn load_registry_at(user_path: &Path) -> Result<Vec<ProviderSpec>> {
    let merged = if !user_path.exists() {
        load_bundled_provider_registry().context("parsing bundled provider registry")?
    } else {
        let user_yaml = fs::read_to_string(user_path)
            .with_context(|| format!("reading provider registry {}", user_path.display()))?;
        merge_provider_registries(
            include_str!("../../../providers/registry.yaml"),
            Some(&user_yaml),
        )
        .with_context(|| format!("merging provider registry {}", user_path.display()))?
    };
    Ok(quarantine_invalid_base_urls(merged, |id, reason| {
        crate::ui::note(quarantine_warning(id, reason));
    }))
}

/// Drop providers whose `base_url` fails the add-time rule, keeping every other
/// entry usable so recovery commands like `provider remove` still work when a
/// legacy entry on disk violates the newer rule. Each skipped entry is reported
/// through `report` as `(id, reason)`; the production caller writes a
/// [`crate::ui::note`] warning, while tests collect warnings directly.
fn quarantine_invalid_base_urls<F>(registry: Vec<ProviderSpec>, mut report: F) -> Vec<ProviderSpec>
where
    F: FnMut(&str, &str),
{
    registry
        .into_iter()
        .filter(|spec| match base_url_rejection(&spec.base_url) {
            Some(reason) => {
                report(&spec.id, reason);
                false
            }
            None => true,
        })
        .collect()
}

/// Warning line for a quarantined provider. Extracted from the loader so tests
/// can assert it names the entry and the recovery command without capturing
/// stderr.
fn quarantine_warning(id: &str, reason: &str) -> String {
    format!(
        "skipping provider '{id}': {reason}; run `capscode provider remove {id}` and re-add it without credentials in the URL"
    )
}

pub(crate) fn append_user_provider(spec: &ProviderSpec) -> Result<()> {
    Config::ensure_config_home().context("preparing config home")?;
    append_user_provider_at(&registry_file_path(), spec)
}

pub(crate) fn append_user_provider_at(path: &Path, spec: &ProviderSpec) -> Result<()> {
    validate_user_spec(spec)?;
    let existing = if path.exists() {
        fs::read_to_string(path)
            .with_context(|| format!("reading provider registry {}", path.display()))?
    } else {
        String::new()
    };
    if user_provider_ids(&existing).iter().any(|id| id == &spec.id) {
        bail!("user provider '{}' already exists", spec.id);
    }
    if load_registry_at(path)?
        .iter()
        .any(|provider| provider.id == spec.id)
    {
        bail!(
            "provider '{}' already exists; edit providers.yaml to override a bundled provider",
            spec.id
        );
    }

    let parent = path
        .parent()
        .context("provider registry path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating config directory {}", parent.display()))?;
    let separator = if existing.is_empty() || existing.ends_with("\n\n") {
        ""
    } else if existing.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let default_model = spec
        .default_model
        .as_deref()
        .map_or_else(String::new, |model| format!("  default_model: {model}\n"));
    let rendered = format!(
        "{existing}{separator}{}:\n  api_style: {}\n  base_url: {}\n  auth: {}\n  key_env: {}\n{default_model}  experimental: {}\n",
        spec.id,
        config::api_style_name(spec.api_style),
        spec.base_url,
        config::auth_scheme_name(spec.auth),
        spec.key_env,
        spec.experimental,
    );
    fs::write(path, rendered)
        .with_context(|| format!("writing provider registry {}", path.display()))
}

pub(crate) fn remove_user_provider(id: &str) -> Result<()> {
    remove_user_provider_at(&registry_file_path(), id)
}

pub(crate) fn remove_user_provider_at(path: &Path, id: &str) -> Result<()> {
    if !path.exists() {
        bail!("provider '{id}' is not defined in the user registry");
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading provider registry {}", path.display()))?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let target = format!("{id}:");
    let Some(start) = lines.iter().position(|line| line.trim_end() == target) else {
        bail!("provider '{id}' is not defined in the user registry");
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| is_provider_header(line))
        .map_or(lines.len(), |offset| start + 1 + offset);
    let mut rendered = String::new();
    rendered.extend(lines[..start].iter().copied());
    rendered.extend(lines[end..].iter().copied());
    while rendered.contains("\n\n\n") {
        rendered = rendered.replace("\n\n\n", "\n\n");
    }
    fs::write(path, rendered)
        .with_context(|| format!("writing provider registry {}", path.display()))
}

fn user_provider_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| is_provider_header(line))
        .map(|line| line.trim_end_matches(':').to_string())
        .collect()
}

fn is_provider_header(line: &str) -> bool {
    !line.starts_with(char::is_whitespace)
        && !line.starts_with('#')
        && line.trim_end().ends_with(':')
}

/// Shared add-time base-URL rule. Returns the reason a URL is rejected, or
/// `None` when it is acceptable. Used by [`validate_user_spec`] (hard fail at
/// `provider add`) and by [`quarantine_invalid_base_urls`] (skip legacy entries
/// at registry load) so the two boundaries never drift apart. Only ever
/// rejects more than the launch-time check in `session::state::provider_route`.
fn base_url_rejection(base_url: &str) -> Option<&'static str> {
    if !base_url.starts_with("https://") {
        return Some("provider base URL must be an https:// URL");
    }
    if base_url.contains(['\n', '\r']) {
        return Some("provider base URL must be a single line");
    }
    // Userinfo (`user:secret@host`) lives in the authority, which runs from the
    // end of the scheme to the first path/query/fragment delimiter.
    let rest = &base_url["https://".len()..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return Some("provider base URL must not contain userinfo or embedded credentials ('@'); put the credential in the keystore, not the URL");
    }
    if base_url.contains('?') {
        return Some("provider base URL must not contain a query string ('?'); put the credential in the keystore, not the URL");
    }
    if base_url.contains('#') {
        return Some("provider base URL must not contain a fragment ('#'); put the credential in the keystore, not the URL");
    }
    None
}

fn validate_user_spec(spec: &ProviderSpec) -> Result<()> {
    if spec.id.is_empty()
        || !spec
            .id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("provider id must contain only lowercase ASCII letters, digits, and '-'");
    }
    if let Some(reason) = base_url_rejection(&spec.base_url) {
        bail!("{reason}");
    }
    super::validate_provider_endpoint_suffix(spec.api_style, &spec.base_url)?;
    if spec.key_env.is_empty()
        || !spec
            .key_env
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        || spec.key_env.as_bytes()[0].is_ascii_digit()
    {
        bail!("provider key env must be an uppercase environment variable name");
    }
    if spec
        .default_model
        .as_deref()
        .is_some_and(|model| model.trim().is_empty() || model.contains(['\n', '\r']))
    {
        bail!("provider default model must be a non-empty single-line model id");
    }
    Ok(())
}

pub(crate) fn resolve_provider_from<G, K, R>(
    registry: &[ProviderSpec],
    explicit_id: Option<&str>,
    tool: Tool,
    defaults: &ProviderDefaults,
    get_env: G,
    read_anthropic_keychain: K,
    read_keystore: R,
) -> Result<ProviderResolution>
where
    G: Fn(&str) -> Option<String>,
    K: FnOnce() -> Option<String>,
    R: Fn(&str) -> Option<String>,
{
    let spec = resolve_provider_spec_from(registry, explicit_id, tool, defaults)?;

    let (credential, credential_source) = if spec.id == "anthropic" {
        resolve_anthropic_credential(&spec, &get_env, read_anthropic_keychain)
    } else {
        match get_env(&spec.key_env).filter(|value| !value.is_empty()) {
            Some(value) => (
                Some(ProviderCredential::api_key(SecretString::from(value))),
                Some(format!("env:{}", spec.key_env)),
            ),
            None => match read_keystore(&spec.id).filter(|value| !value.is_empty()) {
                Some(value) => (
                    Some(ProviderCredential::api_key(SecretString::from(value))),
                    Some(format!("keystore:{}", spec.id)),
                ),
                None => (None, None),
            },
        }
    };

    Ok(ProviderResolution {
        spec,
        credential,
        credential_source,
    })
}

pub(crate) fn resolve_provider_spec_from(
    registry: &[ProviderSpec],
    explicit_id: Option<&str>,
    tool: Tool,
    defaults: &ProviderDefaults,
) -> Result<ProviderSpec> {
    let provider_id = explicit_id
        .filter(|id| !id.is_empty())
        .or_else(|| defaults.get(tool))
        .or_else(|| built_in_default(tool))
        .with_context(|| {
            format!(
                "tool '{}' has no default provider; configure a persisted [defaults] entry or pass --provider",
                tool.name()
            )
        })?;
    let spec = registry
        .iter()
        .find(|spec| spec.id.eq_ignore_ascii_case(provider_id))
        .cloned()
        .with_context(|| {
            format!(
                "unknown provider '{provider_id}'; compatible providers for '{}': {}",
                tool.name(),
                compatible_provider_ids(registry, tool)
            )
        })?;

    if !spec.supports_tool(tool) {
        // Lead with what the chosen provider *can* drive and with the providers
        // that read the same credential: picking an OpenAI-style endpoint for
        // `claude` usually means the key itself is fine and only the wire format
        // or the vendor product is wrong, which a flat list of every compatible
        // id does not say.
        let tool_name = tool.name();
        let siblings = same_credential_provider_ids(registry, tool, &spec.id, &spec.key_env)
            .map_or_else(String::new, |ids| {
                format!(
                    "\n  providers reading {} that support '{tool_name}': {ids}",
                    spec.key_env
                )
            });
        bail!(
            "provider '{}' uses {} API style and is incompatible with tool '{tool_name}'\n  '{}' works with: {}{siblings}\n  all providers for '{tool_name}': {}",
            spec.id,
            config::api_style_name(spec.api_style),
            spec.id,
            supported_tool_names(&spec),
            compatible_provider_ids(registry, tool)
        );
    }

    Ok(spec)
}

fn resolve_anthropic_credential<G, K>(
    spec: &ProviderSpec,
    get_env: &G,
    read_keychain: K,
) -> (Option<ProviderCredential>, Option<String>)
where
    G: Fn(&str) -> Option<String>,
    K: FnOnce() -> Option<String>,
{
    if let Some(value) = get_env("CLAUDE_CODE_OAUTH_TOKEN").filter(|value| !value.is_empty()) {
        return (
            Some(ProviderCredential::oauth(SecretString::from(value))),
            Some("env:CLAUDE_CODE_OAUTH_TOKEN".to_string()),
        );
    }
    if let Some(value) = get_env(&spec.key_env).filter(|value| !value.is_empty()) {
        return (
            Some(ProviderCredential::api_key(SecretString::from(value))),
            Some(format!("env:{}", spec.key_env)),
        );
    }
    match read_keychain().filter(|value| !value.is_empty()) {
        Some(value) => (
            Some(ProviderCredential::oauth(SecretString::from(value))),
            Some("keychain".to_string()),
        ),
        None => (None, None),
    }
}

pub(crate) fn built_in_default(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Claude => Some("anthropic"),
        Tool::Codex => Some("openai"),
        Tool::Opencode | Tool::Pi | Tool::Omp => None,
    }
}

fn compatible_provider_ids(registry: &[ProviderSpec], tool: Tool) -> String {
    let ids: Vec<&str> = registry
        .iter()
        .filter(|spec| spec.supports_tool(tool))
        .map(|spec| spec.id.as_str())
        .collect();
    if ids.is_empty() {
        "none".to_string()
    } else {
        ids.join(", ")
    }
}

fn supported_tool_names(spec: &ProviderSpec) -> String {
    let names: Vec<&str> = Tool::ALL
        .into_iter()
        .filter(|tool| spec.supports_tool(*tool))
        .map(Tool::name)
        .collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// Providers that support `tool` and read the same credential env var as the
/// rejected one — the alternatives reachable without a new signup, modulo the
/// vendor's own product tiers.
fn same_credential_provider_ids(
    registry: &[ProviderSpec],
    tool: Tool,
    rejected_id: &str,
    key_env: &str,
) -> Option<String> {
    let ids: Vec<&str> = registry
        .iter()
        .filter(|spec| {
            spec.id != rejected_id && spec.key_env == key_env && spec.supports_tool(tool)
        })
        .map(|spec| spec.id.as_str())
        .collect();
    (!ids.is_empty()).then(|| ids.join(", "))
}

pub(crate) fn env_value(name: &str) -> Option<String> {
    env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn absent_user_file_loads_bundled_registry() {
        let temp = tempfile::tempdir().unwrap();
        let registry = load_registry_at(&temp.path().join("providers.yaml")).unwrap();
        assert_eq!(registry.len(), 21);
    }

    #[test]
    fn user_file_overrides_a_bundled_preset() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("providers.yaml");
        fs::write(
            &path,
            "openai:\n  base_url: https://override.example/v1\n  key_env: OVERRIDE_KEY\n",
        )
        .unwrap();
        let registry = load_registry_at(&path).unwrap();
        let openai = registry.iter().find(|spec| spec.id == "openai").unwrap();
        assert_eq!(openai.base_url, "https://override.example/v1");
        assert_eq!(openai.key_env, "OVERRIDE_KEY");
        assert_eq!(openai.api_style, ApiStyle::OpenAI);
    }

    #[test]
    fn incompatible_tool_provider_pairs_list_compatible_choices() {
        let registry = load_bundled_provider_registry().unwrap();
        for (tool, provider, expected_style) in [
            (Tool::Claude, "openai", "openai"),
            (Tool::Codex, "anthropic", "anthropic"),
        ] {
            let error = resolve_provider_from(
                &registry,
                Some(provider),
                tool,
                &ProviderDefaults::default(),
                |_| None,
                || None,
                |_| None,
            )
            .unwrap_err();
            let message = error.to_string();
            assert!(message.contains("is incompatible"));
            assert!(message.contains(expected_style));
            assert!(message.contains("all providers for"));
        }
    }

    #[test]
    fn incompatible_pair_names_the_tools_and_same_key_providers_that_work() {
        let registry = load_bundled_provider_registry().unwrap();
        let message = resolve_provider_spec_from(
            &registry,
            Some("zai-api"),
            Tool::Claude,
            &ProviderDefaults::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("'zai-api' works with: codex, opencode, pi, omp"));
        assert!(message.contains(
            "providers reading GLM_API_KEY that support 'claude': zai-api-anthropic, zai-coding"
        ));
    }

    #[test]
    fn incompatible_pair_omits_the_sibling_line_without_a_shared_credential() {
        let registry = load_bundled_provider_registry().unwrap();
        let message = resolve_provider_spec_from(
            &registry,
            Some("grok"),
            Tool::Claude,
            &ProviderDefaults::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(!message.contains("providers reading"));
    }

    #[test]
    fn spec_only_resolution_needs_no_credential_source() {
        let registry = load_bundled_provider_registry().unwrap();
        let spec = resolve_provider_spec_from(
            &registry,
            Some("anthropic"),
            Tool::Claude,
            &ProviderDefaults::default(),
        )
        .unwrap();
        assert_eq!(spec.id, "anthropic");
        assert_eq!(spec.api_style, ApiStyle::Anthropic);
    }

    #[test]
    fn experimental_flag_and_missing_credential_are_surfaced() {
        let registry = load_bundled_provider_registry().unwrap();
        let resolved = resolve_provider_from(
            &registry,
            Some("kimi"),
            Tool::Claude,
            &ProviderDefaults::default(),
            |_| None,
            || None,
            |_| None,
        )
        .unwrap();
        assert!(resolved.spec.experimental);
        assert!(!resolved.credential_available());
        assert_eq!(resolved.credential_source, None);
    }

    #[test]
    fn provider_key_env_resolves_without_entering_the_spec() {
        let registry = load_bundled_provider_registry().unwrap();
        let secret = "sk-live-provider-secret";
        let resolved = resolve_provider_from(
            &registry,
            Some("grok"),
            Tool::Codex,
            &ProviderDefaults::default(),
            |name| (name == "XAI_API_KEY").then(|| secret.to_string()),
            || None,
            |_| None,
        )
        .unwrap();
        assert_eq!(
            resolved.credential.as_ref().unwrap().secret.expose_secret(),
            secret
        );
        assert_eq!(
            resolved.credential_source.as_deref(),
            Some("env:XAI_API_KEY")
        );
        let rendered = format!("{resolved:?}");
        assert!(!rendered.contains(secret));
        assert!(!format!("{:?}", resolved.spec).contains(secret));
    }

    #[test]
    fn provider_key_env_wins_and_does_not_read_the_keystore() {
        let registry = load_bundled_provider_registry().unwrap();
        let resolved = resolve_provider_from(
            &registry,
            Some("grok"),
            Tool::Codex,
            &ProviderDefaults::default(),
            |name| (name == "XAI_API_KEY").then(|| "env-secret".to_string()),
            || None,
            |_| panic!("keystore must not be read when the environment wins"),
        )
        .unwrap();
        assert_eq!(
            resolved.credential_source.as_deref(),
            Some("env:XAI_API_KEY")
        );
    }

    #[test]
    fn provider_key_keystore_fallback_is_used_after_an_environment_miss() {
        let registry = load_bundled_provider_registry().unwrap();
        let resolved = resolve_provider_from(
            &registry,
            Some("grok"),
            Tool::Codex,
            &ProviderDefaults::default(),
            |_| None,
            || None,
            |id| (id == "grok").then(|| "keystore-secret".to_string()),
        )
        .unwrap();
        assert_eq!(resolved.credential_source.as_deref(), Some("keystore:grok"));
        assert_eq!(
            resolved.credential.as_ref().unwrap().secret.expose_secret(),
            "keystore-secret"
        );
    }

    #[test]
    fn user_provider_add_and_remove_preserve_other_entries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("providers.yaml");
        let spec = ProviderSpec {
            id: "example".to_string(),
            label: None,
            api_style: ApiStyle::OpenAI,
            base_url: "https://api.example.test/v1".to_string(),
            auth: AuthScheme::Bearer,
            key_env: "EXAMPLE_API_KEY".to_string(),
            default_model: Some("example-model".to_string()),
            models: Vec::new(),
            experimental: true,
        };
        append_user_provider_at(&path, &spec).unwrap();
        let loaded = load_registry_at(&path).unwrap();
        assert!(loaded.iter().any(|provider| provider == &spec));

        fs::write(
            &path,
            format!("{}\nother:\n  api_style: anthropic\n  base_url: https://other.example\n  auth: x-api-key\n  key_env: OTHER_KEY\n", fs::read_to_string(&path).unwrap()),
        )
        .unwrap();
        remove_user_provider_at(&path, "example").unwrap();
        let rendered = fs::read_to_string(&path).unwrap();
        assert!(!rendered.contains("example:"));
        assert!(rendered.contains("other:"));
    }

    #[test]
    fn user_provider_without_default_model_is_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("providers.yaml");
        let spec = ProviderSpec {
            id: "acme".to_string(),
            label: None,
            api_style: ApiStyle::Anthropic,
            base_url: "https://api.example.test/anthropic".to_string(),
            auth: AuthScheme::Bearer,
            key_env: "ACME_TOKEN".to_string(),
            default_model: None,
            models: Vec::new(),
            experimental: false,
        };

        append_user_provider_at(&path, &spec).unwrap();

        let loaded = load_registry_at(&path).unwrap();
        assert!(loaded.iter().any(|provider| provider == &spec));
        assert!(!fs::read_to_string(path).unwrap().contains("default_model:"));
    }
    fn spec_with_base_url(base_url: &str) -> ProviderSpec {
        ProviderSpec {
            id: "example".to_string(),
            label: None,
            api_style: ApiStyle::OpenAI,
            base_url: base_url.to_string(),
            auth: AuthScheme::Bearer,
            key_env: "EXAMPLE_API_KEY".to_string(),
            default_model: Some("example-model".to_string()),
            models: Vec::new(),
            experimental: false,
        }
    }

    #[test]
    fn add_time_rejects_userinfo_query_and_fragment_urls() {
        // Each unsafe shape is rejected with its own message and the keystore
        // guidance, before the endpoint-suffix check is reached.
        let cases: &[(&str, &str)] = &[
            ("https://user:secret@host.example/v1", "userinfo"),
            ("https://host.example/v1?token=secret", "query"),
            ("https://host.example/v1#section", "fragment"),
        ];
        let mut messages = Vec::new();
        for &(base_url, marker) in cases {
            let error = validate_user_spec(&spec_with_base_url(base_url))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(marker),
                "URL {base_url:?}: expected message mentioning {marker:?}, got: {error}"
            );
            assert!(
                error.contains("keystore"),
                "URL {base_url:?}: message must point at the keystore, got: {error}"
            );
            messages.push(error);
        }
        // The three rejections are distinct, not one generic message.
        let unique: std::collections::HashSet<&str> = messages.iter().map(String::as_str).collect();
        assert_eq!(
            unique.len(),
            cases.len(),
            "expected distinct rejection messages, got: {messages:?}"
        );
        // A clean URL of the same shape still passes (no over-rejection).
        assert!(validate_user_spec(&spec_with_base_url("https://api.example.test/v1")).is_ok());
    }

    #[test]
    fn load_registry_quarantines_legacy_unsafe_url_and_keeps_good_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("providers.yaml");
        fs::write(
            &path,
            "leaked:\n  api_style: openai\n  base_url: https://user:secret@leaked.example/v1\n  auth: bearer\n  key_env: LEAKED_KEY\n  default_model: leaked-model\ngood:\n  api_style: openai\n  base_url: https://good.example/v1\n  auth: bearer\n  key_env: GOOD_KEY\n  default_model: good-model\n",
        )
        .unwrap();

        let registry = load_registry_at(&path).unwrap();
        assert!(
            registry.iter().all(|spec| spec.id != "leaked"),
            "the unsafe legacy entry must be quarantined, not loaded"
        );
        let good = registry
            .iter()
            .find(|spec| spec.id == "good")
            .expect("the good user entry must survive");
        assert_eq!(good.base_url, "https://good.example/v1");
        // Bundled providers are unaffected.
        assert!(registry.iter().any(|spec| spec.id == "anthropic"));
    }

    #[test]
    fn load_registry_with_only_an_unsafe_entry_still_loads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("providers.yaml");
        fs::write(
            &path,
            "leaked:\n  api_style: openai\n  base_url: https://user:secret@leaked.example/v1\n  auth: bearer\n  key_env: LEAKED_KEY\n  default_model: leaked-model\n",
        )
        .unwrap();

        // Loads (does not error), leaving the user contribution empty but the
        // bundled set usable, so `provider remove leaked` can still recover.
        let registry = load_registry_at(&path).unwrap();
        assert!(registry.iter().all(|spec| spec.id != "leaked"));
        assert!(registry.iter().any(|spec| spec.id == "anthropic"));
    }

    #[test]
    fn quarantine_reports_skipped_ids_and_recovery_command() {
        let mut registry = vec![
            spec_with_base_url("https://user:secret@host.example/v1"),
            spec_with_base_url("https://good.example/v1"),
        ];
        registry[0].id = "leaked".to_string();
        registry[1].id = "good".to_string();

        let mut warnings = Vec::new();
        let kept = quarantine_invalid_base_urls(registry, |id, reason| {
            warnings.push((id.to_string(), reason.to_string()));
        });

        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "good");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].0, "leaked");
        assert!(
            warnings[0].1.contains("userinfo"),
            "reason must name the offending component: {}",
            warnings[0].1
        );

        // The user-facing warning names the entry and the fix command.
        let line = quarantine_warning(&warnings[0].0, &warnings[0].1);
        assert!(line.contains("leaked"));
        assert!(line.contains("capscode provider remove leaked"));
        assert!(line.contains("keystore"));
    }
}
