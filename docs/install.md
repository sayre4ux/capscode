# Install

You need:

| Requirement | How to get it |
|---|---|
| macOS or Linux | macOS on Apple silicon or Intel; Linux on x86-64 or arm64 with glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+). Tested on macOS (Apple silicon) with OrbStack and on Ubuntu 24.04 (arm64) with Docker Engine; Docker Desktop and x86-64 are untested |
| Docker with BuildKit, running | macOS: [OrbStack](https://orbstack.dev) or Docker Desktop. Ubuntu: `sudo apt install docker.io docker-buildx`, then add yourself to the `docker` group |
| `curl` and `tar` | Preinstalled on macOS and most Linux distributions |
| About 10 GB free disk | The images take about 4 GB; the gateway's Rust build inside Docker takes most of the rest |

Then run the installer:

```bash
curl -fsSL https://raw.githubusercontent.com/sayre4ux/capscode/main/scripts/install.sh | bash
```

The installer:

1. Checks every requirement above and lists anything missing with the command that fixes it,
   before it changes anything.
2. Downloads the newest release binary for your platform and the matching image build files,
   checks both against the release's `SHA256SUMS`, and installs the binary as
   `~/.local/bin/capscode` (the build files go to `~/.capscode/build/<version>/`). It tells you
   if that directory is not on your `PATH`.
3. Builds the container images with `capscode toolchain build`. The first build is the slow
   step: it downloads the base images and every tool. Later runs reuse Docker's cache. No Rust
   toolchain is needed on your machine; the gateway's broker compiles inside Docker.
4. Runs `capscode init` to set up tools, credentials, provider defaults, and shell completion.
5. Runs `capscode doctor --quick` and prints what to do next.

Install with the installer rather than a browser download. `curl` sets no macOS quarantine
flag, so Gatekeeper does not block the binary; a browser download is quarantined and blocked.

**macOS keychain: one "Always Allow" per CLI update.** The macOS binary is ad-hoc signed:
CapsCode has no Apple Developer ID and is not notarized. macOS identifies an ad-hoc signed
program by its exact build, so the first command that reads a stored key after an install or a
CLI update asks for keychain access. Choose **Always Allow**, in a terminal (an agent's shell
gets no prompt and `capscode doctor` then reports `keychain access denied`). Toolchain updates
do not change the binary and do not prompt.

**Checking the download yourself.** Pass `--verify-attestation` to also require a GitHub
artifact attestation from this repository's release workflow for the binary and the build
files. It needs the [GitHub CLI](https://cli.github.com) and fails if the check cannot run:

```bash
curl -fsSL https://raw.githubusercontent.com/sayre4ux/capscode/main/scripts/install.sh | bash -s -- --verify-attestation
```

Useful options (after `bash -s --` when piping):

| Option | Effect |
|---|---|
| `--check` | Only check the requirements; change nothing |
| `--version <x.y.z>` | Install that release instead of the newest one |
| `--verify-attestation` | Also verify the GitHub attestation with `gh` (see above) |
| `--bin-dir <dir>` | Install the binary somewhere other than `~/.local/bin` |
| `--skip-images` | Skip the image build (run `capscode toolchain build` later) |
| `--skip-init` | Skip `capscode init` (run it later in a terminal) |
| `--skip-doctor` | Skip the final `capscode doctor --quick` |
| `--yes` | Never prompt: runs `capscode init --yes --no-completions` |
| `--binary` / `--from-source` | Choose the install method explicitly (see below) |

**Installing with an AI coding agent?** Ask it to follow [BOOTSTRAP.md](https://github.com/sayre4ux/capscode/blob/main/BOOTSTRAP.md). That file
tells the agent what it may do on its own and what it must leave to you, such as entering API
keys.

## Install from source

Building from source needs, in addition to Docker: git, Rust 1.98 or newer
([rustup.rs](https://rustup.rs), or `rustup update stable`), a C compiler (macOS:
`xcode-select --install`; Debian/Ubuntu: `sudo apt install build-essential`), and about 15 GB of
free disk. Clone and run the installer from the checkout:

```bash
git clone https://github.com/sayre4ux/capscode.git && cd capscode
./scripts/install.sh
```

Inside a checkout the installer builds the CLI (`cargo build --locked --release`, under a minute
on a recent Mac), links it to `~/.local/bin/capscode`, builds the images with
`./scripts/build-images.sh`, and then runs `capscode init` and `capscode doctor --quick` as above.
Run it again after `git pull` to update. `--from-source` forces this method and `--binary` forces
the download even inside a checkout.

<details>
<summary>Manual install, without the installer</summary>

```bash
cargo build --locked --release
mkdir -p "$HOME/.local/bin"
ln -sfn "$PWD/target/release/capscode" "$HOME/.local/bin/capscode"
hash -r
./scripts/build-images.sh
capscode init
```

</details>

After install, `capscode init` can be re-run at any time. For an unattended setup, use
`capscode init --yes`; add `--no-completions` to leave shell startup files alone. For example,
configure Claude and Codex without network credential probes:

```bash
capscode init --yes --tool claude --tool codex --no-probe
```

`build-images.sh` builds the capsule base, five tool images, the **merged** all-tools image,
and the gateway image (including its Rust broker build). The merged image includes every tool
plus `tmux`, Chromium, and `playwright-core`; it is about 2.4 GB. The first build takes a while.
The script stops with a non-zero exit status if any image fails to build, and it verifies each
tool's npm tarball against the integrity hash pinned in `images/toolchain.lock.toml`. The CLI
never pulls missing `capscode/*` images from a registry; it tells you to run
`capscode toolchain build` instead, which runs this script from your checkout or, for a binary
install, from `~/.capscode/build/<version>/`.
Tool images use pinned tool versions, and the merged image tag identifies the full toolchain.

**Supply chain.** CapsCode publishes no images; you build them from this repository's files
(a binary install downloads them as the release's build context). Base
images are pinned by digest, and each tool by version and tarball hash. A tool's own
dependencies are not pinned one by one: they install with `npm install --before=<time>`, where
the time is the pin's `resolved_before` in `images/toolchain.lock.toml`. `bump-tool.sh` normally
selects the newest release on that tool's npm dist-tag channel only after it has been published
for at least three days, and gives dependencies the same three-day cooldown. A maintainer can
record a reason with `--waive-cooldown`; waived dependencies resolve as of the bump time. The
tarball integrity hash is read from the same npm registry for that exact package version. This
does not help if a dependency was already malicious when the pin was set, and Debian packages
float with the Debian index. See the [threat model](threat-model.md#known-residual-risks)
for what a poisoned package could reach.
