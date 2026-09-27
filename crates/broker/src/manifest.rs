//! Parse the non-secret providers manifest (gateway env `CAPSCODE_PROVIDERS`)
//! into the broker's route table and allowlist.
//!
//! The manifest is JSON emitted by the CLI from the resolved provider spec. It
//! carries **no** secrets — only the vhost→authority routing, the credential
//! *env-var name* (`key_env`), the path prefix, and the injection scheme. The
//! real keys arrive separately via `keys.env` and are matched to routes by
//! `key_env`. The allowlist is derived solely from this manifest: default-deny,
//! no wildcard, no host the capsule can inject.

use std::collections::HashMap;

use serde::Deserialize;

use crate::route::{InjectScheme, Route};

/// Errors that make the manifest unusable. Every one is fatal at startup: the
/// broker fails closed rather than serving a partial or ambiguous route table.
#[derive(thiserror::Error, Debug)]
pub enum ManifestError {
    #[error("CAPSCODE_PROVIDERS is not valid JSON: {0}")]
    Json(String),
    #[error("CAPSCODE_PROVIDERS contains no providers; the broker has nothing to route")]
    Empty,
    #[error("CAPSCODE_PROVIDERS entry {index} has an empty `{field}`")]
    EmptyField { index: usize, field: &'static str },
    #[error("CAPSCODE_PROVIDERS lists vhost `{0}` more than once")]
    DuplicateVhost(String),
}

/// One provider entry as it appears in the JSON manifest.
///
/// `id` and `api_style` are part of the contract but not consumed by the broker
/// — routing and injection are fully determined by `vhost`, `authority`,
/// `base_path`, `inject`, and `key_env` — so serde ignores them (and any other
/// forward-compatible field).
#[derive(Deserialize)]
struct ManifestEntry {
    vhost: String,
    authority: String,
    #[serde(default)]
    base_path: String,
    inject: InjectWire,
    key_env: String,
}

/// Wire form of the injection scheme (`"bearer"` | `"x-api-key"`).
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
enum InjectWire {
    Bearer,
    XApiKey,
}

impl From<InjectWire> for InjectScheme {
    fn from(w: InjectWire) -> InjectScheme {
        match w {
            InjectWire::Bearer => InjectScheme::Bearer,
            InjectWire::XApiKey => InjectScheme::XApiKey,
        }
    }
}

/// The parsed, validated route table: vhost → route. The ONLY source of the
/// broker's routes and its default-deny allowlist.
#[derive(Clone, Debug, Default)]
pub struct ProviderManifest {
    pub(crate) routes: HashMap<String, Route>,
}

impl ProviderManifest {
    /// Parse the `CAPSCODE_PROVIDERS` JSON array. Fails closed on invalid JSON, an
    /// empty array, an empty required field, or a duplicate vhost.
    pub fn from_json(json: &str) -> Result<ProviderManifest, ManifestError> {
        let entries: Vec<ManifestEntry> =
            serde_json::from_str(json).map_err(|e| ManifestError::Json(e.to_string()))?;
        if entries.is_empty() {
            return Err(ManifestError::Empty);
        }

        let mut routes: HashMap<String, Route> = HashMap::with_capacity(entries.len());
        for (index, entry) in entries.into_iter().enumerate() {
            let vhost = entry.vhost.trim().to_string();
            let authority = entry.authority.trim().to_string();
            let key_env = entry.key_env.trim().to_string();
            if vhost.is_empty() {
                return Err(ManifestError::EmptyField {
                    index,
                    field: "vhost",
                });
            }
            if authority.is_empty() {
                return Err(ManifestError::EmptyField {
                    index,
                    field: "authority",
                });
            }
            if key_env.is_empty() {
                return Err(ManifestError::EmptyField {
                    index,
                    field: "key_env",
                });
            }
            let route = Route {
                authority,
                base_path: entry.base_path.trim().to_string(),
                inject: entry.inject.into(),
                key_env,
            };
            if routes.insert(vhost.clone(), route).is_some() {
                return Err(ManifestError::DuplicateVhost(vhost));
            }
        }
        Ok(ProviderManifest { routes })
    }

    /// The credential env-var names the manifest references — used for the startup
    /// "at least one usable credential" check and for logging. Never a value.
    pub fn key_envs(&self) -> impl Iterator<Item = &str> {
        self.routes.values().map(|r| r.key_env.as_str())
    }

    /// The virtual hosts served and included in the broker leaf certificate SANs.
    pub fn vhosts(&self) -> impl Iterator<Item = &str> {
        self.routes.keys().map(|s| s.as_str())
    }

    /// Number of routes (providers) in the manifest.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether the manifest has no routes.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"id":"anthropic","vhost":"anthropic.broker.capscode.local","authority":"api.anthropic.com",
       "base_path":"","api_style":"anthropic","inject":"x-api-key","key_env":"ANTHROPIC_API_KEY"},
      {"id":"kimi","vhost":"kimi.broker.capscode.local","authority":"api.moonshot.ai",
       "base_path":"/anthropic","api_style":"anthropic","inject":"bearer","key_env":"MOONSHOT_API_KEY"}
    ]"#;

    #[test]
    fn parses_and_maps_fields() {
        let m = ProviderManifest::from_json(SAMPLE).unwrap();
        assert_eq!(m.len(), 2);

        let kimi = m.routes.get("kimi.broker.capscode.local").unwrap();
        assert_eq!(kimi.authority, "api.moonshot.ai");
        assert_eq!(kimi.base_path, "/anthropic");
        assert_eq!(kimi.inject, InjectScheme::Bearer);
        assert_eq!(kimi.key_env, "MOONSHOT_API_KEY");

        let anthropic = m.routes.get("anthropic.broker.capscode.local").unwrap();
        assert_eq!(anthropic.inject, InjectScheme::XApiKey);
        assert_eq!(anthropic.base_path, "");

        let mut envs: Vec<&str> = m.key_envs().collect();
        envs.sort_unstable();
        assert_eq!(envs, vec!["ANTHROPIC_API_KEY", "MOONSHOT_API_KEY"]);
    }

    #[test]
    fn empty_array_is_rejected() {
        assert!(matches!(
            ProviderManifest::from_json("[]"),
            Err(ManifestError::Empty)
        ));
    }

    #[test]
    fn invalid_json_is_rejected() {
        assert!(matches!(
            ProviderManifest::from_json("not json"),
            Err(ManifestError::Json(_))
        ));
    }

    #[test]
    fn duplicate_vhost_is_rejected() {
        let json = r#"[
          {"vhost":"dup.local","authority":"a.example","inject":"bearer","key_env":"A"},
          {"vhost":"dup.local","authority":"b.example","inject":"bearer","key_env":"B"}
        ]"#;
        assert!(matches!(
            ProviderManifest::from_json(json),
            Err(ManifestError::DuplicateVhost(v)) if v == "dup.local"
        ));
    }

    #[test]
    fn empty_required_field_is_rejected() {
        let json = r#"[{"vhost":"","authority":"a.example","inject":"bearer","key_env":"A"}]"#;
        assert!(matches!(
            ProviderManifest::from_json(json),
            Err(ManifestError::EmptyField { field: "vhost", .. })
        ));
    }

    #[test]
    fn base_path_defaults_to_empty_and_unknown_fields_ignored() {
        // No base_path; extra `id`/`api_style`/future fields present and ignored.
        let json = r#"[{"id":"x","vhost":"v.local","authority":"a.example",
                        "api_style":"openai","future_field":42,"inject":"bearer","key_env":"A"}]"#;
        let m = ProviderManifest::from_json(json).unwrap();
        assert_eq!(m.routes.get("v.local").unwrap().base_path, "");
    }
}
