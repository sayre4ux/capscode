#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TEMP=$(mktemp -d "${TMPDIR:-/tmp}/capscode-channel-merge-test.XXXXXX")
trap 'rm -rf -- "$TEMP"' EXIT
cat >"$TEMP/current.json" <<'JSON'
{"schema":1,"generated_at":"2026-10-18T01:00:00Z","cli":{"version":"1.0.0","tag":"v1.0.0"},"toolchain":{"sequence":7,"digest":"old"}}
JSON
cat >"$TEMP/prepared.json" <<'JSON'
{"schema":1,"generated_at":"2026-10-18T02:00:00Z","cli":{"version":"2.0.0","tag":"v2.0.0"},"toolchain":{"sequence":8,"digest":"new"}}
JSON

"$ROOT/scripts/release/merge-channel-section.sh" --section cli --manifest "$TEMP/prepared.json" \
  <"$TEMP/current.json" >"$TEMP/cli.json"
jq -e '.generated_at == "2026-10-18T01:00:00Z" and .cli.version == "2.0.0" and .toolchain.sequence == 7' \
  "$TEMP/cli.json" >/dev/null

"$ROOT/scripts/release/merge-channel-section.sh" --section toolchain --manifest "$TEMP/prepared.json" \
  <"$TEMP/current.json" >"$TEMP/toolchain.json"
jq -e '.generated_at == "2026-10-18T01:00:00Z" and .cli.version == "1.0.0" and .toolchain.sequence == 8' \
  "$TEMP/toolchain.json" >/dev/null

printf '%s\n' 'PASS channel publisher merges only its own section'
