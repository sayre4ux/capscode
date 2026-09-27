# syntax=docker/dockerfile:1
# CapsCode capsule — omp layer .
#
# omp is the coding agent at https://omp.sh (repo can1357/oh-my-pi), published to
# npm as `@oh-my-pi/pi-coding-agent` (bin name `omp`). It is a different lineage
# from CapsCode's existing `pi` layer (`@earendil-works/pi-coding-agent`) — same
# conceptual niche, unrelated package and config layout (`~/.omp/agent/`).
#
# Bun is required: dist/cli.js has `#!/usr/bin/env bun`, the bundle calls
# `Bun.hash` / `import.meta.require`, and package.json declares
# `engines: { bun: ">=1.3.14" }`. Node cannot run it. npm is still the installer;
# we multi-stage-copy a pinned bun binary into the capsule base and install with
# `--ignore-scripts` (platform natives resolve without postinstall).
#
# The install then prunes ~600 MB of on-device ML a capsule can never reach:
# `@huggingface/transformers` (+ `onnxruntime-node`/`-web`/`-common`, `sharp`,
# `jinja`, `tokenizers`) and `sherpa-onnx-*` back omp's tiny-model titles/mnemopi,
# Whisper STT and Kokoro TTS. All of it needs model weights fetched from
# huggingface.co — which the kill switch drops — and the speech paths need a
# mic/speaker the capsule has no device for. They form a closed dependency island:
# nothing else in the tree references them, and omp declares them optional.
#
# The prune is by name, not `--omit=optional`: npm ignores `--omit` for `-g` root
# specs, and omitting optionals at all would also drop `@oh-my-pi/pi-natives-<plat>`,
# the platform native addon omp cannot start without.
ARG BASE_IMAGE=capscode/capsule-base:dev

# Pinned multi-arch index digest (linux/amd64 + linux/arm64); bun 1.3.14.
FROM oven/bun:1-debian@sha256:9dba1a1b43ce28c9d7931bfc4eb00feb63b0114720a0277a8f939ae4dfc9db6f AS bun

FROM ${BASE_IMAGE}

COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun

ARG OMP_NPM_PACKAGE=@oh-my-pi/pi-coding-agent
# Version is injected from toolchain.lock.toml; keep the default aligned so a
# direct documented image build remains reproducible too.
ARG OMP_VERSION=18.4.6
ARG OMP_INTEGRITY
ARG OMP_BEFORE

RUN set -eu; \
    [ -n "${OMP_INTEGRITY:-}" ] || { echo "capsule.omp: missing OMP_INTEGRITY build arg" >&2; exit 1; }; \
    [ -n "${OMP_BEFORE:-}" ] || { echo "capsule.omp: missing OMP_BEFORE build arg" >&2; exit 1; }; \
    PACKED="$(npm pack --silent --ignore-scripts --pack-destination /tmp "${OMP_NPM_PACKAGE}@${OMP_VERSION}")"; \
    TARBALL="/tmp/$PACKED"; \
    ACTUAL="sha512-$(node -e 'const fs=require("fs"), crypto=require("crypto"); process.stdout.write(crypto.createHash("sha512").update(fs.readFileSync(process.argv[1])).digest("base64"))' "$TARBALL")"; \
    [ "$ACTUAL" = "$OMP_INTEGRITY" ] || { echo "capsule.omp: integrity mismatch for ${OMP_NPM_PACKAGE}@${OMP_VERSION}" >&2; exit 1; }; \
    npm install -g --no-fund --no-audit --ignore-scripts --before="$OMP_BEFORE" "$TARBALL"; \
    rm -f "$TARBALL"; \
    npm cache clean --force; \
    cd "$(npm root -g)/${OMP_NPM_PACKAGE}/node_modules"; \
    rm -rf @huggingface onnxruntime-common onnxruntime-node onnxruntime-web \
           sharp sherpa-onnx-node sherpa-onnx-*

# Sanity: the CLI resolves on PATH.
RUN command -v omp >/dev/null 2>&1 \
    || { echo "capsule.omp: omp CLI not on PATH after install" >&2; exit 1; }
