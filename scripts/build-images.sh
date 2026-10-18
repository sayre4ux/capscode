#!/usr/bin/env bash
# Build the CapsCode container images with the exact tags the CLI resolves
# (capscode/gateway:<crate-version>, capscode/capsule-<tool>:<pinned-tool-version>).
# Run from the repo root: ./scripts/build-images.sh (see --help)
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CALLER_DIR=$PWD
cd "$ROOT"
source "$ROOT/scripts/lib/toolchain-lock.sh"
IMAGE_CONTRACT=2

usage() {
  cat <<'USAGE'
Usage: scripts/build-images.sh [--lock <path>] [--no-smoke]

Build the capsule base, tool, merged, and gateway images with the tags the
capscode CLI expects, then smoke each tool's start-up and save the lock as a
toolchain snapshot under the CapsCode home.

--lock <path>  Build this toolchain lock instead of images/toolchain.lock.toml.
--no-smoke     Skip scripts/smoke-tui.sh after each tool image.
-h, --help     Show this help.

Runs from a CapsCode checkout or from a release build context
(~/.capscode/build/<version>); `capscode toolchain build` finds it for you.
USAGE
}

NO_SMOKE=0
LOCK=$ROOT/images/toolchain.lock.toml
while (($#)); do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --no-smoke) NO_SMOKE=1 ;;
    --lock)
      (($# >= 2)) || { echo "--lock requires a path" >&2; exit 2; }
      LOCK=$2
      shift
      ;;
    *) usage >&2; exit 2 ;;
  esac
  shift
done
if [[ "$LOCK" != /* ]]; then LOCK=$CALLER_DIR/$LOCK; fi
LOCK=$(cd "$(dirname "$LOCK")" && pwd)/$(basename "$LOCK")
[ -f "$LOCK" ] || { echo "!! toolchain lock not found: $LOCK" >&2; exit 1; }

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
lock_value() {
  toolchain_lock_value "$LOCK" "$1" "$2"
}
lock_ver() { lock_value "$1" version; }
lock_integrity() { lock_value "$1" integrity; }
lock_before() { lock_value "$1" resolved_before; }
all_tools_digest() {
  toolchain_lock_digest "$LOCK"
}

toolchain_validate_lock "$LOCK"
record_args=(--record)
if [[ "$LOCK" != "$ROOT/images/toolchain.lock.toml" ]]; then record_args=(); fi

echo "==> capsule base"
docker build -f images/capsule.base.Dockerfile \
  --label capscode=1 --label capscode.role=base --label "capscode.contract=$IMAGE_CONTRACT" \
  -t capscode/capsule-base:dev images/

for tool in claude codex opencode pi omp; do
  ver=$(lock_ver "$tool")
  integrity=$(lock_integrity "$tool")
  argname=$(echo "$tool" | tr '[:lower:]' '[:upper:]')_VERSION
  integrity_argname=$(echo "$tool" | tr '[:lower:]' '[:upper:]')_INTEGRITY
  before_argname=$(echo "$tool" | tr '[:lower:]' '[:upper:]')_BEFORE
  echo "==> capsule-$tool:$ver"
  docker build -f "images/capsule.$tool.Dockerfile" \
    --label capscode=1 --label capscode.role=capsule --label "capscode.contract=$IMAGE_CONTRACT" \
    --build-arg BASE_IMAGE=capscode/capsule-base:dev \
    --build-arg "${argname}=${ver}" \
    --build-arg "${integrity_argname}=${integrity}" \
    --build-arg "${before_argname}=$(lock_before "$tool")" \
    -t "capscode/capsule-$tool:$ver" images/
  if [ "$NO_SMOKE" -eq 0 ]; then
    CAPSCODE_TOOLCHAIN_LOCK="$LOCK" scripts/smoke-tui.sh "$tool" "capscode/capsule-$tool:$ver" ${record_args[@]+"${record_args[@]}"}
  fi
done

ALL_TAG=$(all_tools_digest)
echo "==> capsule-all:$ALL_TAG (merged: every tool + tmux + chromium)"
docker build -f images/capsule.all.Dockerfile \
  --label capscode=1 --label capscode.role=capsule --label "capscode.contract=$IMAGE_CONTRACT" \
  --build-arg BASE_IMAGE=capscode/capsule-base:dev \
  --build-arg "CLAUDE_VERSION=$(lock_ver claude)" \
  --build-arg "CLAUDE_INTEGRITY=$(lock_integrity claude)" \
  --build-arg "CLAUDE_BEFORE=$(lock_before claude)" \
  --build-arg "CODEX_VERSION=$(lock_ver codex)" \
  --build-arg "CODEX_INTEGRITY=$(lock_integrity codex)" \
  --build-arg "CODEX_BEFORE=$(lock_before codex)" \
  --build-arg "OPENCODE_VERSION=$(lock_ver opencode)" \
  --build-arg "OPENCODE_INTEGRITY=$(lock_integrity opencode)" \
  --build-arg "OPENCODE_BEFORE=$(lock_before opencode)" \
  --build-arg "PI_VERSION=$(lock_ver pi)" \
  --build-arg "PI_INTEGRITY=$(lock_integrity pi)" \
  --build-arg "PI_BEFORE=$(lock_before pi)" \
  --build-arg "OMP_VERSION=$(lock_ver omp)" \
  --build-arg "OMP_INTEGRITY=$(lock_integrity omp)" \
  --build-arg "OMP_BEFORE=$(lock_before omp)" \
  --build-arg "PLAYWRIGHT_CORE_VERSION=$(lock_ver playwright_core)" \
  --build-arg "PLAYWRIGHT_CORE_INTEGRITY=$(lock_integrity playwright_core)" \
  --build-arg "PLAYWRIGHT_CORE_BEFORE=$(lock_before playwright_core)" \
  -t "capscode/capsule-all:$ALL_TAG" images/

if [ "$NO_SMOKE" -eq 0 ]; then
  CAPSCODE_TOOLCHAIN_LOCK="$LOCK" scripts/smoke-tui.sh all "capscode/capsule-all:$ALL_TAG" ${record_args[@]+"${record_args[@]}"}
  # DECISION: playwright-core is a library, so its CI smoke is the merged image's successful
  # Dockerfile Node require check rather than a separate interactive TUI run.
  case "$(uname -m)" in
    x86_64|amd64) SMOKE_ARCH=amd64 ;;
    aarch64|arm64) SMOKE_ARCH=arm64 ;;
    *) SMOKE_ARCH=$(uname -m) ;;
  esac
  toolchain_record_smoke_for_lock "$LOCK" "$ROOT/images/toolchain.lock.toml" playwright_core "$(date -u +%Y-%m-%d)" "$SMOKE_ARCH"
fi

echo "==> gateway:$VERSION (compiles capscode-broker; context = repo root)"
docker build -f images/gateway.Dockerfile \
  --label capscode=1 --label capscode.role=gateway --label "capscode.contract=$IMAGE_CONTRACT" \
  -t "capscode/gateway:$VERSION" .

# Store the exact successful build input for later rollback and per-box pinning.
if [ -n "${CAPSCODE_HOME:-}" ]; then
  CONFIG_HOME=$CAPSCODE_HOME
elif [ -n "${HOME:-}" ]; then
  CONFIG_HOME=$HOME/.capscode
else
  CONFIG_HOME=$ROOT/.capscode
fi
SNAPSHOT_DIR=$CONFIG_HOME/toolchains
mkdir -p "$SNAPSHOT_DIR"
chmod 700 "$CONFIG_HOME" "$SNAPSHOT_DIR"
SNAPSHOT_TMP=$(mktemp "$SNAPSHOT_DIR/.${ALL_TAG}.XXXXXX")
cp "$LOCK" "$SNAPSHOT_TMP"
chmod 600 "$SNAPSHOT_TMP"
mv -f "$SNAPSHOT_TMP" "$SNAPSHOT_DIR/$ALL_TAG.lock.toml"

echo "==> done. Images:"
# The listing is informational; an empty match must not fail a successful build.
docker images --format '{{.Repository}}:{{.Tag}}' | grep -E '^capscode/(gateway|capsule)' | grep -E ":$VERSION|:dev|:$(lock_ver claude)" || true
