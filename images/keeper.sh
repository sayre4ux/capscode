#!/bin/sh
# CapsCode keeper — the capsule's long-lived process in devbox mode.
#
# Runs as uid `agent`, started by images/entrypoint.sh after its root-side setup.
# A separate `keeper` uid was the design's intent but is not achievable: tmux
# spawns panes as the SERVER's uid, and the tool must be `agent` (which owns
# /workspace). See images/capsule.all.Dockerfile for the accepted residual.
#
# Why this exists instead of `exec tmux -D`: tmux tears its panes down without
# passing SIGTERM to the processes inside them. A bare `exec tmux -D` therefore
# means `docker stop` reaches tmux, tmux kills the pane, and the AI tool never
# gets a chance to shut down cleanly or flush its state — measured, not assumed
# . This wrapper forwards the signal to each
# pane process first, waits briefly, then stops the server.
set -eu

SOCKET="${KEEPER_SOCKET:-/run/capscode-keeper/sock}"
CONF="${KEEPER_CONF:-/etc/capscode/tmux.conf}"
# How long pane processes get to exit on their own before the server goes away.
# Keep below the runtime's stop timeout or Docker SIGKILLs the whole container
# before this returns.
GRACE="${KEEPER_GRACE_SECS:-3}"

log() { echo "capscode-keeper: $*" >&2; }

# Every descendant of $1, deepest first, read straight from /proc so no procps
# dependency is added to the image. Depth is bounded: a tool that spawns a deep
# tree still terminates because each level is visited once.
# shellcheck disable=SC2329
descendants_of() {
    _parent=$1
    _found=""
    _frontier=$_parent
    _depth=0
    while [ -n "$_frontier" ] && [ "$_depth" -lt 16 ]; do
        _next=""
        for _p in $_frontier; do
            for _stat in /proc/[0-9]*/stat; do
                [ -r "$_stat" ] || continue
                # field 4 is PPID; the comm field can contain spaces, so cut on
                # the closing paren first.
                _line=${_stat#/proc/}
                _pid=${_line%%/*}
                _stat_line=""
                while IFS= read -r _stat_part || [ -n "$_stat_part" ]; do
                    _stat_line="$_stat_line$_stat_part"
                done < "$_stat"
                [ -n "$_stat_line" ] || continue
                # Strip pid + comm through its final ") ", then state; the
                # second remaining field is PPID (field 4 of /proc/PID/stat).
                _stat_line=${_stat_line##*) }
                _stat_line=${_stat_line#* }
                _ppid=${_stat_line%% *}
                [ "$_ppid" = "$_p" ] || continue
                _found="$_found $_pid"
                _next="$_next $_pid"
            done
        done
        _frontier=$_next
        _depth=$((_depth + 1))
    done
    echo "$_found"
}

# shellcheck disable=SC2329
shutdown() {
    # A second stop signal during cleanup should not interrupt a clean shutdown.
    trap '' TERM INT
    # Signal every pane process, deepest work first, then let them settle.
    panes=$(tmux -S "$SOCKET" list-panes -a -F '#{pane_pid}' 2>/dev/null || true)
    if [ -n "$panes" ]; then
        # Signal the pane shell's DESCENDANTS, never the pane pid itself.
        #
        # Measured : tmux runs the pane command
        # through a shell, so `pane_pid` is that shell and the tool is its child.
        # Signalling the pane pid — or its whole process group — kills the shell
        # first, which tears down the pty and SIGHUPs the tool before it can act
        # on SIGTERM. Leaving the shell alive keeps the pty open long enough for
        # the tool to shut down cleanly.
        targets=""
        for pid in $panes; do
            targets="$targets $(descendants_of "$pid")"
        done
        targets=$(echo "$targets" | tr ' ' '\n' | grep -v '^$' | sort -u | tr '\n' ' ')

        if [ -n "$targets" ]; then
            log "forwarding SIGTERM to pane children:$targets"
            for pid in $targets; do
                kill -TERM "$pid" 2>/dev/null || true
            done
        else
            log "no pane children to signal"
        fi

        waited=0
        while [ "$waited" -lt "$GRACE" ]; do
            alive=0
            for pid in $targets; do
                [ -d "/proc/$pid" ] && alive=$((alive + 1))
            done
            [ "$alive" -eq 0 ] && break
            sleep 1
            waited=$((waited + 1))
        done
        log "pane children settled after ${waited}s"
    fi

    tmux -S "$SOCKET" kill-server 2>/dev/null || true
    exit 0
}

trap shutdown TERM INT

# --- periodic WIP commit ------------------------------------------------------
# Crash insurance for unattended work: an OOM kill or a daemon restart wipes the
# container's tmpfs $HOME, and the workspace volume only survives what was
# committed. Commits are CAPSULE-LOCAL — this never writes to the host; only an
# explicit `capscode sync` or `capscode rm` crosses that boundary.
#
# It lives here rather than host-side because a one-shot CLI cannot schedule work
# after it exits, and the keeper is the box's only long-lived process.
AUTOSYNC_SECS="${KEEPER_AUTOSYNC_SECS:-300}"
autosync_loop() {
    # `set -e` is deliberately OFF inside this loop: every check below is
    # expected to "fail" on ordinary ticks (no lock present, nothing staged),
    # and a bare `test && continue` returning 1 would otherwise kill the loop on
    # its very first pass — measured, the loop died silently before committing
    # anything.
    set +e

    # Which repository the WIP commits go into.
    #
    # A git project ingressed with its history uses /workspace/.git directly. A
    # NON-GIT project (the shadow-egress path) has no repo at all — and that is
    # exactly the case where crash insurance matters most, because the host has
    # no commits to fall back on. Measured on a real run: the loop skipped
    # silently forever and the box had no checkpoint of any kind.
    #
    # For those, keep a private repo INSIDE the workspace volume (so it survives
    # the container dying) but NOT at .git, which would turn the export into a
    # nested repository and confuse the host-side shadow diff. The directory is
    # on the copy-back refusal list, so it never reaches the host.
    if [ -d /workspace/.git ]; then
        AUTOSYNC_GIT_ARGS="-C /workspace"
        log "autosync: tracking the workspace's own git repository"
    else
        priv=/workspace/.capscode-autosync.git
        if [ ! -d "$priv" ]; then
            git --git-dir="$priv" --work-tree=/workspace init -q 2>/dev/null \
                && log "autosync: created private checkpoint repo for a non-git workspace"
        fi
        if [ ! -d "$priv" ]; then
            log "autosync: DISABLED — no git repository and none could be created"
            return
        fi
        mkdir -p "$priv/info" 2>/dev/null || return
        if ! grep -qxF '/.capscode-autosync.git/' "$priv/info/exclude" 2>/dev/null; then
            printf '%s\n' '/.capscode-autosync.git/' >> "$priv/info/exclude" 2>/dev/null || return
        fi
        # Older boxes may already have committed the private repository into
        # its own worktree. Drop those index entries without deleting the repo.
        git --git-dir="$priv" --work-tree=/workspace rm -r -f --cached \
            --ignore-unmatch -- .capscode-autosync.git >/dev/null 2>&1 || true
        AUTOSYNC_GIT_ARGS="--git-dir=$priv --work-tree=/workspace"
    fi

    git_operation_in_progress() {
        for marker in MERGE_HEAD CHERRY_PICK_HEAD REVERT_HEAD BISECT_LOG rebase-merge rebase-apply; do
            # The two arguments in AUTOSYNC_GIT_ARGS are intentionally split.
            # shellcheck disable=SC2086
            marker_path=$(git $AUTOSYNC_GIT_ARGS rev-parse --path-format=absolute --git-path "$marker" 2>/dev/null) || continue
            [ -e "$marker_path" ] && return 0
        done
        return 1
    }

    while sleep "$AUTOSYNC_SECS"; do
        # Never fight the tool's own git: a lock means it is mid-operation, so
        # skip this tick rather than stealing the lock or corrupting the index.
        if [ -e /workspace/.git/index.lock ]; then
            log "autosync: skipped (git busy)"
            continue
        fi
        if git_operation_in_progress; then
            log "autosync: skipped (git operation in progress)"
            continue
        fi
        # shellcheck disable=SC2086  # word splitting is the point here
        if ! git $AUTOSYNC_GIT_ARGS add -A 2>/dev/null; then
            continue
        fi
        # shellcheck disable=SC2086
        if git $AUTOSYNC_GIT_ARGS diff --cached --quiet 2>/dev/null; then
            continue
        fi
        # shellcheck disable=SC2086
        if git $AUTOSYNC_GIT_ARGS -c user.name=capsule -c user.email=capsule@localhost \
             -c commit.gpgsign=false -c core.hooksPath=/dev/null \
             commit --no-verify -m 'capsule: autosync WIP' >/dev/null 2>&1; then
            log "autosync: committed WIP"
        fi
    done
}

if [ "${KEEPER_AUTOSYNC:-1}" = "1" ]; then
    autosync_loop &
    log "autosync every ${AUTOSYNC_SECS}s (capsule-local commits only)"
else
    # Say why, don't just go quiet: the host sets this to 0 for a bind-mounted
    # workspace, where a commit would land in the user's real repository. The
    # entry banner claims auto-sync is off, so this line is what makes that
    # claim checkable from inside the box.
    log "autosync: OFF (KEEPER_AUTOSYNC=0) — set by the host for a bind-mounted workspace, where WIP commits would land in your real repository"
fi

log "starting tmux server on $SOCKET"
# -D runs the server in the foreground and accepts NO command; sessions are
# created afterwards by clients. Backgrounded here so the shell stays alive to
# handle signals, with `wait` making it behave like a foreground process.
tmux -f "$CONF" -S "$SOCKET" -D &
server=$!

# The Codex app-server daemon is shared by every pane in the box. Starting it
# here keeps it alive when the first pane that would otherwise start it closes.
# It starts only after the tmux server: the host attaches as soon as the
# container runs, and a client that finds no server starts its own, which makes
# the `-D` server above exit and take the box down with it.
if [ "${CAPSCODE_CODEX_DAEMON:-}" = "1" ]; then
    if codex app-server daemon start; then
        log "Codex app-server daemon started for this box"
    else
        status=$?
        log "WARN: Codex app-server daemon did not start (status $status); continuing without it"
    fi
fi

# `wait` returns early when a trapped signal arrives; loop until the server is
# genuinely gone so a stray signal cannot end the keeper by accident.
while :; do
    if wait "$server"; then
        server_status=0
    else
        server_status=$?
    fi
    # Always wait once before checking liveness so a fast child failure cannot
    # disappear between the check and wait, losing its exit status.
    kill -0 "$server" 2>/dev/null || break
done
log "tmux server exited"
exit "${server_status:-0}"
