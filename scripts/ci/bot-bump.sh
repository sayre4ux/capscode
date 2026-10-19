#!/usr/bin/env bash
set -eu

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
LOCK=images/toolchain.lock.toml
BRANCH=automation/toolchain-bump
TOOLS=(claude codex opencode pi omp playwright_core)

usage() {
  cat <<'USAGE'
Usage: scripts/ci/bot-bump.sh [--tool TOOL|all] [--dry-run]

Resolve each selected tool with bump-tool.sh --latest, build candidate images,
run smoke-tui.sh all, and update one automation PR. Exit status 0 means the
lock already matches main; status 10 means a smoke-passing PR was updated.
Other non-zero statuses indicate an error. --dry-run needs no network or token.
USAGE
}

fail() {
  printf 'bot-bump: %s\n' "$1" >&2
  exit 2
}

selected_tool=all
dry_run=0
while (($#)); do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --tool)
      (($# >= 2)) || fail 'missing value for --tool'
      selected_tool=$2
      shift 2
      ;;
    --dry-run) dry_run=1; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done

if [[ "$selected_tool" != all ]]; then
  case "$selected_tool" in
    claude|codex|opencode|pi|omp|playwright_core) TOOLS=("$selected_tool") ;;
    *) fail "unknown tool '$selected_tool'" ;;
  esac
fi

if ((dry_run)); then
  for tool in "${TOOLS[@]}"; do
    printf 'DRY-RUN: scripts/bump-tool.sh %s --latest --cooldown-days 3\n' "$tool"
  done
  printf '%s\n' 'DRY-RUN: build candidate images, smoke-tui.sh all, and update automation/toolchain-bump'
  printf 'DRY-RUN: would return %s if the candidate lock differs from main\n' 10
  exit 10
fi

cd "$ROOT"
[[ -n "${GITHUB_REPOSITORY:-}" ]] || fail 'GITHUB_REPOSITORY is required outside dry-run mode'
[[ -n "${GH_TOKEN:-}" ]] || fail 'GH_TOKEN is required outside dry-run mode'

# Set before the merge below, which commits when the bump branch already exists.
git config user.name 'github-actions[bot]'
git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
git fetch --no-tags origin main
if git ls-remote --exit-code --heads origin "$BRANCH" >/dev/null 2>&1; then
  git fetch --no-tags origin "$BRANCH"
  git checkout -B "$BRANCH" "origin/$BRANCH"
  git merge --no-edit origin/main
else
  git checkout -B "$BRANCH" origin/main
fi

for tool in "${TOOLS[@]}"; do
  scripts/bump-tool.sh "$tool" --latest --cooldown-days 3
done

if git diff --quiet origin/main -- "$LOCK"; then
  printf 'No qualifying updates for %s.\n' "$selected_tool"
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then printf 'changed=false\n' >>"$GITHUB_OUTPUT"; fi
  exit 0
fi

main_sequence=$(git show "origin/main:$LOCK" | awk '$1 == "sequence" && $2 == "=" { print $3; exit }')
[[ "$main_sequence" =~ ^[0-9]+$ ]] || fail 'main lock is missing an integer sequence field'
candidate_sequence=$(awk '$1 == "sequence" && $2 == "=" { print $3; exit }' "$LOCK")
[[ "$candidate_sequence" =~ ^[0-9]+$ ]] || fail 'candidate lock is missing an integer sequence field'
if ((candidate_sequence <= main_sequence)); then
  candidate_sequence=$((main_sequence + 1))
fi

set_top_level() {
  local key=$1 value=$2
  LOCK_KEY="$key" LOCK_VALUE="$value" perl -0pi -e '
    my ($key, $value) = @ENV{qw(LOCK_KEY LOCK_VALUE)};
    if (/^\Q$key\E\s*=/m) {
      s/^(\Q$key\E\s*=\s*)[^\r\n]*/$1$value/m;
    } else {
      s/^(?=\[)/$key = $value\n/m or die "no TOML table boundary\n";
    }
  ' "$LOCK"
}

set_tool_field() {
  local tool=$1 field=$2 value=$3
  LOCK_TOOL="$tool" LOCK_FIELD="$field" LOCK_VALUE="$value" perl -0pi -e '
    my ($tool, $field, $value) = @ENV{qw(LOCK_TOOL LOCK_FIELD LOCK_VALUE)};
    my $pattern = qr/^\Q$tool\E\s*=\s*\{[^}\r\n]*\}/m;
    my ($line) = /($pattern)/;
    die "missing inline lock entry for $tool\n" unless defined $line;
    my $updated = $line;
    if ($updated =~ /\b\Q$field\E\s*=/) {
      $updated =~ s/(\b\Q$field\E\s*=\s*)(?:"[^"\r\n]*"|\[[^\]]*\]|[0-9]+)/$1 . $value/e
        or die "cannot update $field for $tool\n";
    } else {
      $updated =~ s/\}\s*$/, $field = $value }/;
    }
    s/\Q$line\E/$updated/;
  ' "$LOCK"
}

cli_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
[[ -n "$cli_version" ]] || fail 'could not read workspace CLI version'
smoke_date=$(date -u +%Y-%m-%d)
machine=$(uname -m)
case "$machine" in
  x86_64|amd64) smoke_arch=amd64 ;;
  aarch64|arm64) smoke_arch=arm64 ;;
  *) fail "unsupported smoke architecture: $machine" ;;
esac

set_top_level sequence "$candidate_sequence"
set_top_level min_cli_version "\"$cli_version\""
scripts/build-images.sh --no-smoke
scripts/smoke-tui.sh all

for tool in claude codex opencode pi omp; do
  set_tool_field "$tool" smoke '"ci"'
  set_tool_field "$tool" smoke_date "\"$smoke_date\""
  set_tool_field "$tool" smoke_arch "[\"$smoke_arch\"]"
done

git add "$LOCK"
# A same-day rerun on an existing branch stages nothing new, but the lock still differs from main
# and the merge of main above must reach the PR, so keep going.
if ! git diff --cached --quiet; then
  git commit -m 'Update tested toolchain pins'
fi
git push --set-upstream origin "$BRANCH"

pr_number=$(gh pr list --repo "$GITHUB_REPOSITORY" --head "$BRANCH" --state open --json number --jq '.[0].number // empty')
if [[ -z "$pr_number" ]]; then
  pr_number=$(gh pr create --repo "$GITHUB_REPOSITORY" --base main --head "$BRANCH" \
    --title 'Update tested toolchain pins' \
    --body 'Daily toolchain update. All selected versions passed the three-day cooldown, image build, and smoke-tui gate.')
else
  gh pr edit "$pr_number" --repo "$GITHUB_REPOSITORY" \
    --title 'Update tested toolchain pins' \
    --body 'Daily toolchain update. All selected versions passed the three-day cooldown, image build, and smoke-tui gate.'
fi

"$ROOT/scripts/ci/verify-required-bump-checks.sh"
gh pr merge "$pr_number" --repo "$GITHUB_REPOSITORY" --auto --squash
if [[ "${CAPSCODE_BOT_TRIGGER_TOKEN:-false}" != true ]]; then
  printf '%s\n' 'BOT_TOKEN is not configured: GITHUB_TOKEN will not start pull_request CI, so auto-merge will wait for a maintainer to run the required checks.' >&2
fi

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then printf 'changed=true\n' >>"$GITHUB_OUTPUT"; fi
exit 10
