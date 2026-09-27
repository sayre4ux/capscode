# syntax=docker/dockerfile:1
# CapsCode capsule — claude layer . Thin layer on the base: install
# the pinned Claude Code CLI. Version is pinned via ARG and MUST match
# toolchain.lock.toml [tools].claude. Kimi reuses THIS image with a provider
# base-URL override (see capsule.pi.Dockerfile note) — no separate Kimi image.
#
#   docker build -f images/capsule.claude.Dockerfile \
#     --build-arg BASE_IMAGE=capscode/capsule-base:dev \
#     -t capscode/capsule-claude:dev images/
ARG BASE_IMAGE=capscode/capsule-base:dev
FROM ${BASE_IMAGE}

# Pinned Claude Code version (keep in lockstep with toolchain.lock.toml).
ARG CLAUDE_VERSION=2.1.285
ARG CLAUDE_INTEGRITY
ARG CLAUDE_BEFORE

# Installed at build time as root into the world-readable global prefix; the tool
# still RUNS as the unprivileged `agent` user via the entrypoint. Lifecycle
# scripts stay ENABLED: the CLI's postinstall selects its platform-native binary,
# without which `claude` will not start. Verify the registry tarball before install.
RUN set -eu; \
    [ -n "${CLAUDE_INTEGRITY:-}" ] || { echo "capsule.claude: missing CLAUDE_INTEGRITY build arg" >&2; exit 1; }; \
    [ -n "${CLAUDE_BEFORE:-}" ] || { echo "capsule.claude: missing CLAUDE_BEFORE build arg" >&2; exit 1; }; \
    PACKED="$(npm pack --silent --ignore-scripts --pack-destination /tmp "@anthropic-ai/claude-code@${CLAUDE_VERSION}")"; \
    TARBALL="/tmp/$PACKED"; \
    ACTUAL="sha512-$(node -e 'const fs=require("fs"), crypto=require("crypto"); process.stdout.write(crypto.createHash("sha512").update(fs.readFileSync(process.argv[1])).digest("base64"))' "$TARBALL")"; \
    [ "$ACTUAL" = "$CLAUDE_INTEGRITY" ] || { echo "capsule.claude: integrity mismatch for @anthropic-ai/claude-code@${CLAUDE_VERSION}" >&2; exit 1; }; \
    npm install -g --no-fund --no-audit --before="$CLAUDE_BEFORE" "$TARBALL"; \
    rm -f "$TARBALL"; \
    npm cache clean --force; \
    command -v claude
