#!/bin/sh
# CapsCode capsule entrypoint.
#
# Runs as root only long enough to normalise identity and install the injected
# CA, then drops to `agent` and execs the capsule's long-lived command as PID 1
# (`sleep` for classic sessions, the keeper for devboxes). The CLI starts tools
# later with docker exec or inside the keeper's tmux pane.
#
# Environment consumed (all injected by the CLI; NEVER a real provider key):
#   CAPSCODE_CA_CERT_PEM   per-session CA certificate (PEM) to trust
#   CAPSCODE_TUNNEL_DNS    space-separated tunnel resolver(s) to pin resolv.conf to
#   CAPSCODE_SINKHOLE_HOSTS space-separated telemetry hostnames to null-route
#   CAPSCODE_BROKER_ALIASES space-separated broker vhosts to map to 127.0.0.1
#                          (default: the three provider broker vhosts)
#   CAPSCODE_TOOL_ENV_KEYS CLI-generated allowlist for the keeper and tool env
set -eu

AGENT_USER=agent
AGENT_HOME=/home/agent
# Keeper (devbox mode): the long-lived tmux server. It runs as AGENT, because
# tmux spawns panes as the server's uid and the tool must be `agent` (which owns
# /workspace); a separate uid would need CAP_SETUID to hand panes back to agent.
# See images/capsule.all.Dockerfile for the accepted residual.
KEEPER_RUN_DIR=/run/capscode-keeper
KEEPER_SOCKET="$KEEPER_RUN_DIR/sock"
CA_DIR="$AGENT_HOME/.capscode"
CA_FILE="$CA_DIR/ca.pem"

log() { printf 'capscode-capsule: %s\n' "$*" >&2; }
die() { log "FATAL: $*"; exit 1; }
# shellcheck source=images/codex-package-seed.sh
. /usr/local/lib/capscode/codex-package-seed.sh
# shellcheck source=images/cache-owner.sh
. /usr/local/lib/capscode/cache-owner.sh

# Quote one argument for the eval below. Values may contain PEM newlines or
# shell punctuation, so never interpolate them into a command unquoted.
shell_quote() {
  _sq_rest=$1
  _sq_quote="'"
  _sq_out="'"
  while :; do
    case "$_sq_rest" in
      *"$_sq_quote"*)
        _sq_part=${_sq_rest%%"$_sq_quote"*}
        _sq_rest=${_sq_rest#*"$_sq_quote"}
        _sq_out="$_sq_out$_sq_part'\\''"
        ;;
      *)
        _sq_out="$_sq_out$_sq_rest'"
        break
        ;;
    esac
  done
  printf '%s' "$_sq_out"
}

# Both modes construct their process environment from the keys the CLI put in
# the capsule spec. The keeper adds only the small set of values its supervisor
# needs; the tool keys themselves are never duplicated in an image allowlist.
run_with_capsule_env() {
  [ -n "${CAPSCODE_TOOL_ENV_KEYS:-}" ] \
    || die "CAPSCODE_TOOL_ENV_KEYS is missing; refusing to start the capsule command"

  _env_command="/usr/bin/env -i"
  for _assignment in \
      "HOME=$AGENT_HOME" \
      "PATH=/usr/local/bin:/usr/bin:/bin" \
      "HISTFILE=/dev/null" \
      "TZ=UTC" \
      "LANG=C.UTF-8" \
      "NODE_PATH=${NODE_PATH:-}"; do
    _env_command="$_env_command $(shell_quote "$_assignment")"
  done

  if [ "${CAPSCODE_KEEPER:-}" = "1" ]; then
    for _assignment in \
        "DISABLE_AUTOUPDATER=1" \
        "CAPSCODE_CODEX_DAEMON=${CAPSCODE_CODEX_DAEMON:-}" \
        "CAPSCODE_BOX=${CAPSCODE_BOX:-}" \
        "KEEPER_SOCKET=$KEEPER_SOCKET" \
        "KEEPER_AUTOSYNC=${KEEPER_AUTOSYNC:-1}" \
        "KEEPER_AUTOSYNC_SECS=${KEEPER_AUTOSYNC_SECS:-300}" \
        "KEEPER_GRACE_SECS=${KEEPER_GRACE_SECS:-3}"; do
      _env_command="$_env_command $(shell_quote "$_assignment")"
    done
  fi

  for _key in $CAPSCODE_TOOL_ENV_KEYS; do
    case "$_key" in
      ''|[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_]*|*[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_]*)
        die "invalid key in CAPSCODE_TOOL_ENV_KEYS"
        ;;
    esac
    eval "_value=\${$_key-}"
    # The allowlisted name is read indirectly into _value above.
    # shellcheck disable=SC2154
    _env_command="$_env_command $(shell_quote "$_key=$_value")"
  done
  for _argument in "$@"; do
    _env_command="$_env_command $(shell_quote "$_argument")"
  done

  eval "exec /usr/bin/setpriv --reuid '$AGENT_USER' --regid '$AGENT_USER' --init-groups $_env_command"
}

# --- 1. identity assertions (fail-closed on env, best-effort on hostname) -----
[ "${TZ:-}" = "UTC" ] || die "TZ must be UTC, got '${TZ:-unset}'"
case "${LANG:-}" in
  C.UTF-8|C.utf8) : ;;
  *) die "LANG must be C.UTF-8, got '${LANG:-unset}'" ;;
esac

# Hostname is normalised to `workspace`. Under a shared network namespace the
# runtime cannot pass --hostname, and the unprivileged capsule cannot call
# sethostname(2); in that case the UTS namespace must be shared with a gateway
# whose hostname is already `workspace` (see IMPLEMENTATION NOTES). We attempt
# to set it and only warn if we cannot, rather than break the session.
if [ "$(hostname)" != "workspace" ]; then
  hostname workspace 2>/dev/null \
    || log "WARN: hostname is '$(hostname)', expected 'workspace' (UTS not settable here)"
fi

# --- 1b. hand $HOME itself to the agent user ----------------------------------
# /home/agent is a bare tmpfs mount, so it arrives root:root 0755: the tool (uid
# 10001) can read and traverse it but cannot create anything in it. Tools whose
# state lives in a subdirectory of $HOME then die — Claude Code cannot make
# ~/.claude, so its bash tool and transcript writes fail with EACCES. The
# injected-config script only chowns levels it creates, and a top-level config
# file (~/.claude.json) has none, so $HOME itself has to be fixed here.
chown "$AGENT_USER:$AGENT_USER" "$AGENT_HOME"
chmod 700 "$AGENT_HOME"
ensure_cache_owner /cache "$AGENT_USER" 10001 10001

# --- 1c. use the pinned Codex daemon package from the image ------------------
# The seeded links let Codex use its managed daemon without copying a 385 MB
# executable into this noexec home tmpfs. Unknown layouts warn and leave Codex's
# own fallback in control.
seed_codex_daemon_package "" "$AGENT_HOME" "$AGENT_USER" "$AGENT_USER"

# --- 2. install the injected per-session CA -----------------------------------
install -d -m 700 -o "$AGENT_USER" -g "$AGENT_USER" "$CA_DIR"
if [ -n "${CAPSCODE_CA_CERT_PEM:-}" ]; then
  printf '%s\n' "$CAPSCODE_CA_CERT_PEM" > "$CA_FILE"
elif [ -f /run/capscode/ca.pem ]; then
  cp /run/capscode/ca.pem "$CA_FILE"
fi
if [ -f "$CA_FILE" ]; then
  chown "$AGENT_USER:$AGENT_USER" "$CA_FILE"
  chmod 644 "$CA_FILE"
  # System trust store, only if the rootfs is writable (--mount mode); on a
  # read-only rootfs the env-var trust below is authoritative.
  if cp "$CA_FILE" /usr/local/share/ca-certificates/capscode-session.crt 2>/dev/null; then
    update-ca-certificates >/dev/null 2>&1 || true
  fi
  export NODE_EXTRA_CA_CERTS="$CA_FILE"
  export SSL_CERT_FILE="$CA_FILE"
  export REQUESTS_CA_BUNDLE="$CA_FILE"
  export CURL_CA_BUNDLE="$CA_FILE"
  export GIT_SSL_CAINFO="$CA_FILE"
  log "installed session CA ($CA_FILE)"
else
  log "WARN: no session CA provided; broker TLS will not be trusted"
fi

# --- 3. /etc/hosts + resolv.conf are the GATEWAY's ----------------------------
# The capsule joins the gateway's network namespace (`--network container:<gw>`),
# so Docker mounts the GATEWAY's /etc/hosts and /etc/resolv.conf here — read-only
# under our read-only rootfs. The broker vhost aliases, telemetry sinkhole, and
# tunnel-DNS pinning are therefore written by the gateway entrypoint (which owns
# those files) and inherited here. This capsule must not touch them.

# --- 4. temporary Git identity and disabled history ---------------------------
export HOME="$AGENT_HOME"
export HISTFILE=/dev/null
export GIT_CONFIG_GLOBAL="$AGENT_HOME/.gitconfig"
cat > "$AGENT_HOME/.gitconfig" <<'EOF'
[user]
	name = capsule
	email = capsule@localhost
[init]
	defaultBranch = main
[advice]
	detachedHead = false
EOF
chown "$AGENT_USER:$AGENT_USER" "$AGENT_HOME/.gitconfig"

# A box shell gets a visible prompt in every login shell. Keep this conditional
# so classic one-shot sessions retain their existing shell startup behavior.
if [ -n "${CAPSCODE_BOX:-}" ]; then
  prompt="[capscode:${CAPSCODE_BOX}] \\w \$ "
  printf 'PS1=%s\n' "$(shell_quote "$prompt")" > "$AGENT_HOME/.bashrc"
  chown "$AGENT_USER:$AGENT_USER" "$AGENT_HOME/.bashrc"
  # shellcheck disable=SC2016
  printf '%s\n' '[ -r "$HOME/.bashrc" ] && . "$HOME/.bashrc"' > "$AGENT_HOME/.bash_profile"
  chown "$AGENT_USER:$AGENT_USER" "$AGENT_HOME/.bash_profile"
fi

# --- 6. hand the workspace to the agent user ----------------------------------
# The ingress helper seeds /workspace as root; the tool and all git operations
# run as the unprivileged `agent`, so give agent ownership of its private copy
# (best-effort: an empty/absent volume is fine). Runs while still root.
if [ "${CAPSCODE_WORKSPACE_MODE:-copy}" != "mount" ] && [ -d /workspace ]; then
  chown -R "$AGENT_USER:$AGENT_USER" /workspace 2>/dev/null || \
    log "WARN: could not chown /workspace to $AGENT_USER"
fi

# --- 7. drop privileges and exec the capsule command --------------------------
cd /workspace 2>/dev/null || cd "$AGENT_HOME"

if [ "${CAPSCODE_KEEPER:-}" = "1" ]; then
  # The socket directory must live on a WRITABLE mount: the capsule runs with
  # read_only_rootfs, so the /run/capscode-keeper baked into the image is not
  # writable at runtime and tmux could not create its socket there. The runtime
  # mounts a tmpfs at /run/capscode-keeper; create the 0700 agent-owned
  # directory here, while still root, before dropping.
  install -d -m 0700 -o "$AGENT_USER" -g "$AGENT_USER" "$KEEPER_RUN_DIR" \
    || die "could not prepare keeper socket dir $KEEPER_RUN_DIR"

  log "keeper mode: starting tmux server as user $AGENT_USER"
  run_with_capsule_env /usr/local/bin/capscode-keeper
fi

[ "$#" -gt 0 ] || die "no tool command given"
log "starting '$*' as user $AGENT_USER"

run_with_capsule_env "$@"
