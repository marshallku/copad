#!/usr/bin/env bash
# macOS e2e for copad's native comux sidebar (decisions #169): a REAL dev Copad.app runs a
# REAL comux client in a tab against an isolated comux server, and we check that copad claimed
# the client's sidebar (comux stops drawing it) and draws its own; then that the native panel
# follows comux (a new session appears in it) and that closing copad gives the TUI sidebar back.
#
# Never touches the user's comux (own socket + state) and never kills a running Copad — the dev
# bundle is opened as a SEPARATE instance (`open -n`) and only that pid is stopped.
#
#   scripts/e2e-copad-comux-macos.sh           # build + run
#   KEEP=1 scripts/e2e-copad-comux-macos.sh    # keep the work dir (screenshots)
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
COMUX="$REPO/target/debug/comux"
COCTL="$REPO/target/release/coctl"
APP="$REPO/copad-macos/.build/debug/Copad.app"
log() { printf '[e2e-macos] %s\n' "$*" >&2; }
fail() { log "FAIL: $*"; exit 1; }

cargo build -q -p copad-mux
cargo build -q --release -p copad-ffi -p copad-term -p copad-cli
(cd copad-macos && swift build -q)
[[ -d "$APP/Contents/MacOS" ]] || fail "no dev bundle at $APP (run copad-macos/run.sh once)"
cp copad-macos/.build/debug/Copad "$APP/Contents/MacOS/Copad"
scripts/codesign-dev.sh "$APP" >/dev/null 2>&1 || true

W="$(mktemp -d /tmp/cmc.XXXX)"
APP_PID=""
CAF=""
# The dev instance shares the real Copad's session file: put it back afterwards.
SESSION="$HOME/Library/Application Support/copad/session.json"
SESSION_BAK="$(mktemp /tmp/copad-session.XXXX)"
SESSION_EXISTED=""
if [[ -f "$SESSION" ]]; then SESSION_EXISTED=1; cp "$SESSION" "$SESSION_BAK"; fi
DEV_PID="" # the dev instance's pid, kept until it has really exited
cleanup() {
    [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null || true
    [[ -n "$CAF" ]] && kill "$CAF" 2>/dev/null || true
    COPAD_MUX_SOCK="$W/s" "$COMUX" kill-server >/dev/null 2>&1 || true
    # Only after the app has EXITED (it saves its session on the way out).
    if [[ -n "$DEV_PID" ]]; then
        kill "$DEV_PID" 2>/dev/null || true
        for _ in $(seq 1 100); do kill -0 "$DEV_PID" 2>/dev/null || break; sleep 0.1; done
        kill -9 "$DEV_PID" 2>/dev/null || true
        for _ in $(seq 1 50); do kill -0 "$DEV_PID" 2>/dev/null || break; sleep 0.1; done
    fi
    if [[ -n "$SESSION_EXISTED" ]]; then cp "$SESSION_BAK" "$SESSION"; else rm -f "$SESSION"; fi
    rm -f "$SESSION_BAK"
    [[ "${KEEP:-}" == 1 ]] && { log "kept $W"; return 0; }
    rm -rf "$W"
}
trap cleanup EXIT

export COPAD_MUX_SOCK="$W/s" COPAD_MUX_STATE="$W/state.json" COPAD_MUX_NOTIFY=0 \
       COPAD_MUX_USAGE=0 COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1 XDG_CONFIG_HOME="$W/cfg"
unset COPAD_MUX_PANE COPAD_MUX COPAD_SOCKET COPAD_PANEL_ID COPAD_MUX_HOST_TAG
mkdir -p "$W/cfg/copad"
# The server's HOME is a fixture: Ctrl-b C browses it, Ctrl-b R scans its transcripts.
mkdir -p "$W/home/proj/sub" "$W/home/.claude/projects/-p"
printf '%s\n' '{"type":"attachment","cwd":"/p","entrypoint":"cli"}' \
    '{"type":"user","message":{"role":"user","content":"mac resume probe"}}' \
    >"$W/home/.claude/projects/-p/0c8e9f4a-1b2c-4d5e-8f90-a1b2c3d4e5f7.jsonl"
HOME="$W/home" "$COMUX" server >"$W/server.log" 2>&1 &
for _ in $(seq 1 50); do "$COMUX" list-sessions >/dev/null 2>&1 && break; sleep 0.1; done
"$COMUX" new-session --detached macos-two >/dev/null

# An L0 reader: prints the `clients` topic after the snapshot.
clients() {
    python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n')
f = s.makefile()
f.readline()
snap = json.loads(f.readline())
print(json.dumps(snap["topics"]["clients"]["data"]))
PY
}

before="$(pgrep -x Copad || true)"
caffeinate -d -u -t 300 & CAF=$!
# The app inherits the isolated comux socket, so its MuxHost subscribes to it AND every tab's
# shell exports it to the comux client we start below.
open -n --env COPAD_MUX_SOCK="$COPAD_MUX_SOCK" --env COPAD_MUX_STATE="$COPAD_MUX_STATE" \
    --env XDG_CONFIG_HOME="$XDG_CONFIG_HOME" "$APP"
for _ in $(seq 1 100); do
    APP_PID="$(comm -13 <(echo "$before" | sort) <(pgrep -x Copad | sort) | head -1)"
    [[ -n "$APP_PID" ]] && DEV_PID="$APP_PID" # kept as soon as it exists
    [[ -n "$APP_PID" && -S "/tmp/copad-$APP_PID.sock" ]] && break
    sleep 0.1
done
GUI="/tmp/copad-$APP_PID.sock"
[[ -S "$GUI" ]] || fail "dev Copad did not open its socket"
log "dev Copad pid $APP_PID"
sleep 2

# Start the comux client in the active tab (explicit env: the tab's shell rc may rewrite it).
"$COCTL" --socket "$GUI" terminal exec "env COPAD_MUX_SOCK=$COPAD_MUX_SOCK COPAD_MUX_QUIET_SSH=1 $COMUX" >/dev/null
claimed=""
for _ in $(seq 1 60); do
    c="$(clients)"
    if python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if any(x["tag"].startswith("copad:") and set(["sidebar","status","overlays"]) <= set(x["claimed"]) for x in c["clients"]) and set(["sidebar","status"]) <= set(c["hidden"]) else 1)' "$c"; then
        claimed=1; break
    fi
    sleep 0.5
done
[[ -n "$claimed" ]] || fail "copad never claimed the comux client's sidebar: $(clients)"
log "ok — copad found its tab's comux client by tag and claimed sidebar, status and overlays"

# comux no longer draws its sidebar in that tab: the screen has no `spaces` header.
SCREEN="$("$COCTL" --socket "$GUI" terminal read 2>/dev/null || true)"
grep -q "spaces" <<<"$SCREEN" && fail "the TUI sidebar is still drawn:
$SCREEN"
log "ok — the comux frame in the copad tab has no TUI sidebar"

# The native panel is on screen with comux's sessions (the window's accessibility tree).
ax() {
    osascript <<OSA 2>/dev/null
tell application "System Events"
  set p to first process whose unix id is $APP_PID
  set frontmost of p to true
  return entire contents of window 1 of p as text
end tell
OSA
}
"$COMUX" new-session --detached macos-three >/dev/null
for _ in $(seq 1 20); do ax | grep -q "macos-three" && break; sleep 0.5; done
ax | grep -q "macos-three" || log "warn: macos-three not visible through accessibility (checking via screenshot)"
WID="$(python3 - "$APP_PID" <<'PY'
import sys, Quartz
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") == int(sys.argv[1]) and w["kCGWindowBounds"]["Width"] > 200:
        print(w["kCGWindowNumber"]); break
PY
)"
if [[ -n "$WID" ]]; then
    screencapture -o -x -l"$WID" "$W/native-sidebar.png" && log "screenshot: $W/native-sidebar.png"
fi

# An overlay is delegated: Ctrl-b $ in the tab opens comux's rename prompt as a native SHEET
# in this window; typing a name there renames the session.
session_names() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
print(" ".join(x["name"] for x in json.loads(f.readline())["topics"]["tree"]["data"]["sessions"]))
PY
}
"$COCTL" --socket "$GUI" terminal feed $'\x02$' >/dev/null
sleep 2
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/overlay-sheet.png" && log "screenshot: $W/overlay-sheet.png"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" \
          -e 'tell application "System Events" to keystroke "macrenamed"' \
          -e 'tell application "System Events" to key code 36' >/dev/null
for _ in $(seq 1 20); do session_names | grep -q macrenamed && break; sleep 0.5; done
session_names | grep -q macrenamed || fail "the delegated prompt did not rename the session: $(session_names)"
log "ok — Ctrl-b \$ became a native sheet and its answer renamed the session"
SCREEN="$("$COCTL" --socket "$GUI" terminal read 2>/dev/null || true)"
grep -q "rename session" <<<"$SCREEN" && fail "the TUI drew the prompt as well"
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/after-rename.png"

# Ctrl-b C (decisions #172): the directory browser is a native sheet, filled once comux's
# listing lands; picking "proj" descends, "Create session here" leads to comux's name prompt.
keys() { osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" \
                   -e "tell application \"System Events\" to keystroke \"$1\"" \
                   -e 'tell application "System Events" to key code 36' >/dev/null; }
session_dirs() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
print(" ".join(x["name"] + "=" + x["dir"] for x in json.loads(f.readline())["topics"]["tree"]["data"]["sessions"]))
PY
}
"$COCTL" --socket "$GUI" terminal feed $'\x02C' >/dev/null
sleep 3
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/browser-sheet.png"
keys proj; sleep 3
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/browser-proj.png"
keys create; sleep 3
keys browsed
for _ in $(seq 1 20); do session_dirs | grep -q "browsed=.*/proj" && break; sleep 0.5; done
session_dirs | grep -q "browsed=.*/proj" || fail "the native browser did not create the session in proj: $(session_dirs)"
log "ok — Ctrl-b C was a native sheet: descending into proj and 'Create session here' made a session there"

# The sidebar's resume button asks comux to open Ctrl-b R for this client; the native sheet
# lists the fixture transcript.
# Click it where it is drawn (the sidebar header, left of the bell; the sidebar is 30 columns
# of the default 14pt font wide): a real pointer event.
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null
sleep 1 # the previous sheet's dismissal, and the window becoming key again
python3 - "$APP_PID" <<'PY'
import sys, time, Quartz
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") == int(sys.argv[1]) and w["kCGWindowBounds"]["Width"] > 200:
        b = w["kCGWindowBounds"]; break
pt = (b["X"] + 219, b["Y"] + 82)
for kind in (Quartz.kCGEventLeftMouseDown, Quartz.kCGEventLeftMouseUp):
    e = Quartz.CGEventCreateMouseEvent(None, kind, pt, Quartz.kCGMouseButtonLeft)
    Quartz.CGEventPost(Quartz.kCGHIDEventTap, e)
    time.sleep(0.05)
PY
found=""
sheet_text() {
    osascript -e "tell application \"System Events\" to set b to button 1 of UI element 1 of row 2 of outline 1 of scroll area 1 of group 1 of sheet 1 of window 1 of (first process whose unix id is $APP_PID)" -e "tell application \"System Events\" to return value of attribute \"AXIdentifier\" of b" 2>&1
}
for _ in $(seq 1 20); do sheet_text | grep -q "overlayRow-c:0" && { found=1; break; }; sleep 0.5; done
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/resume-sheet.png"
if [[ -z "$found" ]]; then
    sheet_text | head -c 800 >&2
    fail "the resume button did not bring up comux's resume picker natively"
fi
osascript -e 'tell application "System Events" to key code 53' >/dev/null
log "ok — the native resume button opened comux's Ctrl-b R picker as a sheet with the fixture conversation"

# Closing copad releases the claim: comux draws its own sidebar again for the next client.
kill "$APP_PID"; APP_PID=""
for _ in $(seq 1 40); do
    c="$(clients)"
    python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if "sidebar" not in c["hidden"] else 1)' "$c" && break
    sleep 0.5
done
python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if "sidebar" not in c["hidden"] else 1)' "$(clients)" \
    || fail "the sidebar stayed hidden after copad quit"
log "ok — quitting copad released the claim"
log "PASS: copad macOS native comux sidebar"
