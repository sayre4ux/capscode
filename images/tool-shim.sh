#!/bin/sh
# Generate one box-only wrapper around the executable resolved during the image build.
set -eu

tool=${1:-}
route_key=${2:-}
real_binary=${3:-}
shim_dir=${CAPSCODE_SHIM_DIR:-/usr/local/bin}
launcher=${CAPSCODE_TOOL_LAUNCHER:-/usr/local/bin/capscode-tool-launcher}

case "$tool:$route_key" in
  claude:ANTHROPIC_BASE_URL|codex:OPENAI_BASE_URL|opencode:CAPSCODE_OPENCODE_API_KEY|pi:CAPSCODE_PI_API_KEY|omp:CAPSCODE_OMP_API_KEY) ;;
  *) printf '%s\n' 'tool-shim: unsupported tool or route key' >&2; exit 2 ;;
esac
case "$tool" in
  pi) model_key=CAPSCODE_PI_DEFAULT_MODEL provider_key=CAPSCODE_PI_DEFAULT_PROVIDER ;;
  omp) model_key=CAPSCODE_OMP_DEFAULT_MODEL provider_key=CAPSCODE_OMP_DEFAULT_PROVIDER ;;
  *) model_key='' provider_key='' ;;
esac
case "$real_binary" in
  /*) ;;
  *) printf '%s\n' 'tool-shim: executable path must be absolute' >&2; exit 2 ;;
esac
[ -x "$real_binary" ] || { printf '%s\n' 'tool-shim: executable is missing or not executable' >&2; exit 2; }
[ -d "$shim_dir" ] || { printf '%s\n' 'tool-shim: output directory does not exist' >&2; exit 2; }

# Quote the resolved path as a shell word so the generated shim never consults PATH.
shell_quote() {
  rest=$1
  quote="'"
  out="'"
  while :; do
    case "$rest" in
      *"$quote"*)
        part=${rest%%"$quote"*}
        rest=${rest#*"$quote"}
        out="$out$part'\\''"
        ;;
      *) out="$out$rest'"; break ;;
    esac
  done
  printf '%s' "$out"
}

shim="$shim_dir/$tool"
if [ "$real_binary" = "$shim" ]; then
  printf 'tool-shim: %s resolves to its own shim path\n' "$tool" >&2
  exit 2
fi
# npm installs these entry points as symlinks. Remove the link after its target
# has been captured so redirection cannot truncate the installed executable.
rm -f "$shim"
{
  printf '%s\n' '#!/bin/sh' 'set -eu'
  printf 'tool=%s\n' "$(shell_quote "$tool")"
  printf 'route_key=%s\n' "$(shell_quote "$route_key")"
  printf 'real_binary=%s\n' "$(shell_quote "$real_binary")"
  printf 'launcher=%s\n' "$(shell_quote "$launcher")"
  if [ "$tool" = pi ] || [ "$tool" = omp ]; then
    printf 'model_key=%s\n' "$(shell_quote "$model_key")"
    printf 'provider_key=%s\n' "$(shell_quote "$provider_key")"
    cat <<'SHIM'
has_model=0
for arg do
  case "$arg" in
    --provider|--provider=*|--api-key|--api-key=*|--profile|--profile=*)
      printf 'Cannot start %s: --provider, --api-key, and --profile are managed by this box.\n' "$tool" >&2
      exit 2
      ;;
    --model|--model=*) has_model=1 ;;
  esac
done
if [ "$has_model" -eq 0 ]; then
  eval "default_model=\${$model_key-}"
  if [ -z "$default_model" ]; then
    printf 'Cannot start %s: its default model is missing in this box.\n' "$tool" >&2
    exit 78
  fi
  set -- --model "$default_model" "$@"
fi
# The host names the default provider's entry in the generated model catalog.
eval "provider=\${$provider_key-}"
if [ -z "$provider" ]; then
  printf 'Cannot start %s: its default provider is missing in this box.\n' "$tool" >&2
  exit 78
fi
# DECISION: an explicit --model still goes through the box's provider, never a built-in one.
set -- --provider "$provider" "$@"
SHIM
  fi
  # shellcheck disable=SC2016
  printf '%s\n' 'exec "$launcher" "$route_key" "$real_binary" "$@"'
} > "$shim"
chmod 0755 "$shim"
