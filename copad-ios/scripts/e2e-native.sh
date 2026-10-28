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
#   E2E_KOREAN=1 copad-ios/scripts/e2e-native.sh  # type and delete on the REAL Korean keyboard,
#                                                 # in a throwaway simulator whose only keyboard it is
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
SIM=""
cleanup() {
    [[ -n "$SIM" ]] && xcrun simctl delete "$SIM" >/dev/null 2>&1 || true
    [[ -n "$SSHD_PID" ]] && kill "$SSHD_PID" 2>/dev/null || true
    COPAD_MUX_SOCK="$W/s" "$COMUX" kill-server >/dev/null 2>&1 || true
    [[ "${KEEP:-}" == 1 ]] && { log "kept $W"; return 0; }
    rm -rf "$W"
}
trap cleanup EXIT

# --- the SSH server ---------------------------------------------------------------------
ssh-keygen -q -t ed25519 -N '' -f "$W/hostkey"
# The client key (decisions #173). Default `enclave`: the app makes a REAL Secure Enclave key
# (the Simulator has one on Apple silicon) and the UI test appends its public line to
# authorized_keys before connecting. `E2E_KEY=ecdsa` imports a software P-256 key (same ECDSA
# path, for a Mac without an enclave); `E2E_KEY=ed25519` the Ed25519 path.
KEYTYPE="${E2E_KEY:-enclave}"
if [[ "$KEYTYPE" == enclave ]]; then
    : >"$W/authorized_keys"
elif [[ "$KEYTYPE" == ed25519 ]]; then
    ssh-keygen -q -t ed25519 -N '' -f "$W/client" -C copad-ios-e2e
else
    ssh-keygen -q -t ecdsa -b 256 -N '' -f "$W/client" -C copad-ios-e2e
fi
[[ "$KEYTYPE" == enclave ]] || cp "$W/client.pub" "$W/authorized_keys"
# The app takes the private key as raw bytes: the 32-byte Ed25519 seed
# (DeviceKey.importSeed) or the 32-byte P-256 scalar (DeviceKey.importP256).
KEYB64=""
[[ "$KEYTYPE" == enclave ]] || KEYB64="$(python3 - "$W/client" <<'PY'
import base64, struct, sys
raw = open(sys.argv[1]).read().split('-----')[2]
b = base64.b64decode(''.join(raw.split()))
assert b.startswith(b'openssh-key-v1\0')
i = len(b'openssh-key-v1\0')
def s(b, i):
    n = struct.unpack('>I', b[i:i + 4])[0]
    return b[i + 4:i + 4 + n], i + 4 + n
_, i = s(b, i); _, i = s(b, i); _, i = s(b, i); i += 4; _, i = s(b, i); priv, i = s(b, i)
j = 8; kt, j = s(priv, j)
if kt == b'ssh-ed25519':
    _, j = s(priv, j); sk, j = s(priv, j)
    out = sk[:32]
else:
    _, j = s(priv, j); _, j = s(priv, j); d, j = s(priv, j)
    out = d.lstrip(b'\0').rjust(32, b'\0')
print(base64.b64encode(out).decode())
PY
)"
SEED=""; P256=""
case "$KEYTYPE" in ed25519) SEED="$KEYB64" ;; ecdsa) P256="$KEYB64" ;; esac
ENCLAVE=0; [[ "$KEYTYPE" == enclave ]] && ENCLAVE=1
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
# Pushes (decisions #175, #177), against Apple's REAL sandbox: the UI test backgrounds the app,
# a notification is raised, and the banner must show its DECRYPTED text (the relay and APNs only
# carry a placeholder) before the test taps it.
#   E2E_PUSH_RELAY=<url>           through a push relay (https://aide-push.marshallku.dev, or a
#                                  local `aide push-relay` on http://127.0.0.1:<port>)
#   E2E_APNS_KEY / _KEY_ID / _TEAM straight to APNs with the team's key
PUSH=0
if [[ -n "${E2E_PUSH_RELAY:-}" ]]; then
    PUSH=1
    printf '[push]\nrelay = "%s"\n' "$E2E_PUSH_RELAY" >"$W/cfg/copad/mux.toml"
elif [[ -n "${E2E_APNS_KEY:-}" && -n "${E2E_APNS_KEY_ID:-}" && -n "${E2E_APNS_TEAM:-}" ]]; then
    PUSH=1
    printf '[push]\napns_key = "%s"\napns_key_id = "%s"\napns_team_id = "%s"\n' \
        "$E2E_APNS_KEY" "$E2E_APNS_KEY_ID" "$E2E_APNS_TEAM" >"$W/cfg/copad/mux.toml"
else
    printf '[push]\nrelay = "off"\n' >"$W/cfg/copad/mux.toml"
fi
mux() { env -u COPAD_MUX_PANE -u COPAD_MUX "${MUXENV[@]}" "$COMUX" "$@"; }
# The server's HOME is a fixture: the native Ctrl-b C browser lists it (decisions #172).
mkdir -p "$W/home/proj/sub"
HOME="$W/home" mux server >"$W/server.log" 2>&1 &
for _ in $(seq 1 50); do mux list-sessions >/dev/null 2>&1 && break; sleep 0.1; done
mux new-session --detached e2e-two >/dev/null
# A fake agent: `claude` is classified by argv[0]'s basename — a SYMLINK, run by absolute path
# (a copy of a system binary is killed by macOS on exec).
ln -s /bin/sleep "$W/bin/claude"
tok() { mux list-tree --json | python3 -c 'import json,sys; t=json.load(sys.stdin)["tree"]; print(t[int(sys.argv[1])]["tabs"][0]["panes"][0]["token"])' "$1"; }
TOK="$(tok 0)"
# The push step: once the app has registered and gone to the background it drops
# `notify-now`; the notification is raised for e2e-two's pane, so tapping it must switch there.
if [[ $PUSH == 1 ]]; then
    TOK2="$(tok 1)"
    ( for _ in $(seq 1 1200); do
          if [[ -f "$W/notify-now" ]]; then mux notify --pane "$TOK2" --kind done "push e2e" >/dev/null; break; fi
          sleep 0.25
      done ) &
fi
# In the OTHER session, so the pane the test types into keeps its focus.
mux split --from "$(tok 1)" -- "$W/bin/claude" 1000 >/dev/null
mux notify --pane "$TOK" --kind done "e2e notification" >/dev/null
# What the app runs per SSH channel: this build of comux on this isolated socket.
REMOTE="env COPAD_MUX_SOCK=$W/s COPAD_MUX_STATE=$W/state.json COPAD_MUX_NOTIFY=0 COPAD_MUX_USAGE=0 COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1 XDG_CONFIG_HOME=$W/cfg $COMUX proxy --stdio"

# --- the app ------------------------------------------------------------------------------
xcodegen generate >/dev/null
DEST='platform=iOS Simulator,name=iPhone 17'
TEST=NativeE2ETests/testNativeSessionEndToEnd
if [[ "${E2E_KOREAN:-}" == 1 ]]; then
    # Its own device: the keyboard list is system-wide, and a shared simulator may have other
    # apps (and their tests) coming to the front.
    SIM="$(xcrun simctl create copad-e2e-korean "iPhone 17")"
    xcrun simctl bootstatus "$SIM" -b >/dev/null
    # A new device fills in its default keyboards a while after booting, over anything set
    # before: wait for them, then replace them.
    for _ in $(seq 1 120); do
        xcrun simctl spawn "$SIM" defaults read -g AppleKeyboards 2>/dev/null | grep -q en_US && break
        sleep 0.5
    done
    xcrun simctl spawn "$SIM" defaults write -g AppleKeyboards -array "ko_KR@sw=Korean;hw=Automatic"
    DEST="platform=iOS Simulator,id=$SIM"
    TEST=NativeE2ETests/testKoreanKeyboardTypesAndDeletes
fi
log "running the UI test"
set +e
TEST_RUNNER_E2E_HOST=127.0.0.1 TEST_RUNNER_E2E_PORT="$PORT" TEST_RUNNER_E2E_USER="$USER" \
TEST_RUNNER_E2E_SEED="$SEED" TEST_RUNNER_E2E_P256="$P256" TEST_RUNNER_E2E_ENCLAVE="$ENCLAVE" TEST_RUNNER_E2E_AUTHKEYS="$W/authorized_keys" TEST_RUNNER_E2E_HOSTKEY="$(cut -d' ' -f1,2 "$W/hostkey.pub")" \
TEST_RUNNER_E2E_COMMAND="$REMOTE" TEST_RUNNER_E2E_KOREAN="${E2E_KOREAN:-}" TEST_RUNNER_E2E_PUSH_DIR="$([[ $PUSH == 1 ]] && echo "$W")" \
xcodebuild -project copad-ios.xcodeproj -scheme copad-ios -destination "$DEST" \
    -derivedDataPath build -skipPackagePluginValidation \
    -resultBundlePath "$W/e2e.xcresult" \
    test -only-testing:"copad-iosUITests/$TEST" >"$W/xcodebuild.log" 2>&1
rc=$?
set -e
grep -E "Test Case .*(passed|failed)|error:|XCTAssert" "$W/xcodebuild.log" | head -20 >&2

# --- what the server saw ------------------------------------------------------------------
# The bytes really reached the PTY (not just the app's own mirror), in the pane they were typed.
CAP="$(mux capture-pane "$TOK" -S 200 2>/dev/null || true)"
if [[ "${E2E_KOREAN:-}" == 1 ]]; then
    # Only the line typed after Enter: 한 ran once as a command, and nothing else did.
    [[ "$(grep -c "command not found: 한" <<<"$CAP")" == 1 ]] || { log "FAIL: the pane did not run 한 exactly once"; rc=1; }
    grep -q "% 가" <<<"$CAP" || { log "FAIL: the line after Enter never reached the pane"; rc=1; }
else
grep -q "NATIVE-42" <<<"$CAP" || { log "FAIL: NATIVE-42 never reached the pane"; rc=1; }
grep -q "한글-확인" <<<"$CAP" || { log "FAIL: the Hangul line never reached the pane"; rc=1; }
grep -q "BACK-2" <<<"$CAP" || { log "FAIL: input after resume never reached the pane"; rc=1; }
fi
# The registration carries the app's key, and the push went out sealed.
if [[ $PUSH == 1 ]]; then
    grep -q '"key"' "$W/comux-push.json" 2>/dev/null || { log "FAIL: the app registered without a push key"; rc=1; }
    grep -q "comux push:" "$W/server.log" && { log "FAIL: the push worker reported an error:"; grep "comux push:" "$W/server.log" >&2; rc=1; }
fi
# The app authenticated with the key type under test.
if [[ "$KEYTYPE" == ed25519 ]]; then want=ED25519; else want=ECDSA; fi
grep -q "Accepted publickey for .* $want " "$W/sshd.log" \
    || { log "FAIL: sshd never accepted a $want key from the app"; rc=1; }
mkdir -p "$W/shots"
xcrun xcresulttool export attachments --path "$W/e2e.xcresult" --output-path "$W/shots" >/dev/null 2>&1 || true
[[ $rc == 0 ]] && log "PASS: native comux client end to end" || log "FAIL (xcodebuild log: $W/xcodebuild.log)"
exit $rc
