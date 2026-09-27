//! The passive update check: at most once a day, a ~1 KB GET of `channel.json`
//! that runs beside the user's command and prints at most one line afterwards.
//!
//! It must never fail or slow a command beyond its small budget, so every error
//! (network, parse, policy, state file) ends the check silently.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use capscode_core::UpdateConfig;

use super::channel::{self, Channel, ChannelName};
use super::fetch::Fetch;
use super::state::{self, CheckState};

/// The whole check, including waiting for it after the command, stays within this.
pub(crate) const BUDGET: Duration = Duration::from_secs(3);
/// At most one check per day.
pub(crate) const INTERVAL_SECS: u64 = 24 * 60 * 60;

/// What the check needs to know, gathered by the caller so tests need no globals.
pub(crate) struct PassiveInputs {
    /// `CAPSCODE_NO_UPDATE_CHECK` is set to a non-empty value.
    pub env_disabled: bool,
    /// The user's update settings, or `None` when the config failed to load.
    pub config: Option<UpdateConfig>,
    /// `image.toolchain_lock`, so the active toolchain matches what commands use.
    pub lock_override: Option<PathBuf>,
    pub stderr_tty: bool,
    /// The command is not help, completions, version, JSON output, or `update` itself.
    pub eligible: bool,
    pub now_unix: u64,
    pub home: PathBuf,
    /// How this CLI is updated, for the "CapsCode X is available" notice.
    pub cli_update_command: String,
}

pub(crate) struct PassiveCheck {
    receiver: Receiver<Option<Channel>>,
    deadline: Instant,
    channel: ChannelName,
    lock_override: Option<PathBuf>,
    home: PathBuf,
    own_version: String,
    cli_update_command: String,
}

/// Whether a check is due now, before any network or disk write happens.
pub(crate) fn due(inputs: &PassiveInputs, check: &CheckState) -> Option<ChannelName> {
    if inputs.env_disabled || !inputs.eligible || !inputs.stderr_tty {
        return None;
    }
    let config = inputs.config.as_ref()?;
    if !config.check {
        return None;
    }
    let channel = ChannelName::parse(&config.channel).ok()?;
    let last = check.last_attempt_unix;
    // A timestamp from the future (clock moved back) does not suppress checks forever.
    let recent = last <= inputs.now_unix && inputs.now_unix - last < INTERVAL_SECS;
    (!recent).then_some(channel)
}

/// Start the check in the background if it is due.
pub(crate) fn start_with(inputs: PassiveInputs, fetcher: Arc<dyn Fetch>) -> Option<PassiveCheck> {
    let started = Instant::now();
    let path = state::check_state_path(&inputs.home);
    let mut check = state::read_check_state(&path);
    let channel = due(&inputs, &check)?;
    // Record the attempt first so a failing network backs off for a day as well.
    check.last_attempt_unix = inputs.now_unix;
    state::write_json(&path, &check).ok()?;
    let (sender, receiver) = mpsc::channel();
    let url = channel.manifest_url();
    let worker_channel = channel.clone();
    std::thread::Builder::new()
        .name("capscode-update-check".to_owned())
        .spawn(move || {
            let manifest = fetcher
                .get(&url, channel::MAX_MANIFEST_BYTES)
                .ok()
                .and_then(|bytes| channel::parse(&bytes, &worker_channel).ok());
            let _ = sender.send(manifest);
        })
        .ok()?;
    Some(PassiveCheck {
        receiver,
        deadline: started + BUDGET,
        channel,
        lock_override: inputs.lock_override,
        home: inputs.home,
        own_version: super::own_version().to_owned(),
        cli_update_command: inputs.cli_update_command,
    })
}

impl PassiveCheck {
    #[cfg(test)]
    pub(crate) fn with_version(mut self, version: &str) -> Self {
        self.own_version = version.to_owned();
        self
    }

    /// Wait for the result within the remaining budget and return the notice, if any.
    pub(crate) fn finish(self) -> Option<String> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        let manifest = self.receiver.recv_timeout(remaining).ok()??;
        let path = state::check_state_path(&self.home);
        let mut check = state::read_check_state(&path);
        if check.remember(&self.channel, &manifest) {
            let _ = state::write_json(&path, &check);
        }
        // When the fetched copy is older than the stored one, the notice comes from
        // the newer manifest so a withdrawal it announced is still reported.
        let manifest = check
            .manifest_for(&self.channel)
            .cloned()
            .unwrap_or(manifest);
        let mut active = crate::toolchain::resolve_with_runtime_path(
            self.lock_override.as_deref(),
            &self.home.join("toolchain.lock.toml"),
        )
        .ok()?;
        // Judge the toolchain commands will use; the check itself changes nothing.
        let builtin = crate::toolchain::builtin_lock().ok()?;
        if let super::superseded::Choice::UseBuiltin(_) =
            super::superseded::decide(&active, &self.home, &self.own_version, &builtin)
        {
            active.lock = builtin;
        }
        let update_state = state::read_update_state(&state::update_state_path(&self.home)).ok()?;
        let digest = crate::toolchain::digest(&active.lock);
        let sequence = super::active_sequence(&active.lock, &update_state);
        super::notice_line(
            &manifest,
            &self.own_version,
            &self.cli_update_command,
            &digest,
            &active.lock,
            sequence,
        )
    }
}

/// Start the check for this process, reading the real environment and config.
pub(crate) fn start(eligible: bool) -> Option<PassiveCheck> {
    use std::io::IsTerminal;

    let env_disabled =
        std::env::var_os("CAPSCODE_NO_UPDATE_CHECK").is_some_and(|value| !value.is_empty());
    if env_disabled || !eligible {
        return None;
    }
    let config = capscode_core::Config::load().ok();
    let inputs = PassiveInputs {
        env_disabled,
        lock_override: config
            .as_ref()
            .and_then(|config| config.image.toolchain_lock.clone()),
        config: config.map(|config| config.update),
        stderr_tty: std::io::stderr().is_terminal(),
        eligible,
        now_unix: super::now_unix(),
        home: crate::toolchain::config_home(),
        cli_update_command: super::cli::update_command(),
    };
    start_with(inputs, Arc::new(super::fetch::https_fetcher(BUDGET)))
}
