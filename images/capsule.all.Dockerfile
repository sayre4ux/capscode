# syntax=docker/dockerfile:1
# CapsCode capsule — MERGED image .
#
# One image holding every supported tool, the tmux keeper, and a headless
# Chromium. Replaces the five per-tool images for persistent boxes: a box is
# created once and the operator switches tools inside it without a rebuild.
#
# Tag is a COMPOSITE over every pinned version in toolchain.lock.toml
# (see scripts/build-images.sh + crates/cli/src/config.rs::capsule_all_image),
# because no single tool's version identifies this image.
#
#   docker build -f images/capsule.all.Dockerfile \
#     --build-arg BASE_IMAGE=capscode/capsule-base:dev \
#     -t capscode/capsule-all:<composite> images/
#
# Layer order is cheapest-invalidated-last: apt packages follow the current
# bookworm index, then the npm tools, which change most often.
ARG BASE_IMAGE=capscode/capsule-base:dev

# Bun runtime for omp — pinned multi-arch index digest, bun 1.3.14.
FROM oven/bun:1-debian@sha256:9dba1a1b43ce28c9d7931bfc4eb00feb63b0114720a0277a8f939ae4dfc9db6f AS bun

FROM ${BASE_IMAGE}

# --- system layer -------------------------------------------------------------
# tmux      — the keeper . tini is NOT installed: `docker --init`
#             supplies it as PID 1 for free.
# chromium  — headless browser so the agent can verify its own web work
#             . Debian's build was measured driving
#             playwright-core correctly, so no out-of-band binary download is
#             needed. Its apt version follows the current bookworm index.
# fonts-*   — without these a screenshot renders boxes instead of glyphs, and a
#             `chromium --version` check would never catch it .
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
 && apt-get install -y --no-install-recommends \
      tmux \
      chromium \
      fonts-liberation \
      fonts-noto-color-emoji \
 && rm -rf /var/lib/apt/lists/*

# --- keeper -------------------------------------------------------------------
# The tmux server runs as `agent`, the same uid as the tool.
#
# The design called for a separate `keeper` uid so a compromised agent could not
# `kill-server` or `send-keys` into the operator's pane. LIVE TESTING SHOWED THAT
# IS NOT ACHIEVABLE HERE: tmux spawns every pane as the SERVER's uid, so a
# keeper-owned server can only produce keeper-owned panes — but the tool must run
# as `agent`, which owns /workspace. Spawning a pane under a different uid needs
# CAP_SETUID, which the unprivileged keeper does not have and must not be given.
#
# Residual, accepted: the agent shares a uid with the server, so it can
# `kill-server` (self-inflicted container death — it destroys its own session,
# gaining nothing) and can drive its own pane (which it already controls, being
# the process rendering into it). The config file remains root-owned and is the
# only config the server reads, so an agent-authored ~/.tmux.conf still cannot
# execute when the operator attaches.
COPY tmux.conf /etc/capscode/tmux.conf
# The keeper wrapper: forwards SIGTERM to pane processes before stopping the
# server, because tmux does not (measured — see images/keeper.sh).
COPY keeper.sh /usr/local/bin/capscode-keeper
COPY tool-launcher.sh /usr/local/bin/capscode-tool-launcher
COPY tool-shim.sh /usr/local/bin/capscode-tool-shim
RUN chmod 0755 /usr/local/bin/capscode-keeper /usr/local/bin/capscode-tool-launcher /usr/local/bin/capscode-tool-shim

# --- bun (omp's required runtime) ---------------------------------------------
COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun

# --- tool layers --------------------------------------------------------------
# Versions are injected from toolchain.lock.toml; defaults kept in lockstep so a
# documented direct build stays reproducible.
ARG CLAUDE_VERSION=2.1.285
ARG CODEX_VERSION=0.159.3
ARG OPENCODE_VERSION=1.18.3
ARG PI_VERSION=0.99.2
ARG OMP_VERSION=18.4.6
ARG CLAUDE_INTEGRITY
ARG CODEX_INTEGRITY
ARG OPENCODE_INTEGRITY
ARG PI_INTEGRITY
ARG OMP_INTEGRITY
ARG PLAYWRIGHT_CORE_VERSION=1.56.0
ARG PLAYWRIGHT_CORE_INTEGRITY
ARG CLAUDE_BEFORE
ARG CODEX_BEFORE
ARG OPENCODE_BEFORE
ARG PI_BEFORE
ARG OMP_BEFORE
ARG PLAYWRIGHT_CORE_BEFORE

# Each registry tarball is checked against the lock before it is installed, and
# its dependencies resolve as of the lock's resolved_before time. Layers are
# ordered from least to most frequently changed so a tool bump invalidates only
# itself and the layers after it.
# claude/codex/opencode keep lifecycle scripts enabled because they select their
# platform binaries; pi/omp/playwright-core install with scripts disabled.
# playwright-core drives system chromium via executablePath and must not fetch a
# separate browser.
ENV PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
COPY install-locked-npm.sh /usr/local/bin/capscode-install-locked-npm
RUN chmod 0755 /usr/local/bin/capscode-install-locked-npm

RUN /usr/local/bin/capscode-install-locked-npm playwright-core "$PLAYWRIGHT_CORE_VERSION" "$PLAYWRIGHT_CORE_INTEGRITY" "$PLAYWRIGHT_CORE_BEFORE" ignore
RUN /usr/local/bin/capscode-install-locked-npm opencode-ai "$OPENCODE_VERSION" "$OPENCODE_INTEGRITY" "$OPENCODE_BEFORE" enabled
RUN /usr/local/bin/capscode-install-locked-npm @earendil-works/pi-coding-agent "$PI_VERSION" "$PI_INTEGRITY" "$PI_BEFORE" ignore
# Keep omp's optional ONNX runtime removal in the layer that installs omp.
RUN /usr/local/bin/capscode-install-locked-npm @oh-my-pi/pi-coding-agent "$OMP_VERSION" "$OMP_INTEGRITY" "$OMP_BEFORE" ignore \
 && cd "$(npm root -g)/@oh-my-pi/pi-coding-agent/node_modules" \
 && rm -rf @huggingface onnxruntime-common onnxruntime-node onnxruntime-web \
           sharp sherpa-onnx-node sherpa-onnx-*
RUN /usr/local/bin/capscode-install-locked-npm @anthropic-ai/claude-code "$CLAUDE_VERSION" "$CLAUDE_INTEGRITY" "$CLAUDE_BEFORE" enabled
RUN /usr/local/bin/capscode-install-locked-npm @openai/codex "$CODEX_VERSION" "$CODEX_INTEGRITY" "$CODEX_BEFORE" enabled

# Global modules are not on Node's default resolution path for scripts run from
# /workspace; the agent's Playwright scripts need this.
ENV NODE_PATH=/usr/local/lib/node_modules
ENV DISABLE_AUTOUPDATER=1

# Resolve each installed executable before replacing its PATH entry with a
# wrapper. The wrapper calls this absolute target, so it cannot recurse.
RUN set -eu; \
    for tool in claude codex opencode pi omp; do \
      case "$tool" in \
        claude) route_key=ANTHROPIC_BASE_URL ;; \
        codex) route_key=OPENAI_BASE_URL ;; \
        opencode) route_key=CAPSCODE_OPENCODE_API_KEY ;; \
        pi) route_key=CAPSCODE_PI_API_KEY ;; \
        omp) route_key=CAPSCODE_OMP_API_KEY ;; \
      esac; \
      real_binary="$(readlink -f "/usr/local/bin/$tool")"; \
      case "$real_binary" in /*) ;; *) echo "capsule.all: could not resolve $tool" >&2; exit 1 ;; esac; \
      /usr/local/bin/capscode-tool-shim "$tool" "$route_key" "$real_binary"; \
    done

# --- sanity: every tool resolves, or the build fails --------------------------
RUN for bin in claude codex opencode pi omp tmux chromium bun; do \
      command -v "$bin" >/dev/null 2>&1 || { echo "capsule.all: $bin not on PATH" >&2; exit 1; }; \
    done \
 && node -e "require('playwright-core')" \
 && test -r /etc/capscode/tmux.conf \
 && test -x /usr/local/bin/capscode-keeper \
 && test -x /usr/local/bin/capscode-tool-launcher \
 && test -x /usr/local/bin/capscode-tool-shim
