#!/usr/bin/env bash
# macOS e2e for underline styles (SGR 4:1..4:5 + SGR 58 colour) and focus reports (?1004) on a
# REAL dev Copad.app, opened as a separate instance (the user's Copad is never touched).
#
# Underlines: one row per style, underline colour pure red, text colour the theme's; the capture
# must show red in each row's underline band, and the curly row must spread over more pixel rows
# than the single one. Runs on the Metal and the CoreText painter.
# Focus: a program enables ?1004 and logs what it reads; Finder is brought forward and Copad back,
# and the log must hold CSI O then CSI I.
#
#   scripts/e2e-macos-decorations.sh    KEEP=1 … keeps the screenshots
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
COCTL="$REPO/target/release/coctl"
APP="$REPO/copad-macos/.build/debug/Copad.app"
log() { printf '[e2e-deco] %s\n' "$*" >&2; }
fail() { log "FAIL: $*"; exit 1; }

cargo build -q --release -p copad-ffi -p copad-term -p copad-cli
(cd copad-macos && rm -f .build/debug/Copad && swift build -q)
cp copad-macos/.build/debug/Copad "$APP/Contents/MacOS/Copad"
scripts/codesign-dev.sh "$APP" >/dev/null 2>&1 || true

W="$(mktemp -d /tmp/cdeco.XXXX)"
APP_PID=""
CAF=""
cleanup() {
    [[ -n "$CAF" ]] && kill "$CAF" 2>/dev/null || true
    if [[ -n "$APP_PID" ]]; then
        kill "$APP_PID" 2>/dev/null || true
        for _ in $(seq 1 100); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.1; done
    fi
    [[ "${KEEP:-}" == 1 ]] && { log "kept $W"; return 0; }
    rm -rf "$W"
}
trap cleanup EXIT
export XDG_CONFIG_HOME="$W/cfg"
unset COPAD_MUX_PANE COPAD_MUX COPAD_SOCKET COPAD_PANEL_ID
mkdir -p "$W/cfg/copad"

launch() {
    before="$(pgrep -x Copad || true)"
    open -n --env XDG_CONFIG_HOME="$XDG_CONFIG_HOME" --env COPAD_STATE_DIR="$W/state" "$APP"
    for _ in $(seq 1 100); do
        APP_PID="$(comm -13 <(echo "$before" | sort) <(pgrep -x Copad | sort) | head -1)"
        [[ -n "$APP_PID" && -S "/tmp/copad-$APP_PID.sock" ]] && break
        sleep 0.1
    done
    GUI="/tmp/copad-$APP_PID.sock"
    [[ -S "$GUI" ]] || fail "dev Copad did not open its socket"
    sleep 2
    front
    WID="$(python3 - "$APP_PID" <<'PY'
import sys, Quartz
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") == int(sys.argv[1]) and w["kCGWindowBounds"]["Width"] > 200:
        print(w["kCGWindowNumber"], w["kCGWindowBounds"]["Height"]); break
PY
)"
    [[ -n "$WID" ]] || fail "no window"
}
front() { osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null; }
quit_app() {
    kill "$APP_PID" 2>/dev/null || true
    for _ in $(seq 1 100); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.1; done
    APP_PID=""
}
run() { "$COCTL" --socket "$GUI" terminal exec "$1" >/dev/null; }
wait_screen() {
    for _ in $(seq 1 60); do "$COCTL" --socket "$GUI" terminal read 2>/dev/null | grep -q "$1" && return 0; sleep 0.25; done
    fail "never saw '$1'"
}

# Per text row of the capture: how many pixel rows hold red, top to bottom.
red_rows() {
    python3 - "$1" "$2" <<'PY'
import sys, Quartz
from Foundation import NSURL
src = Quartz.CGImageSourceCreateWithURL(NSURL.fileURLWithPath_(sys.argv[1]), None)
img = Quartz.CGImageSourceCreateImageAtIndex(src, 0, None)
w, h = Quartz.CGImageGetWidth(img), Quartz.CGImageGetHeight(img)
ctx = Quartz.CGBitmapContextCreate(None, w, h, 8, w * 4, Quartz.CGColorSpaceCreateDeviceRGB(), Quartz.kCGImageAlphaPremultipliedLast)
Quartz.CGContextDrawImage(ctx, Quartz.CGRectMake(0, 0, w, h), img)
data = bytes(Quartz.CGBitmapContextGetData(ctx).as_buffer(w * h * 4))
rows = []
for y in range(h):
    base = y * w * 4
    n = sum(1 for x in range(0, w * 4, 4) if data[base + x] > 180 and data[base + x + 1] < 80 and data[base + x + 2] < 80)
    rows.append(n)
# Group consecutive pixel rows holding red into bands.
bands, cur = [], None
for y, n in enumerate(rows):
    if n > 3:
        cur = [y, y, n] if cur is None else [cur[0], y, cur[2] + n]
    elif cur is not None:
        bands.append(cur); cur = None
if cur: bands.append(cur)
# A double underline is two lines with a gap: one band. Specks (window chrome) are not underlines.
merged = []
for b in bands:
    if merged and b[0] - merged[-1][1] <= 3:
        merged[-1] = [merged[-1][0], b[1], merged[-1][2] + b[2]]
    else:
        merged.append(b)
print(" ".join(f"{b[1]-b[0]+1}:{b[2]}" for b in merged if b[2] >= 50))
PY
}

decorations() {
    run "clear; for s in 1 2 3 4 5; do printf '\\033[58;2;255;0;0m\\033[4:%sm%-30s\\033[0m\\n\\n' \$s \"style \$s\"; done; echo deco-done"
    wait_screen deco-done
    sleep 0.6
    screencapture -o -x -l"${WID%% *}" "$W/$PASS-underlines.png"
    bands="$(red_rows "$W/$PASS-underlines.png" x)"
    log "$PASS red bands (height:pixels): $bands"
    read -r -a b <<<"$bands"
    (( ${#b[@]} >= 5 )) || fail "$PASS: expected 5 underline bands, got '${bands}'"
    h1="${b[0]%%:*}"; h3="${b[2]%%:*}"
    p1="${b[0]##*:}"; p4="${b[3]##*:}"; p5="${b[4]##*:}"
    (( h3 > h1 )) || fail "$PASS: curly band ($h3 px) not taller than single ($h1 px)"
    (( p4 < p1 && p5 < p1 )) || fail "$PASS: dotted/dashed ($p4/$p5) not sparser than single ($p1)"
    log "ok — $PASS draws single, double, curly, dotted, dashed in the underline colour"
}

caffeinate -d -u -t 300 & CAF=$!
PASS=metal
launch
decorations

# Focus reports.
cat >"$W/focus.py" <<'PY'
import os, select, sys, termios, tty, time
fd = os.open("/dev/tty", os.O_RDWR)
old = termios.tcgetattr(fd); tty.setraw(fd)
os.write(fd, b"\x1b[?1004h")
buf = b""; end = time.time() + 12
try:
    while time.time() < end and buf.count(b"\x1b[I") < 1 or (time.time() < end and b"\x1b[O" not in buf):
        r, _, _ = select.select([fd], [], [], 0.2)
        if r: buf += os.read(fd, 64)
finally:
    os.write(fd, b"\x1b[?1004l"); termios.tcsetattr(fd, termios.TCSADRAIN, old)
open(sys.argv[1], "wb").write(buf)
PY
run "clear; python3 $W/focus.py $W/focus.out; echo focus-done"
sleep 1.5
osascript -e 'tell application "Finder" to activate' >/dev/null
sleep 1
front
wait_screen focus-done
got="$(python3 -c 'import sys; print(repr(open(sys.argv[1],"rb").read()))' "$W/focus.out")"
python3 - "$W/focus.out" <<'PY' || fail "focus reports: $got"
import sys
b = open(sys.argv[1], "rb").read()
o, i = b.find(b"\x1b[O"), b.rfind(b"\x1b[I")
sys.exit(0 if o >= 0 and i > o else 1)
PY
log "ok — focus out / in reported: $got"
quit_app

printf '[renderer]\ngpu = false\n' >"$W/cfg/copad/config.toml"
PASS=coretext
launch
decorations
quit_app
log "PASS"
