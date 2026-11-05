#!/usr/bin/env bash
# scripts/e2e-comux-render.sh with a REAL Copad.app as the outer terminal: its tab runs a comux
# client whose chrome copad claims, and the screen is read back through `coctl terminal read`
# (what copad's own grid holds). Screenshots land in $SHOTS for a look at the actual pixels.
#
# A SEPARATE instance (`open -n`) with its own state dir, stopped by pid; no synthesized input, so
# it does not take over the keyboard.
#
#   scripts/e2e-comux-render-gui-macos.sh
#   APP=/path/to/Copad.app SHOTS=/some/dir scripts/e2e-comux-render-gui-macos.sh
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
APP="${APP:-$HOME/Applications/Copad.app}"
SHOTS="${SHOTS:-$(mktemp -d /tmp/crshots.XXXX)}"
mkdir -p "$SHOTS"
cargo build -q -p copad-mux -p copad-cli
export COMUX="${COMUX:-$REPO/target/debug/comux}"
COCTL="$REPO/target/debug/coctl"

export E2E_WORK="$(mktemp -d /tmp/crg.XXXX)"
DEV_PID=""
CAF=""
cleanup() {
    [[ -n "$CAF" ]] && kill "$CAF" 2>/dev/null || true
    # Before removing the work dir: it saves its session there on the way out.
    if [[ -n "$DEV_PID" ]]; then
        kill "$DEV_PID" 2>/dev/null || true
        for _ in $(seq 1 100); do kill -0 "$DEV_PID" 2>/dev/null || break; sleep 0.1; done
        kill -9 "$DEV_PID" 2>/dev/null || true
        for _ in $(seq 1 50); do kill -0 "$DEV_PID" 2>/dev/null || break; sleep 0.1; done
    fi
    rm -f "/tmp/copad-$DEV_PID.sock"
    rm -rf "$E2E_WORK"
}
trap cleanup EXIT

before="$(pgrep -x Copad || true)"
caffeinate -d -u -t 400 &
CAF=$!
open -n --env COPAD_STATE_DIR="$E2E_WORK/state" --env COPAD_MUX_SOCK="$E2E_WORK/s" --env COPAD_MUX_STATE="$E2E_WORK/st-no-sidebar.json" \
    --env XDG_CONFIG_HOME="$E2E_WORK/cfg" "$APP"
for _ in $(seq 1 100); do
    DEV_PID="$(comm -13 <(echo "$before" | sort) <(pgrep -x Copad | sort) | head -1)"
    [[ -n "$DEV_PID" && -S "/tmp/copad-$DEV_PID.sock" ]] && break
    sleep 0.1
done
export GUI="/tmp/copad-$DEV_PID.sock"
[[ -S "$GUI" ]] || { echo "FAIL: the Copad instance did not open its socket"; exit 1; }
echo "Copad pid $DEV_PID"
sleep 2

export COCTL
# The client starts drawing chrome, then copad claims it and the pane grows: wait for that, or the
# layout read afterwards is stale.
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
export WID_PID="$DEV_PID" SHOTS
export OUTER_SHOT='python3 -c "
import os, Quartz, subprocess
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get(\"kCGWindowOwnerPID\") == int(os.environ[\"WID_PID\"]) and w[\"kCGWindowBounds\"][\"Width\"] > 200:
        subprocess.run([\"screencapture\", \"-o\", \"-x\", \"-l\" + str(w[\"kCGWindowNumber\"]), os.path.join(os.environ[\"SHOTS\"], os.environ.get(\"SHOT\", \"shot\") + \".png\")])
        break
"'
export SHOT=""
scripts/e2e-comux-render.sh
echo "screenshots: $SHOTS"
