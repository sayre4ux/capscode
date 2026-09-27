# Tool versions and updates

## Tool versions

The reviewed lock at `images/toolchain.lock.toml` is compiled into the CLI. The maintainer or
reviewed update bot changes it with `scripts/bump-tool.sh`; a bump resets that tool's smoke grade
to `none`. Run `capscode toolchain build` (or `./scripts/build-images.sh` in a checkout) to build
and smoke the pinned toolchain. A passing
`scripts/smoke-tui.sh all --record` records the smoke date and architecture.

Use `capscode toolchain show` to inspect the active lock, its source, publish dates, cooldowns,
and smoke results. `capscode toolchain ls` lists saved snapshots and local images. The global
default can be changed to a saved digest with `capscode toolchain use <digest>` or reset with
`capscode toolchain use built-in`. One-shot runs use this global default.

Boxes remember the toolchain they were created with. A stopped box whose toolchain differs from
the current default asks before switching; choose `keep` to preserve its existing environment or
`switch` to use the current one. `capscode resume <box> --toolchain current|keep|<digest>` makes
the choice explicit. Running boxes are left alone and report an older toolchain after you detach.
`capscode create --pin` or `capscode resume <box> --pin` keeps a box on its selected snapshot.
Keeping an older toolchain also keeps its known vulnerabilities and missing security fixes.

`capscode toolchain prune` reports and removes images and snapshots that are not current, used by
a box, or among the newest saved snapshots. It keeps one other snapshot by default; use
`--keep N` to change that count or `--yes` to skip confirmation in automation. To test a specific
version set without changing individual tools per box, use `capscode toolchain pin
<tool>@<version> --untested --integrity <sha512-value>`; obtain the hash with
`npm view <package>@<version> dist.integrity`.

## Updating

Tool versions and the CLI are released separately. A toolchain release publishes a new tested
lock; `capscode update` applies it without rebuilding the CLI:

```bash
capscode update --check   # show what is available and change nothing
capscode update           # verify and apply the newest tested toolchain
capscode toolchain build  # build the images for it (minutes, a few GB)
```

`capscode update` downloads `channel.json` and the lock it names from GitHub over HTTPS and
refuses the lock unless every check passes: its SHA-256 matches the manifest; every package and
base image is one the CLI already allows, and every version, hash, and date has the expected
shape; its sequence number is newer than any toolchain this machine has accepted; it does not
need a newer CLI; it is not withdrawn ("yanked"). A toolchain that moves a tool to an older
version must record why, and the CLI asks you to confirm (or accepts `--yes`). Boxes are not
changed: a running box keeps its toolchain, and resuming a stopped box asks whether to switch or
keep (keep is the default). `capscode ls` marks boxes whose toolchain was withdrawn, and
`capscode toolchain use` and `capscode resume --toolchain <digest>` refuse a withdrawn digest
unless you pass `--force`.

To update the CLI itself, run `capscode update --cli`:

```bash
capscode update --cli --check   # show the new version and its changelog; change nothing
capscode update --cli           # download, verify, and replace the binary (asks first)
capscode toolchain build        # build the images the new version expects
```

It installs tagged releases only (`vX.Y.Z`, never a prerelease or a branch build), shows the
version change and the release's changelog section, and asks before it changes anything; pass
`--yes` when there is no terminal. It downloads the binary for your platform and the release's
image build files, refuses either unless it matches the release's `SHA256SUMS` (and, with
`update.verify_with_gh = true`, a GitHub attestation from the CLI release workflow), checks that
the new binary runs, then replaces the old one with a single rename, so an interrupted update
leaves the old binary in place. It refuses to install an older release unless you pass
`--allow-downgrade`. A source build (a binary under a checkout's `target/`) is not replaced:
update it with `git pull && ./scripts/install.sh` in the checkout. On macOS, the first keychain
read after an update asks once for access (choose Always Allow, see [Install](install.md)).

**Daily check.** Once a day, a command run in a terminal also fetches the 1 KB `channel.json` in
the background and, after the command's own output, prints at most one line such as
`toolchain 8 is available — capscode update`. The request carries only a
`User-Agent: capscode/<version>` header. Any failure is silent, and the check never adds more
than about three seconds to a command. It is skipped for `--help`, completions, `--json` output,
and output that is not a terminal. This is the only network request CapsCode makes from your
machine without being asked. Turn it off with `capscode config set update.check false` or
`CAPSCODE_NO_UPDATE_CHECK=1`.

**Verifying releases yourself.** Each toolchain lock is published with a GitHub artifact
attestation. `capscode toolchain show` prints the lock and attestation URLs for an applied
toolchain. To check a lock by hand with the [GitHub CLI](https://cli.github.com):

```bash
gh attestation verify toolchain.lock.toml --repo sayre4ux/capscode \
  --signer-workflow sayre4ux/capscode/.github/workflows/release-toolchain.yml
```

`capscode config set update.verify_with_gh true` makes `capscode update` run that command on
every downloaded lock, and `capscode update --cli` run it on the downloaded binary and build
files, and refuse the download if `gh` is missing or the verification fails. Both pin the
workflow that must have signed the attestation (`--signer-workflow`). See
[release-assets.md](release-assets.md) for every release file and how to check it.

**Fork channel.** Every update URL comes from `update.channel` (default `sayre4ux/capscode`).
If this repository stops publishing, a fork that runs the same release workflows can take over
with `capscode config set update.channel <owner>/<repo>`. The fork must continue the sequence
numbers above the last release you applied.

> Rebuild the gateway image whenever the broker changes; its binary is baked into that image.
