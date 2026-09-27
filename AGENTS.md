# Guide for AI coding agents

This file tells an AI coding agent (Claude Code, Codex, or similar) how to work in this
repository. Humans may find it useful too; [CONTRIBUTING.md](./CONTRIBUTING.md) is the
human-oriented version.

If you were asked to install CapsCode for a user rather than change its code, follow
[BOOTSTRAP.md](./BOOTSTRAP.md) instead of this file.

## What this project is

CapsCode runs an untrusted AI coding agent inside a hardened Docker **capsule** whose only network
path is a sibling **gateway** container. The gateway holds the real API keys (inside a TLS broker
with a default-deny allowlist) and an nftables kill switch. The capsule only ever sees placeholder
credentials. The host CLI copies the workspace in and applies the agent's work back out under the
user's control. See [docs/threat-model.md](./docs/threat-model.md) for the full model.

## Repository map

```
crates/
  core/      shared types and traits (ContainerSpec, RuntimeAdapter), config, provider registry
  runtime/   Docker Engine adapter (bollard): containers, exec, interactive sessions
  net/       gateway spec, kill-switch rendering, WireGuard parsing, containment verifier
  broker/    the credential-injecting TLS proxy that runs inside the gateway
  git/       workspace ingress (copy in) and egress (patch, branch, or guarded file sync out)
  cli/       the `capscode` binary: commands, session driver, config, terminal handling, UI
images/      Dockerfiles, entrypoints, keeper (tmux) script, kill-switch reference ruleset,
             pinned tool versions (toolchain.lock.toml)
providers/   registry.yaml, the bundled provider registry
scripts/     install.sh sets up a user's machine end to end; build-images.sh builds every
             image with the tags the CLI expects; bump-tool.sh moves a tool pin (version,
             tarball hash, dependency cutoff)
```

## Verification before you call anything done

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests that need a Docker daemon are gated behind `CAPSCODE_TEST_DOCKER=1` or `#[ignore]`.
Changes to containers, networking, entrypoints, capabilities, or the kill switch also need a live
run against real images (`./scripts/build-images.sh`, then `capscode doctor` and a real session):
unit tests cannot see a broken capability drop, nft ruleset, or tunnel.

## Rules that protect users

These are the point of the project. Do not weaken them, even to make a test pass:

- Real credentials never enter the capsule: not in its environment, files, or arguments. They are
  delivered to the gateway over exec stdin into tmpfs.
- The kill switch is default-DROP and fails closed. The broker is default-deny and derives every
  route from the provider manifest.
- Nothing the agent writes reaches the host without passing the egress checks (path validation,
  hook/runner classification, conflict detection) and the user's decision.
- Anything parsed from an imported file (WireGuard config, provider registry) is validated into
  typed values before it reaches a shell, a ruleset, or the gateway.
- When unsure, fail closed with an error that tells the user what to do next.

## Code conventions

- The CLI crate denies `print_stdout`/`print_stderr`; write output through `crate::ui`
  (`data` for machine-readable stdout, `note` for human-facing stderr).
- User-facing text is plain English and actionable. Do not surface internal names or Rust `Debug`
  output in messages.
- Comments explain why, in terms a new contributor understands. Do not reference issue numbers,
  reviews, private documents, or the tools that wrote the code.
- Add a test with every fix; where practical, one that fails without the fix.
- Change code directly. Do not add compatibility shims or feature flags unless something still
  depends on the old behaviour.

## Commits

Small, focused commits with a message that says why. Do not bypass hooks (`--no-verify`) and do
not rewrite published history.
