//! Layered configuration model .
//!
//! Precedence, low to high: built-in defaults -> `$CAPSCODE_HOME/config.toml` (or
//! `$HOME/.capscode/config.toml`) -> `CAPSCODE_*` env vars -> CLI flags. `SecretString`
//! fields never appear in `Debug`/serialized output.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::ids::{Provider, Tool};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimePref {
    #[default]
    Auto,
    Docker,
    OrbStack,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PullPolicy {
    #[default]
    IfMissing,
    Always,
    Never,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImageConfig {
    pub toolchain_lock: Option<PathBuf>,
    pub pull_policy: PullPolicy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryDial {
    #[default]
    Full,
    Shallow,
    RedactAuthors,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub runtime: RuntimePref,
    pub image: ImageConfig,
    pub network: NetworkConfig,
    pub broker: BrokerConfig,
    pub git: GitConfig,
    /// Profile allowlist; empty by default.
    pub profile: ProfileConfig,
    /// Persisted provider choices, keyed by tool.
    pub defaults: ProviderDefaults,
    /// Credential source preferences (keychain opt-in, etc.).
    pub auth: AuthConfig,
    /// How the host project is presented to the capsule.
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    /// Persistent box settings.
    #[serde(default)]
    pub r#box: BoxConfig,
    /// Resource limits for the capsule container.
    #[serde(default)]
    pub capsule: CapsuleConfig,
    /// Update channel and the daily update check.
    #[serde(default)]
    pub update: UpdateConfig,
}

/// Default GitHub repository that publishes `channel.json` and toolchain releases.
pub const DEFAULT_UPDATE_CHANNEL: &str = "sayre4ux/capscode";

/// Update channel settings. The channel is the `owner/repo` every update URL is
/// derived from, so a fork can take over publishing by changing this one value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Run the passive once-a-day update check.
    pub check: bool,
    /// GitHub `owner/repo` that serves the release channel.
    pub channel: String,
    /// Also run `gh attestation verify` on a downloaded toolchain lock.
    pub verify_with_gh: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check: true,
            channel: DEFAULT_UPDATE_CHANNEL.to_string(),
            verify_with_gh: false,
        }
    }
}

/// Whether the host project is copied into the capsule or bind-mounted live.
///
/// `copy` (default) seeds a capsule-local workspace and copy-back on exit.
/// `mount` bind-mounts the host directory: the agent writes to real files with
/// no ingress/egress copy and no copy-back review. That trade is never silent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMode {
    #[default]
    Copy,
    Mount,
}

impl WorkspaceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Mount => "mount",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub mode: WorkspaceMode,
}

/// Default tmux prefix for the box shell: tmux's own default.
pub const DEFAULT_BOX_PREFIX: &str = "C-b";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BoxConfig {
    /// tmux prefix key for the box shell, in tmux key syntax (`C-b`, `C-a`, `M-q`).
    pub prefix: String,
}

impl Default for BoxConfig {
    fn default() -> Self {
        BoxConfig {
            prefix: DEFAULT_BOX_PREFIX.to_string(),
        }
    }
}

/// Process/thread cap applied to every CapsCode container unless `capsule.pids`
/// overrides it for the capsule. Enough for node/npm, a headless browser, and a
/// parallel cargo build; low enough that a fork bomb stops at the cgroup.
// DECISION: the short-lived seeder and box-checker helpers get this cap too; they
// handle untrusted workspace content and the cap costs them nothing.
pub const DEFAULT_PIDS_LIMIT: u32 = 1024;

/// Capsule resource limits. Memory is unlimited unless set; the pids cap always
/// applies and defaults to [`DEFAULT_PIDS_LIMIT`].
// DECISION: a new `[capsule]` section (`capsule.memory`, `capsule.pids`) because
// the limits apply to one-shot sessions and boxes alike, so `box.*` would mislead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CapsuleConfig {
    /// Memory cap (`512m`, `4g`, or bytes). Swap is capped to the same value.
    pub memory: Option<MemoryLimit>,
    /// Process/thread cap for the capsule.
    pub pids: Option<PidsLimit>,
    /// Allow OpenCode to download language servers into the executable `/tmp` cache.
    pub opencode_lsp_download: bool,
    /// Keep each box's helper caches in its own persistent Docker volume.
    pub cache_volume: bool,
}

impl Default for CapsuleConfig {
    fn default() -> Self {
        Self {
            memory: None,
            pids: None,
            opencode_lsp_download: true,
            cache_volume: true,
        }
    }
}

impl CapsuleConfig {
    /// The pids cap the capsule runs with.
    pub fn pids_limit(&self) -> u32 {
        self.pids.map_or(DEFAULT_PIDS_LIMIT, PidsLimit::get)
    }

    /// The memory cap in bytes, or `None` for no limit.
    pub fn memory_bytes(&self) -> Option<u64> {
        self.memory.map(MemoryLimit::bytes)
    }
}

/// A validated capsule memory cap in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SizeInput", into = "String")]
pub struct MemoryLimit(u64);

/// Docker refuses a container memory limit below 6 MiB.
// DECISION: no config size format existed, so this follows Docker's `--memory`
// (binary k/m/g, bare bytes) and rejects Docker's sub-6 MiB floor up front.
const MIN_MEMORY_BYTES: u64 = 6 * 1024 * 1024;

impl MemoryLimit {
    /// Parse `<n>`, `<n>k`, `<n>m`, or `<n>g` (binary units, case-insensitive; a
    /// bare number is bytes), as Docker's `--memory` does.
    pub fn parse(value: &str) -> Result<Self, String> {
        let invalid = || format!("capsule.memory must be a size such as 512m or 4g, got '{value}'");
        let trimmed = value.trim();
        let (digits, multiplier) = match trimmed.char_indices().last() {
            Some((index, unit)) if unit.is_ascii_alphabetic() => {
                let multiplier: u64 = match unit.to_ascii_lowercase() {
                    'k' => 1 << 10,
                    'm' => 1 << 20,
                    'g' => 1 << 30,
                    _ => return Err(invalid()),
                };
                (&trimmed[..index], multiplier)
            }
            _ => (trimmed, 1),
        };
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        let bytes = digits
            .parse::<u64>()
            .ok()
            .and_then(|number| number.checked_mul(multiplier))
            .ok_or_else(|| format!("capsule.memory '{value}' is too large"))?;
        Self::from_bytes(bytes)
    }

    fn from_bytes(bytes: u64) -> Result<Self, String> {
        if bytes < MIN_MEMORY_BYTES {
            return Err(format!(
                "capsule.memory must be at least 6m (Docker's minimum), got {bytes} bytes"
            ));
        }
        if i64::try_from(bytes).is_err() {
            return Err(format!("capsule.memory {bytes} bytes is too large"));
        }
        Ok(MemoryLimit(bytes))
    }

    pub fn bytes(self) -> u64 {
        self.0
    }
}

/// Renders in the largest unit that divides exactly (`4g`, `1536m`, bytes).
impl std::fmt::Display for MemoryLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (unit, size) in [('g', 1u64 << 30), ('m', 1 << 20), ('k', 1 << 10)] {
            if self.0.is_multiple_of(size) {
                return write!(f, "{}{unit}", self.0 / size);
            }
        }
        write!(f, "{}", self.0)
    }
}

impl From<MemoryLimit> for String {
    fn from(limit: MemoryLimit) -> Self {
        limit.to_string()
    }
}

/// TOML and environment values may arrive as a size string or a byte count.
#[derive(Deserialize)]
#[serde(untagged)]
enum SizeInput {
    Bytes(u64),
    Text(String),
}

impl TryFrom<SizeInput> for MemoryLimit {
    type Error = String;

    fn try_from(input: SizeInput) -> Result<Self, String> {
        match input {
            SizeInput::Bytes(bytes) => Self::from_bytes(bytes),
            SizeInput::Text(text) => Self::parse(&text),
        }
    }
}

/// A validated capsule pids cap. Zero is rejected because Docker reads a zero
/// `PidsLimit` as "unlimited".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u32")]
pub struct PidsLimit(u32);

// DECISION: 64 is the floor (the entrypoint, init, tmux, a shell, and the tool
// need a few dozen tasks; lower values only break the capsule). The ceiling is the
// kernel's PID_MAX_LIMIT. There is no "unlimited": raise the cap instead.
const MIN_PIDS: u32 = 64;
const MAX_PIDS: u32 = 4_194_304;

impl PidsLimit {
    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("capsule.pids must be a whole number, got '{value}'"))
            .and_then(Self::try_from)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u64> for PidsLimit {
    type Error = String;

    fn try_from(value: u64) -> Result<Self, String> {
        match u32::try_from(value) {
            Ok(pids) if (MIN_PIDS..=MAX_PIDS).contains(&pids) => Ok(PidsLimit(pids)),
            _ => Err(format!(
                "capsule.pids must be between {MIN_PIDS} and {MAX_PIDS}, got {value}"
            )),
        }
    }
}

impl From<PidsLimit> for u32 {
    fn from(limit: PidsLimit) -> Self {
        limit.0
    }
}

/// OS-credential preferences. Keychain lookup is off unless the user opted in
/// during `capscode init` or `capscode auth import` — an unconditional probe
/// raises an OS authorization dialog on every launch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Consult the OS keychain for the Anthropic credential. Off unless the user
    /// opted in during `capscode init` or `capscode auth import`: an unconditional
    /// probe raises an OS authorization dialog on every launch.
    pub keychain: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDefaults {
    pub claude: Option<String>,
    pub codex: Option<String>,
    pub opencode: Option<String>,
    pub pi: Option<String>,
    pub omp: Option<String>,
}

impl ProviderDefaults {
    pub fn get(&self, tool: Tool) -> Option<&str> {
        match tool {
            Tool::Claude => self.claude.as_deref(),
            Tool::Codex => self.codex.as_deref(),
            Tool::Opencode => self.opencode.as_deref(),
            Tool::Pi => self.pi.as_deref(),
            Tool::Omp => self.omp.as_deref(),
        }
    }

    pub fn set(&mut self, tool: Tool, provider_id: Option<String>) {
        match tool {
            Tool::Claude => self.claude = provider_id,
            Tool::Codex => self.codex = provider_id,
            Tool::Opencode => self.opencode = provider_id,
            Tool::Pi => self.pi = provider_id,
            Tool::Omp => self.omp = provider_id,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// If `None`: no VPN: the kill switch still blocks RFC1918/host traffic.
    pub wireguard_conf: Option<PathBuf>,
    /// Default true when `wireguard_conf` is `Some`.
    pub enforce_vpn: bool,
    pub telemetry_sinkhole: bool,
    pub extra_blocked_cidrs: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            wireguard_conf: None,
            enforce_vpn: false,
            telemetry_sinkhole: true,
            extra_blocked_cidrs: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrokerConfig {
    pub enabled: bool,
    /// Resolved at runtime from env/keychain, never stored in the config file.
    #[serde(skip)]
    pub keys: ProviderKeys,
    /// Defaults to the three provider API hosts.
    pub allow_hosts: Vec<String>,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        BrokerConfig {
            enabled: true,
            keys: ProviderKeys::default(),
            allow_hosts: vec![
                "api.anthropic.com".to_string(),
                "api.openai.com".to_string(),
                "api.moonshot.ai".to_string(),
            ],
        }
    }
}

/// Which upstream HTTP auth scheme a provider credential authenticates with.
///
/// `ApiKey` (the default) covers every long-lived API key. `OAuth` marks a Claude
/// Code subscription setup-token. `CodexAccessToken` marks a ChatGPT-backed Codex
/// access token: it uses bearer injection like OAuth, but requires Codex-specific
/// broker routes and a different capsule sentinel. The kind is not itself secret.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthKind {
    #[default]
    ApiKey,
    OAuth,
    CodexAccessToken,
}

/// A single provider credential: the secret material plus how it authenticates
/// upstream. `Debug` is deliberately not derived so the secret is never rendered
/// outside `SecretString`'s own redaction.
#[derive(Clone)]
pub struct ProviderCredential {
    pub secret: SecretString,
    pub kind: AuthKind,
}

impl ProviderCredential {
    /// A long-lived API-key credential (the default shape).
    pub fn api_key(secret: SecretString) -> Self {
        ProviderCredential {
            secret,
            kind: AuthKind::ApiKey,
        }
    }

    /// A Claude Code subscription OAuth token credential (Bearer).
    pub fn oauth(secret: SecretString) -> Self {
        ProviderCredential {
            secret,
            kind: AuthKind::OAuth,
        }
    }

    pub fn codex_access_token(secret: SecretString) -> Self {
        ProviderCredential {
            secret,
            kind: AuthKind::CodexAccessToken,
        }
    }
}

/// Provider credentials, wrapped so they never appear in `Debug` or serialized
/// output. Only the count is rendered — neither a secret value nor the per-entry
/// auth kind is exposed.
#[derive(Clone, Default)]
pub struct ProviderKeys(pub HashMap<Provider, ProviderCredential>);

impl ProviderKeys {
    /// Iterate the raw secret values (without their kind), for the capsule-leak
    /// assertion that no real credential crossed the gateway boundary.
    pub fn secrets(&self) -> impl Iterator<Item = &SecretString> {
        self.0.values().map(|cred| &cred.secret)
    }
}

impl std::fmt::Debug for ProviderKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ProviderKeys")
            .field(&format_args!("[{} key(s) redacted]", self.0.len()))
            .finish()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitConfig {
    pub history: HistoryDial,
    /// Untracked files explicitly re-included.
    pub include: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub include: Vec<PathBuf>,
}

impl Config {
    /// Root directory for all capscode-owned configuration and state.
    pub fn config_home() -> PathBuf {
        env::var_os("CAPSCODE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".capscode")))
            .unwrap_or_else(|| PathBuf::from(".capscode"))
    }

    /// Directory containing capscode-owned WireGuard configuration files.
    pub fn vpn_dir() -> PathBuf {
        vpn_dir_for(&Self::config_home())
    }

    /// Creates the capscode home if needed and restricts it to mode 0700 on Unix.
    pub fn ensure_config_home() -> io::Result<PathBuf> {
        let home = Self::config_home();
        ensure_private_dir(&home)?;
        Ok(home)
    }

    /// Creates the capscode VPN directory if needed and restricts it to mode 0700 on Unix.
    pub fn ensure_vpn_dir() -> io::Result<PathBuf> {
        Self::ensure_config_home()?;
        let vpn = Self::vpn_dir();
        ensure_private_dir(&vpn)?;
        Ok(vpn)
    }

    /// Path to the on-disk config file under [`Config::config_home`].
    pub fn config_file_path() -> PathBuf {
        Self::config_home().join("config.toml")
    }

    /// Creates the private config home if needed and returns the config file path.
    #[allow(clippy::result_large_err)]
    pub fn prepare_config_file() -> Result<PathBuf, ConfigError> {
        let config_home = Self::config_home();
        ensure_private_dir(&config_home).map_err(|error| {
            ConfigError::Home(format!("preparing {}: {error}", config_home.display()))
        })?;
        Ok(config_home.join("config.toml"))
    }

    /// Builds the defaults -> file -> env layered figment, without CLI flags.
    pub fn figment_with_file(file: impl figment::Provider) -> Figment {
        Figment::from(Serialized::defaults(Config::default()))
            .merge(file)
            .merge(Env::prefixed("CAPSCODE_").split("__"))
    }

    #[allow(clippy::result_large_err)]
    fn base_figment() -> Result<Figment, ConfigError> {
        let config_file = Self::prepare_config_file()?;
        Ok(Self::figment_with_file(Toml::file(config_file)))
    }

    /// Resolves the config from defaults, the on-disk file, and environment
    /// variables (no CLI-flag layer).
    #[allow(clippy::result_large_err)]
    pub fn load() -> Result<Config, ConfigError> {
        Self::base_figment()?.extract().map_err(ConfigError::from)
    }

    /// Resolves the config with an additional, highest-precedence CLI-flag layer.
    #[allow(clippy::result_large_err)]
    pub fn load_with_flags<P: figment::Provider>(flags: P) -> Result<Config, ConfigError> {
        Self::base_figment()?
            .merge(flags)
            .extract()
            .map_err(ConfigError::from)
    }
}

fn vpn_dir_for(home: &Path) -> PathBuf {
    home.join("vpn")
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    tighten_permissions(path, 0o700)
}

#[cfg(unix)]
fn tighten_permissions(path: &Path, maximum: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path)?;
    let current = metadata.permissions().mode() & 0o7777;
    let tightened = current & maximum;
    if tightened != current {
        fs::set_permissions(path, fs::Permissions::from_mode(tightened))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten_permissions(_path: &Path, _maximum: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Toml;
    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        name: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: Option<&OsStr>) -> Self {
            let previous = env::var_os(name);
            match value {
                Some(value) => env::set_var(name, value),
                None => env::remove_var(name),
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => env::set_var(self.name, value),
                None => env::remove_var(self.name),
            }
        }
    }

    fn file_layer() -> figment::providers::Data<Toml> {
        Toml::string(
            r#"
            [network]
            telemetry_sinkhole = false
            "#,
        )
    }

    #[test]
    fn config_precedence_file_env_flags() {
        // File alone overrides the built-in default (true -> false).
        let cfg: Config = Config::figment_with_file(file_layer())
            .extract()
            .expect("file layer should resolve");
        assert!(!cfg.network.telemetry_sinkhole);

        // Env overrides the file.
        std::env::set_var("CAPSCODE_NETWORK__TELEMETRY_SINKHOLE", "true");
        let cfg: Config = Config::figment_with_file(file_layer())
            .extract()
            .expect("env layer should resolve");
        assert!(cfg.network.telemetry_sinkhole);

        // A CLI-flag provider overrides both file and env.
        let flags = Serialized::default("network.telemetry_sinkhole", false);
        let cfg: Config = Config::figment_with_file(file_layer())
            .merge(Env::prefixed("CAPSCODE_").split("__"))
            .merge(flags)
            .extract()
            .expect("flag layer should resolve");
        assert!(!cfg.network.telemetry_sinkhole);

        std::env::remove_var("CAPSCODE_NETWORK__TELEMETRY_SINKHOLE");
    }

    #[test]
    fn default_config_round_trips_through_figment() {
        let cfg = Config::figment_with_file(Toml::string(""))
            .extract::<Config>()
            .expect("defaults alone should resolve");
        assert_eq!(cfg.broker.allow_hosts.len(), 3);
        assert!(!cfg.auth.keychain, "keychain lookup defaults to off");
        assert_eq!(cfg.workspace.mode, WorkspaceMode::Copy);
        assert_eq!(cfg.r#box.prefix, DEFAULT_BOX_PREFIX);
    }

    #[test]
    fn auth_keychain_flag_deserializes_from_toml() {
        let cfg = Config::figment_with_file(Toml::string(
            r#"
            [auth]
            keychain = true
            "#,
        ))
        .extract::<Config>()
        .expect("auth.keychain should resolve");
        assert!(cfg.auth.keychain);
    }

    #[test]
    fn workspace_mode_deserializes_from_toml() {
        let cfg = Config::figment_with_file(Toml::string(
            r#"
            [workspace]
            mode = "mount"
            "#,
        ))
        .extract::<Config>()
        .expect("workspace.mode should resolve");
        assert_eq!(cfg.workspace.mode, WorkspaceMode::Mount);
    }

    #[test]
    fn box_prefix_deserializes_from_toml() {
        let cfg = Config::figment_with_file(Toml::string(
            r#"
            [box]
            prefix = "C-a"
            "#,
        ))
        .extract::<Config>()
        .expect("box.prefix should resolve");
        assert_eq!(cfg.r#box.prefix, "C-a");
    }

    #[test]
    fn memory_limit_parses_binary_sizes_and_bytes() {
        for (text, bytes) in [
            ("512m", 512u64 << 20),
            ("4g", 4 << 30),
            ("4G", 4 << 30),
            ("1536M", 1536 << 20),
            ("65536k", 64 << 20),
            ("8388608", 8 << 20),
            (" 2g ", 2 << 30),
        ] {
            assert_eq!(MemoryLimit::parse(text).unwrap().bytes(), bytes, "{text}");
        }
        assert_eq!(MemoryLimit::parse("4096m").unwrap().to_string(), "4g");
        assert_eq!(MemoryLimit::parse("1536m").unwrap().to_string(), "1536m");
        assert_eq!(
            MemoryLimit::parse("8388609").unwrap().to_string(),
            "8388609"
        );
        for bad in [
            "",
            "g",
            "4x",
            "4gb",
            "4.5g",
            "-1g",
            "+4g",
            "4 g",
            "1m",
            "0",
            "99999999999g",
            "18446744073709551615",
        ] {
            assert!(MemoryLimit::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn pids_limit_rejects_zero_and_out_of_range() {
        assert_eq!(PidsLimit::parse("1024").unwrap().get(), 1024);
        assert_eq!(PidsLimit::parse("64").unwrap().get(), 64);
        for bad in ["0", "63", "4194305", "-1", "abc", ""] {
            assert!(PidsLimit::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn capsule_limits_default_and_deserialize_from_toml() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _memory = EnvGuard::set("CAPSCODE_CAPSULE__MEMORY", None);
        let _pids = EnvGuard::set("CAPSCODE_CAPSULE__PIDS", None);

        let cfg = Config::figment_with_file(Toml::string(""))
            .extract::<Config>()
            .unwrap();
        assert_eq!(cfg.capsule.memory_bytes(), None);
        assert_eq!(cfg.capsule.pids_limit(), DEFAULT_PIDS_LIMIT);

        let cfg = Config::figment_with_file(Toml::string(
            r#"
            [capsule]
            memory = "4g"
            pids = 2048
            "#,
        ))
        .extract::<Config>()
        .unwrap();
        assert_eq!(cfg.capsule.memory_bytes(), Some(4 << 30));
        assert_eq!(cfg.capsule.pids_limit(), 2048);

        let cfg = Config::figment_with_file(Toml::string("[capsule]\nmemory = 1073741824\n"))
            .extract::<Config>()
            .unwrap();
        assert_eq!(cfg.capsule.memory_bytes(), Some(1 << 30));

        for bad in [
            "[capsule]\nmemory = \"lots\"\n",
            "[capsule]\nmemory = \"1m\"\n",
            "[capsule]\npids = 0\n",
            "[capsule]\npids = -1\n",
        ] {
            assert!(
                Config::figment_with_file(Toml::string(bad))
                    .extract::<Config>()
                    .is_err(),
                "{bad:?} must fail to load"
            );
        }
    }

    #[test]
    fn capsule_limits_resolve_from_environment() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _memory = EnvGuard::set("CAPSCODE_CAPSULE__MEMORY", Some(OsStr::new("2g")));
        let _pids = EnvGuard::set("CAPSCODE_CAPSULE__PIDS", Some(OsStr::new("4096")));
        let cfg = Config::figment_with_file(Toml::string(""))
            .extract::<Config>()
            .unwrap();
        assert_eq!(cfg.capsule.memory_bytes(), Some(2 << 30));
        assert_eq!(cfg.capsule.pids_limit(), 4096);

        let _bytes = EnvGuard::set("CAPSCODE_CAPSULE__MEMORY", Some(OsStr::new("536870912")));
        let cfg = Config::figment_with_file(Toml::string(""))
            .extract::<Config>()
            .unwrap();
        assert_eq!(cfg.capsule.memory_bytes(), Some(512 << 20));
    }

    #[test]
    fn capscode_home_override_is_honored() {
        let _lock = ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let _capscode_home = EnvGuard::set("CAPSCODE_HOME", Some(root.path().as_os_str()));

        assert_eq!(Config::config_home(), root.path());
        assert_eq!(Config::config_file_path(), root.path().join("config.toml"));
        assert_eq!(Config::vpn_dir(), root.path().join("vpn"));
    }

    #[test]
    fn default_config_home_is_under_home_dot_capscode() {
        let _lock = ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let _capscode_home = EnvGuard::set("CAPSCODE_HOME", None);
        let _home = EnvGuard::set("HOME", Some(root.path().as_os_str()));

        assert_eq!(Config::config_home(), root.path().join(".capscode"));
        assert_eq!(
            Config::config_file_path(),
            root.path().join(".capscode/config.toml")
        );
    }

    #[test]
    fn prepare_config_file_creates_a_private_home() {
        let _lock = ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("capscode-home");
        let _capscode_home = EnvGuard::set("CAPSCODE_HOME", Some(home.as_os_str()));

        let config_file = Config::prepare_config_file().unwrap();

        assert_eq!(config_file, home.join("config.toml"));
        assert!(home.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&home).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn secret_string_redacts_under_debug() {
        let secret = SecretString::from("sk-live-super-secret-value".to_string());
        let debug_output = format!("{:?}", secret);
        assert!(!debug_output.contains("sk-live-super-secret-value"));
    }

    #[test]
    fn provider_keys_redact_under_debug() {
        let mut map = HashMap::new();
        map.insert(
            Provider::Anthropic,
            ProviderCredential::api_key(SecretString::from(
                "sk-live-super-secret-value".to_string(),
            )),
        );
        let keys = ProviderKeys(map);
        let debug_output = format!("{:?}", keys);
        assert!(!debug_output.contains("sk-live-super-secret-value"));

        // BrokerConfig (and by extension Config) must not leak keys either.
        let broker = BrokerConfig {
            keys,
            ..BrokerConfig::default()
        };
        let debug_output = format!("{:?}", broker);
        assert!(!debug_output.contains("sk-live-super-secret-value"));
    }
}
