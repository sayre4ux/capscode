//! `capscode-broker` binary — the secret broker that runs as PID 1 inside the
//! gateway container .
//!
//! Non-secret configuration (loopback listen endpoint, CA export path, secrets
//! directory) is read from the environment the gateway injects (see
//! `capscode-net::spec::gateway_spec`). The real provider keys are NOT in env —
//! they are read once from `${CAPSCODE_SECRETS_DIR}/keys.env`, a file the host CLI
//! wrote into the gateway tmpfs after start, which this process `unlink`s the
//! instant it has loaded it.
//!
//! Security posture:
//!   * The per-session CA is generated **in-process** ([`SessionCa::generate`]).
//!     The CA private key is created here and never leaves this process — only the
//!     public CA certificate PEM is written out, for the orchestrator to install
//!     in the capsule trust store. The CA private key never
//!     crosses the boundary into the capsule.
//!   * Provider keys are wrapped in [`secrecy::SecretString`] the instant they are
//!     read and are never logged (only a count and the provider names appear); the
//!     raw file buffer is zeroized and the file unlinked right after loading.
//!   * The real keys live only in this gateway process; the capsule shares only
//!     the network namespace, not this process's memory or environment.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use secrecy::SecretString;
use zeroize::Zeroize;

use capscode_broker::{Broker, ProviderManifest, SessionCa};

/// Loopback endpoint the broker binds inside the gateway netns when unset.
const DEFAULT_LISTEN: &str = "127.0.0.1:8443";
/// Default path the per-session CA cert PEM is exported to for the orchestrator
/// to install into the capsule trust store.
const DEFAULT_CA_OUT: &str = "/etc/capscode/broker-ca.pem";

/// Non-secret providers manifest (JSON array). The CLI
/// sets this in the gateway `ContainerSpec.env`; the broker builds its entire
/// route table + allowlist from it.
const ENV_PROVIDERS: &str = "CAPSCODE_PROVIDERS";
/// Loopback endpoint override (matches `capscode-net::spec::ENV_BROKER_LISTEN`).
const ENV_LISTEN: &str = "CAPSCODE_BROKER_LISTEN";
/// Path to write the exported CA certificate PEM (public cert only).
const ENV_CA_OUT: &str = "CAPSCODE_CA_CERT_OUT";
/// Directory of the secrets tmpfs (matches `capscode-net::spec::ENV_SECRETS_DIR`).
const ENV_SECRETS_DIR: &str = "CAPSCODE_SECRETS_DIR";
/// Filename, within `CAPSCODE_SECRETS_DIR`, holding `<VAR>=<key>` lines.
const KEYS_FILE: &str = "keys.env";

/// Validated startup configuration, assembled synchronously before any async work.
struct Startup {
    /// The parsed route table + allowlist, from `CAPSCODE_PROVIDERS`.
    manifest: ProviderManifest,
    /// Real provider credentials keyed by `key_env`, from `keys.env` (the file is
    /// unlinked once loaded). At least one entry is present (checked at load).
    creds: HashMap<String, SecretString>,
    listen: SocketAddr,
    ca_out: PathBuf,
    /// `true` when `CAPSCODE_CA_CERT_OUT` was explicitly set — a write failure is
    /// then fatal (the operator asked for it); the default path is best-effort.
    ca_out_required: bool,
}

fn usage() -> String {
    format!(
        "capscode-broker {ver} — CapsCode secret broker (gateway sidecar)\n\
\n\
Runs as PID 1 inside the gateway container. All configuration is read from the\n\
environment injected by capscode-net::spec::gateway_spec.\n\
\n\
Environment:\n\
  {providers}\n\
      non-secret providers manifest (JSON array). Builds the entire route table\n\
      and the default-deny allowlist. Required; an invalid manifest fails closed.\n\
  {secrets_dir}\n\
      directory of the secrets tmpfs; provider key(s) are read once from\n\
      <dir>/keys.env (then unlinked). At least one key matching a manifest\n\
      key_env is required. Never logged.\n\
  {listen}\n\
      loopback endpoint to bind (default {default_listen}).\n\
  {ca_out}\n\
      path to export the per-session CA certificate PEM for capsule trust\n\
      injection (default {default_ca_out}). Only the public cert is written;\n\
      the CA private key never leaves this process.\n\
\n\
Options:\n\
  -h, --help       print this help and exit\n\
  -V, --version    print version and exit\n",
        ver = env!("CARGO_PKG_VERSION"),
        providers = ENV_PROVIDERS,
        secrets_dir = ENV_SECRETS_DIR,
        listen = ENV_LISTEN,
        ca_out = ENV_CA_OUT,
        default_listen = DEFAULT_LISTEN,
        default_ca_out = DEFAULT_CA_OUT,
    )
}

fn main() -> ExitCode {
    // Handle --help / --version without a runtime or an arg-parser dependency.
    // The broker takes no positional arguments; all config comes from the env.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", usage());
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("capscode-broker {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("capscode-broker: unknown argument {other:?}\n\n{}", usage());
                return ExitCode::from(2);
            }
        }
    }

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    // Read secrets/config from the environment synchronously, before the async
    // runtime spins up any worker threads.
    let startup = match load_config() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "broker startup configuration invalid — refusing to start");
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to build tokio runtime");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(startup)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "broker terminated with error");
            ExitCode::FAILURE
        }
    }
}

/// Assemble the broker configuration from the environment. Fails closed on a
/// missing/invalid manifest and on the absence of any usable credential (a broker
/// with nothing to broker is a misconfiguration and would otherwise 502/401 every
/// request silently).
fn load_config() -> Result<Startup, String> {
    let manifest = load_manifest()?;
    let creds = load_credentials(&manifest)?;

    let listen: SocketAddr = std::env::var(ENV_LISTEN)
        .unwrap_or_else(|_| DEFAULT_LISTEN.to_string())
        .parse()
        .map_err(|e| format!("{ENV_LISTEN} is not a valid socket address: {e}"))?;

    let (ca_out, ca_out_required) = match std::env::var(ENV_CA_OUT) {
        Ok(p) if !p.is_empty() => (PathBuf::from(p), true),
        _ => (PathBuf::from(DEFAULT_CA_OUT), false),
    };

    Ok(Startup {
        manifest,
        creds,
        listen,
        ca_out,
        ca_out_required,
    })
}

/// Parse the non-secret providers manifest from `CAPSCODE_PROVIDERS`. Fails closed:
/// an unset/empty var or an invalid manifest refuses startup — the broker never
/// serves an empty or ambiguous route table.
fn load_manifest() -> Result<ProviderManifest, String> {
    let raw = std::env::var(ENV_PROVIDERS)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            format!("{ENV_PROVIDERS} is unset; the gateway did not provide a providers manifest")
        })?;
    ProviderManifest::from_json(&raw).map_err(|e| format!("{ENV_PROVIDERS} invalid: {e}"))
}

/// Load the provider keys from `${CAPSCODE_SECRETS_DIR}/keys.env`, keeping only the
/// keys referenced by a manifest `key_env`, then `unlink` the file so the secret
/// lives only in this process's memory. Fails closed: an unset directory, an
/// unreadable file, or no key matching any manifest `key_env` refuses startup. A
/// manifest provider whose key is simply absent is not an error here — it becomes
/// unroutable (its requests fail auth downstream), as long as at least one other
/// provider has a usable key.
fn load_credentials(manifest: &ProviderManifest) -> Result<HashMap<String, SecretString>, String> {
    let dir = std::env::var(ENV_SECRETS_DIR)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            format!("{ENV_SECRETS_DIR} is unset; the gateway did not provision a secrets tmpfs")
        })?;
    let keys_path = PathBuf::from(&dir).join(KEYS_FILE);

    let mut raw = std::fs::read_to_string(&keys_path).map_err(|e| {
        format!(
            "cannot read provider keys from {}: {e}",
            keys_path.display()
        )
    })?;

    // Unlink immediately after loading into memory — tmpfs, but we still remove it
    // so a later exec into the gateway cannot read the key material off the file.
    if let Err(e) = std::fs::remove_file(&keys_path) {
        tracing::warn!(
            path = %keys_path.display(),
            error = %e,
            "could not unlink keys.env after load"
        );
    }

    // Only retain keys the manifest actually references; stray secrets are never
    // held in memory (dropping a SecretString zeroizes it).
    let wanted: HashSet<&str> = manifest.key_envs().collect();

    let mut creds: HashMap<String, SecretString> = HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if value.is_empty() {
            continue;
        }
        if wanted.contains(name) {
            creds.insert(name.to_string(), SecretString::from(value.to_string()));
        }
    }
    // Scrub the raw file contents from our own buffer; the SecretStrings own copies.
    raw.zeroize();

    if creds.is_empty() {
        let mut expected: Vec<&str> = manifest.key_envs().collect();
        expected.sort_unstable();
        expected.dedup();
        return Err(format!(
            "no usable provider credential in {} (expected at least one of: {}); \
             refusing to start a broker with nothing to broker",
            keys_path.display(),
            expected.join(", ")
        ));
    }
    Ok(creds)
}

async fn run(startup: Startup) -> Result<(), Box<dyn std::error::Error>> {
    let vhosts: Vec<String> = startup.manifest.vhosts().map(|s| s.to_string()).collect();
    tracing::info!(
        providers = startup.manifest.len(),
        usable_credentials = startup.creds.len(),
        vhosts = ?vhosts,
        "broker configuration loaded"
    );

    // Fresh per-session CA whose leaf covers exactly the manifest vhosts, so TLS
    // termination works for any user-selected provider. The CA private key is
    // generated here and never leaves this process; only the public cert
    // PEM is exported below.
    let ca = SessionCa::generate_with_sans(&vhosts)?;

    // Export the public CA cert so the orchestrator can install it into the
    // capsule trust store. The private key is never written.
    if let Some(parent) = startup.ca_out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&startup.ca_out, ca.ca_cert_pem()) {
        Ok(()) => tracing::info!(
            path = %startup.ca_out.display(),
            "exported session CA certificate for capsule trust injection"
        ),
        Err(e) if startup.ca_out_required => {
            return Err(format!(
                "failed to write CA cert to {}: {e}",
                startup.ca_out.display()
            )
            .into());
        }
        Err(e) => tracing::warn!(
            path = %startup.ca_out.display(),
            error = %e,
            "could not export CA cert; capsule trust must be injected another way"
        ),
    }

    let listen = startup.listen;
    let broker = Broker::build(&startup.manifest, startup.creds, &ca)?;

    tracing::info!(%listen, "secret broker listening");
    broker.serve(listen, shutdown_signal()).await?;
    tracing::info!("secret broker stopped cleanly");
    Ok(())
}

/// Resolve when the broker should shut down: SIGTERM (container stop) or SIGINT
/// (Ctrl-C). Returning from this future drives `Broker::serve` to a clean stop.
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot install SIGTERM handler; using Ctrl-C only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT, shutting down"),
        _ = term.recv() => tracing::info!("received SIGTERM, shutting down"),
    }
}
