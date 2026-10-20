use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use capscode_core::{HistoryDial, RuntimePref, Tool};
use clap::{Arg, Args, Command as ClapCommand, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

#[derive(Debug, Parser)]
#[command(
    name = "capscode",
    version,
    about = "Disposable, privacy-first AI coding capsules",
    after_help = "capscode <tool> [args] run one tool once\ncapscode [<box>] enter a box (this directory's box by default)\ncapscode create|ls|stop|resume|sync|rm manage boxes\ncapscode toolchain show|ls|prune|build|use|pin manage tool versions\ncapscode update [--check] [--cli] apply a newer tested toolchain or CLI release"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Select the container runtime.
    #[arg(long, global = true, value_enum)]
    pub runtime: Option<RuntimeArg>,

    /// Show detailed progress.
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Set up tools, credentials, and defaults.
    Init(InitArgs),
    /// Generate shell completion scripts.
    #[command(hide = true)]
    Completions {
        /// Shell to generate completions for.
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Removed; invoke the tool name directly.
    #[command(hide = true)]
    Run {
        #[arg(num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<OsString>,
    },
    /// Removed; enter a box by name.
    #[command(hide = true)]
    Attach { session: Option<String> },
    /// Removed; list boxes and enter one by name.
    #[command(hide = true)]
    Console,
    /// Removed; use `resume`.
    #[command(hide = true)]
    Start(BoxResumeArgs),
    /// Create a persistent box for this project.
    Create(BoxCreateArgs),
    /// Restart a stopped box, keeping its files.
    Resume(BoxResumeArgs),
    /// Inspect and select toolchain snapshots.
    Toolchain {
        #[command(subcommand)]
        command: ToolchainCommand,
    },
    /// Check the release channel and apply a newer tested toolchain, or with
    /// `--cli` install the newest CapsCode release binary.
    Update {
        /// Only report what is available; change nothing.
        #[arg(long)]
        check: bool,
        /// Accept a recorded tool downgrade, or the binary replacement with `--cli`, without asking.
        #[arg(short = 'y', long)]
        yes: bool,
        /// Update the CapsCode binary itself (tagged releases only) instead of the toolchain.
        #[arg(long)]
        cli: bool,
        /// With `--cli`, allow installing a release older than the running one.
        #[arg(long, requires = "cli")]
        allow_downgrade: bool,
    },
    /// Stop a box and keep its files.
    Stop {
        /// Box name, prefix, or id.
        target: String,
    },
    /// List boxes and sessions.
    Ls {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove a box or one-shot session.
    Rm {
        /// Box name, prefix, id, or one-shot session UUID.
        target: String,
        /// Skip the unsynced-work refusal; the box is stopped before copy-back.
        #[arg(short = 'f', long)]
        force: bool,
        /// Delete the box or session workspace without copying its files back.
        #[arg(long)]
        discard_workspace: bool,
    },
    /// Remove CapsCode's data from this machine.
    #[command(alias = "purge")]
    Uninstall {
        /// Also remove locally tagged capscode/* images.
        #[arg(long)]
        images: bool,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
        /// Delete box workspaces and kept file sync data even if they hold work the host does not have.
        #[arg(long)]
        discard_workspace: bool,
    },
    /// Copy changes back to your project.
    Sync {
        /// Box name, prefix, id, or session UUID.
        target: String,
    },
    /// Check the runtime, network, and credentials.
    Doctor {
        /// Show the local, non-runtime health summary.
        #[arg(long)]
        quick: bool,
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Removed; use `doctor --quick`.
    #[command(hide = true)]
    Status {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show or configure the WireGuard VPN.
    Vpn {
        #[command(subcommand)]
        command: Option<VpnCommand>,
    },
    /// Inspect or manage the Claude setup-token in the OS keychain.
    #[command(hide = true)]
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Manage API providers.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// Manage stored API keys.
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// View or change settings.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// Nested `capscode config` verbs.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// List settings and their effective values.
    List {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print one setting.
    Get {
        /// Configuration key to read.
        key: ConfigKeyArg,
    },
    /// Change one setting.
    Set {
        /// Configuration key to update.
        key: ConfigKeyArg,
        /// New value.
        value: String,
    },
    /// Remove one setting.
    Unset {
        /// Configuration key to remove.
        key: ConfigKeyArg,
    },
    /// Open the config file in $EDITOR.
    Edit,
}

#[derive(Debug, Subcommand)]
pub enum ToolchainCommand {
    /// Show the active lock and each tool's testing and cooldown details.
    Show,
    /// List saved snapshots and locally available CapsCode images.
    Ls,
    /// Remove unreferenced tool images and keep the selected snapshots.
    Prune {
        /// Number of newest snapshots beyond current and box references to keep.
        #[arg(long, default_value_t = 1)]
        keep: usize,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Print guidance for manually pruning BuildKit's CapsCode cache.
        #[arg(long)]
        build_cache: bool,
    },
    /// Rebuild images from a saved snapshot.
    Build {
        /// Snapshot digest; defaults to the current toolchain.
        digest: Option<String>,
        /// Skip the tool startup smoke after building images.
        #[arg(long)]
        no_smoke: bool,
    },
    /// Set the global default from a saved digest, or remove it with `built-in`.
    Use {
        digest: String,
        /// Use the toolchain even if the release channel withdrew it.
        #[arg(long)]
        force: bool,
    },
    /// Pin one tool version in an untested runtime lock.
    Pin {
        /// Tool and version, written as <tool>@<version>.
        tool_version: String,
        /// Required acknowledgement that this combination has not been tested.
        #[arg(long)]
        untested: bool,
        /// npm tarball integrity. Get it with `npm view <pkg>@<ver> dist.integrity`.
        #[arg(long)]
        integrity: Option<String>,
    },
}

/// Allowlisted `config get|set|unset` keys in stable list order.
///
/// Public Clap values are the exact dotted strings from the product contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ConfigKeyArg {
    #[value(name = "auth.keychain")]
    AuthKeychain,
    #[value(name = "box.prefix")]
    BoxPrefix,
    #[value(name = "capsule.memory")]
    CapsuleMemory,
    #[value(name = "capsule.pids")]
    CapsulePids,
    #[value(name = "capsule.opencode_lsp_download")]
    CapsuleOpencodeLspDownload,
    #[value(name = "capsule.cache_volume")]
    CapsuleCacheVolume,
    #[value(name = "defaults.claude")]
    DefaultsClaude,
    #[value(name = "defaults.codex")]
    DefaultsCodex,
    #[value(name = "defaults.opencode")]
    DefaultsOpencode,
    #[value(name = "defaults.pi")]
    DefaultsPi,
    #[value(name = "defaults.omp")]
    DefaultsOmp,
    #[value(name = "runtime")]
    Runtime,
    #[value(name = "update.channel")]
    UpdateChannel,
    #[value(name = "update.check")]
    UpdateCheck,
    #[value(name = "update.verify_with_gh")]
    UpdateVerifyWithGh,
    #[value(name = "workspace.mode")]
    WorkspaceMode,
}

impl ConfigKeyArg {
    /// Stable allowlist order for `config list`, dispatch, and tests.
    pub const fn all() -> [Self; 16] {
        [
            Self::AuthKeychain,
            Self::BoxPrefix,
            Self::CapsuleMemory,
            Self::CapsulePids,
            Self::CapsuleOpencodeLspDownload,
            Self::CapsuleCacheVolume,
            Self::DefaultsClaude,
            Self::DefaultsCodex,
            Self::DefaultsOpencode,
            Self::DefaultsPi,
            Self::DefaultsOmp,
            Self::Runtime,
            Self::UpdateChannel,
            Self::UpdateCheck,
            Self::UpdateVerifyWithGh,
            Self::WorkspaceMode,
        ]
    }

    /// Exact public key string (`auth.keychain`, `defaults.claude`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthKeychain => "auth.keychain",
            Self::BoxPrefix => "box.prefix",
            Self::CapsuleMemory => "capsule.memory",
            Self::CapsulePids => "capsule.pids",
            Self::CapsuleOpencodeLspDownload => "capsule.opencode_lsp_download",
            Self::CapsuleCacheVolume => "capsule.cache_volume",
            Self::DefaultsClaude => "defaults.claude",
            Self::DefaultsCodex => "defaults.codex",
            Self::DefaultsOpencode => "defaults.opencode",
            Self::DefaultsPi => "defaults.pi",
            Self::DefaultsOmp => "defaults.omp",
            Self::Runtime => "runtime",
            Self::UpdateChannel => "update.channel",
            Self::UpdateCheck => "update.check",
            Self::UpdateVerifyWithGh => "update.verify_with_gh",
            Self::WorkspaceMode => "workspace.mode",
        }
    }
}

pub fn completion_command() -> ClapCommand {
    completion_command_with_boxes(crate::boxes::index::names())
}

fn tool_display_name(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "Claude Code",
        Tool::Codex => "Codex",
        Tool::Opencode => "OpenCode",
        Tool::Pi => "Pi",
        Tool::Omp => "omp",
    }
}

/// Copy `command` without hidden subcommands or arguments. clap_complete offers hidden
/// subcommands, so removed commands would otherwise crowd every completion list.
fn without_hidden(command: &ClapCommand) -> ClapCommand {
    // clap's builder takes `&'static str` without its `string` feature; completion runs once
    // per process, so the few leaked names are harmless.
    let leak = |text: &str| -> &'static str { Box::leak(text.to_owned().into_boxed_str()) };
    let mut copy = ClapCommand::new(leak(command.get_name()));
    if let Some(about) = command.get_about() {
        copy = copy.about(about.clone());
    }
    if let Some(long_about) = command.get_long_about() {
        copy = copy.long_about(long_about.clone());
    }
    if let Some(version) = command.get_version() {
        copy = copy.version(leak(version));
    }
    for alias in command.get_visible_aliases() {
        copy = copy.visible_alias(leak(alias));
    }
    for argument in command
        .get_arguments()
        .filter(|argument| !argument.is_hide_set())
    {
        copy = copy.arg(argument.clone());
    }
    for group in command.get_groups() {
        copy = copy.group(group.clone());
    }
    let visible: Vec<&ClapCommand> = command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .collect();
    for subcommand in &visible {
        copy = copy.subcommand(without_hidden(subcommand));
    }
    if !visible.is_empty() {
        // clap's generated `help` entry reads "Print this message or the help of the given
        // subcommand(s)"; offer the same completions under a plainer description.
        let mut help = ClapCommand::new("help").about("Show help for a command");
        for subcommand in &visible {
            let mut entry = ClapCommand::new(leak(subcommand.get_name()));
            if let Some(about) = subcommand.get_about() {
                entry = entry.about(about.clone());
            }
            help = help.subcommand(entry);
        }
        copy = copy.disable_help_subcommand(true).subcommand(help);
    }
    copy
}

pub fn help_command() -> ClapCommand {
    help_command_with_boxes(crate::boxes::index::names())
}

fn help_command_with_boxes(names: Vec<String>) -> ClapCommand {
    let mut command = Cli::command();
    if !names.is_empty() {
        let mut help = "capscode <tool> [args] run one tool once\ncapscode [<box>] enter a box (this directory's box by default)\ncapscode create|ls|stop|resume|sync|rm manage boxes\ncapscode toolchain show|ls|prune|build|use|pin manage tool versions\ncapscode update [--check] [--cli] apply a newer tested toolchain or CLI release\n\nIndexed boxes:".to_string();
        for name in names {
            help.push_str("\n  ");
            help.push_str(&name);
        }
        command = command.after_help(help);
    }
    command
}

fn completion_command_with_boxes(names: Vec<String>) -> ClapCommand {
    let mut command = without_hidden(&Cli::command());
    for name in names {
        let name: &'static str = Box::leak(name.into_boxed_str());
        // zsh folds entries with identical descriptions onto one line, so each gets its own.
        command = command.subcommand(ClapCommand::new(name).about(format!("Enter box {name}")));
    }
    for tool in Tool::ALL {
        let about = format!("Run {} once in a fresh capsule", tool_display_name(tool));
        let tool = tool.name();
        command = command.subcommand(
            ClapCommand::new(tool)
                .about(about)
                .arg(
                    Arg::new("mount")
                        .long("mount")
                        .help("Write directly to the host project without copy-back review.")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("copy")
                        .long("copy")
                        .help("Copy the project into the capsule.")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("pick")
                        .long("pick")
                        .help("Choose a provider for this run.")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("providers")
                        .long("providers")
                        .value_name("IDS")
                        .help("Route additional providers through the gateway."),
                )
                .arg(
                    Arg::new("history")
                        .long("history")
                        .value_name("MODE")
                        .help("Choose how much Git history to copy.")
                        .value_parser(["full", "shallow", "redact-authors"]),
                )
                .arg(
                    Arg::new("include")
                        .long("include")
                        .value_name("PATH")
                        .help("Include an untracked path in the copy.")
                        .action(clap::ArgAction::Append),
                )
                .arg(
                    Arg::new("provider")
                        .long("provider")
                        .value_name("ID")
                        .help("Select a provider."),
                )
                .arg(
                    Arg::new("yes")
                        .long("yes")
                        .help("Accept confirmation prompts.")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("experimental")
                        .long("experimental")
                        .help("Allow an experimental pairing.")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("verbose")
                        .short('v')
                        .long("verbose")
                        .help("Show detailed progress.")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("tool_args")
                        .help("Arguments to pass to the AI tool.")
                        .num_args(0..)
                        .allow_hyphen_values(true)
                        .trailing_var_arg(true),
                ),
        );
    }
    command
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Configure one or more tools (repeatable).
    #[arg(long, value_enum)]
    pub tool: Vec<ToolArg>,

    /// Probe configured provider credentials over the network.
    #[arg(long, conflicts_with = "no_probe")]
    pub probe: bool,

    /// Do not probe provider credentials over the network.
    #[arg(long, conflicts_with = "probe")]
    pub no_probe: bool,

    /// Accept defaults and never prompt.
    #[arg(long)]
    pub yes: bool,

    /// Install completion for this shell without prompting.
    #[arg(long, value_enum, conflicts_with = "no_completions")]
    pub completion_shell: Option<CompletionShellArg>,

    /// Skip shell-completion setup.
    #[arg(long, conflicts_with = "completion_shell")]
    pub no_completions: bool,
}

#[derive(Clone, Debug, Args)]
pub struct BoxCreateArgs {
    /// Optional project box name.
    pub name: Option<String>,
    /// Route all providers or a comma-separated provider id list.
    #[arg(long)]
    pub providers: Option<String>,
    /// Bind-mount this folder instead of copying it: the agent writes straight to your files,
    /// with no copy-back review.
    #[arg(long)]
    pub mount: bool,
    /// Skip confirmation prompts.
    #[arg(long)]
    pub yes: bool,
    /// Keep this box on its current toolchain when a newer default appears.
    #[arg(long)]
    pub pin: bool,
    /// Keep helper caches in temporary storage instead of a persistent box volume.
    #[arg(long)]
    pub no_cache: bool,
}

#[derive(Clone, Debug, Args)]
pub struct BoxResumeArgs {
    /// Box name, prefix, or id.
    pub target: String,
    /// Replace the routed set.
    #[arg(long)]
    pub providers: Option<String>,
    /// Skip confirmation prompts.
    #[arg(long)]
    pub yes: bool,
    /// Select `current`, `keep`, or a saved toolchain digest.
    #[arg(long, value_name = "CURRENT|KEEP|DIGEST")]
    pub toolchain: Option<String>,
    /// Pin this box to the selected toolchain.
    #[arg(long)]
    pub pin: bool,
    /// Recreate the box's helper cache volume before resuming.
    #[arg(long)]
    pub fresh_cache: bool,
    /// Enable a persistent helper-cache volume for this box.
    #[arg(long)]
    pub cache: bool,
    /// Allow `--toolchain <digest>` to select a withdrawn toolchain.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Subcommand)]
pub enum VpnCommand {
    /// Import a WireGuard config file.
    Import {
        /// Path to a native WireGuard configuration file.
        path: PathBuf,
    },
    /// Show the configured WireGuard file and its validation status.
    #[command(hide = true)]
    Status,
    /// Stop using the imported VPN (keeps the file).
    Remove,
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Show credential and keychain status.
    #[command(hide = true)]
    Status,
    /// Read a Claude setup-token and store it in the OS keychain.
    Import,
    /// Remove the Claude setup-token from the OS keychain.
    Remove,
}

impl Command {
    pub fn into_invocation(self) -> Result<Invocation> {
        let Self::External(words) = self else {
            let pointer = match &self {
                Self::Run { .. } => Some("run is now a tool name, such as `capscode claude`"),
                Self::Status { .. } => Some("status is now `doctor --quick`"),
                Self::Attach { .. } => Some("attach is now `capscode <box>`"),
                Self::Console => {
                    Some("console is now `capscode ls`; enter a listed box with `capscode <box>`")
                }
                Self::Start(_) => Some("start is now `resume <box>`"),
                Self::Auth {
                    command: AuthCommand::Import,
                } => Some("auth import is now `key set anthropic`"),
                Self::Auth {
                    command: AuthCommand::Remove,
                } => Some("auth remove is now `key rm anthropic`"),
                Self::Auth {
                    command: AuthCommand::Status,
                } => Some("auth status is now `key ls`"),
                Self::Provider {
                    command: ProviderCommand::SetDefault { .. },
                    ..
                } => Some("provider set-default is now `config set defaults.<tool> <provider>`"),
                Self::Provider {
                    command:
                        ProviderCommand::Key {
                            command: ProviderKeyCommand::Set { .. },
                        },
                    ..
                } => Some("provider key set is now `key set <provider>`"),
                Self::Provider {
                    command:
                        ProviderCommand::Key {
                            command: ProviderKeyCommand::Rm { .. },
                        },
                    ..
                } => Some("provider key rm is now `key rm <provider>`"),
                Self::Provider {
                    command:
                        ProviderCommand::Key {
                            command: ProviderKeyCommand::Ls,
                        },
                    ..
                } => Some("provider key ls is now `key ls`"),
                Self::Provider {
                    command:
                        ProviderCommand::Key {
                            command: ProviderKeyCommand::Reset,
                        },
                    ..
                } => Some("provider key reset is now `key reset`"),
                Self::Vpn {
                    command: Some(VpnCommand::Status),
                    ..
                } => Some("vpn status is now bare `vpn`"),
                _ => None,
            };
            return Ok(if let Some(pointer) = pointer {
                Invocation::Removed(pointer.to_string())
            } else {
                Invocation::Execute(self)
            });
        };
        let Some((word, rest)) = words.split_first() else {
            bail!("missing command");
        };
        let name = word.to_string_lossy();
        let tool = ToolArg::from_str(&name, true).ok();
        let Some(tool) = tool else {
            let names = crate::boxes::index::names();
            let indexed_match = names.iter().any(|box_name| {
                box_name.starts_with(name.as_ref())
                    || box_name.strip_suffix(&format!("-{name}")).is_some()
            });
            let resolved = if indexed_match {
                crate::boxes::resolve(&name)?
            } else {
                crate::boxes::Resolved::None
            };
            return resolve_box_word(&name, rest.first().map(OsString::as_os_str), resolved);
        };

        let mut provider = None;
        let mut providers: Option<String> = None;
        let mut pick = false;
        let mut mount = false;
        let mut copy = false;
        let mut history = HistoryArg::Full;
        let mut include = Vec::new();
        let mut yes = false;
        let mut experimental = false;
        let mut verbose = false;
        let mut help = false;
        let mut version = false;
        let mut tool_args = Vec::new();
        let mut index = 0;
        let mut forwarding = false;
        while index < rest.len() {
            let word = &rest[index];
            if forwarding {
                tool_args.push(word.to_string_lossy().into_owned());
                index += 1;
                continue;
            }
            let text = word.to_string_lossy();
            match text.as_ref() {
                "--" => forwarding = true,
                "--help" | "-h" => help = true,
                "--version" | "-V" => version = true,
                "--yes" => yes = true,
                "--experimental" => experimental = true,
                "--verbose" | "-v" => verbose = true,
                "--mount" => mount = true,
                "--copy" => copy = true,
                "--pick" => pick = true,
                "--providers" => {
                    index += 1;
                    let value = rest
                        .get(index)
                        .context("--providers requires all|none|<id,id>")?
                        .to_str()
                        .context("--providers requires a UTF-8 value")?;
                    providers = Some(value.to_string());
                }
                "--history" => {
                    index += 1;
                    history =
                        alias_history_arg(rest.get(index).context("--history requires a mode")?)?;
                }
                "--include" => {
                    index += 1;
                    include.push(
                        rest.get(index)
                            .cloned()
                            .map(PathBuf::from)
                            .context("--include requires a path")?,
                    );
                }
                "--provider" => {
                    index += 1;
                    let value = rest
                        .get(index)
                        .and_then(|value| value.to_str())
                        .context("--provider requires a UTF-8 provider id")?;
                    if value.is_empty() {
                        bail!("--provider requires a provider id");
                    }
                    provider = Some(value.to_string());
                }
                _ if text.starts_with("--provider=") => {
                    let value = text.trim_start_matches("--provider=");
                    if value.is_empty() {
                        bail!("--provider requires a provider id");
                    }
                    provider = Some(value.to_string());
                }
                _ if text.starts_with("--providers=") => {
                    providers = Some(text.trim_start_matches("--providers=").to_string());
                }
                _ if text.starts_with("--history=") => {
                    history = HistoryArg::from_str(text.trim_start_matches("--history="), true)
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "invalid value for --history; expected full, shallow, or redact-authors"
                            )
                        })?;
                }
                _ if text.starts_with("--include=") => {
                    include.push(PathBuf::from(text.trim_start_matches("--include=")));
                }
                _ => tool_args.push(text.into_owned()),
            }
            index += 1;
        }

        if version {
            return Ok(Invocation::ShowVersion);
        }
        if help {
            return Ok(Invocation::ShowAliasHelp { tool, provider });
        }

        Ok(Invocation::RunTool(RunArgs {
            tool,
            mount,
            copy,
            history,
            include,
            provider,
            providers,
            pick,
            yes,
            experimental,
            verbose,
            tool_args,
        }))
    }
}

fn resolve_box_word(
    name: &str,
    following_word: Option<&std::ffi::OsStr>,
    resolved: crate::boxes::Resolved,
) -> Result<Invocation> {
    Ok(match resolved {
        crate::boxes::Resolved::One(target) if following_word.is_none() => {
            Invocation::EnterBox(target)
        }
        crate::boxes::Resolved::One(target) => {
            let Some(following_word) = following_word.and_then(std::ffi::OsStr::to_str) else {
                return Ok(Invocation::Removed(
                    "box command arguments must be valid UTF-8".into(),
                ));
            };
            Invocation::Removed(format!(
                "`{}` is a box; enter it with `capscode {}` and run `{}` there",
                target.name, target.name, following_word
            ))
        }
        crate::boxes::Resolved::Ambiguous(matches) => {
            let names = matches
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Invocation::Removed(format!(
                "box name '{name}' is ambiguous; matches: {names}. Type more of the name to pick one"
            ))
        }
        crate::boxes::Resolved::None => {
            let suggestion = did_you_mean(name);
            if let Some(suggestion) = suggestion {
                Invocation::Removed(format!(
                    "no box or tool named '{name}'. Did you mean '{suggestion}'?"
                ))
            } else {
                Invocation::Removed(format!("no box or tool named '{name}'"))
            }
        }
    })
}

#[derive(Debug)]
pub enum Invocation {
    Execute(Command),
    RunTool(RunArgs),
    EnterBox(crate::boxes::BoxRef),
    Removed(String),
    ShowAliasHelp {
        tool: ToolArg,
        provider: Option<String>,
    },
    ShowVersion,
}

pub fn did_you_mean(input: &str) -> Option<String> {
    let mut candidates = vec![
        "init",
        "create",
        "resume",
        "stop",
        "ls",
        "sync",
        "rm",
        "doctor",
        "key",
        "provider",
        "config",
        "vpn",
        "uninstall",
        "toolchain",
        "update",
    ];
    candidates.extend(Tool::ALL.iter().map(|tool| tool.name()));
    let names = crate::boxes::index::names();
    candidates.extend(names.iter().map(String::as_str));
    candidates
        .into_iter()
        .map(|candidate| (edit_distance(input, candidate), candidate))
        .filter(|(distance, _)| *distance <= 2)
        .min_by_key(|(distance, candidate)| (*distance, *candidate))
        .map(|(_, candidate)| candidate.to_string())
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut row: Vec<usize> = (0..=right.chars().count()).collect();
    for (i, a) in left.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, b) in right.chars().enumerate() {
            let old = row[j + 1];
            row[j + 1] = (row[j + 1] + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(a != b));
            diagonal = old;
        }
    }
    row[right.chars().count()]
}

#[derive(Clone, Debug, Args)]
pub struct RunArgs {
    /// AI tool to run.
    #[arg(value_enum)]
    pub tool: ToolArg,

    /// Bind-mount the project directory into the capsule.
    #[arg(long)]
    pub mount: bool,

    /// Copy the project into the capsule (default).
    #[arg(long)]
    pub copy: bool,

    /// Choose how much Git history to copy.
    #[arg(long, value_enum, default_value = "full")]
    pub history: HistoryArg,

    /// Include an untracked path in the copy.
    #[arg(long)]
    pub include: Vec<PathBuf>,

    /// Select a provider.
    #[arg(long)]
    pub provider: Option<String>,

    /// Choose a provider for this run without changing the saved default.
    #[arg(long)]
    pub pick: bool,

    /// Route `all`, `none`, or selected providers' credentials into the box.
    #[arg(long)]
    pub providers: Option<String>,

    /// Accept confirmation prompts non-interactively.
    #[arg(long)]
    pub yes: bool,

    /// Allow an experimental tool/provider combination.
    #[arg(long)]
    pub experimental: bool,

    #[arg(last = true)]
    /// Arguments to pass to the AI tool.
    pub tool_args: Vec<String>,

    /// Captured only by the external alias parser; normal `run` uses the global flag.
    #[arg(skip)]
    pub verbose: bool,
}

fn alias_history_arg(value: &OsString) -> Result<HistoryArg> {
    let value = value.to_str().context("--history requires a UTF-8 mode")?;
    HistoryArg::from_str(value, true).map_err(|_| {
        anyhow::anyhow!(
            "invalid value '{value}' for --history; expected full, shallow, or redact-authors"
        )
    })
}

impl RunArgs {
    pub(crate) fn interactive_handoff(
        &self,
        stdin_is_terminal: bool,
        stdout_is_terminal: bool,
    ) -> bool {
        !self.yes
            && stdin_is_terminal
            && stdout_is_terminal
            && !is_batch_invocation(self.tool, &self.tool_args)
    }
}

fn is_batch_invocation(tool: ToolArg, args: &[String]) -> bool {
    match tool {
        // omp spells its non-interactive mode the same way Claude Code does
        // (`-p` / `--print`); `--print-thoughts` only decorates that mode.
        ToolArg::Claude | ToolArg::Omp => args
            .iter()
            .any(|arg| arg == "-p" || arg == "--print" || arg.starts_with("--print=")),
        ToolArg::Codex => first_positional_arg(args, CODEX_OPTIONS_WITH_VALUES) == Some("exec"),
        ToolArg::Opencode => first_positional_arg(args, &[]) == Some("run"),
        ToolArg::Pi => false,
    }
}

const CODEX_OPTIONS_WITH_VALUES: &[&str] = &[
    "-c",
    "--config",
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
    "-i",
    "--image",
    "-m",
    "--model",
    "--local-provider",
    "-p",
    "--profile",
    "-s",
    "--sandbox",
    "-C",
    "--cd",
    "--add-dir",
    "-a",
    "--ask-for-approval",
];

fn first_positional_arg<'a>(args: &'a [String], options_with_values: &[&str]) -> Option<&'a str> {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            return args.get(index + 1).map(String::as_str);
        }
        if !arg.starts_with('-') || arg == "-" {
            return Some(arg);
        }

        let option = arg.split_once('=').map_or(arg, |(option, _)| option);
        if !arg.contains('=') && options_with_values.contains(&option) {
            index += 1;
        }
        index += 1;
    }
    None
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// List all providers.
    List,
    /// Show one provider's details.
    Show {
        /// Provider ID.
        id: String,
    },
    /// Add a custom provider.
    Add(ProviderAddArgs),
    /// Remove a custom provider or override.
    Remove {
        /// Provider ID.
        id: String,
    },
    /// Persist a provider as a tool's default.
    #[command(hide = true)]
    SetDefault {
        /// Tool that uses this default.
        tool: ToolArg,
        /// Provider ID.
        id: String,
    },
    /// Manage provider API keys in the OS keychain bundle.
    #[command(hide = true)]
    Key {
        #[command(subcommand)]
        command: ProviderKeyCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Store an API key.
    Set { provider: String },
    /// List stored keys (never their values).
    Ls,
    /// Remove a stored key.
    Rm { provider: String },
    /// Delete every stored key and token.
    Reset,
}

#[derive(Debug, Subcommand)]
pub enum ProviderKeyCommand {
    /// Store a provider API key from a hidden prompt or stdin.
    Set {
        /// Provider ID.
        id: String,
    },
    /// Remove one provider API key.
    Rm {
        /// Provider ID.
        id: String,
    },
    /// List stored provider keys without showing values.
    Ls,
    /// Delete the entire provider keychain bundle.
    Reset,
}

#[derive(Debug, Args)]
pub struct ProviderAddArgs {
    /// Provider ID.
    #[arg(long)]
    pub id: Option<String>,
    /// Provider API base URL.
    #[arg(long)]
    pub base_url: Option<String>,
    /// API style used by the provider.
    #[arg(long, value_enum)]
    pub api_style: Option<ApiStyleArg>,
    /// Environment variable name for its API key.
    #[arg(long)]
    pub key_env: Option<String>,
    /// Default model id used by multi-provider tools such as Pi.
    #[arg(long)]
    pub default_model: Option<String>,
    /// Authentication scheme for sending the key.
    #[arg(long, value_enum)]
    pub auth: Option<AuthSchemeArg>,
    /// Allow an experimental provider pairing.
    #[arg(long)]
    pub experimental: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ApiStyleArg {
    Anthropic,
    Openai,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum AuthSchemeArg {
    Bearer,
    XApiKey,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum RuntimeArg {
    Auto,
    Docker,
    Orbstack,
}

impl From<RuntimeArg> for RuntimePref {
    fn from(value: RuntimeArg) -> Self {
        match value {
            RuntimeArg::Auto => Self::Auto,
            RuntimeArg::Docker => Self::Docker,
            RuntimeArg::Orbstack => Self::OrbStack,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum HistoryArg {
    Full,
    Shallow,
    RedactAuthors,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CompletionShellArg {
    Bash,
    Fish,
    Zsh,
}

impl CompletionShellArg {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Fish => "fish",
            Self::Zsh => "zsh",
        }
    }
}

impl From<CompletionShellArg> for Shell {
    fn from(value: CompletionShellArg) -> Self {
        match value {
            CompletionShellArg::Bash => Self::Bash,
            CompletionShellArg::Fish => Self::Fish,
            CompletionShellArg::Zsh => Self::Zsh,
        }
    }
}

impl From<HistoryArg> for HistoryDial {
    fn from(value: HistoryArg) -> Self {
        match value {
            HistoryArg::Full => Self::Full,
            HistoryArg::Shallow => Self::Shallow,
            HistoryArg::RedactAuthors => Self::RedactAuthors,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ToolArg {
    Claude,
    Codex,
    Opencode,
    Pi,
    Omp,
}

impl ToolArg {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Pi => "pi",
            Self::Omp => "omp",
        }
    }
}

impl From<ToolArg> for Tool {
    fn from(value: ToolArg) -> Self {
        match value {
            ToolArg::Claude => Self::Claude,
            ToolArg::Codex => Self::Codex,
            ToolArg::Opencode => Self::Opencode,
            ToolArg::Pi => Self::Pi,
            ToolArg::Omp => Self::Omp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::generate;

    fn external(words: &[&str]) -> Command {
        Command::External(words.iter().map(OsString::from).collect())
    }

    fn run_args(tool: ToolArg, tool_args: &[&str]) -> RunArgs {
        RunArgs {
            providers: None,
            pick: false,
            tool,
            mount: false,
            copy: false,
            history: HistoryArg::Full,
            include: Vec::new(),
            provider: None,
            yes: false,
            experimental: false,
            tool_args: tool_args.iter().map(|arg| (*arg).to_string()).collect(),
            verbose: false,
        }
    }

    #[test]
    fn handoff_is_interactive_only_when_both_streams_are_terminals() {
        let args = run_args(ToolArg::Claude, &["hello"]);

        assert!(args.interactive_handoff(true, true));
        assert!(!args.interactive_handoff(false, true));
        assert!(!args.interactive_handoff(true, false));
    }

    #[test]
    fn yes_disables_interactive_handoff() {
        let mut args = run_args(ToolArg::Claude, &["hello"]);
        args.yes = true;

        assert!(!args.interactive_handoff(true, true));
    }

    #[test]
    fn claude_print_modes_disable_interactive_handoff() {
        for print_arg in ["-p", "--print", "--print=plain"] {
            let args = run_args(ToolArg::Claude, &["hello", print_arg]);
            assert!(!args.interactive_handoff(true, true), "{print_arg}");
        }

        let args = run_args(ToolArg::Claude, &["mention --print in the answer"]);
        assert!(args.interactive_handoff(true, true));
    }

    #[test]
    fn codex_exec_disables_interactive_handoff() {
        let direct = run_args(ToolArg::Codex, &["exec", "hello"]);
        assert!(!direct.interactive_handoff(true, true));

        let after_global_option = run_args(ToolArg::Codex, &["--model", "gpt-5", "exec", "hello"]);
        assert!(!after_global_option.interactive_handoff(true, true));

        let interactive_prompt = run_args(ToolArg::Codex, &["discuss", "exec"]);
        assert!(interactive_prompt.interactive_handoff(true, true));
    }

    #[test]
    fn opencode_run_disables_interactive_handoff() {
        let batch = run_args(ToolArg::Opencode, &["run", "hello"]);
        assert!(!batch.interactive_handoff(true, true));

        let interactive_prompt = run_args(ToolArg::Opencode, &["discuss", "run"]);
        assert!(interactive_prompt.interactive_handoff(true, true));
    }

    #[test]
    fn pi_arguments_do_not_imply_batch_mode() {
        let args = run_args(ToolArg::Pi, &["run", "--print"]);
        assert!(args.interactive_handoff(true, true));
    }

    #[test]
    fn omp_print_mode_disables_interactive_handoff() {
        for batch in [
            run_args(ToolArg::Omp, &["-p", "hello"]),
            run_args(ToolArg::Omp, &["--print", "hello"]),
        ] {
            assert!(!batch.interactive_handoff(true, true));
        }

        let interactive = run_args(ToolArg::Omp, &["--print-thoughts"]);
        assert!(interactive.interactive_handoff(true, true));
    }

    #[test]
    fn alias_help_is_classified_without_launching() {
        let invocation = external(&["claude", "--help"]).into_invocation().unwrap();
        assert!(matches!(
            invocation,
            Invocation::ShowAliasHelp {
                tool: ToolArg::Claude,
                provider: None
            }
        ));
    }

    #[test]
    fn short_verbose_flag_is_global_for_aliases() {
        let before = Cli::try_parse_from(["capscode", "-v", "codex", "--help"]).unwrap();
        assert!(before.verbose);

        let after = Cli::try_parse_from(["capscode", "codex", "-v", "prompt"]).unwrap();
        let invocation = after.command.unwrap().into_invocation().unwrap();
        let Invocation::RunTool(args) = invocation else {
            panic!("expected run invocation");
        };
        assert!(args.verbose);
        assert_eq!(args.tool_args, ["prompt"]);
    }

    #[test]
    fn alias_version_is_classified_without_launching() {
        let invocation = external(&["claude", "-V"]).into_invocation().unwrap();
        assert!(matches!(invocation, Invocation::ShowVersion));
    }

    #[test]
    fn delimiter_forwards_later_help_to_tool() {
        let cli = Cli::try_parse_from(["capscode", "claude", "--", "--help"]).unwrap();
        let invocation = cli.command.unwrap().into_invocation().unwrap();
        let Invocation::RunTool(args) = invocation else {
            panic!("expected launch");
        };
        assert_eq!(args.tool_args, ["--help"]);
    }

    #[test]
    fn ordinary_alias_arguments_launch_tool() {
        let invocation = external(&["claude", "foo", "--bar"])
            .into_invocation()
            .unwrap();
        let Invocation::RunTool(args) = invocation else {
            panic!("expected launch");
        };
        assert_eq!(args.tool_args, ["foo", "--bar"]);
    }

    #[test]
    fn unknown_alias_still_errors() {
        let invocation = external(&["unknown-tool"]).into_invocation().unwrap();
        assert!(
            matches!(invocation, Invocation::Removed(message) if message.contains("no box or tool named 'unknown-tool'"))
        );
    }

    #[test]
    fn box_resolution_requires_a_single_word_and_lists_ambiguous_matches() {
        let one = crate::boxes::BoxRef {
            name: "project-abc123".into(),
            id: "abc123".into(),
            session: "session".into(),
        };
        let invocation = resolve_box_word(
            "project-abc123",
            None,
            crate::boxes::Resolved::One(one.clone()),
        )
        .unwrap();
        assert!(matches!(invocation, Invocation::EnterBox(target) if target == one));
        let extra = resolve_box_word(
            "project-abc123",
            Some(std::ffi::OsStr::new("claude")),
            crate::boxes::Resolved::One(one.clone()),
        )
        .unwrap();
        assert!(
            matches!(extra, Invocation::Removed(message) if message == "`project-abc123` is a box; enter it with `capscode project-abc123` and run `claude` there")
        );
        let mut second = one.clone();
        second.name = "project-def456".into();
        let ambiguous = resolve_box_word(
            "project",
            None,
            crate::boxes::Resolved::Ambiguous(vec![one, second]),
        )
        .unwrap();
        assert!(
            matches!(ambiguous, Invocation::Removed(message) if message.contains("project-abc123, project-def456") && message.contains("Type more of the name"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_word_after_box_is_rejected_instead_of_entering() {
        use std::os::unix::ffi::OsStrExt;

        let target = crate::boxes::BoxRef {
            name: "project-abc123".into(),
            id: "abc123".into(),
            session: "session".into(),
        };
        let invalid_word = std::ffi::OsStr::from_bytes(b"claude\xff");
        let invocation = resolve_box_word(
            "project-abc123",
            Some(invalid_word),
            crate::boxes::Resolved::One(target),
        )
        .unwrap();

        assert!(
            matches!(invocation, Invocation::Removed(message) if message == "box command arguments must be valid UTF-8")
        );
    }

    #[test]
    fn did_you_mean_uses_distance_two_for_tools_and_subcommands() {
        assert_eq!(did_you_mean("claud").as_deref(), Some("claude"));
        assert_eq!(did_you_mean("creat").as_deref(), Some("create"));
        assert_eq!(did_you_mean("zzzzzz"), None);
    }

    #[test]
    fn alias_provider_and_confirmation_flags_are_consumed_by_capscode() {
        let invocation = external(&[
            "claude",
            "--provider",
            "kimi",
            "--yes",
            "--experimental",
            "prompt",
        ])
        .into_invocation()
        .unwrap();
        let Invocation::RunTool(args) = invocation else {
            panic!("expected launch");
        };
        assert_eq!(args.provider.as_deref(), Some("kimi"));
        assert!(args.yes);
        assert!(args.experimental);
        assert_eq!(args.tool_args, ["prompt"]);
    }

    #[test]
    fn alias_providers_equals_form_is_consumed_by_capscode() {
        let invocation = external(&["claude", "--providers=all", "prompt"])
            .into_invocation()
            .unwrap();
        let Invocation::RunTool(args) = invocation else {
            panic!("expected launch");
        };
        assert_eq!(args.providers.as_deref(), Some("all"));
        assert_eq!(args.tool_args, ["prompt"]);
    }

    #[test]
    fn every_tool_alias_consumes_capsule_flags_into_run_args() {
        for tool in Tool::ALL.iter().map(|tool| tool.name()) {
            let invocation = external(&[
                tool,
                "--mount",
                "--history",
                "shallow",
                "--include",
                "src/generated",
                "prompt",
            ])
            .into_invocation()
            .unwrap();
            let Invocation::RunTool(args) = invocation else {
                panic!("expected launch for {tool}");
            };
            assert!(args.mount, "{tool} did not consume --mount");
            assert!(!args.copy, "{tool} should not set --copy");
            assert!(matches!(args.history, HistoryArg::Shallow));
            assert_eq!(args.include, [PathBuf::from("src/generated")]);
            assert_eq!(args.tool_args, ["prompt"]);
        }
    }

    #[test]
    fn every_tool_alias_consumes_copy_flag() {
        for tool in Tool::ALL.iter().map(|tool| tool.name()) {
            let invocation = external(&[tool, "--copy", "prompt"])
                .into_invocation()
                .unwrap();
            let Invocation::RunTool(args) = invocation else {
                panic!("expected launch for {tool}");
            };
            assert!(args.copy, "{tool} did not consume --copy");
            assert!(!args.mount, "{tool} should not set --mount");
            assert_eq!(args.tool_args, ["prompt"]);
        }
    }

    #[test]
    fn alias_help_after_provider_never_launches() {
        let invocation = external(&["claude", "--provider", "kimi", "--help"])
            .into_invocation()
            .unwrap();
        assert!(matches!(
            invocation,
            Invocation::ShowAliasHelp {
                tool: ToolArg::Claude,
                provider: Some(provider)
            } if provider == "kimi"
        ));
    }

    #[test]
    fn init_accepts_repeated_tools_and_probe_choice() {
        let cli = Cli::try_parse_from([
            "capscode",
            "init",
            "--tool",
            "claude",
            "--tool",
            "codex",
            "--no-probe",
            "--yes",
        ])
        .unwrap();
        let Command::Init(args) = cli.command.unwrap() else {
            panic!("expected init command");
        };
        assert_eq!(args.tool, [ToolArg::Claude, ToolArg::Codex]);
        assert!(args.no_probe);
        assert!(!args.probe);
        assert!(args.yes);
        assert_eq!(args.completion_shell, None);
        assert!(!args.no_completions);
    }

    #[test]
    fn init_accepts_completion_choice_and_rejects_conflict() {
        let cli = Cli::try_parse_from(["capscode", "init", "--completion-shell", "fish", "--yes"])
            .unwrap();
        let Command::Init(args) = cli.command.unwrap() else {
            panic!("expected init command");
        };
        assert_eq!(args.completion_shell, Some(CompletionShellArg::Fish));

        assert!(Cli::try_parse_from([
            "capscode",
            "init",
            "--completion-shell",
            "zsh",
            "--no-completions",
        ])
        .is_err());
    }

    #[test]
    fn completions_accept_supported_shells() {
        let cli = Cli::try_parse_from(["capscode", "completions", "zsh"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Completions { shell: Shell::Zsh }
        ));
    }

    #[test]
    fn status_accepts_json_flag() {
        let cli = Cli::try_parse_from(["capscode", "status", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Status { json: true }
        ));
    }

    #[test]
    fn uninstall_and_purge_alias_parse_all_flags() {
        let uninstall =
            Cli::try_parse_from(["capscode", "uninstall", "--images", "--yes"]).unwrap();
        assert!(matches!(
            uninstall.command.unwrap(),
            Command::Uninstall {
                images: true,
                yes: true,
                discard_workspace: false
            }
        ));

        let purge = Cli::try_parse_from(["capscode", "purge", "-y"]).unwrap();
        assert!(matches!(
            purge.command.unwrap(),
            Command::Uninstall {
                images: false,
                yes: true,
                discard_workspace: false
            }
        ));

        let plain = Cli::try_parse_from(["capscode", "uninstall"]).unwrap();
        assert!(matches!(
            plain.command.unwrap(),
            Command::Uninstall {
                images: false,
                yes: false,
                discard_workspace: false
            }
        ));

        let discard =
            Cli::try_parse_from(["capscode", "uninstall", "--discard-workspace"]).unwrap();
        assert!(matches!(
            discard.command.unwrap(),
            Command::Uninstall {
                discard_workspace: true,
                ..
            }
        ));
    }

    #[test]
    fn provider_key_verbs_parse_without_a_positional_secret() {
        let set = Cli::try_parse_from(["capscode", "provider", "key", "set", "openai"]).unwrap();
        assert!(matches!(
            set.command.unwrap(),
            Command::Provider {
                command: ProviderCommand::Key {
                    command: ProviderKeyCommand::Set { id }
                }
            } if id == "openai"
        ));

        let ls = Cli::try_parse_from(["capscode", "provider", "key", "ls"]).unwrap();
        assert!(matches!(
            ls.command.unwrap(),
            Command::Provider {
                command: ProviderCommand::Key {
                    command: ProviderKeyCommand::Ls
                }
            }
        ));
        for words in [
            ["capscode", "provider", "key", "rm", "openai"].as_slice(),
            ["capscode", "provider", "key", "reset"].as_slice(),
        ] {
            assert!(Cli::try_parse_from(words).is_ok());
        }
        assert!(Cli::try_parse_from([
            "capscode",
            "provider",
            "key",
            "set",
            "openai",
            "secret-must-not-be-positional",
        ])
        .is_err());
    }

    #[test]
    fn completion_command_omits_hidden_commands() {
        let command = completion_command_with_boxes(Vec::new());
        let names: Vec<&str> = command
            .get_subcommands()
            .map(|sub| sub.get_name())
            .collect();
        for removed in [
            "run",
            "attach",
            "console",
            "start",
            "status",
            "auth",
            "completions",
        ] {
            assert!(!names.contains(&removed), "{removed} is offered: {names:?}");
        }
        for kept in [
            "create", "ls", "rm", "sync", "key", "provider", "config", "codex",
        ] {
            assert!(names.contains(&kept), "{kept} is missing: {names:?}");
        }
        let provider = command.find_subcommand("provider").unwrap();
        assert!(provider.find_subcommand("key").is_none());
        let mut zsh = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Zsh,
            &mut completion_command_with_boxes(Vec::new()),
            "capscode",
            &mut zsh,
        );
        let zsh = String::from_utf8(zsh).unwrap();
        assert!(!zsh.contains("Removed;"));
        assert!(
            zsh.contains("'(--no-probe)--probe["),
            "arg conflicts survive the copy"
        );
    }

    #[test]
    fn completion_command_includes_tool_aliases() {
        let command = completion_command();
        for tool in Tool::ALL.iter().map(|tool| tool.name()) {
            assert!(command.find_subcommand(tool).is_some(), "missing {tool}");
        }
    }

    #[test]
    fn completion_command_includes_file_index_box_names() {
        let command = completion_command_with_boxes(vec!["project-abc123".to_string()]);
        assert!(command.find_subcommand("project-abc123").is_some());
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("project-abc123"));
    }

    #[test]
    fn help_includes_file_index_box_names_without_adding_runtime_subcommands() {
        let mut command = help_command_with_boxes(vec!["project-abc123".to_string()]);
        let help = command.render_help().to_string();
        assert!(help.contains("project-abc123"));
        assert!(command.find_subcommand("project-abc123").is_none());
    }

    #[test]
    fn config_verbs_parse_including_list_json() {
        let list = Cli::try_parse_from(["capscode", "config", "list"]).unwrap();
        assert!(matches!(
            list.command.unwrap(),
            Command::Config {
                command: ConfigCommand::List { json: false }
            }
        ));

        let list_json = Cli::try_parse_from(["capscode", "config", "list", "--json"]).unwrap();
        assert!(matches!(
            list_json.command.unwrap(),
            Command::Config {
                command: ConfigCommand::List { json: true }
            }
        ));

        let get = Cli::try_parse_from(["capscode", "config", "get", "runtime"]).unwrap();
        assert!(matches!(
            get.command.unwrap(),
            Command::Config {
                command: ConfigCommand::Get {
                    key: ConfigKeyArg::Runtime
                }
            }
        ));

        let set =
            Cli::try_parse_from(["capscode", "config", "set", "auth.keychain", "true"]).unwrap();
        assert!(matches!(
            set.command.unwrap(),
            Command::Config {
                command: ConfigCommand::Set {
                    key: ConfigKeyArg::AuthKeychain,
                    value
                }
            } if value == "true"
        ));

        let unset = Cli::try_parse_from(["capscode", "config", "unset", "defaults.pi"]).unwrap();
        assert!(matches!(
            unset.command.unwrap(),
            Command::Config {
                command: ConfigCommand::Unset {
                    key: ConfigKeyArg::DefaultsPi
                }
            }
        ));

        let edit = Cli::try_parse_from(["capscode", "config", "edit"]).unwrap();
        assert!(matches!(
            edit.command.unwrap(),
            Command::Config {
                command: ConfigCommand::Edit
            }
        ));
    }

    #[test]
    fn config_keys_accept_exact_dotted_allowlist_in_stable_order() {
        assert_eq!(
            ConfigKeyArg::all().map(ConfigKeyArg::as_str),
            [
                "auth.keychain",
                "box.prefix",
                "capsule.memory",
                "capsule.pids",
                "capsule.opencode_lsp_download",
                "capsule.cache_volume",
                "defaults.claude",
                "defaults.codex",
                "defaults.opencode",
                "defaults.pi",
                "defaults.omp",
                "runtime",
                "update.channel",
                "update.check",
                "update.verify_with_gh",
                "workspace.mode",
            ]
        );

        for key in ConfigKeyArg::all() {
            let cli = Cli::try_parse_from(["capscode", "config", "get", key.as_str()]).unwrap();
            assert!(matches!(
                cli.command.unwrap(),
                Command::Config {
                    command: ConfigCommand::Get { key: parsed }
                } if parsed == key
            ));
        }
    }

    #[test]
    fn config_rejects_unknown_keys_and_underscore_style_names() {
        assert!(Cli::try_parse_from(["capscode", "config", "get", "network.enforce_vpn"]).is_err());
        assert!(
            Cli::try_parse_from(["capscode", "config", "set", "auth_keychain", "true"]).is_err()
        );
        assert!(Cli::try_parse_from(["capscode", "config", "unset", "defaults_claude"]).is_err());
        assert!(Cli::try_parse_from(["capscode", "config", "get", "runtim"]).is_err());
    }

    #[test]
    fn rm_accepts_short_force_flag() {
        for argv in [
            ["capscode", "rm", "abc", "-f"],
            ["capscode", "rm", "-f", "abc"],
            ["capscode", "rm", "abc", "--force"],
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            assert!(
                matches!(cli.command, Some(Command::Rm { force: true, .. })),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn box_cache_flags_parse() {
        let create = Cli::try_parse_from(["capscode", "create", "--no-cache"]).unwrap();
        assert!(matches!(
            create.command.unwrap(),
            Command::Create(BoxCreateArgs { no_cache: true, .. })
        ));

        let resume =
            Cli::try_parse_from(["capscode", "resume", "alpha-123abc", "--fresh-cache"]).unwrap();
        assert!(matches!(
            resume.command.unwrap(),
            Command::Resume(BoxResumeArgs {
                fresh_cache: true,
                ..
            })
        ));
        let resume =
            Cli::try_parse_from(["capscode", "resume", "alpha-123abc", "--cache"]).unwrap();
        assert!(matches!(
            resume.command.unwrap(),
            Command::Resume(BoxResumeArgs { cache: true, .. })
        ));
    }

    #[test]
    fn toolchain_verbs_and_resume_toolchain_flag_parse() {
        for (argv, expected) in [
            (vec!["capscode", "toolchain", "show"], "show"),
            (vec!["capscode", "toolchain", "ls"], "ls"),
            (
                vec!["capscode", "toolchain", "prune", "--keep", "2", "--yes"],
                "prune",
            ),
            (
                vec!["capscode", "toolchain", "build", "fb24882402b5"],
                "build",
            ),
            (
                vec!["capscode", "toolchain", "build", "--no-smoke"],
                "build without smoke",
            ),
            (vec!["capscode", "toolchain", "use", "built-in"], "use"),
            (
                vec![
                    "capscode",
                    "toolchain",
                    "pin",
                    "codex@0.159.3",
                    "--untested",
                    "--integrity",
                    "sha512-value",
                ],
                "pin",
            ),
        ] {
            let cli = Cli::try_parse_from(argv.clone()).unwrap();
            assert!(
                matches!(cli.command, Some(Command::Toolchain { .. })),
                "expected toolchain {expected} to parse"
            );
        }
        let cli = Cli::try_parse_from(["capscode", "toolchain", "build", "--no-smoke"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Toolchain {
                command: ToolchainCommand::Build { no_smoke: true, .. },
                ..
            })
        ));
        let cli = Cli::try_parse_from([
            "capscode",
            "resume",
            "alpha-123abc",
            "--toolchain",
            "current",
            "--pin",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Resume(BoxResumeArgs {
                toolchain: Some(ref digest),
                pin: true,
                ..
            })) if digest == "current"
        ));
    }

    #[test]
    fn config_help_and_completions_include_verbs_and_dotted_keys() {
        let mut command = completion_command();
        let config = command
            .find_subcommand_mut("config")
            .expect("config subcommand");
        for verb in ["list", "get", "set", "unset", "edit"] {
            assert!(
                config.find_subcommand(verb).is_some(),
                "missing config {verb}"
            );
        }

        // Possible key values live on get/set/unset, not the parent help text.
        let get = config.find_subcommand_mut("get").expect("config get");
        let mut help = Vec::new();
        get.write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        for key in ConfigKeyArg::all() {
            assert!(
                help.contains(key.as_str()),
                "help missing dotted key {}",
                key.as_str()
            );
        }
        assert!(
            !help.contains("auth_keychain"),
            "help must not expose Rust-style key names"
        );

        let mut root = completion_command();
        let mut zsh = Vec::new();
        generate(Shell::Zsh, &mut root, "capscode", &mut zsh);
        let zsh = String::from_utf8(zsh).unwrap();
        assert!(zsh.contains("config"), "zsh completions missing config");
        for verb in ["list", "get", "set", "unset", "edit"] {
            assert!(zsh.contains(verb), "zsh completions missing {verb}");
        }
        for key in ConfigKeyArg::all() {
            assert!(
                zsh.contains(key.as_str()),
                "zsh completions missing dotted key {}",
                key.as_str()
            );
        }
        assert!(!zsh.contains("auth_keychain"));
    }

    #[test]
    fn zsh_completions_include_commands_and_tool_aliases() {
        let mut command = completion_command();
        let mut output = Vec::new();
        generate(Shell::Zsh, &mut command, "capscode", &mut output);
        let output = String::from_utf8(output).unwrap();

        for command in [
            "init",
            "create",
            "doctor",
            "Show help for a command",
            "provider",
            "config",
            "claude",
            "codex",
            "opencode",
            "pi",
            "omp",
        ] {
            assert!(output.contains(command), "missing {command}");
        }
    }

    #[test]
    fn help_does_not_advertise_removed_devbox_switch() {
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("CAPSCODE_DEVBOX"));
    }
}
