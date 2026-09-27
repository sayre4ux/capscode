#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TEMP=$(mktemp -d "${TMPDIR:-/tmp}/capscode-channel-retry-test.XXXXXX")
trap 'rm -rf -- "$TEMP"' EXIT
mkdir -p "$TEMP/bin" "$TEMP/state"
STATE="$TEMP/state"

cat >"$TEMP/state/channel.json" <<'JSON'
{"schema":1,"generated_at":"2026-10-18T01:00:00Z","cli":{"version":"1.0.0","tag":"v1.0.0"},"toolchain":{"sequence":7,"digest":"old"}}
JSON
cat >"$TEMP/state/race.json" <<'JSON'
{"schema":1,"generated_at":"2026-10-18T02:00:00Z","cli":{"version":"1.0.0","tag":"v1.0.0"},"toolchain":{"sequence":8,"digest":"concurrent"}}
JSON
printf '%s\n' old >"$TEMP/state/oid"
printf '0\n' >"$TEMP/state/pushes"
cat >"$TEMP/prepared.json" <<'JSON'
{"schema":1,"generated_at":"2026-10-18T00:00:00Z","cli":{"version":"2.0.0","tag":"v2.0.0"},"toolchain":{"sequence":6,"digest":"stale"}}
JSON

cat >"$TEMP/bin/git" <<'FAKE'
#!/usr/bin/env bash
set -eu
target=
if [[ "${1:-}" == -C ]]; then target=$2; shift 2; fi
command=${1:-}
shift || true
case "$command" in
  rev-parse)
    if [[ "$target" == "$FAKE_REPO_ROOT" || -z "$target" ]]; then
      printf '%s\n' "$FAKE_REPO_ROOT"
    else
      exit 1
    fi
    ;;
  ls-remote)
    printf '%s refs/heads/channel\n' "$(cat "$FAKE_GIT_STATE/oid")"
    ;;
  fetch) ;;
  worktree)
    if [[ "${1:-}" == add ]]; then mkdir -p "${3:-}"; fi
    ;;
  show) cat "$FAKE_GIT_STATE/channel.json" ;;
  add) cp "$target/channel.json" "$FAKE_GIT_STATE/staged.json" ;;
  diff)
    if cmp -s "$FAKE_GIT_STATE/staged.json" "$FAKE_GIT_STATE/channel.json"; then exit 0; else exit 1; fi
    ;;
  reset) : >"$FAKE_GIT_STATE/reset-called"; rm -f "$FAKE_GIT_STATE/staged.json" ;;
  switch|config|commit|rm) ;;
  push)
    pushes=$(cat "$FAKE_GIT_STATE/pushes")
    if ((pushes == 0)); then
      cp "$FAKE_GIT_STATE/race.json" "$FAKE_GIT_STATE/channel.json"
      printf '%s\n' race >"$FAKE_GIT_STATE/oid"
      printf '1\n' >"$FAKE_GIT_STATE/pushes"
      printf '%s\n' 'simulated non-fast-forward'
      exit 1
    fi
    cp "$FAKE_GIT_STATE/staged.json" "$FAKE_GIT_STATE/channel.json"
    printf '%s\n' pushed >"$FAKE_GIT_STATE/oid"
    printf '2\n' >"$FAKE_GIT_STATE/pushes"
    ;;
  *) printf 'unexpected fake git command: %s %s\n' "$command" "$*" >&2; exit 2 ;;
esac
FAKE
chmod +x "$TEMP/bin/git"

FAKE_GIT_STATE="$STATE" FAKE_REPO_ROOT="$ROOT" GITHUB_REPOSITORY=owner/repo GH_TOKEN=fake \
  PATH="$TEMP/bin:$PATH" bash -c 'cd "$FAKE_REPO_ROOT" && scripts/release/publish-channel.sh --section cli --manifest "$1"' _ "$TEMP/prepared.json"

jq -e '.generated_at == "2026-10-18T02:00:00Z" and .cli.version == "2.0.0" and .toolchain.sequence == 8' \
  "$TEMP/state/channel.json" >/dev/null
[[ "$(cat "$TEMP/state/pushes")" == 2 ]]
[[ "$(cat "$TEMP/state/oid")" == pushed ]]

printf '%s\n' 'PASS channel publisher merges across a simulated non-fast-forward'
