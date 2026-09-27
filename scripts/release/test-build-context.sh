#!/usr/bin/env bash
# Check that the release build context holds every file the image builds read:
# each Dockerfile COPY source (in that Dockerfile's build context), the scripts
# build-images.sh runs, and the workspace manifests. Then unpack it and run its
# build-images.sh --help, which is how a binary install starts an image build.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/release/test-build-context.sh

Build the context tarball into a temporary directory and check its contents.
USAGE
}

if (($#)); then
  if (($# == 1)) && { [[ "$1" == --help || "$1" == -h ]]; }; then usage; exit 0; fi
  usage >&2
  exit 2
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/capscode-build-context-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
failures=0
fail() {
  printf 'FAIL %s\n' "$1" >&2
  failures=$((failures + 1))
}

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -1)
top="capscode-$version-build-context"
archive="$tmp/$top.tar.gz"
"$root/scripts/release/build-context.sh" --out "$archive" >/dev/null
tar -tzf "$archive" >"$tmp/names.txt"
tar -tvzf "$archive" >"$tmp/long.txt"

while IFS= read -r name; do
  case "$name" in
    "$top"/*) ;;
    *) fail "entry outside $top/: $name" ;;
  esac
  case "/$name/" in
    */../*|*/./*) fail "entry with a dot component: $name" ;;
  esac
  case "$name" in
    */target/*|*/.git/*|*/.git|*.conf) [[ "$name" == "$top/images/tmux.conf" ]] || fail "unexpected entry: $name" ;;
  esac
done <"$tmp/names.txt"
# Only regular files: the first character of a long listing line is the type.
if grep -qv '^-' "$tmp/long.txt"; then
  fail "non-regular entries: $(grep -v '^-' "$tmp/long.txt" | head -3)"
fi

has_file() { grep -qxF "$top/$1" "$tmp/names.txt"; }
has_dir() { grep -q "^$top/$1/" "$tmp/names.txt"; }
require() {
  has_file "$1" || has_dir "${1%/}" || fail "missing $1 (needed by $2)"
}

for path in Cargo.toml Cargo.lock rust-toolchain.toml images/toolchain.lock.toml \
  scripts/build-images.sh scripts/lib/toolchain-lock.sh scripts/smoke-tui.sh; do
  require "$path" "the image build"
done

# Every script build-images.sh calls by its repository path.
while IFS= read -r script; do
  require "$script" "scripts/build-images.sh"
done < <(grep -o 'scripts/[A-Za-z0-9_-]*\.sh' "$root/scripts/build-images.sh" | sort -u)

# Every Dockerfile build-images.sh uses, and every COPY source in it. Capsule
# images build with images/ as the context; the gateway builds from the root.
# The per-tool line names `images/capsule.$tool.Dockerfile` literally; the loop
# after this one checks every tool's file, so one stand-in tool is enough here.
# shellcheck disable=SC2016
dockerfiles=$(grep -oE 'images/[A-Za-z0-9.$_{}-]+\.Dockerfile' "$root/scripts/build-images.sh" |
  sed 's/\$tool/claude/' | sort -u)
while IFS= read -r dockerfile; do
  require "$dockerfile" "scripts/build-images.sh"
  case "$dockerfile" in
    images/gateway.Dockerfile) context= ;;
    *) context=images/ ;;
  esac
  copies=0
  while IFS= read -r line; do
    # COPY [--flag ...] <src>... <dest>; stages copied with --from are not files.
    read -r -a words <<<"$line"
    sources=()
    for word in "${words[@]:1}"; do
      case "$word" in
        --from=*) sources=(); continue 2 ;;
        --*) ;;
        *) sources+=("$word") ;;
      esac
    done
    ((${#sources[@]} >= 2)) || { fail "cannot parse in $dockerfile: $line"; continue; }
    unset 'sources[${#sources[@]}-1]'
    for source in "${sources[@]}"; do
      copies=$((copies + 1))
      require "$context${source%/}" "$dockerfile"
    done
  done < <(grep -E '^[[:space:]]*COPY[[:space:]]' "$root/$dockerfile")
  if [[ "$dockerfile" == images/gateway.Dockerfile ]] && ((copies < 4)); then
    fail "found only $copies COPY sources in $dockerfile; the parser is out of date"
  fi
done <<<"$dockerfiles"
for tool in claude codex opencode pi omp; do
  require "images/capsule.$tool.Dockerfile" "scripts/build-images.sh"
done

# Executable scripts stay executable.
for script in scripts/build-images.sh scripts/smoke-tui.sh; do
  grep -qE "^-rwxr-xr-x .* $top/$script\$" "$tmp/long.txt" || fail "$script is not executable in the archive"
done

mkdir "$tmp/unpacked"
tar -xzf "$archive" -C "$tmp/unpacked"
if ! (cd "$tmp" && bash "$tmp/unpacked/$top/scripts/build-images.sh" --help >"$tmp/help.txt" 2>&1); then
  fail "the unpacked scripts/build-images.sh --help failed: $(cat "$tmp/help.txt")"
fi
grep -q '^Usage: scripts/build-images.sh' "$tmp/help.txt" || fail 'unexpected build-images.sh --help output'
bash "$tmp/unpacked/$top/scripts/smoke-tui.sh" --help >/dev/null || fail 'the unpacked smoke-tui.sh --help failed'

# Reproducible from the same tree.
"$root/scripts/release/build-context.sh" --out "$tmp/again.tar.gz" >/dev/null
cmp -s "$archive" "$tmp/again.tar.gz" || fail 'two builds of the same tree differ'

if ((failures)); then
  printf '%s build-context check(s) failed\n' "$failures" >&2
  exit 1
fi
printf 'build-context: %s entries, all Dockerfile COPY sources present\n' "$(wc -l <"$tmp/names.txt" | tr -d ' ')"
