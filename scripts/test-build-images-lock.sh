#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TEMP=$(mktemp -d "${TMPDIR:-/tmp}/capscode-build-lock-test.XXXXXX")
trap 'rm -rf -- "$TEMP"' EXIT
source "$ROOT/scripts/lib/toolchain-lock.sh"

cp "$ROOT/images/toolchain.lock.toml" "$TEMP/valid.lock.toml"
toolchain_validate_lock "$TEMP/valid.lock.toml"

rejects() {
  local name=$1 expression=$2
  cp "$TEMP/valid.lock.toml" "$TEMP/$name.lock.toml"
  perl -0pi -e "$expression" "$TEMP/$name.lock.toml"
  if toolchain_validate_lock "$TEMP/$name.lock.toml" >"$TEMP/$name.out" 2>&1; then
    echo "accepted invalid $name lock" >&2
    exit 1
  fi
}

rejects oversized-component 's/version = "0\.159\.3"/version = "1234567890123456789012345.0.0"/'
grep -Fq 'invalid SemVer' "$TEMP/oversized-component.out"
rejects u64-overflow-component 's/version = "0\.159\.3"/version = "18446744073709551616.0.0"/'
grep -Fq 'invalid SemVer' "$TEMP/u64-overflow-component.out"
cp "$TEMP/valid.lock.toml" "$TEMP/u64-max.lock.toml"
perl -0pi -e 's/version = "0\.159\.3"/version = "18446744073709551615.0.0"/' "$TEMP/u64-max.lock.toml"
toolchain_validate_lock "$TEMP/u64-max.lock.toml"
rejects malformed-integrity 's/sha512-/sha512-!/'
grep -Fq 'invalid sha512 integrity' "$TEMP/malformed-integrity.out"
# A backslash must reach the validator as-is, not be read as an escape sequence.
rejects backslash-version 's/version = "0\.159\.3"/version = "0.159\\.3"/'
grep -Fq 'invalid SemVer' "$TEMP/backslash-version.out"
rejects future-cutoff 's/2026-10-01T04:51:25Z/2099-10-01T04:51:25Z/'
grep -Fq 'not in the future' "$TEMP/future-cutoff.out"
rejects fractional-cutoff 's/2026-10-01T04:51:25Z/2026-10-01T04:51:25.1Z/'
grep -Fq 'not in the future' "$TEMP/fractional-cutoff.out"
rejects malformed-cutoff 's/2026-10-01T04:51:25Z/2026-1x-01T04:51:25Z/'
grep -Fq 'not in the future' "$TEMP/malformed-cutoff.out"
rejects unknown-package 's/"\@openai\/codex"/"\@example\/unknown"/'
grep -Fq 'invalid package' "$TEMP/unknown-package.out"

# Validation must work on binary-install hosts that do not have Node installed.
no_node_bin="$TEMP/no-node-bin"
mkdir -p "$no_node_bin"
for tool in awk sort date; do
  ln -s "$(command -v "$tool")" "$no_node_bin/$tool"
done
(
  PATH="$no_node_bin"
  hash -r
  if command -v node >/dev/null 2>&1; then
    echo 'the no-node validation fixture unexpectedly has Node in PATH' >&2
    exit 1
  fi
  toolchain_validate_lock "$TEMP/valid.lock.toml" || {
    echo 'lock validation failed with Node absent from PATH' >&2
    exit 1
  }
)

# An external --lock must not receive CI smoke metadata from the image build.
external_lock="$TEMP/external.lock.toml"
cp "$TEMP/valid.lock.toml" "$external_lock"
cp "$external_lock" "$TEMP/external.before.toml"
toolchain_record_smoke_for_lock \
  "$external_lock" "$ROOT/images/toolchain.lock.toml" playwright_core 2026-10-18 amd64
cmp -s "$TEMP/external.before.toml" "$external_lock" || {
  echo 'CI smoke recording changed the external lock' >&2
  exit 1
}

# Bash 3.2 treats an empty array as unset under `set -u`; keep the build's
# optional smoke arguments safe and exercise the exact expansion on macOS.
# shellcheck disable=SC2016
array_expansion='${record_args[@]+"${record_args[@]}"}'
[[ $(grep -Fc "$array_expansion" "$ROOT/scripts/build-images.sh") -eq 2 ]] || {
  echo 'build-images.sh must guard both optional record_args expansions' >&2
  exit 1
}
bash3_major=$(/bin/bash -c 'printf "%s" "${BASH_VERSINFO[0]}"' 2>/dev/null || true)
if [[ "$bash3_major" == 3 ]]; then
  /bin/bash -u -c '
    record_args=()
    check_empty() { [[ "$#" -eq 1 && "$1" == smoke ]]; }
    check_nonempty() { [[ "$#" -eq 2 && "$1" == smoke && "$2" == --record ]]; }
    check_empty smoke ${record_args[@]+"${record_args[@]}"}
    record_args=(--record)
    check_nonempty smoke ${record_args[@]+"${record_args[@]}"}
  '
else
  echo 'SKIP Bash 3.2 empty-array check: /bin/bash is not Bash 3.x'
fi

echo 'PASS build image lock validation, external lock recording, and optional smoke arguments'
