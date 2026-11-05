#!/usr/bin/env bash
# e2e for the no-focus-theft guarantee (macOS): agent-driven clicking and capture must not
# take the mouse or keyboard from the human. This is the audit of EVERY browser method in
# one run, against:
#
#   - the frontmost application
#   - the mouse cursor position
#   - copad's own focus state (key window, first responder, focused pane), since a change
#     of the frontmost app misses focus moving WITHIN copad
#   - terminal input arriving complete and in order while the browser is driven hard from
#     another process
#
# The last phase feeds the terminal through copad's own input path instead of synthetic
# keystrokes: those need Accessibility permission, and on this machine `System Events`
# reports success while doing nothing. Phase 2 answers the first-responder question
# directly. A real keystroke run is queued in docs/pending-linux-verification.md.
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
    [ -n "${ORIGINAL_FRONT:-}" ] && osascript -e "tell application \"$ORIGINAL_FRONT\" to activate" >/dev/null 2>&1
}
# EXIT only: with `trap cleanup EXIT INT TERM` bash runs the handler and then RESUMES the
# script, deleting the temp $HOME the next command reads. INT/TERM just `exit`, which fires
# the EXIT trap once.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

WWW=$(mktemp -d /private/tmp/copad-www-XXXXXX)
cat > "$WWW/page.html" <<'HTML'
<!doctype html><title>page</title><body style="margin:0">
<a id="link" href="#x">link</a><input id="in" type="text">
<div style="height:4000px"></div></body>
HTML
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
(cd "$WWW" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
SERVER_PID=$!
BASE="http://127.0.0.1:$PORT"
for _ in $(seq 1 40); do curl -sf "$BASE/page.html" >/dev/null 2>&1 && break; sleep 0.25; done

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
front() { osascript -e 'tell application "System Events" to name of first application process whose frontmost is true' 2>/dev/null; }
mouse() { osascript -l JavaScript -e 'ObjC.import("Cocoa"); var p=$.NSEvent.mouseLocation; Math.round(p.x)+","+Math.round(p.y)' 2>/dev/null; }
field() { python3 -c "
import json,sys
try: print(json.loads(sys.argv[1])['result'][sys.argv[2]])
except Exception: print('')" "$1" "$2"; }

ORIGINAL_FRONT=$(front)
TMPHOME=$(mktemp -d /private/tmp/copad-e2e-XXXXXX)
mkdir -p "$TMPHOME/Library/Application Support/copad"
HOME="$TMPHOME" "$APP" >"$TMPHOME/app.log" 2>&1 &
APP_PID=$!
SOCK="/tmp/copad-$APP_PID.sock"
for _ in $(seq 1 40); do [ -S "$SOCK" ] && break; sleep 0.25; done
[ -S "$SOCK" ] || { echo "FAIL: no gui socket"; tail -5 "$TMPHOME/app.log"; exit 1; }
sleep 1
export COPAD_SOCKET="$SOCK"

COPAD_SOCKET="$SOCK" "$COCTL" webview open "$BASE/page.html" --mode tab >/dev/null 2>&1
sleep 3
TAB2=$(field "$(rpc webview.tab.new "{\"url\":\"$BASE/page.html\"}")" tab_id)
sleep 2
PANEL=$(field "$(rpc webview.tab.list)" panel_id)

# Every method the browser surface exposes. `webview.open` appears twice on purpose: with
# `background: true` (the agent-facing shape) and in its default shape, which switches copad
# tabs and calls `makeFirstResponder`.
run_all() {
    rpc webview.state
    rpc webview.page_info
    rpc webview.query '{"selector":"#link"}'
    rpc webview.query_all '{"selector":"a","limit":5}'
    rpc webview.get_styles '{"selector":"#link","properties":"color,display"}'
    rpc webview.get_content
    rpc webview.execute_js '{"code":"1+1"}'
    rpc webview.click '{"selector":"#in"}'
    rpc webview.fill '{"selector":"#in","value":"typed by the agent"}'
    rpc webview.scroll '{"y":500}'
    rpc webview.screenshot
    rpc webview.screenshot "{\"tab_id\":\"$TAB2\"}"
    rpc webview.navigate "{\"url\":\"$BASE/page.html\"}"
    rpc webview.reload
    rpc webview.back
    rpc webview.forward
    rpc webview.tab.list
    rpc webview.tab.new "{\"url\":\"$BASE/page.html\"}"
    rpc webview.net
    rpc webview.console
    rpc browser.secret.list
    rpc browser.secret.fill '{"credential_id":"nobody/nothing"}'
    rpc webview.tab.protect '{"on":false}'
    rpc webview.devtools '{"action":"toggle"}'
    rpc webview.clear_log
    # select/move/close use a SCRATCH tab, not $TAB2: phase 3 still needs $TAB2.
    local scratch
    scratch=$(field "$(rpc webview.tab.new "{\"url\":\"$BASE/page.html\"}")" tab_id)
    if [ -n "$scratch" ]; then
        rpc webview.tab.select "{\"tab_id\":\"$scratch\"}"
        rpc webview.tab.move "{\"tab_id\":\"$scratch\",\"index\":0}"
        rpc webview.tab.close "{\"tab_id\":\"$scratch\"}"
    fi
    rpc webview.open "{\"url\":\"$BASE/page.html\",\"background\":true}"
    # The DEFAULT open switches copad tabs and moves first responder; it is what a plain
    # `coctl webview open` does.
    rpc webview.open "{\"url\":\"$BASE/page.html\"}"
}

focus_state() {
    python3 -c "
import json,sys
try: r=json.loads(sys.argv[1])['result']
except Exception: print(''); raise SystemExit
print('|'.join(str(r.get(k,'')) for k in ('key_window','first_responder','active_tab','active_pane')))" "$(rpc window.focus_state)"
}

echo "[1/4] every browser method leaves the frontmost app and cursor untouched"
BASE_FRONT=$(front); BASE_MOUSE=$(mouse); BASE_FOCUS=$(focus_state)
[ -n "$BASE_FRONT" ] && [ -n "$BASE_MOUSE" ] && [ -n "$BASE_FOCUS" ] \
    || { echo "  FAIL: could not read the baseline (front='$BASE_FRONT' mouse='$BASE_MOUSE' focus='$BASE_FOCUS')"; RC=1; }
COUNT=$(run_all | grep -c '"id":"e2e"')
NOW_FRONT=$(front); NOW_MOUSE=$(mouse)
if [ "$BASE_FRONT" = "$NOW_FRONT" ] && [ "$BASE_MOUSE" = "$NOW_MOUSE" ]; then
    echo "  PASS: $COUNT commands, frontmost=$NOW_FRONT cursor=$NOW_MOUSE unchanged"
else
    echo "  FAIL: after $COUNT commands frontmost $BASE_FRONT->$NOW_FRONT cursor $BASE_MOUSE->$NOW_MOUSE"; RC=1
fi

echo "[2/4] and copad's own key window, first responder and focused pane"
# The exceptions are `tab.select`/`tab.move` (the user asked to switch/reorder), a plain
# `webview.open` (the user asked to go there), `tab.close` of the ACTIVE tab (something must
# take focus), and `tab.protect` (it destroys and rebuilds the web view). They stay out of
# this list; phase 1 covers them with the frontmost-app and cursor checks, which admit no
# exceptions. The agent-facing `webview.open --background`, in every mode, is in THIS list
# because it must not move anything.
page_level_only() {
    rpc webview.state
    rpc webview.page_info
    rpc webview.query '{"selector":"#link"}'
    rpc webview.query_all '{"selector":"a","limit":5}'
    rpc webview.get_styles '{"selector":"#link","properties":"color,display"}'
    rpc webview.get_content
    rpc webview.execute_js '{"code":"1+1"}'
    rpc webview.click '{"selector":"#in"}'
    rpc webview.fill '{"selector":"#in","value":"typed by the agent"}'
    rpc webview.scroll '{"y":500}'
    rpc webview.screenshot
    rpc webview.navigate "{\"url\":\"$BASE/page.html\"}"
    rpc webview.reload
    rpc webview.net
    rpc webview.console
    rpc browser.secret.list
    rpc webview.tab.list
    rpc webview.tab.new "{\"url\":\"$BASE/page.html\"}"
    # The agent-facing open, in every mode: none of these may move focus (including `split_h` and
    # `split_v`, which must honor `background`).
    rpc webview.open "{\"url\":\"$BASE/page.html\",\"background\":true}"
    rpc webview.open "{\"url\":\"$BASE/page.html\",\"background\":true,\"mode\":\"split_h\"}"
    rpc webview.open "{\"url\":\"$BASE/page.html\",\"background\":true,\"mode\":\"split_v\"}"
}
BEFORE_FOCUS=$(focus_state)
PAGE_COUNT=$(page_level_only | grep -c '"id":"e2e"')
NOW_FOCUS=$(focus_state)
if [ -n "$BEFORE_FOCUS" ] && [ "$BEFORE_FOCUS" = "$NOW_FOCUS" ]; then
    echo "  PASS: $NOW_FOCUS unchanged across $PAGE_COUNT page-level commands"
else
    echo "  FAIL: in-app focus moved"
    echo "        before: $BEFORE_FOCUS"
    echo "        after:  $NOW_FOCUS"; RC=1
fi

echo "[3/4] a background tab is driven and captured without being raised"
# The sweep created panes and switched tabs, so address $TAB2's pane explicitly; otherwise
# these would hit whatever pane is active, error out, and "active tab did not move" would
# pass having driven nothing.
BEFORE_ACTIVE=$(field "$(rpc webview.tab.list "{\"id\":\"$PANEL\"}")" active)
DRIVEN=$(rpc webview.execute_js "{\"id\":\"$PANEL\",\"tab_id\":\"$TAB2\",\"code\":\"document.title\"}")
SHOT=$(rpc webview.screenshot "{\"id\":\"$PANEL\",\"tab_id\":\"$TAB2\"}")
AFTER_ACTIVE=$(field "$(rpc webview.tab.list "{\"id\":\"$PANEL\"}")" active)
if ! grep -q '"ok":true' <<<"$DRIVEN" || ! grep -q '"ok":true' <<<"$SHOT"; then
    echo "  FAIL: driving the background tab did not succeed, so this proves nothing"
    echo "        exec: $(head -c 120 <<<"$DRIVEN")"; RC=1
elif [ -n "$BEFORE_ACTIVE" ] && [ "$BEFORE_ACTIVE" = "$AFTER_ACTIVE" ]; then
    echo "  PASS: drove and captured tab $TAB2 with the visible tab still at index $AFTER_ACTIVE"
else
    echo "  FAIL: active tab moved '$BEFORE_ACTIVE' -> '$AFTER_ACTIVE'"; RC=1
fi

echo "[4/4] terminal input survives concurrent browser automation"
rpc tab.new '{}' >/dev/null 2>&1
sleep 2
PANE=$(python3 -c "
import json,sys
try: print(json.loads(sys.argv[1])['result'].get('active_pane',''))
except Exception: print('')" "$(rpc window.focus_state)")
# Drive by PANEL id: with a terminal tab active, a command with no `id` resolves to "no
# active webview" and the concurrency would be fake.
drive_by_id() {
    for _ in 1 2 3 4; do
        rpc webview.page_info "{\"id\":\"$PANEL\"}"
        rpc webview.query "{\"id\":\"$PANEL\",\"selector\":\"#link\"}"
        rpc webview.screenshot "{\"id\":\"$PANEL\"}"
        rpc webview.execute_js "{\"id\":\"$PANEL\",\"code\":\"1+1\"}"
        rpc webview.scroll "{\"id\":\"$PANEL\",\"y\":200}"
    done
}
( sleep 0.2; drive_by_id >/dev/null 2>&1 ) &
DRIVER=$!
SENT=0
for word in the quick brown fox jumps over the lazy dog; do
    rpc terminal.feed "{\"text\":\"$word \"}" >/dev/null 2>&1 && SENT=$((SENT + 1))
    sleep 0.12
done
wait $DRIVER 2>/dev/null
sleep 1
GOT=$(python3 -c "
import json,sys
try: d=json.loads(sys.argv[1])
except Exception: print(''); raise SystemExit
r=d.get('result',{})
if isinstance(r,dict):
    print(r.get('content') or r.get('text') or r.get('screen') or json.dumps(r))
else:
    print(json.dumps(r))" "$(rpc terminal.read '{}' 2>/dev/null)")
FLAT=$(tr -s ' \n' ' ' <<<"$GOT")
if grep -q "the quick brown fox jumps over the lazy dog" <<<"$FLAT"; then
    echo "  PASS: all $SENT words arrived complete and in order while 20 browser commands ran"
else
    echo "  FAIL: input was dropped or reordered under concurrent automation"
    echo "        sent=$SENT got: $(tail -c 160 <<<"$FLAT")"; RC=1
fi

osascript -e "tell application \"$ORIGINAL_FRONT\" to activate" >/dev/null 2>&1
[ $RC -eq 0 ] && echo "ALL PASS" || echo "FAILURES (rc=$RC)"
exit $RC
