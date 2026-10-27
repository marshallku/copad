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

# An overlay is delegated: Ctrl-b $ in the tab opens comux's rename prompt natively (an overlay layer)
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
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/overlay-prompt.png" && log "screenshot: $W/overlay-prompt.png"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" \
          -e 'tell application "System Events" to keystroke "macrenamed"' \
          -e 'tell application "System Events" to key code 36' >/dev/null
for _ in $(seq 1 20); do session_names | grep -q macrenamed && break; sleep 0.5; done
session_names | grep -q macrenamed || fail "the delegated prompt did not rename the session: $(session_names)"
log "ok — Ctrl-b \$ was drawn natively and its answer renamed the session"
SCREEN="$("$COCTL" --socket "$GUI" terminal read 2>/dev/null || true)"
grep -q "rename session" <<<"$SCREEN" && fail "the TUI drew the prompt as well"
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/after-rename.png"

# Ctrl-b C (decisions #172): the directory browser is drawn natively, filled once comux's
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
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/browser.png"
keys proj; sleep 3
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/browser-proj.png"
keys create; sleep 3
keys browsed
for _ in $(seq 1 20); do session_dirs | grep -q "browsed=.*/proj" && break; sleep 0.5; done
session_dirs | grep -q "browsed=.*/proj" || fail "the native browser did not create the session in proj: $(session_dirs)"
log "ok — Ctrl-b C was drawn natively: descending into proj and 'Create session here' made a session there"

# The sidebar's resume button asks comux to open Ctrl-b R for this client; the native picker
# lists the fixture transcript.
# Click it where it is drawn (the sidebar header, left of the bell; the sidebar is 30 columns
# of the default 14pt font wide): a real pointer event.
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null
sleep 1 # the previous overlay's dismissal, and the window becoming key again
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
# The overlay layer is not in the accessibility tree (nor is the rest of the content area), so
# it is checked by what it does: Enter on the picker resumes the fixture conversation, which
# comux opens as a new tab.
tabs_total() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
print(sum(len(x["tabs"]) for x in json.loads(f.readline())["topics"]["tree"]["data"]["sessions"]))
PY
}
BEFORE="$(tabs_total)"
sleep 3 # the transcript scan
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/resume-overlay.png" && log "screenshot: $W/resume-overlay.png"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" \
          -e 'tell application "System Events" to key code 36' >/dev/null
for _ in $(seq 1 20); do [[ "$(tabs_total)" -gt "$BEFORE" ]] && break; sleep 0.5; done
[[ "$(tabs_total)" -gt "$BEFORE" ]] || fail "Enter on the native resume picker did not resume the fixture conversation"
log "ok — the native resume button opened comux's Ctrl-b R picker over the window and Enter resumed the conversation"

# Ctrl-f (decisions #180): the switcher is drawn over the window like the TUI's, with the
# selected session's pane previewed live; typing filters and Enter switches.
active_session() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
t = json.loads(f.readline())["topics"]["tree"]["data"]
print(next(x["name"] for x in t["sessions"] if x["id"] == t["active"]))
PY
}
sleep 1
"$COCTL" --socket "$GUI" terminal feed $'\x06' >/dev/null
sleep 3 # the overlay, then the preview's first refresh
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/switcher.png" && log "screenshot: $W/switcher.png"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" \
          -e 'tell application "System Events" to key code 124' >/dev/null
sleep 1
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/switcher-agents.png" && log "screenshot: $W/switcher-agents.png"
osascript -e 'tell application "System Events" to key code 123' >/dev/null
keys macos-two
for _ in $(seq 1 20); do [[ "$(active_session)" == macos-two ]] && break; sleep 0.5; done
[[ "$(active_session)" == macos-two ]] || fail "Ctrl-f, typing and Enter did not switch the session: $(active_session)"
log "ok — Ctrl-f drew the switcher over the window; ←/→ switched tabs, typing filtered and Enter switched"

# A real pointer click at window point (x, y); a negative y counts from the bottom.
click_at() { python3 - "$APP_PID" "$1" "$2" <<'PY'
import sys, time, Quartz
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") == int(sys.argv[1]) and w["kCGWindowBounds"]["Width"] > 200:
        b = w["kCGWindowBounds"]; break
x = b["X"] + int(sys.argv[2]); y = b["Y"] + (int(sys.argv[3]) if int(sys.argv[3]) >= 0 else b["Height"] + int(sys.argv[3]))
for kind in (Quartz.kCGEventLeftMouseDown, Quartz.kCGEventLeftMouseUp):
    Quartz.CGEventPost(Quartz.kCGHIDEventTap, Quartz.CGEventCreateMouseEvent(None, kind, (x, y), Quartz.kCGMouseButtonLeft))
    time.sleep(0.05)
PY
}

# The notification center keeps the TUI's letters: d dismisses one and the center stays up,
# so D then clears the rest (it would be typed into the shell if the center had closed).
notif_count() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
print(len(json.loads(f.readline())["topics"]["notifications"]["data"]["items"]))
PY
}
PANE_TOK="$("$COMUX" list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["panes"][0]["token"])')"
for body in one two three; do "$COMUX" notify --pane "$PANE_TOK" --kind done "e2e $body" >/dev/null; done
for _ in $(seq 1 20); do [[ "$(notif_count)" == 3 ]] && break; sleep 0.5; done
sleep 1
"$COCTL" --socket "$GUI" terminal feed $'\x02a' >/dev/null
sleep 2
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/center.png" && log "screenshot: $W/center.png"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" \
          -e 'tell application "System Events" to keystroke "d"' >/dev/null
for _ in $(seq 1 20); do [[ "$(notif_count)" == 2 ]] && break; sleep 0.5; done
[[ "$(notif_count)" == 2 ]] || fail "d in the native center did not dismiss one notification: $(notif_count)"
sleep 1
osascript -e 'tell application "System Events" to keystroke "D"' >/dev/null
for _ in $(seq 1 20); do [[ "$(notif_count)" == 0 ]] && break; sleep 0.5; done
[[ "$(notif_count)" == 0 ]] || fail "the center closed after d (D did not clear the rest): $(notif_count)"
osascript -e 'tell application "System Events" to key code 53' >/dev/null
log "ok — the native center dismissed with d, stayed open, and D cleared the rest"

# The notification panel opens OVER the terminal (#183) and opening a notification closes it.
PANE_TOK="$("$COMUX" list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["panes"][0]["token"])')"
"$COMUX" notify --pane "$PANE_TOK" --kind done "panel probe" >/dev/null
for _ in $(seq 1 20); do [[ "$(notif_count)" == 1 ]] && break; sleep 0.5; done
click_at 245 82 # the sidebar's bell
sleep 1.5
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/notif-panel.png" && log "screenshot: $W/notif-panel.png"
WW="$(python3 - "$APP_PID" <<'PY'
import sys, Quartz
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") == int(sys.argv[1]) and w["kCGWindowBounds"]["Width"] > 200:
        print(int(w["kCGWindowBounds"]["Width"])); break
PY
)"
click_at $((WW - 200)) 112 # the first notification row
for _ in $(seq 1 20); do [[ "$(notif_count)" == 0 ]] && break; sleep 0.5; done
[[ "$(notif_count)" == 0 ]] || fail "clicking the notification did not open it"
sleep 1
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/notif-closed.png" && log "screenshot: $W/notif-closed.png"
log "ok — the notification panel opened over the terminal and opening a notification consumed it"

# The chrome never takes the keys (#183): after a click on the status bar's tab chip, typing
# still goes to the terminal.
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null
sleep 0.5
click_at 140 -40 # the "1 zsh" tab chip
sleep 1
osascript -e 'tell application "System Events" to keystroke "echo kept-"' -e 'tell application "System Events" to keystroke "focus"' >/dev/null
sleep 1
"$COCTL" --socket "$GUI" terminal read 2>/dev/null | grep -q "echo kept-focus" \
    || fail "a click on the native status bar took the keys from the terminal"
osascript -e 'tell application "System Events" to key code 51' -e 'tell application "System Events" to keystroke "u" using control down' >/dev/null
log "ok — clicking the native status bar left the keys with the terminal"

# Ctrl-b e (decisions #181): the native sidebar takes the keys like the TUI's — it starts on the
# active space, ↓ moves to the next and Enter switches there.
sleep 1
NEXT="$(python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
t = json.loads(f.readline())["topics"]["tree"]["data"]
ids = [x["id"] for x in t["sessions"]]
print(t["sessions"][(ids.index(t["active"]) + 1) % len(ids)]["name"])
PY
)"
"$COCTL" --socket "$GUI" terminal feed $'\x02e' >/dev/null
sleep 1.5
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" \
          -e 'tell application "System Events" to key code 125' >/dev/null
sleep 1.5 # the selected row's preview popover
[[ -n "$WID" ]] && screencapture -o -x -l"$WID" "$W/sidebar-keys.png" && log "screenshot: $W/sidebar-keys.png"
osascript -e 'tell application "System Events" to key code 36' >/dev/null
for _ in $(seq 1 20); do [[ "$(active_session)" == "$NEXT" ]] && break; sleep 0.5; done
[[ "$(active_session)" == "$NEXT" ]] || fail "Ctrl-b e, Down, Enter did not move to $NEXT: $(active_session)"
log "ok — Ctrl-b e focused the native sidebar; Down + Enter switched to $NEXT"

# Resting the pointer on a sidebar row previews its pane in a card beside the sidebar (drawn in
# the window, so it takes no click). Synthetic pointer moves do not always reach the tracking
# areas, so the row is entered a few times until the area beside it changes.
python3 - "$APP_PID" "$W" <<'PY' || fail "resting the pointer on a sidebar row showed no preview card"
import sys, time, subprocess, Quartz
from PIL import Image, ImageChops, ImageStat
pid, w = int(sys.argv[1]), sys.argv[2]
win = [x for x in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID)
       if x.get("kCGWindowOwnerPID") == pid and x["kCGWindowBounds"]["Width"] > 200][0]
b, wid = win["kCGWindowBounds"], win["kCGWindowNumber"]
def move(x, y):
    Quartz.CGEventPost(Quartz.kCGHIDEventTap, Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventMouseMoved, (x, y), 0))
def shot(name):
    path = f"{w}/{name}.png"
    subprocess.run(["screencapture", "-o", "-x", f"-l{wid}", path], check=True)
    im = Image.open(path).convert("RGB")
    s = im.width / b["Width"]  # retina
    return im.crop((int(270 * s), int(40 * s), int(800 * s), int(400 * s)))
move(b["X"] + 700, b["Y"] + 600); time.sleep(1)
before = shot("sidebar-nohover")
for _ in range(8):
    move(b["X"] + 700, b["Y"] + 600); time.sleep(0.3)
    for dx in (60, 70, 80):
        move(b["X"] + dx, b["Y"] + 130); time.sleep(0.05)
    time.sleep(2)
    after = shot("sidebar-hover")
    if sum(ImageStat.Stat(ImageChops.difference(before, after)).mean) > 6:
        sys.exit(0)
sys.exit(1)
PY
log "screenshot: $W/sidebar-hover.png"
log "ok — resting the pointer on a sidebar row showed its pane preview"
# With the card up, a click on the sidebar's resume button still lands (the card is not a
# window that eats the first click).
sleep 1
BEFORE="$(tabs_total)"
python3 - "$APP_PID" <<'PY'
import sys, time, Quartz
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") == int(sys.argv[1]) and w["kCGWindowBounds"]["Width"] > 200:
        b = w["kCGWindowBounds"]; break
pt = (b["X"] + 219, b["Y"] + 82)
for kind in (Quartz.kCGEventLeftMouseDown, Quartz.kCGEventLeftMouseUp):
    Quartz.CGEventPost(Quartz.kCGHIDEventTap, Quartz.CGEventCreateMouseEvent(None, kind, pt, Quartz.kCGMouseButtonLeft))
    time.sleep(0.05)
PY
sleep 3
osascript -e 'tell application "System Events" to key code 36' >/dev/null
for _ in $(seq 1 20); do [[ "$(tabs_total)" -gt "$BEFORE" ]] && break; sleep 0.5; done
[[ "$(tabs_total)" -gt "$BEFORE" ]] || fail "a click on the sidebar with the preview card up did not land"
log "ok — with the preview card up, the sidebar's resume button still took the first click"

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
