# Maintainers and release authority

CapsCode is currently maintained in the personal repository
`sayre4ux/capscode`; there is no GitHub organization or second owner. The
repository owner is the only current administrator and recovery contact.

## Who can release or merge

- Anyone with repository write access may create a `v*` tag. The release
  workflow requires the matching Keep a Changelog section and a green test
  matrix before it builds the platform binaries and publishes them with the
  source tarball, the image build context, `SHA256SUMS`, and one attestation
  covering all of them (see `docs/release-assets.md`). A prerelease tag
  (`v1.2.0-rc.1`) becomes a GitHub prerelease and does not update the channel.
- Maintainers with write access may merge pull requests to `main` after the
  required CI checks pass. Toolchain lock changes also require the built image
  smoke job; the daily bot enables auto-merge after those checks pass.
- Only the two release workflows write the `channel` branch. Do not make manual
  commits to that branch.
- A toolchain release is created by `release-toolchain.yml` after a lock change
  reaches `main`, or by a maintainer running that workflow manually.
- To withdraw a toolchain version, submit a pull request that adds its
  `tool@version` (or image digest) to `yanked` and advances `sequence`; the
  release workflow publishes the reviewed lock without pushing back to `main`.

## Bumping a tool by hand

The daily bot normally does this. When you bump a tool yourself:

1. `scripts/bump-tool.sh <tool> --latest` picks the newest version on the
   tool's channel that is at least 3 days old and never below the current pin.
   For an urgent fix, pass `--waive-cooldown "<reason>"`; the reason is written
   into the lock and shown by `capscode toolchain show`.
2. Read the version's release notes for anything about daemons, installers,
   auto-update, native binaries, downloads, sandboxing, or temporary files.
   These are the changes that have broken capsules before.
3. `./scripts/build-images.sh` builds the images and runs
   `scripts/smoke-tui.sh` for every tool; on success it records
   `smoke = "ci"`, the date, and the architecture in the lock. Do not use
   `--no-smoke` for a bump.
4. Start each changed tool interactively in a real box: for codex, two panes,
   `codex agents`, closing the first pane, and stop/resume. Then set that
   tool's `smoke = "session"`.
5. Advance `sequence` in the lock and open a pull request. The toolchain
   release runs after it merges.

A CLI release sets the lock's `min_cli_version` to the new version without
advancing `sequence`, because a test requires the two to match. The toolchain
release workflow then finds the lock equal to the latest toolchain release apart
from that line, reports there is nothing to release, and skips its remaining
steps. Any other change at an already released `sequence` still fails it.

## Workflow action pins

Workflow actions are pinned to full commit SHAs with the release tag in a
comment. To update one, resolve the new tag to its commit and replace both.

## Bot token and CI fallback

Set the repository Actions secret `BOT_TOKEN` to a GitHub App token or PAT with
the permissions needed to push the bot branch, open/update its pull request,
and enable auto-merge. The workflow prefers this token because pull requests
opened with the default `GITHUB_TOKEN` do not start the normal `pull_request`
CI workflow. If `BOT_TOKEN` is absent, the workflow falls back to
`GITHUB_TOKEN`, updates the PR, and requests auto-merge. It waits for the
required checks, so a maintainer must trigger and review those checks manually
before the PR can merge.

## Maintainer absence

There is no named successor with owner-level access today. If the repository
owner is unavailable for 90 days, no new upstream release is promised. Do not
bypass required checks or publish from an untrusted account. Contributors may
fork CapsCode, set their own `update.channel` to that fork, and run their own
release workflows. The upstream owner should name a second maintainer and
document the transfer before relying on a succession path.
