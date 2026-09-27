#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT HUP INT TERM

cat > "$TMP/tmux" <<'STUB'
#!/bin/sh
case " $* " in
    *" list-panes "*) exit 1 ;;
    *" kill-server "*)
        [ -f "$KEEPER_TEST_TMUX_PIDFILE" ] && kill -TERM "$(cat "$KEEPER_TEST_TMUX_PIDFILE")" 2>/dev/null || true
        exit 0
        ;;
    *" -D "*)
        echo "$$" > "$KEEPER_TEST_TMUX_PIDFILE"
        trap 'exit 0' TERM INT
        while :; do sleep 1; done
        ;;
    *) exit 0 ;;
esac
STUB
chmod +x "$TMP/tmux"

run_clean_shutdown() {
    KEEPER_TEST_TMUX_PIDFILE="$TMP/tmux.pid" \
        KEEPER_AUTOSYNC=0 PATH="$TMP:$PATH" sh "$ROOT/keeper.sh" >"$TMP/keeper.log" 2>&1 &
    keeper=$!
    tries=0
    while [ ! -s "$TMP/tmux.pid" ] && [ "$tries" -lt 50 ]; do
        sleep 0.1
        tries=$((tries + 1))
    done
    [ -s "$TMP/tmux.pid" ] || {
        cat "$TMP/keeper.log" >&2
        kill -KILL "$keeper" 2>/dev/null || true
        return 1
    }
    kill -TERM "$keeper"
    wait "$keeper"
}

run_tmux_error() {
    cat > "$TMP/tmux-error" <<'STUB'
#!/bin/sh
case " $* " in
    *" -D "*) exit 7 ;;
    *) exit 0 ;;
esac
STUB
    chmod +x "$TMP/tmux-error"
    mkdir -p "$TMP/error-bin"
    cp "$TMP/tmux-error" "$TMP/error-bin/tmux"
    set +e
    KEEPER_AUTOSYNC=0 PATH="$TMP/error-bin:$PATH" sh "$ROOT/keeper.sh" >"$TMP/error.log" 2>&1
    status=$?
    set -e
    [ "$status" -eq 7 ]
}

run_codex_daemon() {
    label=$1
    daemon_signal=$2
    codex_status=$3
    expect_call=$4
    case_dir="$TMP/$label"
    mkdir -p "$case_dir/bin"
    cat > "$case_dir/bin/codex" <<'STUB'
#!/bin/sh
# Record whether the tmux server was already up when the daemon was started.
tries=0
while [ ! -s "$KEEPER_TEST_TMUX_PIDFILE" ] && [ "$tries" -lt 20 ]; do
    sleep 0.1
    tries=$((tries + 1))
done
if [ -s "$KEEPER_TEST_TMUX_PIDFILE" ]; then server=up; else server=down; fi
printf '%s server=%s\n' "$*" "$server" >> "$KEEPER_TEST_CODEX_LOG"
exit "${KEEPER_TEST_CODEX_STATUS:-0}"
STUB
    chmod +x "$case_dir/bin/codex"

    if [ "$daemon_signal" = 1 ]; then
        CAPSCODE_CODEX_DAEMON=1 \
            KEEPER_TEST_TMUX_PIDFILE="$case_dir/tmux.pid" \
            KEEPER_TEST_CODEX_LOG="$case_dir/codex.log" \
            KEEPER_TEST_CODEX_STATUS="$codex_status" \
            KEEPER_AUTOSYNC=0 PATH="$case_dir/bin:$TMP:$PATH" \
            sh "$ROOT/keeper.sh" >"$case_dir/keeper.log" 2>&1 &
    else
        CAPSCODE_CODEX_DAEMON='' \
            KEEPER_TEST_TMUX_PIDFILE="$case_dir/tmux.pid" \
            KEEPER_TEST_CODEX_LOG="$case_dir/codex.log" \
            KEEPER_TEST_CODEX_STATUS="$codex_status" \
            KEEPER_AUTOSYNC=0 PATH="$case_dir/bin:$TMP:$PATH" \
            sh "$ROOT/keeper.sh" >"$case_dir/keeper.log" 2>&1 &
    fi
    keeper=$!
    tries=0
    while [ ! -s "$case_dir/tmux.pid" ] && [ "$tries" -lt 50 ]; do
        sleep 0.1
        tries=$((tries + 1))
    done
    [ -s "$case_dir/tmux.pid" ] || {
        cat "$case_dir/keeper.log" >&2
        kill -KILL "$keeper" 2>/dev/null || true
        return 1
    }

    if [ "$expect_call" = yes ]; then
        tries=0
        until grep -q 'WARN: Codex app-server daemon did not start' "$case_dir/keeper.log" || [ "$tries" -ge 50 ]; do
            sleep 0.1
            tries=$((tries + 1))
        done
        # A client attaching before the tmux server is up would start its own
        # server and end the box, so the daemon must not delay the server.
        [ "$(cat "$case_dir/codex.log")" = "app-server daemon start server=up" ]
        grep -q 'WARN: Codex app-server daemon did not start' "$case_dir/keeper.log"
    else
        [ ! -e "$case_dir/codex.log" ]
    fi
    kill -TERM "$keeper"
    wait "$keeper"
}

run_clean_shutdown
run_tmux_error
run_codex_daemon not-routed "" 0 no
run_codex_daemon routed 1 17 yes
