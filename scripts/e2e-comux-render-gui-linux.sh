#!/usr/bin/env bash
# scripts/e2e-comux-render.sh with a REAL Linux copad (VTE) as the outer terminal, on headless GTK
# (gtk4-broadwayd, so nothing appears on the live desktop). The screen is read back through
# `coctl terminal read`, which is VTE's own text. SHOT_HOLD=N prints `SHOT <name>` at each moment
# worth a look and holds for N seconds, so whatever is on the broadway page (127.0.0.1:8097) can
# be captured from outside.
#
#   cargo build -p copad-linux -p copad-mux -p copad-cli && scripts/e2e-comux-render-gui-linux.sh
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
export COMUX="${COMUX:-$REPO/target/debug/comux}" COCTL="$REPO/target/debug/coctl"
COPAD="$REPO/target/debug/copad"
export E2E_WORK="$(mktemp -d /tmp/crg.XXXX)"
BPID=""
CPID=""
APP=""
cleanup() {
    # Let copad close its window (which removes its socket) BEFORE its display goes away.
    if [[ -n "$APP" ]]; then
        kill "$APP" 2>/dev/null || true
        for _ in $(seq 1 50); do kill -0 "$APP" 2>/dev/null || break; sleep 0.1; done
        rm -f "$XDG_RUNTIME_DIR/copad/gui-$APP.sock" "/tmp/copad-prewarm-$APP.html"
    fi
    [[ -n "$CPID" ]] && kill "$CPID" 2>/dev/null || true
    [[ -n "$BPID" ]] && kill "$BPID" 2>/dev/null || true
    # The session bus's activated services outlive dbus-run-session holding copad's log.
    sleep 0.5
    for f in /proc/[0-9]*/fd/*; do
        case "$(readlink "$f" 2>/dev/null)" in "$E2E_WORK"/*) p=${f#/proc/}; echo "${p%%/*}" ;; esac
    done 2>/dev/null | sort -u | xargs -r kill 2>/dev/null || true
    rm -rf "$E2E_WORK"
}
trap cleanup EXIT

export COPAD_MUX_SOCK="$E2E_WORK/s"
gtk4-broadwayd :17 >"$E2E_WORK/broadway.log" 2>&1 &
BPID=$!
sleep 1
# Its own state dir: copad restores and saves the session of the copad the user is running.
XDG_STATE_HOME="$E2E_WORK/state" XDG_CONFIG_HOME="$E2E_WORK/cfg" GDK_BACKEND=broadway BROADWAY_DISPLAY=:17 \
    COPAD_MUX_STATE="$E2E_WORK/st-no-sidebar.json" dbus-run-session -- "$COPAD" >"$E2E_WORK/copad.log" 2>&1 &
CPID=$!
export GUI=""
for _ in $(seq 1 100); do
    for p in $(pgrep -x copad); do
        if tr '\0' '\n' <"/proc/$p/environ" 2>/dev/null | grep -qx "COPAD_MUX_SOCK=$E2E_WORK/s" && [[ -S "$XDG_RUNTIME_DIR/copad/gui-$p.sock" ]]; then
            GUI="$XDG_RUNTIME_DIR/copad/gui-$p.sock"
            APP=$p
            break 2
        fi
    done
    sleep 0.2
done
[[ -n "$GUI" ]] || { tail -20 "$E2E_WORK/copad.log"; echo "FAIL: copad did not open its socket"; exit 1; }
echo "copad pid $APP"
sleep 3

claimed() {
    python3 - <<'PY'
import json, os, socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
c = json.loads(f.readline())["topics"]["clients"]["data"]
sys.exit(0 if {"status"} <= set(c["hidden"]) else 1)
PY
}
export -f claimed
export OUTER_RUN='"$COCTL" --socket "$GUI" terminal exec "env COPAD_MUX_SOCK=$COPAD_MUX_SOCK COPAD_MUX_QUIET_SSH=1 COPAD_MUX_NOTIFY=0 COPAD_MUX_USAGE=0 COPAD_MUX_UPDATE_CHECK=0 $COMUX" >/dev/null; for _ in $(seq 1 60); do claimed && break; sleep 0.5; done; claimed || { echo "FAIL: copad never claimed the client"; FAILS=$((FAILS + 1)); }'
export OUTER_READ='"$COCTL" --socket "$GUI" terminal read | python3 -c "import json,sys; print(json.load(sys.stdin)[\"text\"])"'
export OUTER_COLS='"$COCTL" --socket "$GUI" terminal state | python3 -c "import json,sys; print(json.load(sys.stdin)[\"cols\"])"'
export OUTER_STOP=':'
if [[ -n "${SHOT_HOLD:-}" ]]; then
    export OUTER_SHOT='echo "SHOT $SHOT"; sleep "$SHOT_HOLD"'
fi
"$REPO/scripts/e2e-comux-render.sh"
