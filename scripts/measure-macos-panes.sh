#!/usr/bin/env bash
# Memory cost of panes in a REAL dev Copad.app (opened as a separate instance with its own config and
# state; the user's Copad is never touched): footprint with one pane, then after opening N more tabs
# that each draw text, so every renderer has rasterized glyphs. Prints the per-pane delta.
#
#   scripts/measure-macos-panes.sh [N=8]       GPU=false … measures the CoreText painter
#   MAX_PER_PANE_MB=6 …                         fail when a pane costs more (a regression gate)
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
N="${1:-8}"
COCTL="$REPO/target/release/coctl"
APP="$REPO/copad-macos/.build/debug/Copad.app"
log() { printf '[panes] %s\n' "$*" >&2; }

cargo build -q --release -p copad-ffi -p copad-term -p copad-cli
(cd copad-macos && rm -f .build/debug/Copad && swift build -q)
cp copad-macos/.build/debug/Copad "$APP/Contents/MacOS/Copad"
scripts/codesign-dev.sh "$APP" >/dev/null 2>&1 || true

W="$(mktemp -d /tmp/cpanes.XXXX)"
APP_PID=""
cleanup() {
    [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null || true
    rm -rf "$W"
}
trap cleanup EXIT
mkdir -p "$W/cfg/copad"
[[ "${GPU:-}" == false ]] && printf '[renderer]\ngpu = false\n' >"$W/cfg/copad/config.toml"

before="$(pgrep -x Copad || true)"
open -n --env XDG_CONFIG_HOME="$W/cfg" --env COPAD_STATE_DIR="$W/state" "$APP"
for _ in $(seq 1 100); do
    APP_PID="$(comm -13 <(echo "$before" | sort) <(pgrep -x Copad | sort) | head -1)"
    [[ -n "$APP_PID" && -S "/tmp/copad-$APP_PID.sock" ]] && break
    sleep 0.1
done
GUI="/tmp/copad-$APP_PID.sock"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null
sleep 3

fill() { "$COCTL" --socket "$GUI" terminal exec "clear; for i in \$(seq 1 40); do printf '%s 한글 ✓ λ → %s\\n' \$i \$(printf 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789'); done" >/dev/null; }
mb() { footprint -p "$APP_PID" 2>/dev/null | awk '/phys_footprint:/ { v=$2; u=$3; if (u ~ /GB/) v*=1024; if (u ~ /KB/) v/=1024; print v; exit }'; }

fill
sleep 3
one="$(mb)"
for _ in $(seq 1 "$N"); do
    "$COCTL" --socket "$GUI" tab new >/dev/null
    sleep 1.2
    fill
    sleep 0.8
done
sleep 3
many="$(mb)"
per="$(python3 -c "print(round(($many - $one) / $N, 1))")"
log "footprint: 1 pane ${one} MB, $((N + 1)) panes ${many} MB → ${per} MB per extra pane"
if [[ -n "${MAX_PER_PANE_MB:-}" ]]; then
    python3 -c "import sys; sys.exit(0 if $per <= $MAX_PER_PANE_MB else 1)" || { log "FAIL: ${per} MB > ${MAX_PER_PANE_MB} MB"; exit 1; }
fi

# A background tab gave its drawables back: switching to it must still show its text.
"$COCTL" --socket "$GUI" tab switch 0 >/dev/null
sleep 1.5
WID="$(python3 - "$APP_PID" <<'PY'
import sys, Quartz
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") == int(sys.argv[1]) and w["kCGWindowBounds"]["Width"] > 200:
        print(w["kCGWindowNumber"]); break
PY
)"
screencapture -o -x -l"$WID" "$W/back.png"
lit="$(python3 - "$W/back.png" <<'PY'
import sys, Quartz
from Foundation import NSURL
img = Quartz.CGImageSourceCreateImageAtIndex(Quartz.CGImageSourceCreateWithURL(NSURL.fileURLWithPath_(sys.argv[1]), None), 0, None)
w, h = Quartz.CGImageGetWidth(img), Quartz.CGImageGetHeight(img)
ctx = Quartz.CGBitmapContextCreate(None, w, h, 8, w * 4, Quartz.CGColorSpaceCreateDeviceRGB(), Quartz.kCGImageAlphaPremultipliedLast)
Quartz.CGContextDrawImage(ctx, Quartz.CGRectMake(0, 0, w, h), img)
d = bytes(Quartz.CGBitmapContextGetData(ctx).as_buffer(w * h * 4))
print(sum(1 for i in range(0, len(d), 4) if d[i] > 150 and d[i + 1] > 150 and d[i + 2] > 150))
PY
)"
log "after switching back to tab 1: $lit text pixels"
(( lit > 40000 )) || { log "FAIL: the first tab did not repaint"; exit 1; }
