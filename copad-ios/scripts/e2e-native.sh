#!/usr/bin/env bash
# Simulator e2e for the native comux client (decisions #168): the app connects over REAL SSH
# to a REAL comux, and XCUITest drives it (UITests/NativeE2ETests.swift).
#
# Nothing on this machine is reconfigured: the SSH server is a throwaway USER-level sshd on a
# high loopback port with its own host key and authorized_keys file, and comux runs on its own
# socket + state file (never the user's server). Everything is removed on exit.
#
#   copad-ios/scripts/e2e-native.sh            # build comux + app, run the UI test
#   KEEP=1 copad-ios/scripts/e2e-native.sh     # keep the work dir (sshd log, screenshots)
set -euo pipefail
cd "$(dirname "$0")/.."
IOS="$PWD"
REPO="$(cd .. && pwd)"
COMUX="$REPO/target/debug/comux"
PORT="${E2E_SSH_PORT:-22422}"

log() { printf '[e2e-native] %s\n' "$*" >&2; }

(cd "$REPO" && cargo build -q -p copad-mux) || { log "comux build failed"; exit 2; }

# Short: comux's socket path must fit SUN_LEN.
W="$(mktemp -d /tmp/cie.XXXX)"
SSHD_PID=""
cleanup() {
    [[ -n "$SSHD_PID" ]] && kill "$SSHD_PID" 2>/dev/null || true
    COPAD_MUX_SOCK="$W/s" "$COMUX" kill-server >/dev/null 2>&1 || true
    [[ "${KEEP:-}" == 1 ]] && { log "kept $W"; return 0; }
    rm -rf "$W"
}
trap cleanup EXIT

# --- the SSH server ---------------------------------------------------------------------
ssh-keygen -q -t ed25519 -N '' -f "$W/hostkey"
ssh-keygen -q -t ed25519 -N '' -f "$W/client" -C copad-ios-e2e
cp "$W/client.pub" "$W/authorized_keys"
# The app takes the client key as its 32-byte Ed25519 seed (DeviceKey.importSeed).
SEED="$(python3 - "$W/client" <<'PY'
import base64, struct, sys
raw = open(sys.argv[1]).read().split('-----')[2]
b = base64.b64decode(''.join(raw.split()))
assert b.startswith(b'openssh-key-v1\0')
i = len(b'openssh-key-v1\0')
def s(b, i):
    n = struct.unpack('>I', b[i:i + 4])[0]
    return b[i + 4:i + 4 + n], i + 4 + n
_, i = s(b, i); _, i = s(b, i); _, i = s(b, i); i += 4; _, i = s(b, i); priv, i = s(b, i)
j = 8; _, j = s(priv, j); _, j = s(priv, j); sk, j = s(priv, j)
print(base64.b64encode(sk[:32]).decode())
PY
)"
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
/usr/sbin/sshd -D -f "$W/sshd_config" -E "$W/sshd.log" &
SSHD_PID=$!
sleep 1
kill -0 "$SSHD_PID" || { log "sshd did not start"; cat "$W/sshd.log"; exit 1; }

# --- the comux server ---------------------------------------------------------------------
MUXENV=(COPAD_MUX_SOCK="$W/s" COPAD_MUX_STATE="$W/state.json" COPAD_MUX_NOTIFY=0
        COPAD_MUX_USAGE=0 COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1 XDG_CONFIG_HOME="$W/cfg")
mkdir -p "$W/cfg/copad" "$W/bin"
mux() { env -u COPAD_MUX_PANE -u COPAD_MUX "${MUXENV[@]}" "$COMUX" "$@"; }
mux server >"$W/server.log" 2>&1 &
for _ in $(seq 1 50); do mux list-sessions >/dev/null 2>&1 && break; sleep 0.1; done
mux new-session --detached e2e-two >/dev/null
# A fake agent: `claude` is classified by argv[0]'s basename — a SYMLINK, run by absolute path
# (a copy of a system binary is killed by macOS on exec).
ln -s /bin/sleep "$W/bin/claude"
tok() { mux list-tree --json | python3 -c 'import json,sys; t=json.load(sys.stdin)["tree"]; print(t[int(sys.argv[1])]["tabs"][0]["panes"][0]["token"])' "$1"; }
TOK="$(tok 0)"
# In the OTHER session, so the pane the test types into keeps its focus.
mux split --from "$(tok 1)" -- "$W/bin/claude" 1000 >/dev/null
mux notify --pane "$TOK" --kind done "e2e notification" >/dev/null
# What the app runs per SSH channel: this build of comux on this isolated socket.
REMOTE="env COPAD_MUX_SOCK=$W/s COPAD_MUX_STATE=$W/state.json COPAD_MUX_NOTIFY=0 COPAD_MUX_USAGE=0 COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1 XDG_CONFIG_HOME=$W/cfg $COMUX proxy --stdio"

# --- the app ------------------------------------------------------------------------------
xcodegen generate >/dev/null
DEST='platform=iOS Simulator,name=iPhone 17'
log "running the UI test"
set +e
TEST_RUNNER_E2E_HOST=127.0.0.1 TEST_RUNNER_E2E_PORT="$PORT" TEST_RUNNER_E2E_USER="$USER" \
TEST_RUNNER_E2E_SEED="$SEED" TEST_RUNNER_E2E_HOSTKEY="$(cut -d' ' -f1,2 "$W/hostkey.pub")" \
TEST_RUNNER_E2E_COMMAND="$REMOTE" \
xcodebuild -project copad-ios.xcodeproj -scheme copad-ios -destination "$DEST" \
    -derivedDataPath build -skipPackagePluginValidation \
    -resultBundlePath "$W/e2e.xcresult" \
    test -only-testing:copad-iosUITests/NativeE2ETests >"$W/xcodebuild.log" 2>&1
rc=$?
set -e
grep -E "Test Case .*(passed|failed)|error:|XCTAssert" "$W/xcodebuild.log" | head -20 >&2

# --- what the server saw ------------------------------------------------------------------
# The bytes really reached the PTY (not just the app's own mirror), in the pane they were typed.
CAP="$(mux capture-pane "$TOK" -S 200 2>/dev/null || true)"
grep -q "NATIVE-42" <<<"$CAP" || { log "FAIL: NATIVE-42 never reached the pane"; rc=1; }
grep -q "한글-확인" <<<"$CAP" || { log "FAIL: the Hangul line never reached the pane"; rc=1; }
grep -q "BACK-2" <<<"$CAP" || { log "FAIL: input after resume never reached the pane"; rc=1; }
mkdir -p "$W/shots"
xcrun xcresulttool export attachments --path "$W/e2e.xcresult" --output-path "$W/shots" >/dev/null 2>&1 || true
[[ $rc == 0 ]] && log "PASS: native comux client end to end" || log "FAIL (xcodebuild log: $W/xcodebuild.log)"
exit $rc
