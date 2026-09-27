#!/usr/bin/env bash
set -eu

usage() {
  cat <<'USAGE'
Usage: scripts/release/channel-json.sh --cli-version VERSION --cli-tag TAG \
  --cli-commit HEX --min-toolchain-sequence N --notes-url URL \
  --toolchain-sequence N --toolchain-tag TAG --toolchain-digest HEX12 \
  --min-cli-version VERSION --lock-url URL --lock-sha256 HEX64 \
  --attestation-url URL [--attestation-sha256 HEX64] [--cli-yanked JSON_ARRAY]
  [--toolchain-yanked JSON_ARRAY] [--generated-at UTC_TIMESTAMP] [--output PATH]
  [--dry-run]

Generate a schema-1 channel.json. Yank arguments are JSON arrays of strings;
toolchain yank entries are tool@version or a 12-character toolchain digest.
--dry-run validates and prints the manifest without writing --output.
USAGE
}

fail() {
  printf 'channel-json: %s\n' "$1" >&2
  exit 2
}

cli_version=
cli_tag=
cli_commit=
min_toolchain_sequence=
notes_url=
toolchain_sequence=
toolchain_tag=
toolchain_digest=
min_cli_version=
lock_url=
lock_sha256=
attestation_url=
attestation_sha256=
cli_yanked='[]'
toolchain_yanked='[]'
generated_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
output=
dry_run=0

while (($#)); do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --cli-version|--cli-tag|--cli-commit|--min-toolchain-sequence|--notes-url|\
    --toolchain-sequence|--toolchain-tag|--toolchain-digest|--min-cli-version|\
    --lock-url|--lock-sha256|--attestation-url|--attestation-sha256|--cli-yanked|\
    --toolchain-yanked|--generated-at|--output)
      (($# >= 2)) || fail "missing value for $1"
      key=$1
      value=$2
      case "$key" in
        --cli-version) cli_version=$value ;;
        --cli-tag) cli_tag=$value ;;
        --cli-commit) cli_commit=$value ;;
        --min-toolchain-sequence) min_toolchain_sequence=$value ;;
        --notes-url) notes_url=$value ;;
        --toolchain-sequence) toolchain_sequence=$value ;;
        --toolchain-tag) toolchain_tag=$value ;;
        --toolchain-digest) toolchain_digest=$value ;;
        --min-cli-version) min_cli_version=$value ;;
        --lock-url) lock_url=$value ;;
        --lock-sha256) lock_sha256=$value ;;
        --attestation-url) attestation_url=$value ;;
        --attestation-sha256) attestation_sha256=$value ;;
        --cli-yanked) cli_yanked=$value ;;
        --toolchain-yanked) toolchain_yanked=$value ;;
        --generated-at) generated_at=$value ;;
        --output) output=$value ;;
      esac
      shift 2
      ;;
    --dry-run) dry_run=1; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done

for required in cli_version cli_tag cli_commit min_toolchain_sequence notes_url \
  toolchain_sequence toolchain_tag toolchain_digest min_cli_version lock_url \
  lock_sha256 attestation_url; do
  [ -n "${!required}" ] || fail "missing required value: $required"
done

semver_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
[[ "$cli_version" =~ $semver_re ]] || fail 'CLI version is not SemVer'
[[ ${#cli_version} -le 64 ]] || fail 'CLI version is too long'
[[ "$cli_tag" == "v$cli_version" ]] || fail 'CLI tag must equal v<version>'
[[ ${#cli_tag} -le 72 ]] || fail 'CLI tag is too long'
[[ "$cli_commit" =~ ^[0-9a-f]{7,64}$ ]] || fail 'CLI commit must be 7-64 lowercase hex characters'
[[ "$min_cli_version" =~ $semver_re ]] || fail 'minimum CLI version is not SemVer'
[[ ${#min_cli_version} -le 64 ]] || fail 'minimum CLI version is too long'
[[ "$toolchain_digest" =~ ^[0-9a-f]{12}$ ]] || fail 'toolchain digest must be 12 lowercase hex characters'
[[ "$lock_sha256" =~ ^[0-9a-f]{64}$ ]] || fail 'lock SHA-256 must be 64 lowercase hex characters'
[[ -z "$attestation_sha256" || "$attestation_sha256" =~ ^[0-9a-f]{64}$ ]] || fail 'attestation SHA-256 must be 64 lowercase hex characters'
[[ "$toolchain_sequence" =~ ^[1-9][0-9]*$ ]] || fail 'toolchain sequence must be a positive integer'
[[ "$min_toolchain_sequence" =~ ^[1-9][0-9]*$ ]] || fail 'minimum toolchain sequence must be a positive integer'

max_json_integer=9007199254740991
(( toolchain_sequence <= max_json_integer )) 2>/dev/null || fail 'toolchain sequence exceeds the exact JSON integer range'
(( min_toolchain_sequence <= max_json_integer )) 2>/dev/null || fail 'minimum toolchain sequence exceeds the exact JSON integer range'
[[ "$toolchain_tag" == "toolchain-$toolchain_sequence" ]] || fail 'toolchain tag must equal toolchain-<sequence>'
[[ "$generated_at" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] || fail 'generated_at must be a UTC RFC 3339 timestamp ending in Z'

for url_name in notes_url lock_url attestation_url; do
  url=${!url_name}
  [[ "$url" == https://* ]] || fail "$url_name must use HTTPS"
  [[ ${#url} -le 2048 ]] || fail "$url_name is too long"
done

json=$(jq -cn \
  --arg generated_at "$generated_at" \
  --arg cli_version "$cli_version" \
  --arg cli_tag "$cli_tag" \
  --arg cli_commit "$cli_commit" \
  --argjson min_toolchain_sequence "$min_toolchain_sequence" \
  --arg notes_url "$notes_url" \
  --argjson cli_yanked "$cli_yanked" \
  --argjson toolchain_sequence "$toolchain_sequence" \
  --arg toolchain_tag "$toolchain_tag" \
  --arg toolchain_digest "$toolchain_digest" \
  --arg min_cli_version "$min_cli_version" \
  --arg lock_url "$lock_url" \
  --arg lock_sha256 "$lock_sha256" \
  --arg attestation_url "$attestation_url" \
  --arg attestation_sha256 "$attestation_sha256" \
  --argjson toolchain_yanked "$toolchain_yanked" \
  '{schema:1, generated_at:$generated_at, cli:{version:$cli_version, tag:$cli_tag, commit:$cli_commit, min_toolchain_sequence:$min_toolchain_sequence, notes_url:$notes_url, yanked_versions:$cli_yanked}, toolchain:({sequence:$toolchain_sequence, tag:$toolchain_tag, digest:$toolchain_digest, min_cli_version:$min_cli_version, lock_url:$lock_url, lock_sha256:$lock_sha256, attestation_url:$attestation_url} + (if $attestation_sha256 == "" then {} else {attestation_sha256:$attestation_sha256} end) + {yanked:$toolchain_yanked})}') || fail 'could not construct JSON'

printf '%s\n' "$json" | jq -e '
  def semver: type == "string" and length <= 64 and test("^(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\\+[0-9A-Za-z.-]+)?$");
  def https_url: type == "string" and length <= 2048 and startswith("https://");
  def string_array($max_len): type == "array" and length <= 256 and all(.[]; type == "string" and length > 0 and length <= $max_len);
  .schema == 1 and
  (.generated_at | type == "string" and test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")) and
  (.cli | (.version | semver) and .tag == ("v" + .version) and (.commit | type == "string" and test("^[0-9a-f]{7,64}$")) and (.min_toolchain_sequence | type == "number" and floor == . and . >= 1 and . <= 9007199254740991) and (.notes_url | https_url) and (.yanked_versions | string_array(64) and all(.[]; test("^(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\\+[0-9A-Za-z.-]+)?$")))) and
  (.toolchain | (.sequence | type == "number" and floor == . and . >= 1 and . <= 9007199254740991) and .tag == ("toolchain-" + (.sequence|tostring)) and (.digest | type == "string" and test("^[0-9a-f]{12}$")) and (.min_cli_version | semver) and (.lock_url | https_url) and (.lock_sha256 | type == "string" and test("^[0-9a-f]{64}$")) and (.attestation_url | https_url) and ((.attestation_sha256 // null) == null or (.attestation_sha256 | type == "string" and test("^[0-9a-f]{64}$"))) and (.yanked | string_array(160) and all(.[]; test("^[0-9a-f]{12}$") or test("^(claude|codex|opencode|pi|omp|playwright_core)@(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\\+[0-9A-Za-z.-]+)?$"))))
' >/dev/null || fail 'manifest did not satisfy channel-format.md'

if ((dry_run)); then
  printf '%s\n' "$json" | jq .
elif [[ -n "$output" ]]; then
  output_dir=$(dirname "$output")
  mkdir -p "$output_dir"
  temp_output=$(mktemp "$output_dir/.channel-json.XXXXXX")
  printf '%s\n' "$json" | jq . >"$temp_output"
  mv "$temp_output" "$output"
else
  printf '%s\n' "$json" | jq .
fi
