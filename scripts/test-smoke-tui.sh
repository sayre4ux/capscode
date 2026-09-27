#!/usr/bin/env bash
# Checks smoke-tui.sh's verdict logic against canned capsule output from a fake
# `docker`, so a harness change that makes the smoke pass vacuously is caught
# without building images.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TEMP=$(mktemp -d "${TMPDIR:-/tmp}/capscode-smoke-test.XXXXXX")
trap 'rm -rf -- "$TEMP"' EXIT
mkdir -p "$TEMP/bin" "$TEMP/transcripts"

# The fake docker prints the fixture named by SMOKE_FIXTURE instead of starting a
# container, and records its argv for the command-shape assertions.
cat >"$TEMP/bin/docker" <<'FAKE'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$SMOKE_ARGV_FILE"
cat "$SMOKE_FIXTURE"
FAKE
chmod +x "$TEMP/bin/docker"

# The entrypoint echoes the inner script, marker names included, before running it.
starting_line="capscode-capsule: starting '/bin/sh -c set -eu; ... echo SMOKE_EXECUTABLES_BEGIN; find /home/agent /tmp -type f -perm -u+x -print; echo SMOKE_EXECUTABLES_END; ...' as user agent"

cat >"$TEMP/pass.txt" <<EOF
$starting_line
SMOKE_PROCESS_ALIVE_AT_20S
SMOKE_PTY_EXIT_STATUS=124
SMOKE_EXECUTABLES_BEGIN
SMOKE_EXECUTABLES_END
SMOKE_TRANSCRIPT_BEGIN
Welcome
SMOKE_TRANSCRIPT_END
EOF

cat >"$TEMP/denied.txt" <<EOF
$starting_line
SMOKE_PROCESS_DEAD_BEFORE_20S
SMOKE_PTY_EXIT_STATUS=1
SMOKE_EXECUTABLES_BEGIN
SMOKE_EXECUTABLES_END
SMOKE_TRANSCRIPT_BEGIN
Error: failed to invoke managed Codex binary /home/agent/.codex/packages/app-server-daemon/releases/.staging.x/bin/codex: Permission denied (os error 13)
SMOKE_TRANSCRIPT_END
EOF

cat >"$TEMP/unexpected-exec.txt" <<EOF
$starting_line
SMOKE_PROCESS_ALIVE_AT_20S
SMOKE_PTY_EXIT_STATUS=124
SMOKE_EXECUTABLES_BEGIN
/home/agent/.pi/agent/bin/rg
SMOKE_EXECUTABLES_END
SMOKE_TRANSCRIPT_BEGIN
SMOKE_TRANSCRIPT_END
EOF

run_smoke() {
  SMOKE_FIXTURE=$1 SMOKE_ARGV_FILE="$TEMP/argv" SMOKE_TRANSCRIPT_DIR="$TEMP/transcripts" \
    PATH="$TEMP/bin:$PATH" CAPSCODE_TOOLCHAIN_LOCK="$ROOT/images/toolchain.lock.toml" \
    "$ROOT/scripts/smoke-tui.sh" codex capscode/capsule-codex:test >"$TEMP/out" 2>&1
}

assertions=0
pass() { assertions=$((assertions + 1)); }

run_smoke "$TEMP/pass.txt" || { cat "$TEMP/out" >&2; echo 'echoed marker names were parsed as executables' >&2; exit 1; }
grep -Fq 'PASS codex' "$TEMP/out"; pass

inner=$(tail -n 3 "$TEMP/argv" | head -n 1)
[[ "$inner" == *'timeout --foreground '* ]] || { echo 'the tool must stay in the pty foreground process group' >&2; exit 1; }
pass
[[ "$inner" == *'/tmp/capscode-smoke.typescript >/dev/null &'* ]] || { echo "script's stdout copy must not reach the marker lines" >&2; exit 1; }
pass

if run_smoke "$TEMP/denied.txt"; then echo 'a dead tool with Permission denied passed' >&2; exit 1; fi
grep -Fq 'tool process was not alive at 20 seconds' "$TEMP/out"; pass
grep -Fq 'transcript contains an executable startup failure' "$TEMP/out"; pass

if run_smoke "$TEMP/unexpected-exec.txt"; then echo 'an unlisted executable under home passed' >&2; exit 1; fi
grep -Fq 'unexpected executable under home/tmp: /home/agent/.pi/agent/bin/rg' "$TEMP/out"; pass

source "$ROOT/scripts/lib/toolchain-lock.sh"
cat >"$TEMP/multiline-pass.txt" <<EOF
$starting_line
SMOKE_PROCESS_ALIVE_AT_20S
SMOKE_PTY_EXIT_STATUS=124
SMOKE_EXECUTABLES_BEGIN
/home/agent/.pi/agent/bin/rg
SMOKE_EXECUTABLES_END
SMOKE_TRANSCRIPT_BEGIN
Welcome
SMOKE_TRANSCRIPT_END
EOF
cat >"$TEMP/multiline.lock.toml" <<'TOML'
schema = 1

[tools.codex]
version = "0.159.3"
smoke = "none"
smoke_date = ""
smoke_arch = [
  "amd64",
  "arm64",
]
home_exec_paths = [
  "/home/agent/.codex/**",
  "/home/agent/.pi/agent/**",
]
TOML
mkdir -p "$TEMP/capscode-home/toolchains"
fixture_digest=$(toolchain_lock_digest "$TEMP/multiline.lock.toml")
snapshot="$TEMP/capscode-home/toolchains/$fixture_digest.lock.toml"
mv "$TEMP/multiline.lock.toml" "$snapshot"
SMOKE_FIXTURE="$TEMP/multiline-pass.txt" SMOKE_ARGV_FILE="$TEMP/argv" \
  SMOKE_TRANSCRIPT_DIR="$TEMP/transcripts" CAPSCODE_HOME="$TEMP/capscode-home" \
  CAPSCODE_TOOLCHAIN_LOCK="$snapshot" PATH="$TEMP/bin:$PATH" \
  "$ROOT/scripts/smoke-tui.sh" codex capscode/capsule-codex:test --record >"$TEMP/multiline.out" 2>&1 || {
    cat "$TEMP/multiline.out" >&2
    echo 'multi-line arrays must be read and smoke metadata must be recorded' >&2
    exit 1
  }
grep -Fq 'PASS codex' "$TEMP/multiline.out"; pass
grep -Fq 'smoke = "ci"' "$snapshot"; pass
case "$(uname -m)" in
  x86_64|amd64) expected_arch=amd64 ;;
  aarch64|arm64) expected_arch=arm64 ;;
  *) expected_arch=$(uname -m) ;;
esac
grep -Fq "smoke_arch = [\"$expected_arch\"]" "$snapshot" || {
  cat "$snapshot" >&2
  echo 'multi-line smoke_arch array was not rewritten' >&2
  exit 1
}
pass
grep -Fq '"/home/agent/.codex/**"' "$snapshot"; pass

cp "$snapshot" "$TEMP/external.lock.toml"
if SMOKE_FIXTURE="$TEMP/pass.txt" SMOKE_ARGV_FILE="$TEMP/argv" \
  SMOKE_TRANSCRIPT_DIR="$TEMP/transcripts" CAPSCODE_HOME="$TEMP/capscode-home" \
  CAPSCODE_TOOLCHAIN_LOCK="$TEMP/external.lock.toml" PATH="$TEMP/bin:$PATH" \
  "$ROOT/scripts/smoke-tui.sh" codex capscode/capsule-codex:test --record >"$TEMP/record-refused.out" 2>&1; then
  echo 'smoke-tui recorded an arbitrary external lock' >&2
  exit 1
fi
grep -Fq 'Refusing to record smoke results into this lock' "$TEMP/record-refused.out"; pass

echo "PASS smoke-tui verdicts ($assertions assertions)"
