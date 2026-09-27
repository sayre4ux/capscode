# CLI reference

## Global options

Runtime selection and verbosity are global; place them before the command or tool alias, for
example `capscode --runtime orbstack -v codex`. Every command has `--help`; version reporting is
available at the top level.

| Option | Values | Purpose |
|---|---|---|
| `--runtime <runtime>` | `auto`, `docker`, `orbstack` | Select or override runtime detection |
| `-v`, `--verbose` | — | Show detailed Git preparation, transfer, and lifecycle output |
| `-h`, `--help` | — | Show command help |
| `-V`, `--version` | — | Show the CapsCode version (`capscode --version`) |

Normal output keeps internal Git clone, snapshot, and transfer details quiet. Add `-v` when
debugging setup or session behavior.

## First-run setup

```text
capscode init [OPTIONS]
```

| Option | Purpose |
|---|---|
| `--tool <tool>` | Configure `claude`, `codex`, `opencode`, `pi`, or `omp`; repeat for multiple tools |
| `--probe` | Probe available provider credentials over the network |
| `--no-probe` | Never perform a credential network probe |
| `--yes` | Accept defaults and never prompt |
| `--completion-shell <shell>` | Install completion for `bash`, `fish`, or `zsh` without prompting |
| `--no-completions` | Skip completion setup |

Interactive setup asks once whether to bind-mount the host directory (`workspace.mode`) and
then offers shell completion, after the runtime, provider, credential, and VPN checks.
Non-interactive and `--yes` runs always choose `copy` without asking: mount mode is a real
security trade and is never selected silently. On macOS and Linux completion uses the shell's
existing user completion location where one is available. Vanilla shell fallbacks add a guarded, idempotent startup-file block; CapsCode never
creates `~/.zfunc`. Existing unmanaged completion files are not overwritten.

## Commands

```text
capscode [<box>]
capscode create [<name>] [--providers all|<id,id>] [--mount] [--pin] [--no-cache] [--yes]
capscode ls
capscode stop <box>
capscode resume <box> [--providers all|<id,id>] [--toolchain current|keep|<digest>] [--pin] [--cache] [--fresh-cache] [--yes]
capscode toolchain show|ls|prune|build|use|pin
capscode update [--check] [--yes]
capscode update --cli [--check] [--yes] [--allow-downgrade]
capscode sync <box|session>
capscode rm <box|session> [-f|--force] [--discard-workspace]
capscode init [OPTIONS]
capscode doctor [--quick] [--json]
capscode uninstall [--images] [--yes] [--discard-workspace]
capscode key set|ls|rm|reset [<provider>]
capscode provider list|show|add|remove ...
capscode config list|get|set|unset|edit ...
capscode vpn [import <path>|remove]
capscode <tool> [OPTIONS] [-- <TOOL_ARGS>...]
```

With no argument, `capscode` enters the box for the current project. If there is no box and
the current directory is a project folder (not `$HOME` or `/`), an interactive terminal offers
to create one for that folder; the first box prompt mentions the 2.4 GB image built by
`capscode toolchain build`. Without an interactive terminal, or outside a project folder,
CapsCode prints help. If multiple boxes belong to the current project, an interactive terminal
offers a numbered choice; a non-interactive run reports their names and asks you to choose
explicitly. `capscode <box>` enters a box by full name, unique prefix, or six-character id. If
multiple boxes match a prefix, CapsCode prints the matching names and asks you to use a longer
prefix. Subcommands take
precedence over box names. A second word after a box name is an error; enter the box and run
the tool there.

`create` makes a box for the current project. Its name is the supplied name (or the folder
name) plus a unique six-character suffix. `--providers` selects the providers routed through
the gateway; the routed list is shown when the box is created and in `ls`. `resume` rebuilds a
stopped box around its existing workspace. Passing `--providers` replaces its routed provider
set; if the box is running, CapsCode asks before restarting it. Running tools are stopped by
resume.

| Command | Purpose |
|---|---|
| `capscode ls` | List boxes, their state, last-entered time, and routed providers; legacy sessions may also appear. |
| `capscode toolchain show|ls|prune|build|use|pin` | Inspect, build, select, and prune saved toolchain snapshots. |
| `capscode update [--check] [--yes]` | Verify and apply the newest tested toolchain from the release channel (see [Updating](toolchain.md#updating)). |
| `capscode update --cli [--check] [--yes] [--allow-downgrade]` | Replace this binary with the newest tagged CLI release after checking it (see [Updating](toolchain.md#updating)). |
| `capscode stop <box>` | Stop a box while keeping its workspace. |
| `capscode sync <box|session>` | Copy workspace changes back to the host; mount mode has nothing to sync. |
| `capscode rm <box|session> [-f|--force]` | Stop a running box, copy its work back, and remove it. `-f`/`--force` skips the unsynced-work refusal but still stops before copy-back; if capture fails, the workspace and stopped box are kept. |
| `capscode rm <box|session> --discard-workspace` | Remove the box or session and delete its workspace without copying anything back, including gitignored files. |
| `capscode doctor` | Run live runtime, VPN, DNS-leak, kill-switch, and credential checks; exits with status 1 if the runtime or a required gateway/containment check fails. |
| `capscode doctor --quick` | Show local status without network probes; exits with status 1 if the runtime is unavailable. |
| `capscode uninstall` | Remove CapsCode runtime objects, completion markers, and `~/.capscode`; the CLI binary and keychain entries remain. It lists every box workspace it would delete and refuses before confirmation if any holds unsynced work or gitignored files, unless you pass `--discard-workspace`. After confirmation, it stops the boxes and checks again before removal; if new work is found, boxes stay stopped and can be synced before retrying. If the container runtime cannot be listed, it refuses while `boxes.json` lists any box, unless you pass `--discard-workspace`. |

Supported tools are `claude`, `codex`, `opencode`, `pi`, and `omp`. Run a tool directly for
a one-shot session, or enter a box and run it there:

```bash
capscode codex exec "summarize this project"
capscode abc-k3f9x2
# inside the box
claude
```

Direct tool commands accept `--mount` or `--copy`, `--history <full|shallow|redact-authors>`,
`--include <path>`, `--provider <id>`, `--pick`, `--yes`, and `--experimental`. Tool arguments
can follow `--`. Global options such as `-v` go before the command. `--providers all|none|<id,id>`
also works on a one-shot run and routes extra provider credentials for that run.

## Old command → new command

| Old command | New command |
|---|---|
| `capscode run <tool> ...` | `capscode <tool> ...` |
| `capscode status` | `capscode doctor --quick` |
| `capscode auth status` | `capscode key ls` |
| `capscode auth import` | `capscode key set anthropic` |
| `capscode auth remove` | `capscode key rm anthropic` |
| `capscode provider key set|ls|rm|reset ...` | `capscode key set|ls|rm|reset ...` |
| `capscode provider set-default <tool> <id>` | `capscode config set defaults.<tool> <id>` |
| `capscode attach` or `capscode console` | `capscode` or `capscode <box>` |
| `capscode vpn status` | `capscode vpn` |

Old commands print a pointer to the replacement for this release; they are not aliases.

## Sessions, setup, configuration, credentials, and providers

| Command | Purpose |
|---|---|
| `capscode init [OPTIONS]` | Configure tools, credentials, provider defaults, VPN, and shell completion. Use `--yes` for unattended setup and `--no-completions` to skip completion setup. |
| `capscode config list [--json]` | Show persisted and effective configuration. |
| `capscode config get|set|unset <key>` | Read or change an allowlisted setting. |
| `capscode config edit` | Open the config file in `$EDITOR`. |
| `capscode key set <provider>` | Store a provider key with a hidden prompt or stdin; `anthropic` accepts a Claude setup token. |
| `capscode key ls` | List stored keys and credential kinds, never values. |
| `capscode key rm <provider>` | Remove a stored key; use `anthropic` for the Claude setup token. |
| `capscode key reset` | Delete stored provider keys and the Anthropic token after confirmation. |
| `capscode provider list` | List bundled and user-defined providers. |
| `capscode provider show <id>` | Show a provider's endpoint and configuration status. |
| `capscode provider add [OPTIONS]` | Add or override a provider without persisting its secret. |
| `capscode provider remove <id>` | Remove a user-defined provider or override. |
| `capscode vpn` | Show VPN status. |
| `capscode vpn import <path>` | Import allowlisted WireGuard settings; hooks are rejected and DNS must use IP addresses. |
| `capscode vpn remove` | Forget the configured path without deleting the VPN file. |

`provider add` accepts `--id`, `--base-url`, `--api-style anthropic|openai`, `--key-env`,
`--auth bearer|x-api-key`, `--default-model`, and `--experimental`. With a terminal, omitted
values are prompted. Set a tool's default provider with
`capscode config set defaults.<tool> <provider>`.

Allowlisted config keys include `auth.keychain`, `box.prefix` (the box shell's tmux prefix,
default `C-b`), `capsule.opencode_lsp_download` (default `true`), `capsule.cache_volume`
(default `true` for boxes), `defaults.claude`,
`defaults.codex`, `defaults.opencode`, `defaults.pi`, `defaults.omp`, `runtime`, and
`workspace.mode` (`copy` or `mount`, for one-shot tool runs; boxes choose at `create`). Writes
are atomic and private, but they normalize the file — comments and exact layout are not preserved.

Two keys limit the capsule's resources. `capsule.pids` caps the number of processes and threads
(default `1024`, allowed 64 to 4194304); it stops a fork bomb at the container. `capsule.memory`
caps memory, for example `capscode config set capsule.memory 4g` (`k`, `m`, and `g` are binary
units; a bare number is bytes). It is unset by default, which means no memory limit. When it is
set, swap is capped to the same value, and files the agent writes to the capsule's `/tmp`,
home, and `/dev/shm` tmpfs mounts count toward it. `/tmp` defaults to 1024 MiB; set
`CAPSCODE_TMP_TMPFS_MIB` in the host environment to change its size. CPU and disk use are not
limited. Both keys
take effect when a session starts or a box is created or resumed; a running box keeps the limits
it was created or resumed with.

### Inside the capsule

`/home/agent` is a `noexec` tmpfs, while `/tmp`, `/workspace`, and a box's `/cache` are executable.
The pinned Codex daemon package is linked from the image into `~/.codex/packages`; `rg` and `fd`
are installed in the base image. Box npm, Bun, and XDG caches use the per-box `/cache` volume by
default. One-shot sessions and boxes created or resumed with cache volumes disabled use
`/tmp/.cache`, which is cleared when the capsule stops. OpenCode language-server downloads are
allowed by default. To turn them off, run
`capscode config set capsule.opencode_lsp_download false`.

The installer creates a symlink at `$HOME/.local/bin/capscode`. To remove that
binary link, delete the link separately. The provider API-key bundle (the `provider-keys`
keychain item, under the `capscode` service by default) also survives `capscode uninstall`;
run `capscode key reset` before uninstalling if you want to erase those stored keys.

## Shell completion

`capscode init` is the recommended setup path. To generate a script manually:

```bash
capscode completions bash
capscode completions fish
capscode completions zsh
```

The generator also supports `elvish` and `powershell`; automatic installation through `init`
currently targets Bash, Fish, and Zsh on macOS and Linux. Package managers should install global
completion files into their platform-managed locations. CapsCode completes its own commands,
options, and tool aliases; forwarded arguments belonging to Claude, Codex, OpenCode, or Pi are
left to those tools.
