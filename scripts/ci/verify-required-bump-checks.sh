#!/usr/bin/env bash
set -eu

repository=${GITHUB_REPOSITORY:-}
[[ -n "$repository" ]] || {
  printf '%s\n' 'GITHUB_REPOSITORY is required to check branch protection' >&2
  exit 1
}

# main is protected by a ruleset; the classic branch protection endpoint reports it as unprotected.
rules=$(gh api --paginate "repos/$repository/rules/branches/main") || {
  printf '%s\n' 'Could not read required checks for main; refusing to enable auto-merge' >&2
  exit 1
}
# DECISION: a ruleset may name a check by its stable job ID or its matrix/display name.
if ! printf '%s\n' "$rules" | jq -e -s '
  [add // [] | .[] | select(.type == "required_status_checks")
    | .parameters.required_status_checks[]?.context]
  | map(ascii_downcase) as $names
  | any($names[]; . == "rust" or startswith("rust checks") or startswith("ci / rust"))
    and any($names[]; contains("toolchain") and contains("smoke"))
' >/dev/null; then
  printf '%s\n' 'Required checks for main must include Rust and toolchain smoke; refusing to enable auto-merge' >&2
  exit 1
fi

printf '%s\n' 'Required Rust and toolchain smoke checks are enabled on main.'
