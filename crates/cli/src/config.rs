use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::fs::File;
use std::io::{self, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::Engine;
use capscode_core::{
    ApiStyle, AuthKind, AuthScheme, Config, ImageRef, MemoryLimit, PidsLimit, Provider,
    ProviderCredential, ProviderDefaults, ProviderKeys, ProviderSpec, RuntimePref, Tool,
    WorkspaceMode,
};
use dialoguer::{theme::ColorfulTheme, Input, Select};
use figment::providers::{Format, Serialized, Toml};
use figment::Figment;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tracing::debug;

use crate::args::{ConfigKeyArg, RunArgs, RuntimeArg};
use crate::ui::data;

#[path = "providers.rs"]
pub(crate) mod providers;

pub(crate) const DEFAULT_KEYCHAIN_SERVICE: &str = "claude-setup-token";
const KEYCHAIN_SERVICE_ENV: &str = "CAPSCODE_KEYCHAIN_SERVICE";

type KeychainRead = std::result::Result<Option<SecretString>, String>;

// A command may resolve provider status through several paths. macOS can show an
// authorization dialog for every native Keychain read, so share one result for
// the lifetime of the process. Import/remove explicitly invalidate this cache.
static ANTHROPIC_KEYCHAIN_CACHE: Mutex<Option<KeychainRead>> = Mutex::new(None);
static PROVIDER_KEYSTORE_CACHE: Mutex<Option<crate::keystore::KeystoreState>> = Mutex::new(None);

// Whether the user opted into keychain reads. Cached because it costs a config
// file read, and invalidated by `set_keychain_enabled` because `init` flips it
// mid-process.
static KEYCHAIN_ENABLED_CACHE: Mutex<Option<bool>> = Mutex::new(None);

#[derive(Clone)]
struct CodexCredential {
    credential: ProviderCredential,
    source: String,
}

type CodexAuthRead = std::result::Result<Option<CodexCredential>, String>;
static CODEX_AUTH_CACHE: Mutex<Option<CodexAuthRead>> = Mutex::new(None);
const MIN_CODEX_ACCESS_TOKEN_LIFETIME_SECS: u64 = 60 * 60;
const MAX_CODEX_AUTH_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize)]
struct CodexAuthFile {
    auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    tokens: Option<CodexTokenData>,
}

#[derive(Deserialize)]
struct CodexTokenData {
    access_token: String,
}

#[derive(Deserialize)]
struct JwtExpiry {
    exp: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AnthropicAuthStatus {
    source: Option<String>,
    kind: Option<AuthKind>,
}

impl AnthropicAuthStatus {
    pub(crate) fn source(&self) -> &str {
        self.source.as_deref().unwrap_or("none")
    }

    pub(crate) fn kind(&self) -> Option<AuthKind> {
        self.kind
    }

    fn configured_line(&self) -> Option<String> {
        self.kind.map(|kind| {
            format!(
                "Anthropic credential: {} ({})",
                self.source(),
                credential_kind_name(kind)
            )
        })
    }

    pub(crate) fn status_line(&self) -> String {
        self.configured_line().unwrap_or_else(|| {
            if keychain_lookup_enabled() {
                "Anthropic credential: none".to_string()
            } else {
                "Anthropic credential: none (keychain lookup disabled; run 'capscode key set anthropic' to store and enable one)".to_string()
            }
        })
    }

    pub(crate) fn doctor_line(&self) -> String {
        self.configured_line().unwrap_or_else(|| {
            if keychain_lookup_enabled() {
                "Anthropic credential: not configured".to_string()
            } else {
                "Anthropic credential: none (keychain lookup disabled; run 'capscode key set anthropic' to store and enable one)".to_string()
            }
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CodexAuthStatus {
    source: Option<String>,
    kind: Option<AuthKind>,
}

impl CodexAuthStatus {
    pub(crate) fn source(&self) -> &str {
        self.source.as_deref().unwrap_or("none")
    }

    pub(crate) fn kind(&self) -> Option<AuthKind> {
        self.kind
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProviderKeystoreStatus {
    Disabled,
    Empty,
    Loaded(Vec<String>),
    Problem(String),
}

#[derive(Debug)]
struct ResolvedProviderKeys {
    keys: ProviderKeys,
    anthropic: AnthropicAuthStatus,
}

pub struct ResolvedRun {
    pub config: Config,
    /// Providers (besides the selected one) that already have a stored key and
    /// are therefore routed at box creation, so a devbox can switch providers
    /// between attaches. Empty outside devbox mode.
    pub(crate) extra_providers: Vec<(ProviderSpec, ProviderCredential)>,
    pub(crate) selected_provider: ProviderSpec,
    pub(crate) selected_credential: Option<ProviderCredential>,
    pub(crate) selected_credential_source: Option<String>,
    pub(crate) allow_experimental: bool,
    pub host_repo: PathBuf,
    pub gateway_image: ImageRef,
    pub capsule_image: ImageRef,
    pub seeder_image: ImageRef,
    pub tool_command: String,
    pub tool_args: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigScalar {
    Bool(bool),
    Integer(i64),
    String(String),
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigRow {
    pub key: ConfigKeyArg,
    pub persisted: Option<toml::Value>,
    pub effective: Option<ConfigScalar>,
    pub source: String,
    pub error: Option<String>,
}

pub fn load(runtime: Option<RuntimeArg>) -> Result<Config> {
    let mut config = Config::load().context("resolving configuration")?;
    if let Some(runtime) = runtime {
        config.runtime = RuntimePref::from(runtime);
    }
    Ok(config)
}

pub(crate) fn load_with_auth_status(
    runtime: Option<RuntimeArg>,
) -> Result<(Config, AnthropicAuthStatus)> {
    let mut config = load(runtime)?;
    let resolved = provider_keys();
    config.broker.keys = resolved.keys;
    Ok((config, resolved.anthropic))
}

pub fn set_wireguard_conf(path: Option<&Path>) -> Result<()> {
    let config_path = Config::prepare_config_file().context("preparing config home")?;
    set_wireguard_conf_at(&config_path, path)
}

// Provider commands and the chooser use this shared resolution path.
pub fn set_provider_default(tool: Tool, provider_id: Option<&str>) -> Result<()> {
    let config_path = Config::prepare_config_file().context("preparing config home")?;
    set_provider_default_at(&config_path, tool, provider_id)
}

pub(crate) fn set_provider_default_at(
    config_path: &Path,
    tool: Tool,
    provider_id: Option<&str>,
) -> Result<()> {
    let canonical = provider_id
        .map(|id| validate_provider_default(tool, id).map(|spec| spec.id))
        .transpose()?;
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    let tool_name = tool.name();
    if let Some(provider_id) = canonical {
        let defaults = root
            .entry("defaults")
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .context("config key 'defaults' must be a TOML table")?;
        defaults.insert(
            tool_name.to_string(),
            toml::Value::String(provider_id.to_string()),
        );
    } else if let Some(defaults) = root.get_mut("defaults") {
        let defaults = defaults
            .as_table_mut()
            .context("config key 'defaults' must be a TOML table")?;
        let removed = defaults.remove(tool_name).is_some();
        if removed && defaults.is_empty() {
            root.remove("defaults");
        }
    }
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn set_wireguard_conf_at(config_path: &Path, path: Option<&Path>) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let previous_wireguard_copy = managed_wireguard_copy(&document, config_path);

    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    let mut new_wireguard_copy = None;
    if let Some(path) = path {
        let network = root
            .entry("network")
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .context("config key 'network' must be a TOML table")?;

        use zeroize::Zeroize;

        let mut source = fs::read_to_string(path)
            .with_context(|| format!("reading WireGuard config {}", path.display()))?;
        let parsed = capscode_net::parse_wireguard_conf(&source);
        source.zeroize();
        let parsed = parsed.context("validating WireGuard config before import")?;
        let mut canonical = parsed.to_wg_quick();
        let digest = wireguard_config_digest(canonical.as_bytes());
        let stored_path = create_private_wireguard_config(config_path, canonical.as_bytes());
        canonical.zeroize();
        let stored_path = stored_path?;
        let Some(stored_path_text) = stored_path.to_str() else {
            let _ = fs::remove_file(&stored_path);
            bail!("stored WireGuard config path is not valid UTF-8");
        };
        network.insert(
            "wireguard_conf".to_string(),
            toml::Value::String(stored_path_text.to_string()),
        );
        network.insert(
            "wireguard_conf_sha256".to_string(),
            toml::Value::String(digest.clone()),
        );
        // Importing a VPN means using it: enforce the tunnel (fail-closed) so
        // sessions route through it. `capscode vpn remove` clears this.
        network.insert("enforce_vpn".to_string(), toml::Value::Boolean(true));
        new_wireguard_copy = Some((stored_path, digest));
    } else if let Some(network) = root.get_mut("network") {
        let network = network
            .as_table_mut()
            .context("config key 'network' must be a TOML table")?;
        network.remove("wireguard_conf");
        network.remove("wireguard_conf_sha256");
        network.remove("enforce_vpn");
    }
    if document == original {
        return Ok(());
    }
    if let Err(error) = write_config_document(config_path, &document) {
        if let Some((stored_path, _)) = new_wireguard_copy {
            let _ = fs::remove_file(stored_path);
        }
        return Err(error);
    }
    if let Some((previous_path, previous_digest)) = previous_wireguard_copy {
        remove_managed_wireguard_copy(&previous_path, &previous_digest)?;
    }
    Ok(())
}

fn managed_wireguard_copy(document: &toml::Value, config_path: &Path) -> Option<(PathBuf, String)> {
    let network = document.get("network")?.as_table()?;
    let path = PathBuf::from(network.get("wireguard_conf")?.as_str()?);
    let digest = network.get("wireguard_conf_sha256")?.as_str()?;
    let parent = config_path.parent()?;
    let file_name = path.file_name()?.to_str()?;
    if path.parent()? != parent
        || !file_name.starts_with("wireguard-")
        || !file_name.ends_with(".conf")
    {
        return None;
    }
    Some((path, digest.to_string()))
}

fn wireguard_config_digest(contents: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(contents);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

fn remove_managed_wireguard_copy(path: &Path, expected_digest: &str) -> Result<()> {
    let Ok(contents) = fs::read(path) else {
        return Ok(());
    };
    if wireguard_config_digest(&contents) != expected_digest {
        return Ok(());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("removing stored WireGuard config {}", path.display())),
    }
}

fn create_private_wireguard_config(config_path: &Path, contents: &[u8]) -> Result<PathBuf> {
    let parent = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("config file path has no parent directory")?;
    parent
        .to_str()
        .context("config directory path is not valid UTF-8")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating config directory {}", parent.display()))?;
    set_private_config_mode(parent)
        .with_context(|| format!("restricting config directory {}", parent.display()))?;

    let temporary =
        write_private_tempfile(parent, "wireguard-", ".conf", contents, "WireGuard config")?;
    let (_, path) = temporary
        .keep()
        .map_err(|error| error.error)
        .context("keeping canonical WireGuard config")?;
    Ok(path)
}

pub fn set_keychain_enabled(enabled: bool) -> Result<()> {
    let config_path = Config::prepare_config_file().context("preparing config home")?;
    set_keychain_enabled_at(&config_path, enabled)?;
    refresh_keychain_enabled_cache_at(&config_path);
    Ok(())
}

pub(crate) fn set_keychain_enabled_at(config_path: &Path, enabled: bool) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();

    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    let auth = root
        .entry("auth")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("config key 'auth' must be a TOML table")?;
    auth.insert("keychain".to_string(), toml::Value::Boolean(enabled));
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn unset_keychain_enabled_at(config_path: &Path) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    if let Some(auth) = root.get_mut("auth") {
        let auth = auth
            .as_table_mut()
            .context("config key 'auth' must be a TOML table")?;
        let removed = auth.remove("keychain").is_some();
        if removed && auth.is_empty() {
            root.remove("auth");
        }
    }
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn set_runtime_at(config_path: &Path, value: &str) -> Result<()> {
    let runtime = parse_runtime_pref(value)?;
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    root.insert(
        "runtime".to_string(),
        toml::Value::String(runtime_name(&runtime).to_string()),
    );
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn unset_runtime_at(config_path: &Path) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    root.remove("runtime");
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub fn set_workspace_mode(mode: WorkspaceMode) -> Result<()> {
    let config_path = Config::prepare_config_file().context("preparing config home")?;
    set_workspace_mode_at(&config_path, mode)
}

pub(crate) fn set_workspace_mode_at(config_path: &Path, mode: WorkspaceMode) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    let workspace = root
        .entry("workspace")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("config key 'workspace' must be a TOML table")?;
    workspace.insert(
        "mode".to_string(),
        toml::Value::String(mode.as_str().to_string()),
    );
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn unset_workspace_mode_at(config_path: &Path) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    if let Some(workspace) = root.get_mut("workspace") {
        let workspace = workspace
            .as_table_mut()
            .context("config key 'workspace' must be a TOML table")?;
        let removed = workspace.remove("mode").is_some();
        if removed && workspace.is_empty() {
            root.remove("workspace");
        }
    }
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn set_box_prefix_at(config_path: &Path, value: &str) -> Result<()> {
    let prefix = parse_box_prefix(value)?;
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    let section = root
        .entry("box")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("config key 'box' must be a TOML table")?;
    section.insert("prefix".to_string(), toml::Value::String(prefix));
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn unset_box_prefix_at(config_path: &Path) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    if let Some(section) = root.get_mut("box") {
        let section = section
            .as_table_mut()
            .context("config key 'box' must be a TOML table")?;
        let removed = section.remove("prefix").is_some();
        if removed && section.is_empty() {
            root.remove("box");
        }
    }
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

/// Persist `capsule.memory` (canonical size string) or `capsule.pids` (integer).
pub(crate) fn set_capsule_limit_at(
    config_path: &Path,
    key: ConfigKeyArg,
    value: &str,
) -> Result<()> {
    let (leaf, persisted) = match key {
        ConfigKeyArg::CapsuleMemory => (
            "memory",
            toml::Value::String(parse_capsule_memory(value)?.to_string()),
        ),
        ConfigKeyArg::CapsulePids => (
            "pids",
            toml::Value::Integer(i64::from(parse_capsule_pids(value)?.get())),
        ),
        _ => bail!("{} is not a capsule limit", key.as_str()),
    };
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    let section = root
        .entry("capsule")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("config key 'capsule' must be a TOML table")?;
    section.insert(leaf.to_string(), persisted);
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn unset_capsule_limit_at(config_path: &Path, key: ConfigKeyArg) -> Result<()> {
    let (_, Some(leaf)) = config_path_parts(key) else {
        bail!("{} is not a capsule limit", key.as_str());
    };
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    if let Some(section) = root.get_mut("capsule") {
        let section = section
            .as_table_mut()
            .context("config key 'capsule' must be a TOML table")?;
        let removed = section.remove(leaf).is_some();
        if removed && section.is_empty() {
            root.remove("capsule");
        }
    }
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn set_capsule_opencode_lsp_download_at(
    config_path: &Path,
    enabled: bool,
) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    let section = root
        .entry("capsule")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("config key 'capsule' must be a TOML table")?;
    section.insert(
        "opencode_lsp_download".to_string(),
        toml::Value::Boolean(enabled),
    );
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn unset_capsule_opencode_lsp_download_at(config_path: &Path) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    if let Some(section) = root.get_mut("capsule") {
        let section = section
            .as_table_mut()
            .context("config key 'capsule' must be a TOML table")?;
        let removed = section.remove("opencode_lsp_download").is_some();
        if removed && section.is_empty() {
            root.remove("capsule");
        }
    }
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn set_capsule_cache_volume_at(config_path: &Path, enabled: bool) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    let section = root
        .entry("capsule")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("config key 'capsule' must be a TOML table")?;
    section.insert("cache_volume".to_string(), toml::Value::Boolean(enabled));
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn unset_capsule_cache_volume_at(config_path: &Path) -> Result<()> {
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    if let Some(section) = root.get_mut("capsule") {
        let section = section
            .as_table_mut()
            .context("config key 'capsule' must be a TOML table")?;
        let removed = section.remove("cache_volume").is_some();
        if removed && section.is_empty() {
            root.remove("capsule");
        }
    }
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

/// Read only the persisted TOML document. Missing files are represented by an
/// empty table; no defaults, environment variables, flags, or credentials are
/// consulted.
pub(crate) fn read_persisted_document_at(config_path: &Path) -> Result<toml::Value> {
    if !config_path.exists() {
        return Ok(toml::Value::Table(toml::map::Map::new()));
    }
    let contents = fs::read_to_string(config_path)
        .with_context(|| format!("reading config file {}", config_path.display()))?;
    let document = toml::from_str::<toml::Value>(&contents)
        .with_context(|| format!("parsing config file {}", config_path.display()))?;
    if !document.is_table() {
        bail!("config file root must be a TOML table");
    }
    Ok(document)
}

/// Atomically replace a dedicated CapsCode config document with a user-only
/// file. The temporary file stays in the destination directory so rename is
/// atomic and a pre-rename error leaves the previous document intact.
pub(crate) fn write_config_document(config_path: &Path, document: &toml::Value) -> Result<()> {
    let parent = config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .context("config file path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating config directory {}", parent.display()))?;
    set_private_config_mode(parent)
        .with_context(|| format!("restricting config directory {}", parent.display()))?;

    let contents = toml::to_string_pretty(document).context("serializing config document")?;
    let temporary = write_private_tempfile(parent, "", "", contents.as_bytes(), "config file")?;
    temporary
        .persist(config_path)
        .map_err(|error| error.error)
        .with_context(|| format!("replacing config file {}", config_path.display()))?;
    Ok(())
}

fn write_private_tempfile(
    parent: &Path,
    prefix: &str,
    suffix: &str,
    contents: &[u8],
    label: &str,
) -> Result<tempfile::NamedTempFile> {
    let mut temporary = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(suffix)
        .tempfile_in(parent)
        .with_context(|| format!("creating temporary {label} in {}", parent.display()))?;
    set_private_file_mode(temporary.path()).with_context(|| {
        format!(
            "restricting temporary {label} {}",
            temporary.path().display()
        )
    })?;
    temporary
        .as_file_mut()
        .write_all(contents)
        .with_context(|| format!("writing temporary {label}"))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary {label}"))?;
    Ok(temporary)
}

#[cfg(unix)]
fn set_private_config_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let current = fs::metadata(path)?.permissions().mode();
    fs::set_permissions(path, fs::Permissions::from_mode(current & 0o700))
}

#[cfg(not(unix))]
fn set_private_config_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Whether the OS keychain may be probed for credentials.
/// Memoized once per process; load failure is treated as disabled (fail safe).
pub(crate) fn keychain_lookup_enabled() -> bool {
    let mut cached = KEYCHAIN_ENABLED_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(enabled) = *cached {
        return enabled;
    }
    let enabled = Config::prepare_config_file()
        .ok()
        .map(|path| effective_keychain_enabled_at(&path))
        .unwrap_or(false);
    *cached = Some(enabled);
    enabled
}

pub fn resolve_run(mut args: RunArgs, runtime: Option<RuntimeArg>) -> Result<ResolvedRun> {
    let mut config = load(runtime)?;
    crate::session::timing::prelude_lap("config-load");
    // Flag beats config in both directions; both flags together is an error.
    config.workspace.mode = resolve_workspace_mode(args.mount, args.copy, config.workspace.mode)?;
    config.git.history = args.history.into();
    config.git.include = args.include;

    let tool = args.tool.into();
    // `--pick` forces the chooser even when a default is saved, and does NOT
    // overwrite that default: choosing a provider for one box should not change
    // what every future run picks. Without it the picker only appears on a
    // first run, which is why a configured user never sees it again.
    if args.pick && !io::stdin().is_terminal() {
        bail!("--pick needs an interactive terminal; pass --provider <id> instead");
    }
    if args.pick {
        if let Some(provider_id) = pick_provider(tool)? {
            args.provider = Some(provider_id);
        }
    } else if should_show_provider_picker(
        args.provider.is_some(),
        config.defaults.get(tool).is_some(),
        io::stdin().is_terminal(),
    ) {
        if let Some(provider_id) = pick_provider(tool)? {
            set_provider_default(tool, Some(&provider_id))?;
            config.defaults = Config::load()
                .context("reloading provider default")?
                .defaults;
            args.provider = Some(provider_id);
        }
    }
    let selected_provider = resolve_provider(args.provider.as_deref(), tool, &config.defaults)?;
    crate::session::timing::prelude_lap("provider-credential");
    if matches!(tool, Tool::Pi | Tool::Omp)
        && selected_provider
            .spec
            .default_model
            .as_deref()
            .is_none_or(|model| model.trim().is_empty())
    {
        let tool_label = match tool {
            Tool::Pi => "Pi",
            Tool::Omp => "Omp",
            _ => tool.name(),
        };
        bail!(
            "provider '{}' has no default_model; add one to providers.yaml before using {tool_label}",
            selected_provider.spec.id
        );
    }
    let tool_name = tool.name();
    let gateway_image = gateway_image(&config)?;
    let capsule_image = capsule_image(&config, tool)?;
    let seeder_image = seeder_image(&config)?;
    crate::session::timing::prelude_lap("image-resolve");

    // Extra providers are OPT-IN, never automatic.
    //
    // A devbox is long-lived and the broker's route table is create-time only,
    // so routing more providers is what lets a box switch later. But routing
    // them means their keys are resident in that box for its whole life, and
    // "I don't want to bring those credentials into this box" is a legitimate,
    // common choice. So the default is the selected provider ALONE, and the
    // user asks for more explicitly:
    //
    //   (unset)          -> only the selected provider's credential
    //   --providers all  -> every provider that has a stored key
    //   --providers a,b  -> exactly those, if they have keys
    let extra_providers = match args.providers.as_deref() {
        Some(spec) => resolve_extra_providers(spec, &selected_provider.spec)?,
        None => Vec::new(),
    };

    Ok(ResolvedRun {
        config,
        extra_providers,
        selected_provider: selected_provider.spec,
        selected_credential: selected_provider.credential,
        selected_credential_source: selected_provider.credential_source,
        allow_experimental: args.yes || args.experimental,
        host_repo: env::current_dir().context("reading current directory")?,
        gateway_image,
        capsule_image,
        seeder_image,
        tool_command: tool_name.to_string(),
        tool_args: args.tool_args,
    })
}

pub fn gateway_image(config: &Config) -> Result<ImageRef> {
    let _ = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    Ok(ImageRef(env::var("CAPSCODE_GATEWAY_IMAGE").unwrap_or_else(
        |_| format!("capscode/gateway:{}", env!("CARGO_PKG_VERSION")),
    )))
}

/// The merged all-tools capsule image (devbox mode).
pub(crate) fn capsule_all_image(config: &Config) -> Result<ImageRef> {
    let resolved = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    Ok(ImageRef(
        env::var("CAPSCODE_CAPSULE_ALL_IMAGE").unwrap_or_else(|_| {
            crate::toolchain::capsule_all_image(&crate::toolchain::digest(&resolved.lock))
        }),
    ))
}

pub(crate) fn capsule_image(config: &Config, tool: Tool) -> Result<ImageRef> {
    let resolved = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    let tool_name = tool.name();
    let locked = resolved.lock.tools.get(tool_name).with_context(|| {
        format!("tool '{tool_name}' has no pinned distribution in toolchain.lock.toml")
    })?;
    let image_env = format!("CAPSCODE_{}_IMAGE", tool_name.to_ascii_uppercase());
    Ok(ImageRef(env::var(&image_env).unwrap_or_else(|_| {
        format!("capscode/capsule-{tool_name}:{}", locked.version)
    })))
}

pub(crate) fn seeder_image(config: &Config) -> Result<ImageRef> {
    let resolved = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    let (repository, _) = resolved
        .lock
        .seeder
        .image
        .rsplit_once(':')
        .context("seeder image in toolchain.lock.toml must include a tag")?;
    Ok(ImageRef(format!(
        "{repository}@{}",
        resolved.lock.seeder.digest
    )))
}

pub(crate) fn resolve_provider(
    value: Option<&str>,
    tool: Tool,
    defaults: &ProviderDefaults,
) -> Result<providers::ProviderResolution> {
    let registry = providers::load_registry()?;
    let mut resolved = providers::resolve_provider_from(
        &registry,
        value,
        tool,
        defaults,
        providers::env_value,
        read_anthropic_keychain,
        read_provider_keystore,
    )?;
    if tool == Tool::Codex && resolved.spec.id == "openai" {
        if let Some(value) = env::var("CODEX_ACCESS_TOKEN")
            .ok()
            .filter(|value| !value.is_empty())
        {
            validate_credential_line(&value, "CODEX_ACCESS_TOKEN")?;
            resolved.credential = Some(ProviderCredential::codex_access_token(SecretString::from(
                value,
            )));
            resolved.credential_source = Some("env:CODEX_ACCESS_TOKEN".to_string());
        } else if resolved.credential.is_none() {
            if let Some(codex) = cached_codex_auth()? {
                resolved.credential = Some(codex.credential);
                resolved.credential_source = Some(codex.source);
            }
        }
    }
    Ok(resolved)
}

pub(crate) fn validate_credential_line(value: &str, source: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{source} must not be empty");
    }
    if value.chars().any(char::is_control) {
        bail!("{source} contains a control character and cannot be injected safely");
    }
    Ok(())
}

fn cached_codex_auth() -> Result<Option<CodexCredential>> {
    let mut cached = CODEX_AUTH_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(value) = cached.as_ref() {
        return value.clone().map_err(anyhow::Error::msg);
    }
    let value = read_codex_auth().map_err(|error| format!("{error:#}"));
    *cached = Some(value.clone());
    value.map_err(anyhow::Error::msg)
}

fn codex_auth_file_path() -> Option<PathBuf> {
    env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .map(|home| home.join("auth.json"))
}

fn read_codex_auth() -> Result<Option<CodexCredential>> {
    let Some(path) = codex_auth_file_path() else {
        return Ok(None);
    };
    read_codex_auth_at(&path, unix_now_secs()?)
}

fn read_codex_auth_at(path: &Path, now_secs: u64) -> Result<Option<CodexCredential>> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting Codex credential file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "Codex credential path {} must be a regular file, not a symlink",
            path.display()
        );
    }
    if metadata.len() > MAX_CODEX_AUTH_FILE_BYTES {
        bail!(
            "Codex credential file {} is unexpectedly large",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "Codex credential file {} must not be accessible by group or other users (run: chmod 600 {})",
                path.display(),
                path.display()
            );
        }
    }
    let file = File::open(path)
        .with_context(|| format!("opening Codex credential file {}", path.display()))?;
    let parsed: CodexAuthFile = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parsing Codex credential file {}", path.display()))?;
    match parsed.auth_mode.as_deref() {
        Some("chatgpt") => {
            let token = parsed
                .tokens
                .map(|tokens| tokens.access_token)
                .filter(|value| !value.is_empty())
                .context(
                    "Codex ChatGPT login has no access token; run 'codex login' on the host",
                )?;
            validate_credential_line(&token, "Codex ChatGPT access token")?;
            validate_codex_token_expiry(&token, now_secs)?;
            Ok(Some(CodexCredential {
                credential: ProviderCredential::codex_access_token(SecretString::from(token)),
                source: "codex-auth:chatgpt".to_string(),
            }))
        }
        Some("apikey" | "apiKey" | "api_key") => {
            let key = parsed
                .openai_api_key
                .filter(|value| !value.is_empty())
                .context("Codex API-key login has no API key; run 'codex login --with-api-key'")?;
            validate_credential_line(&key, "Codex API key")?;
            Ok(Some(CodexCredential {
                credential: ProviderCredential::api_key(SecretString::from(key)),
                source: "codex-auth:api-key".to_string(),
            }))
        }
        Some(_) | None => Ok(None),
    }
}

fn unix_now_secs() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")
        .map(|duration| duration.as_secs())
}

fn validate_codex_token_expiry(token: &str, now_secs: u64) -> Result<()> {
    let payload = token
        .split('.')
        .nth(1)
        .context("Codex ChatGPT access token is not a JWT; run 'codex login' on the host")?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .context("Codex ChatGPT access token has an invalid JWT payload")?;
    let claims: JwtExpiry = serde_json::from_slice(&decoded)
        .context("Codex ChatGPT access token has no readable expiry")?;
    let required_until = now_secs.saturating_add(MIN_CODEX_ACCESS_TOKEN_LIFETIME_SECS);
    if claims.exp <= required_until {
        bail!(
            "Codex ChatGPT access token is expired or expires within one hour; run 'codex login' on the host to refresh it"
        );
    }
    Ok(())
}

pub(crate) fn codex_auth_status() -> Result<CodexAuthStatus> {
    let resolved = resolve_provider(Some("openai"), Tool::Codex, &ProviderDefaults::default())?;
    Ok(CodexAuthStatus {
        source: resolved.credential_source,
        kind: resolved.credential.map(|credential| credential.kind),
    })
}

pub(crate) fn resolve_provider_spec(
    value: Option<&str>,
    tool: Tool,
    defaults: &ProviderDefaults,
) -> Result<ProviderSpec> {
    let registry = providers::load_registry()?;
    providers::resolve_provider_spec_from(&registry, value, tool, defaults)
}

fn read_anthropic_keychain() -> Option<String> {
    // Fail closed: never construct a keyring::Entry unless the user opted in.
    // Entry construction / get_password is what raises the OS authorization dialog.
    keychain_gate(keychain_lookup_enabled(), || {
        let (service, account) = keychain_target().ok()?;
        cached_keychain_read(&ANTHROPIC_KEYCHAIN_CACHE, || {
            let value = keyring::Entry::new(&service, &account)
                .and_then(|entry| entry.get_password())
                .map_err(|error| error.to_string())?;
            Ok((!value.is_empty()).then(|| SecretString::from(value)))
        })
        .ok()
        .flatten()
        .map(|value| value.expose_secret().to_string())
    })
}

/// Gate a keychain probe: when disabled, return `None` without invoking `probe`.
/// Extracted so unit tests can assert the probe is skipped without touching the OS.
fn keychain_gate<T>(enabled: bool, probe: impl FnOnce() -> Option<T>) -> Option<T> {
    if !enabled {
        return None;
    }
    probe()
}

fn cached_keychain_read(
    cache: &Mutex<Option<KeychainRead>>,
    read: impl FnOnce() -> KeychainRead,
) -> KeychainRead {
    let mut cached = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(value) = cached.as_ref() {
        return value.clone();
    }
    let value = read();
    *cached = Some(value.clone());
    value
}

pub(crate) fn clear_keychain_cache() {
    *ANTHROPIC_KEYCHAIN_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

pub(crate) fn clear_keystore_cache() {
    *PROVIDER_KEYSTORE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

fn cached_provider_keystore() -> crate::keystore::KeystoreState {
    let mut cached = PROVIDER_KEYSTORE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(state) = cached.as_ref() {
        return state.clone();
    }
    let state = crate::keystore::load();
    *cached = Some(state.clone());
    state
}

fn read_provider_keystore(id: &str) -> Option<String> {
    // Gate BEFORE touching the keystore: loading constructs a keyring::Entry,
    // which is exactly what can raise the OS authorization dialog. Eagerly
    // evaluating the state as an argument would defeat the opt-in.
    if !keychain_lookup_enabled() {
        return None;
    }
    provider_keystore_value(true, cached_provider_keystore(), id)
}

/// A launch-blocking keystore problem, for the fail-fast missing-credential
/// path: a store that exists but cannot be used must not masquerade as "you
/// never configured a key". Never probes the keychain when lookup is disabled;
/// when enabled, the state is already cached by the resolution attempt.
pub(crate) fn provider_keystore_problem() -> Option<String> {
    if !keychain_lookup_enabled() {
        return None;
    }
    match cached_provider_keystore() {
        state @ (crate::keystore::KeystoreState::AccessDenied
        | crate::keystore::KeystoreState::Corrupt
        | crate::keystore::KeystoreState::VersionTooNew) => {
            Some(crate::keystore::state_error(state).to_string())
        }
        crate::keystore::KeystoreState::ItemMissing | crate::keystore::KeystoreState::Ok(_) => None,
    }
}

fn provider_keystore_value(
    enabled: bool,
    state: crate::keystore::KeystoreState,
    id: &str,
) -> Option<String> {
    keychain_gate(enabled, || match state {
        crate::keystore::KeystoreState::Ok(bundle) => {
            bundle.keys.get(id).map(|key| key.value.clone())
        }
        crate::keystore::KeystoreState::ItemMissing
        | crate::keystore::KeystoreState::AccessDenied
        | crate::keystore::KeystoreState::Corrupt
        | crate::keystore::KeystoreState::VersionTooNew => None,
    })
}

pub(crate) fn provider_keystore_status_line() -> String {
    match provider_keystore_status() {
        ProviderKeystoreStatus::Disabled => "Provider keys: [auth] keychain lookup disabled; run 'capscode key set <provider>' to store and enable one".to_string(),
        ProviderKeystoreStatus::Empty => "Provider keys: no keychain bundle".to_string(),
        ProviderKeystoreStatus::Problem(error) => match error.as_str() {
            "provider keychain is unreadable (access denied, locked, or no keychain backend); check OS keychain permissions and try again" => "Provider keys: keychain access denied".to_string(),
            "provider keychain bundle is corrupt; it was not deleted; run 'capscode key reset' to recover" => "Provider keys: keychain bundle corrupt (run 'capscode key reset')".to_string(),
            "provider keychain bundle version is too new; upgrade capscode or run 'capscode key reset'" => "Provider keys: keychain bundle version too new (upgrade capscode or run 'capscode key reset')".to_string(),
            _ => format!("Provider keys: {error}"),
        },
        ProviderKeystoreStatus::Loaded(ids) => {
            format!("Provider keys: keychain bundle loaded ({} key(s))", ids.len())
        }
    }
}

pub(crate) fn provider_keystore_status() -> ProviderKeystoreStatus {
    if !keychain_lookup_enabled() {
        return ProviderKeystoreStatus::Disabled;
    }
    match cached_provider_keystore() {
        crate::keystore::KeystoreState::ItemMissing => ProviderKeystoreStatus::Empty,
        state @ (crate::keystore::KeystoreState::AccessDenied
        | crate::keystore::KeystoreState::Corrupt
        | crate::keystore::KeystoreState::VersionTooNew) => {
            ProviderKeystoreStatus::Problem(crate::keystore::state_error(state).to_string())
        }
        crate::keystore::KeystoreState::Ok(bundle) => {
            ProviderKeystoreStatus::Loaded(bundle.keys.into_keys().collect())
        }
    }
}

pub(crate) fn should_show_provider_picker(
    has_explicit_provider: bool,
    has_persisted_default: bool,
    stdin_is_terminal: bool,
) -> bool {
    !has_explicit_provider && !has_persisted_default && stdin_is_terminal
}

pub(crate) fn pick_provider(tool: Tool) -> Result<Option<String>> {
    let registry = providers::load_registry()?;
    let compatible: Vec<&ProviderSpec> = registry
        .iter()
        .filter(|provider| provider.supports_tool(tool))
        .collect();

    let configured = |id: &str| -> bool {
        resolve_provider(Some(id), tool, &ProviderDefaults::default())
            .is_ok_and(|resolved| resolved.credential_available())
    };

    // Group the compatible providers under an endpoint-style header instead of
    // tagging every row with "(style)", so the list reads by API family. `●`
    // marks a provider whose credential is already configured.
    enum Entry<'a> {
        Header,
        Provider(&'a str),
        Custom,
        LeaveDefault,
    }
    let mut items: Vec<String> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut default_index = 0usize;
    for (style, label) in [
        (ApiStyle::OpenAI, "OpenAI-compatible"),
        (ApiStyle::Anthropic, "Anthropic-compatible"),
    ] {
        let mut group: Vec<&ProviderSpec> = compatible
            .iter()
            .copied()
            .filter(|provider| provider.api_style == style)
            .collect();
        group.sort_by(|a, b| a.display_name().cmp(b.display_name()));
        if group.is_empty() {
            continue;
        }
        items.push(format!("── {label} ──"));
        entries.push(Entry::Header);
        for provider in group {
            if default_index == 0 {
                default_index = entries.len();
            }
            items.push(format!(
                "  {} {}",
                if configured(&provider.id) {
                    '●'
                } else {
                    '○'
                },
                provider.display_name()
            ));
            entries.push(Entry::Provider(&provider.id));
        }
    }
    items.push("Custom endpoint…".to_string());
    entries.push(Entry::Custom);
    items.push("Leave default".to_string());
    entries.push(Entry::LeaveDefault);

    loop {
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("Provider for {}", tool.name()))
            .items(&items)
            .default(default_index)
            .interact()
            .context("reading provider selection")?;
        match entries[selection] {
            // Headers are labels, not choices — keep prompting.
            Entry::Header => continue,
            Entry::Provider(id) => return Ok(Some(id.to_string())),
            Entry::Custom => {
                let spec = prompt_provider_spec(true)?;
                let id = spec.id.clone();
                if !spec.supports_tool(tool) {
                    bail!(
                        "provider '{}' uses {} API style and is incompatible with tool '{}'",
                        spec.id,
                        api_style_name(spec.api_style),
                        tool.name()
                    );
                }
                providers::append_user_provider(&spec)?;
                return Ok(Some(id));
            }
            Entry::LeaveDefault => return Ok(None),
        }
    }
}

pub(crate) fn prompt_provider_spec(experimental: bool) -> Result<ProviderSpec> {
    let theme = ColorfulTheme::default();
    let id: String = Input::with_theme(&theme)
        .with_prompt("Provider id")
        .interact_text()
        .context("reading provider id")?;
    let styles = ["anthropic", "openai"];
    let style = Select::with_theme(&theme)
        .with_prompt("API style")
        .items(styles)
        .default(0)
        .interact()
        .context("reading provider API style")?;
    let api_style = if style == 0 {
        ApiStyle::Anthropic
    } else {
        ApiStyle::OpenAI
    };
    let base_url: String = Input::with_theme(&theme)
        .with_prompt("Base URL")
        .interact_text()
        .context("reading provider base URL")?;
    validate_provider_endpoint_suffix(api_style, &base_url)?;
    let auth_schemes = ["x-api-key", "bearer"];
    let auth_default = if matches!(api_style, ApiStyle::Anthropic) {
        0
    } else {
        1
    };
    let auth = Select::with_theme(&theme)
        .with_prompt("Auth scheme")
        .items(auth_schemes)
        .default(auth_default)
        .interact()
        .context("reading provider auth scheme")?;
    let key_env: String = Input::with_theme(&theme)
        .with_prompt("Credential environment variable")
        .interact_text()
        .context("reading provider credential environment variable")?;
    let default_model: String = Input::with_theme(&theme)
        .with_prompt("Default model (optional)")
        .allow_empty(true)
        .interact_text()
        .context("reading provider default model")?;
    Ok(ProviderSpec {
        id,
        label: None,
        api_style,
        base_url,
        auth: if auth == 0 {
            AuthScheme::XApiKey
        } else {
            AuthScheme::Bearer
        },
        key_env,
        default_model: (!default_model.trim().is_empty()).then_some(default_model),
        models: Vec::new(),
        experimental,
    })
}

pub(crate) fn validate_provider_endpoint_suffix(api_style: ApiStyle, base_url: &str) -> Result<()> {
    let ends_with_v1 = base_url.trim_end_matches('/').ends_with("/v1");
    match (api_style, ends_with_v1) {
        (ApiStyle::Anthropic, true) => bail!(
            "Anthropic-style provider URLs must omit a trailing /v1; Claude adds /v1/messages"
        ),
        (ApiStyle::OpenAI, false) => bail!(
            "OpenAI-compatible provider URLs must end in /v1; the client adds /chat/completions"
        ),
        _ => Ok(()),
    }
}

#[derive(Debug)]
pub(crate) struct ProviderView {
    pub spec: ProviderSpec,
    pub configured: bool,
    pub credential_source: Option<String>,
}

pub(crate) fn provider_views() -> Result<Vec<ProviderView>> {
    let registry = providers::load_registry()?;
    registry
        .iter()
        .map(|provider| {
            let tool = match provider.api_style {
                ApiStyle::Anthropic => Tool::Claude,
                ApiStyle::OpenAI => Tool::Codex,
            };
            let resolved =
                resolve_provider(Some(&provider.id), tool, &ProviderDefaults::default())?;
            let configured = resolved.credential_available();
            Ok(ProviderView {
                spec: resolved.spec,
                configured,
                credential_source: resolved.credential_source,
            })
        })
        .collect()
}

pub(crate) fn add_user_provider(spec: &ProviderSpec) -> Result<()> {
    providers::append_user_provider(spec)
}

pub(crate) fn remove_user_provider(id: &str) -> Result<()> {
    providers::remove_user_provider(id)
}

pub(crate) fn api_style_name(style: ApiStyle) -> &'static str {
    match style {
        ApiStyle::Anthropic => "anthropic",
        ApiStyle::OpenAI => "openai",
    }
}

pub(crate) fn auth_scheme_name(scheme: AuthScheme) -> &'static str {
    match scheme {
        AuthScheme::Bearer => "bearer",
        AuthScheme::XApiKey => "x-api-key",
    }
}

pub(crate) fn credential_kind_name(kind: AuthKind) -> &'static str {
    match kind {
        AuthKind::ApiKey => "API key",
        AuthKind::OAuth => "OAuth token",
        AuthKind::CodexAccessToken => "ChatGPT access token",
    }
}

fn provider_keys() -> ResolvedProviderKeys {
    provider_keys_from(
        |name| env::var(name).ok(),
        |service, account| {
            // Same opt-in gate as read_anthropic_keychain: never construct Entry
            // unless auth.keychain is true.
            if !keychain_lookup_enabled() {
                return Err("keychain lookup disabled".to_string());
            }
            cached_keychain_read(&ANTHROPIC_KEYCHAIN_CACHE, || {
                let value = keyring::Entry::new(service, account)
                    .and_then(|entry| entry.get_password())
                    .map_err(|error| error.to_string())?;
                Ok((!value.is_empty()).then(|| SecretString::from(value)))
            })?
            .map(|value| value.expose_secret().to_string())
            .ok_or_else(|| "credential not found".to_string())
        },
    )
}

pub(crate) fn anthropic_auth_status() -> AnthropicAuthStatus {
    provider_keys().anthropic
}

fn keychain_service_from(get: &impl Fn(&str) -> Option<String>) -> String {
    get(KEYCHAIN_SERVICE_ENV)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_KEYCHAIN_SERVICE.to_string())
}

fn login_user_from(get: &impl Fn(&str) -> Option<String>) -> Option<String> {
    get("USER")
        .filter(|value| !value.is_empty())
        .or_else(|| get("LOGNAME").filter(|value| !value.is_empty()))
        // Fall back to the OS's own idea of the login user when the environment
        // is stripped (launchd/GUI login items, some SSH/container contexts) — the
        // keychain item is stored under the real account name, so a missing USER
        // must not silently miss it. Tests set USER, so this branch is prod-only.
        .or_else(|| {
            whoami::fallible::username()
                .ok()
                .filter(|value| !value.is_empty())
        })
}

pub(crate) fn keychain_target() -> Result<(String, String)> {
    let get = |name: &str| env::var(name).ok();
    let service = keychain_service_from(&get);
    let account = login_user_from(&get)
        .context("cannot determine login user from USER or LOGNAME for keychain access")?;
    Ok((service, account))
}

/// Resolve provider credentials from injected environment and keychain sources.
/// Anthropic precedence is: CLAUDE_CODE_OAUTH_TOKEN, ANTHROPIC_API_KEY, then the
/// OS keychain. Explicit environment credentials therefore always win.
fn provider_keys_from<E>(
    get: impl Fn(&str) -> Option<String>,
    read_keychain: impl Fn(&str, &str) -> std::result::Result<String, E>,
) -> ResolvedProviderKeys
where
    E: std::fmt::Display,
{
    let mut keys: HashMap<Provider, ProviderCredential> = HashMap::new();
    for (provider, name) in [
        (Provider::OpenAI, "OPENAI_API_KEY"),
        (Provider::Moonshot, "MOONSHOT_API_KEY"),
    ] {
        if let Some(value) = get(name) {
            if !value.is_empty() {
                keys.insert(
                    provider,
                    ProviderCredential::api_key(SecretString::from(value)),
                );
            }
        }
    }

    let env_oauth = get("CLAUDE_CODE_OAUTH_TOKEN").filter(|value| !value.is_empty());
    let env_api_key = get("ANTHROPIC_API_KEY").filter(|value| !value.is_empty());
    let (credential, source) = if let Some(value) = env_oauth {
        (
            Some(ProviderCredential::oauth(SecretString::from(value))),
            Some("env:CLAUDE_CODE_OAUTH_TOKEN".to_string()),
        )
    } else if let Some(value) = env_api_key {
        (
            Some(ProviderCredential::api_key(SecretString::from(value))),
            Some("env:ANTHROPIC_API_KEY".to_string()),
        )
    } else {
        let service = keychain_service_from(&get);
        let value =
            login_user_from(&get).and_then(|account| match read_keychain(&service, &account) {
                Ok(value) if !value.is_empty() => Some(value),
                Ok(_) => None,
                Err(error) => {
                    debug!(service, %error, "Anthropic keychain credential unavailable");
                    None
                }
            });
        match value {
            Some(value) => (
                Some(ProviderCredential::oauth(SecretString::from(value))),
                Some(format!("keychain:{service}")),
            ),
            None => (None, None),
        }
    };

    let kind = credential.as_ref().map(|value| value.kind);
    if let Some(credential) = credential {
        keys.insert(Provider::Anthropic, credential);
    }
    ResolvedProviderKeys {
        keys: ProviderKeys(keys),
        anthropic: AnthropicAuthStatus { source, kind },
    }
}

pub fn runtime_name(pref: &RuntimePref) -> &'static str {
    match pref {
        RuntimePref::Auto => "auto",
        RuntimePref::Docker => "docker",
        RuntimePref::OrbStack => "orbstack",
    }
}

fn config_tool(key: ConfigKeyArg) -> Option<Tool> {
    match key {
        ConfigKeyArg::DefaultsClaude => Some(Tool::Claude),
        ConfigKeyArg::DefaultsCodex => Some(Tool::Codex),
        ConfigKeyArg::DefaultsOpencode => Some(Tool::Opencode),
        ConfigKeyArg::DefaultsPi => Some(Tool::Pi),
        ConfigKeyArg::DefaultsOmp => Some(Tool::Omp),
        ConfigKeyArg::AuthKeychain
        | ConfigKeyArg::BoxPrefix
        | ConfigKeyArg::CapsuleMemory
        | ConfigKeyArg::CapsulePids
        | ConfigKeyArg::CapsuleOpencodeLspDownload
        | ConfigKeyArg::CapsuleCacheVolume
        | ConfigKeyArg::Runtime
        | ConfigKeyArg::UpdateChannel
        | ConfigKeyArg::UpdateCheck
        | ConfigKeyArg::UpdateVerifyWithGh
        | ConfigKeyArg::WorkspaceMode => None,
    }
}

fn config_env_name(key: ConfigKeyArg) -> &'static str {
    match key {
        ConfigKeyArg::AuthKeychain => "CAPSCODE_AUTH__KEYCHAIN",
        ConfigKeyArg::BoxPrefix => "CAPSCODE_BOX__PREFIX",
        ConfigKeyArg::CapsuleMemory => "CAPSCODE_CAPSULE__MEMORY",
        ConfigKeyArg::CapsulePids => "CAPSCODE_CAPSULE__PIDS",
        ConfigKeyArg::CapsuleOpencodeLspDownload => "CAPSCODE_CAPSULE__OPENCODE_LSP_DOWNLOAD",
        ConfigKeyArg::CapsuleCacheVolume => "CAPSCODE_CAPSULE__CACHE_VOLUME",
        ConfigKeyArg::DefaultsClaude => "CAPSCODE_DEFAULTS__CLAUDE",
        ConfigKeyArg::DefaultsCodex => "CAPSCODE_DEFAULTS__CODEX",
        ConfigKeyArg::DefaultsOpencode => "CAPSCODE_DEFAULTS__OPENCODE",
        ConfigKeyArg::DefaultsPi => "CAPSCODE_DEFAULTS__PI",
        ConfigKeyArg::DefaultsOmp => "CAPSCODE_DEFAULTS__OMP",
        ConfigKeyArg::Runtime => "CAPSCODE_RUNTIME",
        ConfigKeyArg::UpdateChannel => "CAPSCODE_UPDATE__CHANNEL",
        ConfigKeyArg::UpdateCheck => "CAPSCODE_UPDATE__CHECK",
        ConfigKeyArg::UpdateVerifyWithGh => "CAPSCODE_UPDATE__VERIFY_WITH_GH",
        ConfigKeyArg::WorkspaceMode => "CAPSCODE_WORKSPACE__MODE",
    }
}

fn config_path_parts(key: ConfigKeyArg) -> (&'static str, Option<&'static str>) {
    match key {
        ConfigKeyArg::AuthKeychain => ("auth", Some("keychain")),
        ConfigKeyArg::BoxPrefix => ("box", Some("prefix")),
        ConfigKeyArg::CapsuleMemory => ("capsule", Some("memory")),
        ConfigKeyArg::CapsulePids => ("capsule", Some("pids")),
        ConfigKeyArg::CapsuleOpencodeLspDownload => ("capsule", Some("opencode_lsp_download")),
        ConfigKeyArg::CapsuleCacheVolume => ("capsule", Some("cache_volume")),
        ConfigKeyArg::DefaultsClaude => ("defaults", Some("claude")),
        ConfigKeyArg::DefaultsCodex => ("defaults", Some("codex")),
        ConfigKeyArg::DefaultsOpencode => ("defaults", Some("opencode")),
        ConfigKeyArg::DefaultsPi => ("defaults", Some("pi")),
        ConfigKeyArg::DefaultsOmp => ("defaults", Some("omp")),
        ConfigKeyArg::Runtime => ("runtime", None),
        ConfigKeyArg::UpdateChannel => ("update", Some("channel")),
        ConfigKeyArg::UpdateCheck => ("update", Some("check")),
        ConfigKeyArg::UpdateVerifyWithGh => ("update", Some("verify_with_gh")),
        ConfigKeyArg::WorkspaceMode => ("workspace", Some("mode")),
    }
}

enum PersistedLookup<'a> {
    Absent,
    Value(&'a toml::Value),
    InvalidParent(&'a toml::Value),
}

fn persisted_lookup<'a>(document: &'a toml::Value, key: ConfigKeyArg) -> PersistedLookup<'a> {
    let Some(root) = document.as_table() else {
        return PersistedLookup::InvalidParent(document);
    };
    let (section, leaf) = config_path_parts(key);
    let Some(section_value) = root.get(section) else {
        return PersistedLookup::Absent;
    };
    let Some(leaf) = leaf else {
        return PersistedLookup::Value(section_value);
    };
    let Some(section) = section_value.as_table() else {
        return PersistedLookup::InvalidParent(section_value);
    };
    section
        .get(leaf)
        .map_or(PersistedLookup::Absent, PersistedLookup::Value)
}

fn persisted_value(document: &toml::Value, key: ConfigKeyArg) -> Option<toml::Value> {
    match persisted_lookup(document, key) {
        PersistedLookup::Value(value) | PersistedLookup::InvalidParent(value) => {
            Some(value.clone())
        }
        PersistedLookup::Absent => None,
    }
}

fn environment_value(name: &str) -> Option<Result<String, String>> {
    env::var_os(name).map(|value| {
        value
            .into_string()
            .map_err(|_| format!("{name} contains non-UTF-8 data"))
    })
}

pub(crate) fn parse_runtime_pref(value: &str) -> Result<RuntimePref> {
    match value {
        "auto" => Ok(RuntimePref::Auto),
        "docker" => Ok(RuntimePref::Docker),
        "orbstack" => Ok(RuntimePref::OrbStack),
        _ => bail!("runtime must be exactly one of: auto, docker, orbstack"),
    }
}

pub(crate) fn parse_keychain_enabled(value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("auth.keychain must be exactly true or false"),
    }
}

pub(crate) fn parse_opencode_lsp_download(value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("capsule.opencode_lsp_download must be exactly true or false"),
    }
}

pub(crate) fn parse_update_bool(key: ConfigKeyArg, value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("{} must be exactly true or false", key.as_str()),
    }
}

/// `update.channel` is the GitHub `owner/repo` every update URL derives from.
pub(crate) fn parse_update_channel(value: &str) -> Result<String> {
    Ok(crate::update::channel::ChannelName::parse(value)?
        .as_str()
        .to_string())
}

/// Set one `<section>.<leaf>` scalar in the persisted config.
fn set_section_value_at(config_path: &Path, key: ConfigKeyArg, value: toml::Value) -> Result<()> {
    let (section_name, Some(leaf)) = config_path_parts(key) else {
        bail!("{} is not a sectioned config key", key.as_str());
    };
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    let section = root
        .entry(section_name)
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .with_context(|| format!("config key '{section_name}' must be a TOML table"))?;
    section.insert(leaf.to_string(), value);
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

/// Remove one `<section>.<leaf>` scalar, dropping the section when it becomes empty.
fn unset_section_value_at(config_path: &Path, key: ConfigKeyArg) -> Result<()> {
    let (section_name, Some(leaf)) = config_path_parts(key) else {
        bail!("{} is not a sectioned config key", key.as_str());
    };
    let mut document = read_persisted_document_at(config_path)?;
    let original = document.clone();
    let root = document
        .as_table_mut()
        .context("config file root must be a TOML table")?;
    if let Some(section) = root.get_mut(section_name) {
        let section = section
            .as_table_mut()
            .with_context(|| format!("config key '{section_name}' must be a TOML table"))?;
        if section.remove(leaf).is_some() && section.is_empty() {
            root.remove(section_name);
        }
    }
    if document == original {
        return Ok(());
    }
    write_config_document(config_path, &document)
}

pub(crate) fn parse_cache_volume(value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("capsule.cache_volume must be exactly true or false"),
    }
}

pub(crate) fn parse_workspace_mode(value: &str) -> Result<WorkspaceMode> {
    match value {
        "copy" => Ok(WorkspaceMode::Copy),
        "mount" => Ok(WorkspaceMode::Mount),
        _ => bail!("workspace.mode must be exactly one of: copy, mount"),
    }
}

pub(crate) fn parse_capsule_memory(value: &str) -> Result<MemoryLimit> {
    MemoryLimit::parse(value).map_err(anyhow::Error::msg)
}

pub(crate) fn parse_capsule_pids(value: &str) -> Result<PidsLimit> {
    PidsLimit::parse(value).map_err(anyhow::Error::msg)
}

/// A persisted `capsule.memory` is a size string, or an integer byte count.
fn persisted_capsule_memory(key: ConfigKeyArg, value: &toml::Value) -> Result<MemoryLimit> {
    match value {
        toml::Value::String(text) => parse_capsule_memory(text),
        toml::Value::Integer(bytes) => u64::try_from(*bytes)
            .map_err(|_| anyhow::anyhow!("capsule.memory must not be negative"))
            .and_then(|bytes| parse_capsule_memory(&bytes.to_string())),
        other => bail!(invalid_type(key, "a size string", other)),
    }
}

fn persisted_capsule_pids(key: ConfigKeyArg, value: &toml::Value) -> Result<PidsLimit> {
    let pids = value
        .as_integer()
        .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "an integer", value)))?;
    parse_capsule_pids(&pids.to_string())
}

/// Named tmux keys accepted as a box prefix, alone or after modifiers.
const TMUX_NAMED_KEYS: &[&str] = &[
    "Space", "Tab", "Enter", "Escape", "BSpace", "Home", "End", "IC", "DC", "PageUp", "PageDown",
    "Up", "Down", "Left", "Right", "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10",
    "F11", "F12",
];

/// Validate a tmux key name for `box.prefix`: optional `C-`/`M-`/`S-` modifiers (or a
/// leading `^`) followed by one printable ASCII character or a named key.
pub(crate) fn parse_box_prefix(value: &str) -> Result<String> {
    let invalid = || {
        anyhow::anyhow!("box.prefix must be a tmux key such as C-b, C-a, M-q or F12, got '{value}'")
    };
    let base = match value.strip_prefix('^') {
        Some(base) => base,
        None => {
            let mut rest = value;
            while let Some(stripped) = ["C-", "M-", "S-"]
                .iter()
                .find_map(|modifier| rest.strip_prefix(modifier))
            {
                rest = stripped;
            }
            rest
        }
    };
    let single_char = base.len() == 1 && base.bytes().all(|byte| byte.is_ascii_graphic());
    // DECISION: `;` is rejected because tmux reads an argument ending in `;` as a command
    // separator, and `None` is rejected because a box with no prefix cannot be detached.
    if base.contains(';') || !(single_char || TMUX_NAMED_KEYS.contains(&base)) {
        return Err(invalid());
    }
    Ok(value.to_string())
}

/// Resolve the effective workspace mode: flags beat config; both flags error.
pub(crate) fn resolve_workspace_mode(
    mount_flag: bool,
    copy_flag: bool,
    from_config: WorkspaceMode,
) -> Result<WorkspaceMode> {
    match (mount_flag, copy_flag) {
        (true, true) => bail!("--mount and --copy cannot be used together"),
        (true, false) => Ok(WorkspaceMode::Mount),
        (false, true) => Ok(WorkspaceMode::Copy),
        (false, false) => Ok(from_config),
    }
}

pub(crate) fn validate_provider_default(tool: Tool, provider_id: &str) -> Result<ProviderSpec> {
    if provider_id.trim().is_empty() {
        bail!(
            "{} provider id must not be empty or whitespace-only",
            config_key_for_tool(tool)
        );
    }
    let spec = resolve_provider_spec(Some(provider_id), tool, &ProviderDefaults::default())?;
    if matches!(tool, Tool::Pi | Tool::Omp)
        && spec
            .default_model
            .as_deref()
            .is_none_or(|model| model.trim().is_empty())
    {
        bail!(
            "provider '{}' has no non-empty default_model; {} requires one",
            spec.id,
            tool.name()
        );
    }
    Ok(spec)
}

fn config_key_for_tool(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "defaults.claude",
        Tool::Codex => "defaults.codex",
        Tool::Opencode => "defaults.opencode",
        Tool::Pi => "defaults.pi",
        Tool::Omp => "defaults.omp",
    }
}

fn built_in_provider_id(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Claude => Some("anthropic"),
        Tool::Codex => Some("openai"),
        Tool::Opencode | Tool::Pi | Tool::Omp => None,
    }
}

fn invalid_type(key: ConfigKeyArg, expected: &str, value: &toml::Value) -> String {
    format!(
        "{} must be {expected}, got {}",
        key.as_str(),
        toml_type_name(value)
    )
}

fn toml_type_name(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

fn resolve_config_text(key: ConfigKeyArg, value: &str) -> Result<Option<ConfigScalar>> {
    match key {
        ConfigKeyArg::AuthKeychain => Ok(Some(ConfigScalar::Bool(parse_keychain_enabled(value)?))),
        ConfigKeyArg::BoxPrefix => Ok(Some(ConfigScalar::String(parse_box_prefix(value)?))),
        ConfigKeyArg::CapsuleMemory => Ok(Some(ConfigScalar::String(
            parse_capsule_memory(value)?.to_string(),
        ))),
        ConfigKeyArg::CapsulePids => Ok(Some(ConfigScalar::Integer(i64::from(
            parse_capsule_pids(value)?.get(),
        )))),
        ConfigKeyArg::CapsuleOpencodeLspDownload => Ok(Some(ConfigScalar::Bool(
            parse_opencode_lsp_download(value)?,
        ))),
        ConfigKeyArg::CapsuleCacheVolume => {
            Ok(Some(ConfigScalar::Bool(parse_cache_volume(value)?)))
        }
        ConfigKeyArg::UpdateCheck | ConfigKeyArg::UpdateVerifyWithGh => {
            Ok(Some(ConfigScalar::Bool(parse_update_bool(key, value)?)))
        }
        ConfigKeyArg::UpdateChannel => Ok(Some(ConfigScalar::String(parse_update_channel(value)?))),
        ConfigKeyArg::Runtime => Ok(Some(ConfigScalar::String(
            runtime_name(&parse_runtime_pref(value)?).to_string(),
        ))),
        ConfigKeyArg::WorkspaceMode => Ok(Some(ConfigScalar::String(
            parse_workspace_mode(value)?.as_str().to_string(),
        ))),
        _ => {
            let tool = config_tool(key).expect("provider key has a tool");
            let spec = validate_provider_default(tool, value)?;
            Ok(Some(ConfigScalar::String(spec.id)))
        }
    }
}

fn resolve_config_persisted(
    key: ConfigKeyArg,
    value: &toml::Value,
) -> Result<Option<ConfigScalar>> {
    match key {
        ConfigKeyArg::AuthKeychain => value
            .as_bool()
            .map(|value| Some(ConfigScalar::Bool(value)))
            .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a boolean", value))),
        ConfigKeyArg::CapsuleMemory => persisted_capsule_memory(key, value)
            .map(|memory| Some(ConfigScalar::String(memory.to_string()))),
        ConfigKeyArg::CapsulePids => persisted_capsule_pids(key, value)
            .map(|pids| Some(ConfigScalar::Integer(i64::from(pids.get())))),
        ConfigKeyArg::CapsuleOpencodeLspDownload => value
            .as_bool()
            .map(|enabled| Some(ConfigScalar::Bool(enabled)))
            .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a boolean", value))),
        ConfigKeyArg::CapsuleCacheVolume
        | ConfigKeyArg::UpdateCheck
        | ConfigKeyArg::UpdateVerifyWithGh => value
            .as_bool()
            .map(|enabled| Some(ConfigScalar::Bool(enabled)))
            .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a boolean", value))),
        ConfigKeyArg::BoxPrefix
        | ConfigKeyArg::Runtime
        | ConfigKeyArg::UpdateChannel
        | ConfigKeyArg::WorkspaceMode => value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a string", value)))
            .and_then(|value| resolve_config_text(key, value)),
        _ => value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a string", value)))
            .and_then(|value| resolve_config_text(key, value)),
    }
}

fn resolve_config_row(document: &toml::Value, key: ConfigKeyArg) -> ConfigRow {
    let persisted = persisted_value(document, key);
    let lookup = persisted_lookup(document, key);
    let env_name = config_env_name(key);
    let env_value = environment_value(env_name);
    let source = if env_value.is_some() {
        format!("env:{env_name}")
    } else if !matches!(lookup, PersistedLookup::Absent) {
        "file".to_string()
    } else {
        "built-in".to_string()
    };

    let resolution = if let Some(env_value) = env_value {
        match env_value {
            Ok(value) => resolve_config_text(key, &value),
            Err(error) => Err(anyhow::anyhow!(error)),
        }
    } else {
        match lookup {
            PersistedLookup::Value(value) => resolve_config_persisted(key, value),
            PersistedLookup::InvalidParent(value) => Err(anyhow::anyhow!(
                "{} has a non-table parent ({})",
                key.as_str(),
                toml_type_name(value)
            )),
            PersistedLookup::Absent => match config_tool(key).and_then(built_in_provider_id) {
                Some(provider_id) => resolve_config_text(key, provider_id),
                None => match key {
                    ConfigKeyArg::AuthKeychain => Ok(Some(ConfigScalar::Bool(false))),
                    ConfigKeyArg::BoxPrefix => Ok(Some(ConfigScalar::String(
                        capscode_core::DEFAULT_BOX_PREFIX.to_string(),
                    ))),
                    ConfigKeyArg::CapsulePids => Ok(Some(ConfigScalar::Integer(i64::from(
                        capscode_core::DEFAULT_PIDS_LIMIT,
                    )))),
                    ConfigKeyArg::CapsuleOpencodeLspDownload => Ok(Some(ConfigScalar::Bool(true))),
                    ConfigKeyArg::CapsuleCacheVolume => Ok(Some(ConfigScalar::Bool(true))),
                    ConfigKeyArg::UpdateCheck => Ok(Some(ConfigScalar::Bool(true))),
                    ConfigKeyArg::UpdateVerifyWithGh => Ok(Some(ConfigScalar::Bool(false))),
                    ConfigKeyArg::UpdateChannel => Ok(Some(ConfigScalar::String(
                        capscode_core::DEFAULT_UPDATE_CHANNEL.to_string(),
                    ))),
                    ConfigKeyArg::Runtime => Ok(Some(ConfigScalar::String("auto".to_string()))),
                    ConfigKeyArg::WorkspaceMode => {
                        Ok(Some(ConfigScalar::String("copy".to_string())))
                    }
                    _ => Ok(None),
                },
            },
        }
    };
    match resolution {
        Ok(effective) => ConfigRow {
            key,
            persisted,
            effective,
            source,
            error: None,
        },
        Err(error) => ConfigRow {
            key,
            persisted,
            effective: None,
            source,
            error: Some(error.to_string()),
        },
    }
}

pub(crate) fn config_rows_at(config_path: &Path) -> Result<Vec<ConfigRow>> {
    let document = read_persisted_document_at(config_path)?;
    Ok(ConfigKeyArg::all()
        .into_iter()
        .map(|key| resolve_config_row(&document, key))
        .collect())
}

fn persisted_value_for_get(document: &toml::Value, key: ConfigKeyArg) -> Result<ConfigScalar> {
    match persisted_lookup(document, key) {
        // DECISION: an unset key still exits 1 (as `git config` does), but says where the
        // effective default is shown.
        PersistedLookup::Absent => bail!(
            "{} is unset in the config file; `capscode config list` shows its effective value",
            key.as_str()
        ),
        PersistedLookup::InvalidParent(value) => bail!(
            "{} has a non-table parent ({})",
            key.as_str(),
            toml_type_name(value)
        ),
        PersistedLookup::Value(value) => match key {
            ConfigKeyArg::AuthKeychain => value
                .as_bool()
                .map(ConfigScalar::Bool)
                .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a boolean", value))),
            ConfigKeyArg::Runtime => value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a string", value)))
                .and_then(|value| {
                    parse_runtime_pref(value)?;
                    Ok(ConfigScalar::String(value.to_string()))
                }),
            ConfigKeyArg::WorkspaceMode => value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a string", value)))
                .and_then(|value| {
                    parse_workspace_mode(value)?;
                    Ok(ConfigScalar::String(value.to_string()))
                }),
            ConfigKeyArg::BoxPrefix => value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a string", value)))
                .and_then(|value| Ok(ConfigScalar::String(parse_box_prefix(value)?))),
            ConfigKeyArg::CapsuleMemory => {
                persisted_capsule_memory(key, value)?;
                Ok(match value {
                    toml::Value::Integer(bytes) => ConfigScalar::Integer(*bytes),
                    other => ConfigScalar::String(other.as_str().unwrap_or_default().to_string()),
                })
            }
            ConfigKeyArg::CapsulePids => persisted_capsule_pids(key, value)
                .map(|pids| ConfigScalar::Integer(i64::from(pids.get()))),
            ConfigKeyArg::CapsuleOpencodeLspDownload => value
                .as_bool()
                .map(ConfigScalar::Bool)
                .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a boolean", value))),
            ConfigKeyArg::CapsuleCacheVolume
            | ConfigKeyArg::UpdateCheck
            | ConfigKeyArg::UpdateVerifyWithGh => value
                .as_bool()
                .map(ConfigScalar::Bool)
                .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a boolean", value))),
            ConfigKeyArg::UpdateChannel => value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a string", value)))
                .and_then(|value| Ok(ConfigScalar::String(parse_update_channel(value)?))),
            _ => value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a string", value)))
                .and_then(|value| {
                    if value.trim().is_empty() {
                        bail!(
                            "{} provider id must not be empty or whitespace-only",
                            key.as_str()
                        );
                    }
                    Ok(ConfigScalar::String(value.to_string()))
                }),
        },
    }
}

pub(crate) fn config_list(json: bool) -> Result<()> {
    let config_path = Config::prepare_config_file().context("preparing config home")?;
    let rows = config_rows_at(&config_path)?;
    let has_errors = rows.iter().any(|row| row.error.is_some());
    if json {
        let mut object = serde_json::Map::new();
        for row in &rows {
            let persisted = row
                .persisted
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .context("serializing persisted config value")?;
            let effective = row.effective.as_ref().map(config_scalar_json);
            object.insert(
                row.key.as_str().to_string(),
                serde_json::json!({
                    "persisted": persisted,
                    "effective": effective,
                    "source": row.source,
                    "error": row.error,
                }),
            );
        }
        data(format!("{}", serde_json::Value::Object(object)));
    } else {
        data(format_config_table(&rows).to_string());
    }
    if has_errors {
        bail!("config list: one or more effective values are invalid");
    }
    Ok(())
}

fn format_config_table(rows: &[ConfigRow]) -> String {
    let mut cells = Vec::new();
    for row in rows {
        let persisted = row.persisted.as_ref().map_or_else(
            || "-".to_string(),
            |value| match (row.key, value) {
                (
                    ConfigKeyArg::CapsuleMemory | ConfigKeyArg::CapsulePids,
                    toml::Value::Integer(number),
                ) => number.to_string(),
                _ => format_persisted_text(value),
            },
        );
        let effective = match (&row.error, &row.effective) {
            (Some(error), _) => format!("ERROR: {error}"),
            (None, Some(value)) => format_config_scalar(value),
            (None, None) => "-".to_string(),
        };
        cells.push(vec![
            row.key.as_str().to_string(),
            persisted,
            effective,
            row.source.clone(),
        ]);
    }
    crate::ui::table(
        &["KEY", "PERSISTED", "EFFECTIVE", "SOURCE"],
        &cells,
        crate::ui::Caps::PLAIN,
    )
}

fn config_scalar_json(value: &ConfigScalar) -> serde_json::Value {
    match value {
        ConfigScalar::Bool(value) => serde_json::Value::Bool(*value),
        ConfigScalar::Integer(value) => serde_json::Value::from(*value),
        ConfigScalar::String(value) => serde_json::Value::String(value.clone()),
    }
}

fn format_config_scalar(value: &ConfigScalar) -> String {
    match value {
        ConfigScalar::Bool(value) => value.to_string(),
        ConfigScalar::Integer(value) => value.to_string(),
        ConfigScalar::String(value) => value.clone(),
    }
}

fn format_persisted_text(value: &toml::Value) -> String {
    match value {
        toml::Value::String(value) => value.clone(),
        toml::Value::Boolean(value) => value.to_string(),
        _ => format!("INVALID({value})"),
    }
}

pub(crate) fn config_get(key: ConfigKeyArg) -> Result<()> {
    let config_path = Config::prepare_config_file().context("preparing config home")?;
    let document = read_persisted_document_at(&config_path)?;
    let value = persisted_value_for_get(&document, key)?;
    data(format_config_scalar(&value).to_string());
    Ok(())
}

pub(crate) fn config_set(key: ConfigKeyArg, value: &str) -> Result<()> {
    let config_path = Config::prepare_config_file().context("preparing config home")?;
    match key {
        ConfigKeyArg::AuthKeychain => {
            let enabled = parse_keychain_enabled(value)?;
            set_keychain_enabled_at(&config_path, enabled)?;
            refresh_keychain_enabled_cache_at(&config_path);
        }
        ConfigKeyArg::BoxPrefix => set_box_prefix_at(&config_path, value)?,
        ConfigKeyArg::CapsuleMemory | ConfigKeyArg::CapsulePids => {
            set_capsule_limit_at(&config_path, key, value)?
        }
        ConfigKeyArg::CapsuleOpencodeLspDownload => {
            set_capsule_opencode_lsp_download_at(&config_path, parse_opencode_lsp_download(value)?)?
        }
        ConfigKeyArg::CapsuleCacheVolume => {
            set_capsule_cache_volume_at(&config_path, parse_cache_volume(value)?)?
        }
        ConfigKeyArg::UpdateCheck | ConfigKeyArg::UpdateVerifyWithGh => set_section_value_at(
            &config_path,
            key,
            toml::Value::Boolean(parse_update_bool(key, value)?),
        )?,
        ConfigKeyArg::UpdateChannel => set_section_value_at(
            &config_path,
            key,
            toml::Value::String(parse_update_channel(value)?),
        )?,
        ConfigKeyArg::Runtime => set_runtime_at(&config_path, value)?,
        ConfigKeyArg::WorkspaceMode => {
            let mode = parse_workspace_mode(value)?;
            set_workspace_mode_at(&config_path, mode)?;
        }
        _ => set_provider_default_at(
            &config_path,
            config_tool(key).expect("provider key"),
            Some(value),
        )?,
    }
    Ok(())
}

pub(crate) fn config_unset(key: ConfigKeyArg) -> Result<()> {
    let config_path = Config::prepare_config_file().context("preparing config home")?;
    match key {
        ConfigKeyArg::AuthKeychain => {
            unset_keychain_enabled_at(&config_path)?;
            refresh_keychain_enabled_cache_at(&config_path);
        }
        ConfigKeyArg::BoxPrefix => unset_box_prefix_at(&config_path)?,
        ConfigKeyArg::CapsuleMemory | ConfigKeyArg::CapsulePids => {
            unset_capsule_limit_at(&config_path, key)?
        }
        ConfigKeyArg::CapsuleOpencodeLspDownload => {
            unset_capsule_opencode_lsp_download_at(&config_path)?
        }
        ConfigKeyArg::CapsuleCacheVolume => unset_capsule_cache_volume_at(&config_path)?,
        ConfigKeyArg::UpdateChannel
        | ConfigKeyArg::UpdateCheck
        | ConfigKeyArg::UpdateVerifyWithGh => unset_section_value_at(&config_path, key)?,
        ConfigKeyArg::Runtime => unset_runtime_at(&config_path)?,
        ConfigKeyArg::WorkspaceMode => unset_workspace_mode_at(&config_path)?,
        _ => set_provider_default_at(&config_path, config_tool(key).expect("provider key"), None)?,
    }
    Ok(())
}

fn schema_children(path: &str) -> Option<Vec<&'static str>> {
    match path {
        "image" => Some(vec!["toolchain_lock", "pull_policy"]),
        "network" => Some(vec![
            "wireguard_conf",
            "enforce_vpn",
            "telemetry_sinkhole",
            "extra_blocked_cidrs",
        ]),
        "broker" => Some(vec!["enabled", "allow_hosts"]),
        "git" => Some(vec!["history", "include", "mount"]),
        "profile" => Some(vec!["include"]),
        "defaults" => Some(Tool::ALL.iter().map(|tool| tool.name()).collect()),
        "auth" => Some(vec!["keychain"]),
        "workspace" => Some(vec!["mode"]),
        "box" => Some(vec!["prefix"]),
        "capsule" => Some(vec![
            "memory",
            "pids",
            "opencode_lsp_download",
            "cache_volume",
        ]),
        "update" => Some(vec!["check", "channel", "verify_with_gh"]),
        _ => None,
    }
}

fn validate_table_keys(
    table: &toml::map::Map<String, toml::Value>,
    path: &str,
    allowed: &[&str],
) -> Result<()> {
    for (key, value) in table {
        if !allowed.contains(&key.as_str()) {
            let full = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            bail!("unknown config key '{full}'")
        }
        let full = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        if let Some(children) = schema_children(&full) {
            if let Some(child_table) = value.as_table() {
                validate_table_keys(child_table, &full, &children)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_unknown_config_keys(document: &toml::Value) -> Result<()> {
    let root = document
        .as_table()
        .context("config file root must be a TOML table")?;
    validate_table_keys(
        root,
        "",
        &[
            "runtime",
            "image",
            "network",
            "broker",
            "git",
            "profile",
            "defaults",
            "auth",
            "workspace",
            "box",
            "capsule",
            "update",
        ],
    )
}

fn validate_persisted_provider_defaults(document: &toml::Value) -> Result<()> {
    for (key, tool) in [
        (ConfigKeyArg::DefaultsClaude, Tool::Claude),
        (ConfigKeyArg::DefaultsCodex, Tool::Codex),
        (ConfigKeyArg::DefaultsOpencode, Tool::Opencode),
        (ConfigKeyArg::DefaultsPi, Tool::Pi),
        (ConfigKeyArg::DefaultsOmp, Tool::Omp),
    ] {
        if let PersistedLookup::Value(value) = persisted_lookup(document, key) {
            let provider_id = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!(invalid_type(key, "a string", value)))?;
            validate_provider_default(tool, provider_id)
                .with_context(|| format!("validating {}", key.as_str()))?;
        }
    }
    Ok(())
}

/// Validate an edited file against the complete typed Config schema without
/// consulting CAPSCODE_* environment variables or any credential source.
pub(crate) fn validate_config_document(contents: &str) -> Result<Config> {
    let document = toml::from_str::<toml::Value>(contents).context("parsing config TOML")?;
    validate_unknown_config_keys(&document)?;
    let config: Config = Figment::from(Serialized::defaults(Config::default()))
        .merge(Toml::string(contents))
        .extract()
        .context("validating typed config")?;
    validate_persisted_provider_defaults(&document)?;
    parse_update_channel(&config.update.channel).context("validating update.channel")?;
    Ok(config)
}

fn effective_keychain_enabled_at(config_path: &Path) -> bool {
    if let Some(value) = environment_value("CAPSCODE_AUTH__KEYCHAIN") {
        return value
            .ok()
            .and_then(|value| parse_keychain_enabled(&value).ok())
            .unwrap_or(false);
    }
    let Ok(document) = read_persisted_document_at(config_path) else {
        return false;
    };
    match persisted_lookup(&document, ConfigKeyArg::AuthKeychain) {
        PersistedLookup::Value(value) => value.as_bool().unwrap_or(false),
        PersistedLookup::Absent | PersistedLookup::InvalidParent(_) => false,
    }
}

fn refresh_keychain_enabled_cache_at(config_path: &Path) {
    let enabled = effective_keychain_enabled_at(config_path);
    *KEYCHAIN_ENABLED_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(enabled);
}

fn editor_command(editor: Option<&str>) -> (String, Vec<String>) {
    let mut tokens = editor
        .unwrap_or_default()
        .split_ascii_whitespace()
        .map(str::to_string);
    let executable = tokens.next().unwrap_or_else(|| "vi".to_string());
    (executable, tokens.collect())
}

pub(crate) fn config_edit() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("config edit requires an interactive terminal")
    }
    let config_path = Config::prepare_config_file().context("preparing config home")?;
    let editor = env::var("EDITOR").ok();
    let (executable, arguments) = editor_command(editor.as_deref());
    let status = match Command::new(&executable)
        .args(arguments)
        .arg(&config_path)
        .status()
    {
        Ok(status) => status,
        Err(error) => return Err(error).with_context(|| format!("spawning editor '{executable}'")),
    };
    if !status.success() {
        refresh_keychain_enabled_cache_to_false();
        bail!("editor '{executable}' exited unsuccessfully: {status}")
    }
    let contents = match fs::read_to_string(&config_path) {
        Ok(contents) => contents,
        Err(error) => {
            refresh_keychain_enabled_cache_to_false();
            return Err(error)
                .with_context(|| format!("reading edited config file {}", config_path.display()));
        }
    };
    if let Err(error) = validate_config_document(&contents) {
        refresh_keychain_enabled_cache_to_false();
        bail!("{}", invalid_config_edit_message(&error))
    }
    refresh_keychain_enabled_cache_at(&config_path);
    data("config OK");
    Ok(())
}

fn invalid_config_edit_message(error: &anyhow::Error) -> String {
    format!(
        "config invalid; the file was left as edited and must be fixed: {error:#}; keychain lookup fails safe to disabled until the config is valid"
    )
}

fn refresh_keychain_enabled_cache_to_false() {
    *KEYCHAIN_ENABLED_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(false);
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

/// Parse `--providers` into the extra credential set.
///
/// Errors on an unknown id rather than silently routing less than asked: a
/// typo that quietly drops a provider would show up much later as "why can't
/// this box reach X".
fn resolve_extra_providers(
    spec: &str,
    selected: &ProviderSpec,
) -> Result<Vec<(ProviderSpec, ProviderCredential)>> {
    let available = extra_keyed_providers(selected);
    if spec.eq_ignore_ascii_case("all") {
        return Ok(available);
    }
    if spec.eq_ignore_ascii_case("none") || spec.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for wanted in spec.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        if wanted == selected.id {
            continue; // already routed as the selected provider
        }
        match available.iter().find(|(spec, _)| spec.id == wanted) {
            Some(found) => out.push(found.clone()),
            None => bail!(
                "provider '{wanted}' has no stored credential, so it cannot be routed; \
                 store one with `capscode key set {wanted}` or drop it from --providers"
            ),
        }
    }
    Ok(out)
}

/// Every registry provider — other than `selected` — that has a usable stored
/// credential right now.
///
/// Only key-bearing providers are routed: an unconfigured provider is never
/// exposed to the capsule, which keeps the blast radius to keys the user has
/// actually set up. Resolution failures are skipped silently; a provider that
/// cannot produce a credential simply is not routed.
fn extra_keyed_providers(selected: &ProviderSpec) -> Vec<(ProviderSpec, ProviderCredential)> {
    let registry = match providers::load_registry() {
        Ok(registry) => registry,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for spec in registry {
        if spec.id == selected.id {
            continue;
        }
        // Env wins over the keystore, exactly as the normal resolution path
        // does, so a box routes the same credential the user would get.
        let secret: Option<String> = std::env::var(&spec.key_env)
            .ok()
            .filter(|value: &String| !value.is_empty())
            .or_else(|| read_provider_keystore(&spec.id));
        if let Some(secret) = secret.filter(|value: &String| !value.is_empty()) {
            out.push((
                spec,
                ProviderCredential::api_key(secrecy::SecretString::from(secret)),
            ));
        }
    }
    out
}

/// Providers whose credentials are available to route into a new box.
/// Environment values take precedence over the encrypted store; Anthropic's
/// setup token keychain item is considered after those sources.
pub(crate) fn box_candidates() -> Result<Vec<(ProviderSpec, ProviderCredential)>> {
    let registry = providers::load_registry().context("loading provider registry")?;
    let anthropic = provider_keys().keys.0.get(&Provider::Anthropic).cloned();
    // Codex users signed in with ChatGPT have no OpenAI API key; their login is the
    // credential for the `openai` route, exactly as on the one-shot path.
    let codex_login = match env::var("CODEX_ACCESS_TOKEN")
        .ok()
        .filter(|value| !value.is_empty())
    {
        Some(value) => {
            validate_credential_line(&value, "CODEX_ACCESS_TOKEN")?;
            Some(ProviderCredential::codex_access_token(SecretString::from(
                value,
            )))
        }
        None => cached_codex_auth()?.map(|codex| codex.credential),
    };
    Ok(box_candidates_from(
        registry,
        |name| env::var(name).ok(),
        read_provider_keystore,
        anthropic,
        codex_login,
    ))
}

fn box_candidates_from<E, K>(
    registry: Vec<ProviderSpec>,
    get_env: E,
    mut read_keystore: K,
    anthropic: Option<ProviderCredential>,
    codex_login: Option<ProviderCredential>,
) -> Vec<(ProviderSpec, ProviderCredential)>
where
    E: Fn(&str) -> Option<String>,
    K: FnMut(&str) -> Option<String>,
{
    let mut out = Vec::new();
    for spec in registry {
        let oauth = if spec.id == "anthropic" {
            get_env("CLAUDE_CODE_OAUTH_TOKEN").filter(|value| !value.is_empty())
        } else {
            None
        };
        let value = oauth
            .clone()
            .or_else(|| get_env(&spec.key_env).filter(|value| !value.is_empty()))
            .or_else(|| read_keystore(&spec.id).filter(|value| !value.is_empty()));
        let credential = value
            .map(|value| {
                if oauth.is_some() {
                    ProviderCredential::oauth(SecretString::from(value))
                } else {
                    ProviderCredential::api_key(SecretString::from(value))
                }
            })
            .or_else(|| {
                (spec.id == "anthropic")
                    .then(|| anthropic.clone())
                    .flatten()
            })
            .or_else(|| (spec.id == "openai").then(|| codex_login.clone()).flatten());
        if let Some(credential) = credential {
            out.push((spec, credential));
        }
    }
    out
}

/// Resolve the configured provider id for each tool, using persisted defaults
/// before the built-in Claude/OpenAI choices. Environment overrides are not
/// persisted box identity and therefore do not affect this mapping.
pub(crate) fn tool_defaults() -> Result<BTreeMap<Tool, String>> {
    let document = read_persisted_document_at(&Config::config_file_path())?;
    let registry = providers::load_registry().context("loading provider registry")?;
    tool_defaults_from(&document, &registry)
}

fn tool_defaults_from(
    document: &toml::Value,
    registry: &[ProviderSpec],
) -> Result<BTreeMap<Tool, String>> {
    validate_persisted_provider_defaults(document)?;
    let persisted = document.get("defaults").and_then(toml::Value::as_table);
    let mut defaults = BTreeMap::new();
    for tool in Tool::ALL {
        let provider_id = persisted
            .and_then(|values| values.get(tool.name()))
            .and_then(toml::Value::as_str)
            .or_else(|| providers::built_in_default(tool));
        let Some(provider_id) = provider_id else {
            continue;
        };
        let spec = providers::resolve_provider_spec_from(
            registry,
            Some(provider_id),
            tool,
            &ProviderDefaults::default(),
        )?;
        defaults.insert(tool, spec.id);
    }
    Ok(defaults)
}

#[cfg(test)]
mod capsule_all_tag_tests {
    /// Pins the digest algorithm so `scripts/build-images.sh` and this crate
    /// cannot drift into tagging and resolving different images. The expected
    /// value is the sha256 of the canonical listing, produced by the SAME shell
    /// pipeline the build script uses.
    #[test]
    fn toolchain_digest_is_stable_and_shell_compatible() {
        let lock = crate::toolchain::builtin_lock().unwrap();
        assert_eq!(crate::toolchain::digest(&lock), "fb24882402b5");
    }

    #[test]
    fn shipped_toolchain_lock_parses_with_smoke_metadata() {
        let lock =
            crate::toolchain::parse_validated(include_str!("../../../images/toolchain.lock.toml"))
                .expect("shipped toolchain lock parses");
        for name in ["claude", "codex", "opencode", "pi", "omp"] {
            assert!(lock.tools.contains_key(name), "missing tool {name}");
            assert!(
                !lock.tools[name].version.is_empty(),
                "empty tool version for {name}"
            );
        }
    }
}
