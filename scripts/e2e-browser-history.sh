#!/usr/bin/env bash
# e2e for the Browser Workbench history restore (macOS).
#
# Under `restore = "full"` a pane must come back with its back/forward stack and scroll
# offset, not just its URL. The check that matters is the BACK-LIST DEPTH: a corrupt or
# cross-version `interactionState` fails silently inside WebKit and leaves a view reporting
# exactly the URL a plain `load()` fallback would produce.
#
# Under `restore = "origin"` the same sequence must write NO blob: the blob is sensitive
# persistence and only the explicit opt-in earns it.
#
# Pages come from a local throwaway HTTP server: the scroll assertion needs a page that is
# genuinely tall before and after a restart (injecting `body.style.height` does not survive,
# the browser clamps the restored offset to 0), and it keeps the run offline and
# deterministic.
#
# The harness temporarily rewrites the real ~/.config/copad/config.toml, because the Swift
# side ignores $HOME for config. Everything else is isolated in a temp home. The original
# config is restored on every exit path, including a signal.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
APP="$REPO/copad-macos/.build/debug/Copad.app/Contents/MacOS/Copad"
COCTL="$REPO/target/release/coctl"; [ -x "$COCTL" ] || COCTL="$REPO/target/debug/coctl"
CONFIG="$HOME/.config/copad/config.toml"
[ -x "$APP" ] || { echo "build the bundle first"; exit 2; }

RC=0
CONFIG_BACKUP=$(mktemp /private/tmp/copad-cfg-XXXXXX)
cp "$CONFIG" "$CONFIG_BACKUP" 2>/dev/null || : > "$CONFIG_BACKUP"
cleanup() {
    cp "$CONFIG_BACKUP" "$CONFIG" 2>/dev/null
    rm -f "$CONFIG_BACKUP"
    [ -n "${APP_PID:-}" ] && kill -9 "$APP_PID" 2>/dev/null
    [ -n "${SOCK:-}" ] && rm -f "$SOCK"
    [ -n "${TMPHOME:-}" ] && rm -rf "$TMPHOME"
    [ -n "${SERVER_PID:-}" ] && kill -9 "$SERVER_PID" 2>/dev/null
    [ -n "${WWW:-}" ] && rm -rf "$WWW"
}
# EXIT only: with `trap cleanup EXIT INT TERM` bash runs the handler and then RESUMES the
# script, deleting the temp $HOME the next command reads. INT/TERM just `exit`, which fires
# the EXIT trap once.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Three tall pages, so a back-list has somewhere to go and the scroll offset room to survive.
WWW=$(mktemp -d /private/tmp/copad-www-XXXXXX)
for page in a b c; do
    cat > "$WWW/$page.html" <<HTML
<!doctype html><title>page $page</title>
<body style="margin:0">
<div style="height:6000px;background:linear-gradient(#fff,#ccc)">page $page</div>
</body>
HTML
done
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
(cd "$WWW" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
SERVER_PID=$!
BASE="http://127.0.0.1:$PORT"
for _ in $(seq 1 40); do
    curl -sf "$BASE/a.html" >/dev/null 2>&1 && break
    sleep 0.25
done
curl -sf "$BASE/a.html" >/dev/null 2>&1 || { echo "FAIL: local server did not start"; exit 1; }

set_policy() {
    cp "$CONFIG_BACKUP" "$CONFIG"
    printf '\n[browser]\nrestore = "%s"\n' "$1" >> "$CONFIG"
}

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

launch() {
    mkdir -p "$TMPHOME/Library/Application Support/copad"
    HOME="$TMPHOME" "$APP" >>"$TMPHOME/app.log" 2>&1 &
    APP_PID=$!
    SOCK="/tmp/copad-$APP_PID.sock"
    for _ in $(seq 1 40); do [ -S "$SOCK" ] && break; sleep 0.25; done
    [ -S "$SOCK" ] || { echo "  FAIL: no gui socket"; tail -5 "$TMPHOME/app.log"; return 1; }
    sleep 1
}

stop() {
    kill -TERM "$APP_PID" 2>/dev/null; sleep 1; kill -9 "$APP_PID" 2>/dev/null
    rm -f "$SOCK"; APP_PID=""; SOCK=""
}

# Build a two-deep history in one pane, then scroll.
build_history() {
    COPAD_SOCKET="$SOCK" "$COCTL" webview open "$BASE/a.html" --mode tab >/dev/null 2>&1
    sleep 2
    rpc webview.navigate "{\"url\":\"$BASE/b.html\"}" >/dev/null
    sleep 2
    rpc webview.navigate "{\"url\":\"$BASE/c.html\"}" >/dev/null
    sleep 2
    rpc webview.execute_js '{"code":"window.scrollTo(0,1200); String(window.scrollY)"}' >/dev/null
    # The scroll reporter is a trailing 150ms throttle, and the session save is debounced 0.8s
    # on top.
    sleep 2
}

pane_json() {
    python3 - "$TMPHOME/Library/Application Support/copad/session.json" <<'PY'
import json, sys
try:
    doc = json.load(open(sys.argv[1]))
except Exception:
    print("{}"); raise SystemExit
def leaves(n):
    if n.get("type") == "leaf":
        yield n["content"]
    else:
        yield from leaves(n["first"]); yield from leaves(n["second"])
for c in (c for t in doc["tabs"] for c in leaves(t["root"]) if c["kind"] == "webview"):
    print(json.dumps(c)); break
else:
    print("{}")
PY
}

echo "[1/3] restore = full: a blob is written with a real back-list depth"
set_policy full
TMPHOME=$(mktemp -d /private/tmp/copad-e2e-XXXXXX)
launch || exit 1
build_history
sleep 2
PANE=$(pane_json)
echo "  $PANE"
DEPTH=$(python3 -c "import json,sys; p=json.loads(sys.argv[1]); print((p.get('pane') or {}).get('tabs',[{}])[0].get('history_depth',''))" "$PANE")
GEN=$(python3 -c "import json,sys; p=json.loads(sys.argv[1]); print((p.get('pane') or {}).get('tabs',[{}])[0].get('history_generation',''))" "$PANE")
BLOBS=$(ls "$TMPHOME/Library/Application Support/copad/browser/history/" 2>/dev/null | wc -l | tr -d ' ')
SAVED_SCROLL=$(python3 -c "import json,sys; p=json.loads(sys.argv[1]); print((p.get('pane') or {}).get('tabs',[{}])[0].get('scroll_y',0))" "$PANE")
if [ -n "$GEN" ] && [ "${DEPTH:-0}" -ge 2 ] && [ "$BLOBS" -ge 1 ]; then
    echo "  PASS: generation=$GEN depth=$DEPTH blobs=$BLOBS scroll_y=$SAVED_SCROLL"
else
    echo "  FAIL: generation='$GEN' depth='$DEPTH' blobs=$BLOBS"; RC=1
fi
# Sampling scroll only on didFinish captured the position the page LOADED at (always 0), so
# restore put the user back at the top.
if python3 -c "import sys; sys.exit(0 if float(sys.argv[1] or 0) > 500 else 1)" "$SAVED_SCROLL"; then
    echo "  PASS: the scroll offset was captured, not left at 0"
else
    echo "  FAIL: scroll_y persisted as $SAVED_SCROLL after scrolling to 1200"; RC=1
fi

echo "[2/3] restore = full: the back-list survives a restart"
stop
launch || exit 1
sleep 3
STATE=$(rpc webview.state)
echo "  $STATE"
CANBACK=$(python3 -c "
import json,sys
try: r=json.loads(sys.argv[1]).get('result',{})
except Exception: r={}
print(str(r.get('can_go_back', r.get('canGoBack',''))).lower())" "$STATE")
SCROLL_RAW=$(rpc webview.execute_js '{"code":"String(Math.round(window.scrollY))"}')
SCROLL=$(python3 -c "
import json,sys
try: print(json.loads(sys.argv[1]).get('result',{}).get('result',''))
except Exception: print('')" "$SCROLL_RAW")
echo "  scrollY after restart -> ${SCROLL:-<none>}"
if [ "$CANBACK" = "true" ]; then
    echo "  PASS: the restored pane can go back — history was restored, not re-loaded"
else
    echo "  FAIL: can_go_back=$CANBACK (a plain URL load would look exactly like this)"; RC=1
fi
# The scroll offset is a restore hint (a page whose height changed clamps it): assert it
# landed in the right neighbourhood, not on an exact pixel.
if [ -n "$SCROLL" ] && [ "$SCROLL" -gt 500 ]; then
    echo "  PASS: scroll restored to $SCROLL (was ~1200 before the restart)"
else
    echo "  FAIL: scroll came back as '${SCROLL:-<none>}' — the user is back at the top"; RC=1
fi
stop
rm -rf "$TMPHOME"; TMPHOME=""

echo "[3/3] restore = origin: the same sequence writes NO blob"
set_policy origin
TMPHOME=$(mktemp -d /private/tmp/copad-e2e-XXXXXX)
launch || exit 1
build_history
sleep 2
PANE=$(pane_json)
HAS=$(python3 -c "import json,sys; p=json.loads(sys.argv[1]); print((p.get('pane') or {}).get('tabs',[{}])[0].get('history_generation','none'))" "$PANE")
BLOBS=$(ls "$TMPHOME/Library/Application Support/copad/browser/history/" 2>/dev/null | wc -l | tr -d ' ')
if [ "$HAS" = "none" ] && [ "$BLOBS" -eq 0 ]; then
    echo "  PASS: no generation recorded, no blob on disk"
else
    echo "  FAIL: generation=$HAS blobs=$BLOBS under origin-only policy"; RC=1
fi
stop

[ $RC -eq 0 ] && echo "ALL PASS" || echo "FAILURES (rc=$RC)"
exit $RC
