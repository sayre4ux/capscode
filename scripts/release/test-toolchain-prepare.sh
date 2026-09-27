#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TEMP=$(mktemp -d "${TMPDIR:-/tmp}/capscode-toolchain-prepare-test.XXXXXX")
trap 'rm -rf -- "$TEMP"' EXIT
LOCK="$TEMP/toolchain.lock.toml"
cp "$ROOT/images/toolchain.lock.toml" "$LOCK"
previous=$(git -C "$ROOT" tag --list 'toolchain-*' | sed -n 's/^toolchain-\([0-9][0-9]*\)$/\1/p' | sort -n | tail -1)
previous=${previous:-0}

set_sequence() {
  LOCK_SEQUENCE=$1 perl -0pi -e 's/^sequence\s*=\s*\d+/sequence = $ENV{LOCK_SEQUENCE}/m' "$LOCK"
}

set_sequence "$previous"
cp "$LOCK" "$TEMP/before.lock.toml"
if "$ROOT/scripts/release/toolchain-release.sh" prepare --lock "$LOCK" --out-dir "$TEMP/out" \
  --digest 0123456789ab --dry-run >"$TEMP/same.out" 2>"$TEMP/same.err"; then
  printf '%s\n' 'Accepted a toolchain sequence that was already published' >&2
  exit 1
fi
grep -Fq "must be greater than the latest release ($previous)" "$TEMP/same.err"
cmp -s "$LOCK" "$TEMP/before.lock.toml"

set_sequence "$((previous + 2))"
cp "$LOCK" "$TEMP/advanced.lock.toml"
"$ROOT/scripts/release/toolchain-release.sh" prepare --lock "$LOCK" --out-dir "$TEMP/out" \
  --digest 0123456789ab --dry-run >"$TEMP/advanced.out"
grep -Fq "DRY-RUN: prepare toolchain-$((previous + 2))" "$TEMP/advanced.out"
cmp -s "$LOCK" "$TEMP/advanced.lock.toml"

printf '%s\n' 'PASS toolchain release requires a reviewed sequence and leaves its lock unchanged'
