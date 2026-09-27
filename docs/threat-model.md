# Threat model

## System and assets

CapsCode runs an AI coding tool in a Docker capsule. The capsule shares the gateway container's
network namespace; the gateway hosts the credential broker and network rules. The host CLI
creates and configures both containers, passes selected credentials to the gateway, and
collects work from the capsule.

Assets include:

- Host files, repositories, local configuration, and credentials.
- Provider API keys and OAuth tokens held by the gateway.
- The integrity of the host working tree and the work returned from a capsule.
- Network identity, including the public egress IP and DNS resolver in use.
- Provider requests and code sent to model providers.

## Adversaries

- **A hostile agent in the capsule** can execute arbitrary code as the unprivileged capsule
  user, inspect files made available to the tool, and attempt network connections or to alter
  the returned work.
- **A malicious repository** can contain hostile source, build instructions, configuration,
  and files that could become hooks or other host-side runners if copied back carelessly.
- **A malicious VPN configuration** could try to make the privileged gateway execute commands
  or weaken its network rules. Import accepts only allowlisted WireGuard configuration keys;
  `PreUp`, `PostUp`, `PreDown`, and `PostDown` hooks are rejected, and DNS entries must be IP
  addresses.
- **A malicious provider registry entry** is host-side configuration that can select an
  upstream authority for a provider. Treat custom registries as trusted input: an entry can
  cause the selected provider credential to be sent to its configured endpoint.
- **Another local process** runs outside CapsCode's container boundary. CapsCode does not
  isolate the user from processes that already have access to the user's files, keychain, or
  Docker daemon.

## Trust boundaries and controls

- The capsule runs as an unprivileged user with a read-only root filesystem and does not receive
  real provider credentials. In copy mode it receives a workspace snapshot. Mount mode exposes
  the host project read-write and therefore gives up that workspace boundary.
- `/home/agent` is a `noexec` tmpfs; `/tmp` and `/workspace` are executable. This does not form a
  boundary against the agent, which can already execute code from `/workspace`. It constrains tool
  self-install paths under home, so the Codex daemon is linked from the pinned image into
  `~/.codex/packages`. One-shot npm, Bun, and XDG caches use `/tmp/.cache`; box caches use their
  per-box `/cache` volume, where runtime-downloaded helpers can execute.
- A box also has a per-box cache volume at `/cache` for helpers the tools fetch at run time
  (language servers, `npx` MCP servers, package caches). Like the workspace it is agent-writable,
  executable and survives stop/resume; unlike the workspace it is never copied back to the host
  and is deleted with the box. Resume resets the tool home and `/tmp`; it does not reset the
  workspace or this cache. Caches are per box and never shared between projects.
- The gateway is privileged and holds the real credentials. Secrets are sent from the host to
  gateway memory/tmpfs; the capsule receives only sentinel values. The gateway broker accepts
  configured provider routes and replaces credential values. It forwards the tool's own
  User-Agent and other request headers unchanged, so upstream requests look like that tool's
  normal traffic. The provider receives the real credential and request/code content.
- The capsule can reach the broker over the shared network namespace. The broker's provider
  routes are default-deny; the network kill switch separately controls direct and forwarded
  egress. Container isolation does not hide the host's public IP from public destinations in
  containment-only mode.
- Copy-back is a separate host-side boundary. Git-mode changes that touch hook or runner files
  are saved on a `capscode/<id>` branch instead of being applied to the worktree. Applying a
  Git patch to the host relies on the host's Git implementation. Mount mode writes directly to
  the host project while the session is running.
- **Boxes.** Routing is chosen when a box is created or resumed. Every provider routed into a
  box is reachable by the agent for the box's life, so the owner is responsible for watching
  usage and spend. Real keys remain in the gateway; the capsule receives only sentinels. A box
  keeps its selected toolchain until the owner switches it. Keeping an older toolchain also keeps
  its known vulnerabilities and misses later security fixes.
- Docker daemon access is controlled by the host. A box has the privacy of that Docker socket:
  any local process or user allowed to `docker exec` into it can access its tmux session.
- **Resource exhaustion.** Every CapsCode container has a process and thread cap (Docker
  `PidsLimit`): 1024 by default, and `capsule.pids` changes it for the capsule. A fork bomb in
  the capsule stops at that cap. Memory is limited only when `capsule.memory` is set; swap is
  then capped to the same value, and tmpfs files written in the capsule count toward it. The
  tmpfs mounts also have their own size limits.

- **Update channel.** The CLI fetches `channel.json` and toolchain locks from the GitHub
  repository named by `update.channel` over HTTPS (Mozilla root store, redirects only to GitHub
  hosts, size caps). There is no maintainer signing key: the trust model is TLS plus client
  policy. A lock is applied only if its SHA-256 matches the manifest, its sequence is newer than
  any this machine accepted, it does not need a newer CLI, it is not withdrawn, every package and
  base-image repository is in the CLI's compiled-in allowlist, and every value has the expected
  shape before it reaches a Dockerfile `ARG` or shell; a tool downgrade also needs a recorded
  reason and the user's confirmation. Toolchain releases carry GitHub artifact attestations;
  verifying them is opt-in (`update.verify_with_gh`, which runs `gh attestation verify` and
  fails closed) or manual.

  Someone who controls the GitHub repository, its release workflow, or the `channel` branch can
  publish a lock this client accepts. Such a lock can select any published version of an
  allowlisted npm package, including an old or freshly compromised one, any digest under the
  allowlisted Docker Hub repositories (`debian`, `node`, `oven/bun`, `alpine`), and any
  `resolved_before` time. It cannot name another package or repository, change the bytes npm or
  Docker Hub serve (the image build checks the tarball hash and pins the base digest), replay an
  older sequence, roll a tool back without the user confirming, or run anything on the host: the
  lock is data, and the Dockerfiles and build scripts come from the CLI checkout. The exposure is
  the same as a compromised upstream package. The stronger link is the CLI itself: whoever
  controls the repository controls the code a `git pull && ./scripts/install.sh` builds and runs
  on the host, and the release binary the installer and `capscode update --cli` download.

- **CLI binary updates.** `install.sh` (binary install) and `capscode update --cli` install only a
  plain `vX.Y.Z` release named by the channel (or requested by version), and only after the
  binary and the image build context match the release's `SHA256SUMS`; with
  `--verify-attestation` or `update.verify_with_gh` they also require a GitHub attestation from
  `release-cli.yml`. The checksum file comes from the same release over the same TLS
  connection, so it protects against corruption and a tampered mirror or CDN object, not
  against someone who can publish a release; the attestation ties the files to the repository's
  release workflow, which is the same trust as the source. The build context is unpacked as
  untrusted input (no links, devices, absolute paths, or `..`), into the CapsCode home, and its
  scripts run only when the user builds images. The binary is replaced by renaming a synced
  temporary file after the new binary has run `--version`, so a failed update leaves the old
  binary. The macOS binary is ad-hoc signed without an Apple Developer ID: its signature shows
  integrity only, and the keychain asks once for access after each update.

## Allowed egress by mode

| Flow | Containment-only (default) | WireGuard tunnel |
|---|---|---|
| Capsule to broker | Allowed to the local broker listener. The broker forwards only configured provider routes. | Same. |
| Capsule to public internet | Allowed without destination filtering, apart from explicit blocked ranges and telemetry sinkholes. Egress uses the host's public network path and IP. | Allowed only through `wg0`; traffic not routed through the tunnel is dropped, including packets of connections opened before the route changed. This is routing containment, not destination filtering. |
| DNS | Uses the ambient resolver; DNS is not pinned to a VPN. | Only IP-address resolvers from the validated WireGuard configuration are allowed, over `wg0`; other DNS is dropped. |
| Host, LAN, and metadata addresses | Configured IPv4 private, host-gateway, link-local, CGNAT, and metadata ranges are blocked, for open connections as well as new ones. IPv6 ULA and link-local ranges are blocked. A globally addressed host IPv6 alias is a residual. | The same address blocks apply. Non-tunnel egress is also dropped. |
| Gateway verifier | At session start the gateway connects to `1.1.1.1:443` and fetches `https://1.1.1.1/cdn-cgi/trace` (Cloudflare) to report the public egress IP and country. Informational only; failure never blocks a session. | Same, through `wg0`. |
| IPv6 | ULA (`fc00::/7`) and link-local (`fe80::/10`) destinations are blocked. Other global IPv6 destinations can be reached; `2000::/3` is not comprehensively blocked. | IPv6 egress is allowed only when routed through `wg0`; otherwise the kill switch drops it. |

Host egress by the CLI itself is separate from the capsule's. Besides the Docker daemon and the
image builds the user starts, the only request CapsCode makes without being asked is the daily
update check: one HTTPS GET of `channel.json` (about 1 KB) from `raw.githubusercontent.com`, at
most once every 24 hours, sending only `User-Agent: capscode/<version>` and no identifiers. It
runs only for commands in a terminal and is off with `update.check = false` or
`CAPSCODE_NO_UPDATE_CHECK=1`. GitHub sees the host's IP address and the CLI version. `capscode
update` adds the lock download from GitHub release storage, and `capscode update --cli` the
release's `SHA256SUMS`, notes, binary, and build context.

Neither mode filters public destinations for data exfiltration. Containment-only deliberately
allows public egress, and a WireGuard tunnel changes the route without hiding the user's
provider account or filtering what the agent sends to a public endpoint.

## Known residual risks

Tool versions normally wait three days after publication before `bump-tool.sh` selects a
dist-tag release, and dependency resolution uses the same three-day cutoff. A maintainer can
record a cooldown waiver with its reason. The SHA-512 tarball integrity comes from the same npm
registry that serves the selected package version, so it detects a changed tarball but is not an
independent source for the expected hash. Maintainers or the reviewed update bot can author lock
changes; the lock and image build still require review and a passing smoke before they become the
compiled-in default. A box kept on an older snapshot does not receive tool updates until it is
switched.

- Image builds fetch from Docker Hub, Debian, and npm. Base images are pinned by digest and tool
  tarballs by hash; tool dependencies resolve as of each pin's `resolved_before` time, and
  Debian packages float. A package that was already malicious when a pin was set runs as root
  with network access during the image build, where it could alter the image, and later as the
  agent inside the capsule, where the capsule's controls apply. CapsCode ships no prebuilt
  images and cannot guarantee that upstream packages are uncompromised.

- Without an imported WireGuard configuration, public traffic uses the host's egress IP and
  ambient DNS. Host-origin privacy is opt-in.
- Host-side Git patch application depends on the host Git implementation. CapsCode checks the
  patch before applying it, but a future Git vulnerability could affect the host.
- The gateway broker remains root with `NET_ADMIN` for its lifetime and parses capsule-controlled
  HTTP while the real credentials are in gateway memory.
- IPv6 containment covers ULA and link-local ranges, not all globally addressed host aliases in
  `2000::/3`.
- A box is accessible to anyone with sufficient Docker daemon/socket permissions to execute
  into its container.
- Without `capsule.memory`, the capsule has no memory limit, so a hostile agent can exhaust the
  memory of the host on Linux, or of the shared Docker VM on macOS. CPU time, disk I/O, and
  writes to the workspace volume are not limited in either case.
- Public egress in both modes is not a data-loss-prevention boundary. Provider requests include
  the real credential and code; CapsCode does not anonymize the account or prevent the provider
  from receiving those requests.
- Helper binaries and MCP servers a tool downloads at runtime into `/tmp` (for example, through
  `npx`) are not pinned by the image; they run with the capsule's controls.
- Helper binaries in `/cache` are unpinned and persist until `rm`, `resume --fresh-cache`, or a
  toolchain switch.
