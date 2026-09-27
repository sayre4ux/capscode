# syntax=docker/dockerfile:1
# CapsCode capsule — pi layer .
#
# Pi is "a minimal agent harness" AI coding CLI (https://pi.dev). It is published
# to npm as `@earendil-works/pi-coding-agent`. Install with `--ignore-scripts`
# (pi's own recommended flag) so no postinstall script runs inside the capsule —
# consistent with CapsCode's "nothing untrusted executes at build time" posture.
#
# NOTE: `pi` (the bare npm name) is an UNRELATED joke package — always use the
# scoped `@earendil-works/pi-coding-agent`. Pin the version via toolchain.lock.toml.
ARG BASE_IMAGE=capscode/capsule-base:dev
FROM ${BASE_IMAGE}

ARG PI_NPM_PACKAGE=@earendil-works/pi-coding-agent
# Version is injected from toolchain.lock.toml; keep the default aligned so a
# direct documented image build remains reproducible too.
ARG PI_VERSION=0.99.2
ARG PI_INTEGRITY
ARG PI_BEFORE

RUN set -eu; \
    [ -n "${PI_INTEGRITY:-}" ] || { echo "capsule.pi: missing PI_INTEGRITY build arg" >&2; exit 1; }; \
    [ -n "${PI_BEFORE:-}" ] || { echo "capsule.pi: missing PI_BEFORE build arg" >&2; exit 1; }; \
    PACKED="$(npm pack --silent --ignore-scripts --pack-destination /tmp "${PI_NPM_PACKAGE}@${PI_VERSION}")"; \
    TARBALL="/tmp/$PACKED"; \
    ACTUAL="sha512-$(node -e 'const fs=require("fs"), crypto=require("crypto"); process.stdout.write(crypto.createHash("sha512").update(fs.readFileSync(process.argv[1])).digest("base64"))' "$TARBALL")"; \
    [ "$ACTUAL" = "$PI_INTEGRITY" ] || { echo "capsule.pi: integrity mismatch for ${PI_NPM_PACKAGE}@${PI_VERSION}" >&2; exit 1; }; \
    npm install -g --no-fund --no-audit --ignore-scripts --before="$PI_BEFORE" "$TARBALL"; \
    rm -f "$TARBALL"; \
    npm cache clean --force

# Sanity: the CLI resolves on PATH.
RUN command -v pi >/dev/null 2>&1 || command -v pi-coding-agent >/dev/null 2>&1 \
    || { echo "capsule.pi: pi CLI not on PATH after install" >&2; exit 1; }
