#!/usr/bin/env bash
# e2e for browser observability (macOS):
#   1. `fetch` and `XMLHttpRequest` a page makes are captured, readable via `coctl webview net`.
#   2. `console.*` and uncaught errors are captured, readable via `coctl webview console`,
#      and filterable by level.
#   3. Navigations are captured NATIVELY (the user script cannot see them); otherwise a page
#      merely navigated to would look like it made no requests.
#   4. Every read declares its coverage, so an empty list is never read as "nothing happened".
#   5. Credential-shaped content is redacted before it reaches the file.
#   6. A BACKGROUND tab's activity is captured and scoped by `--tab-id`.
#   7. Reading the log takes neither the frontmost app nor the cursor.
#
# Pages come from a local throwaway HTTP server so the run is offline and the page's request
# is one we control.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
APP="$REPO/copad-macos/.build/debug/Copad.app/Contents/MacOS/Copad"
COCTL="$REPO/target/release/coctl"; [ -x "$COCTL" ] || COCTL="$REPO/target/debug/coctl"
[ -x "$APP" ] || { echo "build the bundle first"; exit 2; }

RC=0
cleanup() {
    [ -n "${APP_PID:-}" ] && kill -9 "$APP_PID" 2>/dev/null
    [ -n "${SOCK:-}" ] && rm -f "$SOCK"
    [ -n "${SERVER_PID:-}" ] && kill -9 "$SERVER_PID" 2>/dev/null
    [ -n "${WWW:-}" ] && rm -rf "$WWW"
    [ -n "${TMPHOME:-}" ] && rm -rf "$TMPHOME"
}
# EXIT only: with `trap cleanup EXIT INT TERM` bash runs the handler and then RESUMES the
# script, deleting the temp $HOME the next command reads. INT/TERM just `exit`, which fires
# the EXIT trap once.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

WWW=$(mktemp -d /private/tmp/copad-www-XXXXXX)
cat > "$WWW/noisy.html" <<'HTML'
<!doctype html><title>noisy</title><body>
<script>
console.log('hello from the page');
console.error('something went wrong');
fetch('/data.json').then(r => r.json()).then(() => console.info('fetch done'));
const x = new XMLHttpRequest();
x.open('GET', '/xhr.json');
x.send();
// A credential-shaped console line: the redactor must catch it before it
// reaches a file the agent can read.
console.log('{"password":"hunter2"}');
setTimeout(() => { throw new Error('uncaught boom'); }, 100);
</script>
</body>
HTML
echo '{"ok":true}' > "$WWW/data.json"
echo '{"ok":true}' > "$WWW/xhr.json"
cat > "$WWW/quiet.html" <<'HTML'
<!doctype html><title>quiet</title><body><script>
fetch('/data.json').then(() => console.log('background tab fetched'));
</script></body>
HTML
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
(cd "$WWW" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
SERVER_PID=$!
BASE="http://127.0.0.1:$PORT"
for _ in $(seq 1 40); do curl -sf "$BASE/noisy.html" >/dev/null 2>&1 && break; sleep 0.25; done
curl -sf "$BASE/noisy.html" >/dev/null 2>&1 || { echo "FAIL: local server did not start"; exit 1; }

rpc() {
    python3 - "$SOCK" "$1" "${2:-{\}}" <<'PY'
import json, socket, sys
sock, method, params = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(20)
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
mouse() {
    local v
    v=$(osascript -l JavaScript -e 'ObjC.import("Cocoa"); var p=$.NSEvent.mouseLocation; Math.round(p.x)+","+Math.round(p.y)' 2>/dev/null)
    [[ "$v" =~ ^-?[0-9]+,-?[0-9]+$ ]] || return 1
    echo "$v"
}
front() {
    local v
    v=$(osascript -e 'tell application "System Events" to name of first application process whose frontmost is true' 2>/dev/null)
    [ -n "$v" ] || return 1
    echo "$v"
}

TMPHOME=$(mktemp -d /private/tmp/copad-e2e-XXXXXX)
mkdir -p "$TMPHOME/Library/Application Support/copad"
HOME="$TMPHOME" "$APP" >"$TMPHOME/app.log" 2>&1 &
APP_PID=$!
SOCK="/tmp/copad-$APP_PID.sock"
for _ in $(seq 1 40); do [ -S "$SOCK" ] && break; sleep 0.25; done
[ -S "$SOCK" ] || { echo "FAIL: no gui socket"; tail -5 "$TMPHOME/app.log"; exit 1; }
sleep 1
export COPAD_SOCKET="$SOCK"

BASE_FRONT=$(front) || RC=1
BASE_MOUSE=$(mouse) || RC=1

COPAD_SOCKET="$SOCK" "$COCTL" webview open "$BASE/noisy.html" --mode tab >/dev/null 2>&1
sleep 4

NET=$(COPAD_SOCKET="$SOCK" "$COCTL" webview net --json 2>&1)
CONSOLE=$(COPAD_SOCKET="$SOCK" "$COCTL" webview console --json 2>&1)

has() { python3 -c "
import json,sys
try: d=json.loads(sys.argv[1])
except Exception: print('0'); raise SystemExit
recs=(d.get('result') or d).get('records',[])
print(sum(1 for r in recs if sys.argv[2] in json.dumps(r)))" "$1" "$2"; }

echo "[1/7] fetch and XHR are captured"
F=$(has "$NET" "/data.json"); X=$(has "$NET" "/xhr.json")
if [ "$F" -ge 1 ] && [ "$X" -ge 1 ]; then
    echo "  PASS: fetch=$F xhr=$X records"
else
    echo "  FAIL: fetch=$F xhr=$X — $NET"; RC=1
fi

echo "[2/7] console output and uncaught errors are captured"
L=$(has "$CONSOLE" "hello from the page")
E=$(has "$CONSOLE" "something went wrong")
U=$(has "$CONSOLE" "uncaught boom")
if [ "$L" -ge 1 ] && [ "$E" -ge 1 ] && [ "$U" -ge 1 ]; then
    echo "  PASS: log=$L error=$E uncaught=$U"
else
    echo "  FAIL: log=$L error=$E uncaught=$U — $CONSOLE"; RC=1
fi

echo "[3/7] a navigation is captured natively"
N=$(python3 -c "
import json,sys
d=json.loads(sys.argv[1]); recs=(d.get('result') or d).get('records',[])
print(sum(1 for r in recs if r.get('source')=='navigation'))" "$NET")
if [ "$N" -ge 1 ]; then
    echo "  PASS: $N navigation record(s) — the user script cannot see these"
else
    echo "  FAIL: no navigation record; a merely-navigated page would look silent"; RC=1
fi

echo "[4/7] every read declares its coverage"
COV=$(python3 -c "
import json,sys
d=json.loads(sys.argv[1]); print((d.get('result') or d).get('coverage',''))" "$NET")
if [ "$COV" = "js+navigation" ]; then
    echo "  PASS: coverage=$COV"
else
    echo "  FAIL: coverage='$COV'"; RC=1
fi

echo "[5/7] credential-shaped console content is redacted"
if grep -q "hunter2" <<<"$CONSOLE"; then
    echo "  FAIL: 'hunter2' reached the agent-readable log"; RC=1
else
    R=$(has "$CONSOLE" "redacted")
    echo "  PASS: the password is absent (redaction markers: $R)"
fi

echo "[6/7] a background tab's activity is captured and scoped by tab_id"
TAB2=$(python3 -c "
import json,sys
print(json.loads(sys.argv[1])['result']['tab_id'])" "$(rpc webview.tab.new "{\"url\":\"$BASE/quiet.html\"}")")
sleep 3
SCOPED=$(COPAD_SOCKET="$SOCK" "$COCTL" webview console --tab-id "$TAB2" --json 2>&1)
B=$(has "$SCOPED" "background tab fetched")
OTHER=$(has "$SCOPED" "hello from the page")
if [ "$B" -ge 1 ] && [ "$OTHER" -eq 0 ]; then
    echo "  PASS: the background tab logged, and --tab-id excluded the other tab"
else
    echo "  FAIL: background=$B leaked-from-other-tab=$OTHER"; RC=1
fi

echo "[7/7] reading the log took neither focus nor the cursor"
NOW_FRONT=$(front) || RC=1
NOW_MOUSE=$(mouse) || RC=1
if [ "$BASE_FRONT" = "$NOW_FRONT" ] && [ "$BASE_MOUSE" = "$NOW_MOUSE" ]; then
    echo "  PASS: frontmost=$NOW_FRONT cursor=$NOW_MOUSE unchanged"
else
    echo "  FAIL: frontmost $BASE_FRONT->$NOW_FRONT cursor $BASE_MOUSE->$NOW_MOUSE"; RC=1
fi

[ $RC -eq 0 ] && echo "ALL PASS" || echo "FAILURES (rc=$RC)"
exit $RC
