#!/usr/bin/env bash
set -eu

usage() {
  cat <<'USAGE'
Usage: scripts/release/test-channel-json.sh

Run local fixtures and invalid-input checks for channel-json.sh.
USAGE
}

if (($#)); then
  if (($# == 1)) && { [[ "$1" == --help || "$1" == -h ]]; }; then usage; exit 0; fi
  usage >&2
  exit 2
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
generator="$root/scripts/release/channel-json.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/capscode-channel-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

args=(
  --cli-version 0.1.0
  --cli-tag v0.1.0
  --cli-commit 749dcba0c0e2aabb
  --min-toolchain-sequence 1
  --notes-url https://github.com/sayre4ux/capscode/releases/tag/v0.1.0
  --toolchain-sequence 7
  --toolchain-tag toolchain-7
  --toolchain-digest fb24882402b5
  --min-cli-version 0.1.0
  --lock-url https://github.com/sayre4ux/capscode/releases/download/toolchain-7/toolchain.lock.toml
  --lock-sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
  --attestation-url https://github.com/sayre4ux/capscode/releases/download/toolchain-7/toolchain.lock.toml.sigstore
  --cli-yanked '["0.0.9"]'
  --toolchain-yanked '["codex@0.160.0"]'
  --generated-at 2026-10-18T06:17:00Z
)

"$generator" "${args[@]}" --dry-run >"$tmp/actual.json"
cat >"$tmp/expected.json" <<'JSON'
{
  "schema": 1,
  "generated_at": "2026-10-18T06:17:00Z",
  "cli": {
    "version": "0.1.0",
    "tag": "v0.1.0",
    "commit": "749dcba0c0e2aabb",
    "min_toolchain_sequence": 1,
    "notes_url": "https://github.com/sayre4ux/capscode/releases/tag/v0.1.0",
    "yanked_versions": [
      "0.0.9"
    ]
  },
  "toolchain": {
    "sequence": 7,
    "tag": "toolchain-7",
    "digest": "fb24882402b5",
    "min_cli_version": "0.1.0",
    "lock_url": "https://github.com/sayre4ux/capscode/releases/download/toolchain-7/toolchain.lock.toml",
    "lock_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "attestation_url": "https://github.com/sayre4ux/capscode/releases/download/toolchain-7/toolchain.lock.toml.sigstore",
    "yanked": [
      "codex@0.160.0"
    ]
  }
}
JSON
diff -u "$tmp/expected.json" "$tmp/actual.json"

if "$generator" "${args[@]}" --toolchain-digest invalid --dry-run >"$tmp/bad-digest.out" 2>"$tmp/bad-digest.err"; then
  printf '%s\n' 'Expected invalid digest to be refused' >&2
  exit 1
fi
grep -F 'toolchain digest must be 12 lowercase hex characters' "$tmp/bad-digest.err" >/dev/null

if "$generator" "${args[@]}" --toolchain-sequence seven --dry-run >"$tmp/bad-sequence.out" 2>"$tmp/bad-sequence.err"; then
  printf '%s\n' 'Expected non-integer sequence to be refused' >&2
  exit 1
fi
grep -F 'toolchain sequence must be a positive integer' "$tmp/bad-sequence.err" >/dev/null

"$generator" "${args[@]}" --output "$tmp/written.json"
diff -u "$tmp/expected.json" "$tmp/written.json"

# Optional attestation hash and digest-form yank entries.
attestation_sha=fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210
"$generator" "${args[@]}" --attestation-sha256 "$attestation_sha" \
  --toolchain-yanked '["codex@0.160.0","6d07f8901234"]' --dry-run >"$tmp/extended.json"
jq -e --arg sha "$attestation_sha" \
  '.toolchain.attestation_sha256 == $sha and .toolchain.yanked == ["codex@0.160.0","6d07f8901234"]' \
  "$tmp/extended.json" >/dev/null
if "$generator" "${args[@]}" --attestation-sha256 not-hex --dry-run >/dev/null 2>"$tmp/bad-attestation.err"; then
  printf '%s\n' 'Expected an invalid attestation SHA-256 to be refused' >&2
  exit 1
fi
grep -F 'attestation SHA-256 must be 64 lowercase hex characters' "$tmp/bad-attestation.err" >/dev/null
if "$generator" "${args[@]}" --toolchain-yanked '["6D07F8901234"]' --dry-run >/dev/null 2>&1; then
  printf '%s\n' 'Expected an uppercase digest yank entry to be refused' >&2
  exit 1
fi
printf '%s\n' 'PASS channel-json fixture, validation refusals, and output write'
