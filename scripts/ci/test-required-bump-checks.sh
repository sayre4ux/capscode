#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TEMP=$(mktemp -d "${TMPDIR:-/tmp}/capscode-required-checks-test.XXXXXX")
trap 'rm -rf -- "$TEMP"' EXIT
mkdir -p "$TEMP/bin"
cat >"$TEMP/bin/gh" <<'FAKE'
#!/usr/bin/env bash
cat "$GH_CHECKS_FIXTURE"
FAKE
chmod +x "$TEMP/bin/gh"

cat >"$TEMP/valid.json" <<'JSON'
{"contexts":["Rust checks (ubuntu-latest)","Build and smoke toolchain images"]}
JSON
cat >"$TEMP/missing-smoke.json" <<'JSON'
{"contexts":["Rust checks (ubuntu-latest)","Build gateway image"]}
JSON
cat >"$TEMP/missing-rust.json" <<'JSON'
{"checks":[{"context":"toolchain-smoke"}]}
JSON

GH_CHECKS_FIXTURE="$TEMP/valid.json" PATH="$TEMP/bin:$PATH" GITHUB_REPOSITORY=owner/repo \
  "$ROOT/scripts/ci/verify-required-bump-checks.sh" >/dev/null
for fixture in missing-smoke missing-rust; do
  if GH_CHECKS_FIXTURE="$TEMP/$fixture.json" PATH="$TEMP/bin:$PATH" GITHUB_REPOSITORY=owner/repo \
    "$ROOT/scripts/ci/verify-required-bump-checks.sh" >"$TEMP/$fixture.out" 2>&1; then
    printf 'Accepted required-check fixture missing %s\n' "$fixture" >&2
    exit 1
  fi
  grep -Fq 'must include Rust and toolchain smoke' "$TEMP/$fixture.out"
done

printf '%s\n' 'PASS required bump check gate'
