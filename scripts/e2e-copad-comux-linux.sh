#!/usr/bin/env bash
# Linux e2e for copad's native comux sidebar (decisions #169): headless GTK (gtk4-broadwayd,
# so nothing appears on the live desktop), an isolated comux (own socket + state), a REAL
# copad-linux running a REAL comux client in a tab. Checks the claim, that comux stops drawing
# its TUI sidebar in that tab, and that quitting copad gives it back.
#
#   scripts/e2e-copad-comux-linux.sh                 # after cargo build -p copad-linux -p copad-mux -p copad-cli
#   SHOT_WAIT=25 scripts/e2e-copad-comux-linux.sh    # hold the window for a screenshot of
#                                                    # http://127.0.0.1:8097/ (the broadway page)
#   OVERLAY_DRIVER=1 …   # also open comux's rename prompt (Ctrl-b $) and wait up to 90s for
#                        # something to type "linuxrenamed" + Enter into the native dialog on
#                        # the broadway page (the Mac drives it with tabd through an ssh tunnel)
set -euo pipefail
R="$(cd "$(dirname "$0")/.." && pwd)"
COMUX=$R/target/debug/comux; COCTL=$R/target/debug/coctl; COPAD=$R/target/debug/copad
W=$(mktemp -d /tmp/cml.XXXX)
BPID=""; CPID=""
cleanup() {
  [[ -n "${APP:-}" ]] && kill "$APP" 2>/dev/null || true
  [[ -n "$CPID" ]] && kill "$CPID" 2>/dev/null || true
  [[ -n "$BPID" ]] && kill "$BPID" 2>/dev/null || true
  COPAD_MUX_SOCK=$W/s $COMUX kill-server >/dev/null 2>&1 || true
  [[ "${KEEP:-}" == 1 ]] || rm -rf "$W"
}
trap cleanup EXIT
fail() { echo "FAIL: $*"; exit 1; }
export COPAD_MUX_SOCK=$W/s COPAD_MUX_STATE=$W/st.json COPAD_MUX_NOTIFY=0 COPAD_MUX_USAGE=0 \
       COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1
unset COPAD_MUX_PANE COPAD_MUX COPAD_SOCKET COPAD_PANEL_ID
$COMUX server >$W/server.log 2>&1 &
for _ in $(seq 1 50); do $COMUX list-sessions >/dev/null 2>&1 && break; sleep 0.1; done
$COMUX new-session --detached linux-two >/dev/null

clients() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
print(json.dumps(json.loads(f.readline())["topics"]["clients"]["data"]))
PY
}

gtk4-broadwayd :17 >$W/broadway.log 2>&1 & BPID=$!
sleep 1
XDG_CONFIG_HOME=$W/cfg GDK_BACKEND=broadway BROADWAY_DISPLAY=:17 dbus-run-session -- $COPAD >$W/copad.log 2>&1 &
CPID=$!
GUI=""
for _ in $(seq 1 100); do
  for p in $(pgrep -x copad); do
    if tr '\0' '\n' </proc/$p/environ 2>/dev/null | grep -qx "COPAD_MUX_SOCK=$W/s" && [ -S "$XDG_RUNTIME_DIR/copad/gui-$p.sock" ]; then
      GUI=$XDG_RUNTIME_DIR/copad/gui-$p.sock; APP=$p; break 2
    fi
  done
  sleep 0.2
done
[[ -n "$GUI" ]] || { tail -20 $W/copad.log; fail "dev copad did not open its socket"; }
echo "gui $GUI"
sleep 3
$COCTL --socket "$GUI" terminal exec "env COPAD_MUX_SOCK=$W/s COPAD_MUX_QUIET_SSH=1 $COMUX" >/dev/null
ok=""
for _ in $(seq 1 60); do
  c=$(clients)
  python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if any(x["tag"].startswith("copad:") and {"sidebar","status","overlays"} <= set(x["claimed"]) for x in c["clients"]) and {"sidebar","status"} <= set(c["hidden"]) else 1)' "$c" && { ok=1; break; }
  sleep 0.5
done
[[ -n "$ok" ]] || fail "copad-linux never claimed the sidebar: $(clients)"
echo "ok — copad-linux found its tab's comux by tag and claimed sidebar, status and overlays"
SCREEN=$($COCTL --socket "$GUI" terminal read 2>/dev/null || true)
grep -q "spaces" <<<"$SCREEN" && fail "TUI sidebar still drawn: $SCREEN"
echo "ok — comux in the copad tab no longer draws its sidebar"
$COMUX new-session --detached linux-three >/dev/null
if [[ -n "${OVERLAY_DRIVER:-}" ]]; then
  names() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
print(" ".join(x["name"] for x in json.loads(f.readline())["topics"]["tree"]["data"]["sessions"]))
PY
  }
  $COCTL --socket "$GUI" terminal feed $'\x02$' >/dev/null
  echo "OVERLAY_OPEN"
  for _ in $(seq 1 180); do names | grep -q linuxrenamed && break; sleep 0.5; done
  names | grep -q linuxrenamed || fail "the native rename dialog never renamed the session: $(names)"
  SCREEN=$($COCTL --socket "$GUI" terminal read 2>/dev/null || true)
  grep -q "rename session" <<<"$SCREEN" && fail "the TUI drew the prompt as well"
  echo "ok — Ctrl-b \$ became a native GTK dialog and its answer renamed the session"
fi
sleep 2
echo "BROADWAY http://127.0.0.1:8097/  W=$W"
if [[ -n "${SHOT_WAIT:-}" ]]; then sleep "$SHOT_WAIT"; fi
kill "$APP"; kill "$CPID" 2>/dev/null; CPID=""
for _ in $(seq 1 40); do
  python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if "sidebar" not in c["hidden"] else 1)' "$(clients)" && break
  sleep 0.5
done
python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if "sidebar" not in c["hidden"] else 1)' "$(clients)" || fail "claim survived copad exit"
echo "ok — quitting copad released the claim"
echo "PASS: copad-linux native comux sidebar"
