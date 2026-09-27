#!/usr/bin/env bash
set -eu

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# Release binaries, one per platform; docs/release-assets.md names them.
PLATFORMS=(macos-universal linux-x86_64 linux-aarch64)

usage() {
  cat <<'USAGE'
Usage:
  scripts/release/cli-release.sh prepare --tag vVERSION --binaries-dir PATH
      [--out-dir PATH] [--dry-run]
  scripts/release/cli-release.sh publish --tag vVERSION --bundle PATH
      [--out-dir PATH] [--dry-run]

prepare enforces the changelog gate, checks that the checkout is the tagged
commit, creates the source tarball and the image build context, copies the
platform binaries from --binaries-dir, writes SHA256SUMS over every asset, and
(for a release that is not a prerelease) regenerates channel.json from the
current channel branch. publish attaches the assets and the attestation bundle
(--bundle, from the attest step) to the GitHub release and, except for a
prerelease, updates the channel branch.
USAGE
}

fail() {
  printf 'cli-release: %s\n' "$1" >&2
  exit 2
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}

mode=${1:-}
[[ -n "$mode" ]] || { usage >&2; exit 2; }
shift
if [[ "$mode" == --help || "$mode" == -h ]]; then usage; exit 0; fi

tag=
out_dir=release-assets
binaries_dir=
bundle=
dry_run=0
while (($#)); do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --tag|--out-dir|--binaries-dir|--bundle)
      (($# >= 2)) || fail "missing value for $1"
      case "$1" in
        --tag) tag=$2 ;;
        --out-dir) out_dir=$2 ;;
        --binaries-dir) binaries_dir=$2 ;;
        --bundle) bundle=$2 ;;
      esac
      shift 2
      ;;
    --dry-run) dry_run=1; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done

[[ "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] || fail 'tag must be v<SemVer>'
version=${tag#v}
[[ "$version" != *+* ]] || fail 'build metadata is not allowed in a release tag'
# DECISION: a prerelease tag (v1.2.0-rc.1) gets a GitHub prerelease with every asset
# but does not touch channel.json, so the passive check never announces it and
# `capscode update --cli`, which installs plain vX.Y.Z releases only, keeps
# offering the newest stable release.
prerelease=0
[[ "$version" != *-* ]] || prerelease=1
binary_assets=()
for platform in "${PLATFORMS[@]}"; do
  binary_assets+=("capscode-$version-$platform")
done
context_asset="capscode-$version-build-context.tar.gz"
notes_asset="capscode-$version-release-notes.md"
source_asset_name="capscode-$version-src.tar.gz"
bundle_asset="capscode-$version-attestation.sigstore.json"

if ((dry_run)); then
  case "$mode" in
    prepare)
      scripts/release/changelog-section.sh "$version" >/dev/null
      printf 'DRY-RUN: require CHANGELOG.md section %s and a clean checkout of %s\n' "$version" "$tag"
      printf 'DRY-RUN: create %s, %s, and %s\n' "$source_asset_name" "$context_asset" "$notes_asset"
      printf 'DRY-RUN: copy %s from the binaries directory\n' "${binary_assets[*]}"
      printf 'DRY-RUN: write SHA256SUMS over every asset\n'
      if ((prerelease)); then
        printf 'DRY-RUN: prerelease; channel.json is left unchanged\n'
      else
        printf 'DRY-RUN: regenerate channel.json\n'
      fi
      ;;
    publish)
      printf 'DRY-RUN: attach the assets, SHA256SUMS, and %s to %s\n' "$bundle_asset" "$tag"
      if ((prerelease)); then
        printf 'DRY-RUN: mark %s as a prerelease and leave channel.json unchanged\n' "$tag"
      else
        printf 'DRY-RUN: publish channel.json\n'
      fi
      ;;
    *) fail "unknown mode '$mode'" ;;
  esac
  exit 0
fi

cd "$ROOT"
[[ "$out_dir" = /* ]] || out_dir="$ROOT/$out_dir"
case "$mode" in
  prepare)
    cargo_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
    [[ "$version" == "$cargo_version" ]] || fail "tag version $version does not match Cargo.toml version $cargo_version"
    commit=$(git rev-parse "$tag^{commit}") || fail "tag not found locally: $tag"
    # The build context is packaged from the working tree, so it must be the tag.
    [[ "$(git rev-parse HEAD)" == "$commit" ]] || fail "the checkout is not at $tag"
    [[ -z "$(git status --porcelain --untracked-files=normal)" ]] || fail 'the checkout has local changes'
    [[ -n "$binaries_dir" && -d "$binaries_dir" ]] || fail '--binaries-dir must name the directory holding the platform binaries'
    mkdir -p "$out_dir"
    scripts/release/changelog-section.sh "$version" >"$out_dir/release-notes.md"
    cp "$out_dir/release-notes.md" "$out_dir/$notes_asset"
    source_asset="$out_dir/$source_asset_name"
    git archive --format=tar.gz --prefix="capscode-$version-src/" --output="$source_asset" "$tag"
    scripts/release/build-context.sh --version "$version" --out "$out_dir/$context_asset" >/dev/null
    for asset in "${binary_assets[@]}"; do
      [[ -f "$binaries_dir/$asset" ]] || fail "missing binary $binaries_dir/$asset"
      cp "$binaries_dir/$asset" "$out_dir/$asset"
      chmod 755 "$out_dir/$asset"
    done
    (
      cd "$out_dir"
      sha256_of "$source_asset_name" "$context_asset" "$notes_asset" "${binary_assets[@]}" | LC_ALL=C sort -k2 >SHA256SUMS
    )

    if ((prerelease)); then
      printf 'Prepared prerelease %s in %s (channel.json unchanged)\n' "$tag" "$out_dir"
      exit 0
    fi
    if ! git show-ref --verify --quiet refs/remotes/origin/channel; then
      if git ls-remote --exit-code --heads origin channel >/dev/null 2>&1; then
        git fetch --no-tags origin refs/heads/channel:refs/remotes/origin/channel
      else
        # DECISION: toolchain release initializes the shared manifest; CLI release preserves its lock metadata.
        fail 'channel branch is not initialized; publish a toolchain release first'
      fi
    fi
    channel_file=$(mktemp "${TMPDIR:-/tmp}/capscode-channel.XXXXXX")
    trap 'rm -f "$channel_file"' EXIT
    git show origin/channel:channel.json >"$channel_file"
    repo=${GITHUB_REPOSITORY:-sayre4ux/capscode}
    cli_commit=$(git rev-parse "$commit")
    min_toolchain=$(jq -r '.cli.min_toolchain_sequence' "$channel_file")
    cli_yanked=$(jq -c '.cli.yanked_versions' "$channel_file")
    toolchain_sequence=$(jq -r '.toolchain.sequence' "$channel_file")
    toolchain_tag=$(jq -r '.toolchain.tag' "$channel_file")
    toolchain_digest=$(jq -r '.toolchain.digest' "$channel_file")
    min_cli_version=$(jq -r '.toolchain.min_cli_version' "$channel_file")
    lock_url=$(jq -r '.toolchain.lock_url' "$channel_file")
    lock_sha256=$(jq -r '.toolchain.lock_sha256' "$channel_file")
    attestation_url=$(jq -r '.toolchain.attestation_url' "$channel_file")
    attestation_sha256=$(jq -r '.toolchain.attestation_sha256 // ""' "$channel_file")
    toolchain_yanked=$(jq -c '.toolchain.yanked' "$channel_file")
    attestation_args=()
    [[ -z "$attestation_sha256" ]] || attestation_args=(--attestation-sha256 "$attestation_sha256")
    scripts/release/channel-json.sh \
      --cli-version "$version" --cli-tag "$tag" --cli-commit "$cli_commit" \
      --min-toolchain-sequence "$min_toolchain" \
      --notes-url "https://github.com/$repo/releases/tag/$tag" \
      --toolchain-sequence "$toolchain_sequence" --toolchain-tag "$toolchain_tag" \
      --toolchain-digest "$toolchain_digest" --min-cli-version "$min_cli_version" \
      --lock-url "$lock_url" --lock-sha256 "$lock_sha256" \
      --attestation-url "$attestation_url" ${attestation_args[@]+"${attestation_args[@]}"} --cli-yanked "$cli_yanked" \
      --toolchain-yanked "$toolchain_yanked" --output "$out_dir/channel.json"
    printf 'Prepared CLI release %s in %s\n' "$tag" "$out_dir"
    ;;
  publish)
    [[ -n "${GITHUB_REPOSITORY:-}" ]] || fail 'GITHUB_REPOSITORY is required'
    [[ -n "${GH_TOKEN:-}" ]] || fail 'GH_TOKEN is required'
    [[ -n "$bundle" && -f "$bundle" ]] || fail '--bundle must name the attestation bundle from the attest step'
    notes="$out_dir/release-notes.md"
    assets=()
    for asset in "$source_asset_name" "$context_asset" "$notes_asset" "${binary_assets[@]}" SHA256SUMS; do
      [[ -f "$out_dir/$asset" ]] || fail "prepared asset is missing: $asset"
      assets+=("$out_dir/$asset")
    done
    [[ -f "$notes" ]] || fail 'prepared release notes are missing'
    ((prerelease)) || [[ -f "$out_dir/channel.json" ]] || fail 'prepared channel.json is missing'
    # Every asset must still match the checksums users will verify against.
    (cd "$out_dir" && sha256_of -c SHA256SUMS >/dev/null) || fail 'an asset no longer matches SHA256SUMS'
    cp "$bundle" "$out_dir/$bundle_asset"
    assets+=("$out_dir/$bundle_asset")
    prerelease_args=()
    ((prerelease == 0)) || prerelease_args=(--prerelease)
    if gh release view "$tag" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
      # DECISION: published asset names are immutable; only missing assets may be attached.
      existing_assets=$(gh release view "$tag" --repo "$GITHUB_REPOSITORY" --json assets --jq '.assets[].name') ||
        fail "could not inspect assets already attached to $tag"
      for asset in "${assets[@]}"; do
        asset_name=${asset##*/}
        if grep -Fqx "$asset_name" <<<"$existing_assets"; then
          fail "release $tag already contains asset $asset_name; refusing to replace it"
        fi
      done
      gh release upload "$tag" "${assets[@]}" --repo "$GITHUB_REPOSITORY"
      gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --title "CapsCode $tag" --notes-file "$notes" ${prerelease_args[@]+"${prerelease_args[@]}"}
    else
      gh release create "$tag" "${assets[@]}" --repo "$GITHUB_REPOSITORY" \
        --title "CapsCode $tag" --notes-file "$notes" ${prerelease_args[@]+"${prerelease_args[@]}"}
    fi
    if ((prerelease == 0)); then
      scripts/release/publish-channel.sh --section cli --manifest "$out_dir/channel.json"
    fi
    ;;
  *) fail "unknown mode '$mode'" ;;
esac
