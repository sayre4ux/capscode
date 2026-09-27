#!/usr/bin/env bash
set -eu

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
LOCK_REL=images/toolchain.lock.toml

usage() {
  cat <<'USAGE'
Usage:
  scripts/release/toolchain-release.sh prepare [--lock PATH] [--out-dir PATH]
      [--digest HEX12] [--dry-run]
  scripts/release/toolchain-release.sh channel --bundle PATH [--out-dir PATH]
  scripts/release/toolchain-release.sh publish [--out-dir PATH] [--dry-run]

prepare validates the sequence, builds and smokes the lock, and assembles
the lock, the smoke report, and channel.partial.json (the manifest without the
attestation hash). --digest is for offline dry-runs. --lock is accepted for
dry-run fixtures; a live release uses the canonical lock.
channel runs after the lock is attested: it checks that the attestation bundle
(--bundle) covers the lock, stores it as toolchain.lock.toml.sigstore, and writes
channel.json with the bundle's SHA-256. It needs no network.
publish creates a new toolchain release and merges its section into channel.json.
The --dry-run paths need no GitHub credentials and make no network calls.
USAGE
}

fail() {
  printf 'toolchain-release: %s\n' "$1" >&2
  exit 2
}

top_value() {
  local key=$1
  awk -v key="$key" '
    /^\[/ { exit }
    $1 == key && $2 == "=" { sub(/^[^=]*=[[:space:]]*/, ""); print; exit }
  ' "$LOCK_PATH" | sed -E 's/^"([^"]*)"$/\1/'
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

latest_toolchain_sequence() {
  git -C "$ROOT" tag --list 'toolchain-*' |
    sed -n 's/^toolchain-\([0-9][0-9]*\)$/\1/p' |
    sort -n | tail -1
}

mode=${1:-}
[[ -n "$mode" ]] || { usage >&2; exit 2; }
shift
if [[ "$mode" == --help || "$mode" == -h ]]; then usage; exit 0; fi

lock_input=$LOCK_REL
out_dir=release-assets
digest_input=
bundle=
dry_run=0
while (($#)); do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --bundle)
      (($# >= 2)) || fail 'missing value for --bundle'
      bundle=$2
      shift 2
      ;;
    --lock)
      (($# >= 2)) || fail 'missing value for --lock'
      lock_input=$2
      shift 2
      ;;
    --out-dir)
      (($# >= 2)) || fail 'missing value for --out-dir'
      out_dir=$2
      shift 2
      ;;
    --digest)
      (($# >= 2)) || fail 'missing value for --digest'
      digest_input=$2
      shift 2
      ;;
    --dry-run) dry_run=1; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done

case "$mode" in
  prepare)
    cd "$ROOT"
    [[ "$lock_input" = /* ]] || lock_input="$ROOT/$lock_input"
    [[ "$out_dir" = /* ]] || out_dir="$ROOT/$out_dir"
    LOCK_PATH=$lock_input
    [[ -f "$LOCK_PATH" ]] || fail "lock not found: $LOCK_PATH"
    if ((!dry_run)) && [[ "$LOCK_PATH" != "$ROOT/$LOCK_REL" ]]; then
      # DECISION: smoke-tui.sh reads the canonical lock, so a live release must use it too.
      fail 'release builds and smoke only the canonical images/toolchain.lock.toml'
    fi
    sequence=$(top_value sequence)
    min_cli_version=$(top_value min_cli_version)
    yanked=$(top_value yanked)
    [[ "$sequence" =~ ^[0-9]+$ ]] || fail 'lock is missing an integer sequence field'
    [[ -n "$min_cli_version" ]] || fail 'lock is missing min_cli_version'
    [[ -n "$yanked" ]] || yanked='[]'
    yanked=$(printf '%s\n' "$yanked" | jq -ce 'if type == "array" and all(.[]; type == "string") then . else error("expected a string array") end') || fail 'lock yanked field must be an array of strings'
    previous=$(latest_toolchain_sequence)
    previous=${previous:-0}
    [[ "$previous" =~ ^[0-9]+$ ]] || fail 'could not read the latest local toolchain tag'
    # DECISION: release sequences must increase, but maintainers may skip a value when a release is withdrawn.
    if ((sequence <= previous)); then
      fail "lock sequence must be greater than the latest release ($previous); advance sequence in a PR before releasing (got $sequence)"
    fi
    tag="toolchain-$sequence"

    if ((dry_run)); then
      [[ "$digest_input" =~ ^[0-9a-f]{12}$ ]] || fail '--dry-run requires --digest with 12 lowercase hex characters'
      printf 'DRY-RUN: prepare %s from %s (digest %s)\n' "$tag" "$LOCK_PATH" "$digest_input"
      printf 'DRY-RUN: build and smoke images, then write lock, smoke report, and channel.json under %s\n' "$out_dir"
      exit 0
    fi

    mkdir -p "$out_dir"
    if [[ -n "$digest_input" ]]; then
      digest=$digest_input
    else
      build_log="$out_dir/image-build.log"
      if ! scripts/build-images.sh --no-smoke >"$build_log" 2>&1; then
        cat "$build_log" >&2
        fail 'image build failed'
      fi
      # DECISION: consume the composite tag printed by build-images.sh so its all_tools_digest function remains the single implementation.
      digest=$(sed -n 's/^==> capsule-all:\([0-9a-f]\{12\}\).*/\1/p' "$build_log" | tail -1)
      [[ "$digest" =~ ^[0-9a-f]{12}$ ]] || fail 'build-images.sh did not report the composite toolchain digest'
    fi

    smoke_report="$out_dir/smoke-report.txt"
    if ! scripts/smoke-tui.sh all >"$smoke_report" 2>&1; then
      cat "$smoke_report" >&2
      fail 'tool smoke failed'
    fi
    printf 'PASS composite image digest: %s\n' "$digest" >>"$smoke_report"
    cp "$LOCK_PATH" "$out_dir/toolchain.lock.toml"
    lock_sha256=$(sha256_file "$out_dir/toolchain.lock.toml")

    repo=${GITHUB_REPOSITORY:-sayre4ux/capscode}
    channel_file=
    if git -C "$ROOT" show-ref --verify --quiet refs/remotes/origin/channel; then
      channel_file=$(mktemp "${TMPDIR:-/tmp}/capscode-channel.XXXXXX")
      git -C "$ROOT" show origin/channel:channel.json >"$channel_file"
    elif git -C "$ROOT" ls-remote --exit-code --heads origin channel >/dev/null 2>&1; then
      git -C "$ROOT" fetch --no-tags origin refs/heads/channel:refs/remotes/origin/channel
      channel_file=$(mktemp "${TMPDIR:-/tmp}/capscode-channel.XXXXXX")
      git -C "$ROOT" show origin/channel:channel.json >"$channel_file"
    fi

    if [[ -n "$channel_file" ]]; then
      cli_version=$(jq -r '.cli.version' "$channel_file")
      cli_tag=$(jq -r '.cli.tag' "$channel_file")
      cli_commit=$(jq -r '.cli.commit' "$channel_file")
      min_toolchain=$(jq -r '.cli.min_toolchain_sequence' "$channel_file")
      notes_url=$(jq -r '.cli.notes_url' "$channel_file")
      cli_yanked=$(jq -c '.cli.yanked_versions' "$channel_file")
      rm -f "$channel_file"
    else
      # DECISION: the first toolchain release establishes the baseline CLI minimum until a channel exists.
      cli_tag=$(git -C "$ROOT" tag --list 'v*' --sort=-version:refname | head -1)
      if [[ -z "$cli_tag" ]]; then
        cli_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
        cli_tag="v$cli_version"
        cli_commit=$(git -C "$ROOT" rev-parse HEAD)
      else
        cli_version=${cli_tag#v}
        cli_commit=$(git -C "$ROOT" rev-parse "$cli_tag^{commit}")
      fi
      min_toolchain=$sequence
      notes_url="https://github.com/$repo/releases/tag/$cli_tag"
      cli_yanked='[]'
    fi

    lock_url="https://github.com/$repo/releases/download/$tag/toolchain.lock.toml"
    attestation_url="https://github.com/$repo/releases/download/$tag/toolchain.lock.toml.sigstore"
    scripts/release/channel-json.sh \
      --cli-version "$cli_version" --cli-tag "$cli_tag" --cli-commit "$cli_commit" \
      --min-toolchain-sequence "$min_toolchain" --notes-url "$notes_url" \
      --toolchain-sequence "$sequence" --toolchain-tag "$tag" --toolchain-digest "$digest" \
      --min-cli-version "$min_cli_version" --lock-url "$lock_url" --lock-sha256 "$lock_sha256" \
      --attestation-url "$attestation_url" --cli-yanked "$cli_yanked" --toolchain-yanked "$yanked" \
      --output "$out_dir/channel.partial.json"
    rm -f "$out_dir/channel.json"
    printf 'Prepared %s in %s\n' "$tag" "$out_dir"
    ;;
  channel)
    # The lock's attestation bundle exists only after the attest step, so the
    # manifest that records the bundle's hash is written here, between the lock
    # attestation and the channel.json attestation.
    [[ "$out_dir" = /* ]] || out_dir="$ROOT/$out_dir"
    partial="$out_dir/channel.partial.json"
    lock="$out_dir/toolchain.lock.toml"
    [[ -f "$partial" && -f "$lock" ]] || fail 'run prepare first: channel.partial.json or toolchain.lock.toml is missing'
    [[ -n "$bundle" && -f "$bundle" ]] || fail '--bundle must name the lock attestation bundle'
    lock_hash=$(sha256_file "$lock")
    [[ "$(jq -r '.toolchain.lock_sha256' "$partial")" == "$lock_hash" ]] || fail 'the prepared manifest does not match the lock'
    jq -e --arg digest "$lock_hash" \
      '.dsseEnvelope.payload | @base64d | fromjson | [.subject[].digest.sha256] | index($digest) != null' \
      "$bundle" >/dev/null 2>&1 || fail 'the attestation bundle does not cover toolchain.lock.toml'
    cp "$bundle" "$out_dir/toolchain.lock.toml.sigstore"
    bundle_hash=$(sha256_file "$out_dir/toolchain.lock.toml.sigstore")
    field() { jq -r "$1" "$partial"; }
    scripts/release/channel-json.sh \
      --generated-at "$(field .generated_at)" \
      --cli-version "$(field .cli.version)" --cli-tag "$(field .cli.tag)" \
      --cli-commit "$(field .cli.commit)" \
      --min-toolchain-sequence "$(field .cli.min_toolchain_sequence)" \
      --notes-url "$(field .cli.notes_url)" \
      --toolchain-sequence "$(field .toolchain.sequence)" --toolchain-tag "$(field .toolchain.tag)" \
      --toolchain-digest "$(field .toolchain.digest)" \
      --min-cli-version "$(field .toolchain.min_cli_version)" \
      --lock-url "$(field .toolchain.lock_url)" --lock-sha256 "$lock_hash" \
      --attestation-url "$(field .toolchain.attestation_url)" --attestation-sha256 "$bundle_hash" \
      --cli-yanked "$(jq -c .cli.yanked_versions "$partial")" \
      --toolchain-yanked "$(jq -c .toolchain.yanked "$partial")" \
      --output "$out_dir/channel.json"
    printf 'Wrote %s with attestation_sha256 %s\n' "$out_dir/channel.json" "$bundle_hash"
    ;;
  publish)
    [[ "$out_dir" = /* ]] || out_dir="$ROOT/$out_dir"
    if ((dry_run)); then
      printf 'DRY-RUN: publish toolchain release using assets in %s and update refs/heads/channel\n' "$out_dir"
      exit 0
    fi
    [[ -n "${GITHUB_REPOSITORY:-}" ]] || fail 'GITHUB_REPOSITORY is required'
    [[ -n "${GH_TOKEN:-}" ]] || fail 'GH_TOKEN is required'
    for asset in toolchain.lock.toml toolchain.lock.toml.sigstore smoke-report.txt channel.json; do
      [[ -f "$out_dir/$asset" ]] || fail "missing prepared asset: $out_dir/$asset"
    done
    [[ "$(jq -r '.toolchain.attestation_sha256 // ""' "$out_dir/channel.json")" == "$(sha256_file "$out_dir/toolchain.lock.toml.sigstore")" ]] ||
      fail 'channel.json does not record the attestation bundle; run the channel step after attesting the lock'

    sequence=$(jq -r '.toolchain.sequence' "$out_dir/channel.json")
    tag="toolchain-$sequence"
    repo=$GITHUB_REPOSITORY
    cd "$ROOT"

    release_target=$(git rev-parse HEAD)

    assets=("$out_dir/toolchain.lock.toml" "$out_dir/toolchain.lock.toml.sigstore" "$out_dir/smoke-report.txt")
    if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
      fail "release $tag already exists; existing releases and assets are never overwritten"
    fi
    gh release create "$tag" "${assets[@]}" --repo "$repo" --target "$release_target" \
      --title "CapsCode toolchain $sequence" --notes "Tested toolchain lock sequence $sequence." \
      --latest=false
    scripts/release/publish-channel.sh --section toolchain --manifest "$out_dir/channel.json"
    ;;
  *) fail "unknown mode '$mode'" ;;
esac
