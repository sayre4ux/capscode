#!/bin/sh
# CapsCode gateway entrypoint .
#
# The gateway is the ONLY privileged container (NET_ADMIN + /dev/net/tun). It
# owns the network namespace the capsule later shares. It runs NO user/AI code:
# its whole job is to (1) bring up the userspace WireGuard tunnel, (2) apply the
# nftables kill switch fail-closed, and (3) run the secret broker as PID 1.
#
# Ordering rationale (why the tunnel comes up BEFORE the kill switch is applied):
# the gateway is trusted and no untrusted code exists in this netns yet — the
# capsule is started by the host ONLY after `NetworkStack::verify` passes, which
# the host runs after this script has applied the kill switch. So the brief
# unfiltered window while we negotiate the handshake never overlaps the tool.
# Canonical WireGuard configs contain no hooks, and the capsule is not running yet.
# The @wg_peer / @host_gw / @sinkhole4 sets are boot-filled here :
# their values are unknown at host-render time.
#
# Environment consumed (all injected by capscode-net::spec::gateway_spec):
#   CAPSCODE_NET_MODE        `tunnel` | `containment-only`
#   CAPSCODE_SECRETS_DIR     path to the secrets tmpfs the host CLI injects into
#                            (default /run/capscode). NO secret is ever in env.
#   CAPSCODE_TUNNEL_DNS      space-separated tunnel resolver(s)
#   CAPSCODE_KILLSWITCH_NFT  rendered `nft -f` ruleset (empty boot sets)
#   CAPSCODE_SINKHOLE        `1` | `0`
#   CAPSCODE_SINKHOLE_HOSTS  space-separated telemetry hostnames (when SINKHOLE=1)
#   CAPSCODE_CA_CERT_PEM     host-generated CA cert PEM (public). NOTE: the broker
#                            generates its OWN per-session CA in-process (its key
#                            never leaves this process) and exports the public
#                            cert to CAPSCODE_CA_CERT_OUT for capsule trust. The
#                            orchestrator MUST inject THAT exported cert into the
#                            capsule (not a separately host-generated one).
#   CAPSCODE_CA_CERT_OUT     path the broker writes its public CA cert PEM to
#                            (default /etc/capscode/broker-ca.pem)
#   CAPSCODE_BROKER_LISTEN   loopback endpoint the broker binds (127.0.0.1:8443)
#
# Secrets injected post-start into $CAPSCODE_SECRETS_DIR by the host CLI (over an
# exec stdin pipe, never env/disk): keys.env (read by the broker), wg0.conf (read
# below in tunnel mode), and a `ready` sentinel that gates this whole script.
set -eu

log() { printf 'capscode-gateway: %s\n' "$*" >&2; }
die() { log "FATAL: $*"; exit 1; }

NET_MODE="${CAPSCODE_NET_MODE:-containment-only}"
SECRETS_DIR="${CAPSCODE_SECRETS_DIR:-/run/capscode}"
NFT_FILE=/etc/capscode/killswitch.nft
WG_CONF="$SECRETS_DIR/wg0.conf"
BROKER_BIN=/usr/local/bin/capscode-broker

# --- 0. block until the host has injected the secrets (fail-closed) ------------
# The host CLI writes keys.env (+ wg0.conf in tunnel mode) into $SECRETS_DIR, then
# a `ready` sentinel. Until that appears the broker cannot serve and the tunnel
# does not come up. Bounded so a stuck/absent host cannot hang the container.
i=0
while [ ! -f "$SECRETS_DIR/ready" ]; do
  i=$((i + 1))
  [ "$i" -gt 30 ] && die "secrets not injected within 30s (no $SECRETS_DIR/ready) — refusing to start"
  sleep 1
done
log "secrets injected; proceeding"

# --- 1. bring up the userspace WireGuard tunnel (tunnel mode only) -------------
# Userspace impl so the gateway needs only NET_ADMIN + /dev/net/tun, never a host
# kernel module . The config is read straight from the tmpfs so the
# private key never lands on the container's writable layer.
export WG_QUICK_USERSPACE_IMPLEMENTATION=wireguard-go
export LOG_LEVEL=silent

# --- 2. resolve DNS-dependent boot-set values BEFORE the tunnel reroutes egress
# In tunnel mode, `wg-quick up` (below) reroutes ALL egress through wg0. Running
# these getent lookups afterwards — before the tunnel DNS is pinned and the
# handshake has settled — sends them through the not-yet-ready tunnel and hangs
# the whole bring-up (one timeout per host, many hosts). The host_gw / sinkhole
# addresses are identical regardless of the outgoing interface, so resolve them
# here, off-tunnel, while ordinary egress is still fast.
#
# @host_gw := the runtime's host-gateway addresses.
# Two independent sources, because the authoritative host address differs per
# runtime and is often NOT the default-route gateway:
#   (1) the default-route gateway (the classic bridge gateway), and
#   (2) the per-runtime host aliases host.docker.internal / host.orbstack.internal
#       / gateway.docker.internal, which on OrbStack resolve into 198.19/16 (NOT
#       RFC1918) and on Docker Desktop into 192.168/16.
# Layer (b) — the static 198.19/16 + RFC1918 containment set in the ruleset —
# already drops the host even if NONE of these resolve; this set is the belt-and-
# suspenders exact-address layer for host addresses outside those ranges.
host_gw_addrs=$(ip -4 route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="via") print $(i+1)}' | sort -u || true)
for hg_alias in host.docker.internal host.orbstack.internal gateway.docker.internal; do
  a=$(getent ahostsv4 "$hg_alias" 2>/dev/null | awk '{print $1}' | sort -u || true)
  [ -n "$a" ] && host_gw_addrs="$host_gw_addrs $a"
done
# Deduplicate (multiple aliases frequently resolve to the same host address).
if [ -n "$host_gw_addrs" ]; then
  # shellcheck disable=SC2086
  host_gw_addrs=$(printf '%s\n' $host_gw_addrs | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | sort -u | tr '\n' ' ')
fi

# @sinkhole4 := best-effort resolution of the telemetry hostnames:
# the drop is authoritative at the nft layer regardless of any tool setting).
sinkhole_addrs=""
if [ "${CAPSCODE_SINKHOLE:-0}" = "1" ] && [ -n "${CAPSCODE_SINKHOLE_HOSTS:-}" ]; then
  for h in $CAPSCODE_SINKHOLE_HOSTS; do
    a=$(getent ahostsv4 "$h" 2>/dev/null | awk '{print $1}' | sort -u || true)
    [ -n "$a" ] && sinkhole_addrs="$sinkhole_addrs $a"
  done
fi

# --- 3. bring up the userspace WireGuard tunnel (tunnel mode only) -------------
wg_peer_addrs=""
if [ "$NET_MODE" = "tunnel" ]; then
  [ -f "$WG_CONF" ] || die "tunnel mode but $WG_CONF was not injected"
  log "bringing up wg0 from tmpfs (userspace wireguard-go)"
  wg-quick up "$WG_CONF" || die "wg-quick up failed"

  # @wg_peer := the actually-negotiated peer endpoint address. `wg show` returns
  # the resolved peer IP directly (works even for a hostname Endpoint, since
  # wg-quick already resolved it to connect); the getent fallback is a single
  # lookup only hit if `wg show` yields nothing.
  wg_peer_addrs=$(wg show wg0 endpoints 2>/dev/null \
    | awk '{print $2}' | sed 's/:[0-9]*$//; s/^\[//; s/\]$//' \
    | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' || true)
  if [ -z "$wg_peer_addrs" ]; then
    ep_host=$(awk -F'[=:]' 'tolower($1) ~ /endpoint/ {gsub(/ /,"",$2); print $2}' "$WG_CONF" | head -1)
    if [ -n "${ep_host:-}" ]; then
      wg_peer_addrs=$(getent ahostsv4 "$ep_host" 2>/dev/null | awk '{print $1}' | sort -u || true)
    fi
  fi
  # The kill switch passes WireGuard packets on the uplink only to @wg_peer, so
  # an empty set means a dead tunnel. Refuse to start instead of running without
  # working egress.
  [ -n "$wg_peer_addrs" ] \
    || die "no IPv4 peer endpoint address from 'wg show' or the Endpoint lookup — refusing to start with an unusable tunnel"
fi

# --- 3. apply the kill switch (fail-closed) -----------------------------------
install -d -m 755 /etc/capscode
if [ -n "${CAPSCODE_KILLSWITCH_NFT:-}" ]; then
  printf '%s\n' "$CAPSCODE_KILLSWITCH_NFT" > "$NFT_FILE"
elif [ ! -f "$NFT_FILE" ]; then
  die "no kill-switch ruleset (CAPSCODE_KILLSWITCH_NFT unset and no baked $NFT_FILE)"
else
  log "WARN: using baked reference ruleset; empty boot sets not session-specific"
fi
nft -f "$NFT_FILE" || die "nft -f failed — refusing to run without the kill switch"

# Populate the boot sets. host_gw / sinkhole4 are best-effort per element (the
# static containment ranges already cover the usual host addresses). wg_peer is
# required in tunnel mode: without it the tunnel's own packets are dropped.
add_elems() {
  required=$1; set_name=$2; shift 2
  for ip in "$@"; do
    [ -n "$ip" ] || continue
    if nft add element inet capscode "$set_name" "{ $ip }" 2>/dev/null; then
      log "added $ip to @$set_name"
    elif [ "$required" = required ]; then
      die "could not add $ip to @$set_name — refusing to start with an unusable tunnel"
    else
      log "WARN: could not add $ip to @$set_name (set absent or dup)"
    fi
  done
}
# shellcheck disable=SC2086
[ -n "$wg_peer_addrs" ]  && add_elems required    wg_peer   $wg_peer_addrs
# shellcheck disable=SC2086
[ -n "$host_gw_addrs" ]  && add_elems best-effort host_gw   $host_gw_addrs
# shellcheck disable=SC2086
[ -n "$sinkhole_addrs" ] && add_elems best-effort sinkhole4 $sinkhole_addrs

log "kill switch active (mode=$NET_MODE)"

# --- 3b. shared /etc/hosts + resolv.conf (inherited by the capsule) -----------
# The capsule joins THIS container's network namespace, so Docker mounts this
# /etc/hosts and /etc/resolv.conf into the capsule too. The capsule rootfs is
# read-only and cannot write them, so we set them here: broker vhost aliases →
# 127.0.0.1 (the broker binds loopback in this shared netns), telemetry sinkhole
# → 0.0.0.0, and DNS pinned to the tunnel resolver in tunnel mode.
BROKER_ALIASES="${CAPSCODE_BROKER_ALIASES:-anthropic.broker.capscode.local openai.broker.capscode.local moonshot.broker.capscode.local}"
{
  echo "# --- capscode broker vhosts ---"
  for h in $BROKER_ALIASES; do
    echo "127.0.0.1 $h"
  done
  if [ "${CAPSCODE_SINKHOLE:-0}" = "1" ] && [ -n "${CAPSCODE_SINKHOLE_HOSTS:-}" ]; then
    echo "# --- capscode telemetry sinkhole ---"
    for h in $CAPSCODE_SINKHOLE_HOSTS; do
      echo "0.0.0.0 $h"
    done
  fi
} >> /etc/hosts
if [ -n "${CAPSCODE_TUNNEL_DNS:-}" ]; then
  {
    echo "# capscode: DNS pinned to the tunnel resolver"
    for ns in $CAPSCODE_TUNNEL_DNS; do
      echo "nameserver $ns"
    done
    echo "options ndots:0"
  } > /etc/resolv.conf
fi

# --- 4. run the secret broker as PID 1 ----------------------------------------
# The broker reads CAPSCODE_BROKER_LISTEN from env and the real provider keys from
# $CAPSCODE_SECRETS_DIR/keys.env (which it unlinks after loading), generates the
# per-session CA in-process (its private key never
# leaves this process), and exports the public CA cert to CAPSCODE_CA_CERT_OUT
# for the orchestrator to install into the capsule trust store. exec → PID 1 →
# clean SIGTERM handling → its exit tears the gateway down cleanly.
[ -x "$BROKER_BIN" ] || die "broker binary missing/not executable at $BROKER_BIN (image build is broken)"
export CAPSCODE_CA_CERT_OUT="${CAPSCODE_CA_CERT_OUT:-/etc/capscode/broker-ca.pem}"
log "starting secret broker on ${CAPSCODE_BROKER_LISTEN:-127.0.0.1:8443}"
exec "$BROKER_BIN"
