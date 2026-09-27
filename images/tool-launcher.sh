#!/bin/sh
# Verify the broker route before a tool can offer its own vendor login.
set -u

route_key=${1:-}
real_binary=${2:-}
[ "$#" -ge 2 ] || exit 78
shift 2

case "$route_key" in
  ANTHROPIC_BASE_URL) tool=claude ;;
  OPENAI_BASE_URL) tool=codex ;;
  CAPSCODE_PI_API_KEY) tool=pi ;;
  CAPSCODE_OMP_API_KEY) tool=omp ;;
  CAPSCODE_OPENCODE_API_KEY) tool=opencode ;;
  *)
    printf '%s\n' 'invalid tool route; refusing to start inside the capsule' >&2
    exit 78
    ;;
esac

eval "route_value=\${$route_key-}"
if [ -z "$route_value" ]; then
  printf 'Cannot start %s: its provider is not routed in this box.\n' "$tool" >&2
  exit 78
fi

[ -x "$real_binary" ] || {
  printf 'Cannot start %s: its installed executable is missing.\n' "$tool" >&2
  exit 127
}
exec "$real_binary" "$@"
