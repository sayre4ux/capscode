#!/usr/bin/env bash
# Tests for install.sh's binary path against a local file:// mirror of a release.
# No network, no Docker build, no keychain: every run uses a temporary HOME and
# bin directory and passes --skip-images --skip-init --skip-doctor.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/test-install.sh [--binary PATH]

--binary PATH  The file to serve as the release binary. It must print
               "capscode <workspace version>" for --version. Defaults to
               target/debug/capscode when it exists; on Linux a small stand-in
               script is used otherwise. On macOS the binary must carry a valid
               code signature (a cargo build does).
USAGE
}

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
binary=
while (($#)); do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --binary)
      (($# >= 2)) || { usage >&2; exit 2; }
      binary=$2
      shift 2
      ;;
    *) usage >&2; exit 2 ;;
  esac
done

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -1)
case "$(uname -s)/$(uname -m)" in
  Darwin/*) platform=macos-universal ;;
  Linux/x86_64) platform=linux-x86_64 ;;
  Linux/aarch64|Linux/arm64) platform=linux-aarch64 ;;
  *) printf 'skip: no release platform for this host\n'; exit 0 ;;
esac

tmp=$(mktemp -d "${TMPDIR:-/tmp}/capscode-install-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
if [[ -z "$binary" ]]; then
  if [[ -x "$root/target/debug/capscode" ]]; then
    binary="$root/target/debug/capscode"
  elif [[ "$(uname -s)" == Linux ]]; then
    binary="$tmp/stand-in"
    # The stand-in's own "$1" must stay literal.
    # shellcheck disable=SC2016
    printf '#!/bin/sh\n[ "$1" = --version ] && echo "capscode %s"\n' "$version" >"$binary"
    chmod 755 "$binary"
  else
    printf 'test-install: build the CLI first (cargo build -p capscode) or pass --binary\n' >&2
    exit 2
  fi
fi

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

passes=0
failures=0
pass() { passes=$((passes + 1)); printf 'ok   %s\n' "$1"; }
fail() { failures=$((failures + 1)); printf 'FAIL %s\n' "$1" >&2; }

binary_name="capscode-$version-$platform"
context_name="capscode-$version-build-context.tar.gz"
top="capscode-$version-build-context"
"$root/scripts/release/build-context.sh" --out "$tmp/good-context.tar.gz" >/dev/null

# A fresh mirror: <mirror>/v<version>/<assets> and <mirror>/channel.json.
# MIRROR_BINARY replaces the release binary (default: the binary under test).
make_mirror() {
  local mirror=$1 channel_version=${2:-$version}
  rm -rf "$mirror"
  mkdir -p "$mirror/v$version"
  cp "${MIRROR_BINARY:-$binary}" "$mirror/v$version/$binary_name"
  cp "$tmp/good-context.tar.gz" "$mirror/v$version/$context_name"
  write_sums "$mirror"
  printf '{\n  "schema": 1,\n  "cli": {\n    "version": "%s",\n    "tag": "v%s",\n    "yanked_versions": []\n  },\n  "toolchain": {"sequence": 1}\n}\n' \
    "$channel_version" "$channel_version" >"$mirror/channel.json"
}

write_sums() {
  local dir="$1/v$version"
  {
    printf '%s  %s\n' "$(sha256 "$dir/$binary_name")" "$binary_name"
    printf '%s  %s\n' "$(sha256 "$dir/$context_name")" "$context_name"
  } >"$dir/SHA256SUMS"
}

# Run the installer with a temporary HOME; extra arguments go to install.sh.
# Sets: out (combined output), status.
run_install() {
  local case_dir=$1
  shift
  mkdir -p "$case_dir/home"
  set +e
  out=$(cd "$case_dir" && env -u CAPSCODE_HOME HOME="$case_dir/home" \
    CAPSCODE_RELEASE_BASE_URL="file://$tmp/mirror" PATH="${TEST_PATH:-$PATH}" \
    bash "$root/scripts/install.sh" --binary --bin-dir "$case_dir/bin" \
    --skip-images --skip-init --skip-doctor "$@" 2>&1)
  status=$?
  set -e
}

assert_refused() {
  local name=$1 case_dir=$2 want=$3
  if ((status == 0)); then
    fail "$name: installer succeeded"
  elif [[ "$out" != *"$want"* ]]; then
    fail "$name: missing '$want' in output: $out"
  elif [[ -e "$case_dir/bin/capscode" ]]; then
    fail "$name: a binary was installed"
  elif [[ -e "$case_dir/home/.capscode/build/$version" ]]; then
    fail "$name: a build context was installed"
  else
    pass "$name"
  fi
}

assert_installed() {
  local name=$1 case_dir=$2
  local installed="$case_dir/bin/capscode" context="$case_dir/home/.capscode/build/$version"
  if ((status != 0)); then
    fail "$name: installer failed: $out"
  elif [[ ! -f "$installed" || -L "$installed" || ! -x "$installed" ]]; then
    fail "$name: $installed is not an executable file"
  elif ! cmp -s "$installed" "$binary"; then
    fail "$name: installed binary differs from the release binary"
  elif [[ "$(CAPSCODE_NO_UPDATE_CHECK=1 "$installed" --version)" != "capscode $version" ]]; then
    fail "$name: installed binary does not report $version"
  elif [[ ! -x "$context/scripts/build-images.sh" || ! -f "$context/images/gateway.Dockerfile" ]]; then
    fail "$name: build context missing under $context"
  elif [[ "$(find "$case_dir/bin" "$case_dir/home/.capscode/build" -name '.*' -mindepth 1 | wc -l | tr -d ' ')" != 0 ]]; then
    fail "$name: temporary files left behind"
  elif [[ -z "$(find "$case_dir/home/.capscode" -maxdepth 0 -perm 700)" ]]; then
    fail "$name: ~/.capscode is not private"
  else
    pass "$name"
  fi
}

# ---- installs ---------------------------------------------------------------

make_mirror "$tmp/mirror"
run_install "$tmp/c-version" --version "$version"
assert_installed "installs the requested version" "$tmp/c-version"

run_install "$tmp/c-latest"
assert_installed "installs the channel's newest version" "$tmp/c-latest"

# Re-running updates in place and replaces the old build context whole.
touch "$tmp/c-latest/home/.capscode/build/$version/stale"
run_install "$tmp/c-latest"
assert_installed "a re-run updates in place" "$tmp/c-latest"
if [[ ! -e "$tmp/c-latest/home/.capscode/build/$version/stale" ]]; then
  pass "a re-run replaces the build context"
else
  fail "a re-run kept files from the old build context"
fi

# Run outside a checkout (as with curl | bash), without --binary: binary mode.
mkdir -p "$tmp/c-pipe/home"
set +e
out=$(cd "$tmp/c-pipe" && env -u CAPSCODE_HOME HOME="$tmp/c-pipe/home" \
  CAPSCODE_RELEASE_BASE_URL="file://$tmp/mirror" \
  bash -s -- --bin-dir "$tmp/c-pipe/bin" --skip-images --skip-init --skip-doctor <"$root/scripts/install.sh" 2>&1)
status=$?
set -e
assert_installed "piped into bash it installs the binary" "$tmp/c-pipe"

# An earlier source install's symlink is replaced; its target is left alone.
mkdir -p "$tmp/c-link/bin" "$tmp/c-link/checkout"
printf 'source build\n' >"$tmp/c-link/checkout/capscode"
ln -s "$tmp/c-link/checkout/capscode" "$tmp/c-link/bin/capscode"
run_install "$tmp/c-link"
assert_installed "replaces a source-install symlink" "$tmp/c-link"
if [[ "$(cat "$tmp/c-link/checkout/capscode")" == "source build" ]]; then
  pass "the symlink target is untouched"
else
  fail "the source build was overwritten"
fi

# ---- refusals -------------------------------------------------------------------

make_mirror "$tmp/mirror"
printf 'x' >>"$tmp/mirror/v$version/$binary_name"
run_install "$tmp/r-tampered" --version "$version"
assert_refused "a tampered binary is refused" "$tmp/r-tampered" "does not match its SHA-256 in SHA256SUMS"

# A tampered binary must not replace a working install either.
mkdir -p "$tmp/r-keep/bin"
printf 'old\n' >"$tmp/r-keep/bin/capscode"
run_install "$tmp/r-keep" --version "$version"
if [[ $status -ne 0 && "$(cat "$tmp/r-keep/bin/capscode")" == old ]]; then
  pass "a refused install leaves the old binary"
else
  fail "a refused install changed the old binary"
fi

make_mirror "$tmp/mirror"
printf 'x' >>"$tmp/mirror/v$version/$context_name"
run_install "$tmp/r-context" --version "$version"
assert_refused "a tampered build context is refused" "$tmp/r-context" "does not match its SHA-256 in SHA256SUMS"

make_mirror "$tmp/mirror"
grep -v " $binary_name\$" "$tmp/mirror/v$version/SHA256SUMS" >"$tmp/sums" && mv "$tmp/sums" "$tmp/mirror/v$version/SHA256SUMS"
run_install "$tmp/r-missing" --version "$version"
assert_refused "a binary missing from SHA256SUMS is refused" "$tmp/r-missing" "no single entry for $binary_name"

make_mirror "$tmp/mirror"
printf '%s  %s\n' "$(printf '0%.0s' {1..64})" "$binary_name" >>"$tmp/mirror/v$version/SHA256SUMS"
run_install "$tmp/r-duplicate" --version "$version"
assert_refused "a duplicated SHA256SUMS entry is refused" "$tmp/r-duplicate" "no single entry for $binary_name"

make_mirror "$tmp/mirror"
rm "$tmp/mirror/v$version/SHA256SUMS"
run_install "$tmp/r-nosums" --version "$version"
assert_refused "a release without SHA256SUMS is refused" "$tmp/r-nosums" "download failed"

make_mirror "$tmp/mirror"
run_install "$tmp/r-pre" --version "$version-rc.1"
assert_refused "a prerelease version is refused" "$tmp/r-pre" "not a plain release version"

make_mirror "$tmp/mirror" "$version-rc.1"
run_install "$tmp/r-pre-channel"
assert_refused "a prerelease in the channel is refused" "$tmp/r-pre-channel" "not a plain release version"

make_mirror "$tmp/mirror"
set +e
out=$(env HOME="$tmp/home-http" CAPSCODE_RELEASE_BASE_URL="http://127.0.0.1:9/mirror" \
  bash "$root/scripts/install.sh" --binary --bin-dir "$tmp/r-http/bin" --skip-images --skip-init --skip-doctor 2>&1)
status=$?
set -e
if [[ $status -eq 2 && "$out" == *"https:// or file://"* && ! -e "$tmp/r-http/bin/capscode" ]]; then
  pass "a plain-http mirror is refused"
else
  fail "a plain-http mirror was not refused: $out"
fi

if [[ "$(uname -s)" == Darwin ]]; then
  make_mirror "$tmp/mirror"
  codesign --remove-signature "$tmp/mirror/v$version/$binary_name"
  write_sums "$tmp/mirror"
  run_install "$tmp/r-unsigned" --version "$version"
  assert_refused "an unsigned macOS binary is refused" "$tmp/r-unsigned" "invalid code signature"
fi

# Hostile build contexts whose checksums match: the structure check refuses them.
evil_context() {
  local kind=$1 dir="$tmp/evil-$1"
  rm -rf "$dir"
  mkdir -p "$dir/$top/scripts"
  printf '#!/bin/sh\n' >"$dir/$top/scripts/build-images.sh"
  printf 'x\n' >"$dir/escaped"
  case "$kind" in
    dotdot) (cd "$dir" && tar -czPf "$dir.tar.gz" "$top" "$top/../escaped") ;;
    absolute) (cd "$dir" && tar -czPf "$dir.tar.gz" "$top" "$dir/escaped") ;;
    symlink)
      ln -s /etc "$dir/$top/scripts/outside"
      (cd "$dir" && tar -czf "$dir.tar.gz" "$top")
      ;;
  esac
  make_mirror "$tmp/mirror"
  cp "$dir.tar.gz" "$tmp/mirror/v$version/$context_name"
  write_sums "$tmp/mirror"
}
for kind in dotdot absolute symlink; do
  evil_context "$kind"
  run_install "$tmp/r-$kind" --version "$version"
  case "$kind" in
    dotdot) want="entry with '..'" ;;
    absolute) want="entry outside $top/" ;;
    symlink) want="links or special files" ;;
  esac
  assert_refused "a build context with a $kind entry is refused" "$tmp/r-$kind" "$want"
  [[ ! -e "$tmp/r-$kind/home/.capscode/escaped" && ! -e "$tmp/r-$kind/home/.capscode/build/escaped" ]] ||
    fail "a $kind entry escaped"
done

# ---- the release channel ------------------------------------------------------

# Replace the mirror's channel.json with $1.
write_channel() {
  printf '%s\n' "$1" >"$tmp/mirror/channel.json"
}

# Fields are found by their place in the document: a `version` elsewhere, or a
# nested object inside `cli`, does not confuse the reader.
make_mirror "$tmp/mirror"
write_channel "{\"toolchain\": {\"version\": \"9.9.9\", \"sequence\": 1},
  \"cli\": {\"notes\": {\"version\": \"8.8.8\"}, \"tag\": \"v$version\", \"yanked_versions\": [\"0.0.1\"],
  \"version\": \"$version\"}, \"schema\": 1}"
run_install "$tmp/c-channel-order"
assert_installed "the channel's cli.version is read by its place in the JSON" "$tmp/c-channel-order"

channel_case() {
  local name=$1 want=$2 body=$3
  shift 3
  make_mirror "$tmp/mirror"
  write_channel "$body"
  run_install "$tmp/r-channel-$name" "$@"
  assert_refused "channel: $name is refused" "$tmp/r-channel-$name" "$want"
}
good_cli="\"version\": \"$version\", \"tag\": \"v$version\""
channel_case "schema 2" "its schema is not 1" \
  "{\"schema\": 2, \"cli\": {$good_cli, \"yanked_versions\": []}}"
channel_case "schema 1.0" "its schema is not 1" \
  "{\"schema\": 1.0, \"cli\": {$good_cli, \"yanked_versions\": []}}"
channel_case "a tag that is not v<version>" "cli.tag is not v<cli.version>" \
  "{\"schema\": 1, \"cli\": {\"version\": \"$version\", \"tag\": \"v9.9.9\", \"yanked_versions\": []}}"
channel_case "a withdrawn newest version" "also lists it as withdrawn" \
  "{\"schema\": 1, \"cli\": {$good_cli, \"yanked_versions\": [\"0.0.1\", \"$version\"]}}"
channel_case "a withdrawn --version" "CapsCode $version was withdrawn" \
  "{\"schema\": 1, \"cli\": {\"version\": \"9.9.9\", \"tag\": \"v9.9.9\", \"yanked_versions\": [\"$version\"]}}" \
  --version "$version"
channel_case "a duplicated cli.version" "more than one cli.version" \
  "{\"schema\": 1, \"cli\": {\"version\": \"9.9.9\", $good_cli, \"yanked_versions\": []}}"
channel_case "an escaped key" "a key contains an escape" \
  "{\"schema\": 1, \"cli\": {$good_cli, \"yanked_versions\": [], \"vers\\u0069on\": \"9.9.9\"}}"
channel_case "an invalid escape in an ignored field" "invalid JSON escape" \
  "{\"schema\": 1, \"cli\": {$good_cli, \"yanked_versions\": []}, \"extra\": \"\\x\"}"
channel_case "an escaped version" "cli.version is missing or not a plain string" \
  "{\"schema\": 1, \"cli\": {\"version\": \"0.1.\\u0030\", \"tag\": \"v0.1.\\u0030\", \"yanked_versions\": []}}"
channel_case "a missing yank list" "cli.yanked_versions is missing" \
  "{\"schema\": 1, \"cli\": {$good_cli}}"
channel_case "truncated JSON" "not usable" \
  "{\"schema\": 1, \"cli\": {$good_cli, \"yanked_versions\": []"
channel_case "text after the JSON" "text after the JSON object" \
  "{\"schema\": 1, \"cli\": {$good_cli, \"yanked_versions\": []}} {}"
channel_case "a version in a list" "cli.version is missing or not a plain string" \
  "{\"schema\": 1, \"cli\": {\"version\": [\"$version\"], \"tag\": \"v$version\", \"yanked_versions\": []}}"

# ---- build context size limits ------------------------------------------------

# Hostile build contexts whose checksums match: too large once unpacked, or too
# many entries. Both stay small as downloads.
oversize_context() {
  local kind=$1 dir="$tmp/over-$1"
  rm -rf "$dir"
  mkdir -p "$dir/$top/scripts"
  printf '#!/bin/sh\n' >"$dir/$top/scripts/build-images.sh"
  case "$kind" in
    bytes) head -c $((64 * 1024 * 1024 + 1)) /dev/zero >"$dir/$top/big" ;;
    entries) mkdir "$dir/$top/many" && (cd "$dir/$top/many" && seq 1 20000 | xargs touch) ;;
  esac
  (cd "$dir" && tar -czf "$dir.tar.gz" "$top")
  rm -rf "$dir"
  make_mirror "$tmp/mirror"
  cp "$dir.tar.gz" "$tmp/mirror/v$version/$context_name"
  write_sums "$tmp/mirror"
}
oversize_context bytes
run_install "$tmp/r-bytes" --version "$version"
assert_refused "a build context over 64 MiB unpacked is refused" "$tmp/r-bytes" "unpacks to more than 67108864 bytes"
oversize_context entries
run_install "$tmp/r-entries" --version "$version"
assert_refused "a build context with more than 20000 entries is refused" "$tmp/r-entries" "more than 20000 entries"

# ---- image build step --------------------------------------------------------------

# A stand-in release binary that records how the installer runs it, and a fake
# docker that only answers the prerequisite checks: nothing reaches a real daemon.
standin="$tmp/standin/capscode"
mkdir -p "$tmp/standin" "$tmp/fake-docker"
if [[ "$(uname -s)" == Darwin ]]; then
  if command -v cc >/dev/null 2>&1; then
    cat >"$tmp/standin/main.c" <<'C'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "--version") == 0) { printf("capscode %s\n", VERSION); return 0; }
  const char *log = getenv("STANDIN_LOG");
  FILE *out = log ? fopen(log, "a") : NULL;
  for (int i = 1; out && i < argc; i++) fprintf(out, i > 1 ? " %s" : "%s", argv[i]);
  if (out) { fputc('\n', out); fclose(out); }
  const char *code = getenv("STANDIN_EXIT");
  return code ? atoi(code) : 0;
}
C
    # The linker signs it ad hoc, which is what the installer checks on macOS.
    cc -DVERSION="\"$version\"" -o "$standin" "$tmp/standin/main.c" || standin=
  else
    standin=
  fi
else
  # The stand-in's own "$@" must stay literal.
  # shellcheck disable=SC2016
  printf '#!/bin/sh\n[ "$1" = --version ] && { echo "capscode %s"; exit 0; }\n[ -n "$STANDIN_LOG" ] && echo "$*" >>"$STANDIN_LOG"\nexit "${STANDIN_EXIT:-0}"\n' "$version" >"$standin"
  chmod 755 "$standin"
fi
cat >"$tmp/fake-docker/docker" <<'SH'
#!/bin/sh
case "$1" in
  info|buildx) exit 0 ;;
  version) echo 27.0.0; exit 0 ;;
esac
exit 1
SH
chmod 755 "$tmp/fake-docker/docker"

# Like run_install, but builds images (through the stand-in).
run_install_images() {
  local case_dir=$1
  shift
  mkdir -p "$case_dir/home"
  set +e
  out=$(cd "$case_dir" && env -u CAPSCODE_HOME HOME="$case_dir/home" \
    CAPSCODE_RELEASE_BASE_URL="file://$tmp/mirror" PATH="$tmp/fake-docker:$PATH" \
    STANDIN_LOG="$case_dir/standin-log" \
    bash "$root/scripts/install.sh" --binary --bin-dir "$case_dir/bin" \
    --skip-init --skip-doctor "$@" 2>&1)
  status=$?
  set -e
}

if [[ -z "$standin" ]]; then
  printf 'skip image build step: no C compiler for a signed stand-in binary\n'
else
  MIRROR_BINARY=$standin make_mirror "$tmp/mirror"
  run_install_images "$tmp/i-smoke" --version "$version"
  if ((status == 0)) && [[ "$(cat "$tmp/i-smoke/standin-log")" == "toolchain build" ]]; then
    pass "the image build runs \`capscode toolchain build\`"
  else
    fail "unexpected image build: status $status, log: $(cat "$tmp/i-smoke/standin-log" 2>/dev/null), output: $out"
  fi
  run_install_images "$tmp/i-nosmoke" --version "$version" --no-smoke
  if ((status == 0)) && [[ "$(cat "$tmp/i-nosmoke/standin-log")" == "toolchain build --no-smoke" ]]; then
    pass "--no-smoke reaches \`capscode toolchain build\`"
  else
    fail "--no-smoke was not passed through: status $status, log: $(cat "$tmp/i-nosmoke/standin-log" 2>/dev/null), output: $out"
  fi
  STANDIN_EXIT=1 run_install_images "$tmp/i-fail" --version "$version"
  if ((status == 1)) && [[ "$out" == *"building the container images failed"* && "$out" == *"toolchain build --no-smoke"* && "$out" == *"--skip-images"* ]]; then
    pass "a failed image build says how to retry or skip the smoke test"
  else
    fail "a failed image build gave no retry guidance: status $status, output: $out"
  fi
  STANDIN_EXIT=1 run_install_images "$tmp/i-fail-nosmoke" --version "$version" --no-smoke
  if ((status == 1)) && [[ "$out" == *"retry with \`$tmp/i-fail-nosmoke/bin/capscode toolchain build --no-smoke\`"* ]]; then
    pass "a failed --no-smoke build suggests retrying the same way"
  else
    fail "a failed --no-smoke build gave the wrong retry: status $status, output: $out"
  fi
fi

# ---- --verify-attestation -----------------------------------------------------------

make_mirror "$tmp/mirror"
fake_gh() {
  mkdir -p "$1"
  printf '#!/bin/sh\nprintf "%%s " "$@" >>"%s/gh-args"\nprintf "\\n" >>"%s/gh-args"\nexit %s\n' "$1" "$1" "$2" >"$1/gh"
  chmod 755 "$1/gh"
}
fake_gh "$tmp/gh-reject" 1
TEST_PATH="$tmp/gh-reject:$PATH" run_install "$tmp/a-reject" --version "$version" --verify-attestation
assert_refused "a rejected attestation is refused" "$tmp/a-reject" "gh attestation verify rejected"

fake_gh "$tmp/gh-accept" 0
TEST_PATH="$tmp/gh-accept:$PATH" run_install "$tmp/a-accept" --version "$version" --verify-attestation
assert_installed "an accepted attestation installs" "$tmp/a-accept"
if grep -q -- "--repo sayre4ux/capscode --signer-workflow sayre4ux/capscode/.github/workflows/release-cli.yml" "$tmp/gh-accept/gh-args" &&
  [[ "$(wc -l <"$tmp/gh-accept/gh-args" | tr -d ' ')" == 2 ]]; then
  pass "attestation checks pin the CLI release workflow for the binary and the build context"
else
  fail "unexpected gh arguments: $(cat "$tmp/gh-accept/gh-args")"
fi

# Without gh on PATH, --verify-attestation fails closed. Drop every PATH entry
# that holds a gh, unless that would drop the base system directories.
no_gh_path=
skip_no_gh=0
IFS=: read -r -a path_dirs <<<"$PATH"
for dir in "${path_dirs[@]:-}"; do
  if [[ -x "$dir/gh" ]]; then
    case "$dir" in /bin|/usr/bin) skip_no_gh=1 ;; esac
    continue
  fi
  no_gh_path=${no_gh_path:+$no_gh_path:}$dir
done
if ((skip_no_gh)); then
  printf 'skip --verify-attestation without gh: gh lives in a base system directory here\n'
else
  TEST_PATH=$no_gh_path run_install "$tmp/a-nogh" --version "$version" --verify-attestation
  assert_refused "--verify-attestation without gh is refused" "$tmp/a-nogh" "GitHub CLI (gh), needed by --verify-attestation"
fi

printf '\n%s passed, %s failed\n' "$passes" "$failures"
((failures == 0))
