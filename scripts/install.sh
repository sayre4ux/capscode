#!/usr/bin/env bash
# Install or update CapsCode: check prerequisites, put the CLI on PATH, build the
# container images, then run first-time setup.
#
# Two ways to get the CLI:
#   binary       download the release binary and the image build context, verify
#                both against the release's SHA256SUMS (and, with
#                --verify-attestation, GitHub's artifact attestation), and install
#                them atomically. No Rust toolchain needed.
#   from source  build the CLI from this checkout with cargo and link it.
#
# Run as `curl -fsSL https://raw.githubusercontent.com/sayre4ux/capscode/main/scripts/install.sh | bash`
# (binary) or `./scripts/install.sh` from a checkout (source). See --help.
#
# All the work happens in main(), called on the last line, so a download that is
# cut short runs nothing.
set -euo pipefail

REPO=sayre4ux/capscode
RELEASE_WORKFLOW=.github/workflows/release-cli.yml
MAX_BINARY_BYTES=67108864
MAX_CONTEXT_BYTES=16777216
MAX_SMALL_BYTES=65536
# The same limits `capscode update --cli` applies when it unpacks a build context.
MAX_UNPACKED_BYTES=67108864
MAX_CONTEXT_ENTRIES=20000

usage() {
  cat <<'EOF'
Usage: install.sh [options]

Checks prerequisites, installs the capscode CLI into a bin directory, builds the
container images, and runs `capscode init`. Safe to re-run; it updates in place.

How the CLI is installed:
  --binary              Download the release binary (the default when this script
                        is not run from a CapsCode checkout, e.g. `curl ... | bash`)
  --from-source         Build from this checkout with cargo (the default when run
                        as ./scripts/install.sh inside a checkout)
  --version VERSION     Binary install: install this release (e.g. 0.2.0) instead
                        of the newest one
  --verify-attestation  Binary install: also require `gh attestation verify` to
                        accept the binary and the build context (needs the GitHub
                        CLI, gh); fails if it cannot

The binary install reads the release channel (channel.json) and refuses a version
the channel lists as withdrawn, even one named with --version.

Other options:
  --check          Only check prerequisites; change nothing
  --bin-dir DIR    Where to put capscode (default: $HOME/.local/bin)
  --skip-images    Do not build container images
  --no-smoke       Build the images but skip the start-up smoke test of each tool
  --skip-init      Do not run `capscode init` (run it yourself later)
  --skip-doctor    Do not run `capscode doctor --quick` at the end
  --yes            Never prompt: run `capscode init --yes --no-completions`
  -h, --help       Show this help

Exit status: 0 success, 1 a prerequisite, download, verification, or step
failed, 2 bad arguments.
EOF
}

step() { printf '\n==> %s\n' "$*"; }
ok() { printf '  ok       %s\n' "$*"; }
bad() { printf '  MISSING  %s\n' "$*"; FAILED=1; }
warn() { printf '  warning  %s\n' "$*"; }
die() { printf '\nerror: %s\n' "$2" >&2; exit "$1"; }

# Compare dotted versions: succeed when $1 >= $2.
version_at_least() {
  [ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -n1)" = "$2" ]
}

# Free space in GB on the filesystem holding $1.
free_gb() { df -Pk "$1" 2>/dev/null | awk 'NR == 2 { print int($4 / 1048576) }'; }

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# The one SHA256SUMS line for asset $2 in file $1: `<64 lowercase hex>  <name>`.
expected_sha256() {
  awk -v name="$2" '
    NF == 2 && length($1) == 64 && $1 ~ /^[0-9a-f]+$/ && ($2 == name || $2 == "*" name) {
      found = $1; count++
    }
    END { if (count != 1) exit 1; print found }
  ' "$1"
}

# Read the fields the installer needs from channel.json ($1) with a small JSON
# parser: the whole document must be valid JSON, and the fields are found by their
# place in it, not by text matching. Prints `version <v>`, `tag <t>`, and one
# `yanked <v>` line per withdrawn CLI version; fails with a reason otherwise.
# Keys with escapes are refused, so a key cannot hide behind `\u` spelling, and
# each field the installer reads must appear exactly once.
read_channel() {
  if [ "$(tr -d '\000' <"$1" | wc -c)" -ne "$(wc -c <"$1")" ]; then
    echo "it contains NUL bytes"
    return 1
  fi
  LC_ALL=C awk '
    function fail(why) { print why; failed = 1; exit 1 }
    function skip_space(   c) {
      while (pos <= n) {
        c = substr(s, pos, 1)
        if (c != " " && c != "\t" && c != "\n" && c != "\r") return
        pos++
      }
    }
    function read_string(   out, c, hex) {
      if (substr(s, pos, 1) != "\"") fail("expected a string at byte " pos)
      pos++; out = ""; escaped = 0
      while (pos <= n) {
        c = substr(s, pos, 1)
        if (c == "\"") { pos++; return out }
        if (c < " ") fail("a string contains a control character")
        if (c == "\\") {
          escaped = 1; pos++; c = substr(s, pos, 1)
          if (c == "u") {
            hex = substr(s, pos + 1, 4)
            if (length(hex) != 4 || hex !~ /^[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]$/) fail("invalid JSON escape")
            out = out "\\" "u" hex; pos += 5; continue
          }
          if (c != "\"" && c != "\\" && c != "/" && c != "b" && c != "f" && c != "n" && c != "r" && c != "t") fail("invalid JSON escape")
          out = out "\\" c; pos++; continue
        }
        out = out c; pos++
      }
      fail("a string is not terminated")
    }
    function note(path, type, value) {
      if (path == ".schema" || path == ".cli" || path == ".cli.version" || path == ".cli.tag" || path == ".cli.yanked_versions") {
        if (path in seen) fail("it has more than one " substr(path, 2))
        seen[path] = 1; kind[path] = type; text[path] = value
      } else if (path == ".cli.yanked_versions[]") {
        if (type != "string") fail("cli.yanked_versions must hold strings")
        yanked[++yanked_count] = value
      }
    }
    function read_value(path, depth,   c, key, token) {
      if (depth > 32) fail("it is nested too deeply")
      skip_space()
      if (pos > n) fail("it ends early")
      c = substr(s, pos, 1)
      if (c == "{") {
        note(path, "object", ""); pos++; skip_space()
        if (substr(s, pos, 1) == "}") { pos++; return }
        while (1) {
          skip_space(); key = read_string()
          if (escaped) fail("a key contains an escape")
          skip_space()
          if (substr(s, pos, 1) != ":") fail("expected : at byte " pos)
          pos++
          read_value(path "." key, depth + 1)
          skip_space(); c = substr(s, pos, 1); pos++
          if (c == "}") return
          if (c != ",") fail("expected , or } at byte " (pos - 1))
        }
      }
      if (c == "[") {
        note(path, "array", ""); pos++; skip_space()
        if (substr(s, pos, 1) == "]") { pos++; return }
        while (1) {
          read_value(path "[]", depth + 1)
          skip_space(); c = substr(s, pos, 1); pos++
          if (c == "]") return
          if (c != ",") fail("expected , or ] at byte " (pos - 1))
        }
      }
      if (c == "\"") {
        token = read_string()
        note(path, escaped ? "escaped string" : "string", token)
        return
      }
      token = ""
      while (pos <= n) {
        c = substr(s, pos, 1)
        if (index("0123456789+-.eEtrufalsn", c) == 0) break
        token = token c; pos++
      }
      if (token == "true" || token == "false" || token == "null") { note(path, "literal", token); return }
      if (token ~ /^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?$/) { note(path, "number", token); return }
      fail("unexpected text at byte " pos)
    }
    { s = s $0 "\n" }
    END {
      if (failed) exit 1
      n = length(s); pos = 1
      skip_space()
      if (substr(s, pos, 1) != "{") fail("it is not a JSON object")
      read_value("", 0)
      skip_space()
      if (pos <= n) fail("it has text after the JSON object")
      if (kind[".schema"] != "number" || text[".schema"] != "1") fail("its schema is not 1")
      if (kind[".cli"] != "object") fail("it has no cli section")
      if (kind[".cli.version"] != "string") fail("cli.version is missing or not a plain string")
      if (kind[".cli.tag"] != "string") fail("cli.tag is missing or not a plain string")
      if (kind[".cli.yanked_versions"] != "array") fail("cli.yanked_versions is missing or not a list")
      if (text[".cli.tag"] != "v" text[".cli.version"]) fail("cli.tag is not v<cli.version>")
      print "version " text[".cli.version"]
      print "tag " text[".cli.tag"]
      for (i = 1; i <= yanked_count; i++) print "yanked " yanked[i]
    }
  ' "$1"
}

release_platform() {
  case "$(uname -s)/$(uname -m)" in
    Darwin/arm64|Darwin/x86_64) echo macos-universal ;;
    Linux/x86_64|Linux/amd64) echo linux-x86_64 ;;
    Linux/aarch64|Linux/arm64) echo linux-aarch64 ;;
    *) return 1 ;;
  esac
}

check_docker() {
  local os=$1 report=bad
  # DECISION: without an image build, Docker is needed later, not for the install itself.
  [ "$SKIP_IMAGES" -eq 1 ] && report=warn
  if ! command -v docker >/dev/null 2>&1; then
    if [ "$os" = Darwin ]; then
      $report "Docker: install OrbStack (https://orbstack.dev) or Docker Desktop"
    else
      $report "Docker: install Docker Engine with buildx (Ubuntu: \`sudo apt install docker.io docker-buildx\`, then \`sudo usermod -aG docker \$USER\` and log in again)"
    fi
  elif ! docker info >/dev/null 2>&1; then
    if [ "$os" = Darwin ]; then
      $report "Docker daemon is not running: start OrbStack (\`orb start\`) or Docker Desktop"
    else
      $report "Docker daemon is not reachable: start it (\`sudo systemctl start docker\`) and make sure your user may use it"
    fi
  else
    ok "Docker $(docker version --format '{{.Server.Version}}' 2>/dev/null) (daemon reachable)"
    if docker buildx version >/dev/null 2>&1; then
      ok "Docker BuildKit (buildx)"
    else
      $report "Docker BuildKit: install the buildx plugin (bundled with OrbStack and Docker Desktop)"
    fi
  fi
}

check_prerequisites() {
  FAILED=0
  local os
  os=$(uname -s)
  step "Checking prerequisites"
  case "$os" in
    Darwin) ok "macOS $(sw_vers -productVersion 2>/dev/null || true) ($(uname -m))" ;;
    Linux) ok "Linux ($(uname -m))" ;;
    *) bad "unsupported OS '$os'; CapsCode runs on macOS and Linux" ;;
  esac

  if [ "$MODE" = binary ]; then
    if PLATFORM=$(release_platform); then
      ok "release binary: $PLATFORM"
    else
      bad "no release binary for $(uname -s) $(uname -m); build from source instead (git clone https://github.com/$REPO.git, then ./scripts/install.sh --from-source)"
    fi
    for tool in curl tar gzip; do
      if command -v "$tool" >/dev/null 2>&1; then ok "$tool"; else bad "$tool: install it with your package manager"; fi
    done
    if command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1; then
      ok "SHA-256 tool"
    else
      bad "sha256sum or shasum: install coreutils"
    fi
    if [ "$VERIFY_ATTESTATION" -eq 1 ]; then
      if command -v gh >/dev/null 2>&1; then
        ok "GitHub CLI (gh) for --verify-attestation"
      else
        bad "GitHub CLI (gh), needed by --verify-attestation: https://cli.github.com"
      fi
    fi
  else
    if command -v git >/dev/null 2>&1; then
      ok "git"
    else
      bad "git: install it with your package manager (Ubuntu: \`sudo apt install git\`)"
    fi

    local need_rust have_rust
    need_rust=$(sed -n 's/^rust-version = "\(.*\)"/\1/p' "$CHECKOUT/Cargo.toml" | head -n1)
    if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
      have_rust=$(rustc --version | awk '{print $2}')
      if version_at_least "$have_rust" "$need_rust"; then
        ok "Rust $have_rust (needs $need_rust or newer)"
      else
        bad "Rust $have_rust is older than $need_rust: run \`rustup update stable\`"
      fi
    else
      bad "Rust $need_rust or newer: install from https://rustup.rs (distribution packages are usually too old)"
    fi

    if command -v cc >/dev/null 2>&1; then
      ok "C compiler ($(command -v cc))"
    elif [ "$os" = Darwin ]; then
      bad "C compiler: run \`xcode-select --install\`"
    else
      bad "C compiler: install build-essential (Debian/Ubuntu) or gcc"
    fi
  fi

  check_docker "$os"

  local space where need
  if [ "$MODE" = binary ]; then where=$HOME; need=10; else where=$CHECKOUT; need=15; fi
  space=$(free_gb "$where")
  if [ -n "$space" ] && [ "$space" -lt "$need" ]; then
    warn "only ${space} GB free; the install needs about $need GB (mostly container images)"
  elif [ -n "$space" ]; then
    ok "${space} GB free disk space"
  fi

  if [ "$FAILED" -ne 0 ]; then
    printf '\nFix the MISSING items above, then run the installer again.\n' >&2
    return 1
  fi
}

# Download $1 to $2, refusing anything over $3 bytes. HTTPS only, including
# redirects, unless a test mirror (file://) is configured.
fetch() {
  curl --fail --silent --show-error --location --retry 2 \
    --proto "$CURL_PROTO" --proto-redir '=https' --tlsv1.2 \
    --max-filesize "$3" --output "$2" "$1" ||
    die 1 "download failed: $1"
}

# Check $2 (a downloaded file) against the SHA256SUMS entry for asset $3 in $1.
verify_checksum() {
  local expected actual
  expected=$(expected_sha256 "$1" "$3") ||
    die 1 "SHA256SUMS for v$VERSION has no single entry for $3; refusing to install"
  actual=$(sha256_of "$2")
  [ "$actual" = "$expected" ] ||
    die 1 "$3 does not match its SHA-256 in SHA256SUMS (expected $expected, got $actual); refusing to install"
  ok "$3 matches SHA256SUMS"
}

verify_attestation() {
  gh attestation verify "$1" --repo "$REPO" --signer-workflow "$REPO/$RELEASE_WORKFLOW" >/dev/null ||
    die 1 "gh attestation verify rejected $2; refusing to install"
  ok "$2 has a GitHub attestation from $REPO/$RELEASE_WORKFLOW"
}

# Unpack the verified build context into $CAPSCODE_DIR/build/$VERSION, replacing
# an earlier copy only once the new one is complete.
install_build_context() {
  local archive=$1 top build_dir staging dest old name size entries
  top="capscode-$VERSION-build-context"
  # Bound what unpacking can write before tar touches the disk: the decompressed
  # stream (headers included, as `capscode update --cli` counts it) and the file
  # contents tar would write (which also covers sparse files). `head` stops each
  # count one byte past the limit, so a decompression bomb is never expanded in full.
  size=$( (gzip -dc <"$archive" 2>/dev/null || true) | head -c $((MAX_UNPACKED_BYTES + 1)) | wc -c | tr -d ' ')
  [ "$size" -le "$MAX_UNPACKED_BYTES" ] ||
    die 1 "the build context unpacks to more than $MAX_UNPACKED_BYTES bytes; refusing it"
  size=$( (tar -xzOf "$archive" 2>/dev/null || true) | head -c $((MAX_UNPACKED_BYTES + 1)) | wc -c | tr -d ' ')
  [ "$size" -le "$MAX_UNPACKED_BYTES" ] ||
    die 1 "the build context unpacks to more than $MAX_UNPACKED_BYTES bytes; refusing it"
  tar -tzf "$archive" >"$WORK/context-names" || die 1 "the build context is not a readable tar.gz"
  entries=$(wc -l <"$WORK/context-names" | tr -d ' ')
  [ "$entries" -le "$MAX_CONTEXT_ENTRIES" ] ||
    die 1 "the build context has more than $MAX_CONTEXT_ENTRIES entries; refusing it"
  while IFS= read -r name; do
    case "$name" in
      "$top"/*) ;;
      *) die 1 "the build context has an entry outside $top/: $name; refusing it" ;;
    esac
    case "/$name/" in
      */../*) die 1 "the build context has an entry with '..': $name; refusing it" ;;
    esac
  done <"$WORK/context-names"
  # A long listing starts with the entry type: only files and directories. The
  # listing goes to a file first: `tar | grep -q` under pipefail would report the
  # SIGPIPE tar gets when grep stops early, hiding a match.
  tar -tvzf "$archive" >"$WORK/context-long" || die 1 "the build context is not a readable tar.gz"
  if grep -qv '^[-d]' "$WORK/context-long"; then
    die 1 "the build context contains links or special files; refusing it"
  fi

  build_dir="$CAPSCODE_DIR/build"
  mkdir -p "$build_dir"
  chmod 700 "$CAPSCODE_DIR" "$build_dir"
  staging=$(mktemp -d "$build_dir/.unpack-XXXXXX")
  tar -xzf "$archive" -C "$staging" --no-same-owner || { rm -rf "$staging"; die 1 "unpacking the build context failed"; }
  [ -f "$staging/$top/scripts/build-images.sh" ] || { rm -rf "$staging"; die 1 "the build context has no scripts/build-images.sh"; }
  dest="$build_dir/$VERSION"
  old=
  if [ -e "$dest" ] || [ -L "$dest" ]; then
    old=$(mktemp -d "$build_dir/.old-XXXXXX")
    mv "$dest" "$old/previous"
  fi
  mv "$staging/$top" "$dest"
  rm -rf "$staging"
  [ -z "$old" ] || rm -rf "$old"
  ok "image build files in $dest"
}

install_binary() {
  local platform=$PLATFORM binary_name context_name base channel_url reported
  if [ -n "${CAPSCODE_RELEASE_BASE_URL:-}" ]; then
    # DECISION: one hidden override, https:// or file:// only, for the test mirror.
    # For the installer's own tests only: a mirror laid out like GitHub's
    # releases/download/ tree (<base>/v<version>/<asset>, plus <base>/channel.json).
    # It changes where files come from, never how they are checked: SHA256SUMS
    # and every other check below still apply.
    case "$CAPSCODE_RELEASE_BASE_URL" in
      https://*|file://*) ;;
      *) die 2 "CAPSCODE_RELEASE_BASE_URL must be an https:// or file:// URL" ;;
    esac
    base=${CAPSCODE_RELEASE_BASE_URL%/}
    channel_url="$base/channel.json"
    CURL_PROTO='=https,file'
    warn "CAPSCODE_RELEASE_BASE_URL is set; downloading from $base (a test mirror)"
  else
    base="https://github.com/$REPO/releases/download"
    channel_url="https://raw.githubusercontent.com/$REPO/refs/heads/channel/channel.json"
    CURL_PROTO='=https'
  fi

  # DECISION: the newest version comes from channel.json (cli.version), the same source
  # `capscode update --cli` uses, not GitHub's "latest release", which can be a toolchain release.
  # DECISION: the channel is read for --version too, so a withdrawn release is refused
  # either way; an unreachable or malformed channel stops the install.
  step "Reading the CapsCode release channel"
  fetch "$channel_url" "$WORK/channel.json" "$MAX_SMALL_BYTES"
  local channel key value channel_version='' yanked=''
  channel=$(read_channel "$WORK/channel.json") ||
    die 1 "the release channel (channel.json) is not usable: $channel; refusing to install"
  while IFS=' ' read -r key value; do
    case "$key" in
      version) channel_version=$value ;;
      yanked) yanked="$yanked $value " ;;
    esac
  done <<<"$channel"
  if [ -n "$WANT_VERSION" ]; then
    VERSION=${WANT_VERSION#v}
  else
    VERSION=$channel_version
  fi
  # Tagged releases only: no prereleases, branches, or nightlies.
  [[ "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
    die 1 "'$VERSION' is not a plain release version (X.Y.Z); refusing to install it"
  case "$yanked" in
    *" $VERSION "*)
      if [ -n "$WANT_VERSION" ]; then
        die 1 "CapsCode $VERSION was withdrawn (the release channel lists it in cli.yanked_versions); refusing to install it. Leave out --version to install the newest release."
      fi
      die 1 "the release channel offers CapsCode $VERSION but also lists it as withdrawn; refusing to install it. Try again later, or name an earlier release with --version."
      ;;
  esac
  ok "release channel lists CapsCode $channel_version as the newest release"
  local tag="v$VERSION"
  binary_name="capscode-$VERSION-$platform"
  context_name="capscode-$VERSION-build-context.tar.gz"

  step "Downloading CapsCode $VERSION ($platform)"
  fetch "$base/$tag/SHA256SUMS" "$WORK/SHA256SUMS" "$MAX_SMALL_BYTES"
  fetch "$base/$tag/$binary_name" "$WORK/$binary_name" "$MAX_BINARY_BYTES"
  fetch "$base/$tag/$context_name" "$WORK/$context_name" "$MAX_CONTEXT_BYTES"

  step "Verifying the downloads"
  verify_checksum "$WORK/SHA256SUMS" "$WORK/$binary_name" "$binary_name"
  verify_checksum "$WORK/SHA256SUMS" "$WORK/$context_name" "$context_name"
  if [ "$VERIFY_ATTESTATION" -eq 1 ]; then
    verify_attestation "$WORK/$binary_name" "$binary_name"
    verify_attestation "$WORK/$context_name" "$context_name"
  fi
  chmod 755 "$WORK/$binary_name"
  if [ "$(uname -s)" = Darwin ]; then
    # The release is ad-hoc signed (no Apple Developer ID). curl adds no
    # quarantine attribute, so Gatekeeper does not block it; the kernel still
    # requires the signature to be intact.
    codesign --verify --strict "$WORK/$binary_name" 2>/dev/null ||
      die 1 "$binary_name has an invalid code signature; refusing to install"
    ok "code signature intact (ad-hoc)"
  fi
  reported=$(CAPSCODE_NO_UPDATE_CHECK=1 "$WORK/$binary_name" --version 2>/dev/null || true)
  [ "$reported" = "capscode $VERSION" ] ||
    die 1 "$binary_name does not run here or reports '$reported'; refusing to install"
  ok "$reported runs"

  step "Installing"
  install_build_context "$WORK/$context_name"
  mkdir -p "$BIN_DIR"
  local target="$BIN_DIR/capscode" staged
  if [ -L "$target" ]; then
    warn "replacing the link $target -> $(readlink "$target") (an earlier source install) with the release binary"
  fi
  staged=$(mktemp "$BIN_DIR/.capscode.XXXXXX")
  cp "$WORK/$binary_name" "$staged"
  chmod 755 "$staged"
  mv -f "$staged" "$target"
  ok "$target ($reported)"
  BINARY=$target
}

install_from_source() {
  cd "$CHECKOUT"
  step "Building the capscode CLI (cargo build --locked --release)"
  cargo build --locked --release
  BINARY="$CHECKOUT/target/release/capscode"

  step "Linking $BIN_DIR/capscode"
  mkdir -p "$BIN_DIR"
  ln -sfn "$BINARY" "$BIN_DIR/capscode"
  ok "$BIN_DIR/capscode -> $BINARY"
}

# The image build ($1 is the command that runs it) failed: say how to go on.
build_failed() {
  local retry=$1
  [ "$NO_SMOKE" -eq 0 ] || retry="$1 --no-smoke"
  local hint="Fix the problem shown above, then retry with \`$retry\` or rerun the installer."
  if [ "$NO_SMOKE" -eq 0 ]; then
    hint="$hint If only the start-up smoke test failed, \`$1 --no-smoke\` (or the installer's --no-smoke) builds the images without it."
  fi
  die 1 "building the container images failed. $hint To finish setup without images, rerun the installer with --skip-images."
}

can_prompt() {
  [ -t 1 ] || return 1
  [ -t 0 ] && return 0
  # Under `curl | bash` stdin is the script; prompts can still use the terminal.
  (: </dev/tty) 2>/dev/null
}

main() {
  BIN_DIR="$HOME/.local/bin"
  CHECK_ONLY=0
  SKIP_IMAGES=0
  SKIP_INIT=0
  SKIP_DOCTOR=0
  NO_SMOKE=0
  YES=0
  MODE=
  WANT_VERSION=
  VERIFY_ATTESTATION=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --check) CHECK_ONLY=1 ;;
      --bin-dir)
        [ $# -ge 2 ] || { echo "--bin-dir needs a directory" >&2; exit 2; }
        BIN_DIR=$2
        shift
        ;;
      --bin-dir=*) BIN_DIR=${1#--bin-dir=} ;;
      --skip-images) SKIP_IMAGES=1 ;;
      --skip-init) SKIP_INIT=1 ;;
      --skip-doctor) SKIP_DOCTOR=1 ;;
      --no-smoke) NO_SMOKE=1 ;;
      --yes) YES=1 ;;
      --binary) [ "$MODE" != source ] || { echo "--binary and --from-source conflict" >&2; exit 2; }; MODE=binary ;;
      --from-source) [ "$MODE" != binary ] || { echo "--binary and --from-source conflict" >&2; exit 2; }; MODE=source ;;
      --version)
        [ $# -ge 2 ] || { echo "--version needs a release version" >&2; exit 2; }
        WANT_VERSION=$2
        shift
        ;;
      --version=*) WANT_VERSION=${1#--version=} ;;
      --verify-attestation) VERIFY_ATTESTATION=1 ;;
      -h|--help) usage; exit 0 ;;
      *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
  done

  # A checkout is the directory above this script when it holds the workspace.
  CHECKOUT=
  local script=${BASH_SOURCE[0]:-} candidate
  if [ -n "$script" ] && [ -f "$script" ]; then
    candidate=$(cd "$(dirname "$script")/.." && pwd)
    if [ -f "$candidate/Cargo.toml" ] && [ -d "$candidate/crates/cli" ] && [ -f "$candidate/scripts/build-images.sh" ]; then
      CHECKOUT=$candidate
    fi
  fi
  # DECISION: an explicit --binary or --from-source wins; otherwise a checkout means
  # a source install (the developer flow) and anything else, such as `curl | bash`,
  # means the binary install.
  if [ -z "$MODE" ]; then
    if [ -n "$CHECKOUT" ]; then MODE=source; else MODE=binary; fi
  fi
  if [ "$MODE" = source ]; then
    [ -n "$CHECKOUT" ] ||
      die 2 "--from-source needs a CapsCode checkout: git clone https://github.com/$REPO.git && cd capscode && ./scripts/install.sh"
    [ -z "$WANT_VERSION" ] || die 2 "--version applies to the binary install; check out the tag instead (git checkout v<version>)"
    [ "$VERIFY_ATTESTATION" -eq 0 ] || die 2 "--verify-attestation applies to the binary install"
  fi
  CAPSCODE_DIR=${CAPSCODE_HOME:-$HOME/.capscode}

  check_prerequisites || exit 1
  [ "$CHECK_ONLY" -eq 1 ] && { printf '\nAll prerequisites are in place.\n'; exit 0; }

  if [ "$MODE" = binary ]; then
    WORK=$(mktemp -d "${TMPDIR:-/tmp}/capscode-install.XXXXXX")
    trap 'rm -rf "$WORK"' EXIT
    install_binary
  else
    install_from_source
  fi

  case ":$PATH:" in
    *":$BIN_DIR:"*) ON_PATH=1 ;;
    *)
      ON_PATH=0
      warn "$BIN_DIR is not on your PATH. Add this line to your shell profile:"
      # The literal $PATH is what the user pastes into their profile.
      # shellcheck disable=SC2016
      printf '             export PATH="%s:$PATH"\n' "$BIN_DIR"
      ;;
  esac

  if [ "$SKIP_IMAGES" -eq 1 ]; then
    step "Skipping container images (--skip-images); build them later with \`capscode toolchain build\`"
  else
    local build_command smoke_args=()
    [ "$NO_SMOKE" -eq 0 ] || smoke_args=(--no-smoke)
    if [ "$MODE" = binary ]; then
      build_command="capscode toolchain build"
      [ "$ON_PATH" -eq 1 ] || build_command="$BINARY toolchain build"
      step "Building container images ($build_command ${smoke_args[*]-}; the first run downloads several GB)"
      "$BINARY" toolchain build ${smoke_args[@]+"${smoke_args[@]}"} || build_failed "$build_command"
    else
      build_command="./scripts/build-images.sh"
      step "Building container images ($build_command ${smoke_args[*]-}; the first run downloads several GB)"
      ./scripts/build-images.sh ${smoke_args[@]+"${smoke_args[@]}"} || build_failed "$build_command"
    fi
  fi

  if [ "$SKIP_INIT" -eq 1 ]; then
    step "Skipping first-time setup (--skip-init)"
  elif [ "$YES" -eq 1 ]; then
    step "Running first-time setup without prompts (capscode init --yes --no-completions)"
    "$BINARY" init --yes --no-completions
  elif can_prompt; then
    step "Running first-time setup (capscode init)"
    if [ -t 0 ]; then "$BINARY" init; else "$BINARY" init </dev/tty; fi
  else
    step "Skipping first-time setup: no terminal. Run \`capscode init\` in a terminal."
  fi

  if [ "$SKIP_DOCTOR" -eq 0 ]; then
    step "Checking the installation (capscode doctor --quick)"
    "$BINARY" doctor --quick || warn "doctor reported problems; see above"
  fi

  local command_name=capscode update_command
  [ "$ON_PATH" -eq 1 ] || command_name="$BIN_DIR/capscode"
  if [ "$MODE" = binary ]; then
    update_command="$command_name update --cli"
  else
    update_command="git pull && ./scripts/install.sh"
  fi
  cat <<EOF

CapsCode is installed.
  Store a provider key:   $command_name key set <provider>   (list providers: $command_name provider list)
  Run a tool once:        cd ~/my-project && $command_name codex
  Create a box:           cd ~/my-project && $command_name create
  Full check (network):   $command_name doctor
  Update later:           $update_command
EOF
  if [ "$(uname -s)" = Darwin ]; then
    cat <<'EOF'
  macOS keychain:         the first command that reads a stored key asks for keychain
                          access; choose "Always Allow". This happens once per CLI
                          install or update, because each build has a new signature.
EOF
  fi
}

main "$@"
