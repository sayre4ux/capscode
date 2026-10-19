#!/usr/bin/env bash
# Update a tool pin, tarball integrity, dependency cutoff, and smoke metadata.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LOCK=${CAPSCODE_TOOLCHAIN_LOCK:-$ROOT/images/toolchain.lock.toml}

tool_known() {
  case "$1" in
    claude|codex|opencode|pi|omp|playwright_core) return 0 ;;
    *) return 1 ;;
  esac
}

lock_line_for() {
  local lock=$1 tool=$2
  awk -v tool="$tool" '$1 == tool && $2 == "=" { print; exit }' "$lock"
}

field_from_lock_line() {
  local line=$1 field=$2
  sed -n "s/.*${field} = \"\\([^\"]*\\)\".*/\\1/p" <<<"$line"
}

select_latest_version() {
  local time_json=$1 versions_json=$2 dist_tag_version=$3 cooldown_days=$4 now_epoch=$5 pinned_version=$6
  node - "$time_json" "$versions_json" "$dist_tag_version" "$cooldown_days" "$now_epoch" "$pinned_version" <<'NODE'
const fs = require('fs');
const readJson = value => {
  const trimmed = value.trimStart();
  return JSON.parse(trimmed.startsWith('{') || trimmed.startsWith('[')
    ? value
    : fs.readFileSync(value, 'utf8'));
};
const times = readJson(process.argv[2]);
let versions = readJson(process.argv[3]);
if (!Array.isArray(versions)) versions = [versions];
const distTag = process.argv[4];
const cooldownDays = Number(process.argv[5]);
const now = Number(process.argv[6]);
const pinned = process.argv[7];
const cutoff = now - cooldownDays * 86400;
function parse(value) {
  const match = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/.exec(value);
  if (!match) return null;
  return { core: match.slice(1, 4).map(BigInt), pre: match[4] ? match[4].split('.') : null };
}
function compare(left, right) {
  const a = parse(left), b = parse(right);
  if (!a || !b) return NaN;
  for (let i = 0; i < 3; i++) if (a.core[i] !== b.core[i]) return a.core[i] < b.core[i] ? -1 : 1;
  if (a.pre === null || b.pre === null) return a.pre === b.pre ? 0 : a.pre === null ? 1 : -1;
  const length = Math.min(a.pre.length, b.pre.length);
  for (let i = 0; i < length; i++) {
    const x = a.pre[i], y = b.pre[i];
    if (x === y) continue;
    const xn = /^\d+$/.test(x), yn = /^\d+$/.test(y);
    if (xn && yn) return BigInt(x) < BigInt(y) ? -1 : 1;
    if (xn !== yn) return xn ? -1 : 1;
    return x < y ? -1 : 1;
  }
  return Math.sign(a.pre.length - b.pre.length);
}
if (!parse(distTag) || !parse(pinned)) {
  console.error(`npm returned an invalid dist-tag version: ${distTag}`);
  process.exit(2);
}
const eligible = versions.filter(version => {
  const published = Date.parse(times[version] || '');
  return parse(version) && compare(version, distTag) <= 0 && compare(version, pinned) >= 0 && Number.isFinite(published)
    && Math.floor(published / 1000) <= cutoff;
});
eligible.sort(compare);
if (!eligible.length) {
  console.error(`no ${distTag} channel version is at least ${cooldownDays} days old and not below the current pin ${pinned}`);
  process.exit(3);
}
process.stdout.write(eligible[eligible.length - 1]);
NODE
}

ensure_version_old_enough() {
  local version=$1 time_json=$2 now_epoch=$3 cooldown_days=$4 waive_reason=${5:-}
  node - "$version" "$time_json" "$now_epoch" "$cooldown_days" "$waive_reason" <<'NODE'
const version = process.argv[2];
const input = process.argv[3];
const times = JSON.parse(input.trimStart().startsWith('{') ? input : require('fs').readFileSync(input, 'utf8'));
const now = Number(process.argv[4]);
const days = Number(process.argv[5]);
const waiver = process.argv[6];
const published = Date.parse(times[version] || '');
if (!Number.isFinite(published)) {
  console.error(`npm publish time is missing for ${version}`);
  process.exit(2);
}
if (now - Math.floor(published / 1000) < days * 86400 && !waiver) {
  console.error(`${version} is younger than the ${days}-day cooldown; pass --waive-cooldown "<reason>" to proceed`);
  process.exit(3);
}
NODE
}

resolved_before_for() {
  local now_epoch=$1 cooldown_days=$2 waived=${3:-false}
  node - "$now_epoch" "$cooldown_days" "$waived" <<'NODE'
const now = Number(process.argv[2]);
const days = Number(process.argv[3]);
const waived = process.argv[4] === 'true';
const seconds = waived ? now : now - days * 86400;
process.stdout.write(new Date(seconds * 1000).toISOString().replace(/\.000Z$/, 'Z'));
NODE
}

published_at_for() {
  local version=$1 time_json=$2
  node - "$version" "$time_json" <<'NODE'
const version = process.argv[2];
const input = process.argv[3];
const times = JSON.parse(input.trimStart().startsWith('{') ? input : require('fs').readFileSync(input, 'utf8'));
const time = Date.parse(times[version] || '');
if (!Number.isFinite(time)) process.exit(2);
process.stdout.write(new Date(time).toISOString().replace(/\.000Z$/, 'Z'));
NODE
}

write_lock_entry() {
  local lock=$1 tool=$2 version=$3 integrity=$4 before=$5 published=$6 channel=$7 waiver=$8
  local waiver_toml
  waiver_toml=$(node -e 'process.stdout.write(JSON.stringify(process.argv[1]))' "$waiver")
  LOCK_PATH="$lock" TOOL="$tool" VERSION="$version" INTEGRITY="$integrity" \
    BEFORE="$before" PUBLISHED="$published" CHANNEL="$channel" WAIVER="$waiver_toml" \
    perl -i -pe '
      if (/^\Q$ENV{TOOL}\E\s*=\s*\{/) {
        my @fields = (
          [version => "\"$ENV{VERSION}\""],
          [integrity => "\"$ENV{INTEGRITY}\""],
          [resolved_before => "\"$ENV{BEFORE}\""],
          [published_at => "\"$ENV{PUBLISHED}\""],
          [channel => "\"$ENV{CHANNEL}\""],
          [smoke => "\"none\""],
          [smoke_date => "\"\""],
          [smoke_arch => "[]"],
          [cooldown_waived => $ENV{WAIVER}],
        );
        my $line = $_;
        for my $field (@fields) {
          my ($key, $value) = @$field;
          my $pattern = qr/\b\Q$key\E\s*=\s*(?:"(?:\\.|[^"])*"|\[[^\]]*\])/;
          my $replacement = "$key = $value";
          if ($line =~ $pattern) {
            $line =~ s/$pattern/$replacement/;
          } else {
            $line =~ s/\s*}[ \t]*$/, $replacement }/;
          }
        }
        $_ = $line;
      }' "$lock"
}

main() {
  if (($# < 1)); then
    echo "usage: $0 <tool> <version|--latest> [--cooldown-days N] [--waive-cooldown \"<reason>\"]" >&2
    return 2
  fi
  local tool=$1 request=$2 cooldown_days=3 waive_reason='' channel package line old_version
  shift 2
  while (($#)); do
    case "$1" in
      --cooldown-days)
        (($# >= 2)) || { echo "--cooldown-days requires a number" >&2; return 2; }
        cooldown_days=$2
        shift
        ;;
      --waive-cooldown)
        (($# >= 2)) || { echo "--waive-cooldown requires a reason" >&2; return 2; }
        waive_reason=$2
        shift
        ;;
      *) echo "unknown option '$1'" >&2; return 2 ;;
    esac
    shift
  done
  tool_known "$tool" || { echo "!! unknown tool '$tool'" >&2; return 2; }
  [[ "$cooldown_days" =~ ^[0-9]+$ ]] || { echo "!! cooldown days must be a non-negative integer" >&2; return 2; }
  [[ "$waive_reason" != *$'\n'* && "$waive_reason" != *$'\r'* ]] || { echo "!! cooldown waiver reason must be one line" >&2; return 2; }
  line=$(lock_line_for "$LOCK" "$tool")
  [[ -n "$line" ]] || { echo "!! no $tool entry in $LOCK" >&2; return 1; }
  package=$(field_from_lock_line "$line" package)
  old_version=$(field_from_lock_line "$line" version)
  channel=$(field_from_lock_line "$line" channel)
  [[ -n "$channel" ]] || channel=latest
  [[ -n "$package" ]] || { echo "!! no npm package for $tool in $LOCK" >&2; return 1; }

  local now_epoch time_json version versions_json dist_tag integrity before published waived selection_output selection_status
  now_epoch=$(date -u +%s)
  time_json=$(npm view "$package" time --json)
  if [[ "$request" == "--latest" ]]; then
    dist_tag=$(npm view "$package" "dist-tags.$channel")
    versions_json=$(npm view "$package" versions --json)
    if [[ -n "$waive_reason" ]]; then
      version=$dist_tag
      ensure_version_old_enough "$version" "$time_json" "$now_epoch" "$cooldown_days" "$waive_reason"
    else
      if selection_output=$(select_latest_version "$time_json" "$versions_json" "$dist_tag" "$cooldown_days" "$now_epoch" "$old_version" 2>&1); then
        version=$selection_output
      else
        selection_status=$?
        if [[ "$selection_status" == 3 ]]; then
          # DECISION: no eligible release leaves this pin alone so batch bump runs can continue.
          echo "$tool: no eligible $channel version is at least $cooldown_days days old and not below the current pin $old_version; lock left unchanged"
          return 0
        fi
        printf '%s\n' "$selection_output" >&2
        return "$selection_status"
      fi
    fi
  else
    version=$request
    [[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] ||
      { echo "!! version must use SemVer, for example 1.2.3" >&2; return 2; }
    ensure_version_old_enough "$version" "$time_json" "$now_epoch" "$cooldown_days" "$waive_reason"
  fi
  if [[ "$request" == "--latest" && "$version" == "$old_version" ]]; then
    echo "$tool: already pinned at $version; lock left unchanged"
    return 0
  fi
  published=$(published_at_for "$version" "$time_json") || {
    echo "!! npm publish time is missing for $version" >&2
    return 1
  }
  integrity=$(npm view "$package@$version" dist.integrity)
  [[ "$integrity" =~ ^sha512-[A-Za-z0-9+/]+=*$ ]] ||
    { echo "!! npm returned no sha512 integrity for $package@$version" >&2; return 1; }
  if [[ -n "$waive_reason" ]]; then
    waived=true
  else
    waived=false
  fi
  before=$(resolved_before_for "$now_epoch" "$cooldown_days" "$waived")
  write_lock_entry "$LOCK" "$tool" "$version" "$integrity" "$before" "$published" "$channel" "$waive_reason"

  local arg dockerfile
  arg=$(echo "$tool" | tr '[:lower:]' '[:upper:]')_VERSION
  for dockerfile in "$ROOT"/images/*.Dockerfile; do
    ARG="$arg" VERSION="$version" perl -i -pe 's/^ARG \Q$ENV{ARG}\E=.*/ARG $ENV{ARG}=$ENV{VERSION}/' "$dockerfile"
  done

  echo "$tool: $old_version -> $version ($package; channel $channel)"
  echo "  integrity       $integrity"
  echo "  published_at    $published"
  echo "  resolved_before $before"
  if [[ -n "$waive_reason" ]]; then echo "  cooldown_waived $waive_reason"; fi
  echo "  smoke           none (run ./scripts/smoke-tui.sh $tool --record after building)"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
