#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

cat > "$tmp/fake-tool" <<'TOOL'
#!/bin/sh
printf '<%s>\n' "$@"
TOOL
chmod 0755 "$tmp/fake-tool"
cp images/tool-launcher.sh "$tmp/capscode-tool-launcher"
chmod 0755 "$tmp/capscode-tool-launcher"

for tool in pi omp; do
  if [ "$tool" = pi ]; then
    route_key=CAPSCODE_PI_API_KEY
    model_key=CAPSCODE_PI_DEFAULT_MODEL
    provider_key=CAPSCODE_PI_DEFAULT_PROVIDER
  else
    route_key=CAPSCODE_OMP_API_KEY
    model_key=CAPSCODE_OMP_DEFAULT_MODEL
    provider_key=CAPSCODE_OMP_DEFAULT_PROVIDER
  fi

  # The provider comes from the host's catalog entry, never a fixed per-tool name.
  expected_provider=capscode-openai
  ln -s "$tmp/fake-tool" "$tmp/$tool"
  CAPSCODE_SHIM_DIR="$tmp" \
  CAPSCODE_TOOL_LAUNCHER="$tmp/capscode-tool-launcher" \
    sh images/tool-shim.sh "$tool" "$route_key" "$tmp/fake-tool"

  output=$(env "$route_key=placeholder" "$model_key=default/model" "$provider_key=$expected_provider" "$tmp/$tool" prompt)
  [[ "$output" == "<--provider>"$'\n'"<$expected_provider>"$'\n'"<--model>"$'\n'"<default/model>"$'\n'"<prompt>" ]]

  output=$(env "$route_key=placeholder" "$model_key=default/model" "$provider_key=$expected_provider" "$tmp/$tool" --model custom/model prompt)
  [[ "$output" == "<--provider>"$'\n'"<$expected_provider>"$'\n'"<--model>"$'\n'"<custom/model>"$'\n'"<prompt>" ]]

  if env "$route_key=placeholder" "$model_key=default/model" "$tmp/$tool" prompt >"$tmp/out" 2>"$tmp/err"; then
    echo "$tool started without a default provider" >&2
    exit 1
  fi
  grep -q "its default provider is missing in this box" "$tmp/err"

  for forbidden in --provider --api-key --profile; do
    if env "$route_key=placeholder" "$model_key=default/model" "$provider_key=$expected_provider" "$tmp/$tool" "$forbidden" value >"$tmp/out" 2>"$tmp/err"; then
      echo "$tool accepted $forbidden" >&2
      exit 1
    fi
    grep -q "managed by this box" "$tmp/err"
  done
done

for route in \
  ANTHROPIC_BASE_URL:claude \
  OPENAI_BASE_URL:codex \
  CAPSCODE_PI_API_KEY:pi \
  CAPSCODE_OMP_API_KEY:omp \
  CAPSCODE_OPENCODE_API_KEY:opencode; do
  route_key=${route%%:*}
  tool=${route#*:}
  output=$(env "$route_key=placeholder" sh images/tool-launcher.sh "$route_key" "$tmp/fake-tool" ok)
  [[ "$output" == '<ok>' ]]
  if env "$route_key=" sh images/tool-launcher.sh "$route_key" "$tmp/fake-tool" >"$tmp/out" 2>"$tmp/err"; then
    echo "launcher accepted an empty route setting for $tool" >&2
    exit 1
  fi
  grep -q "$tool: its provider is not routed in this box" "$tmp/err"
done

echo 'box tool shim checks passed'
