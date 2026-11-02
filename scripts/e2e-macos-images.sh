#!/usr/bin/env bash
# macOS e2e for the kitty graphics protocol (decisions #189): a REAL dev Copad.app (opened as a
# separate instance — the user's Copad is never touched) runs the REAL clients that use it:
# `kitten icat` and yazi's image preview. Asserts on the pixels of window screenshots (a pure red
# fixture image, counted in the capture) and on the protocol replies the pane receives.
#
#   KITTEN=/path/to/kitten YAZI=/path/to/yazi scripts/e2e-macos-images.sh
#   KEEP=1 …    # keep the work dir (screenshots)
#
# kitten: https://github.com/kovidgoyal/kitty/releases (kitten-darwin-arm64);
# yazi:   https://github.com/sxyazi/yazi/releases (yazi-aarch64-apple-darwin.zip).
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
: "${KITTEN:?set KITTEN to a kitten binary}" "${YAZI:?set YAZI to a yazi binary}"
COCTL="$REPO/target/release/coctl"
APP="$REPO/copad-macos/.build/debug/Copad.app"
log() { printf '[e2e-images] %s\n' "$*" >&2; }
fail() { log "FAIL: $*"; exit 1; }

cargo build -q --release -p copad-ffi -p copad-term -p copad-cli
(cd copad-macos && rm -f .build/debug/Copad && swift build -q)
[[ -d "$APP/Contents/MacOS" ]] || fail "no dev bundle at $APP (run copad-macos/run.sh once)"
cp copad-macos/.build/debug/Copad "$APP/Contents/MacOS/Copad"
scripts/codesign-dev.sh "$APP" >/dev/null 2>&1 || true

W="$(mktemp -d /tmp/cimg.XXXX)"
APP_PID=""
CAF=""
cleanup() {
    [[ -n "$CAF" ]] && kill "$CAF" 2>/dev/null || true
    if [[ -n "$APP_PID" ]]; then
        kill "$APP_PID" 2>/dev/null || true
        for _ in $(seq 1 100); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.1; done
        kill -9 "$APP_PID" 2>/dev/null || true
    fi
    [[ "${KEEP:-}" == 1 ]] && { log "kept $W"; return 0; }
    rm -rf "$W"
}
trap cleanup EXIT

export XDG_CONFIG_HOME="$W/cfg"
unset COPAD_MUX_PANE COPAD_MUX COPAD_SOCKET COPAD_PANEL_ID
mkdir -p "$W/cfg/copad" "$W/fx" "$W/yazi"
printf '[terminal]\nimages = true\n' >"$W/cfg/copad/config.toml"

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
    open -n --env XDG_CONFIG_HOME="$XDG_CONFIG_HOME" --env COPAD_STATE_DIR="$W/state" "$APP"
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
quit_app() {
    kill "$APP_PID" 2>/dev/null || true
    for _ in $(seq 1 100); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.1; done
    APP_PID=""
}
shot() { screencapture -o -x -l"$WID" "$W/$PASS-$1.png" || fail "screenshot $1"; red_pixels "$W/$PASS-$1.png" "${@:2}"; }
run() { "$COCTL" --socket "$GUI" terminal exec "$1" >/dev/null; }
feed() { "$COCTL" --socket "$GUI" terminal feed "$1" >/dev/null; }
wait_screen() {
    for _ in $(seq 1 60); do "$COCTL" --socket "$GUI" terminal read 2>/dev/null | grep -q "$1" && return 0; sleep 0.25; done
    fail "never saw '$1' on screen"
}

# Pictures every painter must get right: icat, orientation, clear, and yazi's preview lifecycle.
pictures() {
    run "clear; $KITTEN icat $W/fx/red.png; echo icat-done"
    wait_screen icat-done
    sleep 0.5
    n="$(shot icat)"
    (( n > 20000 )) || fail "icat: only $n red pixels"
    log "ok — kitten icat drew the image ($n red pixels)"
    run "clear; $KITTEN icat $W/split.png; echo split-done"
    wait_screen split-done
    sleep 0.5
    o="$(shot split orientation)"
    [[ "$o" == ok ]] || fail "image orientation: $o"
    log "ok — the image is upright (red above blue)"
    # Under the text (z < 0) but over cells that paint their own background.
    run "clear; for i in 1 2 3 4 5 6 7 8 9 10 11 12; do printf '\\033[42m%60s\\033[0m\\n' ''; done; printf '\\033[12A'; $KITTEN icat --z-index -1 $W/fx/red.png; echo under-done"
    wait_screen under-done
    sleep 0.5
    n="$(shot under-text)"
    (( n > 5000 )) || fail "a z < 0 image over coloured cells: only $n red pixels"
    log "ok — a z < 0 image shows over cell backgrounds ($n)"
    run "clear"
    sleep 0.8
    n="$(shot cleared)"
    (( n < 100 )) || fail "clear left $n red pixels"
    log "ok — clear removed it ($n)"

    run "cd $W/fx && YAZI_CONFIG_HOME=$W/yazi $YAZI red.png"
    sleep 4
    n="$(shot yazi-preview)"
    (( n > 5000 )) || fail "yazi preview: only $n red pixels"
    log "ok — yazi previewed the image ($n red pixels)"
    feed "k"
    sleep 2
    n="$(shot yazi-text)"
    (( n < 100 )) || fail "yazi left $n red pixels after moving to the text file"
    log "ok — moving to a.txt erased the preview ($n)"
    feed "j"
    sleep 2
    n="$(shot yazi-back)"
    (( n > 5000 )) || fail "yazi did not redraw the preview ($n)"
    log "ok — moving back redrew it ($n)"
    feed "q"
    sleep 1
    n="$(shot yazi-quit)"
    (( n < 100 )) || fail "quitting yazi left $n red pixels"
    log "ok — quitting yazi left no image behind ($n)"
}

caffeinate -d -u -t 400 & CAF=$!
PASS=metal
launch

# 1. The cell size is reported in device pixels (CSI 16 t), so programs can size images.
run "python3 $W/reply.py $W/cell.out '\\x1b[16t'; echo cell-done"
wait_screen cell-done
cell="$(tr -d '\033' <"$W/cell.out")"
[[ "$cell" =~ ^\[6\;([0-9]+)\;([0-9]+)t$ ]] || fail "CSI 16 t answered '$cell'"
(( BASH_REMATCH[1] > 4 && BASH_REMATCH[2] > 4 )) || fail "cell size $cell"
log "ok — CSI 16 t: $cell"

# 2. kitten's own capability probe (needs a DA1 reply longer than VT102's).
run "$KITTEN icat --detect-support 2>$W/detect.err; echo detect=\$?"
wait_screen "detect="
"$COCTL" --socket "$GUI" terminal read | grep -q "detect=0" || fail "kitten icat --detect-support: $(cat "$W/detect.err")"
log "ok — kitten icat --detect-support: $(cat "$W/detect.err")"

# 3. The pane names itself, not the terminal Copad was started from.
run "echo tp=\$TERM_PROGRAM"
wait_screen "tp=copad"
log "ok — TERM_PROGRAM=copad"

pictures

# 4. images = false (hot reload): the probe goes unanswered, kitten sees no support.
printf '[terminal]\nimages = false\n' >"$W/cfg/copad/config.toml"
sleep 2
run "clear; $KITTEN icat --detect-support 2>/dev/null; echo off=\$?"
wait_screen "off="
"$COCTL" --socket "$GUI" terminal read | grep -q "off=1" || fail "images = false still detected support"
log "ok — images = false: kitten detects no support"
quit_app

# 5. The CoreText painter draws the same pictures.
printf '[terminal]\nimages = true\n[renderer]\ngpu = false\n' >"$W/cfg/copad/config.toml"
PASS=coretext
launch
pictures
quit_app
log "PASS"
