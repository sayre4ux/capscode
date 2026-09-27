#!/usr/bin/env bash
set -eu

section=
manifest=
while (($#)); do
  case "$1" in
    --section)
      (($# >= 2)) || { printf '%s\n' 'Missing --section value' >&2; exit 2; }
      section=$2
      shift 2
      ;;
    --manifest)
      (($# >= 2)) || { printf '%s\n' 'Missing --manifest path' >&2; exit 2; }
      manifest=$2
      shift 2
      ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done

[[ "$section" == cli || "$section" == toolchain ]] || { printf '%s\n' 'Section must be cli or toolchain' >&2; exit 2; }
[[ -f "$manifest" ]] || { printf 'Manifest not found: %s\n' "$manifest" >&2; exit 1; }
jq -e --arg section "$section" '.schema == 1 and (.[$section]|type == "object")' "$manifest" >/dev/null || {
  printf '%s\n' 'Prepared manifest failed basic schema validation' >&2
  exit 1
}
jq --arg section "$section" --slurpfile prepared "$manifest" '.[$section] = $prepared[0][$section]'
