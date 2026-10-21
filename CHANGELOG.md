# Changelog

This file follows the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format.

## [Unreleased]

### Fixed

- After a full non-Git `sync`, `rm` could keep saying the box had work that was not synced.
  Edits newer than the box's last automatic checkpoint left its private checkpoint repository
  dirty, and a stopped box never took another checkpoint. A full sync now commits the tree it
  brought back, so the box reads as synced until it changes again.

## [0.1.1] - 2026-10-20

### Fixed

- `uninstall` could delete the only copy of file sync changes that did not apply. A non-Git
  `sync` that applied some files but left conflicts staged marked the box as synced, so
  `uninstall` removed the box and `~/.capscode` without warning. A sync now marks a box synced
  only when nothing is left behind (pending deletions aside), and says when it applied only part
  of the work. `uninstall` also refuses while any session keeps staged changes, backups of
  overwritten host files, or earlier handoffs under `~/.capscode/sessions`, unless you pass
  `--discard-workspace`.
- `capscode key set` and `capscode init` now name the keychain service the key was written to
  when `CAPSCODE_KEYSTORE_SERVICE` overrides it.

## [0.1.0] - 2026-10-19

First public release.

### Added

- One-shot Docker capsules for Claude Code, Codex, OpenCode, Pi, and omp: `capscode <tool>`.
- Persistent per-project boxes with every tool installed, entered with `capscode [<box>]` and
  managed with `create`, `ls`, `stop`, `resume`, `sync`, and `rm`.
- A credential broker in the gateway that holds provider credentials and injects them into
  upstream requests; the capsule only sees placeholders.
- Network containment with an nftables kill switch, and an optional WireGuard tunnel mode.
- Git and non-Git workspace handoff with path validation, hook and runner classification,
  conflict detection, and configurable history modes.
- Prebuilt CLI binaries for macOS (universal, ad-hoc signed) and Linux (x86-64 and arm64), with
  `SHA256SUMS` and GitHub artifact attestations, plus the image build files as a release asset.
- `scripts/install.sh`, which checks prerequisites, downloads and verifies the release binary (or
  builds the CLI from a checkout), builds the images, and runs first-time setup; `BOOTSTRAP.md`, step-by-step install instructions for AI coding agents; and
  `scripts/bump-tool.sh` for moving a tool pin.
- `capscode update --cli`, which replaces the binary with the newest tagged release after
  checking its checksum (and optionally its attestation), and `capscode update` for tested
  toolchain releases.
- Credential storage in the OS keychain (`capscode key`), provider configuration
  (`capscode provider`, `capscode config`), shell completions, and diagnostics
  (`capscode doctor`, `capscode doctor --quick`).

[Unreleased]: https://github.com/sayre4ux/capscode/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/sayre4ux/capscode/releases/tag/v0.1.1
[0.1.0]: https://github.com/sayre4ux/capscode/releases/tag/v0.1.0
