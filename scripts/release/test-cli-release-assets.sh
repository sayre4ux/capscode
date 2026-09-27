#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/capscode-cli-release-assets.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
out="$tmp/out"
mkdir -p "$out" "$tmp/bin"

tag=v0.1.0-rc.1
version=${tag#v}
assets=(
  "capscode-$version-src.tar.gz"
  "capscode-$version-build-context.tar.gz"
  "capscode-$version-release-notes.md"
  "capscode-$version-macos-universal"
  "capscode-$version-linux-x86_64"
  "capscode-$version-linux-aarch64"
)
for asset in "${assets[@]}"; do printf 'fixture %s\n' "$asset" >"$out/$asset"; done
printf 'release notes\n' >"$out/release-notes.md"
printf '{}\n' >"$out/channel.json"
printf 'bundle\n' >"$tmp/bundle.json"
(
  cd "$out"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${assets[@]}" | LC_ALL=C sort -k2 >SHA256SUMS
  else
    shasum -a 256 "${assets[@]}" | LC_ALL=C sort -k2 >SHA256SUMS
  fi
)

cat >"$tmp/bin/gh" <<'GH'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$1" == release && "$2" == view ]]; then
  if [[ "$*" == *"--json assets"* ]]; then printf '%s\n' "$EXISTING_ASSET"; fi
  exit 0
fi
printf '%s\n' "$*" >>"$GH_LOG"
GH
chmod +x "$tmp/bin/gh"

if output=$(PATH="$tmp/bin:$PATH" GH_TOKEN=fake GITHUB_REPOSITORY=example/capscode \
  EXISTING_ASSET="${assets[0]}" GH_LOG="$tmp/gh.log" \
  "$root/scripts/release/cli-release.sh" publish --tag "$tag" --bundle "$tmp/bundle.json" \
  --out-dir "$out" 2>&1); then
  printf 'FAIL an existing release asset was accepted\n%s\n' "$output" >&2
  exit 1
fi
[[ "$output" == *"already contains asset ${assets[0]}; refusing to replace it"* ]] || {
  printf 'FAIL wrong release-asset refusal: %s\n' "$output" >&2
  exit 1
}
[[ ! -s "$tmp/gh.log" ]] || { printf 'FAIL the script attempted to upload over an asset\n' >&2; exit 1; }

printf 'CLI release publication refuses an existing asset without uploading\n'
