#!/usr/bin/env bash
# Produce capscode-<version>-build-context.tar.gz: exactly the files that
# scripts/build-images.sh needs, so a binary install can build the images without
# a checkout or a host Rust toolchain (the gateway image compiles the broker inside
# Docker). The CLI and install.sh unpack it to ~/.capscode/build/<version>/.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

usage() {
  cat <<'USAGE'
Usage: scripts/release/build-context.sh [--version VERSION] [--out PATH] [--list]

Package the image build context for a release. VERSION defaults to the
workspace version in Cargo.toml; PATH defaults to
release-assets/capscode-<VERSION>-build-context.tar.gz.

--list  Print the repository paths that would be packaged, one per line.

Files come from the working tree, limited to what git tracks or would track
(ignored files such as target/ or a stray WireGuard *.conf are never packaged).
Every entry sits under capscode-<VERSION>-build-context/ and is a regular file;
the script fails on a symlink.
USAGE
}

fail() {
  printf 'build-context: %s\n' "$1" >&2
  exit 2
}

# Repository paths the image builds read. Keep in sync with the Dockerfiles' COPY
# sources (capsule images use images/ as their context, the gateway uses the
# repository root) and the scripts build-images.sh runs;
# scripts/release/test-build-context.sh checks both.
CONTEXT_PATHS=(
  Cargo.toml
  Cargo.lock
  rust-toolchain.toml
  LICENSE
  crates
  images
  providers
  scripts/build-images.sh
  scripts/lib
  scripts/smoke-tui.sh
)

version=
out=
list_only=0
while (($#)); do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --version)
      (($# >= 2)) || fail 'missing value for --version'
      version=$2
      shift 2
      ;;
    --out)
      (($# >= 2)) || fail 'missing value for --out'
      out=$2
      shift 2
      ;;
    --list) list_only=1; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done

cd "$ROOT"
[[ -n "$version" ]] || version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$ ]] ||
  fail "version must be SemVer without build metadata, got '$version'"
top="capscode-$version-build-context"
[[ -n "$out" ]] || out="$ROOT/release-assets/$top.tar.gz"
[[ "$out" = /* ]] || out="$PWD/$out"

git rev-parse --is-inside-work-tree >/dev/null 2>&1 || fail 'run from a git checkout of CapsCode'
files=()
while IFS= read -r -d '' path; do
  # Tracked files deleted in the working tree are listed by --cached; skip them.
  if [[ -L "$path" ]]; then
    fail "refusing to package symlink $path"
  fi
  [[ -f "$path" ]] || continue
  files+=("$path")
done < <(git ls-files -z --cached --others --exclude-standard -- "${CONTEXT_PATHS[@]}" | LC_ALL=C sort -zu)
((${#files[@]} > 0)) || fail 'no files selected'
for required in Cargo.toml Cargo.lock scripts/build-images.sh images/toolchain.lock.toml; do
  [[ -f "$required" ]] || fail "missing $required"
done

if ((list_only)); then
  printf '%s\n' "${files[@]}"
  exit 0
fi

staging=$(mktemp -d "${TMPDIR:-/tmp}/capscode-build-context.XXXXXX")
trap 'rm -rf "$staging"' EXIT
list="$staging/files.txt"
: >"$list"
for path in "${files[@]}"; do
  dest="$staging/$top/$path"
  mkdir -p "$(dirname "$dest")"
  cp "$path" "$dest"
  if [[ -x "$path" ]]; then chmod 755 "$dest"; else chmod 644 "$dest"; fi
  # Fixed timestamps keep the archive reproducible from the same tree.
  touch -t 198001010000 "$dest"
  printf '%s\n' "$top/$path" >>"$list"
done

mkdir -p "$(dirname "$out")"
tmp_out=$(mktemp "$(dirname "$out")/.build-context.XXXXXX")
tar_flags=(--format=ustar)
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
  tar_flags+=(--owner=0 --group=0 --numeric-owner)
else
  tar_flags+=(--uid 0 --gid 0 --uname '' --gname '')
fi
# COPYFILE_DISABLE keeps macOS tar from adding AppleDouble (._*) entries.
(cd "$staging" && COPYFILE_DISABLE=1 tar "${tar_flags[@]}" -cf - -T "$list") | gzip -n -9 >"$tmp_out"
chmod 644 "$tmp_out"
mv -f "$tmp_out" "$out"
printf 'Wrote %s (%s files)\n' "$out" "${#files[@]}"
