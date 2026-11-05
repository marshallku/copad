#!/usr/bin/env bash
# End-to-end verification of the Browser Workbench dispatcher gate on Linux, driving the REAL
# copad GTK app over its per-instance socket.
#
#   1. Every method registered-but-not-built answers `unsupported_capability`, not
#      `unknown_method`.
#   2. A typo is NOT claimed by the gate (asserted negatively, see step 2).
#   3. An UNPROTECTED pane behaves as before: reads return page data and a failed write still
#      reports why (`redacts_write_result` is false without protection).
#   4. A `tab_id` naming a tab the pane does not hold answers `tab_closed`, never a silent
#      retarget onto the active tab.
#   5. A panel id that never existed still answers the handler's own `not_found`; the
#      delivery-time fail-closed is only for a pane that vanished MID-FLIGHT.
#
# Not asserted: the behaviour of a PROTECTED tab, since `webview.tab.protect` is unimplemented
# on Linux. Its refusal paths are unit-tested in copad-core.
#
# Requires a display: it opens a window on the current session for a few seconds, then kills it.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COPAD="$REPO/target/debug/copad"
COCTL="$REPO/target/debug/coctl"
COPADD="$REPO/target/debug/copadd"

[[ -x "$COPAD" ]] || { echo "build first: cargo build -p copad-linux"; exit 2; }
[[ -x "$COCTL" ]] || { echo "build first: cargo build -p copad-cli"; exit 2; }
[[ -x "$COPADD" ]] || { echo "build first: cargo build -p copad-daemon"; exit 2; }

if [[ -z "${DISPLAY:-}" && -z "${WAYLAND_DISPLAY:-}" ]]; then
    echo "SKIP: no DISPLAY or WAYLAND_DISPLAY — copad is a GTK app and cannot run headless here." >&2
    exit 0
fi

WORK="$(mktemp -d -t copad-e2e-browser.XXXXXX)"
# The work path is interpolated into a socket path and a TOML file; keep it to
# a strict allowlist so a hostile TMPDIR cannot inject either.
if ! printf '%s' "$WORK" | LC_ALL=C grep -Eq '^[A-Za-z0-9._/-]+$'; then
    echo "Refusing to run: WORK path '$WORK' has characters outside [A-Za-z0-9._/-]." >&2
    exit 2
fi

# A copad pane exports COPAD_SOCKET, so running this from inside copad would hand every
# child the USER'S LIVE socket and could drive their real browser panes. Clear it before
# launching anything.
unset COPAD_SOCKET

# Isolate every path the app touches. XDG_RUNTIME_DIR is what decides where the
# per-instance GUI socket lands (`copad_core::paths::gui_socket_path`), so this
# is also what guarantees we never drive the user's live copad.
export XDG_RUNTIME_DIR="$WORK/run"
export XDG_CONFIG_HOME="$WORK/config"
export XDG_STATE_HOME="$WORK/state"
export XDG_CACHE_HOME="$WORK/cache"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/copad" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"
chmod 700 "$XDG_RUNTIME_DIR"

GUI_LOG="$WORK/gui.log"
DAEMON_LOG="$WORK/daemon.log"
COPAD_PID=""
COPADD_PID=""
PASS=0
FAIL=0

stop() {
    local pid="$1"
    [[ -n "$pid" ]] || return 0
    kill -0 "$pid" 2>/dev/null || return 0
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
    done
    kill -9 "$pid" 2>/dev/null || true
}

cleanup() {
    stop "$COPAD_PID"
    stop "$COPADD_PID"
    rm -rf "$WORK"
}
trap cleanup EXIT

ok()   { PASS=$((PASS + 1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  \033[31mFAIL\033[0m %s\n' "$1"; }

# `coctl call` prints the response JSON. On an error response it exits non-zero,
# so every call is guarded — we are asserting ON the errors here.
call() {
    local method="$1" params="${2:-\{\}}"
    "$COCTL" call "$method" --params "$params" 2>&1 || true
}

expect_code() {
    local label="$1" method="$2" params="$3" code="$4"
    local out
    out="$(call "$method" "$params")"
    if printf '%s' "$out" | grep -q "$code"; then
        ok "$label"
    else
        bad "$label — expected '$code', got: $(printf '%s' "$out" | head -c 200)"
    fi
}

# copadd binds `runtime_dir()/socket`, which the isolated XDG_RUNTIME_DIR
# redirects — and it is the same path the GUI's `daemon_socket_path()` resolves,
# so the GUI registers with THIS daemon and not the user's live one.
DAEMON_SOCKET="$XDG_RUNTIME_DIR/copad/socket"
echo "== launching copadd =="
"$COPADD" >"$DAEMON_LOG" 2>&1 &
COPADD_PID=$!
for _ in $(seq 1 60); do
    [[ -S "$DAEMON_SOCKET" ]] && break
    sleep 0.2
done
[[ -S "$DAEMON_SOCKET" ]] || { echo "copadd socket never appeared; log:" >&2; cat "$DAEMON_LOG" >&2; exit 1; }

echo "== launching copad (isolated XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR) =="
"$COPAD" >"$GUI_LOG" 2>&1 &
COPAD_PID=$!

SOCKET=""
for _ in $(seq 1 100); do
    if ! kill -0 "$COPAD_PID" 2>/dev/null; then
        echo "copad exited during startup; log:" >&2
        cat "$GUI_LOG" >&2
        exit 1
    fi
    candidate="$XDG_RUNTIME_DIR/copad/gui-$COPAD_PID.sock"
    if [[ -S "$candidate" ]]; then
        SOCKET="$candidate"
        break
    fi
    sleep 0.2
done
[[ -n "$SOCKET" ]] || { echo "GUI socket never appeared; log:" >&2; cat "$GUI_LOG" >&2; exit 1; }
export COPAD_SOCKET="$SOCKET"
echo "   socket: $SOCKET"

echo
echo "== 1. registered-but-not-built methods answer unsupported_capability =="
# Mirrors `crate::browser::UNIMPLEMENTED`. Kept as a literal list rather than
# read from the binary, so removing a name from the source without building the
# method shows up here as a failure.
for m in \
    webview.tab.new webview.tab.list webview.tab.select webview.tab.close \
    webview.tab.move webview.tab.protect \
    webview.profile.list webview.profile.clear \
    browser.secret.list browser.secret.fill browser.secret.save browser.secret.delete \
    webview.net webview.console webview.clear_log
do
    expect_code "$m" "$m" '{}' 'unsupported_capability'
done

echo
echo "== 2. a typo is not claimed by the gate =="
# NEGATIVE on purpose: on Linux an unrecognised method does not come back as
# `unknown_method`. copad-linux proxies anything the GUI does not know to copadd (`socket.rs`
# default arm -> `daemon_forward::forward`), and with no daemon here the client just waits
# (docs/troubleshooting.md). So: a short timeout, and the pass condition is that the gate did
# not answer.
expect_not_claimed() {
    local label="$1" method="$2"
    local out
    out="$(timeout 5 "$COCTL" call "$method" --params '{}' 2>&1 || true)"
    if printf '%s' "$out" | grep -q 'unsupported_capability'; then
        bad "$label — the gate claimed a typo as unimplemented: $(printf '%s' "$out" | head -c 200)"
    else
        ok "$label"
    fi
}
expect_not_claimed "webview.opne is not claimed as unimplemented" webview.opne
expect_not_claimed "browser.nonsense is not claimed as unimplemented" browser.nonsense

echo
echo "== 3. an unprotected pane is unchanged =="
OPEN_OUT="$(call webview.open '{"url":"about:blank","mode":"tab"}')"
PANEL_ID="$(printf '%s' "$OPEN_OUT" | grep -oE '"panel_id"[[:space:]]*:[[:space:]]*"[^"]+"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/')"
if [[ -n "$PANEL_ID" ]]; then
    ok "webview.open returns a panel_id (not the opaque protected response)"
else
    bad "webview.open did not return a panel_id: $(printf '%s' "$OPEN_OUT" | head -c 200)"
fi

if [[ -n "$PANEL_ID" ]]; then
    sleep 1

    STATE_OUT="$(call webview.state "{\"id\":\"$PANEL_ID\"}")"
    if printf '%s' "$STATE_OUT" | grep -q '"url"'; then
        ok "webview.state still returns live page data"
    else
        bad "webview.state did not return page data: $(printf '%s' "$STATE_OUT" | head -c 200)"
    fi

    # The selector-oracle defence must NOT be active without protection: a
    # missing element still reports that it was missing.
    CLICK_OUT="$(call webview.click "{\"id\":\"$PANEL_ID\",\"selector\":\"#definitely-not-here\"}")"
    if printf '%s' "$CLICK_OUT" | grep -q 'not found'; then
        ok "webview.click still reports not-found on an unprotected pane"
    else
        bad "webview.click was redacted without protection: $(printf '%s' "$CLICK_OUT" | head -c 200)"
    fi

    echo
    echo "== 4. a foreign tab_id is tab_closed, not a silent retarget =="
    expect_code "unknown tab_id" webview.state \
        "{\"id\":\"$PANEL_ID\",\"tab_id\":\"00000000-0000-0000-0000-000000000000\"}" \
        'tab_closed'
fi

echo
echo "== 5. an unknown panel id still reaches the handler's own error =="
expect_code "unknown panel id" webview.state '{"id":"no-such-panel"}' 'not_found'

echo
echo "== 6. the DAEMON-routed path reaches the same gate =="
# Steps 1-5 speak to the GUI socket. A real `coctl` call goes to copadd, which routes by
# capability (`gui_registry::method_capability`); `browser.*` once fell through there as
# `unknown_method` for a method the GUI registers. This proves the route end to end.
COPAD_SOCKET="$DAEMON_SOCKET" \
    expect_code "browser.secret.list via copadd" browser.secret.list '{}' 'unsupported_capability'
COPAD_SOCKET="$DAEMON_SOCKET" \
    expect_code "webview.tab.list via copadd" webview.tab.list '{}' 'unsupported_capability'

echo
echo "-- $PASS passed, $FAIL failed --"
[[ "$FAIL" -eq 0 ]]
