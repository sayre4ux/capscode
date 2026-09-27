#!/usr/bin/env bash
# Build one release binary of the capscode CLI and check it before it is uploaded.
#
#   macos-universal  arm64 + x86_64 joined with lipo, ad-hoc signed (no Developer ID)
#   linux-x86_64     glibc build on the oldest supported runner (see GLIBC_MAX)
#   linux-aarch64    the same on arm64
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

# DECISION: Linux binaries link glibc dynamically rather than musl statically. The
# CLI's C dependencies (aws-lc for TLS, libgit2, vendored libdbus for the Secret
# Service keyring) are built and tested against glibc in CI, and musl's allocator
# is slow under the async runtime. Building on Ubuntu 22.04 (glibc 2.35) and
# refusing any symbol newer than that keeps the binary running on Ubuntu 22.04+
# and Debian 12+ (glibc 2.36).
GLIBC_MAX=2.35
# Shared libraries a Linux binary may need; anything else would be missing on a
# minimal host.
ALLOWED_LIBS='^(libc|libm|libgcc_s|libpthread|libdl|librt|ld-linux-x86-64|ld-linux-aarch64)\.so'
SIGNING_IDENTIFIER=dev.capscode.cli

usage() {
  cat <<'USAGE'
Usage: scripts/release/build-binary.sh --platform PLATFORM --out-dir DIR
       [--version VERSION] [--target-dir DIR] [--dry-run]

PLATFORM is macos-universal, linux-x86_64, or linux-aarch64. Writes
DIR/capscode-<VERSION>-<PLATFORM>. VERSION defaults to Cargo.toml's version and
must match it. --target-dir defaults to a new temporary directory, so a local
run never replaces target/release/capscode (which a source install links to).
--dry-run prints the steps without building.
USAGE
}

fail() {
  printf 'build-binary: %s\n' "$1" >&2
  exit 2
}

platform=
out_dir=
version=
target_dir=
dry_run=0
while (($#)); do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --platform|--out-dir|--version|--target-dir)
      (($# >= 2)) || fail "missing value for $1"
      case "$1" in
        --platform) platform=$2 ;;
        --out-dir) out_dir=$2 ;;
        --version) version=$2 ;;
        --target-dir) target_dir=$2 ;;
      esac
      shift 2
      ;;
    --dry-run) dry_run=1; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done

cd "$ROOT"
cargo_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
[[ -n "$version" ]] || version=$cargo_version
[[ "$version" == "$cargo_version" ]] || fail "version $version does not match Cargo.toml version $cargo_version"
[[ -n "$out_dir" ]] || fail '--out-dir is required'
case "$platform" in
  macos-universal) targets=(aarch64-apple-darwin x86_64-apple-darwin); want_os=Darwin ;;
  linux-x86_64) targets=(x86_64-unknown-linux-gnu); want_os=Linux; want_arch=x86_64 ;;
  linux-aarch64) targets=(aarch64-unknown-linux-gnu); want_os=Linux; want_arch=aarch64 ;;
  *) fail "unknown platform '$platform' (macos-universal, linux-x86_64, linux-aarch64)" ;;
esac
asset="capscode-$version-$platform"

if ((dry_run)); then
  for target in "${targets[@]}"; do
    printf 'DRY-RUN: cargo build --locked --release -p capscode --target %s\n' "$target"
  done
  case "$platform" in
    macos-universal)
      printf 'DRY-RUN: lipo -create both slices -> %s; codesign --force --sign - --identifier %s; codesign --verify --strict\n' "$asset" "$SIGNING_IDENTIFIER"
      ;;
    *)
      printf 'DRY-RUN: copy -> %s; require glibc symbols <= %s and only base system libraries\n' "$asset" "$GLIBC_MAX"
      ;;
  esac
  printf 'DRY-RUN: run %s --version and expect "capscode %s"\n' "$asset" "$version"
  exit 0
fi

[[ "$(uname -s)" == "$want_os" ]] || fail "$platform must be built on $want_os"
if [[ -n "${want_arch:-}" ]]; then
  arch=$(uname -m)
  [[ "$arch" == "$want_arch" || ("$want_arch" == aarch64 && "$arch" == arm64) ]] ||
    fail "$platform must be built on a $want_arch host, not $arch"
fi
if [[ -z "$target_dir" ]]; then
  target_dir=$(mktemp -d "${TMPDIR:-/tmp}/capscode-release-target.XXXXXX")
fi
mkdir -p "$out_dir"
out="$out_dir/$asset"

built=()
for target in "${targets[@]}"; do
  CARGO_TARGET_DIR=$target_dir cargo build --locked --release -p capscode --target "$target"
  built+=("$target_dir/$target/release/capscode")
done

case "$platform" in
  macos-universal)
    lipo -create -output "$out" "${built[@]}"
    archs=$(lipo -archs "$out")
    [[ " $archs " == *" arm64 "* && " $archs " == *" x86_64 "* ]] || fail "universal binary has architectures '$archs'"
    # Ad-hoc signature: no certificate and no Apple Developer ID. The kernel
    # requires a valid signature on Apple silicon; Gatekeeper is not involved
    # because install.sh and `capscode update --cli` download without the
    # quarantine attribute.
    codesign --force --sign - --identifier "$SIGNING_IDENTIFIER" "$out"
    codesign --verify --strict --verbose=2 "$out"
    ;;
  linux-*)
    cp "${built[0]}" "$out"
    newest=$(objdump -T "$out" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sed 's/^GLIBC_//' | sort -V | tail -1)
    [[ -n "$newest" ]] || fail 'found no glibc symbol versions; is this a glibc binary?'
    [[ "$(printf '%s\n%s\n' "$newest" "$GLIBC_MAX" | sort -V | tail -1)" == "$GLIBC_MAX" ]] ||
      fail "binary needs glibc $newest, newer than the supported $GLIBC_MAX"
    while IFS= read -r library; do
      [[ "$library" =~ $ALLOWED_LIBS ]] || fail "binary links $library, which a minimal host may lack"
    done < <(readelf -d "$out" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
    printf 'glibc requirement: %s (supported: %s)\n' "$newest" "$GLIBC_MAX"
    ;;
esac
chmod 755 "$out"
reported=$(CAPSCODE_NO_UPDATE_CHECK=1 "$out" --version)
[[ "$reported" == "capscode $version" ]] || fail "binary reports '$reported', expected 'capscode $version'"
printf 'Built %s\n' "$out"
