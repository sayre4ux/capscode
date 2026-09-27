#!/usr/bin/env bash
set -eu

repository=${GITHUB_REPOSITORY:-}
[[ -n "$repository" ]] || {
  printf '%s\n' 'GITHUB_REPOSITORY is required to check branch protection' >&2
  exit 1
}

checks=$(gh api "repos/$repository/branches/main/protection/required_status_checks") || {
  printf '%s\n' 'Could not read required checks for main; refusing to enable auto-merge' >&2
  exit 1
}
# DECISION: branch protection may report a stable job ID or its matrix/display name.
if ! printf '%s\n' "$checks" | jq -e '
  ((.contexts // []) + ((.checks // []) | map(.context // empty)))
  | map(ascii_downcase) as $names
  | any($names[]; . == "rust" or startswith("rust checks") or startswith("ci / rust"))
    and any($names[]; contains("toolchain") and contains("smoke"))
' >/dev/null; then
  printf '%s\n' 'Required checks for main must include Rust and toolchain smoke; refusing to enable auto-merge' >&2
  exit 1
fi

printf '%s\n' 'Required Rust and toolchain smoke checks are enabled on main.'
