#!/usr/bin/env bash
# Linux e2e for images through the relay's shadow terminal (decisions #190), driven from a Mac:
# a REAL copad-linux runs on thinkarch under gtk4-broadwayd (nothing on the live desktop, its
# own config and state: scripts/e2e-linux-images.sh HOLD_UNTIL=…), real kitten and yazi run in it
# over ssh, and the broadway page is screenshotted here with tabd through an ssh tunnel. The
# checks are pixel counts of a pure red fixture image in those captures.
#
#   scripts/e2e-linux-images-remote.sh
#   HOST=marshall@thinkarch TOOLS=~/dev/copad-img-tools KEEP=1 …
#
# TOOLS on the host holds `kitten` (kitten-linux-amd64) and `yz/*/yazi` (the release zip).
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${HOST:-marshall@thinkarch}"
TOOLS="${TOOLS:-~/dev/copad-img-tools}"
COPY="dev/copad-img" # the build copy on the host (never the user's ~/dev/copad)
PORT=18097
log() { printf '[e2e-linux-img] %s\n' "$*" >&2; }
fail() { log "FAIL: $*"; exit 1; }

rsync -a --delete --exclude target --exclude .build --exclude .git "$REPO/" "$HOST:$COPY/"
ssh "$HOST" "cd ~/$COPY && cargo build -q -p copad-linux -p copad-cli"

W="$(mktemp -d /tmp/clim.XXXX)"
STOP="/tmp/copad-img-stop.$$"
TUN=""
cleanup() {
    ssh "$HOST" "touch $STOP" 2>/dev/null || true
    [[ -n "$TUN" ]] && kill "$TUN" 2>/dev/null || true
    sleep 2
    ssh "$HOST" "rm -f $STOP" 2>/dev/null || true
    [[ "${KEEP:-}" == 1 ]] && { log "kept $W"; return 0; }
    rm -rf "$W"
}
trap cleanup EXIT

ssh "$HOST" "cd ~/$COPY && HOLD_UNTIL=$STOP scripts/e2e-linux-images.sh" >"$W/host.log" 2>&1 &
for _ in $(seq 1 120); do grep -q '^ready ' "$W/host.log" && break; sleep 0.5; done
read -r _ GUI HW < <(grep '^ready ' "$W/host.log") || fail "host copad did not come up: $(tail -5 "$W/host.log")"
log "host copad: $GUI (work dir $HW)"
ssh -N -L "$PORT:127.0.0.1:8097" "$HOST" & TUN=$!
sleep 1.5

coctl() { ssh "$HOST" "~/$COPY/target/debug/coctl --socket $GUI $*"; }
run() { coctl "terminal exec $(printf '%q' "$1")" >/dev/null; }
feed() { coctl "terminal feed $(printf '%q' "$1")" >/dev/null; }
wait_screen() {
    for _ in $(seq 1 60); do coctl "terminal read" 2>/dev/null | grep -q "$1" && return 0; sleep 0.5; done
    fail "never saw '$1'"
}
tabd navigate "http://127.0.0.1:$PORT/" >/dev/null
tabd set-viewport 1200 800 >/dev/null 2>&1 || true
sleep 2
red() {
    tabd screenshot --out "$W/$1.png" >/dev/null
    python3 - "$W/$1.png" <<'PY'
import sys, Quartz
from Foundation import NSURL
img = Quartz.CGImageSourceCreateImageAtIndex(Quartz.CGImageSourceCreateWithURL(NSURL.fileURLWithPath_(sys.argv[1]), None), 0, None)
w, h = Quartz.CGImageGetWidth(img), Quartz.CGImageGetHeight(img)
ctx = Quartz.CGBitmapContextCreate(None, w, h, 8, w * 4, Quartz.CGColorSpaceCreateDeviceRGB(), Quartz.kCGImageAlphaPremultipliedLast)
Quartz.CGContextDrawImage(ctx, Quartz.CGRectMake(0, 0, w, h), img)
d = bytes(Quartz.CGBitmapContextGetData(ctx).as_buffer(w * h * 4))
print(sum(1 for i in range(0, len(d), 4) if d[i] > 200 and d[i + 1] < 60 and d[i + 2] < 60))
PY
}

FX="$HW/fx"
ssh "$HOST" "mkdir -p $FX && python3 - $FX/red.png" <<'PY'
import struct, sys, zlib
w, h = 160, 100
raw = b"".join(b"\x00" + b"\xff\x00\x00" * w for _ in range(h))
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
open(sys.argv[1], "wb").write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
PY
ssh "$HOST" "echo text >$FX/a.txt"

run "$TOOLS/kitten icat --detect-support 2>$HW/detect.err; echo detect=\$?"
wait_screen "detect="
coctl "terminal read" | grep -q "detect=0" || fail "kitten --detect-support: $(ssh "$HOST" cat "$HW/detect.err")"
log "ok — kitten icat --detect-support: $(ssh "$HOST" cat "$HW/detect.err")"

run "clear; $TOOLS/kitten icat $FX/red.png; echo icat-done"
wait_screen icat-done
sleep 1.5
n="$(red icat)"; (( n > 4000 )) || fail "icat: $n red pixels"
log "ok — kitten icat drew the image ($n red pixels)"
run "seq 1 3; echo scrolled"
wait_screen scrolled
sleep 1.5
n="$(red icat-scrolled)"; (( n > 4000 )) || fail "after more output: $n red pixels"
log "ok — the image scrolled with the text and is still drawn ($n)"
# Under the text (z < 0): the layer below VTE, seen through the default background with text
# over it. (On VTE a cell that paints its own background covers it too — decisions #190.)
run "clear; for i in 1 2 3 4 5 6 7 8; do echo text-over-the-image; done; printf '\\033[8A'; $TOOLS/kitten icat --align left --z-index -1 $FX/red.png; echo under-done"
wait_screen under-done
sleep 1.5
n="$(red under)"; (( n > 3000 )) || fail "a z < 0 image under text: $n red pixels"
coctl "terminal read" | grep -q "text-over-the-image" || fail "the text over the z < 0 image is gone"
log "ok — a z < 0 image shows under the text ($n)"
run "clear"
sleep 1.5
n="$(red cleared)"; (( n < 50 )) || fail "clear left $n red pixels"
log "ok — clear removed it ($n)"

run "cd $FX && YAZI_CONFIG_HOME=$HW/yazi $(ssh "$HOST" "ls $TOOLS/yz/*/yazi") red.png"
sleep 5
n="$(red yazi)"; (( n > 2000 )) || fail "yazi preview: $n red pixels"
log "ok — yazi previewed the image ($n)"
feed "k"; sleep 2.5
n="$(red yazi-text)"; (( n < 50 )) || fail "yazi left $n red pixels on a.txt"
log "ok — moving to a.txt erased it ($n)"
feed "j"; sleep 2.5
n="$(red yazi-back)"; (( n > 2000 )) || fail "yazi did not redraw ($n)"
log "ok — moving back redrew it ($n)"
feed "q"; sleep 1.5
n="$(red yazi-quit)"; (( n < 50 )) || fail "quitting yazi left $n red pixels"
log "ok — quitting yazi left nothing ($n)"
log "PASS"
