# Release channel and toolchain lock format

This document defines the `channel.json` manifest and the release metadata in
`images/toolchain.lock.toml`. JSON object fields listed here are required. The
channel reader accepts unknown fields for forward compatibility; it must reject
missing required fields, wrong types, and values outside the constraints below.

## `channel.json`

The two release workflows are the only writers. Each writes the regenerated file
to the workflow-only `channel` branch. URLs use HTTPS and point to the GitHub
release assets for the corresponding tag.

| Field | Type | Constraint | Written by |
|---|---|---|---|
| `schema` | integer | Exactly `1`. | Both release workflows |
| `generated_at` | string | UTC RFC 3339 timestamp ending in `Z`; at most 32 characters. | Both release workflows |
| `cli` | object | All fields below are required. | Both release workflows |
| `cli.version` | string | SemVer, at most 64 characters; every numeric part fits in an unsigned 64-bit integer. | CLI release workflow |
| `cli.tag` | string | Exactly `v` followed by `cli.version`; at most 72 characters. | CLI release workflow |
| `cli.commit` | string | 7–64 lowercase hexadecimal characters identifying the tagged commit. | CLI release workflow |
| `cli.min_toolchain_sequence` | integer | At least `1`, within the exact JSON integer range. | CLI release workflow |
| `cli.notes_url` | string | HTTPS URL, at most 2,048 characters. | CLI release workflow |
| `cli.yanked_versions` | array of strings | Each item is a SemVer release version, at most 64 characters, with numeric parts that fit in an unsigned 64-bit integer. Empty is valid. | CLI release workflow |
| `toolchain` | object | All fields below are required. | Both release workflows |
| `toolchain.sequence` | integer | At least `1`, within the exact JSON integer range; equals the lock's `sequence`. | Toolchain release workflow |
| `toolchain.tag` | string | Exactly `toolchain-<sequence>`. | Toolchain release workflow |
| `toolchain.digest` | string | Exactly 12 lowercase hexadecimal characters: the first 12 hex characters of SHA-256 over the lock's image inputs, one tab-separated line each: `base<TAB><key><TAB><value>` for every `[base]` entry, `seeder<TAB>digest<TAB><value>` and `seeder<TAB>image<TAB><value>`, and `tool<TAB><name><TAB><field><TAB><value>` for each tool's `package`, `version`, `integrity`, and `resolved_before`. The lines are sorted bytewise and joined with newlines (no final newline). Smoke and release metadata are not covered. `scripts/lib/toolchain-lock.sh` computes the same value. | Toolchain release workflow |
| `toolchain.min_cli_version` | string | SemVer, at most 64 characters, with numeric parts that fit in an unsigned 64-bit integer; equals the lock's value. | Toolchain release workflow |
| `toolchain.lock_url` | string | HTTPS URL for `toolchain.lock.toml`, at most 2,048 characters. | Toolchain release workflow |
| `toolchain.lock_sha256` | string | Exactly 64 lowercase hexadecimal characters; SHA-256 of the released lock bytes. | Toolchain release workflow |
| `toolchain.attestation_url` | string | HTTPS URL for the lock attestation bundle, at most 2,048 characters. | Toolchain release workflow |
| `toolchain.attestation_sha256` | string, optional | Exactly 64 lowercase hexadecimal characters; SHA-256 of the lock's attestation bundle (`attestation_url`). The toolchain release workflow attests the lock first and then writes this field. Readers accept a manifest without it, the only optional field, so a manifest from an older workflow still parses. | Toolchain release workflow |
| `toolchain.yanked` | array of strings | Each item is `tool@version`, where `tool` is a lock tool key and `version` is SemVer, or a 12-character toolchain digest that withdraws a whole toolchain; at most 160 characters per item. Empty is valid. | Toolchain release workflow |

`cli.yanked_versions` withdraws CLI releases. `toolchain.yanked` withdraws
individual tool versions, with entries that use the tool key from the lock, such
as `codex@0.160.0`, or whole toolchains by digest, such as `6d07f8901234`. A
toolchain is withdrawn when its digest is listed or any of its tool versions is. A `toolchain` release updates its object and preserves the
current `cli` object. A CLI release updates its object and preserves the current
`toolchain` object. On the first release, the workflow initializes both objects
from the tagged source and lock.

`sequence` is strictly increasing and advances once for each toolchain release.
The released tag must match it. `digest` covers everything that changes the built images (base and seeder images, and each tool's package, version, integrity, and dependency cutoff), while
`lock_sha256` covers the complete TOML file, including its release metadata.

## `images/toolchain.lock.toml`

The lock file is TOML. `scripts/bump-tool.sh` writes a tool's npm version,
integrity, and dependency cutoff. The daily bot writes the sequence, compatibility
metadata, and smoke results after its image smoke gate passes. Maintainers may
edit base image pins and recorded downgrade reasons. Release workflows publish
the lock but do not change package pins.

### Top-level release metadata

| Field | Type | Constraint | Written by |
|---|---|---|---|
| `schema` | integer | Exactly `1`; readers reject a higher schema they do not support. | Maintainer / bump bot |
| `sequence` | integer | Unsigned 64-bit range; at least `1`; advances once per toolchain release. | Daily bump bot |
| `min_cli_version` | string | SemVer; the oldest CLI version allowed to consume this lock. | Daily bump bot |
| `yanked` | array of strings | At most 256 entries, each `tool@version` for a withdrawn tool version or a 12-character toolchain digest; each item is at most 160 characters. | Maintainer |

### Base and seeder tables

No other keys are accepted in `[base]` or `[seeder]`. Each image field must name
exactly one repository: `[base].image` is `debian`, `[base].node_image` is
`node`, `[base].bun_image` is `oven/bun`, and `[seeder].image` is `alpine`. Tags
are 1–128 characters of letters, digits, `.`, `_`, and `-`, starting with a letter,
digit, or `_`.

| Field | Type | Constraint | Written by |
|---|---|---|---|
| `[base].image`, `[base].node_image`, `[base].bun_image` | string | Docker image name and tag, at most 255 characters; the repository component is validated by the CLI allowlist. | Maintainer |
| `[base].digest`, `[base].node_digest`, `[base].bun_digest` | string | `sha256:` plus exactly 64 lowercase hexadecimal characters. | Maintainer |
| `[base].node_version`, `[base].bun_version` | string | SemVer runtime version shipped by the corresponding image, at most 64 characters. | Maintainer |
| `[seeder].image` | string | Docker image name and tag, at most 255 characters; validated by the CLI allowlist. | Maintainer |
| `[seeder].digest` | string | `sha256:` plus exactly 64 lowercase hexadecimal characters. | Maintainer |

### `[tools]` entries

Each key is a stable CapsCode tool identifier: `claude`, `codex`, `opencode`,
`pi`, `omp`, or `playwright_core`.

| Field | Type | Constraint | Written by |
|---|---|---|---|
| `package` | string | Exact npm package name, at most 214 characters; must match the CLI's compiled-in allowlist for that tool. | `bump-tool.sh` or maintainer |
| `version` | string | Exact npm SemVer version, at most 64 characters. | `bump-tool.sh` |
| `integrity` | string | `sha512-` followed by the npm tarball's base64 SHA-512 integrity value, at most 128 characters. | `bump-tool.sh` |
| `resolved_before` | string | UTC RFC 3339 timestamp ending in `Z`, at most 32 characters; npm dependencies resolve only to versions published by this cutoff. | `bump-tool.sh` |
| `channel` | string | npm dist-tag used by `bump-tool.sh --latest`; 1–64 ASCII letters, digits, dots, underscores, or hyphens. | Maintainer |
| `home_exec_paths` | array of strings | At most 256 absolute executable path patterns under the tool home, each at most 1,024 characters. Empty is valid. | Maintainer / smoke review |
| `path_tools` | array of strings | At most 32 executable names, each at most 64 characters; currently `rg` and `fd`. | Maintainer |
| `daemon_seed_layout` | integer | Positive integer Codex managed-daemon package layout version. Present only when needed by that tool. | Maintainer |
| `needs_exec_tmpdir` | boolean | Whether startup needs an executable temporary directory. | Maintainer |
| `smoke` | string | One of `none`, `ci`, or `session`. | Daily bump bot after smoke; maintainer after a real session |
| `smoke_date` | string | Empty when `smoke` is `none`; otherwise a real UTC calendar date in `YYYY-MM-DD` form for the recorded smoke. | Daily bump bot / maintainer |
| `smoke_arch` | array of strings | Empty when `smoke` is `none`; otherwise at most 2 architecture labels from `amd64` and `arm64` on which smoke passed. | Daily bump bot / maintainer |
| `cooldown_waived` | string | Empty when not waived; otherwise a reason of at most 512 characters for bypassing the three-day npm cooldown. | `bump-tool.sh` |
| `downgrade_reason` | string | Optional reason of at most 512 characters for a deliberate version downgrade; omitted when not applicable. | Maintainer |

Fields specific to a tool may be absent for other tools. Unknown TOML keys at the
top level or inside a tool entry are ignored by older readers within the same
schema, provided each key is a lowercase identifier (`a-z`, digits, `_`, at most
64 characters) and every string inside it is printable text of at most 1,024
characters with no control characters. Schema changes that alter the meaning or
type of an existing field require a schema increment.

`published_at` and `resolved_before` accept optional fractional seconds before the
`Z`. The tool set must match the CLI's built-in lock exactly: no tool may be added
or removed, and each `package` must equal the built-in package for that tool.

## Client checks

`capscode update` applies a channel lock only when all of the following hold, and
refuses otherwise. There is no signing key; these checks and TLS are the trust
model.

1. `channel.json` is fetched from
   `https://raw.githubusercontent.com/<update.channel>/refs/heads/channel/channel.json`
   and is at most 64 KiB. The lock is at most 256 KiB. Requests carry only a
   `User-Agent: capscode/<version>` header.
2. Every fetch uses HTTPS with certificate verification against the Mozilla root
   store. Redirects are followed by the client one hop at a time, at most five,
   and only to `https://` URLs on `github.com`, `raw.githubusercontent.com`,
   `objects.githubusercontent.com`, or `release-assets.githubusercontent.com`
   without credentials or a non-standard port.
3. `schema` is `1` and every field passes the constraints above. `lock_url` and
   `attestation_url` must equal
   `https://github.com/<update.channel>/releases/download/<tag>/toolchain.lock.toml`
   and the same URL with `.sigstore`; `notes_url` must be under
   `https://github.com/<update.channel>/`.
4. `toolchain.sequence` is greater than both the active lock's `sequence` and the
   highest sequence this machine has accepted before (kept in
   `~/.capscode/update-state.json`), so an older manifest is refused even after
   `capscode toolchain use built-in`. A fork that takes over the channel continues
   the sequence above the last upstream release.
5. `toolchain.min_cli_version` and the lock's `min_cli_version` are equal and not
   newer than the running CLI. Versions are ordered by SemVer precedence; a
   version the client cannot order (for example a numeric part too large for
   64 bits) refuses the lock rather than comparing as equal.
6. SHA-256 of the downloaded lock bytes equals `lock_sha256`; the lock passes the
   validation above; its `sequence` equals `toolchain.sequence`; its digest equals
   `toolchain.digest`; it is not marked `untested`.
7. Neither the toolchain digest nor any of its tool versions appears in
   `toolchain.yanked`, the lock's `yanked`, the active lock's `yanked`, or the
   `toolchain.yanked` of the newest manifest stored on this machine.
8. A tool whose version is lower than in the active lock needs a non-empty
   `downgrade_reason` in the new lock, then interactive confirmation or `--yes`.
   A tool version pair that cannot be ordered refuses the lock.
9. With `update.verify_with_gh = true`, `gh attestation verify <lock> --repo
   <update.channel> --signer-workflow
   <update.channel>/.github/workflows/release-toolchain.yml` must succeed; a
   missing `gh` also refuses the lock.

The passive daily check applies the manifest checks (1–3) and reports at most one
line; any failure is silent and it never applies anything.

Both `capscode update` and the passive check store the last valid manifest in
`~/.capscode/update-check.json`; its yank lists are what `ls`, `toolchain use`,
and `resume` consult offline. A fetched manifest replaces the stored one only if
its `toolchain.sequence` and `generated_at` are both at least the stored
manifest's, so a stale copy served by a cache cannot roll a yank list back. A
manifest from a different `update.channel` always replaces it.

### Runtime lock from an older CLI

`capscode update` writes the channel lock to `~/.capscode/toolchain.lock.toml`,
which wins over the lock built into the binary. When a newer CLI's built-in lock
has a higher `sequence` (or, when the stored manifest describes the running CLI
release, `cli.min_toolchain_sequence` is higher than the runtime lock's
`sequence`), the client prefers the built-in lock, removes the runtime lock, and
prints one note naming `capscode toolchain use <digest>` to switch back. This
applies only to a runtime lock that `capscode update` wrote and nothing has
rewritten since; a lock chosen with `toolchain use` or `toolchain pin` (or marked
`untested`) is kept with a warning, and `image.toolchain_lock` always wins.

### Bounds for the channel parser

String fields use the length caps in the table above, and arrays hold at most
256 items. JSON sequence values are at most `9,007,199,254,740,991` (2^53 - 1),
the largest integer every common JSON implementation represents exactly. TOML
`sequence` keeps the full unsigned 64-bit range.

CLI binaries, the image build context, and `SHA256SUMS` are described in
[release-assets.md](release-assets.md).
