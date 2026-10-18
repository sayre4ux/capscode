#!/usr/bin/env bash
set -eu

usage() {
  cat <<'USAGE'
Usage: scripts/release/publish-channel.sh --section cli|toolchain --manifest PATH [--dry-run]

Re-read channel.json from the channel branch, replace only the published
section, and push with a bounded retry if another release updates the branch.
USAGE
}

manifest=
section=
dry_run=0
while (($#)); do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --manifest)
      (($# >= 2)) || { printf '%s\n' 'Missing --manifest path' >&2; exit 2; }
      manifest=$2
      shift 2
      ;;
    --section)
      (($# >= 2)) || { printf '%s\n' 'Missing --section value' >&2; exit 2; }
      section=$2
      shift 2
      ;;
    --dry-run) dry_run=1; shift ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done

[[ "$section" == cli || "$section" == toolchain ]] || { printf '%s\n' 'Section must be cli or toolchain' >&2; exit 2; }
[[ -n "$manifest" ]] || { usage >&2; exit 2; }
[[ -f "$manifest" ]] || { printf 'Manifest not found: %s\n' "$manifest" >&2; exit 1; }
jq -e --arg section "$section" '.schema == 1 and (.[$section]|type == "object")' "$manifest" >/dev/null || {
  printf '%s\n' 'Manifest failed basic schema validation' >&2
  exit 1
}

repo_root=$(git -C "$(dirname "$manifest")" rev-parse --show-toplevel 2>/dev/null || git rev-parse --show-toplevel)
if ((dry_run)); then
  printf 'DRY-RUN: merge only the %s section from %s into refs/heads/channel in %s\n' "$section" "$manifest" "$repo_root"
  exit 0
fi

[[ -n "${GITHUB_REPOSITORY:-}" ]] || { printf '%s\n' 'GITHUB_REPOSITORY is required' >&2; exit 1; }
[[ -n "${GH_TOKEN:-}" ]] || { printf '%s\n' 'GH_TOKEN is required' >&2; exit 1; }

temp_parent=$(mktemp -d "${TMPDIR:-/tmp}/capscode-channel-worktree.XXXXXX")
temp_worktree="$temp_parent/worktree"
# shellcheck disable=SC2317,SC2329
cleanup() {
  git -C "$repo_root" worktree remove --force "$temp_worktree" >/dev/null 2>&1 || true
  rm -rf "$temp_parent"
}
trap cleanup EXIT

worktree_ready=0
for attempt in 1 2 3; do
  remote_line=$(git -C "$repo_root" ls-remote --heads origin refs/heads/channel) || {
    printf '%s\n' 'Could not read the channel branch from origin' >&2
    exit 1
  }
  remote_oid=${remote_line%%[[:space:]]*}
  if [[ -n "$remote_oid" ]]; then
    git -C "$repo_root" fetch --no-tags origin +refs/heads/channel:refs/remotes/origin/channel
    if ((worktree_ready)); then
      git -C "$temp_worktree" reset --hard origin/channel >/dev/null
      git -C "$temp_worktree" switch -C channel origin/channel >/dev/null
    else
      git -C "$repo_root" worktree add --detach "$temp_worktree" origin/channel >/dev/null
      git -C "$temp_worktree" switch -C channel origin/channel >/dev/null
      worktree_ready=1
    fi
    base_manifest=$(git -C "$temp_worktree" show origin/channel:channel.json) || {
      printf '%s\n' 'The channel branch has no channel.json' >&2
      exit 1
    }
  else
    if ((worktree_ready)) || [[ "$section" != toolchain ]]; then
      printf '%s\n' 'The channel branch disappeared during publication' >&2
      exit 1
    fi
    git -C "$repo_root" worktree add --detach "$temp_worktree" HEAD >/dev/null
    git -C "$temp_worktree" switch --orphan channel >/dev/null
    git -C "$temp_worktree" rm -rf . >/dev/null 2>&1 || true
    worktree_ready=1
    base_manifest=$(cat "$manifest")
  fi

  merged_manifest=$(printf '%s\n' "$base_manifest" | "$repo_root/scripts/release/merge-channel-section.sh" --section "$section" --manifest "$manifest") || {
    printf '%s\n' 'Could not merge the prepared channel section' >&2
    exit 1
  }
  printf '%s\n' "$merged_manifest" >"$temp_worktree/channel.json"
  git -C "$temp_worktree" add channel.json
  if git -C "$temp_worktree" diff --cached --quiet; then
    printf '%s\n' 'channel.json is already current.'
    exit 0
  fi
  git -C "$temp_worktree" config user.name 'github-actions[bot]'
  git -C "$temp_worktree" config user.email '41898282+github-actions[bot]@users.noreply.github.com'
  git -C "$temp_worktree" commit -m "Update $section release channel"

  if git -C "$temp_worktree" push origin HEAD:refs/heads/channel >"$temp_parent/push.log" 2>&1; then
    cat "$temp_parent/push.log"
    exit 0
  fi
  if ((attempt == 3)); then
    cat "$temp_parent/push.log" >&2
    printf '%s\n' 'Could not publish channel.json after three attempts' >&2
    exit 1
  fi

  latest_line=$(git -C "$repo_root" ls-remote --heads origin refs/heads/channel) || {
    cat "$temp_parent/push.log" >&2
    printf '%s\n' 'Could not re-read the channel branch after a rejected push' >&2
    exit 1
  }
  latest_oid=${latest_line%%[[:space:]]*}
  # DECISION: retry only when the channel moved, so credential and branch-rule failures surface immediately.
  if [[ "$latest_oid" == "$remote_oid" ]]; then
    cat "$temp_parent/push.log" >&2
    printf '%s\n' 'Channel push failed and the remote branch did not change' >&2
    exit 1
  fi
done

printf '%s\n' 'Could not publish channel.json after three attempts' >&2
exit 1
