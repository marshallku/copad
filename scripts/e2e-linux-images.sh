#!/usr/bin/env bash
# Linux e2e for `[terminal] images` (sixel through VTE): a REAL copad-linux on a headless GTK
# display (gtk4-broadwayd, so nothing appears on the live desktop) with its own config/state.
# Asserts that the terminal advertises sixel in its DA1 reply only while `images` is on, that
# turning it off on a running copad (config reload) withdraws it, and draws a sixel image so a
# screenshot of http://127.0.0.1:8097/ can confirm the pixels.
#
#   scripts/e2e-linux-images.sh                 # after cargo build -p copad-linux -p copad-cli
#   SHOT_WAIT=30 scripts/e2e-linux-images.sh    # hold the image on screen for a screenshot
set -euo pipefail
R="$(cd "$(dirname "$0")/.." && pwd)"
COCTL=$R/target/debug/coctl; COPAD=$R/target/debug/copad
W=$(mktemp -d /tmp/cimg.XXXX)
BPID=""; CPID=""; APP=""
cleanup() {
  if [[ -n "$APP" ]]; then
    kill "$APP" 2>/dev/null || true
    for _ in $(seq 1 50); do kill -0 "$APP" 2>/dev/null || break; sleep 0.1; done
    rm -f "$XDG_RUNTIME_DIR/copad/gui-$APP.sock" "/tmp/copad-prewarm-$APP.html"
  fi
  [[ -n "$CPID" ]] && kill "$CPID" 2>/dev/null || true
  [[ -n "$BPID" ]] && kill "$BPID" 2>/dev/null || true
  # dbus-activated helpers outlive dbus-run-session holding copad's log under $W.
  sleep 0.5
  for f in /proc/[0-9]*/fd/*; do
    case "$(readlink "$f" 2>/dev/null)" in "$W"/*) p=${f#/proc/}; echo "${p%%/*}" ;; esac
  done 2>/dev/null | sort -u | xargs -r kill 2>/dev/null || true
  [[ "${KEEP:-}" == 1 ]] || rm -rf "$W"
}
trap cleanup EXIT
fail() { echo "FAIL: $*"; exit 1; }
unset COPAD_MUX_PANE COPAD_MUX COPAD_SOCKET COPAD_PANEL_ID

mkdir -p "$W/cfg/copad"
printf '[terminal]\nimages = true\n' >"$W/cfg/copad/config.toml"

# Writes the terminal's DA1 reply to $1, raw, read straight off the pane's tty.
cat >"$W/da1.py" <<'PY'
import os, select, sys, termios, tty
fd = os.open("/dev/tty", os.O_RDWR)
old = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    os.write(fd, b"\x1b[c")
    buf = b""
    while not buf.endswith(b"c"):
        r, _, _ = select.select([fd], [], [], 2)
        if not r:
            break
        buf += os.read(fd, 64)
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
open(sys.argv[1], "wb").write(buf)
PY

gtk4-broadwayd :17 >"$W/broadway.log" 2>&1 & BPID=$!
sleep 1
XDG_STATE_HOME=$W/state XDG_CONFIG_HOME=$W/cfg COPAD_E2E_TAG=$W GDK_BACKEND=broadway BROADWAY_DISPLAY=:17 \
  dbus-run-session -- "$COPAD" >"$W/copad.log" 2>&1 &
CPID=$!
GUI=""
for _ in $(seq 1 100); do
  for p in $(pgrep -x copad); do
    if tr '\0' '\n' </proc/$p/environ 2>/dev/null | grep -qx "COPAD_E2E_TAG=$W" && [ -S "$XDG_RUNTIME_DIR/copad/gui-$p.sock" ]; then
      GUI=$XDG_RUNTIME_DIR/copad/gui-$p.sock; APP=$p; break 2
    fi
  done
  sleep 0.2
done
[[ -n "$GUI" ]] || { tail -20 "$W/copad.log"; fail "dev copad did not open its socket"; }
echo "gui $GUI"
sleep 2

da1() {
  rm -f "$W/da1.out"
  $COCTL --socket "$GUI" terminal exec "python3 $W/da1.py $W/da1.out" >/dev/null
  for _ in $(seq 1 50); do [[ -s "$W/da1.out" ]] && break; sleep 0.1; done
  [[ -s "$W/da1.out" ]] || fail "no DA1 reply"
  tr -d '\033' <"$W/da1.out"
}
has_sixel() { [[ "$1" =~ ^\[\?[0-9\;]*\;4(\;|c) ]]; }

r=$(da1); echo "DA1 with images on: $r"
if grep -q "built without sixel" "$W/copad.log"; then
  # The distro's libvte has no sixel (Arch's doesn't): VTE must not claim it, and copad must
  # have said why images are off.
  has_sixel "$r" && fail "sixel advertised by a VTE built without it ($r)"
  echo "PASS (this libvte has no sixel: the switch is inert and copad logged why)"
  exit 0
fi
has_sixel "$r" || fail "sixel not advertised while images = true ($r)"

printf '[terminal]\nimages = false\n' >"$W/cfg/copad/config.toml"
sleep 2
r=$(da1); echo "DA1 after reload with images off: $r"
has_sixel "$r" && fail "sixel still advertised after images = false ($r)"

printf '[terminal]\nimages = true\n' >"$W/cfg/copad/config.toml"
sleep 2
r=$(da1); echo "DA1 after reload with images on: $r"
has_sixel "$r" || fail "sixel not re-advertised ($r)"

magick -size 240x120 gradient:red-blue "$W/g.png"
$COCTL --socket "$GUI" terminal exec "clear; img2sixel $W/g.png; echo sixel-drawn" >/dev/null
for _ in $(seq 1 50); do
  $COCTL --socket "$GUI" terminal read 2>/dev/null | grep -q '^sixel-drawn' && break; sleep 0.1
done
$COCTL --socket "$GUI" terminal read | grep -q '^sixel-drawn' || fail "img2sixel did not finish"
[[ -n "${SHOT_WAIT:-}" ]] && { echo "holding ${SHOT_WAIT}s for a screenshot"; sleep "$SHOT_WAIT"; }
echo "PASS"
