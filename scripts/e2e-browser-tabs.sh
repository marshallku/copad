#!/usr/bin/env bash
# e2e for the browser tab strip (macOS):
#   1. `webview.tab.new` opens a tab in the BACKGROUND by default; an agent must never yank
#      the visible page from under the user.
#   2. A background tab can be screenshotted without raising or focusing it (`takeSnapshot`
#      renders a web view that is in no window).
#   3. Every page-level method takes a `tab_id`, so a background tab is driven unselected.
#   4. list / select / move / close behave, and closing the last tab is refused.
#   5. None of the above changes the frontmost app or the cursor.
#   6. Multiple tabs survive a restart, each keeping its own stable id.
#
# Pages come from a local throwaway HTTP server (distinguishable tabs for the screenshot
# check, no network). Isolated by $HOME so it cannot touch the user's session.
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
# Solid, distinct colours so a screenshot can be told apart from a blank one
# AND from the other tab's.
cat > "$WWW/red.html" <<'HTML'
<!doctype html><title>RED</title><body style="margin:0;background:#ff0000;height:100vh"></body>
HTML
cat > "$WWW/blue.html" <<'HTML'
<!doctype html><title>BLUE</title><body style="margin:0;background:#0000ff;height:100vh"></body>
HTML
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
(cd "$WWW" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
SERVER_PID=$!
BASE="http://127.0.0.1:$PORT"
for _ in $(seq 1 40); do curl -sf "$BASE/red.html" >/dev/null 2>&1 && break; sleep 0.25; done
curl -sf "$BASE/red.html" >/dev/null 2>&1 || { echo "FAIL: local server did not start"; exit 1; }

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
jq_() { python3 -c "
import json,sys
try: d=json.loads(sys.argv[1])
except Exception: print(''); raise SystemExit
cur=d
for k in sys.argv[2].split('.'):
    if k.isdigit(): cur=cur[int(k)] if isinstance(cur,list) and len(cur)>int(k) else ''
    else: cur=cur.get(k,'') if isinstance(cur,dict) else ''
print(cur if not isinstance(cur,(dict,list)) else json.dumps(cur))" "$1" "$2"; }
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

BASE_FRONT=$(front) || { echo "FAIL: cannot read frontmost app"; RC=1; }
BASE_MOUSE=$(mouse) || { echo "FAIL: cannot read cursor"; RC=1; }

echo "[1/6] a new tab opens in the background by default"
COPAD_SOCKET="$SOCK" "$COCTL" webview open "$BASE/red.html" --mode tab >/dev/null 2>&1
sleep 2
NEW=$(rpc webview.tab.new "{\"url\":\"$BASE/blue.html\"}")
TAB2=$(jq_ "$NEW" result.tab_id)
sleep 2
LIST=$(rpc webview.tab.list)
COUNT=$(python3 -c "import json,sys; print(len(json.loads(sys.argv[1])['result']['tabs']))" "$LIST")
ACTIVE=$(jq_ "$LIST" result.active)
if [ "$COUNT" = "2" ] && [ "$ACTIVE" = "0" ]; then
    echo "  PASS: 2 tabs, active still 0 — the new tab did not steal the view"
else
    echo "  FAIL: count=$COUNT active=$ACTIVE  ($LIST)"; RC=1
fi

echo "[2/6] a BACKGROUND tab screenshots without being raised"
SHOT=$(rpc webview.screenshot "{\"tab_id\":\"$TAB2\"}")
B64=$(jq_ "$SHOT" result.image_b64)
VERDICT=$(python3 - "$B64" <<'PY'
import base64, sys, struct, zlib
raw = sys.argv[1]
if not raw:
    print("no image"); raise SystemExit
data = base64.b64decode(raw)
pos, w, h, idat, bitdepth, ctype = 8, 0, 0, b"", 0, 0
while pos < len(data):
    ln = struct.unpack(">I", data[pos:pos+4])[0]
    typ = data[pos+4:pos+8]
    body = data[pos+8:pos+8+ln]
    if typ == b"IHDR":
        w, h, bitdepth, ctype = struct.unpack(">IIBB", body[:10])
    elif typ == b"IDAT":
        idat += body
    pos += 12 + ln
if ctype != 6 or bitdepth != 8:
    print(f"unsupported png ctype={ctype} depth={bitdepth}"); raise SystemExit
buf = zlib.decompress(idat)
stride = w * 4
prev = bytearray(stride)
rows = []
p = 0
for _ in range(h):
    ft = buf[p]; p += 1
    line = bytearray(buf[p:p+stride]); p += stride
    for i in range(stride):
        a = line[i-4] if i >= 4 else 0
        b = prev[i]
        c = prev[i-4] if i >= 4 else 0
        if ft == 1: line[i] = (line[i] + a) & 255
        elif ft == 2: line[i] = (line[i] + b) & 255
        elif ft == 3: line[i] = (line[i] + (a + b) // 2) & 255
        elif ft == 4:
            pp = a + b - c
            pa, pb, pc = abs(pp-a), abs(pp-b), abs(pp-c)
            pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
            line[i] = (line[i] + pr) & 255
    rows.append(bytes(line)); prev = line
y = h // 2; x = w // 2
r, g, bch = rows[y][x*4], rows[y][x*4+1], rows[y][x*4+2]
print(f"{w}x{h} rgb=({r},{g},{bch}) " + ("BLUE" if bch > 200 and r < 80 else ("RED" if r > 200 and bch < 80 else "OTHER")))
PY
)
echo "  $VERDICT"
case "$VERDICT" in
    *BLUE*) echo "  PASS: the background tab rendered its own page" ;;
    *) echo "  FAIL: expected the background (blue) tab's pixels"; RC=1 ;;
esac

echo "[3/6] a background tab is drivable by tab_id without selecting it"
TITLE=$(rpc webview.execute_js "{\"tab_id\":\"$TAB2\",\"code\":\"document.title\"}")
GOT=$(jq_ "$TITLE" result.result)
ACTIVE_AFTER=$(jq_ "$(rpc webview.tab.list)" result.active)
if [ "$GOT" = "BLUE" ] && [ "$ACTIVE_AFTER" = "0" ]; then
    echo "  PASS: ran JS in the background tab ('$GOT') and it stayed in the background"
else
    echo "  FAIL: title='$GOT' active=$ACTIVE_AFTER"; RC=1
fi

echo "[4/6] select / move / close, and the last tab is refused"
SEL=$(rpc webview.tab.select "{\"tab_id\":\"$TAB2\"}")
[ "$(jq_ "$SEL" result.active)" = "1" ] || { echo "  FAIL: select -> $SEL"; RC=1; }
MOV=$(rpc webview.tab.move "{\"tab_id\":\"$TAB2\",\"index\":0}")
LIST=$(rpc webview.tab.list)
FIRST=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['result']['tabs'][0]['id'])" "$LIST")
STILL=$(jq_ "$LIST" result.active)
if [ "$FIRST" = "$TAB2" ] && [ "$STILL" = "0" ]; then
    echo "  PASS: moved to index 0 and the SAME tab stayed selected"
else
    echo "  FAIL: first=$FIRST active=$STILL (move -> $MOV)"; RC=1
fi
CLOSE=$(rpc webview.tab.close "{\"tab_id\":\"$TAB2\"}")
[ "$(jq_ "$CLOSE" result.remaining)" = "1" ] || { echo "  FAIL: close -> $CLOSE"; RC=1; }
LAST_ID=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['result']['tabs'][0]['id'])" "$(rpc webview.tab.list)")
REFUSE=$(rpc webview.tab.close "{\"tab_id\":\"$LAST_ID\"}")
if grep -q '"refused"' <<<"$REFUSE"; then
    echo "  PASS: closing the pane's last tab is refused"
else
    echo "  FAIL: last tab close -> $REFUSE"; RC=1
fi

echo "[5/6] the coctl surface reaches the same tabs, then focus/cursor check"
COCTL_LIST=$(COPAD_SOCKET="$SOCK" "$COCTL" webview tab list --json 2>&1)
if grep -q '"tabs"' <<<"$COCTL_LIST"; then
    echo "  PASS: coctl webview tab list works without a panel id"
else
    echo "  FAIL: coctl webview tab list -> $COCTL_LIST"; RC=1
fi
COCTL_NEW=$(COPAD_SOCKET="$SOCK" "$COCTL" webview tab new "$BASE/red.html" --json 2>&1)
sleep 1
AFTER_NEW=$(jq_ "$(rpc webview.tab.list)" result.active)
if grep -q '"tab_id"' <<<"$COCTL_NEW" && [ "$AFTER_NEW" = "0" ]; then
    echo "  PASS: coctl-opened tab stayed in the background"
else
    echo "  FAIL: coctl tab new -> $COCTL_NEW (active=$AFTER_NEW)"; RC=1
fi

NOW_FRONT=$(front) || RC=1
NOW_MOUSE=$(mouse) || RC=1
if [ "$BASE_FRONT" = "$NOW_FRONT" ] && [ "$BASE_MOUSE" = "$NOW_MOUSE" ]; then
    echo "  PASS: frontmost=$NOW_FRONT cursor=$NOW_MOUSE unchanged"
else
    echo "  FAIL: frontmost $BASE_FRONT->$NOW_FRONT cursor $BASE_MOUSE->$NOW_MOUSE"; RC=1
fi

echo "[6/6] multiple tabs survive a restart with their own ids"
rpc webview.tab.new "{\"url\":\"$BASE/blue.html\"}" >/dev/null
sleep 2
BEFORE=$(python3 -c "
import json,sys; d=json.loads(sys.argv[1])['result']
print(','.join(t['id'] for t in d['tabs']))" "$(rpc webview.tab.list)")
kill -TERM "$APP_PID" 2>/dev/null; sleep 1; kill -9 "$APP_PID" 2>/dev/null; rm -f "$SOCK"
HOME="$TMPHOME" "$APP" >>"$TMPHOME/app.log" 2>&1 &
APP_PID=$!
SOCK="/tmp/copad-$APP_PID.sock"
for _ in $(seq 1 40); do [ -S "$SOCK" ] && break; sleep 0.25; done
sleep 3
AFTER=$(python3 -c "
import json,sys
try: d=json.loads(sys.argv[1])['result']
except Exception: print(''); raise SystemExit
print(','.join(t['id'] for t in d['tabs']))" "$(rpc webview.tab.list)")
if [ -n "$BEFORE" ] && [ "$BEFORE" = "$AFTER" ]; then
    echo "  PASS: $BEFORE survived the restart intact"
else
    echo "  FAIL: before='$BEFORE' after='$AFTER'"; RC=1
fi

[ $RC -eq 0 ] && echo "ALL PASS" || echo "FAILURES (rc=$RC)"
exit $RC
