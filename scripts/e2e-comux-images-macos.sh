#!/usr/bin/env bash
# macOS e2e for images inside comux (decisions #194): an isolated comux server (own socket and
# state — never the user's), a comux client in a REAL dev Copad.app (a separate instance), and
# real kitten icat and yazi running in the comux PANE. comux turns the pane's images into runs
# the client places in Copad; the checks count the pure red fixture's pixels in window captures.
#
#   KITTEN=/path/to/kitten YAZI=/path/to/yazi scripts/e2e-comux-images-macos.sh   (KEEP=1 keeps)
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
: "${KITTEN:?set KITTEN to a kitten binary}" "${YAZI:?set YAZI to a yazi binary}"
COCTL="$REPO/target/release/coctl"
COMUX="$REPO/target/debug/comux"
APP="$REPO/copad-macos/.build/debug/Copad.app"
log() { printf '[e2e-comux-img] %s\n' "$*" >&2; }
fail() { log "FAIL: $*"; exit 1; }

cargo build -q -p copad-mux
cargo build -q --release -p copad-ffi -p copad-term -p copad-cli
(cd copad-macos && rm -f .build/debug/Copad && swift build -q)
cp copad-macos/.build/debug/Copad "$APP/Contents/MacOS/Copad"
scripts/codesign-dev.sh "$APP" >/dev/null 2>&1 || true

W="$(mktemp -d /tmp/ccim.XXXX)"
APP_PID=""
CAF=""
cleanup() {
    [[ -n "$CAF" ]] && kill "$CAF" 2>/dev/null || true
    if [[ -n "$APP_PID" ]]; then
        kill "$APP_PID" 2>/dev/null || true
        for _ in $(seq 1 100); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.1; done
    fi
    COPAD_MUX_SOCK="$W/s" "$COMUX" kill-server >/dev/null 2>&1 || true
    [[ "${KEEP:-}" == 1 ]] && { log "kept $W"; return 0; }
    rm -rf "$W"
}
trap cleanup EXIT
export XDG_CONFIG_HOME="$W/cfg" COPAD_MUX_SOCK="$W/s" COPAD_MUX_STATE="$W/state.json" COPAD_MUX_NOTIFY=0 \
       COPAD_MUX_USAGE=0 COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1
unset COPAD_MUX_PANE COPAD_MUX COPAD_SOCKET COPAD_PANEL_ID COPAD_MUX_HOST_TAG
mkdir -p "$W/cfg/copad" "$W/fx" "$W/yazi"
printf '[terminal]\nimages = true\n' >"$W/cfg/copad/config.toml"
# Panes run a plain shell: the user's rc files would print into the captures.
SHELL=/bin/sh "$COMUX" server >"$W/server.log" 2>&1 &
for _ in $(seq 1 50); do "$COMUX" list-sessions >/dev/null 2>&1 && break; sleep 0.1; done
# Fixtures: a pure red PNG (counted in screenshots) and a text file to move the yazi cursor to.
python3 - "$W/fx/red.png" <<'PY'
import struct, sys, zlib
w, h = 240, 160
raw = b"".join(b"\x00" + b"\xff\x00\x00" * w for _ in range(h))
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) \
    + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
PY
echo "plain text" >"$W/fx/a.txt"
# Top half red, bottom half blue: catches a flipped or mis-mapped image.
python3 - "$W/split.png" <<'PY'
import struct, sys, zlib
w, h = 120, 120
raw = b"".join(b"\x00" + (b"\xff\x00\x00" if y < h // 2 else b"\x00\x00\xff") * w for y in range(h))
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) \
    + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
PY

# Raw reply to one query, read off the pane's tty: `reply.py <out> <query-escaped>`.
cat >"$W/reply.py" <<'PY'
import os, select, sys, termios, tty
fd = os.open("/dev/tty", os.O_RDWR)
old = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    os.write(fd, sys.argv[2].encode().decode("unicode_escape").encode("latin-1"))
    buf = b""
    while True:
        r, _, _ = select.select([fd], [], [], 1.5)
        if not r:
            break
        buf += os.read(fd, 256)
        if buf.endswith(b"t") or buf.endswith(b"\x1b\\"):
            break
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
open(sys.argv[1], "wb").write(buf)
PY

# Red pixels in a window capture (the fixture is pure red; theme red is pink, g > 100).
red_pixels() {
    python3 - "$@" <<'PY'
import sys, Quartz
from Foundation import NSURL
src = Quartz.CGImageSourceCreateWithURL(NSURL.fileURLWithPath_(sys.argv[1]), None)
img = Quartz.CGImageSourceCreateImageAtIndex(src, 0, None)
w, h = Quartz.CGImageGetWidth(img), Quartz.CGImageGetHeight(img)
cs = Quartz.CGColorSpaceCreateDeviceRGB()
ctx = Quartz.CGBitmapContextCreate(None, w, h, 8, w * 4, cs, Quartz.kCGImageAlphaPremultipliedLast)
Quartz.CGContextDrawImage(ctx, Quartz.CGRectMake(0, 0, w, h), img)
data = bytes(Quartz.CGBitmapContextGetData(ctx).as_buffer(w * h * 4))
red = blue = ry = by = 0
for i in range(0, len(data), 4):
    r, g, b = data[i], data[i + 1], data[i + 2]
    if g < 60 and r > 200 and b < 60:
        red += 1; ry += i // (w * 4)
    elif g < 60 and b > 200 and r < 60:
        blue += 1; by += i // (w * 4)
if len(sys.argv) > 2:
    # Orientation: red must sit above blue.
    print("ok" if red > 1000 and blue > 1000 and ry / red < by / blue else f"bad red={red} blue={blue}")
else:
    print(red)
PY
}

launch() {
    before="$(pgrep -x Copad || true)"
    open -n --env XDG_CONFIG_HOME="$XDG_CONFIG_HOME" --env COPAD_STATE_DIR="$W/state" \
        --env COPAD_MUX_SOCK="$COPAD_MUX_SOCK" --env COPAD_MUX_STATE="$COPAD_MUX_STATE" "$APP"
    for _ in $(seq 1 100); do
        APP_PID="$(comm -13 <(echo "$before" | sort) <(pgrep -x Copad | sort) | head -1)"
        [[ -n "$APP_PID" && -S "/tmp/copad-$APP_PID.sock" ]] && break
        sleep 0.1
    done
    GUI="/tmp/copad-$APP_PID.sock"
    [[ -S "$GUI" ]] || fail "dev Copad did not open its socket"
    log "dev Copad pid $APP_PID ($PASS)"
    sleep 2
    osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" || true
    WID="$(python3 - "$APP_PID" <<'PY'
import sys, Quartz
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") == int(sys.argv[1]) and w["kCGWindowBounds"]["Width"] > 200:
        print(w["kCGWindowNumber"]); break
PY
)"
    [[ -n "$WID" ]] || fail "no window for the dev Copad"
}
shot() { screencapture -o -x -l"$WID" "$W/$PASS-$1.png" || fail "screenshot $1"; red_pixels "$W/$PASS-$1.png" "${@:2}"; }
run() { "$COCTL" --socket "$GUI" terminal exec "$1" >/dev/null; }
feed() { "$COCTL" --socket "$GUI" terminal feed "$1" >/dev/null; }
wait_screen() {
    for _ in $(seq 1 60); do "$COCTL" --socket "$GUI" terminal read 2>/dev/null | grep -q "$1" && return 0; sleep 0.25; done
    fail "never saw '$1' on screen"
}


caffeinate -d -u -t 400 & CAF=$!
PASS=comux
launch
# The comux client in Copad's tab; everything typed into the tab now goes to the comux pane.
run "env COPAD_MUX_SOCK=$COPAD_MUX_SOCK COPAD_MUX_QUIET_SSH=1 $COMUX"
sleep 3
run "clear; echo pane-ready"
wait_screen pane-ready

run "clear; $KITTEN icat --detect-support 2>&1; echo detect=\$?"
wait_screen "detect="
"$COCTL" --socket "$GUI" terminal read | grep -q "detect=0" || fail "kitten in a comux pane sees no graphics: $("$COCTL" --socket "$GUI" terminal read | grep -v '^ *$' | tail -5)"
log "ok — kitten in the comux pane detects graphics"

run "clear; $KITTEN icat --align left $W/fx/red.png; echo icat-done"
wait_screen icat-done
sleep 1
n="$(shot icat)"
(( n > 20000 )) || fail "icat in a comux pane: only $n red pixels"
log "ok — kitten icat in a comux pane drew through comux into Copad ($n red pixels)"
run "clear"
sleep 1
n="$(shot cleared)"
(( n < 100 )) || fail "clear in the pane left $n red pixels"
log "ok — clear in the pane removed it ($n)"

run "cd $W/fx && YAZI_CONFIG_HOME=$W/yazi $YAZI red.png"
sleep 4
n="$(shot yazi-preview)"
(( n > 5000 )) || fail "yazi in a comux pane: only $n red pixels"
log "ok — yazi in a comux pane previewed the image ($n)"
feed "k"; sleep 2
n="$(shot yazi-text)"
(( n < 100 )) || fail "yazi left $n red pixels on a.txt"
log "ok — moving to a.txt erased it ($n)"
feed "j"; sleep 2
n="$(shot yazi-back)"
(( n > 5000 )) || fail "yazi did not redraw ($n)"
log "ok — moving back redrew it ($n)"
feed "q"; sleep 1.5
n="$(shot yazi-quit)"
(( n < 100 )) || fail "quitting yazi left $n red pixels"
log "ok — quitting yazi left nothing ($n)"
log "PASS"
