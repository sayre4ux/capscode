#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT HUP INT TERM
LOG="$TMP/entrypoint.log"
: >"$LOG"
log() { printf 'capscode-capsule: %s\n' "$*" >>"$LOG"; }
# shellcheck source=images/codex-package-seed.sh
. "$ROOT/codex-package-seed.sh"
# shellcheck source=images/cache-owner.sh
. "$ROOT/cache-owner.sh"

OWNER=$(id -un)
GROUP=$(id -gn)
mkdir -p "$TMP/bin"
cat >"$TMP/bin/chown" <<'STUB'
#!/bin/sh
if [ "${1:-}" = "-h" ]; then
  shift
  [ "${1:-}" = "$ENTRY_TEST_OWNER:$ENTRY_TEST_GROUP" ] || exit 2
  shift
  [ "$#" -eq 2 ] || exit 2
  for path do
    [ -L "$path" ] || exit 2
  done
  printf '%s\n' "$*" >>"$ENTRY_TEST_CHOWN_LOG"
else
  [ "${1:-}" = "$ENTRY_TEST_CACHE_USER:$ENTRY_TEST_CACHE_USER" ] || exit 2
  [ "$#" -eq 2 ] && [ "$2" = "$ENTRY_TEST_CACHE_PATH" ] || exit 2
  printf '%s\n' "$2" >>"$ENTRY_TEST_CACHE_CHOWN_LOG"
fi
STUB
chmod +x "$TMP/bin/chown"
cat >"$TMP/bin/stat" <<'STUB'
#!/bin/sh
[ "${1:-}" = "-c" ] && [ "${2:-}" = "%u:%g" ] || exit 2
[ "${3:-}" = "$ENTRY_TEST_CACHE_PATH" ] || exit 2
printf '%s\n' "$ENTRY_TEST_CACHE_OWNER_IDS"
STUB
chmod +x "$TMP/bin/stat"
cat >"$TMP/bin/mountpoint" <<'STUB'
#!/bin/sh
exit 1
STUB
chmod +x "$TMP/bin/mountpoint"
export ENTRY_TEST_OWNER="$OWNER"
export ENTRY_TEST_GROUP="$GROUP"
export ENTRY_TEST_CHOWN_LOG="$TMP/chown.log"
export ENTRY_TEST_CACHE_USER=agent
export ENTRY_TEST_CACHE_PATH="$TMP/cache-volume"
export ENTRY_TEST_CACHE_OWNER_IDS=999:999
export ENTRY_TEST_CACHE_CHOWN_LOG="$TMP/cache-chown.log"
PATH="$TMP/bin:$PATH"
export PATH

VENDOR="$TMP/vendor"
HOME_DIR="$TMP/home"
mkdir -p "$VENDOR" "$HOME_DIR"
cat >"$VENDOR/codex-package.json" <<'JSON'
{"layoutVersion":1,"version":"9.9.9","target":"x86_64-unknown-linux-musl"}
JSON
seed_codex_daemon_package "$VENDOR" "$HOME_DIR" "$OWNER" "$GROUP"

PACKAGE="$HOME_DIR/.codex/packages/app-server-daemon"
RELEASE="$PACKAGE/releases/9.9.9-x86_64-unknown-linux-musl"
[ -L "$RELEASE" ]
[ "$(readlink "$RELEASE")" = "$VENDOR" ]
[ -L "$PACKAGE/current" ]
[ "$(readlink "$PACKAGE/current")" = "$RELEASE" ]
case "$(readlink "$PACKAGE/current")" in
  /*) ;;
  *) printf '%s\n' 'Codex current symlink is not absolute' >&2; exit 1 ;;
esac
[ -s "$ENTRY_TEST_CHOWN_LOG" ]
grep -q 'seeded Codex daemon package 9.9.9-x86_64-unknown-linux-musl' "$LOG"

for manifest in layout-two missing; do
  : >"$LOG"
  bad_vendor="$TMP/$manifest-vendor"
  bad_home="$TMP/$manifest-home"
  mkdir -p "$bad_vendor" "$bad_home"
  if [ "$manifest" = layout-two ]; then
    printf '%s\n' '{"layoutVersion":2,"version":"9.9.9","target":"x86_64-unknown-linux-musl"}' \
      >"$bad_vendor/codex-package.json"
  fi
  seed_codex_daemon_package "$bad_vendor" "$bad_home" "$OWNER" "$GROUP"
  [ ! -e "$bad_home/.codex/packages/app-server-daemon/current" ]
  [ ! -L "$bad_home/.codex/packages/app-server-daemon/current" ]
  grep -q 'WARN:' "$LOG"
done

mkdir -p "$ENTRY_TEST_CACHE_PATH"
ensure_cache_owner "$ENTRY_TEST_CACHE_PATH" "$ENTRY_TEST_CACHE_USER" 10001 10001
[ "$(cat "$ENTRY_TEST_CACHE_CHOWN_LOG")" = "$ENTRY_TEST_CACHE_PATH" ]

ENTRY_TEST_CACHE_OWNER_IDS=10001:10001
export ENTRY_TEST_CACHE_OWNER_IDS
ensure_cache_owner "$ENTRY_TEST_CACHE_PATH" "$ENTRY_TEST_CACHE_USER" 10001 10001
[ "$(wc -l <"$ENTRY_TEST_CACHE_CHOWN_LOG" | tr -d ' ')" = 1 ]

ensure_cache_owner "$TMP/cache-absent" "$ENTRY_TEST_CACHE_USER" 10001 10001
[ "$(wc -l <"$ENTRY_TEST_CACHE_CHOWN_LOG" | tr -d ' ')" = 1 ]

# The Codex daemon switch reaches keeper.sh through keeper-only setup, outside
# the allowlist forwarded to the agent tools.
# shellcheck disable=SC2016
grep -Fq 'CAPSCODE_CODEX_DAEMON=${CAPSCODE_CODEX_DAEMON:-}' "$ROOT/entrypoint.sh"
