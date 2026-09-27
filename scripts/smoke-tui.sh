#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LOCK=${CAPSCODE_TOOLCHAIN_LOCK:-$ROOT/images/toolchain.lock.toml}
source "$ROOT/scripts/lib/toolchain-lock.sh"
scratch_dirs=()
cleanup() {
  local scratch
  for scratch in "${scratch_dirs[@]:-}"; do
    # An `[[ ]] && rm` as the trap's last command would turn an empty list into exit status 1.
    if [[ -n "$scratch" ]]; then rm -rf -- "$scratch"; fi
  done
}
trap cleanup EXIT

# DECISION: keep Docker's mount arguments here; a Rust test parses these
# constants and checks them against capsule_spec_with.
SMOKE_TMP_TARGET=/tmp
SMOKE_TMP_OPTIONS=rw,exec,nosuid,nodev
SMOKE_TMP_SIZE_MIB=1024
SMOKE_HOME_TARGET=/home/agent
SMOKE_HOME_OPTIONS=rw,noexec,nosuid,nodev
SMOKE_HOME_SIZE_MIB=512
SMOKE_SHM_TARGET=/dev/shm
SMOKE_SHM_OPTIONS=rw,noexec,nosuid,nodev
SMOKE_SHM_SIZE_MIB=256
SMOKE_WORKSPACE_TARGET=/workspace

# DECISION: playwright_core is a library dependency, so smoke the five interactive tools only.
SMOKE_TOOLS=(claude codex opencode pi omp)

usage() {
  cat <<'USAGE'
Usage: scripts/smoke-tui.sh <tool|all> [image] [--dry-run] [--negative-control]

Start each pinned tool's interactive TUI in a throwaway, network-isolated
capsule and check its startup transcript and executable paths.

--dry-run          Print the exact docker run argv without starting a container.
--negative-control Drop /tmp exec and skip the Codex package seed.
--record           Record a successful start-up smoke in the toolchain lock.
USAGE
}

target=
image_override=
dry_run=0
record=0
negative_control=${SMOKE_NEGATIVE_CONTROL:-0}
while (($#)); do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --dry-run) dry_run=1 ;;
    --record) record=1 ;;
    --negative-control) negative_control=1 ;;
    *)
      if [[ -z "$target" ]]; then
        target=$1
      elif [[ -z "$image_override" ]]; then
        image_override=$1
      else
        printf 'Unexpected argument: %s\n' "$1" >&2
        usage >&2
        exit 2
      fi
      ;;
  esac
  shift
done

if [[ -z "$target" ]]; then usage >&2; exit 2; fi
if [[ "$negative_control" != 0 && "$negative_control" != 1 ]]; then
  printf '%s\n' 'SMOKE_NEGATIVE_CONTROL must be 0 or 1' >&2
  exit 2
fi
case "$target" in
  all) tools=("${SMOKE_TOOLS[@]}") ;;
  claude|codex|opencode|pi|omp) tools=("$target") ;;
  *) printf 'Unknown tool: %s\n' "$target" >&2; usage >&2; exit 2 ;;
esac

lock_value_for() {
  local tool=$1 field=$2
  awk -v tool="$tool" -v field="$field" '
    function value(line, field, expression, result, rest, ending) {
      expression = field "[[:space:]]*=[[:space:]]*"
      if (!match(line, expression)) return
      rest = substr(line, RSTART + RLENGTH)
      sub(/^[[:space:]]*/, "", rest)
      if (substr(rest, 1, 1) == "\"") {
        if (match(rest, /^\"[^\"]*\"/)) print substr(rest, 2, RLENGTH - 2)
        return
      }
      if (substr(rest, 1, 1) != "[") return
      result = rest
      while (index(result, "]") == 0 && (getline line) > 0) result = result " " line
      ending = index(result, "]")
      if (!ending) return
      result = substr(result, 1, ending)
      gsub(/[[:space:]]/, "", result)
      print result
    }
    /^\[tools\]$/ { section = "tools"; next }
    /^\[tools\.[^]]+\]$/ {
      section = ($0 == "[tools." tool "]") ? "tool" : "other"
      next
    }
    /^\[/ { section = "other"; next }
    section == "tools" && $1 == tool && $2 == "=" { value($0, field); exit }
    section == "tool" && $1 == field { value($0, field); exit }
  ' "$LOCK"
}

lock_has_field() {
  local tool=$1 field=$2
  awk -v tool="$tool" -v field="$field" '
    /^\[tools\]$/ { section = "tools"; next }
    /^\[tools\.[^]]+\]$/ {
      section = ($0 == "[tools." tool "]") ? "tool" : "other"
      next
    }
    /^\[/ { section = "other"; next }
    section == "tools" && $1 == tool && $2 == "=" && index($0, field " =") { found = 1; exit }
    section == "tool" && $1 == field { found = 1; exit }
    END { exit !found }
  ' "$LOCK"
}

record_lock_allowed() {
  local lock_real repo_lock_real config_home snapshot_dir snapshot_real expected_digest
  lock_real=$(perl -MCwd=realpath -e 'print realpath($ARGV[0]) // ""' "$LOCK")
  repo_lock_real=$(perl -MCwd=realpath -e 'print realpath($ARGV[0]) // ""' "$ROOT/images/toolchain.lock.toml")
  [[ -n "$lock_real" ]] || return 1
  [[ "$lock_real" == "$repo_lock_real" ]] && return 0

  if [[ -n "${CAPSCODE_HOME:-}" ]]; then config_home=$CAPSCODE_HOME
  elif [[ -n "${HOME:-}" ]]; then config_home=$HOME/.capscode
  else return 1
  fi
  snapshot_dir=$config_home/toolchains
  snapshot_real=$(perl -MCwd=realpath -e 'print realpath($ARGV[0]) // ""' "$snapshot_dir")
  [[ -n "$snapshot_real" && "$(dirname "$lock_real")" == "$snapshot_real" ]] || return 1
  expected_digest=$(toolchain_lock_digest "$LOCK") || return 1
  [[ "$(basename "$lock_real")" == "$expected_digest.lock.toml" ]]
}

# DECISION: a successful credential-free start-up smoke records CI grade; a
# maintainer session remains a separate manual grade.
record_smoke() {
  local tool=$1 date arch
  date=$(date -u +%Y-%m-%d)
  case "$(uname -m)" in
    x86_64|amd64) arch=amd64 ;;
    aarch64|arm64) arch=arm64 ;;
    *) arch=$(uname -m) ;;
  esac
  toolchain_record_smoke "$LOCK" "$tool" "$date" "$arch"
}

if ((record)) && ! record_lock_allowed; then
  printf '%s\n' 'Refusing to record smoke results into this lock; use the repository lock or a CapsCode-owned toolchain snapshot.' >&2
  exit 1
fi

# The merged image's pi and omp box shims select the host-written catalog entry
# named by CAPSCODE_*_DEFAULT_PROVIDER, so the smoke writes a one-model catalog
# in the same shape the CLI does. pi reads apiKey as "$VAR", omp as a bare name.
smoke_catalog() {
  printf '{"providers":{"capscode-smoke":{"baseUrl":"https://127.0.0.1:8443","apiKey":"%s","api":"openai-completions","models":[{"id":"capscode-smoke-model","name":"CapsCode smoke"}]}}}' "$1"
}

run_tool() {
  local tool=$1 image=$2 scratch=$3
  local tmp_options=$SMOKE_TMP_OPTIONS
  if ((negative_control)); then
    tmp_options=rw,noexec,nosuid,nodev
  fi

  local tool_env_keys='TZ LANG LC_ALL HISTFILE CAPSCODE_CA_CERT_PEM CAPSCODE_WORKSPACE_MODE ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL CLAUDE_CODE_OAUTH_TOKEN OPENAI_API_KEY OPENAI_BASE_URL CAPSCODE_CODEX_BROKER_TOKEN CAPSCODE_OPENCODE_API_KEY CAPSCODE_PI_API_KEY CAPSCODE_OMP_API_KEY CAPSCODE_PI_DEFAULT_MODEL CAPSCODE_OMP_DEFAULT_MODEL CAPSCODE_PI_DEFAULT_PROVIDER CAPSCODE_OMP_DEFAULT_PROVIDER SMOKE_PI_CATALOG SMOKE_OMP_CATALOG NODE_EXTRA_CA_CERTS SSL_CERT_FILE REQUESTS_CA_BUNDLE CURL_CA_BUNDLE GIT_SSL_CAINFO GIT_CONFIG_GLOBAL GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 XDG_CACHE_HOME npm_config_cache BUN_INSTALL_CACHE_DIR OPENCODE_DISABLE_AUTOUPDATE'
  # This string runs inside the capsule; its parameter references expand there.
  # script(1) also copies the tool's output to its own stdout; discard that copy
  # (the typescript keeps it) so terminal escapes cannot prefix the marker lines.
  # timeout needs --foreground: otherwise it moves the tool out of the pty's
  # foreground process group, the first terminal write stops the tool (SIGTTOU),
  # and a stopped tool looks alive at 20 seconds without ever starting.
  # shellcheck disable=SC2016
  local inner_script='set -eu; mkdir -p "$HOME/.pi/agent" "$HOME/.omp/agent"; printf %s "$SMOKE_PI_CATALOG" >"$HOME/.pi/agent/models.json"; printf %s "$SMOKE_OMP_CATALOG" >"$HOME/.omp/agent/models.yml"; script -qefc "timeout --foreground --signal=TERM --kill-after=3s 30s __SMOKE_TOOL__" /tmp/capscode-smoke.typescript >/dev/null & pty_pid=$!; sleep 20; if kill -0 "$pty_pid" 2>/dev/null; then echo SMOKE_PROCESS_ALIVE_AT_20S; else echo SMOKE_PROCESS_DEAD_BEFORE_20S; fi; if wait "$pty_pid"; then pty_status=0; else pty_status=$?; fi; echo SMOKE_PTY_EXIT_STATUS=$pty_status; echo SMOKE_EXECUTABLES_BEGIN; find /home/agent /tmp -type f -perm -u+x -print; echo SMOKE_EXECUTABLES_END; echo SMOKE_TRANSCRIPT_BEGIN; cat /tmp/capscode-smoke.typescript 2>/dev/null || true; echo SMOKE_TRANSCRIPT_END'
  inner_script=${inner_script//__SMOKE_TOOL__/$tool}

  # pi's catalog names its key variable as a literal "$VAR" string.
  # shellcheck disable=SC2016
  local -a args=(
    docker run --rm --network none --read-only --hostname workspace --workdir "$SMOKE_WORKSPACE_TARGET"
    --cap-drop=ALL
    --cap-add=CHOWN --cap-add=DAC_OVERRIDE --cap-add=FOWNER --cap-add=SETUID --cap-add=SETGID
    --security-opt=no-new-privileges --pids-limit=1024
    --tmpfs "$SMOKE_TMP_TARGET:$tmp_options,size=${SMOKE_TMP_SIZE_MIB}m"
    --tmpfs "$SMOKE_HOME_TARGET:$SMOKE_HOME_OPTIONS,size=${SMOKE_HOME_SIZE_MIB}m"
    --tmpfs "$SMOKE_SHM_TARGET:$SMOKE_SHM_OPTIONS,size=${SMOKE_SHM_SIZE_MIB}m"
    --mount "type=bind,source=$scratch,target=$SMOKE_WORKSPACE_TARGET"
    -e TZ=UTC -e LANG=C.UTF-8 -e LC_ALL=C.UTF-8 -e HISTFILE=/dev/null
    -e CAPSCODE_CA_CERT_PEM= -e CAPSCODE_WORKSPACE_MODE=copy
    -e CAPSCODE_TOOL_ENV_KEYS="$tool_env_keys"
    -e ANTHROPIC_AUTH_TOKEN=capscode-smoke-sentinel
    -e ANTHROPIC_BASE_URL=https://127.0.0.1:8443
    -e CLAUDE_CODE_OAUTH_TOKEN=capscode-smoke-sentinel
    -e OPENAI_API_KEY=capscode-smoke-sentinel
    -e OPENAI_BASE_URL=https://127.0.0.1:8443
    -e CAPSCODE_CODEX_BROKER_TOKEN=capscode-smoke-sentinel
    -e CAPSCODE_OPENCODE_API_KEY=capscode-smoke-sentinel
    -e CAPSCODE_PI_API_KEY=capscode-smoke-sentinel
    -e CAPSCODE_OMP_API_KEY=capscode-smoke-sentinel
    -e CAPSCODE_PI_DEFAULT_MODEL=capscode-smoke-model
    -e CAPSCODE_OMP_DEFAULT_MODEL=capscode-smoke-model
    -e CAPSCODE_PI_DEFAULT_PROVIDER=capscode-smoke
    -e CAPSCODE_OMP_DEFAULT_PROVIDER=capscode-smoke
    -e SMOKE_PI_CATALOG="$(smoke_catalog '$CAPSCODE_PI_API_KEY')"
    -e SMOKE_OMP_CATALOG="$(smoke_catalog CAPSCODE_OMP_API_KEY)"
    -e NODE_EXTRA_CA_CERTS=/home/agent/.capscode/ca.pem
    -e SSL_CERT_FILE=/home/agent/.capscode/ca.pem
    -e REQUESTS_CA_BUNDLE=/home/agent/.capscode/ca.pem
    -e CURL_CA_BUNDLE=/home/agent/.capscode/ca.pem
    -e GIT_SSL_CAINFO=/home/agent/.capscode/ca.pem
    -e GIT_CONFIG_GLOBAL=/home/agent/.gitconfig
    -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0=/workspace
    -e XDG_CACHE_HOME=/tmp/.cache -e npm_config_cache=/tmp/.cache/npm
    -e BUN_INSTALL_CACHE_DIR=/tmp/.cache/bun -e OPENCODE_DISABLE_AUTOUPDATE=1
  )
  if ((negative_control)); then
    args+=(-e CAPSCODE_SMOKE_SKIP_CODEX_SEED=1)
  fi
  args+=(--entrypoint /usr/local/bin/capscode-entrypoint "$image" /bin/sh -c "$inner_script" capscode-smoke "$tool")

  if ((dry_run)); then
    printf 'DRY-RUN %s: ' "$tool"
    printf '%q ' "${args[@]}"
    printf '\n'
    return 0
  fi

  local transcript_dir=${SMOKE_TRANSCRIPT_DIR:-${TMPDIR:-/tmp}/capscode-smoke-tui}
  mkdir -p "$transcript_dir"
  local stamp transcript status reason
  stamp=$(date -u +%Y%m%dT%H%M%SZ)
  transcript="$transcript_dir/$tool-$stamp-$$.log"
  status=0
  "${args[@]}" >"$transcript" 2>&1 || status=$?
  reason=

  if ((status != 0)); then reason="docker run exited with status $status"; fi
  if ! grep -Fxq 'SMOKE_PROCESS_ALIVE_AT_20S' "$transcript"; then
    [[ -n "$reason" ]] && reason+="; "
    reason+="tool process was not alive at 20 seconds"
  fi
  if grep -Eiq 'Permission denied|EACCES|failed to map segment|failed to invoke managed' "$transcript"; then
    [[ -n "$reason" ]] && reason+="; "
    reason+="transcript contains an executable startup failure"
  fi

  local version raw_paths exec_path allowed pattern exec_paths
  local -a path_patterns=()
  version=$(lock_value_for "$tool" version)
  raw_paths=$(lock_value_for "$tool" home_exec_paths)
  if [[ -z "$version" ]] || ! lock_has_field "$tool" home_exec_paths; then
    [[ -n "$reason" ]] && reason+="; "
    reason+="toolchain lock is missing the version or executable allowlist"
  fi
  raw_paths=${raw_paths//\"/}
  raw_paths=${raw_paths//[[:space:]]/}
  if [[ -n "$raw_paths" ]]; then
    IFS=, read -r -a path_patterns <<<"$raw_paths"
  fi
  # Match the markers as whole lines: the entrypoint echoes the inner script,
  # marker names included, before it runs.
  exec_paths=$(awk '$0 == "SMOKE_EXECUTABLES_BEGIN" {capture=1; next} $0 == "SMOKE_EXECUTABLES_END" {capture=0} capture {print}' "$transcript")
  while IFS= read -r exec_path; do
    [[ -n "$exec_path" ]] || continue
    allowed=0
    for pattern in "${path_patterns[@]:-}"; do
      # The allowlist deliberately supports glob patterns from the TOML lock.
      # shellcheck disable=SC2053
      [[ -n "$pattern" && "$exec_path" == $pattern ]] && allowed=1
    done
    if (( ! allowed )); then
      [[ -n "$reason" ]] && reason+="; "
      reason+="unexpected executable under home/tmp: $exec_path"
    fi
  done <<<"$exec_paths"

  if [[ -z "$reason" ]]; then
    printf 'PASS %s (%s) transcript: %s\n' "$tool" "$image" "$transcript"
    if ((record)); then record_smoke "$tool"; fi
    return 0
  fi
  printf 'FAIL %s: %s; transcript: %s\n' "$tool" "$reason" "$transcript" >&2
  return 1
}

failures=0
for tool in "${tools[@]}"; do
  version=$(lock_value_for "$tool" version)
  if [[ -z "$version" ]]; then
    printf 'FAIL %s: no version in images/toolchain.lock.toml\n' "$tool" >&2
    failures=$((failures + 1))
    continue
  fi
  image=${image_override:-capscode/capsule-$tool:$version}
  scratch=$(mktemp -d "${TMPDIR:-/tmp}/capscode-smoke-workspace.XXXXXX")
  scratch_dirs+=("$scratch")
  if ! run_tool "$tool" "$image" "$scratch"; then
    failures=$((failures + 1))
  fi
  rm -rf "$scratch"
done

((failures == 0))
