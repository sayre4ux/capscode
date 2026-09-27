# Bootstrapping CapsCode: instructions for AI agents

This file is for an AI coding agent that a user has asked to install CapsCode. Read all of it
before you run anything. Humans can follow the shorter [install guide](docs/install.md)
instead.

CapsCode runs AI coding tools in Docker containers that never see the user's real API keys.
Installing it touches the user's credentials, Docker daemon, and shell setup, so some steps are
yours to run and some belong to the user. The rules below say which.

## What "done" means

1. `capscode --version` prints a version, from a `capscode` on the user's `PATH`.
2. `capscode doctor --quick` shows the runtime and every image as present.
3. The user has at least one credential that `capscode doctor --quick` lists, and they
   entered it themselves.
4. You have reported the result to the user in the format under [Report back](#report-back).

## Rules

1. **Never handle secrets.** Do not ask the user to paste an API key, setup token, or WireGuard
   config into the conversation. Do not pipe a key into `capscode key set` yourself. Do not
   read `~/.capscode/`, the OS keychain, `~/.codex/auth.json`, or shell profiles looking for
   keys. The user enters credentials in their own terminal (step 5).
2. **Ask before installing system software**: Rust, Xcode Command Line Tools, compilers, Docker,
   OrbStack, or any package-manager install. Ask before any `sudo`. Ask before starting or
   stopping Docker.
3. **Ask before editing shell startup files** (`~/.zshrc`, `~/.bashrc`, and similar), including
   adding a directory to `PATH` or installing shell completion.
4. **Do not weaken a check to get past it.** Never edit `images/toolchain.lock.toml`, a
   Dockerfile, the installer, or the kill-switch and broker settings to make a build or check
   pass. If a download fails its checksum or attestation check, or an image build fails an
   integrity check, stop and report it: it can mean a tampered file.
5. **Do not run destructive commands** unless the user asks for that exact command:
   `capscode uninstall`, `capscode rm`, `--force`, `--discard-workspace`, `docker system prune`.
6. **Ask before spending the user's tokens.** A test session calls a paid model API.

## Step 1: Get the installer

The default is the **binary install**: the installer downloads the release binary and the image
build files, checks them against the release's `SHA256SUMS`, and needs no Rust toolchain.
Download the installer to a file in the user's home first, so you run exactly what you fetched
(a fixed name under the shared `/tmp` could be created beforehand by another account):

```bash
curl -fsSL https://raw.githubusercontent.com/sayre4ux/capscode/main/scripts/install.sh -o ~/capscode-install.sh
```

Delete `~/capscode-install.sh` when you are done.

Use the **source install** instead only if the user asks for it or step 2 reports no release
binary for the machine. Then clone the repository (skip this if the current directory is already
a CapsCode checkout, with `scripts/install.sh`) and use `./scripts/install.sh` wherever the steps
below say `bash ~/capscode-install.sh`:

```bash
git clone https://github.com/sayre4ux/capscode.git && cd capscode
```

## Step 2: Check prerequisites

```bash
bash ~/capscode-install.sh --check
```

This changes nothing. Each line reads `ok`, `warning`, or `MISSING`, and every `MISSING` line
names the command that fixes it. Exit status 0 means everything is in place; 1 means something
is missing.

For each `MISSING` item, tell the user what is missing and the fix command, and ask before
running it (rule 2). Common fixes:

| Missing | Fix |
|---|---|
| Docker | macOS: install OrbStack from https://orbstack.dev. Ubuntu: `sudo apt install docker.io docker-buildx`, then `sudo usermod -aG docker $USER` and log in again |
| Docker daemon not running | macOS: `orb start` or open Docker Desktop. Linux: `sudo systemctl start docker` |
| Docker BuildKit | Install the buildx plugin; OrbStack and Docker Desktop include it |
| `curl`, `tar`, `gzip`, or a SHA-256 tool | Install it with the package manager (Ubuntu: `sudo apt install curl tar gzip coreutils`) |
| No release binary for this machine | Use the source install (step 1) |
| Source install only: Rust, or Rust too old | Install from https://rustup.rs, or `rustup update stable` |
| Source install only: C compiler | macOS: `xcode-select --install` (opens a dialog the user must accept). Debian/Ubuntu: `sudo apt install build-essential` |
| Source install only: git | macOS: `xcode-select --install`. Ubuntu: `sudo apt install git` |

A `warning` about disk space is not fatal, but the image build needs about 10 GB (15 GB for a
source install). Re-run the check until it exits 0.

## Step 3: Install

You have no interactive terminal, so run the installer without prompts:

```bash
bash ~/capscode-install.sh --yes
```

The binary install downloads and verifies the CLI, installs it as `~/.local/bin/capscode`, puts
the image build files in `~/.capscode/build/<version>/`, builds the container images with
`capscode toolchain build`, runs `capscode init --yes --no-completions`, and ends with
`capscode doctor --quick`. A source install builds the CLI with cargo and links it instead.
Progress lines start with `==>`.

- **Allow a long timeout.** The download takes seconds, but the first image build downloads
  base images and every tool (several GB) and is the slow step. If your tool calls time out,
  run the command in the background and poll it. A re-run reuses Docker's cache.
- **Exit status:** 0 success, 1 a prerequisite, download, verification, or build step failed
  (the error is above the last `==>` line), 2 a bad option.
- If `gh` (the GitHub CLI) is installed and the user wants the extra check, add
  `--verify-attestation`; it also requires GitHub's attestation for the binary and build files
  and fails if that check fails.
- To install somewhere other than `~/.local/bin`, add `--bin-dir <dir>`.
- Each tool image gets a short start-up smoke test while it builds. If the image build fails,
  the error says how to retry. When only the smoke test failed, rerun with `--no-smoke` to build
  without it, or use `--skip-images` to finish setup and build the images later with
  `capscode toolchain build`.
- If the user wants to walk through setup themselves (credentials, VPN, completion), use
  `--skip-init` instead of `--yes` and ask them to run `capscode init` in their own terminal
  afterwards.

## Step 4: Make `capscode` reachable

If the installer printed `warning  <dir> is not on your PATH`, show the user the `export PATH=...`
line it printed and ask whether to add it to their shell profile (rule 3). Until then, call the
binary by its full path, for example `~/.local/bin/capscode`.

## Step 5: Credentials (the user does this)

List the providers CapsCode knows:

```bash
capscode provider list
```

Ask the user which tools and providers they want. Then tell them the command to run **in their
own terminal**; it prompts for the key without echoing it:

| Tool or provider | Command for the user |
|---|---|
| Claude Code (Anthropic API key or `claude setup-token` output) | `capscode key set anthropic` |
| Codex with a ChatGPT login | `codex login` on the host, once; CapsCode reuses that login (`$CODEX_HOME`, default `~/.codex`) |
| Any other provider in `capscode provider list` | `capscode key set <provider-id>` |

On macOS, the first command that reads a stored key after an install or CLI update asks for
keychain access in a dialog. Only the user can answer it: ask them to run `capscode doctor
--quick` once in their own terminal and choose **Always Allow**. Until then, your shell sees
`keychain access denied`. This happens once per CLI install or update.

An exported environment variable (for example `OPENAI_API_KEY`) also works and overrides stored
keys. When the user says they are done, run `capscode doctor --quick`: the "Providers &
Credentials" section lists what CapsCode found, without printing any value.

An optional VPN is the user's call: `capscode vpn import <file.conf>` with a WireGuard config
they provide. Do not look for or open VPN configs yourself.

## Step 6: Verify

```bash
capscode doctor
```

This starts a short-lived gateway container and checks the runtime, network containment, and
credentials. It contacts `1.1.1.1` (Cloudflare) to report the public egress IP. Every check
should pass or explain itself.

If the user agrees to spend a few tokens (rule 6), run one real session in a throwaway Git
repository:

```bash
mkdir -p /tmp/capscode-smoke && cd /tmp/capscode-smoke && git init -q
git -c user.name=smoke -c user.email=smoke@localhost commit -q --allow-empty -m init
capscode codex --yes exec "Reply with exactly the word PONG and nothing else."
```

Swap in the tool the user configured. `PONG` in the output means the tool reached its model
through CapsCode's broker. Boxes (`capscode create`, `capscode <box>`) need an interactive
terminal to enter, so leave trying those to the user.

## When something fails

| Symptom | What to do |
|---|---|
| `capscode: command not found` | Step 4, or use the full path |
| `does not match its SHA-256 in SHA256SUMS`, `refusing to install`, or `gh attestation verify rejected` | Stop. Report it to the user verbatim; do not retry from another source or skip the check (rule 4) |
| `keychain access denied` (macOS) | The user must allow keychain access once in their terminal: step 5 |
| Cargo error mentioning `linker 'cc' not found` | Source install: C compiler missing, step 2 |
| `Cannot connect to the Docker daemon` / `no supported container runtime detected` | Docker is not running: ask the user to start it |
| Image build: `integrity mismatch` | Stop. Report it to the user verbatim; do not retry around it (rule 4) |
| Image build: `ENOVERSIONS` or network errors | Retry once. If it fails again, report the error |
| `no stored keys; run capscode key set <id>` | Credentials missing: step 5 |
| `requires --yes when stdin is not interactive` | Add `--yes` only if the user agreed to that choice; otherwise hand the command to the user |
| Anything else | Report the command, its exit status, and the last 30 lines of output |

Do not work around a failure by changing CapsCode's code or configuration.

## Report back

Tell the user, briefly:

1. The installed version (`capscode --version`), whether it is a binary or source install, and
   where the binary is.
2. Whether the images built, and the `capscode doctor --quick` result.
3. Anything they still need to do: add the bin directory to `PATH`, store a key, start Docker.
4. How to start: `cd <their project> && capscode <tool>` for a one-shot run, or
   `capscode create` then `capscode` for a persistent box.

## Updating and removing

- Update the CLI (binary install): `capscode update --cli --check` shows the new version and its
  changelog; with the user's agreement, `capscode update --cli --yes` downloads, verifies, and
  replaces the binary, then `capscode toolchain build` builds the images it expects. On macOS
  the user then allows keychain access once more (step 5).
- Update a source install: `git pull && ./scripts/install.sh --yes --skip-init` (keeps the
  existing setup).
- Update the tool versions: `capscode update --check`, then `capscode update` and
  `capscode toolchain build`.
- Remove: only when the user asks. `capscode uninstall` lists what it will delete and refuses
  while a box holds work that is not synced. It leaves the binary link and stored keychain items;
  see [Configuration and uninstall](docs/cli.md#sessions-setup-configuration-credentials-and-providers)
  in the CLI reference.
