#!/usr/bin/env bash
# Remote e2e for copad-linux's native comux chrome over SSH (decisions #171), fully on one
# Linux machine and without touching its SSH setup: a throwaway USER-level sshd on a loopback
# port (own host key + authorized_keys), an ssh wrapper handed to copad as `COPAD_SSH` (own
# config + known_hosts), an isolated comux behind that sshd, and copad-linux under headless GTK
# (gtk4-broadwayd). Checks that `comux.open_remote` attaches the remote client in a new tab, finds
# it by its remote tag over the L0 link, claims its chrome, and that the TUI sidebar is gone.
#
#   scripts/e2e-copad-comux-remote-linux.sh     # after cargo build -p copad-linux -p copad-mux -p copad-cli
set -euo pipefail
R="$(cd "$(dirname "$0")/.." && pwd)"
COMUX=$R/target/debug/comux; COCTL=$R/target/debug/coctl; COPAD=$R/target/debug/copad
PORT="${E2E_SSH_PORT:-22433}"
W=$(mktemp -d /tmp/cmrl.XXXX)
SSHD=""; BPID=""; CPID=""; APP=""
cleanup() {
  [[ -n "$APP" ]] && kill "$APP" 2>/dev/null || true
  [[ -n "$CPID" ]] && kill "$CPID" 2>/dev/null || true
  [[ -n "$BPID" ]] && kill "$BPID" 2>/dev/null || true
  [[ -n "$SSHD" ]] && kill "$SSHD" 2>/dev/null || true
  COPAD_MUX_SOCK=$W/s $COMUX kill-server >/dev/null 2>&1 || true
  [[ "${KEEP:-}" == 1 ]] || rm -rf "$W"
}
trap cleanup EXIT
fail() { echo "FAIL: $*"; exit 1; }

# --- throwaway sshd + client config ------------------------------------------------------
ssh-keygen -q -t ed25519 -N '' -f "$W/hostkey"
ssh-keygen -q -t ed25519 -N '' -f "$W/client" -C copad-e2e
cp "$W/client.pub" "$W/authorized_keys"
cat >"$W/sshd_config" <<EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey $W/hostkey
AuthorizedKeysFile $W/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
PidFile $W/sshd.pid
EOF
/usr/sbin/sshd -D -f "$W/sshd_config" -E "$W/sshd.log" & SSHD=$!
cat >"$W/ssh_config" <<EOF
Host e2e-remote
  HostName 127.0.0.1
  Port $PORT
  User $USER
  IdentityFile $W/client
  IdentitiesOnly yes
  UserKnownHostsFile $W/known_hosts
  StrictHostKeyChecking accept-new
  BatchMode yes
EOF
printf '#!/bin/sh\nexec ssh -F %s "$@"\n' "$W/ssh_config" >"$W/sshwrap"
chmod +x "$W/sshwrap"
sleep 1
"$W/sshwrap" e2e-remote true || fail "the throwaway sshd does not accept the test key"

# --- the "remote" comux (isolated) -------------------------------------------------------
RENV="COPAD_MUX_SOCK=$W/s COPAD_MUX_STATE=$W/st.json COPAD_MUX_NOTIFY=0 COPAD_MUX_USAGE=0 COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1"
export COPAD_MUX_SOCK=$W/s
unset COPAD_MUX_PANE COPAD_MUX COPAD_SOCKET COPAD_PANEL_ID
env $RENV $COMUX server >"$W/server.log" 2>&1 &
for _ in $(seq 1 50); do $COMUX list-sessions >/dev/null 2>&1 && break; sleep 0.1; done
env $RENV $COMUX new-session --detached rl-two >/dev/null
clients() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
print(json.dumps(json.loads(f.readline())["topics"]["clients"]["data"]))
PY
}

# --- copad-linux, headless, with the ssh wrapper and NO local comux ----------------------
gtk4-broadwayd :19 >"$W/broadway.log" 2>&1 & BPID=$!
sleep 1
# The local comux socket points nowhere: everything copad shows must come over SSH.
env COPAD_SSH="$W/sshwrap" COPAD_MUX_SOCK="$W/no-local-comux" XDG_CONFIG_HOME="$W/cfg" \
    GDK_BACKEND=broadway BROADWAY_DISPLAY=:19 dbus-run-session -- "$COPAD" >"$W/copad.log" 2>&1 &
CPID=$!
GUI=""
for _ in $(seq 1 100); do
  for p in $(pgrep -x copad); do
    if tr '\0' '\n' </proc/$p/environ 2>/dev/null | grep -qx "COPAD_SSH=$W/sshwrap" && [ -S "$XDG_RUNTIME_DIR/copad/gui-$p.sock" ]; then
      GUI=$XDG_RUNTIME_DIR/copad/gui-$p.sock; APP=$p; break 2
    fi
  done
  sleep 0.2
done
[[ -n "$GUI" ]] || { tail -20 "$W/copad.log"; fail "dev copad did not open its socket"; }
sleep 2

$COCTL --socket "$GUI" call comux.open_remote --params \
  "$(python3 -c 'import json,sys; print(json.dumps({"dest":"e2e-remote","comux":"env "+sys.argv[1]+" "+sys.argv[2]}))' "$RENV" "$COMUX")" >/dev/null
ok=""
for _ in $(seq 1 60); do
  c=$(clients)
  python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if any(x["tag"].startswith("copad-remote:") and {"sidebar","status","overlays"} <= set(x["claimed"]) for x in c["clients"]) and {"sidebar","status"} <= set(c["hidden"]) else 1)' "$c" && { ok=1; break; }
  sleep 1
done
[[ -n "$ok" ]] || { tail -5 "$W/sshd.log"; fail "copad-linux never claimed the remote client's chrome: $(clients)"; }
echo "ok — copad-linux opened the remote comux over SSH, found it by its remote tag and claimed its chrome"
SCREEN=$($COCTL --socket "$GUI" terminal read 2>/dev/null || true)
grep -q "spaces" <<<"$SCREEN" && fail "the remote TUI sidebar is still drawn: $SCREEN"
echo "ok — the remote comux frame in the copad tab has no TUI sidebar"
grep -q "Accepted publickey" "$W/sshd.log" || fail "nothing came through the throwaway sshd"
echo "ok — both connections went through the ssh wrapper and the throwaway sshd"
if [[ -n "${SHOT_WAIT:-}" ]]; then echo "BROADWAY http://127.0.0.1:8099/"; sleep "$SHOT_WAIT"; fi
echo "PASS: copad-linux native chrome over SSH"
