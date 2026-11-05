#!/usr/bin/env bash
# e2e for Browser Workbench persistence + focus claims (macOS).
#
#   1. A browser pane persists a tab snapshot whose id survives a restart, and a snapshot taken
#      before the page finishes loading keeps its URL instead of writing "" (which erases the
#      pane). An unroutable address makes WKWebView never produce a URL for that case.
#   2. Driving the browser steals neither the frontmost application nor the mouse cursor.
#
# Isolated by $HOME, so it cannot touch the live session.json. The Swift side reads config via
# `homeDirectoryForCurrentUser`, which ignores $HOME: config is the real one (read-only here),
# while everything Rust writes lands in the temp home.
#
# Usage: scripts/e2e-browser-persist.sh
#
# Build the bundle first, but note `copad-macos/run.sh` ends with
# `pkill -x Copad` + `open -n` — it KILLS a running installed Copad and takes
# over. To build without disturbing a live session:
#
#   cargo build --release -p copad-ffi -p copad-term
#   cd copad-macos && swift build
#   # then lay out .build/debug/Copad.app as run.sh does, minus the last two lines
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
APP="$REPO/copad-macos/.build/debug/Copad.app/Contents/MacOS/Copad"
COCTL="$REPO/target/release/coctl"; [ -x "$COCTL" ] || COCTL="$REPO/target/debug/coctl"
# Every CLI call below runs after `export COPAD_SOCKET=` for the instance under test;
# otherwise coctl inherits the ambient socket and opens a tab in the user's REAL copad.
[ -x "$APP" ] || { echo "build the bundle first — see the header"; exit 2; }

# Raw JSON-RPC over the per-instance GUI socket. `coctl webview <verb>` requires `--id` and no
# CLI verb exposes panel ids; the macOS dispatcher falls back to the active webview when `id`
# is omitted.
rpc() {
    python3 - "$COPAD_SOCKET" "$1" "${2:-{\}}" <<'PY'
import json, socket, sys
sock, method, params = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(15)
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

RC=0

# Every instance this run launched, for the reaper below. The inline kills do not run when an
# assertion `exit`s or on Ctrl-C (a run once left a debug Copad holding a temp $HOME for 17
# hours). A FILE, not an array: `launch` runs in `$(...)`, a subshell. The record is written
# before the caller reads the pid, so an interrupt between spawn and read is still reapable.
LAUNCHED=$(mktemp /private/tmp/copad-e2e-pids-XXXXXX)
cleanup() {
    [ -n "${LAUNCHED:-}" ] && [ -f "$LAUNCHED" ] || return 0
    while read -r pid home sock; do
        [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null
        [ -n "$sock" ] && rm -f "$sock"
        [ -n "$home" ] && rm -rf "$home"
    done <"$LAUNCHED"
    rm -f "$LAUNCHED"
}
# EXIT only: with `trap cleanup EXIT INT TERM` bash runs the handler and then RESUMES the
# script, deleting the temp $HOME the next command reads. INT/TERM just `exit`, which fires
# the EXIT trap once.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
SESSION_REL="Library/Application Support/copad/session.json"

# The Swift side ignores $HOME for config, so the app reads the REAL `[browser] restore`.
# Derive expectations from it, or a user who opted into `url`/`full` sees false failures.
RESTORE=$(python3 - "$HOME/.config/copad/config.toml" <<'PY'
import re, sys
try:
    text = open(sys.argv[1]).read()
except OSError:
    print("origin"); raise SystemExit
section = re.split(r"^\[", text, flags=re.M)
for part in section:
    if part.startswith("browser]"):
        m = re.search(r'^\s*restore\s*=\s*"([^"]*)"', part, flags=re.M)
        if m and m.group(1).strip().lower() in ("origin", "url", "full"):
            print(m.group(1).strip().lower()); raise SystemExit
print("origin")
PY
)
echo "restore policy in effect: $RESTORE"
# Under origin-only the path is dropped; under url/full it survives.
expected_url() {
    if [ "$RESTORE" = "origin" ]; then echo "$1"; else echo "$1$2"; fi
}

# A probe that fails must never compare equal to another failed probe — two
# empty strings would otherwise "match" and report PASS without measuring
# anything.
mouse() {
    local v
    v=$(osascript -l JavaScript -e 'ObjC.import("Cocoa"); var p=$.NSEvent.mouseLocation; Math.round(p.x)+","+Math.round(p.y)' 2>/dev/null)
    [[ "$v" =~ ^-?[0-9]+,-?[0-9]+$ ]] || { echo "FAIL: could not read the cursor position" >&2; return 1; }
    echo "$v"
}
front() {
    local v
    v=$(osascript -e 'tell application "System Events" to name of first application process whose frontmost is true' 2>/dev/null)
    [ -n "$v" ] || { echo "FAIL: could not read the frontmost application" >&2; return 1; }
    echo "$v"
}

# Launch an isolated instance and echo its pid + socket.
launch() {
    local home="$1"
    mkdir -p "$home/Library/Application Support/copad"
    HOME="$home" "$APP" >"$home/app.log" 2>&1 &
    local pid=$!
    local sock="/tmp/copad-$pid.sock"
    # Recorded before the socket wait, so an instance that never came up is
    # reaped too rather than surviving as an orphan with no socket to find it by.
    echo "$pid $home $sock" >>"$LAUNCHED"
    for _ in $(seq 1 40); do [ -S "$sock" ] && break; sleep 0.25; done
    [ -S "$sock" ] || { echo "FAIL: no gui socket at $sock"; tail -5 "$home/app.log"; kill -9 "$pid" 2>/dev/null; return 1; }
    echo "$pid $sock"
}

# check_pane <session.json> <expected-url> [expected-tab-id]
# Prints the tab id it found on the last line so the caller can compare it
# across a restart.
check_pane() {
    python3 - "$1" "$2" "${3:-}" <<'PY'
import json, sys
path, expect = sys.argv[1], sys.argv[2]
expect_id = sys.argv[3] if len(sys.argv) > 3 else ""
try:
    doc = json.load(open(path))
except Exception as e:
    print("FAIL: no readable session.json:", e); sys.exit(1)
def leaves(n):
    if n.get("type") == "leaf":
        yield n["content"]
    else:
        yield from leaves(n["first"]); yield from leaves(n["second"])
wvs = [c for t in doc["tabs"] for c in leaves(t["root"]) if c["kind"] == "webview"]
if not wvs:
    print("FAIL: no webview pane persisted"); sys.exit(1)
w = wvs[0]
print("  url  =", repr(w["url"]))
print("  pane =", json.dumps(w.get("pane")))
ok = True
if w["url"] != expect:
    print(f"  FAIL: expected {expect!r}, got {w['url']!r}"); ok = False
pane = w.get("pane")
if pane is None:
    print("  FAIL: no browser pane snapshot"); ok = False
else:
    tabs = pane["tabs"]
    if len(tabs) != 1:
        print("  FAIL: expected 1 tab, got", len(tabs)); ok = False
    elif not tabs[0].get("id"):
        print("  FAIL: tab has no stable id"); ok = False
    elif tabs[0]["url"] != w["url"]:
        print("  FAIL: pane url and tab url disagree"); ok = False
    elif expect_id and tabs[0]["id"] != expect_id:
        print(f"  FAIL: tab id changed across restart: {expect_id} -> {tabs[0]['id']}"); ok = False
    else:
        note = " (unchanged across restart)" if expect_id else ""
        print("  PASS: tab id =", tabs[0]["id"], " profile =", pane["profile"], note, sep="")
        print("TABID=" + tabs[0]["id"])
sys.exit(0 if ok else 1)
PY
}

tabid_of() { grep '^TABID=' <<<"$1" | tail -1 | cut -d= -f2; }

echo "[1/5] a browser pane persists a tab snapshot whose id survives a restart"
H1=$(mktemp -d /private/tmp/copad-e2e-XXXXXX)
read -r PID1 SOCK1 <<<"$(launch "$H1")" || exit 1
export COPAD_SOCKET="$SOCK1"; sleep 1
BASE_FRONT=$(front) || RC=1
BASE_MOUSE=$(mouse) || RC=1
"$COCTL" webview open "https://example.com" --mode tab >/dev/null 2>&1 \
    || { echo "  FAIL: webview open failed"; RC=1; }
sleep 2
# WebKit normalizes a bare origin to include the root path, so under `url`/`full`
# the persisted value carries a trailing slash.
OUT1=$(check_pane "$H1/$SESSION_REL" "$(expected_url https://example.com /)") || RC=1
echo "$OUT1" | grep -v '^TABID='
ID1=$(tabid_of "$OUT1")

echo "[2/5] agent-driven commands take neither focus nor the cursor"
DROVE=0
drive() {
    local out
    out=$(rpc "$1" "${2:-{\}}" 2>/dev/null)
    if grep -q '"ok":true' <<<"$out"; then
        DROVE=$((DROVE + 1))
    else
        echo "  FAIL: $1 did not succeed — a command that never ran proves nothing: $out"; RC=1
    fi
}
drive webview.page_info
drive webview.query '{"selector":"h1"}'
# Not `a`: example.com's only link goes to iana.org, and clicking it would race the restart
# assertion below into persisting the wrong origin. `h1` exercises the same click path.
drive webview.click '{"selector":"h1"}'
drive webview.screenshot
NOW_FRONT=$(front) || RC=1
NOW_MOUSE=$(mouse) || RC=1
if [ "$DROVE" -eq 4 ] && [ -n "$BASE_FRONT" ] && [ -n "$BASE_MOUSE" ] \
   && [ "$BASE_FRONT" = "$NOW_FRONT" ] && [ "$BASE_MOUSE" = "$NOW_MOUSE" ]; then
    echo "  PASS: frontmost=$NOW_FRONT cursor=$NOW_MOUSE unchanged across $DROVE commands"
else
    echo "  FAIL: drove=$DROVE/4, frontmost $BASE_FRONT->$NOW_FRONT, cursor $BASE_MOUSE->$NOW_MOUSE"; RC=1
fi

# Restart against the SAME home. A tab id that is reissued on every restore
# would still have passed the nonempty check above; only this catches it.
kill -TERM "$PID1" 2>/dev/null; sleep 1; kill -9 "$PID1" 2>/dev/null; rm -f "$SOCK1"
read -r PID1B SOCK1B <<<"$(launch "$H1")" || exit 1
export COPAD_SOCKET="$SOCK1B"; sleep 3
"$COCTL" webview open "https://example.org" --mode tab >/dev/null 2>&1
sleep 2
if [ -n "$ID1" ]; then
    OUT1B=$(check_pane "$H1/$SESSION_REL" "$(expected_url https://example.com /)" "$ID1") || RC=1
    echo "$OUT1B" | grep -v '^TABID='
else
    echo "  FAIL: no tab id captured before the restart"; RC=1
fi
kill -9 "$PID1B" 2>/dev/null; rm -f "$SOCK1B"; rm -rf "$H1"

echo "[3/5] a pane whose page never loads still persists its URL"
H2=$(mktemp -d /private/tmp/copad-e2e-XXXXXX)
read -r PID2 SOCK2 <<<"$(launch "$H2")" || exit 1
export COPAD_SOCKET="$SOCK2"; sleep 1
# Unroutable: WKWebView never produces a url, so the snapshot must come from the pending
# restore URL or the pane is written out as "" and lost.
"$COCTL" webview open "https://10.255.255.1/never-loads" --mode tab >/dev/null 2>&1 \
    || { echo "  FAIL: webview open failed"; RC=1; }
sleep 2.5
OUT2=$(check_pane "$H2/$SESSION_REL" "$(expected_url https://10.255.255.1 /never-loads)") || RC=1
echo "$OUT2" | grep -v '^TABID='
kill -9 "$PID2" 2>/dev/null; rm -f "$SOCK2"; rm -rf "$H2"

echo "[3b/5] a BLANK pane navigated to an unloadable URL also persists it"
H2B=$(mktemp -d /private/tmp/copad-e2e-XXXXXX)
read -r PID2B SOCK2B <<<"$(launch "$H2B")" || exit 1
export COPAD_SOCKET="$SOCK2B"; sleep 1
# Open blank, then navigate: `pendingURL` is seeded at init in [3/5], so only this covers
# `navigate(to:)`.
"$COCTL" webview open "about:blank" --mode tab >/dev/null 2>&1
sleep 1
drive_ok=$(rpc webview.navigate '{"url":"https://10.255.255.2/never-loads"}')
grep -q '"ok":true' <<<"$drive_ok" || { echo "  FAIL: navigate rejected: $drive_ok"; RC=1; }
sleep 2.5
OUT2B=$(check_pane "$H2B/$SESSION_REL" "$(expected_url https://10.255.255.2 /never-loads)") || RC=1
echo "$OUT2B" | grep -v '^TABID='
kill -9 "$PID2B" 2>/dev/null; rm -f "$SOCK2B"; rm -rf "$H2B"

echo "[4/5] not-yet-built methods answer unsupported_capability, not unknown_method"
H3=$(mktemp -d /private/tmp/copad-e2e-XXXXXX)
read -r PID3 SOCK3 <<<"$(launch "$H3")" || exit 1
export COPAD_SOCKET="$SOCK3"; sleep 1
for m in webview.profile.list webview.profile.clear; do
    OUT=$(rpc "$m")
    if grep -q '"unsupported_capability"' <<<"$OUT"; then
        echo "  PASS: $m -> unsupported_capability"
    else
        echo "  FAIL: $m -> $OUT"; RC=1
    fi
done
OUT=$(rpc browser.secret.fill '{"credential_id":"nobody/nothing"}')
if grep -q "requires_protected" <<<"$OUT"; then
    echo "  PASS: browser.secret.fill on an unprotected tab -> requires_protected"
else
    echo "  FAIL: browser.secret.fill -> $OUT"; RC=1
fi
kill -9 "$PID3" 2>/dev/null; rm -f "$SOCK3"; rm -rf "$H3"

echo "[5/5] a page-initiated navigation updates the persisted URL"
H4=$(mktemp -d /private/tmp/copad-e2e-XXXXXX)
read -r PID4 SOCK4 <<<"$(launch "$H4")" || exit 1
export COPAD_SOCKET="$SOCK4"; sleep 1
"$COCTL" webview open "https://example.com" --mode tab >/dev/null 2>&1
sleep 3
# A page-initiated navigation via `location.href` never passes through navigate()/goBack()/
# reload(), so it exercises `didStartProvisionalNavigation` adopting a navigation this
# controller did not start. Not a click on example.com's link: WebKit follows that synthetic
# click only sometimes.
GO=$(rpc webview.execute_js '{"code":"location.href = \"https://example.org/\"; \"ok\""}')
grep -q '"ok":true' <<<"$GO" || { echo "  FAIL: execute_js rejected: $GO"; RC=1; }

persisted_url() {
    python3 - "$1" <<'PY'
import json, sys
try:
    doc = json.load(open(sys.argv[1]))
except Exception:
    print(""); raise SystemExit
def leaves(n):
    if n.get("type") == "leaf":
        yield n["content"]
    else:
        yield from leaves(n["first"]); yield from leaves(n["second"])
for c in (c for t in doc["tabs"] for c in leaves(t["root"]) if c["kind"] == "webview"):
    print(c["url"]); break
else:
    print("")
PY
}

# Poll, not sleep: this depends on a real page load over the network.
FOUND=""
for _ in $(seq 1 30); do
    FOUND=$(persisted_url "$H4/$SESSION_REL")
    case "$FOUND" in
        ""|https://example.com|https://example.com/) sleep 0.5 ;;
        *) break ;;
    esac
done
case "$FOUND" in
    ""|https://example.com|https://example.com/)
        echo "  FAIL: still persisting ${FOUND:-<nothing>} 15s after a page-initiated navigation"; RC=1 ;;
    *)
        echo "  PASS: followed the page-initiated navigation to $FOUND" ;;
esac
kill -9 "$PID4" 2>/dev/null; rm -f "$SOCK4"; rm -rf "$H4"

[ $RC -eq 0 ] && echo "ALL PASS" || echo "FAILURES (rc=$RC)"
exit $RC
