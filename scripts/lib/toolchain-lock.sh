#!/usr/bin/env bash

# Read the string fields used by the build and digest from both inline tool
# tables and the expanded tables written by toml::to_string_pretty.
toolchain_lock_value() {
  local lock=$1 tool=$2 field=$3
  awk -v tool="$tool" -v field="$field" '
    function value(line, field, expression, result) {
      expression = field "[[:space:]]*=[[:space:]]*\"[^\"]*\""
      if (!match(line, expression)) return
      result = substr(line, RSTART, RLENGTH)
      sub(/^[^=]*=[[:space:]]*"/, "", result)
      sub(/"$/, "", result)
      print result
    }
    /^\[tools\]$/ { section = "tools"; next }
    /^\[tools\.[^]]+\]$/ {
      section = ($0 == "[tools." tool "]") ? "tool" : "other"
      next
    }
    /^\[base\]$/ { section = "base"; next }
    /^\[seeder\]$/ { section = "seeder"; next }
    /^\[/ { section = "other"; next }
    section == "tools" && $1 == tool && $2 == "=" { value($0, field); exit }
    section == "tool" && $1 == field { value($0, field); exit }
    (section == "base" || section == "seeder") && $1 == field && $2 == "=" { value($0, field); exit }
  ' "$lock"
}

toolchain_lock_top_value() {
  local lock=$1 field=$2
  awk -v field="$field" '
    /^\[/ { exit }
    $1 == field && $2 == "=" { sub(/^[^=]*=[[:space:]]*/, ""); gsub(/["[:space:]]/, ""); print; exit }
  ' "$lock"
}

toolchain_lock_tool_names() {
  local lock=$1
  awk '
    /^\[tools\]$/ { section = "inline"; next }
    /^\[tools\.([^]]+)\]$/ { name = $0; sub(/^\[tools\./, "", name); sub(/\]$/, "", name); print name; next }
    /^\[/ { section = "other"; next }
    section == "inline" && $1 ~ /^[a-z_][a-z0-9_]*$/ && $2 == "=" { print $1 }
  ' "$lock" | LC_ALL=C sort -u
}

toolchain_sha256_hex() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum | awk '{print $1}'
  else shasum -a 256 | awk '{print $1}'; fi
}

# Canonical lock identity: sorted tab-separated records, newline-joined with no
# final newline. This includes every input that selects package bytes or base
# image bytes while ignoring smoke/release metadata.
toolchain_lock_records() {
  local lock=$1
  awk '
    function get(line, key, expression, result) {
      expression = key "[[:space:]]*=[[:space:]]*\"[^\"]*\""
      if (!match(line, expression)) return ""
      result = substr(line, RSTART, RLENGTH)
      sub(/^[^=]*=[[:space:]]*"/, "", result)
      sub(/"$/, "", result)
      return result
    }
    function emit_tool(name, line, field, value) {
      for (field = 1; field <= 4; field++) {
        key = (field == 1 ? "package" : field == 2 ? "version" : field == 3 ? "integrity" : "resolved_before")
        value = get(line, key)
        if (value != "") printf "tool\t%s\t%s\t%s\n", name, key, value
      }
    }
    /^\[base\]$/ { section = "base"; next }
    /^\[seeder\]$/ { section = "seeder"; next }
    /^\[tools\]$/ { section = "tools"; next }
    /^\[tools\.([^]]+)\]$/ {
      name = $0; sub(/^\[tools\./, "", name); sub(/\]$/, "", name)
      section = "tool"; next
    }
    /^\[/ { section = "other"; next }
    section == "base" && $1 ~ /^[a-z_][a-z0-9_]*$/ && $2 == "=" {
      value = get($0, $1)
      if (value != "") printf "base\t%s\t%s\n", $1, value
    }
    section == "seeder" && ($1 == "image" || $1 == "digest") && $2 == "=" {
      value = get($0, $1)
      if (value != "") printf "seeder\t%s\t%s\n", $1, value
    }
    section == "tools" && $2 == "=" {
      name = $1; sub(/^[[:space:]]*/, "", name)
      emit_tool(name, $0)
    }
    section == "tool" && $2 == "=" { emit_tool(name, $0) }
  ' "$lock"
}

toolchain_lock_digest() {
  local canonical
  canonical=$(toolchain_lock_records "$1" | LC_ALL=C sort)
  printf '%s' "$canonical" | toolchain_sha256_hex | cut -c1-12
}

toolchain_record_smoke() {
  local lock=$1 tool=$2 date=$3 arch=$4
  TOOL="$tool" SMOKE_DATE="$date" SMOKE_ARCH="$arch" perl -0pi -e '
    my ($tool, $date, $arch) = @ENV{qw(TOOL SMOKE_DATE SMOKE_ARCH)};
    if (/^\Q$tool\E\s*=\s*\{.*?\}/ms) {
      s{^\Q$tool\E\s*=\s*\{.*?\}}{
        my $entry = $&;
        $entry =~ s/(\bsmoke\s*=\s*)"[^"]*"/${1}"ci"/;
        $entry =~ s/(\bsmoke_date\s*=\s*)"[^"]*"/${1}"$date"/;
        $entry =~ s/(\bsmoke_arch\s*=\s*)\[[^]]*\]/${1}["$arch"]/s;
        $entry;
      }mse;
    } else {
      s{(^\[tools\.\Q$tool\E\][ \t]*\n)(.*?)(?=^\[|\z)}{
        my ($header, $section) = ($1, $2);
        $section =~ s/^([ \t]*smoke\s*=\s*)"[^"]*"/${1}"ci"/m;
        $section =~ s/^([ \t]*smoke_date\s*=\s*)"[^"]*"/${1}"$date"/m;
        $section =~ s/^([ \t]*smoke_arch\s*=\s*)\[[^]]*\]/${1}["$arch"]/ms;
        $header . $section;
      }mse;
    }
  ' "$lock"
}

# DECISION: only the repository lock records CI smoke results; external lock
# files belong to the caller who passed them to the build.
toolchain_record_smoke_for_lock() {
  local lock=$1 repository_lock=$2 tool=$3 date=$4 arch=$5
  [[ "$lock" == "$repository_lock" ]] || return 0
  toolchain_record_smoke "$lock" "$tool" "$date" "$arch"
}

toolchain_expected_package() {
  case "$1" in
    claude) printf '%s' '@anthropic-ai/claude-code' ;;
    codex) printf '%s' '@openai/codex' ;;
    opencode) printf '%s' 'opencode-ai' ;;
    pi) printf '%s' '@earendil-works/pi-coding-agent' ;;
    omp) printf '%s' '@oh-my-pi/pi-coding-agent' ;;
    playwright_core) printf '%s' 'playwright-core' ;;
    *) return 1 ;;
  esac
}

# DECISION: validate lock fields with awk and standard shell tools so binary
# installs can build images on supported hosts without an extra runtime.
toolchain_valid_semver() {
  # Values travel through the environment because `awk -v` would interpret backslash escapes.
  TOOLCHAIN_LOCK_VALUE=$1 LC_ALL=C awk '
    function greater_than_max(part, max, i, left_digit, right_digit) {
      if (length(part) != length(max)) return length(part) > length(max)
      for (i = 1; i <= length(part); i++) {
        left_digit = substr(part, i, 1)
        right_digit = substr(max, i, 1)
        if (left_digit != right_digit) return left_digit > right_digit
      }
      return 0
    }
    function valid_ids(list, prerelease, count, i, item, parts) {
      if (list == "") return 0
      count = split(list, parts, /\./)
      for (i = 1; i <= count; i++) {
        item = parts[i]
        if (item !~ /^[0-9A-Za-z-]+$/) return 0
        if (prerelease && item ~ /^[0-9]+$/ && length(item) > 1 && substr(item, 1, 1) == "0") return 0
      }
      return 1
    }
    BEGIN {
      value = ENVIRON["TOOLCHAIN_LOCK_VALUE"]
      max = "18446744073709551615"
      if (length(value) > 64) exit 1
      plus = index(value, "+")
      if (plus) {
        build = substr(value, plus + 1)
        value = substr(value, 1, plus - 1)
        if (index(build, "+") || !valid_ids(build, 0)) exit 1
      }
      dash = index(value, "-")
      if (dash) {
        prerelease = substr(value, dash + 1)
        value = substr(value, 1, dash - 1)
        if (!valid_ids(prerelease, 1)) exit 1
      }
      count = split(value, core, /\./)
      if (count != 3) exit 1
      for (i = 1; i <= 3; i++) {
        part = core[i]
        if (part !~ /^(0|[1-9][0-9]*)$/ || length(part) > 20 || greater_than_max(part, max)) exit 1
      }
    }
  '
}

toolchain_valid_integrity() {
  TOOLCHAIN_LOCK_VALUE=$1 LC_ALL=C awk '
    BEGIN {
      value = ENVIRON["TOOLCHAIN_LOCK_VALUE"]
      if (length(value) != 95 || substr(value, 1, 7) != "sha512-" || substr(value, 94, 2) != "==") exit 1
      encoded = substr(value, 8, 86)
      if (encoded !~ /^[A-Za-z0-9+\/]+$/) exit 1
    }
  '
}

toolchain_timestamp_not_future() {
  local now
  now=$(date -u +%Y-%m-%dT%H:%M:%SZ) || return 1
  TOOLCHAIN_LOCK_VALUE=$1 LC_ALL=C awk -v now="$now" '
    BEGIN {
      value = ENVIRON["TOOLCHAIN_LOCK_VALUE"]
      if (length(value) != 20 || substr(value, 5, 1) != "-" || substr(value, 8, 1) != "-" || substr(value, 11, 1) != "T" || substr(value, 14, 1) != ":" || substr(value, 17, 1) != ":" || substr(value, 20, 1) != "Z") exit 1
      digits = substr(value, 1, 4) substr(value, 6, 2) substr(value, 9, 2) substr(value, 12, 2) substr(value, 15, 2) substr(value, 18, 2)
      if (digits !~ /^[0-9]+$/) exit 1
      year = substr(value, 1, 4) + 0
      month = substr(value, 6, 2) + 0
      day = substr(value, 9, 2) + 0
      hour = substr(value, 12, 2) + 0
      minute = substr(value, 15, 2) + 0
      second = substr(value, 18, 2) + 0
      if (year < 1 || month < 1 || month > 12 || hour > 23 || minute > 59 || second > 60) exit 1
      leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
      days = (month == 2 ? (leap ? 29 : 28) : (month == 4 || month == 6 || month == 9 || month == 11 ? 30 : 31))
      if (day < 1 || day > days || value > now) exit 1
    }
  '
}

toolchain_validate_lock() {
  local lock=$1 schema names tool package version integrity before expected
  schema=$(toolchain_lock_top_value "$lock" schema)
  [[ "$schema" == 1 ]] || { printf '!! toolchain lock schema must be 1\n' >&2; return 1; }
  names=$(toolchain_lock_tool_names "$lock") || return 1
  [[ "$names" == $'claude\ncodex\nomp\nopencode\npi\nplaywright_core' ]] || {
    printf '!! toolchain lock must contain exactly the known tool entries\n' >&2
    return 1
  }
  for tool in claude codex opencode pi omp playwright_core; do
    expected=$(toolchain_expected_package "$tool")
    package=$(toolchain_lock_value "$lock" "$tool" package)
    version=$(toolchain_lock_value "$lock" "$tool" version)
    integrity=$(toolchain_lock_value "$lock" "$tool" integrity)
    before=$(toolchain_lock_value "$lock" "$tool" resolved_before)
    [[ "$package" == "$expected" ]] || { printf '!! invalid package for %s in toolchain lock\n' "$tool" >&2; return 1; }
    toolchain_valid_semver "$version" || { printf '!! invalid SemVer version for %s in toolchain lock\n' "$tool" >&2; return 1; }
    toolchain_valid_integrity "$integrity" || { printf '!! invalid sha512 integrity for %s in toolchain lock\n' "$tool" >&2; return 1; }
    toolchain_timestamp_not_future "$before" || { printf '!! resolved_before for %s must be a valid UTC timestamp that is not in the future\n' "$tool" >&2; return 1; }
  done
}
