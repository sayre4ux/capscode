---
hide:
  - navigation
  - toc
---

# CapsCode { .cc-visually-hidden }

<p align="center">
  <img src="assets/wordmark-light.png#only-light" alt="CapsCode" width="380">
  <img src="assets/wordmark-dark.png#only-dark" alt="CapsCode" width="380">
</p>

**Isolated Docker capsules for AI coding tools.** Run `claude`, `codex`, `opencode`, `pi`, or
`omp` in a container with a private copy of your project, credentials held outside the
container, and network containment. You decide what comes back to your host.

```bash
curl -fsSL https://raw.githubusercontent.com/sayre4ux/capscode/main/scripts/install.sh | bash
cd ~/my-project
capscode claude
```

CapsCode is at v0.1, its first public release. It is tested on macOS (Apple silicon) with
OrbStack, and on Ubuntu 24.04 (arm64) with Docker Engine for install, `doctor`, and one-shot
sessions.

## Start here

| Page | Covers |
|---|---|
| [Install](install.md) | Requirements, installer options, building from source, supply chain |
| [Authentication](authentication.md) | API keys, subscriptions, keychain storage, providers |
| [Boxes](boxes.md) | Create, enter, sync, resume, cache volumes, mount mode |
| [How your files are handled](files.md) | Git snapshots, copy-back, branches, non-git folders |
| [Networking](networking.md) | Containment-only mode, WireGuard tunnel, egress checks |
| [Tool versions and updates](toolchain.md) | Pinned toolchains, `capscode update`, verifying releases |
| [CLI reference](cli.md) | Every command, option, and config key |
| [Threat model](threat-model.md) | What CapsCode protects, what it does not, and residual risks |
| [Roadmap](roadmap.md) | Planned work and non-goals |

Source, issues, and releases are on [GitHub](https://github.com/sayre4ux/capscode).
