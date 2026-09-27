# syntax=docker/dockerfile:1
# CapsCode capsule — codex layer . Pinned OpenAI Codex CLI. Version
# ARG MUST match toolchain.lock.toml [tools].codex. Kimi can also reuse this
# image with a provider base-URL override — no separate Kimi image.
#
#   docker build -f images/capsule.codex.Dockerfile \
#     --build-arg BASE_IMAGE=capscode/capsule-base:dev \
#     -t capscode/capsule-codex:dev images/
ARG BASE_IMAGE=capscode/capsule-base:dev
FROM ${BASE_IMAGE}

ARG CODEX_VERSION=0.159.3
ARG CODEX_INTEGRITY
ARG CODEX_BEFORE

RUN set -eu; \
    [ -n "${CODEX_INTEGRITY:-}" ] || { echo "capsule.codex: missing CODEX_INTEGRITY build arg" >&2; exit 1; }; \
    [ -n "${CODEX_BEFORE:-}" ] || { echo "capsule.codex: missing CODEX_BEFORE build arg" >&2; exit 1; }; \
    PACKED="$(npm pack --silent --ignore-scripts --pack-destination /tmp "@openai/codex@${CODEX_VERSION}")"; \
    TARBALL="/tmp/$PACKED"; \
    ACTUAL="sha512-$(node -e 'const fs=require("fs"), crypto=require("crypto"); process.stdout.write(crypto.createHash("sha512").update(fs.readFileSync(process.argv[1])).digest("base64"))' "$TARBALL")"; \
    [ "$ACTUAL" = "$CODEX_INTEGRITY" ] || { echo "capsule.codex: integrity mismatch for @openai/codex@${CODEX_VERSION}" >&2; exit 1; }; \
    npm install -g --no-fund --no-audit --before="$CODEX_BEFORE" "$TARBALL"; \
    rm -f "$TARBALL"; \
    npm cache clean --force; \
    command -v codex
