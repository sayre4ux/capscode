#!/usr/bin/env bash
# Check `toolchain-release.sh channel`: it records the lock attestation bundle's
# SHA-256 in channel.json and refuses a bundle that does not cover the lock.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/release/test-toolchain-channel.sh

Run offline fixtures for the channel step of the toolchain release.
USAGE
}

if (($#)); then
  if (($# == 1)) && { [[ "$1" == --help || "$1" == -h ]]; }; then usage; exit 0; fi
  usage >&2
  exit 2
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
release="$root/scripts/release/toolchain-release.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/capscode-toolchain-channel-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# A sigstore bundle whose in-toto statement names `digest` as a subject.
bundle_for() {
  local digest=$1 statement
  statement=$(jq -cn --arg digest "$digest" \
    '{_type: "https://in-toto.io/Statement/v1", subject: [{name: "toolchain.lock.toml", digest: {sha256: $digest}}]}')
  jq -n --arg payload "$(printf '%s' "$statement" | base64 | tr -d '\n')" \
    '{mediaType: "application/vnd.dev.sigstore.bundle.v0.3+json", dsseEnvelope: {payload: $payload, payloadType: "application/vnd.in-toto+json", signatures: []}}'
}

out="$tmp/out"
mkdir -p "$out"
cp "$root/images/toolchain.lock.toml" "$out/toolchain.lock.toml"
lock_sha=$(sha256 "$out/toolchain.lock.toml")
"$root/scripts/release/channel-json.sh" \
  --cli-version 0.1.0 --cli-tag v0.1.0 --cli-commit 749dcba0c0e2aabb \
  --min-toolchain-sequence 1 \
  --notes-url https://github.com/sayre4ux/capscode/releases/tag/v0.1.0 \
  --toolchain-sequence 7 --toolchain-tag toolchain-7 --toolchain-digest fb24882402b5 \
  --min-cli-version 0.1.0 \
  --lock-url https://github.com/sayre4ux/capscode/releases/download/toolchain-7/toolchain.lock.toml \
  --lock-sha256 "$lock_sha" \
  --attestation-url https://github.com/sayre4ux/capscode/releases/download/toolchain-7/toolchain.lock.toml.sigstore \
  --toolchain-yanked '["codex@0.160.0"]' --generated-at 2026-10-18T06:17:00Z \
  --output "$out/channel.partial.json"

bundle_for "0000000000000000000000000000000000000000000000000000000000000000" >"$tmp/wrong.json"
if "$release" channel --bundle "$tmp/wrong.json" --out-dir "$out" >/dev/null 2>"$tmp/err"; then
  printf 'FAIL a bundle for another digest was accepted\n' >&2
  exit 1
fi
grep -q 'does not cover toolchain.lock.toml' "$tmp/err" || { cat "$tmp/err" >&2; exit 1; }
[[ ! -e "$out/channel.json" ]] || { printf 'FAIL channel.json written for a wrong bundle\n' >&2; exit 1; }

bundle_for "$lock_sha" >"$tmp/bundle.json"
"$release" channel --bundle "$tmp/bundle.json" --out-dir "$out" >/dev/null
bundle_sha=$(sha256 "$tmp/bundle.json")
jq -e --arg sha "$bundle_sha" --arg lock "$lock_sha" '
  .toolchain.attestation_sha256 == $sha and .toolchain.lock_sha256 == $lock and
  .generated_at == "2026-10-18T06:17:00Z" and .toolchain.yanked == ["codex@0.160.0"] and
  .cli.tag == "v0.1.0"
' "$out/channel.json" >/dev/null || { printf 'FAIL channel.json fields\n' >&2; cat "$out/channel.json" >&2; exit 1; }
cmp -s "$tmp/bundle.json" "$out/toolchain.lock.toml.sigstore" || { printf 'FAIL bundle not stored\n' >&2; exit 1; }
printf 'toolchain channel step: bundle hash recorded, foreign bundle refused\n'
