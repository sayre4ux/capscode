#![deny(clippy::print_stdout, clippy::print_stderr)]

//! `capscode` command surface and session orchestrator.

mod args;
mod atomic_file;
mod boxes;
mod build_context;
mod commands;
mod config;
mod discovery;
mod init;
mod keystore;
mod redact;
mod session;
mod shell_completion;
mod terminal;
mod toolchain;
pub mod ui;
mod update;

use std::future::Future;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::FromArgMatches;
use dialoguer::{Confirm, Select};

use crate::args::{
    Cli, Command, ConfigCommand, Invocation, KeyCommand, ProviderCommand, VpnCommand,
};
use crate::ui::{data, note};

const ENTER_TTY_ERROR: &str = "entering a box needs an interactive terminal; use `capscode sync <box>` or `capscode rm <box>` from scripts";

#[tokio::main]
async fn main() -> ExitCode {
    session::timing::mark_process_start();
    // Before any Tokio signal registration or command dispatch: snapshot cooked
    // termios and install the cooperative SIGINT/SIGTERM prompt restorer.
    terminal::install_prompt_restorer();
    match entrypoint().await {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            if !terminal::hangup_received() {
                terminal::restore_prompt_terminal();
                note(format!("error: {error:#}"));
            }
            ExitCode::FAILURE
        }
    }
}

async fn entrypoint() -> Result<i32> {
    let command = crate::args::help_command();
    let matches = command.get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    let runtime = cli.runtime;
    let Some(command_value) = cli.command else {
        let check = update::passive::start(true);
        let result = default_entry(runtime).await;
        return finish_update_check(check, result);
    };
    let invocation = command_value.into_invocation()?;
    let verbose = cli.verbose
        || matches!(
            &invocation,
            Invocation::RunTool(args) if args.verbose
        );
    redact::init_tracing(verbose);
    // The daily update check runs beside the command and reports after it.
    let check = update::passive::start(passive_check_eligible(&invocation));
    let result = dispatch(invocation, runtime, verbose).await;
    finish_update_check(check, result)
}

/// Print the update notice after the command's own output, only if it succeeded.
fn finish_update_check(
    check: Option<update::passive::PassiveCheck>,
    result: Result<i32>,
) -> Result<i32> {
    if result.is_ok() && !terminal::hangup_received() {
        if let Some(line) = check.and_then(update::passive::PassiveCheck::finish) {
            note(line);
        }
    }
    result
}

/// Help, completions, version output, machine-readable output, `config`, `update`
/// (which fetches anyway), and `uninstall` (which removes the state directory) skip
/// the passive check.
fn passive_check_eligible(invocation: &Invocation) -> bool {
    match invocation {
        Invocation::Execute(command) => match command {
            Command::Completions { .. }
            | Command::Update { .. }
            | Command::Uninstall { .. }
            | Command::Config { .. } => false,
            Command::Ls { json } | Command::Doctor { json, .. } | Command::Status { json } => !json,
            _ => true,
        },
        Invocation::ShowVersion | Invocation::ShowAliasHelp { .. } | Invocation::Removed(_) => {
            false
        }
        Invocation::RunTool(_) | Invocation::EnterBox(_) => true,
    }
}

async fn dispatch(
    invocation: Invocation,
    runtime: Option<crate::args::RuntimeArg>,
    verbose: bool,
) -> Result<i32> {
    match invocation {
        Invocation::Execute(Command::Init(args)) => init::init(args, runtime).await,
        Invocation::Execute(Command::Completions { shell }) => commands::completions(shell),
        Invocation::Execute(Command::Create(args)) => {
            let project = std::env::current_dir()?;
            if args.mount {
                confirm_mount(&project, args.yes)?;
            }
            let selected_runtime = commands::selected_runtime(runtime).await?;
            let created = crate::boxes::create(
                &project,
                crate::boxes::CreateOptions {
                    base: args.name,
                    providers: args.providers,
                    mount: args.mount,
                    pin: args.pin,
                    no_cache: args.no_cache,
                },
                selected_runtime,
            )
            .await?;
            data(format!("Created box {}", created.name));
            Ok(())
        }
        Invocation::Execute(Command::Resume(args)) => {
            let target = resolve_box(&args.target)?;
            let selected_runtime = commands::selected_runtime(runtime).await?;
            crate::boxes::resume(
                &target,
                crate::boxes::ResumeOptions {
                    providers: args.providers,
                    yes: args.yes,
                    toolchain: args.toolchain,
                    pin: args.pin,
                    fresh_cache: args.fresh_cache,
                    cache: args.cache,
                    force: args.force,
                },
                selected_runtime,
            )
            .await?;
            Ok(())
        }
        Invocation::Execute(Command::Stop { target }) => {
            let target = resolve_box(&target)?;
            let selected_runtime = commands::selected_runtime(runtime).await?;
            crate::boxes::stop(&target, selected_runtime).await
        }
        Invocation::RunTool(args) => {
            let code = commands::run(args, runtime, verbose).await?;
            return Ok(code);
        }
        Invocation::Execute(Command::Run { .. }) => {
            bail!("run is now a tool name, such as `capscode claude`")
        }
        Invocation::Execute(Command::Attach { .. }) => bail!("attach is now `capscode <box>`"),
        Invocation::Execute(Command::Console) => {
            bail!("console is now `capscode ls`; enter a listed box with `capscode <box>`")
        }
        Invocation::Execute(Command::Start(_)) => bail!("start is now `resume <box>`"),
        Invocation::Execute(Command::Ls { json }) => commands::ls(runtime, json).await,
        Invocation::Execute(Command::Toolchain { command }) => {
            commands::toolchain(command, runtime).await
        }
        Invocation::Execute(Command::Update {
            check,
            yes,
            cli: true,
            allow_downgrade,
        }) => update::cli::update_cli(update::cli::CliUpdateOptions {
            check_only: check,
            yes,
            allow_downgrade,
        }),
        Invocation::Execute(Command::Update { check, yes, .. }) => {
            update::update(update::UpdateOptions {
                check_only: check,
                yes,
            })
        }
        Invocation::Execute(Command::Rm {
            target,
            force,
            discard_workspace,
        }) => lifecycle_rm(runtime, target, force, discard_workspace).await,
        Invocation::Execute(Command::Uninstall {
            images,
            yes,
            discard_workspace,
        }) => commands::uninstall(runtime, images, yes, discard_workspace).await,
        Invocation::Execute(Command::Sync { target }) => {
            lifecycle_sync(runtime, target, verbose).await
        }
        Invocation::Execute(Command::Doctor { quick, json }) => {
            if quick {
                return commands::status(runtime, json).await;
            } else {
                return commands::doctor(runtime, json).await;
            }
        }
        Invocation::Execute(Command::Status { .. }) => bail!("status is now `doctor --quick`"),
        Invocation::Execute(Command::Vpn { command }) => match command {
            Some(VpnCommand::Import { path }) => commands::vpn_import(&path),
            Some(VpnCommand::Status) => bail!("vpn status is now bare `vpn`"),
            Some(VpnCommand::Remove) => commands::vpn_remove(),
            None => commands::vpn_status(runtime),
        },
        Invocation::Execute(Command::Auth { .. }) => bail!("auth is now `key set|ls|rm|reset`"),
        Invocation::Execute(Command::Provider { command }) => match command {
            ProviderCommand::List => commands::provider_list(),
            ProviderCommand::Show { id } => commands::provider_show(&id),
            ProviderCommand::Add(args) => commands::provider_add(args),
            ProviderCommand::Remove { id } => commands::provider_remove(&id),
            ProviderCommand::SetDefault { .. } => {
                bail!("provider set-default is now `config set defaults.<tool> <provider>`")
            }
            ProviderCommand::Key { .. } => bail!("provider key is now `key set|ls|rm|reset`"),
        },
        Invocation::Execute(Command::Key { command }) => match command {
            KeyCommand::Set { provider } if provider == "anthropic" => commands::auth_import(),
            KeyCommand::Set { provider } => commands::provider_key_set(&provider),
            KeyCommand::Ls => commands::provider_key_list(),
            KeyCommand::Rm { provider } if provider == "anthropic" => commands::auth_remove(),
            KeyCommand::Rm { provider } => commands::provider_key_remove(&provider),
            KeyCommand::Reset => commands::provider_key_reset(),
        },
        Invocation::Execute(Command::Config { command }) => {
            // --runtime is an operational override for capsule lifecycle commands.
            // It must not mask persisted or effective config values under inspection.
            if runtime.is_some() {
                bail!("--runtime cannot be combined with config commands");
            }
            match command {
                ConfigCommand::List { json } => commands::config_list(json),
                ConfigCommand::Get { key } => commands::config_get(key),
                ConfigCommand::Set { key, value } => commands::config_set(key, &value),
                ConfigCommand::Unset { key } => commands::config_unset(key),
                ConfigCommand::Edit => commands::config_edit(),
            }
        }
        Invocation::Execute(Command::External(_)) => {
            unreachable!("external commands are normalized by into_invocation")
        }
        Invocation::ShowAliasHelp { tool, provider } => {
            commands::alias_help(tool, provider.as_deref())
        }
        Invocation::ShowVersion => {
            data(format!("capscode {}", env!("CARGO_PKG_VERSION")));
            Ok(())
        }
        Invocation::Removed(pointer) => {
            note(pointer);
            return Ok(2);
        }
        Invocation::EnterBox(target) => {
            if !has_interactive_terminal(
                std::io::stdin().is_terminal(),
                std::io::stdout().is_terminal(),
            ) {
                note(ENTER_TTY_ERROR);
                return Ok(2);
            }
            let selected_runtime = commands::selected_runtime(runtime).await?;
            return crate::boxes::enter(&target, selected_runtime).await;
        }
    }?;
    Ok(0)
}

async fn default_entry(runtime_flag: Option<crate::args::RuntimeArg>) -> Result<i32> {
    let project = std::env::current_dir()?.canonicalize()?;
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .and_then(|path| path.canonicalize().ok());
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if home.as_deref() == Some(project.as_path()) || project.parent().is_none() {
        note(crate::args::help_command().render_help().to_string());
        return Ok(2);
    }
    let boxes = crate::boxes::for_project(&project)?;
    match default_entry_decision(boxes.len(), tty) {
        DefaultEntryDecision::ListAndExit => {
            note(format!(
                "Several boxes belong to {}:\n{}\nRun `capscode <box>` to enter one.",
                project.display(),
                boxes
                    .iter()
                    .enumerate()
                    .map(|(i, item)| format!("{}. {}", i + 1, item.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
            Ok(2)
        }
        DefaultEntryDecision::EnterNeedsTty => {
            note(ENTER_TTY_ERROR);
            Ok(2)
        }
        DefaultEntryDecision::ShowHelp => {
            note(crate::args::help_command().render_help().to_string());
            Ok(2)
        }
        DefaultEntryDecision::Enter => {
            let runtime = crate::commands::selected_runtime(runtime_flag).await?;
            crate::boxes::enter(&boxes[0], runtime).await
        }
        DefaultEntryDecision::Create => {
            let name = project
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("project");
            let confirmed = Confirm::with_theme(&crate::ui::theme())
                .with_prompt(format!(
                    "No box for {}. Create '{name}'? This needs the 2.4 GB capsule-all image (build it with: {})",
                    project.display(),
                    crate::build_context::image_build_hint()
                ))
                .default(false)
                .interact()?;
            if !confirmed {
                return Ok(0);
            }
            let runtime = crate::commands::selected_runtime(runtime_flag).await?;
            let created = crate::boxes::create(
                &project,
                crate::boxes::CreateOptions {
                    base: None,
                    providers: None,
                    mount: false,
                    pin: false,
                    no_cache: false,
                },
                Arc::clone(&runtime),
            )
            .await?;
            crate::boxes::enter(&created, runtime).await
        }
        DefaultEntryDecision::Choose => {
            let choices = boxes
                .iter()
                .map(|target| target.name.as_str())
                .collect::<Vec<_>>();
            let selected = Select::with_theme(&crate::ui::theme())
                .with_prompt("Choose this project's box")
                .items(&choices)
                .default(0)
                .interact()?;
            let runtime = crate::commands::selected_runtime(runtime_flag).await?;
            crate::boxes::enter(&boxes[selected], runtime).await
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DefaultEntryDecision {
    Enter,
    Create,
    Choose,
    ListAndExit,
    EnterNeedsTty,
    ShowHelp,
}

fn default_entry_decision(box_count: usize, tty: bool) -> DefaultEntryDecision {
    match (box_count, tty) {
        (0, true) => DefaultEntryDecision::Create,
        (1, true) => DefaultEntryDecision::Enter,
        (2.., true) => DefaultEntryDecision::Choose,
        (0, false) => DefaultEntryDecision::ShowHelp,
        (1, false) => DefaultEntryDecision::EnterNeedsTty,
        (2.., false) => DefaultEntryDecision::ListAndExit,
    }
}

fn has_interactive_terminal(stdin_is_terminal: bool, stdout_is_terminal: bool) -> bool {
    stdin_is_terminal && stdout_is_terminal
}

fn resolve_box(word: &str) -> Result<crate::boxes::BoxRef> {
    match crate::boxes::resolve(word)? {
        crate::boxes::Resolved::One(target) => Ok(target),
        crate::boxes::Resolved::Ambiguous(matches) => bail!(
            "box name '{word}' is ambiguous: {}. Type more of the name to pick one",
            matches
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        crate::boxes::Resolved::None => bail!("no box named '{word}'"),
    }
}

async fn resolve_before_runtime<T, Resolve, Select, Fut>(
    resolve: Resolve,
    select_runtime: Select,
) -> Result<(T, Fut::Output)>
where
    Resolve: FnOnce() -> Result<T>,
    Select: FnOnce() -> Fut,
    Fut: Future,
{
    let target = resolve()?;
    let runtime = select_runtime().await;
    Ok((target, runtime))
}

async fn lifecycle_sync(
    runtime_flag: Option<crate::args::RuntimeArg>,
    target: String,
    verbose: bool,
) -> Result<()> {
    if crate::commands::validate_session_id(&target).is_ok() {
        let box_target = box_for_session(&target)?;
        let runtime = crate::commands::selected_runtime(runtime_flag).await?;
        let sync_box_name = box_target.as_ref().map(|target| target.name.clone());
        crate::commands::sync_then_record(
            || crate::commands::sync(runtime, Some(target), verbose, sync_box_name.as_deref()),
            move |identity| async move {
                if let Some(box_target) = box_target {
                    crate::boxes::record_synced(&box_target, identity).await
                } else {
                    Ok(())
                }
            },
        )
        .await?;
        return Ok(());
    }
    let (target, runtime) = resolve_before_runtime(
        || resolve_box(&target),
        || crate::commands::selected_runtime(runtime_flag),
    )
    .await?;
    let runtime = runtime?;
    let sync_session = target.session.clone();
    let sync_box_name = target.name.clone();
    crate::commands::sync_then_record(
        || crate::commands::sync(runtime, Some(sync_session), verbose, Some(&sync_box_name)),
        |identity| crate::boxes::record_synced(&target, identity),
    )
    .await?;
    Ok(())
}

fn unchecked_box_hint(name: &str) -> String {
    format!(
        "cannot check whether box '{name}' has unsynced work; run `capscode rm {name} --discard-workspace` to remove it and delete its workspace without copying it back"
    )
}

async fn lifecycle_rm(
    runtime_flag: Option<crate::args::RuntimeArg>,
    target: String,
    force: bool,
    discard_workspace: bool,
) -> Result<()> {
    let is_session = crate::commands::validate_session_id(&target).is_ok();
    if is_session {
        let box_target = box_for_session(&target)?;
        let runtime = match crate::commands::selected_runtime(runtime_flag).await {
            Ok(runtime) => runtime,
            Err(error) => match &box_target {
                Some(box_target) if !discard_workspace => {
                    return Err(error.context(unchecked_box_hint(&box_target.name)));
                }
                _ => return Err(error),
            },
        };
        if let Some(box_target) = box_target {
            remove_box(Arc::clone(&runtime), box_target, force, discard_workspace).await?;
            return Ok(());
        }
        return crate::commands::rm_with_confirmation(
            runtime,
            Some(target),
            false,
            force,
            discard_workspace,
            false,
        )
        .await;
    }
    let (target, runtime) = resolve_before_runtime(
        || resolve_box(&target),
        || crate::commands::selected_runtime(runtime_flag),
    )
    .await?;
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) if !discard_workspace => {
            // DECISION: a known box whose runtime is unavailable gets the same explicit discard-workspace guidance as an unreadable workspace.
            return Err(error.context(unchecked_box_hint(&target.name)));
        }
        Err(error) => return Err(error),
    };
    remove_box(runtime, target, force, discard_workspace).await
}

/// `create --mount` gives the agent write access to the real folder, so it is never silent.
// DECISION: without a terminal, mount mode needs an explicit --yes.
fn confirm_mount(project: &std::path::Path, yes: bool) -> Result<()> {
    note(format!(
        "Mount mode: the agent in this box writes directly to {}. Its changes skip the copy-back \
         review and reach your files immediately.",
        project.display()
    ));
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("`capscode create --mount` requires --yes when stdin is not interactive");
    }
    let confirmed = Confirm::with_theme(&crate::ui::theme())
        .with_prompt("Create a mount-mode box?")
        .default(false)
        .interact()
        .context("reading mount-mode confirmation")?;
    if !confirmed {
        bail!("box not created");
    }
    Ok(())
}

/// How many gitignored paths `rm` names before summarizing the rest.
const IGNORED_PATHS_SHOWN: usize = 10;

async fn remove_box(
    runtime: Arc<dyn capscode_core::RuntimeAdapter>,
    target: crate::boxes::BoxRef,
    force: bool,
    discard_workspace: bool,
) -> Result<()> {
    if !discard_workspace {
        let work = match crate::boxes::unsynced_work(&target, Arc::clone(&runtime)).await {
            Ok(work) => work,
            Err(_) => bail!(
                "cannot check whether box '{name}' has unsynced work; run `capscode rm {name} \
                 --discard-workspace` to remove it and delete its workspace without copying it back",
                name = target.name
            ),
        };
        // DECISION: `-f` skips only the unsynced-work refusal; rm still stops both roles before capture.
        // Gitignored files are never copied back, so only --discard-workspace gets past them.
        if work.unsynced && !force {
            bail!(
                "box '{name}' has work that is not synced to your machine; run `capscode sync {name}` \
                 first, `capscode rm {name} -f` to copy it back and remove the box, or \
                 `capscode rm {name} --discard-workspace` to delete it",
                name = target.name
            );
        }
        if !work.ignored.is_empty() {
            bail!("{}", ignored_files_message(&target.name, &work.ignored));
        }
    }
    crate::commands::rm_box_with_confirmation(runtime, &target, force, discard_workspace).await
}

fn ignored_files_message(name: &str, ignored: &[String]) -> String {
    let mut message = format!(
        "box '{name}' holds gitignored files that sync never copies to your machine; removing it \
         deletes them:"
    );
    for path in ignored.iter().take(IGNORED_PATHS_SHOWN) {
        message.push_str("\n  ");
        message.push_str(path);
    }
    if ignored.len() > IGNORED_PATHS_SHOWN {
        message.push_str(&format!(
            "\n  ... and {} more",
            ignored.len() - IGNORED_PATHS_SHOWN
        ));
    }
    message.push_str(&format!(
        "\nCopy out anything you need (`capscode {name}` opens the box), then run \
         `capscode rm {name} --discard-workspace`."
    ));
    message
}

fn box_for_session(session: &str) -> Result<Option<crate::boxes::BoxRef>> {
    for name in crate::boxes::index::names() {
        match crate::boxes::resolve(&name)? {
            crate::boxes::Resolved::One(target) if box_session_matches(&target, session) => {
                return Ok(Some(target));
            }
            crate::boxes::Resolved::One(_) | crate::boxes::Resolved::None => {}
            crate::boxes::Resolved::Ambiguous(matches) => {
                bail!(
                    "box index entry '{name}' is ambiguous: {}",
                    matches
                        .iter()
                        .map(|item| item.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    Ok(None)
}

fn box_session_matches(target: &crate::boxes::BoxRef, session: &str) -> bool {
    target.session == session
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_update_check_skips_machine_readable_and_housekeeping_commands() {
        let eligible = |args: &[&str]| {
            let cli = <Cli as clap::Parser>::try_parse_from(args).unwrap();
            passive_check_eligible(&cli.command.unwrap().into_invocation().unwrap())
        };
        for skipped in [
            &["capscode", "ls", "--json"][..],
            &["capscode", "doctor", "--json"],
            &["capscode", "doctor", "--quick", "--json"],
            &["capscode", "completions", "zsh"],
            &["capscode", "update", "--check"],
            &["capscode", "uninstall", "--yes"],
            &["capscode", "config", "list", "--json"],
            &["capscode", "config", "get", "runtime"],
        ] {
            assert!(!eligible(skipped), "{skipped:?} runs the update check");
        }
        for checked in [
            &["capscode", "ls"][..],
            &["capscode", "doctor"],
            &["capscode", "toolchain", "show"],
            &["capscode", "resume", "alpha"],
        ] {
            assert!(eligible(checked), "{checked:?} skips the update check");
        }
    }

    #[test]
    fn ignored_files_message_names_paths_and_the_only_way_past_them() {
        let paths: Vec<String> = (0..12).map(|index| format!("file{index}")).collect();
        let message = ignored_files_message("alpha-123abc", &paths);
        assert!(message.contains("\n  file0"));
        assert!(message.contains("\n  file9"));
        assert!(!message.contains("file10"));
        assert!(message.contains("... and 2 more"));
        assert!(message.contains("capscode rm alpha-123abc --discard-workspace"));
    }

    #[test]
    fn entering_requires_both_standard_streams_to_be_terminals() {
        assert!(has_interactive_terminal(true, true));
        assert!(!has_interactive_terminal(false, true));
        assert!(!has_interactive_terminal(true, false));
        assert!(!has_interactive_terminal(false, false));
    }

    #[test]
    fn default_entry_lists_multiple_boxes_without_a_tty_and_prompts_with_one() {
        assert_eq!(
            default_entry_decision(2, false),
            DefaultEntryDecision::ListAndExit
        );
        assert_eq!(
            default_entry_decision(2, true),
            DefaultEntryDecision::Choose
        );
        assert_eq!(
            default_entry_decision(1, false),
            DefaultEntryDecision::EnterNeedsTty
        );
    }

    #[test]
    fn box_session_lookup_compares_box_session_ids() {
        let target = crate::boxes::BoxRef {
            name: "project-abc123".to_string(),
            id: "abc123".to_string(),
            session: "00000000-0000-0000-0000-000000000001".to_string(),
        };
        assert!(box_session_matches(
            &target,
            "00000000-0000-0000-0000-000000000001"
        ));
        assert!(!box_session_matches(
            &target,
            "00000000-0000-0000-0000-000000000002"
        ));
    }

    #[tokio::test]
    async fn box_target_resolution_precedes_runtime_selection() {
        let selected_runtime = std::sync::atomic::AtomicBool::new(false);
        let result = resolve_before_runtime(
            || Err::<(), anyhow::Error>(anyhow::anyhow!("no box named 'typo'")),
            || async {
                selected_runtime.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok::<(), anyhow::Error>(())
            },
        )
        .await;

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no box named 'typo'"));
        assert!(!selected_runtime.load(std::sync::atomic::Ordering::SeqCst));
    }
}
