use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ids::Tool;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiStyle {
    Anthropic,
    OpenAI,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthScheme {
    Bearer,
    XApiKey,
}

/// Public, non-secret description of a provider endpoint.
///
/// `key_env` is an environment-variable name. Credential values are resolved
/// separately and can never be serialized into this type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSpec {
    pub id: String,
    /// Human-facing name shown in the picker. Lets several registry ids share a
    /// vendor name — e.g. `kimi` (Anthropic endpoint) and `kimi-openai` (OpenAI
    /// endpoint) both display as `kimi`. Defaults to `id` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub api_style: ApiStyle,
    pub base_url: String,
    pub auth: AuthScheme,
    pub key_env: String,
    /// Model selected when a multi-provider tool needs an explicit starting
    /// model. Optional for existing user registries; Pi refuses to launch
    /// without it instead of falling back to a direct built-in provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default)]
    pub experimental: bool,
}

impl ProviderSpec {
    /// Name to display to the user: the explicit `label`, else the `id`.
    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }

    /// Every model ID this provider exposes to catalog-driven tools (Pi, opencode).
    /// Falls back to the single `default_model` when no explicit list is given, so
    /// existing single-model providers are unchanged.
    pub fn model_ids(&self) -> Vec<String> {
        // Guarantee the launch `default_model` is in the catalog (first), so tools
        // can never boot on a model their picker omits, then dedup while keeping
        // order — a drifted registry (`default_model` absent from `models`, or a
        // repeated id) can't produce duplicate or unreachable picker entries.
        let mut ids: Vec<String> = Vec::new();
        ids.extend(self.default_model.iter().cloned());
        ids.extend(self.models.iter().cloned());
        let mut seen = std::collections::HashSet::new();
        ids.retain(|model| seen.insert(model.clone()));
        ids
    }

    pub fn supports_tool(&self, tool: Tool) -> bool {
        match (self.api_style, tool) {
            (ApiStyle::Anthropic, Tool::Claude | Tool::Pi | Tool::Omp | Tool::Opencode)
            | (ApiStyle::OpenAI, Tool::Codex | Tool::Pi | Tool::Omp | Tool::Opencode) => true,
            (ApiStyle::Anthropic, Tool::Codex) | (ApiStyle::OpenAI, Tool::Claude) => false,
        }
    }
}

#[derive(Debug, Error)]
pub enum ProviderRegistryError {
    #[error("parsing provider registry YAML: {0}")]
    Parse(String),
    #[error("provider '{id}' is missing required field '{field}'")]
    MissingField { id: String, field: &'static str },
    #[error("provider id must not be empty")]
    EmptyId,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderFields {
    label: Option<String>,
    api_style: Option<ApiStyle>,
    base_url: Option<String>,
    auth: Option<AuthScheme>,
    key_env: Option<String>,
    default_model: Option<String>,
    models: Option<String>,
    experimental: Option<bool>,
}

impl ProviderFields {
    fn overlay(&mut self, other: ProviderFields) {
        if other.label.is_some() {
            self.label = other.label;
        }
        if other.api_style.is_some() {
            self.api_style = other.api_style;
        }
        if other.base_url.is_some() {
            self.base_url = other.base_url;
        }
        if other.auth.is_some() {
            self.auth = other.auth;
        }
        if other.key_env.is_some() {
            self.key_env = other.key_env;
        }
        if other.default_model.is_some() {
            self.default_model = other.default_model;
        }
        if other.models.is_some() {
            self.models = other.models;
        }
        if other.experimental.is_some() {
            self.experimental = other.experimental;
        }
    }

    fn into_spec(self, id: String) -> Result<ProviderSpec, ProviderRegistryError> {
        if id.trim().is_empty() {
            return Err(ProviderRegistryError::EmptyId);
        }
        let models = self
            .models
            .map(|raw| {
                raw.split(',')
                    .map(|model| model.trim().to_string())
                    .filter(|model| !model.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        macro_rules! required {
            ($field:ident) => {
                self.$field
                    .ok_or_else(|| ProviderRegistryError::MissingField {
                        id: id.clone(),
                        field: stringify!($field),
                    })?
            };
        }
        Ok(ProviderSpec {
            label: self.label,
            api_style: required!(api_style),
            base_url: required!(base_url),
            auth: required!(auth),
            key_env: required!(key_env),
            default_model: self.default_model,
            models,
            experimental: self.experimental.unwrap_or(false),
            id,
        })
    }
}

fn parse_fields(yaml: &str) -> Result<BTreeMap<String, ProviderFields>, ProviderRegistryError> {
    // The registry schema is intentionally tiny: a top-level id mapping whose
    // values contain scalar fields only. Convert that constrained YAML shape to
    // TOML, then let Serde enforce types and reject unknown fields.
    let mut toml_source = String::new();
    let mut current_provider = false;
    for (index, line) in yaml.lines().enumerate() {
        let line_number = index + 1;
        if line.contains('\t') {
            return Err(ProviderRegistryError::Parse(format!(
                "line {line_number}: tabs are not supported"
            )));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') {
            let id = trimmed.strip_suffix(':').ok_or_else(|| {
                ProviderRegistryError::Parse(format!(
                    "line {line_number}: expected a provider id followed by ':'"
                ))
            })?;
            if id.is_empty() {
                return Err(ProviderRegistryError::EmptyId);
            }
            toml_source.push_str("[\"");
            toml_source.push_str(&escape_toml_string(id));
            toml_source.push_str("\"]\n");
            current_provider = true;
            continue;
        }
        if !current_provider || !line.starts_with("  ") || line.starts_with("   ") {
            return Err(ProviderRegistryError::Parse(format!(
                "line {line_number}: provider fields must use exactly two spaces of indentation"
            )));
        }
        let (field, raw_value) = trimmed.split_once(':').ok_or_else(|| {
            ProviderRegistryError::Parse(format!("line {line_number}: expected 'field: value'"))
        })?;
        let value = raw_value.trim();
        if field.is_empty() || value.is_empty() {
            return Err(ProviderRegistryError::Parse(format!(
                "line {line_number}: expected a non-empty field and value"
            )));
        }
        toml_source.push_str(field);
        toml_source.push_str(" = ");
        if field == "experimental" {
            match value {
                "true" | "false" => toml_source.push_str(value),
                _ => {
                    return Err(ProviderRegistryError::Parse(format!(
                        "line {line_number}: experimental must be true or false"
                    )))
                }
            }
        } else {
            let value = unquote_yaml_scalar(value);
            toml_source.push('"');
            toml_source.push_str(&escape_toml_string(value));
            toml_source.push('"');
        }
        toml_source.push('\n');
    }
    toml::from_str(&toml_source).map_err(|error| ProviderRegistryError::Parse(error.to_string()))
}

fn unquote_yaml_scalar(value: &str) -> &str {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Parses the registry embedded in `capscode-core`.
pub fn load_bundled_provider_registry() -> Result<Vec<ProviderSpec>, ProviderRegistryError> {
    merge_provider_registries(include_str!("../../../providers/registry.yaml"), None)
}

/// Parses the bundled registry and applies optional field-level user overrides.
/// A user id replaces only the fields it supplies and may also define a complete
/// new provider.
pub fn merge_provider_registries(
    bundled_yaml: &str,
    user_yaml: Option<&str>,
) -> Result<Vec<ProviderSpec>, ProviderRegistryError> {
    let mut providers = parse_fields(bundled_yaml)?;
    if let Some(user_yaml) = user_yaml {
        for (id, fields) in parse_fields(user_yaml)? {
            providers.entry(id).or_default().overlay(fields);
        }
    }
    providers
        .into_iter()
        .map(|(id, fields)| fields.into_spec(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_registry_parses_and_prints_a_resolved_spec() {
        let registry = load_bundled_provider_registry().unwrap();
        for provider in &registry {
            let serialized = toml::to_string(provider).unwrap();
            assert!(
                serialized
                    .lines()
                    .any(|line| line.starts_with("default_model = ")),
                "bundled provider '{}' has no default_model",
                provider.id
            );
        }
        let anthropic = registry.iter().find(|spec| spec.id == "anthropic").unwrap();
        eprintln!(
            "bundled provider count: {}; resolved: {anthropic:?}",
            registry.len()
        );
        assert_eq!(registry.len(), 21);
        assert_eq!(anthropic.api_style, ApiStyle::Anthropic);
        assert_eq!(anthropic.display_name(), "anthropic");
        assert_eq!(anthropic.model_ids(), ["claude-opus-4-8"]);
        assert!(anthropic.supports_tool(Tool::Claude));
        assert!(!anthropic.supports_tool(Tool::Codex));
        // A `label` lets a distinct id present a vendor name in the picker:
        // kimi-openai (its own id, for config stability) shows as "kimi".
        let kimi_openai = registry
            .iter()
            .find(|spec| spec.id == "kimi-openai")
            .unwrap();
        assert_eq!(kimi_openai.display_name(), "kimi");
        assert_eq!(kimi_openai.api_style, ApiStyle::OpenAI);

        for id in ["kimi-coding", "kimi-coding-openai"] {
            let provider = registry.iter().find(|spec| spec.id == id).unwrap();
            assert_eq!(
                provider.model_ids(),
                ["kimi-for-coding", "kimi-for-coding-highspeed", "k3"]
            );
        }
    }

    #[test]
    fn bundled_zai_matrix_distinguishes_product_region_and_protocol() {
        let registry = load_bundled_provider_registry().unwrap();
        let expected = [
            (
                "zai-api",
                ApiStyle::OpenAI,
                "https://api.z.ai/api/paas/v4",
                "GLM_API_KEY",
                "zai-api",
            ),
            (
                "zai-api-anthropic",
                ApiStyle::Anthropic,
                "https://api.z.ai/api/anthropic",
                "GLM_API_KEY",
                "zai-api",
            ),
            (
                "zai-api-cn",
                ApiStyle::OpenAI,
                "https://open.bigmodel.cn/api/paas/v4",
                "GLM_CN_API_KEY",
                "zai-api-cn",
            ),
            (
                "zai-api-cn-anthropic",
                ApiStyle::Anthropic,
                "https://open.bigmodel.cn/api/anthropic",
                "GLM_CN_API_KEY",
                "zai-api-cn",
            ),
            (
                "zai-coding",
                ApiStyle::Anthropic,
                "https://api.z.ai/api/anthropic",
                "GLM_API_KEY",
                "zai-coding",
            ),
            (
                "zai-coding-openai",
                ApiStyle::OpenAI,
                "https://api.z.ai/api/coding/paas/v4",
                "GLM_API_KEY",
                "zai-coding",
            ),
            (
                "zai-coding-cn",
                ApiStyle::Anthropic,
                "https://open.bigmodel.cn/api/anthropic",
                "GLM_CN_API_KEY",
                "zai-coding-cn",
            ),
            (
                "zai-coding-cn-openai",
                ApiStyle::OpenAI,
                "https://open.bigmodel.cn/api/coding/paas/v4",
                "GLM_CN_API_KEY",
                "zai-coding-cn",
            ),
        ];

        for (id, api_style, base_url, key_env, display_name) in expected {
            let provider = registry.iter().find(|spec| spec.id == id).unwrap();
            assert_eq!(provider.api_style, api_style, "{id}");
            assert_eq!(provider.base_url.as_str(), base_url, "{id}");
            assert_eq!(provider.key_env, key_env, "{id}");
            assert_eq!(provider.display_name(), display_name, "{id}");
            assert_eq!(provider.default_model.as_deref(), Some("glm-5.2"), "{id}");
            assert_eq!(
                provider.model_ids(),
                ["glm-5.2", "glm-5-turbo", "glm-4.7"],
                "{id}"
            );
            assert!(provider.supports_tool(Tool::Omp), "{id}");
        }

        assert!(
            registry.iter().all(|provider| provider.id != "glm"),
            "the ambiguous unpublished glm id must not return"
        );
    }

    #[test]
    fn user_registry_overrides_fields_and_extends() {
        let merged = merge_provider_registries(
            r#"
preset:
  api_style: openai
  base_url: https://preset.example/v1
  auth: bearer
  key_env: PRESET_KEY
"#,
            Some(
                r#"
preset:
  base_url: https://override.example/v1
  key_env: OVERRIDE_KEY
  models: first, second, , third
custom:
  api_style: anthropic
  base_url: https://custom.example/anthropic
  auth: x-api-key
  key_env: CUSTOM_KEY
"#,
            ),
        )
        .unwrap();
        let preset = merged.iter().find(|spec| spec.id == "preset").unwrap();
        assert_eq!(preset.api_style, ApiStyle::OpenAI);
        assert_eq!(preset.base_url, "https://override.example/v1");
        assert_eq!(preset.key_env, "OVERRIDE_KEY");
        assert_eq!(preset.model_ids(), ["first", "second", "third"]);
        assert!(merged.iter().any(|spec| spec.id == "custom"));
    }

    #[test]
    fn registry_schema_cannot_store_secret_values() {
        let error = merge_provider_registries(
            r#"
unsafe:
  api_style: openai
  base_url: https://example.com/v1
  auth: bearer
  key_env: SAFE_ENV_NAME
  api_key: sk-live-secret
"#,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field `api_key`"));
    }
}
