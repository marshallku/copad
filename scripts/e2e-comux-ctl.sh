#!/usr/bin/env bash
# End-to-end verification of the comux control CLI's destructive verbs
# (`close-tab` / `kill-session`) against a REAL headless server.
#
# These can't be unit-tested: `App` owns live PTYs, so the only honest check is to
# drive an actual server over its control socket. The server runs on a throwaway
# socket AND a throwaway state file — without the latter it would restore (and then
# kill) the user's real sessions.
#
# Every request runs under a deadline on purpose: the first version of this script
# caught a wedged server (a pane whose shell outlived its SIGHUP blocked the
# single-writer loop inside `PaneTerm::drop`), and a hang is the exact regression
# worth failing on rather than waiting out.
#
# Steps:
#   1. headless server on an isolated socket/state
#   2. split → close reaps the pane; new-tab → close-tab reaps the tab
#   3. close-tab on a session's LAST tab is refused
#   4. new-session → kill-session <i> reaps it
#   5. kill-session on the LAST session is refused
#   6. a pane whose shell IGNORES SIGHUP is killed without wedging the server
#   7. pane identity (`$COPAD_MUX_PANE`) + `notify` + cross-session `jump` by token
#   8. a bad index is refused; `--json` with no index is a usage error (exit 2)
#   9. capture-pane reads a pane's output back (by default/token/index) + refusals

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMUX="$REPO/target/debug/comux"
[[ -x "$COMUX" ]] || { echo "build first: cargo build -p copad-mux"; exit 2; }

WORK="$(mktemp -d -t comux-e2e.XXXXXX)"
export COPAD_MUX_SOCK="$WORK/sock"
export COPAD_MUX_STATE="$WORK/state.json"
# Keep the run hermetic: no desktop toasts, no network (usage/update pollers).
export COPAD_MUX_NOTIFY=0
export COPAD_MUX_USAGE=0
export COPAD_MUX_UPDATE_CHECK=0
export COPAD_MUX_QUIET_SSH=1

SERVER_PID=""
DEAF_PID=""
cleanup() {
    # Step 6's shell ignores SIGHUP by design: kill it explicitly or it is reparented
    # to init and outlives every run of this script.
    [[ -n "$DEAF_PID" ]] && kill -9 "$DEAF_PID" 2>/dev/null || true
    [[ -n "$SERVER_PID" ]] && kill -9 "$SERVER_PID" 2>/dev/null || true
    rm -rf "$WORK"
    return 0
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "  ok — $*"; }

# Run a command under a hard deadline, since `timeout(1)` is not on a stock macOS.
# A wedged server must fail this script, not hang it.
t() {
    local secs="$1" i=0 pid
    shift
    "$@" &
    pid=$!
    while kill -0 "$pid" 2>/dev/null; do
        if (( i++ >= secs * 10 )); then
            kill -9 "$pid" 2>/dev/null
            fail "timed out after ${secs}s (server wedged?): $*"
        fi
        sleep 0.1
    done
    wait "$pid"
}

# Entries in a `--json` listing's array field, read from a file `t` wrote.
count() { python3 -c 'import json,sys; print(len(json.load(sys.stdin)[sys.argv[1]] or []))' "$1" <"$WORK/json"; }
panes_now() { t 10 "$COMUX" list --json >"$WORK/json"; count panes; }
tabs_now() { t 10 "$COMUX" list-tabs --json >"$WORK/json"; count tabs; }
sessions_now() { t 10 "$COMUX" list-sessions --json >"$WORK/json"; count sessions; }

echo "1. start a headless server on $COPAD_MUX_SOCK"
"$COMUX" server >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do [[ -S "$COPAD_MUX_SOCK" ]] && break; sleep 0.1; done
[[ -S "$COPAD_MUX_SOCK" ]] || fail "server never created its socket (see $WORK/server.log)"
ok "server up (pid $SERVER_PID)"

echo "2. split → close reaps the pane; new-tab → close-tab reaps the tab"
# `close` (pane) shares the `PaneTerm::drop` path the new verbs use, so it is checked here too.
t 10 "$COMUX" split -h >/dev/null
before="$(panes_now)"
[[ "$before" == "2" ]] || fail "expected 2 panes after split, got $before"
t 10 "$COMUX" close 1 >/dev/null || fail "close 1 exited non-zero"
after="$(panes_now)"
[[ "$after" == "1" ]] || fail "expected 1 pane after close, got $after"
t 10 "$COMUX" new-tab >/dev/null
before="$(tabs_now)"
[[ "$before" == "2" ]] || fail "expected 2 tabs after new-tab, got $before"
t 10 "$COMUX" close-tab 1 >/dev/null || fail "close-tab 1 exited non-zero"
after="$(tabs_now)"
[[ "$after" == "1" ]] || fail "expected 1 tab after close-tab, got $after"
ok "2 panes → close 1 → 1 pane; 2 tabs → close-tab 1 → 1 tab"

echo "3. the last tab of a session is refused"
if t 10 "$COMUX" close-tab 0 >"$WORK/out" 2>&1; then
    fail "close-tab on the last tab should have failed: $(cat "$WORK/out")"
fi
grep -q "last tab" "$WORK/out" || fail "expected a 'last tab' error, got: $(cat "$WORK/out")"
[[ "$(tabs_now)" == "1" ]] || fail "the refused tab was closed anyway"
ok "refused, and the tab survives"

echo "4. new-session, then kill-session reaps it"
t 10 "$COMUX" new-session e2e-victim >/dev/null
before="$(sessions_now)"
[[ "$before" == "2" ]] || fail "expected 2 sessions after new-session, got $before"
t 10 "$COMUX" kill-session 1 >/dev/null || fail "kill-session 1 exited non-zero"
after="$(sessions_now)"
[[ "$after" == "1" ]] || fail "expected 1 session after kill-session, got $after"
ok "2 sessions → kill-session 1 → 1 session"

echo "5. the last session is refused"
if t 10 "$COMUX" kill-session 0 >"$WORK/out" 2>&1; then
    fail "kill-session on the last session should have failed: $(cat "$WORK/out")"
fi
grep -q "last session" "$WORK/out" || fail "expected a 'last session' error, got: $(cat "$WORK/out")"
[[ "$(sessions_now)" == "1" ]] || fail "the refused session died anyway"
ok "refused, and the session survives"

echo "6. a shell that ignores SIGHUP is reaped without wedging the server"
# `Pty::drop` SIGHUPs the pane's shell and then `waitpid`s for it with no timeout, so a
# shell that survives the signal used to block whoever dropped the pane — on the server
# that is the single-writer loop, i.e. the whole mux. Reproduce it deterministically
# rather than waiting for a slow-starting shell to lose the same race by accident.
pgrep -P "$SERVER_PID" | sort >"$WORK/pids-before"
t 10 "$COMUX" new-session e2e-deaf >/dev/null
sleep 2 # let the shell finish sourcing its rc files, so it can accept the trap
pgrep -P "$SERVER_PID" | sort >"$WORK/pids-after"
DEAF_PID="$(comm -13 "$WORK/pids-before" "$WORK/pids-after" | head -1)"
[[ -n "$DEAF_PID" ]] || fail "could not identify the new session's shell"
t 10 "$COMUX" send 0 $'trap "" HUP\n' >/dev/null
sleep 1
t 12 "$COMUX" kill-session 1 >/dev/null || fail "kill-session on a SIGHUP-deaf pane failed"
t 10 "$COMUX" health >/dev/null || fail "the server wedged reaping a SIGHUP-deaf pane"
[[ "$(sessions_now)" == "1" ]] || fail "the SIGHUP-deaf session was not removed"
# Detaching the wait alone would only trade the wedge for a leak, so the shell must also be
# escalated to SIGKILL and collected. `kill -0` still succeeds on a zombie, so this waits for
# the reaper to finish the job, not merely for the signal.
for _ in $(seq 1 75); do kill -0 "$DEAF_PID" 2>/dev/null || break; sleep 0.2; done
kill -0 "$DEAF_PID" 2>/dev/null && fail "the SIGHUP-deaf shell (pid $DEAF_PID) leaked"
DEAF_PID=""
ok "reaped off the loop, escalated to SIGKILL; the server stayed responsive"

echo "7. pane identity, notify, and jump by token"
t 10 "$COMUX" list --json >"$WORK/json"
token0="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["panes"][0]["token"])' <"$WORK/json")"
[[ -n "$token0" ]] || fail "pane 0 has no \$COPAD_MUX_PANE token"
# Incarnation-qualified (`<nonce>-<n>`), so a token from a previous server can't resolve.
[[ "$token0" == *-* ]] || fail "token '$token0' is not <nonce>-<counter>"
t 10 "$COMUX" notify --pane "$token0" --kind blocked "e2e says hello" >/dev/null \
    || fail "notify on a live pane failed"
if t 10 "$COMUX" notify --pane "no-such-pane" --kind done body >"$WORK/out" 2>&1; then
    fail "notify on an unknown pane should have failed: $(cat "$WORK/out")"
fi
grep -q "unknown pane" "$WORK/out" || fail "expected 'unknown pane', got: $(cat "$WORK/out")"
if t 10 "$COMUX" notify --pane "$token0" --kind sideways body >"$WORK/out" 2>&1; then
    fail "notify with a bogus kind should have failed: $(cat "$WORK/out")"
fi
grep -q "unknown kind" "$WORK/out" || fail "expected 'unknown kind', got: $(cat "$WORK/out")"
# No pane given and no $COPAD_MUX_PANE in the environment: a usage error, NEVER a
# notification silently attributed to whichever pane happens to be active.
set +e
(unset COPAD_MUX_PANE; t 10 "$COMUX" notify "orphan" >/dev/null 2>&1); code=$?
set -e
[[ "$code" == "2" ]] || fail "expected usage exit 2 for a pane-less notify, got $code"
# Jump crosses sessions by identity: switch away, then jump back by the token.
t 10 "$COMUX" new-session e2e-jump >/dev/null
t 10 "$COMUX" list-sessions --json >"$WORK/json"
active="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["active_session"])' <"$WORK/json")"
[[ "$active" == "1" ]] || fail "expected the new session to be active, got index $active"
t 10 "$COMUX" jump "$token0" >/dev/null || fail "jump to a live pane failed"
t 10 "$COMUX" list-sessions --json >"$WORK/json"
active="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["active_session"])' <"$WORK/json")"
[[ "$active" == "0" ]] || fail "jump did not switch back to session 0 (active=$active)"
if t 10 "$COMUX" jump "deadbeef-99" >"$WORK/out" 2>&1; then
    fail "jump to a stale token should have failed: $(cat "$WORK/out")"
fi
grep -q "unknown pane" "$WORK/out" || fail "expected 'unknown pane', got: $(cat "$WORK/out")"
t 10 "$COMUX" kill-session 1 >/dev/null || fail "could not clean up the jump session"
ok "token minted per pane; notify validated; jump switched sessions by identity"

echo "8. bad index / missing index"
t 10 "$COMUX" close-tab 99 >/dev/null 2>&1 && fail "close-tab 99 should have failed"
t 10 "$COMUX" kill-session 99 >/dev/null 2>&1 && fail "kill-session 99 should have failed"
# `--json` suppresses the fuzzy picker, so an omitted index stays a usage error (2).
set +e
t 10 "$COMUX" close-tab --json >/dev/null 2>&1; code=$?
set -e
[[ "$code" == "2" ]] || fail "expected usage exit 2 for a picker-less close-tab, got $code"
ok "out-of-range refused; --json with no index is exit 2"

echo "9. capture-pane reads a pane back"
# The marker must NOT appear literally in the command we send, or capture would match
# the shell's echo of the input line instead of the command's OUTPUT.
t 10 "$COMUX" send 0 "printf 'CAP%s_OK\n' TURE" >/dev/null
t 10 "$COMUX" send 0 $'\n' >/dev/null
found=""
for _ in $(seq 1 50); do
    t 10 "$COMUX" capture-pane >"$WORK/cap" 2>/dev/null || true
    if grep -q "CAPTURE_OK" "$WORK/cap"; then found=1; break; fi
    sleep 0.2
done
[[ -n "$found" ]] || fail "capture-pane never saw the marker: $(tail -3 "$WORK/cap")"
# Addressing by token and by index must agree once output has gone quiet.
t 10 "$COMUX" list --json >"$WORK/json"
tok="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["panes"][0]["token"])' <"$WORK/json")"
sleep 0.5
t 10 "$COMUX" capture-pane "$tok" >"$WORK/by-token"
t 10 "$COMUX" capture-pane --index 0 >"$WORK/by-index"
diff -q "$WORK/by-token" "$WORK/by-index" >/dev/null \
    || fail "capture by token and by index disagree"
# The resolved pane is echoed, so a defaulted target is auditable.
t 10 "$COMUX" capture-pane --json >"$WORK/json"
echoed="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pane"])' <"$WORK/json")"
[[ "$echoed" == "$tok" ]] || fail "capture-pane echoed pane '$echoed', expected '$tok'"
# A deep request must be answered by the budget, not by hanging.
t 10 "$COMUX" capture-pane -S 999999 >/dev/null || fail "an over-long capture failed"
# Refusals.
t 10 "$COMUX" capture-pane no-such-pane >/dev/null 2>&1 && fail "unknown target should fail"
t 10 "$COMUX" capture-pane -S 0 >/dev/null 2>&1 && fail "--lines 0 should fail"
set +e
t 10 "$COMUX" capture-pane "$tok" --index 0 >/dev/null 2>&1; code=$?
set -e
[[ "$code" == "2" ]] || fail "target + --index should be usage exit 2, got $code"
ok "marker read back; token/index agree; resolved pane echoed; refusals honored"

echo "10. the server is still responsive and shuts down cleanly"
t 10 "$COMUX" health >/dev/null || fail "health failed — the server did not survive the run"
t 15 "$COMUX" kill-server >/dev/null || fail "kill-server failed"
ok "healthy, then stopped"

echo
echo "PASS — comux close-tab / kill-session / notify / jump / capture-pane verified end to end"
