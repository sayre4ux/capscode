#!/usr/bin/env bash
set -eu

usage() {
  cat <<'USAGE'
Usage: scripts/release/changelog-section.sh <version>

Print the Keep a Changelog section whose heading is exactly `## [<version>]`.
Exit non-zero when CHANGELOG.md has no matching section.
USAGE
}

if (($# == 1)) && { [[ "$1" == --help || "$1" == -h ]]; }; then
  usage
  exit 0
fi
if (($# != 1)); then
  usage >&2
  exit 2
fi

version=$1
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] || {
  printf 'Invalid changelog version: %s\n' "$version" >&2
  exit 2
}

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
file="$root/CHANGELOG.md"
tmp=$(mktemp "${TMPDIR:-/tmp}/capscode-changelog.XXXXXX")
trap 'rm -f "$tmp"' EXIT

awk -v heading="## [$version]" '
  !found && ($0 == heading || index($0, heading " ") == 1 || index($0, heading " -") == 1) {
    found = 1
    print
    next
  }
  found && /^## / { exit }
  found { print }
  END { if (!found) exit 1 }
' "$file" >"$tmp" || {
  printf 'No CHANGELOG.md section found for %s\n' "$version" >&2
  exit 1
}

cat "$tmp"
