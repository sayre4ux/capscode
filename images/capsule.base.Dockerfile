# syntax=docker/dockerfile:1
# CapsCode capsule BASE image .
#
# The uniform environment every AI tool runs in. Two users on the same CapsCode
# version get a byte-identical environment ("normalize don't randomize").
# Runs untrusted AI code — but only ever as the unprivileged `agent` user, which
# images/entrypoint.sh drops to with setpriv after normalising identity. This
# image NEVER setuids the tool and adds NO capabilities; it is built to run under
# read_only_rootfs=true with tmpfs for /home/agent, /tmp and the /workspace
# volume supplied at run time.
#
# Build context: images/
#   docker build -f images/capsule.base.Dockerfile -t capscode/capsule-base:dev images/
#
# Node.js placement (justification): claude, codex and opencode are all
# npm-distributed Node CLIs, so a single pinned Node LTS lives HERE, in the base.
# That gives one uniform runtime for the identity-normalisation guarantee and
# keeps every tool layer thin (`npm i -g <pkg>@<pinned>`). A tool that needs a
# different runtime (e.g. `pi`, whose upstream is unconfirmed — see
# toolchain.lock.toml) adds it in its own layer without perturbing this base.

# --- Node LTS source, pinned BY DIGEST (see toolchain.lock.toml [base].node) ---
FROM node:22-bookworm-slim@sha256:6c74791e557ce11fc957704f6d4fe134a7bc8d6f5ca4403205b2966bd488f6b3 AS node

# --- debian:bookworm-slim pinned BY DIGEST (see toolchain.lock.toml [base]) -----
FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818

# git + rsync drive the git ingress/egress engine; util-linux provides `setpriv`
# (entrypoint privilege drop); ca-certificates anchors the system trust store the
# entrypoint appends the per-session CA to. procps provides `ps`, which Codex's
# managed app-server daemon runs to record its own pid at startup; without it
# interactive codex exits. No recommends → smaller, deterministic.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
 && apt-get install -y --no-install-recommends \
      git \
      rsync \
      ca-certificates \
      ripgrep \
      fd-find \
      util-linux \
      procps \
 && rm -rf /var/lib/apt/lists/* \
 && ps -p 1 >/dev/null

# Debian calls fd-find's executable `fdfind`; tools expect the usual `fd` name.
RUN ln -s /usr/bin/fdfind /usr/local/bin/fd

# Bring the pinned Node LTS across from the digest-pinned node stage (ABI-safe:
# node:22-bookworm-slim is itself debian bookworm). Re-create the npm/npx shims.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -sf ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -sf ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && node --version && npm --version

# Non-root agent user with a fixed uid/gid for reproducibility. HOME is a tmpfs
# at run time; this just anchors ownership and the path the entrypoint expects.
RUN groupadd --gid 10001 agent \
 && useradd --uid 10001 --gid 10001 --create-home --home-dir /home/agent --shell /bin/sh agent \
 && install -d -m 0755 -o agent -g agent /workspace \
 && install -d -o 10001 -g 10001 -m 0755 /cache

# Strip the setuid/setgid bits off the stock Debian
# binaries (su, mount, passwd, newgrp, ...). This capsule runs untrusted AI code
# as `agent`; none of these need to be setuid here, and removing the bits shrinks
# the local privilege-escalation surface. The image sets NO setuid bit of its own.
RUN find / -xdev -perm /6000 -type f -exec chmod ug-s {} + 2>/dev/null || true

# Baked identity normalisation . Runtime flags reassert these; the
# entrypoint fails closed if they are ever wrong.
ENV TZ=UTC \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    HOME=/home/agent \
    HISTFILE=/dev/null \
    NODE_OPTIONS=--no-deprecation \
    npm_config_update_notifier=false \
    npm_config_fund=false

WORKDIR /workspace

# Root at start (entrypoint installs the CA / pins resolv.conf, then `setpriv`s
# down to `agent`). NO `USER agent` here, and NO setuid bit anywhere — privilege
# drop is done explicitly by the entrypoint, not by the image.
COPY entrypoint.sh /usr/local/bin/capscode-entrypoint
COPY codex-package-seed.sh /usr/local/lib/capscode/codex-package-seed.sh
COPY cache-owner.sh /usr/local/lib/capscode/cache-owner.sh
RUN chmod 0755 /usr/local/bin/capscode-entrypoint
ENTRYPOINT ["/usr/local/bin/capscode-entrypoint"]
