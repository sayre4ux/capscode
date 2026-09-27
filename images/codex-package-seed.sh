#!/bin/sh

# Create Codex's managed daemon package links to the binary already installed in
# the pinned image. The user's home stays noexec, so a copied daemon cannot run.
seed_codex_daemon_package() {
  _codex_vendor=${1:-}
  _codex_home=${2:-/home/agent}
  _codex_user=${3:-agent}
  _codex_group=${4:-$_codex_user}

  if [ "${CAPSCODE_SMOKE_SKIP_CODEX_SEED:-0}" = "1" ]; then
    log "WARN: Codex daemon seed skipped by the smoke negative control"
    return 0
  fi

  if [ -z "$_codex_vendor" ]; then
    for _codex_candidate in /usr/local/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-*/vendor/*-unknown-linux-musl; do
      [ -d "$_codex_candidate" ] || continue
      if [ -n "$_codex_vendor" ]; then
        log "WARN: multiple Codex daemon vendor directories found; skipping the seed"
        return 0
      fi
      _codex_vendor=$_codex_candidate
    done
  fi

  case "$_codex_home" in
    /*) ;;
    *) log "WARN: Codex daemon seed home is not an absolute path; skipping the seed"; return 0 ;;
  esac
  if [ -z "$_codex_vendor" ]; then
    return 0
  fi
  if [ ! -f "$_codex_vendor/codex-package.json" ]; then
    log "WARN: Codex daemon manifest was not found; skipping the seed"
    return 0
  fi

  _codex_manifest="$_codex_vendor/codex-package.json"
  _codex_layout=$(sed -n 's/.*"layoutVersion"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$_codex_manifest" 2>/dev/null || true)
  _codex_version=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$_codex_manifest" 2>/dev/null || true)
  _codex_target=$(sed -n 's/.*"target"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$_codex_manifest" 2>/dev/null || true)
  if [ "$_codex_layout" != "1" ]; then
    log "WARN: Codex daemon package layout is not supported; skipping the seed"
    return 0
  fi
  case "$_codex_version" in
    ''|*[!A-Za-z0-9._+-]*)
      log "WARN: Codex daemon package version is not valid; skipping the seed"
      return 0
      ;;
  esac
  case "$_codex_target" in
    ''|*[!A-Za-z0-9._+-]*)
      log "WARN: Codex daemon package target is not valid; skipping the seed"
      return 0
      ;;
  esac

  _codex_package="$_codex_home/.codex/packages/app-server-daemon"
  _codex_release="$_codex_package/releases/$_codex_version-$_codex_target"
  _codex_current="$_codex_package/current"
  if ! install -d -m 755 -o "$_codex_user" -g "$_codex_group" \
    "$_codex_home/.codex" "$_codex_home/.codex/packages" \
    "$_codex_package" "$_codex_package/releases"; then
    log "WARN: could not prepare Codex daemon package directories; skipping the seed"
    return 0
  fi

  if [ -e "$_codex_release" ] || [ -L "$_codex_release" ] || [ -e "$_codex_current" ] || [ -L "$_codex_current" ]; then
    if [ ! -L "$_codex_release" ] || [ "$(readlink "$_codex_release" 2>/dev/null || true)" != "$_codex_vendor" ] \
      || [ ! -L "$_codex_current" ] || [ "$(readlink "$_codex_current" 2>/dev/null || true)" != "$_codex_release" ]; then
      log "WARN: Codex daemon package paths already exist; skipping the seed"
      return 0
    fi
  else
    if ! ln -s "$_codex_vendor" "$_codex_release" || ! ln -s "$_codex_release" "$_codex_current"; then
      log "WARN: could not create Codex daemon package links; skipping the seed"
      return 0
    fi
  fi

  if ! chown -h "$_codex_user:$_codex_group" "$_codex_release" "$_codex_current"; then
    log "WARN: could not assign Codex daemon package links to the agent user"
    return 0
  fi
  log "seeded Codex daemon package $_codex_version-$_codex_target from the image"
  return 0
}
