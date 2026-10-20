use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use capscode_core::{
    ApiStyle, AuthKind, AuthScheme, ProviderCredential, ProviderSpec, WorkspaceMode,
};
use dialoguer::{Confirm, Input, MultiSelect, Password};
use secrecy::{ExposeSecret, SecretString};
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

use crate::args::{InitArgs, RuntimeArg, ToolArg};
use crate::ui::{header, kv, note, rule, styled, theme, Caps, Status};
use crate::{commands, config, keystore, shell_completion};

const ALL_TOOLS: [ToolArg; 5] = [
    ToolArg::Claude,
    ToolArg::Codex,
    ToolArg::Opencode,
    ToolArg::Pi,
    ToolArg::Omp,
];

#[derive(Clone, Debug, Eq, PartialEq)]
enum CredentialSource {
    Environment(String),
    Keychain,
    CodexLogin(String),
    None,
}

impl CredentialSource {
    fn status(&self) -> &'static str {
        match self {
            Self::Environment(_) => "configured (env)",
            Self::Keychain => "configured (keychain)",
            Self::CodexLogin(_) => "configured (Codex login)",
            Self::None => "not configured",
        }
    }
}

#[derive(Debug)]
enum RuntimeCheck {
    Pass(String),
    Fail(String),
}

#[derive(Debug)]
enum ImageCheck {
    Present(String),
    Missing(String),
    Error(String, String),
    Skipped(String),
}

#[derive(Debug)]
struct PreflightReport {
    runtime: RuntimeCheck,
    images: Vec<ImageCheck>,
}

impl PreflightReport {
    fn ready(&self) -> bool {
        matches!(self.runtime, RuntimeCheck::Pass(_))
            && self
                .images
                .iter()
                .all(|image| matches!(image, ImageCheck::Present(_)))
    }
}

#[derive(Debug)]
struct DefaultSummary {
    tool: ToolArg,
    provider: String,
}

#[derive(Debug)]
struct CredentialSummary {
    provider: String,
    source: CredentialSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialStorage {
    AnthropicKeychain,
    ProviderKeychain,
}

fn credential_storage(spec: &ProviderSpec) -> CredentialStorage {
    if spec.id.eq_ignore_ascii_case("anthropic") {
        CredentialStorage::AnthropicKeychain
    } else {
        CredentialStorage::ProviderKeychain
    }
}

fn credential_entry_prompt(spec: &ProviderSpec) -> String {
    match credential_storage(spec) {
        CredentialStorage::AnthropicKeychain => format!(
            "Enter a credential for {} now? (saved to your keychain)",
            spec.id
        ),
        CredentialStorage::ProviderKeychain => format!(
            "Store an API key for {} now? (saved to your keychain; {} still takes precedence)",
            spec.id, spec.key_env
        ),
    }
}

pub async fn init(args: InitArgs, runtime_flag: Option<RuntimeArg>) -> Result<()> {
    let interactive = io::stdin().is_terminal() && !args.yes;
    let requested_tools = deduplicate_tools(&args.tool);
    let image_tools = if requested_tools.is_empty() {
        ALL_TOOLS.to_vec()
    } else {
        requested_tools.clone()
    };
    // Load without forcing credential materialization for the fast path.
    // Non-interactive must never hang on keychain or network.
    let (loaded, loaded_defaults) = if interactive {
        let full = config::load(runtime_flag)?;
        let defs = full.defaults.clone();
        (full, defs)
    } else {
        // Lightweight load: only the parts needed for images/runtime; skip auth overlay.
        let mut cfg = capscode_core::Config::load().context("resolving configuration")?;
        if let Some(rt) = runtime_flag {
            cfg.runtime = capscode_core::RuntimePref::from(rt);
        }
        let defs = cfg.defaults.clone();
        (cfg, defs)
    };

    let caps = Caps::stderr();
    note(header("CapsCode first-run setup", caps));
    note(rule(Some("Preflight"), caps));
    let preflight = run_preflight(&loaded, &image_tools).await;
    note(build_preflight_report(&preflight, caps));

    if !interactive {
        return run_non_interactive(
            args,
            runtime_flag,
            loaded_defaults,
            preflight,
            requested_tools,
            caps,
        );
    }

    let tools = if requested_tools.is_empty() {
        prompt_for_tools()?
    } else {
        requested_tools
    };
    let probe = probe_choice(&args, true, || {
        Confirm::with_theme(&theme())
            .with_prompt("Probe available credentials over the network?")
            .default(true)
            .interact()
            .context("reading credential probe choice")
    })?;

    note(rule(Some("Credentials"), caps));
    let mut defaults = Vec::new();
    let mut credentials = Vec::new();
    for tool in tools.iter().copied() {
        let Some(provider_id) = config::pick_provider(tool.into())? else {
            note(kv(
                tool.name(),
                "provider selection skipped",
                Some(Status::Dim),
                caps,
            ));
            continue;
        };
        let resolved = config::resolve_provider(Some(&provider_id), tool.into(), &loaded.defaults)?;
        note(kv(
            tool.name(),
            &format!("provider: {}", resolved.spec.id),
            None,
            caps,
        ));

        let mut source = classify_credential_source(resolved.credential_source.as_deref());
        let mut credential = resolved.credential;
        let source_status = match source {
            CredentialSource::None => Status::Warn,
            _ => Status::Ok,
        };
        note(kv("credential", source.status(), Some(source_status), caps));
        let storage = credential_storage(&resolved.spec);
        let entry_prompt = credential_entry_prompt(&resolved.spec);
        if credential.is_none()
            && Confirm::with_theme(&theme())
                .with_prompt(entry_prompt)
                .default(false)
                .interact()
                .context("reading credential entry choice")?
        {
            let entry_label = match storage {
                CredentialStorage::AnthropicKeychain => "Claude setup-token",
                CredentialStorage::ProviderKeychain => "Provider API key",
            };
            let entered = Zeroizing::new(
                Password::with_theme(&theme())
                    .with_prompt(entry_label)
                    .interact()
                    .context("reading provider credential")?,
            );
            if entered.is_empty() {
                bail!("provider credential must not be empty");
            }
            match storage {
                CredentialStorage::AnthropicKeychain => {
                    let service = commands::store_anthropic_token(&entered)?;
                    note(kv(
                        "stored",
                        &format!("keychain:{service}"),
                        Some(Status::Ok),
                        caps,
                    ));
                    credential = Some(ProviderCredential::oauth(SecretString::from(
                        entered.to_string(),
                    )));
                }
                CredentialStorage::ProviderKeychain => {
                    commands::store_provider_key_in_keychain(&resolved.spec, &entered)?;
                    note(kv(
                        "stored",
                        &format!(
                            "keychain service '{}' (account '{}')",
                            keystore::service(),
                            keystore::ACCOUNT
                        ),
                        Some(Status::Ok),
                        caps,
                    ));
                    credential = Some(ProviderCredential::api_key(SecretString::from(
                        entered.to_string(),
                    )));
                }
            }
            source = CredentialSource::Keychain;
        }

        let (check_line, check_status) =
            credential_check_line(&resolved.spec, credential.as_ref(), probe).await;
        note(kv("check", &check_line, Some(check_status), caps));
        credentials.push(CredentialSummary {
            provider: resolved.spec.id.clone(),
            source,
        });

        if Confirm::with_theme(&theme())
            .with_prompt(format!(
                "Set {} as the default provider for {}?",
                resolved.spec.id,
                tool.name()
            ))
            .default(true)
            .interact()
            .context("reading provider default choice")?
        {
            config::set_provider_default(tool.into(), Some(&resolved.spec.id))?;
            note(kv(
                "default",
                &format!("{} set to {}", tool.name(), resolved.spec.id),
                Some(Status::Ok),
                caps,
            ));
            defaults.push(DefaultSummary {
                tool,
                provider: resolved.spec.id,
            });
        }
    }

    note(rule(Some("Network"), caps));
    if Confirm::with_theme(&theme())
        .with_prompt("Import a WireGuard VPN configuration?")
        .default(false)
        .interact()
        .context("reading VPN import choice")?
    {
        let path: String = Input::with_theme(&theme())
            .with_prompt("WireGuard config path")
            .interact_text()
            .context("reading WireGuard config path")?;
        commands::vpn_import(Path::new(&path))?;
    }
    let vpn = commands::init_vpn_status_line(runtime_flag)?;
    note(styled(Status::Dim, &vpn, caps));

    note(rule(Some("Workspace"), caps));
    let workspace_mode = prompt_workspace_mode()?;
    config::set_workspace_mode(workspace_mode)?;
    note(kv(
        "workspace.mode",
        workspace_mode.as_str(),
        Some(Status::Ok),
        caps,
    ));

    note(rule(Some("Completions"), caps));
    note(
        (shell_completion::configure(args.completion_shell, args.no_completions, true, false)?)
            .to_string(),
    );

    note(rule(Some("Summary"), caps));
    let cwd = std::env::current_dir().context("reading current directory")?;
    note(
        build_summary(
            &cwd,
            preflight.ready(),
            &defaults,
            &credentials,
            &vpn,
            tools.first().copied().unwrap_or(ToolArg::Claude),
        )
        .to_string(),
    );
    Ok(())
}

fn run_non_interactive(
    args: InitArgs,
    _runtime_flag: Option<RuntimeArg>,
    defaults_config: capscode_core::ProviderDefaults,
    preflight: PreflightReport,
    tools: Vec<ToolArg>,
    caps: Caps,
) -> Result<()> {
    // Non-interactive: only set defaults when --tool was explicitly given.
    // Never touch keychain or network. Print instructions for interactive steps.
    let mut defaults = Vec::new();
    if !tools.is_empty() {
        // Resolve provider ids without credential reads by supplying a no-op keychain.
        let registry = config::providers::load_registry()?;
        note(rule(Some("Defaults"), caps));
        for tool in tools.iter().copied() {
            match config::providers::resolve_provider_from(
                &registry,
                None,
                tool.into(),
                &defaults_config,
                config::providers::env_value,
                || None,
                |_| None,
            ) {
                Ok(resolved) => {
                    config::set_provider_default(tool.into(), Some(&resolved.spec.id))?;
                    note(styled(
                        Status::Ok,
                        &format!(
                            "Default provider for {} set to {}.",
                            tool.name(),
                            resolved.spec.id
                        ),
                        caps,
                    ));
                    defaults.push(DefaultSummary {
                        tool,
                        provider: resolved.spec.id,
                    });
                }
                Err(error) => note(styled(
                    Status::Warn,
                    &format!("{} default: skipped ({error})", tool.name()),
                    caps,
                )),
            }
        }
    }

    // Non-interactive / --yes: workspace.mode defaults to copy with no prompt.
    // Mount mode is a real security trade and must never be chosen silently.
    config::set_workspace_mode(WorkspaceMode::Copy)?;
    note(rule(Some("Workspace"), caps));
    note(kv(
        "workspace.mode",
        WorkspaceMode::Copy.as_str(),
        Some(Status::Ok),
        caps,
    ));

    note(rule(Some("Completions"), caps));
    note(
        (shell_completion::configure(args.completion_shell, args.no_completions, false, args.yes)?)
            .to_string(),
    );
    note(rule(Some("Summary"), caps));
    note(styled(
        Status::Dim,
        "Credential entry and VPN import require an interactive terminal.",
        caps,
    ));
    if args.probe {
        note(styled(
            Status::Dim,
            "Credential probes were skipped in non-interactive setup.",
            caps,
        ));
    }
    note(styled(
        Status::Dim,
        "Use 'capscode key set anthropic' or export the provider key, then rerun 'capscode init'.",
        caps,
    ));
    note(styled(
        Status::Dim,
        "Use 'capscode vpn import <path>' to configure an optional VPN.",
        caps,
    ));
    // Do not query live VPN/credential state here — that would require keychain/config loads
    // which can block. The summary reflects only what non-interactive actually did.
    let vpn_line = "VPN: unknown in non-interactive (run 'capscode vpn' interactively)";
    let cwd = std::env::current_dir().context("reading current directory")?;
    note(
        build_summary(
            &cwd,
            preflight.ready(),
            &defaults,
            &[], // no credential enumeration in non-interactive
            vpn_line,
            tools.first().copied().unwrap_or(ToolArg::Claude),
        )
        .to_string(),
    );
    Ok(())
}

async fn run_preflight(loaded: &capscode_core::Config, tools: &[ToolArg]) -> PreflightReport {
    let mut images = Vec::with_capacity(tools.len() + 1);
    match capscode_runtime::detect(loaded.runtime.clone()).await {
        Ok(runtime) => {
            let runtime_name = crate::commands::runtime_kind_label(runtime.kind()).to_string();
            let mut required = Vec::with_capacity(tools.len() + 1);
            match config::gateway_image(loaded) {
                Ok(image) => required.push(image),
                Err(error) => {
                    images.push(ImageCheck::Error("gateway".to_string(), error.to_string()))
                }
            }
            for tool in tools.iter().copied() {
                match config::capsule_image(loaded, tool.into()) {
                    Ok(image) if !required.contains(&image) => required.push(image),
                    Ok(_) => {}
                    Err(error) => images.push(ImageCheck::Error(
                        format!("capsule-{}", tool.name()),
                        error.to_string(),
                    )),
                }
            }
            for image in required {
                match runtime.image_present(&image).await {
                    Ok(true) => images.push(ImageCheck::Present(image.to_string())),
                    Ok(false) => images.push(ImageCheck::Missing(image.to_string())),
                    Err(error) => {
                        images.push(ImageCheck::Error(image.to_string(), error.to_string()))
                    }
                }
            }
            PreflightReport {
                runtime: RuntimeCheck::Pass(runtime_name),
                images,
            }
        }
        Err(error) => {
            if let Ok(image) = config::gateway_image(loaded) {
                images.push(ImageCheck::Skipped(image.to_string()));
            }
            for tool in tools.iter().copied() {
                if let Ok(image) = config::capsule_image(loaded, tool.into()) {
                    images.push(ImageCheck::Skipped(image.to_string()));
                }
            }
            PreflightReport {
                runtime: RuntimeCheck::Fail(error.to_string()),
                images,
            }
        }
    }
}

fn build_preflight_report(report: &PreflightReport, caps: Caps) -> String {
    let mut lines = Vec::new();
    match &report.runtime {
        RuntimeCheck::Pass(runtime) => lines.push(kv("Runtime", runtime, Some(Status::Ok), caps)),
        RuntimeCheck::Fail(error) => lines.push(kv("Runtime", error, Some(Status::Fail), caps)),
    }
    let mut missing = false;
    for image in &report.images {
        match image {
            ImageCheck::Present(image) => {
                lines.push(kv("Image", image, Some(Status::Ok), caps));
            }
            ImageCheck::Missing(image) => {
                missing = true;
                lines.push(kv(
                    "Image",
                    &format!("{image} (missing)"),
                    Some(Status::Fail),
                    caps,
                ));
            }
            ImageCheck::Error(image, error) => {
                lines.push(kv(
                    "Image",
                    &format!("{image} ({error})"),
                    Some(Status::Fail),
                    caps,
                ));
            }
            ImageCheck::Skipped(image) => {
                lines.push(kv(
                    "Image",
                    &format!("{image} (runtime unavailable)"),
                    Some(Status::Dim),
                    caps,
                ));
            }
        }
    }
    if missing {
        lines.push(format!(
            "Build missing images: {}",
            crate::build_context::image_build_hint()
        ));
    }
    lines.join("\n")
}

fn prompt_for_tools() -> Result<Vec<ToolArg>> {
    let labels = ALL_TOOLS.map(ToolArg::name);
    let selected = MultiSelect::with_theme(&theme())
        .with_prompt("Tools to configure")
        .items(labels)
        .defaults(&[true, true, false, false])
        .interact()
        .context("reading tool selection")?;
    Ok(selected.into_iter().map(|index| ALL_TOOLS[index]).collect())
}

/// Ask once: copy (default) or mount. Mount means the agent writes directly to
/// the host directory with no copy-back review standing between it and the host.
fn prompt_workspace_mode() -> Result<WorkspaceMode> {
    let mount = Confirm::with_theme(&theme())
        .with_prompt(
            "Bind-mount the host directory into the capsule? \
             In mount mode the agent writes directly to the host directory and no copy-back review stands between it and the host.",
        )
        .default(false)
        .interact()
        .context("reading workspace mode choice")?;
    Ok(if mount {
        WorkspaceMode::Mount
    } else {
        WorkspaceMode::Copy
    })
}

fn deduplicate_tools(tools: &[ToolArg]) -> Vec<ToolArg> {
    let mut unique = Vec::new();
    for tool in tools.iter().copied() {
        if !unique.contains(&tool) {
            unique.push(tool);
        }
    }
    unique
}

fn probe_choice<F>(args: &InitArgs, interactive: bool, prompt: F) -> Result<bool>
where
    F: FnOnce() -> Result<bool>,
{
    if args.no_probe {
        Ok(false)
    } else if args.probe {
        Ok(true)
    } else if interactive {
        prompt()
    } else {
        Ok(false)
    }
}

fn classify_credential_source(source: Option<&str>) -> CredentialSource {
    match source {
        Some(value) if value.starts_with("env:") => {
            CredentialSource::Environment(value.trim_start_matches("env:").to_string())
        }
        Some(value) if value.starts_with("keychain") => CredentialSource::Keychain,
        Some(value) if value.starts_with("codex-auth:") => {
            CredentialSource::CodexLogin(value.trim_start_matches("codex-auth:").to_string())
        }
        _ => CredentialSource::None,
    }
}

pub(crate) fn well_formed_key(value: &str) -> bool {
    (12..=4096).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b'/' | b'=')
        })
}

async fn credential_check_line(
    spec: &ProviderSpec,
    credential: Option<&ProviderCredential>,
    probe: bool,
) -> (String, Status) {
    let Some(credential) = credential else {
        return ("no key present".to_string(), Status::Warn);
    };
    if credential.kind == AuthKind::CodexAccessToken {
        return (
            "ChatGPT access token present; expiry checked locally".to_string(),
            Status::Ok,
        );
    }
    let offline = offline_check_line(credential.secret.expose_secret());
    if !probe || !well_formed_key(credential.secret.expose_secret()) {
        let status = if well_formed_key(credential.secret.expose_secret()) {
            Status::Ok
        } else {
            Status::Fail
        };
        return (offline, status);
    }
    match probe_provider(spec, credential).await {
        Ok(status) if (200..300).contains(&status) => (
            format!("reachable/authenticated (HTTP {status})"),
            Status::Ok,
        ),
        Ok(status @ (401 | 403)) => (
            format!("reachable; authentication failed (HTTP {status})"),
            Status::Fail,
        ),
        Ok(status) => (
            format!("reachable; authentication not rejected (HTTP {status})"),
            Status::Warn,
        ),
        Err(_) => (
            format!("network unavailable; offline fallback: {offline}"),
            Status::Warn,
        ),
    }
}

fn offline_check_line(value: &str) -> String {
    if well_formed_key(value) {
        "key present & well-formed".to_string()
    } else {
        "key present but malformed".to_string()
    }
}

async fn probe_provider(spec: &ProviderSpec, credential: &ProviderCredential) -> Result<u16> {
    let url = probe_url(spec);
    if !url.starts_with("https://") {
        bail!("credential probes require an HTTPS provider URL");
    }
    let body = r#"{"model":"capscode-credential-probe","max_tokens":1,"messages":[{"role":"user","content":"ping"}]}"#;
    let (header, prefix) = if credential.kind == AuthKind::OAuth || spec.auth == AuthScheme::Bearer
    {
        ("Authorization", "Bearer ")
    } else {
        ("x-api-key", "")
    };
    let secret = credential.secret.expose_secret();
    if !well_formed_key(secret) {
        bail!("credential is not safe to pass to the probe client");
    }
    let curl_config = Zeroizing::new(format!("header = \"{header}: {prefix}{secret}\"\n"));

    let mut command = tokio::process::Command::new("curl");
    command
        .arg("--disable")
        .arg("--config")
        .arg("-")
        .arg("--silent")
        .arg("--proto")
        .arg("=https")
        .arg("--output")
        .arg("/dev/null")
        .arg("--write-out")
        .arg("%{http_code}")
        .arg("--max-time")
        .arg("10")
        .arg("--request")
        .arg("POST")
        .arg("--header")
        .arg("content-type: application/json")
        .arg("--header")
        .arg("anthropic-version: 2023-06-01")
        .arg("--data")
        .arg(body)
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().context("starting credential probe")?;
    let mut stdin = child
        .stdin
        .take()
        .context("opening credential probe input")?;
    stdin
        .write_all(curl_config.as_bytes())
        .await
        .context("sending credential to probe")?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(12), child.wait_with_output())
        .await
        .context("credential probe timed out")??;
    if !output.status.success() {
        bail!("credential probe could not reach provider");
    }
    let status = std::str::from_utf8(&output.stdout)
        .context("credential probe returned a non-UTF-8 status")?
        .parse::<u16>()
        .context("credential probe returned an invalid HTTP status")?;
    Ok(status)
}

fn probe_url(spec: &ProviderSpec) -> String {
    let base = spec.base_url.trim_end_matches('/');
    let suffix = match spec.api_style {
        ApiStyle::Anthropic if base.ends_with("/v1") => "/messages",
        ApiStyle::Anthropic => "/v1/messages",
        ApiStyle::OpenAI if base.ends_with("/v1") => "/chat/completions",
        ApiStyle::OpenAI => "/v1/chat/completions",
    };
    format!("{base}{suffix}")
}

fn build_summary(
    cwd: &Path,
    ready: bool,
    defaults: &[DefaultSummary],
    credentials: &[CredentialSummary],
    vpn: &str,
    next_tool: ToolArg,
) -> String {
    let mut lines = vec![if ready {
        "You're ready to run CapsCode.".to_string()
    } else {
        "Setup recorded; resolve the preflight failures before running CapsCode.".to_string()
    }];
    if defaults.is_empty() {
        lines.push("Provider defaults: unchanged".to_string());
    } else {
        lines.push(format!(
            "Provider defaults: {}",
            defaults
                .iter()
                .map(|item| format!("{}={}", item.tool.name(), item.provider))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if credentials.is_empty() {
        lines.push("Credentials: none detected".to_string());
    } else {
        lines.push(format!(
            "Credentials: {}",
            credentials
                .iter()
                .map(|item| match &item.source {
                    CredentialSource::Environment(name) => {
                        format!("{} (env:{name})", item.provider)
                    }
                    CredentialSource::Keychain => format!("{} (keychain)", item.provider),
                    CredentialSource::CodexLogin(mode) => {
                        format!("{} (Codex {mode} login)", item.provider)
                    }
                    CredentialSource::None => format!("{} (missing)", item.provider),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.push(vpn.to_string());
    lines.push("Workspace changes follow the selected workspace mode.".to_string());
    lines.push(format!(
        "Next: cd {} && capscode {}",
        shell_quote(cwd),
        next_tool.name()
    ));
    lines.join("\n")
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'.'))
    {
        value.into_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "sk-test-super-secret-value";

    fn init_args() -> InitArgs {
        InitArgs {
            tool: Vec::new(),
            probe: false,
            no_probe: false,
            yes: false,
            completion_shell: None,
            no_completions: false,
        }
    }

    #[test]
    fn credential_source_classifies_env_keychain_and_none() {
        assert_eq!(
            classify_credential_source(Some("env:OPENAI_API_KEY")),
            CredentialSource::Environment("OPENAI_API_KEY".to_string())
        );
        assert_eq!(
            classify_credential_source(Some("keychain:claude-setup-token")),
            CredentialSource::Keychain
        );
        assert_eq!(
            classify_credential_source(Some("codex-auth:chatgpt")),
            CredentialSource::CodexLogin("chatgpt".to_string())
        );
        assert_eq!(classify_credential_source(None), CredentialSource::None);
    }

    #[test]
    fn non_interactive_workspace_mode_defaults_to_copy() {
        // Mirrors run_non_interactive: --yes / non-tty never prompts and never
        // opts into mount.
        assert_eq!(WorkspaceMode::default(), WorkspaceMode::Copy);
        assert_eq!(WorkspaceMode::Copy.as_str(), "copy");
    }

    #[test]
    fn non_anthropic_init_credentials_use_the_provider_keychain() {
        let spec = config::providers::load_registry()
            .unwrap()
            .into_iter()
            .find(|provider| provider.id == "zai-coding-openai")
            .unwrap();

        assert_eq!(
            credential_storage(&spec),
            CredentialStorage::ProviderKeychain
        );
        assert_eq!(
            credential_entry_prompt(&spec),
            "Store an API key for zai-coding-openai now? (saved to your keychain; GLM_API_KEY still takes precedence)"
        );
    }

    #[test]
    fn offline_key_check_rejects_short_whitespace_and_config_injection() {
        assert!(well_formed_key("sk-valid_123456"));
        assert!(!well_formed_key("short"));
        assert!(!well_formed_key("sk-value with-space"));
        assert!(!well_formed_key("sk-value\nheader=bad"));
        assert!(!well_formed_key("sk-value\"header"));
    }

    #[test]
    fn report_and_summary_builders_never_render_secret_values() {
        let preflight = PreflightReport {
            runtime: RuntimeCheck::Pass("Docker".to_string()),
            images: vec![
                ImageCheck::Present("capscode/gateway:0.1.0".to_string()),
                ImageCheck::Missing("capscode/capsule-claude:1".to_string()),
            ],
        };
        let report = build_preflight_report(&preflight, Caps::PLAIN);
        assert!(report.contains(&crate::build_context::image_build_hint()));
        assert!(!report.contains(SECRET));

        let summary = build_summary(
            Path::new("/tmp/project"),
            false,
            &[DefaultSummary {
                tool: ToolArg::Claude,
                provider: "anthropic".to_string(),
            }],
            &[CredentialSummary {
                provider: "anthropic".to_string(),
                source: CredentialSource::Keychain,
            }],
            "VPN: not configured (containment-only)",
            ToolArg::Claude,
        );
        assert!(summary.contains("capscode claude"));
        assert!(!summary.contains(SECRET));
    }

    #[test]
    fn summary_explains_workspace_mode_behavior() {
        let summary = build_summary(
            Path::new("/tmp/project"),
            true,
            &[],
            &[],
            "VPN: not configured (containment-only)",
            ToolArg::Codex,
        );

        assert!(summary.contains("Workspace changes follow the selected workspace mode."));
    }

    #[test]
    fn non_interactive_probe_choice_never_calls_prompt() {
        let selected = probe_choice(&init_args(), false, || -> Result<bool> {
            panic!("non-interactive setup attempted to prompt")
        })
        .unwrap();
        assert!(!selected);
    }

    #[test]
    fn probe_urls_preserve_provider_base_paths() {
        let anthropic = ProviderSpec {
            id: "anthropic".to_string(),
            label: None,
            api_style: ApiStyle::Anthropic,
            base_url: "https://api.example/anthropic".to_string(),
            auth: AuthScheme::XApiKey,
            key_env: "ANTHROPIC_API_KEY".to_string(),
            default_model: Some("claude-opus-4-8".to_string()),
            models: Vec::new(),
            experimental: false,
        };
        let openai = ProviderSpec {
            api_style: ApiStyle::OpenAI,
            base_url: "https://api.example/v1".to_string(),
            ..anthropic.clone()
        };
        assert_eq!(
            probe_url(&anthropic),
            "https://api.example/anthropic/v1/messages"
        );
        assert_eq!(
            probe_url(&openai),
            "https://api.example/v1/chat/completions"
        );
    }

    #[test]
    fn preflight_report_renders_status_glyphs_for_each_outcome() {
        let report = PreflightReport {
            runtime: RuntimeCheck::Fail("docker not found".to_string()),
            images: vec![
                ImageCheck::Present("capscode/gateway:0.1.0".to_string()),
                ImageCheck::Missing("capscode/capsule-claude:1".to_string()),
                ImageCheck::Error("capscode/capsule-codex:1".to_string(), "no tag".to_string()),
                ImageCheck::Skipped("capscode/capsule-pi:1".to_string()),
            ],
        };
        let rendered = build_preflight_report(&report, Caps::PLAIN);
        // Caps::PLAIN: ASCII glyphs, no colour. Each outcome carries its status glyph.
        assert!(
            rendered.contains("[ok]"),
            "present image should show pass glyph"
        );
        assert!(
            rendered.contains("[x]"),
            "missing/error should show fail glyph"
        );
        assert!(rendered.contains("[-]"), "skipped should show dim glyph");
        assert!(rendered.contains("docker not found"));
        assert!(rendered.contains("capsule-claude:1 (missing)"));
        assert!(rendered.contains("capsule-codex:1 (no tag)"));
        assert!(rendered.contains("capsule-pi:1 (runtime unavailable)"));
        assert!(rendered.contains(&crate::build_context::image_build_hint()));
        assert!(
            !rendered.contains('\u{2714}'),
            "no unicode glyph under Caps::PLAIN"
        );
    }
}
