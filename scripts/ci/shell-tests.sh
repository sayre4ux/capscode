#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TEST_BASH=${BASH:-bash}

for test_script in \
  "$ROOT/scripts/test-smoke-tui.sh" \
  "$ROOT/scripts/test-box-tool-shims.sh" \
  "$ROOT/scripts/test-bump-tool.sh" \
  "$ROOT/scripts/test-build-images-lock.sh" \
  "$ROOT/scripts/test-install.sh" \
  "$ROOT/scripts/ci/test-required-bump-checks.sh"; do
  "$TEST_BASH" "$test_script"
done

for test_script in "$ROOT"/scripts/release/test-*.sh; do
  "$TEST_BASH" "$test_script"
done

sh "$ROOT/images/entrypoint_test.sh"
sh "$ROOT/images/keeper_test.sh"
