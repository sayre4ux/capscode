#!/usr/bin/env bash
set -eu

usage() {
  cat <<'USAGE'
Usage: scripts/release/test-changelog-section.sh

Check changelog extraction and missing-section failure behavior.
USAGE
}

if (($#)); then
  if (($# == 1)) && { [[ "$1" == --help || "$1" == -h ]]; }; then usage; exit 0; fi
  usage >&2
  exit 2
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
extractor="$root/scripts/release/changelog-section.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/capscode-changelog-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

"$extractor" 0.1.0 >"$tmp/section.md"
grep -E '^## \[0\.1\.0\] - ' "$tmp/section.md" >/dev/null
grep -Fx 'First public release.' "$tmp/section.md" >/dev/null
if grep -F '## [' "$tmp/section.md" | grep -vF '## [0.1.0]' >/dev/null; then
  printf '%s\n' 'Changelog output crossed into another release section' >&2
  exit 1
fi

if "$extractor" 999.0.0 >"$tmp/missing.out" 2>"$tmp/missing.err"; then
  printf '%s\n' 'Expected missing changelog version to fail' >&2
  exit 1
fi
grep -F 'No CHANGELOG.md section found for 999.0.0' "$tmp/missing.err" >/dev/null
printf '%s\n' 'PASS changelog extraction and missing-section refusal'
