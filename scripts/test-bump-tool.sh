#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TEMP=$(mktemp -d "${TMPDIR:-/tmp}/capscode-bump-test.XXXXXX")
trap 'rm -rf -- "$TEMP"' EXIT
export CAPSCODE_TOOLCHAIN_LOCK="$TEMP/toolchain.lock.toml"
source "$ROOT/scripts/bump-tool.sh"

cat >"$LOCK" <<'TOML'
schema = 1

[tools]
codex = { package = "@openai/codex", version = "1.0.0", integrity = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", resolved_before = "2024-01-01T00:00:00Z", channel = "latest", smoke = "ci", smoke_date = "2024-01-01", smoke_arch = ["amd64"], home_exec_paths = ["/home/agent/.cache/*"] }
opencode = { package = "opencode-ai", version = "1.0.0", integrity = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", resolved_before = "2024-01-01T00:00:00Z", channel = "latest", smoke = "ci", smoke_date = "2024-01-01", smoke_arch = ["amd64"], home_exec_paths = [] }
TOML

now=$(date -u +%s)
node - "$now" >"$TEMP/time.json" <<'NODE'
const now = Number(process.argv[2]);
const iso = seconds => new Date((now - seconds) * 1000).toISOString();
process.stdout.write(JSON.stringify({
  "1.0.0": iso(10 * 86400),
  "1.2.0": iso(4 * 86400),
  "1.3.0": iso(86400)
}));
NODE
printf '["1.0.0","1.2.0","1.3.0"]\n' >"$TEMP/versions.json"

selected=$(select_latest_version "$TEMP/time.json" "$TEMP/versions.json" 1.3.0 3 "$now" 1.0.0)
[[ "$selected" == 1.2.0 ]] || { echo "expected latest eligible version 1.2.0, got $selected" >&2; exit 1; }
if select_latest_version "$TEMP/time.json" "$TEMP/versions.json" 1.3.0 3 "$now" 1.3.0 >"$TEMP/downgrade.out" 2>"$TEMP/downgrade.err"; then
  echo "a candidate below the current pin was selected" >&2
  exit 1
fi
grep -Fq 'not below the current pin 1.3.0' "$TEMP/downgrade.err"

# A stable channel must never pick a prerelease, even one that sorts between the pin and the
# dist-tag. codex publishes per-platform alpha builds such as 0.164.0-alpha.6-win32-x64 under
# the same package, and one of those broke the daily bump when 0.164.0 was still in cooldown.
node - "$now" >"$TEMP/pre-time.json" <<'NODE'
const now = Number(process.argv[2]);
const iso = seconds => new Date((now - seconds) * 1000).toISOString();
process.stdout.write(JSON.stringify({
  "0.163.0": iso(8 * 86400),
  "0.164.0-alpha.6-win32-x64": iso(4 * 86400),
  "0.164.0-alpha.6-linux-x64": iso(4 * 86400),
  "0.164.0-alpha.12": iso(4 * 86400),
  "0.164.0": iso(3600)
}));
NODE
printf '["0.163.0","0.164.0-alpha.6-win32-x64","0.164.0-alpha.6-linux-x64","0.164.0-alpha.12","0.164.0"]\n' >"$TEMP/pre-versions.json"
if selected=$(select_latest_version "$TEMP/pre-time.json" "$TEMP/pre-versions.json" 0.164.0 3 "$now" 0.163.0 2>"$TEMP/pre.err"); then
  [[ "$selected" == 0.163.0 ]] || { echo "stable channel selected prerelease $selected" >&2; exit 1; }
else
  echo "stable channel selection failed: $(cat "$TEMP/pre.err")" >&2
  exit 1
fi
# A prerelease channel still follows its own prereleases.
selected=$(select_latest_version "$TEMP/pre-time.json" "$TEMP/pre-versions.json" 0.164.0-alpha.12 3 "$now" 0.163.0)
[[ "$selected" == 0.164.0-alpha.12 ]] || { echo "prerelease channel expected 0.164.0-alpha.12, got $selected" >&2; exit 1; }

if ensure_version_old_enough 1.3.0 "$TEMP/time.json" "$now" 3 2>"$TEMP/young.err"; then
  echo "a version inside the cooldown was accepted without waiver" >&2
  exit 1
fi
grep -Fq 'younger than the 3-day cooldown' "$TEMP/young.err"
ensure_version_old_enough 1.3.0 "$TEMP/time.json" "$now" 3 "urgent security fix"

expected_before=$(node -e 'const n=Number(process.argv[1]); process.stdout.write(new Date((n-3*86400)*1000).toISOString().replace(/\.000Z$/, "Z"))' "$now")
expected_now=$(node -e 'const n=Number(process.argv[1]); process.stdout.write(new Date(n*1000).toISOString().replace(/\.000Z$/, "Z"))' "$now")
[[ "$(resolved_before_for "$now" 3 false)" == "$expected_before" ]]
[[ "$(resolved_before_for "$now" 3 true)" == "$expected_now" ]]

published=$(published_at_for 1.3.0 "$TEMP/time.json")
integrity=sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==
write_lock_entry "$LOCK" codex 1.3.0 "$integrity" "$expected_now" "$published" latest 'urgent security fix'
grep -Fq 'version = "1.3.0"' "$LOCK"
grep -Fq 'resolved_before = "'"$expected_now"'"' "$LOCK"
grep -Fq 'cooldown_waived = "urgent security fix"' "$LOCK"
grep -Fq 'smoke = "none"' "$LOCK"
grep -Fq 'smoke_arch = []' "$LOCK"
grep -Fq 'home_exec_paths = ["/home/agent/.cache/*"]' "$LOCK"
# Appending fields must keep the line break, or the next tool's entry is lost.
[[ "$(field_from_lock_line "$(lock_line_for "$LOCK" opencode)" version)" == 1.0.0 ]] ||
  { echo "writing the codex entry swallowed the opencode entry" >&2; exit 1; }

cat >"$TEMP/noop.lock.toml" <<'TOML'
schema = 1

[tools]
codex = { package = "@openai/codex", version = "1.2.0", integrity = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", resolved_before = "2024-01-01T00:00:00Z", channel = "latest", smoke = "ci", smoke_date = "2024-01-01", smoke_arch = ["amd64"], home_exec_paths = [] }
TOML
noop_before=$(shasum -a 256 "$TEMP/noop.lock.toml" | awk '{print $1}')
LOCK=$TEMP/noop.lock.toml
npm() {
  case "$*" in
    *" time --json") cat "$TEMP/time.json" ;;
    *" versions --json") cat "$TEMP/versions.json" ;;
    *" dist-tags.latest") printf '%s\n' 1.3.0 ;;
    *) echo "unexpected npm arguments: $*" >&2; return 2 ;;
  esac
}
main codex --latest --cooldown-days 3 >"$TEMP/noop.out"
noop_after=$(shasum -a 256 "$TEMP/noop.lock.toml" | awk '{print $1}')
[[ "$noop_before" == "$noop_after" ]] || { echo "same-version latest run rewrote the lock" >&2; exit 1; }
grep -Fq 'already pinned at 1.2.0; lock left unchanged' "$TEMP/noop.out"

cat >"$TEMP/ahead.lock.toml" <<'TOML'
schema = 1

[tools]
codex = { package = "@openai/codex", version = "1.4.0", integrity = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", resolved_before = "2024-01-01T00:00:00Z", channel = "latest", smoke = "ci", smoke_date = "2024-01-01", smoke_arch = ["amd64"], home_exec_paths = [] }
TOML
ahead_before=$(shasum -a 256 "$TEMP/ahead.lock.toml" | awk '{print $1}')
LOCK=$TEMP/ahead.lock.toml
main codex --latest --cooldown-days 3 >"$TEMP/ahead.out" 2>"$TEMP/ahead.err"
ahead_after=$(shasum -a 256 "$TEMP/ahead.lock.toml" | awk '{print $1}')
[[ "$ahead_before" == "$ahead_after" ]] || { echo "ahead pin was changed without an eligible release" >&2; exit 1; }
grep -Fq 'no eligible latest version is at least 3 days old and not below the current pin 1.4.0; lock left unchanged' "$TEMP/ahead.out"
[[ ! -s "$TEMP/ahead.err" ]] || { cat "$TEMP/ahead.err" >&2; exit 1; }

# npm metadata larger than the OS argument limits (codex's is ~300 KiB) must still bump.
node - "$TEMP/time.json" >"$TEMP/big-time.json" <<'NODE'
const times = JSON.parse(require('fs').readFileSync(process.argv[2], 'utf8'));
for (let i = 0; i < 40000; i++) times[`0.0.${i}-padding`] = '2020-01-01T00:00:00.000Z';
process.stdout.write(JSON.stringify(times));
NODE
(($(wc -c <"$TEMP/big-time.json") > 1100000)) || { echo "big time JSON is too small to test argv limits" >&2; exit 1; }
cat >"$TEMP/big.lock.toml" <<'TOML'
schema = 1

[tools]
codex = { package = "@openai/codex", version = "1.0.0", integrity = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", resolved_before = "2024-01-01T00:00:00Z", channel = "latest", smoke = "ci", smoke_date = "2024-01-01", smoke_arch = ["amd64"], home_exec_paths = [] }
TOML
LOCK=$TEMP/big.lock.toml
npm() {
  case "$*" in
    *" time --json") cat "$TEMP/big-time.json" ;;
    *" versions --json") cat "$TEMP/versions.json" ;;
    *" dist-tags.latest") printf '%s\n' 1.3.0 ;;
    *" dist.integrity") printf '%s\n' sha512-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB== ;;
    *) echo "unexpected npm arguments: $*" >&2; return 2 ;;
  esac
}
# ROOT points at a copy so the Dockerfile ARG rewrite stays out of the checkout.
mkdir -p "$TEMP/root/images"
printf 'ARG CODEX_VERSION=1.0.0\n' >"$TEMP/root/images/capsule.codex.Dockerfile"
(ROOT=$TEMP/root main codex --latest --cooldown-days 3 >"$TEMP/big.out")
grep -Fxq 'ARG CODEX_VERSION=1.2.0' "$TEMP/root/images/capsule.codex.Dockerfile"
grep -Fq 'codex: 1.0.0 -> 1.2.0' "$TEMP/big.out"
grep -Fq 'version = "1.2.0"' "$LOCK"

echo 'PASS bump-tool cooldown selection, monotonic pins, no-op stability, and lock metadata'
