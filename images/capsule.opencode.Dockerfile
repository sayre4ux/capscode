# syntax=docker/dockerfile:1
# CapsCode capsule — opencode layer . Pinned opencode CLI. Version
# ARG MUST match toolchain.lock.toml [tools].opencode.
#
#   docker build -f images/capsule.opencode.Dockerfile \
#     --build-arg BASE_IMAGE=capscode/capsule-base:dev \
#     -t capscode/capsule-opencode:dev images/
ARG BASE_IMAGE=capscode/capsule-base:dev
FROM ${BASE_IMAGE}

ARG OPENCODE_VERSION=1.18.3
ARG OPENCODE_INTEGRITY
ARG OPENCODE_BEFORE

RUN set -eu; \
    [ -n "${OPENCODE_INTEGRITY:-}" ] || { echo "capsule.opencode: missing OPENCODE_INTEGRITY build arg" >&2; exit 1; }; \
    [ -n "${OPENCODE_BEFORE:-}" ] || { echo "capsule.opencode: missing OPENCODE_BEFORE build arg" >&2; exit 1; }; \
    PACKED="$(npm pack --silent --ignore-scripts --pack-destination /tmp "opencode-ai@${OPENCODE_VERSION}")"; \
    TARBALL="/tmp/$PACKED"; \
    ACTUAL="sha512-$(node -e 'const fs=require("fs"), crypto=require("crypto"); process.stdout.write(crypto.createHash("sha512").update(fs.readFileSync(process.argv[1])).digest("base64"))' "$TARBALL")"; \
    [ "$ACTUAL" = "$OPENCODE_INTEGRITY" ] || { echo "capsule.opencode: integrity mismatch for opencode-ai@${OPENCODE_VERSION}" >&2; exit 1; }; \
    npm install -g --no-fund --no-audit --before="$OPENCODE_BEFORE" "$TARBALL"; \
    rm -f "$TARBALL"; \
    npm cache clean --force; \
    command -v opencode
