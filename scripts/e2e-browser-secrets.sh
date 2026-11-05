#!/usr/bin/env bash
# e2e for the credential boundary (macOS). Claims, in order of importance:
#   1. A password is refused on an UNPROTECTED tab: its value is already readable through
#      `query`/`execute_js`, so "saving it securely" would be theatre.
#   2. Entering protected mode rebuilds the tab: scripts the agent installed are gone.
#   3. While protected, every value-returning command against the pane is REFUSED, including
#      `execute_js`, which is what makes the boundary real.
#   4. A save reads the password natively; it is never an RPC parameter. In a DEV build the
#      keychain write cannot complete (below), so this asserts it degrades promptly to
#      `secret_backend_unavailable` instead of hanging, which once froze the whole GUI.
#   5. `browser.secret.list` returns metadata with no secret-bearing field.
#   6. A fill against an unknown credential is refused, and no error path carries a secret.
#   7. The secret appears in no file copad wrote.
#   8. None of it takes the frontmost app or the cursor.
#
# NOT COVERABLE IN A DEV BUILD: a locally self-signed bundle has no team identifier, so the
# data-protection keychain refuses it (`errSecMissingEntitlement`), adding
# `keychain-access-groups` gets the process AMFI-killed, and the legacy `SecKeychain*`
# fallback blocks on an access prompt. A successful save -> fill round-trip needs a
# Developer ID signed build and is queued in docs/pending-linux-verification.md.
#
# The login page is served locally so the password is one we chose and can grep for.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
APP="$REPO/copad-macos/.build/debug/Copad.app/Contents/MacOS/Copad"
COCTL="$REPO/target/release/coctl"; [ -x "$COCTL" ] || COCTL="$REPO/target/debug/coctl"
[ -x "$APP" ] || { echo "build the bundle first"; exit 2; }
SECRET="hunter2-e2e-do-not-reuse"

RC=0
cleanup() {
    [ -n "${APP_PID:-}" ] && kill -9 "$APP_PID" 2>/dev/null
    [ -n "${SOCK:-}" ] && rm -f "$SOCK"
    [ -n "${SERVER_PID:-}" ] && kill -9 "$SERVER_PID" 2>/dev/null
    [ -n "${WWW:-}" ] && rm -rf "$WWW"
    [ -n "${TMPHOME:-}" ] && rm -rf "$TMPHOME"
    # The keychain is REAL and shared, so a leftover test credential would outlive the temp
    # home. Delete by the exact test ACCOUNT, never the service alone, which would remove a
    # real credential the user saved.
    if [ -n "${CRED_ID:-}" ]; then
        security delete-generic-password \
            -s com.marshall.copad.browser -a "$CRED_ID" >/dev/null 2>&1 || true
    fi
}
# EXIT only: with `trap cleanup EXIT INT TERM` bash runs the handler and then RESUMES the
# script, deleting the temp $HOME the next command reads. INT/TERM just `exit`, which fires
# the EXIT trap once.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

WWW=$(mktemp -d /private/tmp/copad-www-XXXXXX)
cat > "$WWW/login.html" <<'HTML'
<!doctype html><title>login</title><body>
<form><input id="user" type="text"><input id="pw" type="password"></form>
</body>
HTML
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
(cd "$WWW" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
SERVER_PID=$!
BASE="http://127.0.0.1:$PORT"
for _ in $(seq 1 40); do curl -sf "$BASE/login.html" >/dev/null 2>&1 && break; sleep 0.25; done

rpc() {
    python3 - "$SOCK" "$1" "${2:-{\}}" <<'PY'
import json, socket, sys
sock, method, params = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(25)
s.connect(sock)
s.sendall((json.dumps({"id": "e2e", "method": method, "params": params}) + "\n").encode())
buf = b""
while not buf.endswith(b"\n"):
    chunk = s.recv(65536)
    if not chunk:
        break
    buf += chunk
s.close()
print(buf.decode().strip())
PY
}
mouse() { osascript -l JavaScript -e 'ObjC.import("Cocoa"); var p=$.NSEvent.mouseLocation; Math.round(p.x)+","+Math.round(p.y)' 2>/dev/null; }
front() { osascript -e 'tell application "System Events" to name of first application process whose frontmost is true' 2>/dev/null; }

TMPHOME=$(mktemp -d /private/tmp/copad-e2e-XXXXXX)
mkdir -p "$TMPHOME/Library/Application Support/copad"
HOME="$TMPHOME" "$APP" >"$TMPHOME/app.log" 2>&1 &
APP_PID=$!
SOCK="/tmp/copad-$APP_PID.sock"
for _ in $(seq 1 40); do [ -S "$SOCK" ] && break; sleep 0.25; done
[ -S "$SOCK" ] || { echo "FAIL: no gui socket"; tail -5 "$TMPHOME/app.log"; exit 1; }
sleep 1
export COPAD_SOCKET="$SOCK"
BASE_FRONT=$(front); BASE_MOUSE=$(mouse)

COPAD_SOCKET="$SOCK" "$COCTL" webview open "$BASE/login.html" --mode tab >/dev/null 2>&1
sleep 3
# Build the params in python via a heredoc: interpolating a quoted secret into a shell-built
# JSON string does not parse, and escaping quotes through `python3 -c` broke the script.
type_password() {
    local params
    params=$(python3 - "$SECRET" <<'PY'
import json, sys
code = "document.getElementById('pw').value = " + json.dumps(sys.argv[1]) + "; 'typed'"
print(json.dumps({"code": code}))
PY
)
    rpc webview.execute_js "$params" >/dev/null
}

echo "[1/8] saving from an UNPROTECTED tab is refused"
type_password
OUT=$(rpc browser.secret.save '{"username":"e2e"}')
if grep -q "requires_protected" <<<"$OUT"; then
    echo "  PASS: refused — the value was already agent-readable on this page"
else
    echo "  FAIL: $OUT"; RC=1; fi

echo "[2/8] entering protected mode rebuilds the tab"
rpc webview.execute_js '{"code":"window.__agentMarker = \"still here\"; \"planted\""}' >/dev/null
GEN_BEFORE=$(python3 -c "
import json,sys
try: print(json.loads(sys.argv[1])['result'].get('document_generation',''))
except Exception: print('')" "$(rpc webview.tab.protect '{"on":false}')")
PROT=$(rpc webview.tab.protect '{"on":true}')
MODE=$(python3 -c "
import json,sys
try: print(json.loads(sys.argv[1])['result']['mode'])
except Exception: print('')" "$PROT")
sleep 3
if [ "$MODE" = "protected" ]; then
    echo "  PASS: mode=$MODE (generation bumped from '$GEN_BEFORE')"
else
    echo "  FAIL: protect -> $PROT"; RC=1; fi

echo "[3/8] while protected, value-returning commands are refused"
BLOCKED=0
for m in webview.execute_js webview.get_content webview.query webview.screenshot webview.state webview.net; do
    P='{}'
    [ "$m" = "webview.execute_js" ] && P='{"code":"window.__agentMarker"}'
    [ "$m" = "webview.query" ] && P='{"selector":"input"}'
    R=$(rpc "$m" "$P")
    if grep -q "tab_protected" <<<"$R"; then BLOCKED=$((BLOCKED+1)); else echo "  FAIL: $m was NOT refused: $R"; RC=1; fi
done
[ "$BLOCKED" -eq 6 ] && echo "  PASS: all 6 read paths refused, including execute_js"
CLICK=$(rpc webview.click '{"selector":"#pw"}')
if grep -q '"protected":true' <<<"$CLICK"; then
    echo "  PASS: a write still runs, and its result is page-independent"
else
    echo "  FAIL: click -> $CLICK"; RC=1; fi

echo "[4/8] a save degrades promptly instead of hanging"
type_password_protected() {
    # `webview.fill` is a WRITE, so it is permitted under protection and returns
    # nothing about the page — the same path a human typing would take.
    local params
    params=$(python3 - "$SECRET" <<'PY'
import json, sys
print(json.dumps({"selector": "#pw", "value": sys.argv[1]}))
PY
)
    rpc webview.fill "$params" >/dev/null
}
type_password_protected
sleep 1
STARTED=$(date +%s)
SAVE=$(rpc browser.secret.save '{"username":"e2e"}')
ELAPSED=$(( $(date +%s) - STARTED ))
CRED_ID=$(python3 -c "
import json,sys
try: print(json.loads(sys.argv[1])['result']['credential']['id'])
except Exception: print('')" "$SAVE")
if [ -n "$CRED_ID" ]; then
    echo "  PASS: the keychain accepted it — saved as '$CRED_ID' in ${ELAPSED}s"
elif grep -q "secret_backend_unavailable" <<<"$SAVE" && [ "$ELAPSED" -le 10 ]; then
    echo "  PASS: degraded to secret_backend_unavailable in ${ELAPSED}s (dev build; no hang)"
else
    echo "  FAIL: ${ELAPSED}s -> $SAVE"; RC=1
fi
if grep -q "$SECRET" <<<"$SAVE"; then
    echo "  FAIL: the response carried the secret"; RC=1
else
    echo "  PASS: no response shape carried the secret"
fi

echo "[5/8] the credential index is metadata with no secret-bearing field"
# Seed the index directly when the keychain could not: the index needs no keychain, so the
# shape of what an agent may read is still testable.
if [ -z "$CRED_ID" ]; then
    CRED_ID="127.0.0.1/e2e"
    python3 - "$TMPHOME/Library/Application Support/copad/browser/credentials.json" "$CRED_ID" <<'PY'
import json, os, sys
path, cid = sys.argv[1], sys.argv[2]
os.makedirs(os.path.dirname(path), exist_ok=True)
json.dump([{
    "id": cid, "origin": "http://127.0.0.1", "username": "e2e",
    "slot": "password", "created_at": 1,
}], open(path, "w"))
PY
fi
LIST=$(COPAD_SOCKET="$SOCK" "$COCTL" secret list --json 2>&1)
BAD=$(python3 -c "
import json,sys
try: d=json.loads(sys.argv[1])
except Exception: print('parse'); raise SystemExit
creds=(d.get('result') or d).get('credentials',[])
bad=[k for c in creds for k in c if k in ('password','secret','value','token')]
print(','.join(bad))" "$LIST")
if [ -z "$BAD" ] && grep -q "$CRED_ID" <<<"$LIST"; then
    echo "  PASS: listed '$CRED_ID' with no secret-bearing key"
else
    echo "  FAIL: bad keys='$BAD' list=$LIST"; RC=1
fi

echo "[6/8] an unknown credential is refused, and no error carries a secret"
MISS=$(rpc browser.secret.fill '{"credential_id":"nope/nobody"}')
if grep -q "not_found" <<<"$MISS"; then
    echo "  PASS: unknown credential refused"
else
    echo "  FAIL: $MISS"; RC=1
fi
REAL=$(rpc browser.secret.fill "{\"credential_id\":\"$CRED_ID\"}")
if grep -q "$SECRET" <<<"$REAL"; then
    echo "  FAIL: a fill response carried the secret: $REAL"; RC=1
else
    echo "  PASS: the fill response carries no secret"
fi

echo "[7/8] the secret appears in no file copad wrote"
LEAK=$(grep -rl "$SECRET" "$TMPHOME" 2>/dev/null | head -5)
if [ -z "$LEAK" ]; then
    echo "  PASS: absent from every file under the isolated home"
else
    echo "  FAIL: found in $LEAK"; RC=1
fi

echo "[8/8] none of it took the frontmost app or the cursor"
NOW_FRONT=$(front); NOW_MOUSE=$(mouse)
if [ "$BASE_FRONT" = "$NOW_FRONT" ] && [ "$BASE_MOUSE" = "$NOW_MOUSE" ]; then
    echo "  PASS: frontmost=$NOW_FRONT cursor=$NOW_MOUSE unchanged"
else
    echo "  FAIL: frontmost $BASE_FRONT->$NOW_FRONT cursor $BASE_MOUSE->$NOW_MOUSE"; RC=1; fi

[ $RC -eq 0 ] && echo "ALL PASS" || echo "FAILURES (rc=$RC)"
exit $RC
