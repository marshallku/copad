#!/usr/bin/env bash
# Remote e2e for copad's native comux chrome over SSH (decisions #171): a dev Copad.app on this
# Mac opens a comux that runs on ANOTHER machine (`comux.open_remote`), and the native sidebar,
# status strip and a delegated prompt work against it exactly as they do locally.
#
#   REMOTE=marshall@thinkarch REMOTE_COMUX=/home/marshall/dev/copad-comux/target/debug/comux \
#       scripts/e2e-copad-comux-remote-macos.sh
#
# The remote side runs an ISOLATED comux (own socket + state under a remote temp dir) with the
# given binary (a build that speaks L0); nothing of the remote user's own comux is touched. Only
# the dev Copad instance this script opens is stopped.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
REMOTE="${REMOTE:?REMOTE=user@host}"
REMOTE_COMUX="${REMOTE_COMUX:?REMOTE_COMUX=/path/to/comux on the remote}"
COCTL="$REPO/target/release/coctl"
APP="$REPO/copad-macos/.build/debug/Copad.app"
log() { printf '[e2e-remote] %s\n' "$*" >&2; }
fail() { log "FAIL: $*"; exit 1; }

cargo build -q --release -p copad-ffi -p copad-term -p copad-cli
(cd copad-macos && swift build -q)
cp copad-macos/.build/debug/Copad "$APP/Contents/MacOS/Copad"
scripts/codesign-dev.sh "$APP" >/dev/null 2>&1 || true

W="$(mktemp -d /tmp/cmr.XXXX)"
RW="$(ssh -o BatchMode=yes "$REMOTE" 'mktemp -d /tmp/cmr.XXXX')"
RENV="COPAD_MUX_SOCK=$RW/s COPAD_MUX_STATE=$RW/st.json COPAD_MUX_NOTIFY=0 COPAD_MUX_USAGE=0 COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1 XDG_CONFIG_HOME=$RW/cfg"
APP_PID=""; CAF=""
cleanup() {
    [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null || true
    [[ -n "$CAF" ]] && kill "$CAF" 2>/dev/null || true
    ssh -o BatchMode=yes "$REMOTE" "env $RENV $REMOTE_COMUX kill-server >/dev/null 2>&1; rm -rf $RW" || true
    [[ "${KEEP:-}" == 1 ]] && { log "kept $W"; return 0; }
    rm -rf "$W"
}
trap cleanup EXIT

ssh -o BatchMode=yes "$REMOTE" "mkdir -p $RW/cfg/copad; env $RENV $REMOTE_COMUX new-session --detached remote-two >/dev/null"
# The remote L0 view, read ON the remote (python3 there).
remote_topic() {
    ssh -o BatchMode=yes "$REMOTE" "COPAD_MUX_SOCK=$RW/s python3 -c '
import json, os, socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(os.environ[\"COPAD_MUX_SOCK\"])
s.sendall(b\"{\\\"t\\\":\\\"subscribe\\\",\\\"v\\\":1}\\n\"); f = s.makefile(); f.readline()
print(json.dumps(json.loads(f.readline())[\"topics\"][sys.argv[1]][\"data\"]))
' $1"
}

before="$(pgrep -x Copad || true)"
caffeinate -d -u -t 300 & CAF=$!
open -n "$APP"
for _ in $(seq 1 100); do
    APP_PID="$(comm -13 <(echo "$before" | sort) <(pgrep -x Copad | sort) | head -1)"
    [[ -n "$APP_PID" && -S "/tmp/copad-$APP_PID.sock" ]] && break
    sleep 0.1
done
GUI="/tmp/copad-$APP_PID.sock"
[[ -S "$GUI" ]] || fail "dev Copad did not open its socket"
sleep 2

"$COCTL" --socket "$GUI" call comux.open_remote \
    --params "$(python3 -c 'import json,sys; print(json.dumps({"dest": sys.argv[1], "comux": "env " + sys.argv[2] + " " + sys.argv[3]}))' "$REMOTE" "$RENV" "$REMOTE_COMUX")" >/dev/null
ok=""
for _ in $(seq 1 60); do
    c="$(remote_topic clients 2>/dev/null || true)"
    if python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if any(x["tag"].startswith("copad-remote:") and {"sidebar","status","overlays"} <= set(x["claimed"]) for x in c["clients"]) and {"sidebar","status"} <= set(c["hidden"]) else 1)' "$c" 2>/dev/null; then
        ok=1; break
    fi
    sleep 1
done
[[ -n "$ok" ]] || fail "copad never claimed the REMOTE client's chrome: $(remote_topic clients)"
log "ok — copad opened the remote comux, found it by its remote tag over SSH and claimed its chrome"

SCREEN="$("$COCTL" --socket "$GUI" terminal read 2>/dev/null || true)"
grep -q "spaces" <<<"$SCREEN" && fail "the remote comux still draws its TUI sidebar:
$SCREEN"
log "ok — the remote comux frame in the copad tab has no TUI sidebar"

WID="$(python3 - "$APP_PID" <<'PY'
import sys, Quartz
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") == int(sys.argv[1]) and w["kCGWindowBounds"]["Width"] > 200:
        print(w["kCGWindowNumber"]); break
PY
)"
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/remote-native.png" && log "screenshot: $W/remote-native.png"

# A delegated overlay from the REMOTE comux: Ctrl-b $ → native sheet → its answer renames.
"$COCTL" --socket "$GUI" terminal feed $'\x02$' >/dev/null
sleep 3
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/remote-sheet.png"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" \
          -e 'tell application "System Events" to keystroke "remoterenamed"' \
          -e 'tell application "System Events" to key code 36' >/dev/null
for _ in $(seq 1 20); do remote_topic tree | grep -q remoterenamed && break; sleep 1; done
remote_topic tree | grep -q remoterenamed || fail "the remote prompt answered natively did not rename: $(remote_topic tree)"
log "ok — Ctrl-b \$ on the remote comux became a native sheet and its answer renamed the remote session"
log "PASS: copad macOS native chrome over SSH"
