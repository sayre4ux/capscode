#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
toolchain_release="$ROOT/scripts/release/toolchain-release.sh"
channel_publisher="$ROOT/scripts/release/publish-channel.sh"

grep -Fq 'lock sequence must be greater than the latest release' "$toolchain_release"
grep -Fq 'release $tag already exists; existing releases and assets are never overwritten' "$toolchain_release"
grep -Fq 'publish-channel.sh --section toolchain' "$toolchain_release"
if grep -Eq 'git push .*refs/heads/main|gh release upload .*--clobber|set_tool_value ' "$toolchain_release"; then
  printf '%s\n' 'Toolchain release must not push lock changes or rewrite release assets' >&2
  exit 1
fi

for workflow in release-cli.yml release-toolchain.yml; do
  grep -Fq 'group: release-publish-${{ github.repository }}' "$ROOT/.github/workflows/$workflow"
done
grep -Fq 'ref: ${{ github.sha }}' "$ROOT/.github/workflows/release-toolchain.yml"
grep -Fq 'TOOL_INPUT: ${{ inputs.tool || '\''all'\'' }}' "$ROOT/.github/workflows/toolchain-bump.yml"
grep -Fq 'for attempt in 1 2 3' "$channel_publisher"
grep -Fq 'verify-required-bump-checks.sh' "$ROOT/scripts/ci/bot-bump.sh"
grep -Fq 'advances `sequence`' "$ROOT/MAINTAINERS.md"

printf '%s\n' 'PASS release workflows preserve lock and channel publication guards'
