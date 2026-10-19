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

required() {
  local contexts=$1
  printf '[{"type":"deletion","ruleset_source_type":"Repository","ruleset_source":"owner/repo","ruleset_id":1},'
  printf '{"type":"required_status_checks","ruleset_source_type":"Repository","ruleset_source":"owner/repo","ruleset_id":1,'
  printf '"parameters":{"strict_required_status_checks_policy":false,"required_status_checks":[%s]}}]\n' "$contexts"
}
required '{"context":"Rust checks (ubuntu-latest)"},{"context":"Build and smoke toolchain images","integration_id":15368}' >"$TEMP/valid.json"
required '{"context":"Rust checks (ubuntu-latest)"},{"context":"Build gateway image"}' >"$TEMP/missing-smoke.json"
required '{"context":"toolchain-smoke"}' >"$TEMP/missing-rust.json"
printf '%s\n' '[{"type":"deletion","ruleset_source_type":"Repository","ruleset_source":"owner/repo","ruleset_id":1}]' >"$TEMP/no-checks-rule.json"
printf '%s\n' '[]' >"$TEMP/no-rules.json"

GH_CHECKS_FIXTURE="$TEMP/valid.json" PATH="$TEMP/bin:$PATH" GITHUB_REPOSITORY=owner/repo \
  "$ROOT/scripts/ci/verify-required-bump-checks.sh" >/dev/null
for fixture in missing-smoke missing-rust no-checks-rule no-rules; do
  if GH_CHECKS_FIXTURE="$TEMP/$fixture.json" PATH="$TEMP/bin:$PATH" GITHUB_REPOSITORY=owner/repo \
    "$ROOT/scripts/ci/verify-required-bump-checks.sh" >"$TEMP/$fixture.out" 2>&1; then
    printf 'Accepted required-check fixture missing %s\n' "$fixture" >&2
    exit 1
  fi
  grep -Fq 'must include Rust and toolchain smoke' "$TEMP/$fixture.out"
done

printf '%s\n' 'PASS required bump check gate'
