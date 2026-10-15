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
#  10. wait-output matches a fresh marker, times out at 124, refuses bad input
#  11. list-agents (empty vs populated) + wait-agent level-match / deadline / refusals
#  12. `comux skill` emits the embedded agent guide, and the recipe it teaches works
#  13. bell + title: an unfocused pane's BEL and OSC 0 title reach `list --json`
#  14. a codex pane reads ready/blocked, not the `idle` it used to fall through to
#  15. an agent's DOING line is read from its own structured log (agentpoll)
#  16. `comux host` publishes CPU/memory/GPU/load, omitting what it could not read
#  17. the top bar: off by default, and it moves the sidebar AND its click zones
#  18. dragging a split divider moves the branch it is on, and only that one
#  19. a notification jump focuses the exact copad TAB, and degrades when it cannot
#  20. the SSH fleet: an unreachable machine is reported with a reason, never dropped
#  21. a bell rung in a HIDDEN tab still reaches the rendered status bar
#  22. an idle client silently repaints itself on a timer, and never clears to do it
#  23. every verb answers --help (0) / rejects unknown flags (2) without touching state
#  24. an index-less rename renames the CALLER's tab/session, not the one the user is viewing
#  25. sessions/tabs addressed by id/name (ambiguous = exit 2); create verbs return their ids
#  26. list-tree shows every session/tab/pane; list-agents says which tab and pane an agent is in
#  27. wait-agent --after-seq only matches a NEWER status change; wait verbs answer in JSON
#  28. send --enter / --paste (bracketed only when the program asked) / --file
#  29. new-tab --session / --no-focus and split --from a background pane move nobody's view
#  30. spawn runs argv verbatim in a background tab; it closes on exit unless --keep

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMUX="$REPO/target/debug/comux"
# BUILD, rather than merely checking the binary exists. Testing a stale binary is the worst
# outcome this script has: it reports a failure that is not in the tree (or, worse, a pass for
# a fix that is not in the binary), and the message points at the code under test. It cost a
# real session — a rebase pulled in a fleet fix, the binary was 20 minutes older, and step 20
# reported the deadlock that commit had just removed. `COMUX_E2E_NO_BUILD=1` opts out for the
# rare deliberate case of running against a binary you built some other way.
if [[ "${COMUX_E2E_NO_BUILD:-0}" != 1 ]]; then
    (cd "$REPO" && cargo build -q -p copad-mux) || { echo "build failed"; exit 2; }
fi
[[ -x "$COMUX" ]] || { echo "build first: cargo build -p copad-mux"; exit 2; }

WORK="$(mktemp -d -t comux-e2e.XXXXXX)"
export COPAD_MUX_SOCK="$WORK/sock"
export COPAD_MUX_STATE="$WORK/state.json"
# Keep the run hermetic: no desktop toasts, no network (usage/update pollers).
export COPAD_MUX_NOTIFY=0
export COPAD_MUX_USAGE=0
export COPAD_MUX_UPDATE_CHECK=0
export COPAD_MUX_QUIET_SSH=1
# This script usually runs INSIDE a comux pane. Its pane identity names a pane on the user's
# real server, and verbs that default to "the caller's pane" would read it — a step that wants
# a caller pane sets one explicitly from the throwaway server.
unset COPAD_MUX_PANE COPAD_MUX
# Config isolation. Without this the throwaway server reads the DEVELOPER's real
# `~/.config/copad/mux.toml`, so every assertion below silently depends on their personal
# settings — a custom prefix, `sidebar = false`, a different `sidebar_density`. Point
# `XDG_CONFIG_HOME` at the work dir so the run starts from documented defaults, and so a step
# that needs a setting can write one without touching anything the user owns.
export XDG_CONFIG_HOME="$WORK/cfg"
mkdir -p "$WORK/cfg/copad"

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
t 10 "$COMUX" notify --pane "$token0" --kind blocked "e2e says hello" >"$WORK/out" \
    || fail "notify on a live pane failed"
# The response echoes WHERE the notification was attributed (`tool · space/tab`). A pushing
# hook has no other way to tell a mis-addressed pane from a correctly addressed one — the
# toast is the only other evidence and it is transient. The tab half is the point: a space
# can hold many agents, so the space name alone does not answer "which one is asking me".
grep -q '/' "$WORK/out" || fail "notify did not report which tab it was attributed to: $(cat "$WORK/out")"
COPAD_MUX_PANE="$token0" t 10 "$COMUX" rename-tab "e2e-tab" >/dev/null || fail "rename-tab failed"
t 10 "$COMUX" notify --pane "$token0" --kind done "named" >"$WORK/out" \
    || fail "notify after rename-tab failed"
grep -q '/e2e-tab$' "$WORK/out" || fail "a named tab must title the notification: $(cat "$WORK/out")"
# Clearing the name falls back to the 1-based index — the `Ctrl-b <n>` that jumps there,
# which is more useful than repeating the space name on every row.
COPAD_MUX_PANE="$token0" t 10 "$COMUX" rename-tab "" >/dev/null || fail "clearing the tab name failed"
t 10 "$COMUX" notify --pane "$token0" --kind done "unnamed" >"$WORK/out" \
    || fail "notify after clearing the tab name failed"
grep -qE '/tab [0-9]+$' "$WORK/out" || fail "an unnamed tab must fall back to its index: $(cat "$WORK/out")"
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
ok "token minted per pane; notify validated + reports its space/tab attribution; jump switched sessions by identity"

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

echo "10. wait-output blocks until a pane's text matches"
# The marker is ASSEMBLED IN THE PANE from fragments: the sent command contains "MARK-%s"
# and the id separately, never "MARK-<id>" as a literal. Otherwise the shell's echo of the
# command satisfies the wait before the command has run, and this step would pass while
# testing nothing. The id is fresh per run so a leftover match can't satisfy it either.
ID="$$-$RANDOM-$(date +%s)"
t 10 "$COMUX" send 0 "sleep 1; printf 'MARK-%s\n' '$ID'" >/dev/null
t 10 "$COMUX" send 0 $'\n' >/dev/null
# Wait on the pane EXPLICITLY (the same one we sent to), not on whatever is focused.
t 30 "$COMUX" wait-output "$tok" --timeout 20 --interval 100 "MARK-$ID" >"$WORK/wait" \
    || fail "wait-output did not see MARK-$ID: $(cat "$WORK/wait")"
grep -q "MARK-$ID" "$WORK/wait" || fail "wait-output printed no matching line"
# A pattern that never appears must hit the deadline (124), not hang the harness.
start=$(date +%s)
set +e
t 20 "$COMUX" wait-output --timeout 2 --interval 100 "NEVER_APPEARS_$ID" >/dev/null 2>&1; code=$?
set -e
elapsed=$(( $(date +%s) - start ))
[[ "$code" == "124" ]] || fail "expected timeout exit 124, got $code"
(( elapsed <= 8 )) || fail "the deadline did not fire promptly (${elapsed}s)"
# Usage refusals, including the --timeout overflow that used to panic with exit 101.
set +e
t 10 "$COMUX" wait-output --interval 0 X >/dev/null 2>&1; c1=$?
t 10 "$COMUX" wait-output >/dev/null 2>&1; c2=$?
t 10 "$COMUX" wait-output --timeout 18446744073709551615 X >/dev/null 2>&1; c3=$?
set -e
[[ "$c1" == "2" ]] || fail "--interval 0 should be usage exit 2, got $c1"
[[ "$c2" == "2" ]] || fail "a missing pattern should be usage exit 2, got $c2"
[[ "$c3" == "2" ]] || fail "an overflowing --timeout should be usage exit 2, got $c3 (101 = panic)"
# An unknown pane fails rather than waiting out the clock.
set +e
start=$(date +%s)
t 15 "$COMUX" wait-output no-such-pane X --timeout 10 >/dev/null 2>&1; c4=$?
elapsed=$(( $(date +%s) - start ))
set -e
[[ "$c4" == "1" ]] || fail "an unknown pane should fail with 1, got $c4"
(( elapsed <= 5 )) || fail "an unknown pane was waited out instead of refused (${elapsed}s)"
# Run the recipe `comux wait-output --help` PRINTS, verbatim. The help is there to be
# pasted, and it has already shipped broken once (Rust string escaping turned $'\n' into a
# literal backslash-n, so the command was never submitted) — a test that only reads the
# source cannot catch that.
id=$(date +%s%N)
t 10 "$COMUX" send 0 "printf 'MARK-%s\n' '$id'" >/dev/null
t 10 "$COMUX" send 0 $'
' >/dev/null
t 30 "$COMUX" wait-output --index 0 --timeout 20 --interval 100 "MARK-$id" >/dev/null     || fail "the recipe printed by --help does not work as written"
ok "matched a pane-assembled fresh marker; timed out at 124; overflow + refusals honored; \
documented recipe runs"

echo "11. list-agents / wait-agent"
# With no agent running, a listing is an EMPTY LIST, not an error — `wait-agent` branches on
# exactly that distinction (empty = "not classified as an agent yet", keep waiting).
t 10 "$COMUX" list-agents --json >"$WORK/json"
[[ "$(count agents)" == "0" ]] || fail "expected no agents on a fresh server"
python3 -c 'import json,sys; a=json.load(sys.stdin)["agents"]; sys.exit(0 if a==[] else 1)' <"$WORK/json" \
    || fail "an empty listing must be [] (not null) so it differs from a non-listing response"
# A pane is classified as an agent purely by its foreground process NAME, so the fixture has
# to be a real executable that reports `claude` — a shell script would report the
# INTERPRETER's name instead. It is a SYMLINK rather than a copy: macOS kills a copied
# Apple-signed binary on exec (the copy's signature no longer validates), while a symlink
# executes the original and still reports the LINK's name. This proves the classification +
# listing + polling plumbing; it says nothing about real-agent status accuracy, which no
# hermetic harness can check.
mkdir -p "$WORK/bin"
ln -sf /bin/sleep "$WORK/bin/claude" || fail "could not build the agent fixture"
# Give the pane a status the heuristic can actually read. A bare `sleep` shows no agent UI,
# so it resolves to `idle` — which is deliberately NOT waitable (it also means "no reading").
# Printing "esc to interrupt" first is, incidentally, a live demonstration of the caveat the
# --help states: ordinary output can be read as an agent status.
t 10 "$COMUX" send 0 "printf 'esc to interrupt\n'; $WORK/bin/claude 600" >/dev/null
t 10 "$COMUX" send 0 $'\n' >/dev/null
seen=""
for _ in $(seq 1 40); do
    t 10 "$COMUX" list-agents --json >"$WORK/json"
    [[ "$(count agents)" != "0" ]] && { seen=1; break; }
    sleep 0.25
done
if [[ -z "$seen" ]]; then
    echo "--- diagnostics ---" >&2
    t 10 "$COMUX" list >&2 || true
    t 10 "$COMUX" capture-pane -S 40 >&2 || true
    fail "the fixture pane was never classified as an agent"
fi
atok="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["agents"][0]["token"])' <"$WORK/json")"
astat="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["agents"][0]["status"])' <"$WORK/json")"
[[ "$atok" == "$tok" ]] || fail "list-agents reported token '$atok', expected '$tok'"
[[ "$astat" == "working" ]] || fail "expected the fixture to read as 'working', got '$astat'"
# Waiting for the status it IS in returns at once (level-triggered, as documented).
t 15 "$COMUX" wait-agent "$tok" --status working --timeout 10 >"$WORK/out" \
    || fail "wait-agent did not match the agent's current status"
grep -q working "$WORK/out" || fail "wait-agent printed no status line"
# Waiting for a status it is NOT in must reach the deadline, not hang and not fail early.
start=$(date +%s)
set +e
t 20 "$COMUX" wait-agent "$tok" --status blocked --timeout 2 --interval 100 >/dev/null 2>&1; code=$?
set -e
elapsed=$(( $(date +%s) - start ))
[[ "$code" == "124" ]] || fail "wait-agent on a non-matching status should time out (124), got $code"
(( elapsed <= 8 )) || fail "wait-agent did not honor its deadline (${elapsed}s)"
# Refusals: an absent pane fails fast; unwaitable/unknown statuses and a missing target are usage errors.
set +e
start=$(date +%s)
t 15 "$COMUX" wait-agent no-such-pane --status ready --timeout 10 >/dev/null 2>&1; c1=$?
elapsed=$(( $(date +%s) - start ))
t 10 "$COMUX" wait-agent "$tok" --status idle >/dev/null 2>&1; c2=$?
t 10 "$COMUX" wait-agent "$tok" --status done >/dev/null 2>&1; c3=$?
t 10 "$COMUX" wait-agent --status ready >/dev/null 2>&1; c4=$?
set -e
[[ "$c1" == "1" ]] || fail "an absent pane should fail with 1, got $c1"
(( elapsed <= 5 )) || fail "an absent pane was waited out instead of refused (${elapsed}s)"
[[ "$c2" == "2" ]] || fail "--status idle should be usage exit 2, got $c2"
[[ "$c3" == "2" ]] || fail "an unknown --status should be usage exit 2, got $c3"
[[ "$c4" == "2" ]] || fail "a missing target should be usage exit 2, got $c4"
# Put the pane back to a shell so the shutdown step is not reaping a 600s sleep.
t 10 "$COMUX" send 0 $'\x03' >/dev/null
ok "empty listing is []; fixture classified; level-match, deadline, and refusals honored"

echo "12. the embedded agent skill, and the recipe it teaches"
t 10 "$COMUX" skill >"$WORK/skill.md" || fail "comux skill failed"
[[ "$(head -1 "$WORK/skill.md")" == "---" ]] || fail "the skill has no YAML frontmatter"
grep -q 'COPAD_MUX' "$WORK/skill.md" || fail "the skill lost its inside-comux gate"
(( $(wc -l <"$WORK/skill.md") > 50 )) || fail "the skill looks truncated"
# Run the recipe the SKILL teaches, as it teaches it. A skill is a document the model
# FOLLOWS, so the only honest check is that following it works.
#
# Deliberately NOT against pane 0: the first version used a disposable pane 0 and so could
# not have caught the recipe's real defect — a hard-coded index, which on a one-pane tab is
# the AGENT'S OWN pane. The split names the pane it created, so nothing is inferred.
sib="$(t 10 "$COMUX" split --from "$tok")" || fail "split --from failed"
[[ -n "$sib" && "$sib" != "$tok" ]] || fail "split did not name a NEW pane (got '$sib')"
id=$(date +%s%N)
t 10 "$COMUX" send "$sib" "printf 'DONE-%s\n' '$id'" >/dev/null
t 10 "$COMUX" send "$sib" $'\n' >/dev/null
t 30 "$COMUX" wait-output "$sib" "DONE-$id" --timeout 20 --interval 100 >/dev/null \
    || fail "the recipe the skill teaches does not work"
# The pane we split FROM must not have received it — the defect a disposable pane 0 hid.
t 10 "$COMUX" capture-pane "$tok" -S 60 >"$WORK/orig"
grep -q "DONE-$id" "$WORK/orig" && fail "the recipe typed into the source pane, not the new one"
# `split --from` must refuse an identity it cannot resolve, and refuse a raw terminal id
# (recycled across restarts) rather than splitting something arbitrary.
t 10 "$COMUX" split --from no-such-pane >/dev/null 2>&1 && fail "split --from should refuse an unknown pane"
ok_split_panes="$(panes_now)"
# A token-addressed send crosses sessions WITHOUT moving the user's view — that is the whole
# point of the addressing change, and the property most likely to regress silently.
t 10 "$COMUX" new-session e2e-send >/dev/null
t 10 "$COMUX" list-sessions --json >"$WORK/json"
act_before="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["active_session"])' <"$WORK/json")"
xid=$(date +%s%N)
t 10 "$COMUX" send "$sib" "printf 'XS-%s\n' '$xid'" >/dev/null
t 10 "$COMUX" send "$sib" $'\n' >/dev/null
t 30 "$COMUX" wait-output "$sib" "XS-$xid" --timeout 20 --interval 100 >/dev/null \
    || fail "a token-addressed send did not reach a pane in another session"
t 10 "$COMUX" list-sessions --json >"$WORK/json"
act_after="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["active_session"])' <"$WORK/json")"
[[ "$act_before" == "$act_after" ]] \
    || fail "a cross-session send moved the active session ($act_before -> $act_after)"
t 10 "$COMUX" kill-session "$act_after" >/dev/null || fail "could not clean up the send session"
# Refusals: a terminal id is not an address for a WRITE; no target at all is a usage error.
t 10 "$COMUX" send term0 "x" >/dev/null 2>&1 && fail "send should refuse a raw terminal id"
set +e
t 10 "$COMUX" send >/dev/null 2>&1; c=$?
set -e
[[ "$c" == "2" ]] || fail "a target-less send should be usage exit 2, got $c"
# Index addressing must still work exactly as before.
t 10 "$COMUX" list --json >"$WORK/json"
sib_i="$(SIB="$sib" python3 -c 'import json,sys,os; d=json.load(sys.stdin); print(next(p["index"] for p in d["panes"] if p["token"]==os.environ["SIB"]))' <"$WORK/json")"
iid=$(date +%s%N)
t 10 "$COMUX" send "$sib_i" "printf 'IDX-%s\n' '$iid'" >/dev/null
t 10 "$COMUX" send "$sib_i" $'\n' >/dev/null
t 30 "$COMUX" wait-output "$sib" "IDX-$iid" --timeout 20 --interval 100 >/dev/null \
    || fail "index-addressed send regressed"
t 10 "$COMUX" close "$sib_i" >/dev/null || fail "could not clean up the split"
ok "skill recipe runs on a server-named pane; token send crosses sessions without moving the view; index send unchanged"

echo "13. bell and title, the two pane events comux used to drop"
# Must be an UNFOCUSED pane. The focused one is acknowledged on the next frame (that is the
# rule — a bell you are looking at is a bell you saw), so testing it would assert nothing.
# This headless server has NO client attached, so nothing is acknowledged at all here, which
# is itself the detached half of the contract.
bell_pane="$(t 10 "$COMUX" split --from "$tok")" || fail "split --from failed"
t 10 "$COMUX" focus 0 >/dev/null || fail "could not focus away from the bell pane"
t 10 "$COMUX" send "$bell_pane" "printf '\a'" >/dev/null
t 10 "$COMUX" send "$bell_pane" $'\n' >/dev/null
rang=""
for _ in $(seq 1 40); do
    t 10 "$COMUX" list --json >"$WORK/json"
    if BP="$bell_pane" python3 -c 'import json,sys,os; print(any(p["bell"] for p in json.load(sys.stdin)["panes"] if p["token"]==os.environ["BP"]))' <"$WORK/json" | grep -q True; then
        rang=1; break
    fi
    sleep 0.2
done
[[ -n "$rang" ]] || fail "a BEL in an unfocused pane was not reported"
# A title set via OSC 0 must show up WITHOUT displacing the process label — the label comes
# from the foreground process sweep, which is the more trustworthy of the two.
# `; sleep 8` matters: the shell rewrites the title on every PROMPT (zsh/bash commonly set
# it to the cwd), so a title set by a command that then returns is overwritten before the
# poll can see it. That is exactly why titles are NOT used as pane labels — the process
# sweep is the more trustworthy source — and the test has to hold the shell to observe one.
t 10 "$COMUX" send "$bell_pane" "printf '\033]0;e2e-title\a'; sleep 8" >/dev/null
t 10 "$COMUX" send "$bell_pane" $'\n' >/dev/null
titled=""
for _ in $(seq 1 40); do
    t 10 "$COMUX" list --json >"$WORK/json"
    if BP="$bell_pane" python3 -c 'import json,sys,os
d=json.load(sys.stdin)
p=next(p for p in d["panes"] if p["token"]==os.environ["BP"])
print(p["title"]=="e2e-title" and p["label"]!="e2e-title")' <"$WORK/json" | grep -q True; then
        titled=1; break
    fi
    sleep 0.2
done
[[ -n "$titled" ]] || fail "the pane title was not reported, or it displaced the process label"
bi="$(BP="$bell_pane" python3 -c 'import json,sys,os; print(next(p["index"] for p in json.load(sys.stdin)["panes"] if p["token"]==os.environ["BP"]))' <"$WORK/json")"
t 10 "$COMUX" close "$bi" >/dev/null || fail "could not clean up the bell pane"
ok "an unfocused pane's BEL is reported; its OSC 0 title is exposed without displacing the label"

echo "14. a codex pane is classified, not left at 'idle'"
# Regression guard for the bug this step exists for: comux read an agent pane's status with
# markers that only knew Claude's UI, so a codex pane reported `idle` BOTH when parked at its
# composer and when blocked on an approval — and `idle` is deliberately not waitable, so
# codex was invisible to `wait-agent`, the attention count and the blocked toast alike.
#
# The fixture RENDERS codex's real screen text (captured verbatim from codex-cli 0.154.0)
# rather than running codex: a real agent needs credentials, costs quota and is not
# deterministic. This checks the wiring end to end — process name -> classification ->
# marker resolution -> the wire -> wait-agent — which the unit tests cannot reach.
#
# The screens are written to FILES here and `cat`-ed in the pane. Typing them as a `printf`
# argument made the test pass for the wrong reason: the shell ECHOES the command line, so the
# marker was on screen whether or not the command ever ran — the same trap `wait-output
# --help` warns callers about, hit by its own harness.
mkdir -p "$WORK/bin"
ln -sf /bin/sleep "$WORK/bin/codex" || fail "could not build the codex fixture"
printf '\xe2\x80\xba Ask Codex to do anything\n  Luna Reserve medium \xc2\xb7 /private/tmp\n' \
    >"$WORK/codex-ready.txt"
printf 'Would you like to run the following command?\n\xe2\x80\xba 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)\nPress enter to confirm or esc to cancel\n' \
    >"$WORK/codex-blocked.txt"
cx="$(t 10 "$COMUX" split --from "$tok")" || fail "split for the codex fixture failed"
[[ -n "$cx" ]] || fail "split did not name the codex pane"

# Parked at its composer: the cursor glyph is codex's U+203A, not Claude's U+276F.
t 10 "$COMUX" send "$cx" "clear; cat $WORK/codex-ready.txt; $WORK/bin/codex 600" >/dev/null
t 10 "$COMUX" send "$cx" $'\n' >/dev/null
t 40 "$COMUX" wait-agent "$cx" --status ready --timeout 25 --interval 200 >"$WORK/out" \
    || { t 10 "$COMUX" capture-pane "$cx" -S 20 >&2 || true
         fail "a codex pane parked at its composer was not reported ready (the pre-fix bug: idle)"; }
grep -q ready "$WORK/out" || fail "wait-agent printed no status line for the codex fixture"

# Blocked on an approval: the dialog replaces the composer, and its prose is what promotes the
# pane past "not ready" to a positive "blocked".
t 10 "$COMUX" send "$cx" $'\x03' >/dev/null
t 10 "$COMUX" send "$cx" "clear; cat $WORK/codex-blocked.txt; $WORK/bin/codex 600" >/dev/null
t 10 "$COMUX" send "$cx" $'\n' >/dev/null
t 40 "$COMUX" wait-agent "$cx" --status blocked --timeout 25 --interval 200 >/dev/null \
    || { t 10 "$COMUX" capture-pane "$cx" -S 20 >&2 || true
         fail "a codex pane on an approval prompt was not reported blocked (the pre-fix bug: idle)"; }
t 10 "$COMUX" send "$cx" $'\x03' >/dev/null
t 10 "$COMUX" list --json >"$WORK/json"
cxi="$(CX="$cx" python3 -c 'import json,sys,os; print(next(p["index"] for p in json.load(sys.stdin)["panes"] if p["token"]==os.environ["CX"]))' <"$WORK/json")"
t 10 "$COMUX" close "$cxi" >/dev/null || fail "could not clean up the codex pane"
ok "codex reads ready at its composer and blocked on an approval, not idle"

echo "15. an agent's activity line is read from its own log"
# End-to-end for the `agentpoll` thread: pane label -> the wanted-set handed to the poller ->
# source-file resolution (which forks `lsof` on macOS, off the render loop) -> bounded tail
# read -> parse -> the wire. None of that is reachable from a unit test.
#
# The CODEX path is the one that can be faked honestly: it locates the log by finding a
# rollout file the agent process holds OPEN, so a fixture that is named `codex` and holds one
# open exercises the real resolution. (Claude's path derives its transcript from
# `~/.claude/sessions/<pid>.json`, a file the harness does not own, so it is covered by unit
# tests plus an ignored live oracle — `cargo test -p copad-mux -- --ignored live_`.)
mkdir -p "$WORK/bin2" "$WORK/sessions"
roll="$WORK/sessions/rollout-2026-09-12T00-00-00-01a09154-4f7e-76a1-81e0-daae1f6d49b3.jsonl"
# A real codex `CommandExecution`: `command` is an ARRAY and codex pre-parses it into
# `parsed_cmd`. Reading `command` as a string is a bug a hand-written fixture hid once.
cat >"$roll" <<'ROLLEOF'
{"type":"response_item","timestamp":"2026-09-12T00:00:01.000Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"run the tests"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test --all\"}"}}
{"type":"response_item","payload":{"type":"function_call_output","output":"tests ok \u001b]0;pwned\u0007 done"}}
{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning"}}}
{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":["/bin/zsh","-lc","cargo test --all"],"parsed_cmd":[{"type":"unknown","cmd":"cargo test --all"}]}}}
ROLLEOF
# The SECOND conversation, written up front: the fixture must be ONE process that rotates its
# own rollout, not two processes. Killing the fixture and starting another changes the agent
# PID, which retires the cache entry and resolves from scratch — a rotation test built that way
# passes with the fix reverted, which is exactly what the first draft of this step did.
roll2="$WORK/sessions/rollout-2026-09-12T01-00-00-01a09154-4f7e-76a1-81e0-daae1f6d49b4.jsonl"
cat >"$roll2" <<'ROLLEOF'
{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":["/bin/zsh","-lc","git bisect run"],"parsed_cmd":[{"type":"unknown","cmd":"git bisect run"}]}}}
ROLLEOF
dx="$(t 10 "$COMUX" split --from "$tok")" || fail "split for the activity fixture failed"
# The fixture is `codex` symlinked to PYTHON, holding its own rollout open and rotating it on
# cue. Three shapes this went through, each ruled out by something real:
#   * NOT `exec`-ing it: the pane's shell has to survive the Ctrl-C below, or the pane closes
#     itself and the cleanup has nothing to address.
#   * NOT `codex -c 'exec 9< …; sleep 600'` with `codex` a symlink to `/bin/sh`. That only held
#     on macOS, whose `/bin/sh` is bash 3.2. Where `/bin/sh` is a modern bash or dash, the shell
#     EXEC-OPTIMISES the last command of a `-c` list, so the process replaces itself with
#     `sleep` and its `comm` stops being `codex` — the fd survives, the IDENTITY does not, and
#     the sweep never classifies the pane as an agent at all.
#   * NOT the pane's shell holding the descriptor and a `sleep`-symlink inheriting it. That is
#     fine for reading ONE conversation, but an inherited fd is a copy: the agent process can
#     never rotate it, and rotating by killing and restarting the fixture changes the agent PID
#     — which retires the cache entry and resolves from scratch, so the test passes with the fix
#     reverted. (It did. That draft is why this comment exists.)
# Python holds its own rollout, swaps it in place, and optimises nothing away — which is also
# what real codex does.
cat >"$WORK/rotate.py" <<'PYEOF'
import os, sys, time
live = open(sys.argv[1])
while not os.path.exists(sys.argv[3]):
    time.sleep(0.25)
# Exactly what `codex /new` and `claude /clear` do: close the old log, open a new one, and
# leave the old one sitting on disk.
live.close()
live = open(sys.argv[2])
time.sleep(600)
PYEOF
ln -sf "$(command -v python3)" "$WORK/bin2/codex" || fail "could not build the rotating fixture"
t 10 "$COMUX" send "$dx" "$WORK/bin2/codex $WORK/rotate.py $roll $roll2 $WORK/switch" >/dev/null
t 10 "$COMUX" send "$dx" $'\n' >/dev/null
seen=""
for _ in $(seq 1 60); do
    t 10 "$COMUX" list-agents --json >"$WORK/json"
    if DX="$dx" python3 -c 'import json,sys,os
a=[x for x in json.load(sys.stdin)["agents"] if x["token"]==os.environ["DX"]]
sys.exit(0 if a and a[0].get("detail") else 1)' <"$WORK/json"; then
        seen=1; break
    fi
    sleep 0.5
done
[[ -n "$seen" ]] || { t 10 "$COMUX" list-agents >&2 || true
                      fail "no activity was ever read from the fixture's rollout"; }
det="$(DX="$dx" python3 -c 'import json,sys,os; print(next(x["detail"] for x in json.load(sys.stdin)["agents"] if x["token"]==os.environ["DX"]))' <"$WORK/json")"
[[ "$det" == "running: cargo test --all" ]] \
    || fail "expected the NEWEST rollout item, got '$det'"
# The conversation itself is exposed too: its id and the file it is logged to.
DX="$dx" ROLL="$roll" python3 -c 'import json,sys,os
a=next(x for x in json.load(sys.stdin)["agents"] if x["token"]==os.environ["DX"])
assert os.path.realpath(a["transcript_path"])==os.path.realpath(os.environ["ROLL"]), a
assert a["agent_session_id"]=="01a09154-4f7e-76a1-81e0-daae1f6d49b3", a' <"$WORK/json" \
    || fail "list-agents did not expose the conversation id + transcript: $(cat "$WORK/json")"
# ...and `agent-log` reads that conversation back, normalized, oldest first.
t 10 "$COMUX" agent-log "$dx" --last 2 --json >"$WORK/log.json" || fail "agent-log failed: $(cat "$WORK/log.json")"
python3 -c 'import json,sys; d=json.load(sys.stdin); e=d["entries"]
assert d["tool"]=="codex" and d["partial"] is False, d
assert [(x["role"],x["kind"],x.get("name")) for x in e]==[("assistant","tool_use","exec_command"),("tool","tool_result",None)], e
assert e[0]["text"]=="cargo test --all", e' <"$WORK/log.json" \
    || fail "agent-log --json shape: $(cat "$WORK/log.json")"
t 10 "$COMUX" agent-log "$dx" >"$WORK/log.txt" || fail "agent-log (human) failed"
grep -q "user      text: run the tests" "$WORK/log.txt" || fail "agent-log (human) misses the prompt: $(cat "$WORK/log.txt")"
LC_ALL=C grep -q $'\x1b' "$WORK/log.txt" && fail "agent-log printed a raw ESC from a tool result"
rc=0; t 10 "$COMUX" agent-log "$tok" >/dev/null 2>&1 || rc=$?
[[ "$rc" == 1 ]] || fail "agent-log on a non-agent pane should exit 1, got $rc"
# Absence must stay absent: a pane with no readable log carries no `detail` key at all, so a
# caller can tell "no reading" from "doing nothing" (the same contract as #105's [] vs null).
if python3 -c 'import json,sys
sys.exit(0 if any("detail" in x for x in json.load(sys.stdin)["agents"] if x["tool"]!="codex") else 1)' <"$WORK/json"; then
    fail "a pane with no readable log must omit detail entirely, not send an empty one"
fi
# The conversation can change INSIDE the same pid — `codex /new` rotates to a fresh rollout,
# `claude /clear` mints a new sessionId — and in both cases the PREVIOUS log stays on disk. A
# resolver that re-resolves only when its cached file has VANISHED therefore never fires, and
# republishes the old conversation forever.
touch "$WORK/switch"
# Deliberately longer than SOURCE_RECHECK (30s): resolving a codex source forks `lsof`, which
# decision #88 bars from the sweep cadence, so the rotation is picked up on a timer rather than
# at once. Waiting it out is also the assertion that the timer is a DELAY and not a ceiling.
rotated=""
for _ in $(seq 1 100); do
    t 10 "$COMUX" list-agents --json >"$WORK/json"
    det="$(DX="$dx" python3 -c 'import json,sys,os
a=[x for x in json.load(sys.stdin)["agents"] if x["token"]==os.environ["DX"]]
print(a[0].get("detail","") if a else "")' <"$WORK/json")"
    [[ "$det" == "running: git bisect run" ]] && { rotated=1; break; }
    sleep 0.5
done
[[ -f "$roll" ]] || fail "the first rollout must survive the rotation, or this proves nothing"
[[ -n "$rotated" ]] || fail "the activity line never followed the new conversation: '$det'"
DX="$dx" python3 -c 'import json,sys,os
a=next(x for x in json.load(sys.stdin)["agents"] if x["token"]==os.environ["DX"])
assert a["agent_session_id"]=="01a09154-4f7e-76a1-81e0-daae1f6d49b4", a' <"$WORK/json" \
    || fail "the conversation id did not follow the rotation: $(cat "$WORK/json")"
t 10 "$COMUX" send "$dx" $'\x03' >/dev/null
t 10 "$COMUX" list --json >"$WORK/json"
dxi="$(DX="$dx" python3 -c 'import json,sys,os; print(next(p["index"] for p in json.load(sys.stdin)["panes"] if p["token"]==os.environ["DX"]))' <"$WORK/json")"
t 10 "$COMUX" close "$dxi" >/dev/null || fail "could not clean up the activity pane"
ok "the newest rollout item became the DOING line, followed a new conversation in the same pane, and a pane with no log omits it"

echo "16. the host readout"
# `comux host` is served from the poller's last reading, so it must answer immediately and
# must never invent a value. The CPU percentage is a RATE between two samples, so it is the
# one field that is legitimately absent right after boot — hence the retry rather than a
# single assertion.
t 10 "$COMUX" host --json >"$WORK/json" || fail "comux host failed"
python3 -c 'import json,sys; h=json.load(sys.stdin)["host"]; sys.exit(0 if isinstance(h,dict) else 1)' \
    <"$WORK/json" || fail "comux host returned no host object"
got=""
for _ in $(seq 1 20); do
    t 10 "$COMUX" host --json >"$WORK/json"
    if python3 -c 'import json,sys
h=json.load(sys.stdin)["host"]
ok=[k for k in ("cpu","mem_used","load1") if k in h]
sys.exit(0 if ok else 1)' <"$WORK/json"; then
        got=1; break
    fi
    sleep 0.5
done
[[ -n "$got" ]] || fail "the host poller never published a reading: $(cat "$WORK/json")"
# Ranges, because a bad probe is far likelier to produce a nonsense number than an error.
python3 -c 'import json,sys
h=json.load(sys.stdin)["host"]
for k in ("cpu","gpu"):
    if k in h and not (0.0 <= h[k] <= 100.0):
        print(f"{k} out of range: {h[k]}"); sys.exit(1)
if "mem_used" in h and "mem_total" in h and not (0 < h["mem_used"] <= h["mem_total"]):
    print("memory is not a share of the machine:", h); sys.exit(1)
if "load1" in h and h["load1"] < 0:
    print("negative load:", h); sys.exit(1)' <"$WORK/json" || fail "implausible host reading"
# Absence is absence: a field that could not be read is OMITTED, so the plain readout has no
# line for it at all rather than a zero or a dash.
t 10 "$COMUX" host >"$WORK/out" || fail "plain host readout failed"
grep -qE '^(cpu|mem|gpu|load1) ' "$WORK/out" || fail "plain host readout printed nothing usable"
ok "host metrics are published, in range, and omit what could not be read"

echo "17. the top bar, and what it does to every coordinate below it"
# The top bar is a LAYOUT change: it pushes the pane grid, the sidebar and every sidebar click
# zone down a row. The rendering is only checkable by looking at a real frame, and the click
# zones only by clicking — so this step drives a real client over a pty.
#
# The offsets are easy to half-apply: the first version of this change moved the sidebar's
# DRAWING but only two of its four click zones, which reads perfectly and sends clicks to the
# wrong session.
# NOT named `pty.py`: a script with that name SHADOWS the standard-library `pty` module it
# imports, and the failure is `module 'pty' has no attribute 'fork'` from inside its own
# import line.
cat >"$WORK/ptydrive.py" <<'PYEOF'
import os, pty, sys, time, select, re, fcntl, termios, struct
# capture:  pty.py cap  -- cmd...        -> prints the final frame
# click:    pty.py <row> <col> -- cmd... -> clicks (1-based screen coords), then detaches
mode = sys.argv[1]
rows, cols = 24, 100
cmd = sys.argv[sys.argv.index("--") + 1:]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp(cmd[0], cmd)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
def drain(sec):
    out = b""; end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try: c = os.read(fd, 65536)
            except OSError: break
            if not c: break
            out += c
    return out
buf = drain(6)
if mode != "cap":
    row, col = int(sys.argv[1]), int(sys.argv[2])
    os.write(fd, f"\x1b[<0;{col};{row}M".encode()); time.sleep(0.2)
    os.write(fd, f"\x1b[<0;{col};{row}m".encode())
    drain(3)
os.write(fd, b"\x02d")
time.sleep(0.8)
try: os.close(fd)
except OSError: pass
try: os.waitpid(pid, 0)
except ChildProcessError: pass
if mode != "cap":
    sys.exit(0)
grid = [[" "] * cols for _ in range(rows)]
cy = cx = 0
text = buf.decode("utf-8", "replace"); i = 0
while i < len(text):
    ch = text[i]
    if ch == "\x1b":
        m = re.match(r"\x1b\[([0-9;?]*)([A-Za-z])", text[i:])
        if m:
            params, fin = m.group(1), m.group(2)
            if fin == "H":
                p = [int(x) for x in params.split(";") if x.isdigit()] or [1, 1]
                cy = p[0] - 1 if p else 0
                cx = p[1] - 1 if len(p) > 1 else 0
            elif fin == "J" and params in ("2", ""):
                grid = [[" "] * cols for _ in range(rows)]
            i += m.end(); continue
        m2 = re.match(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", text[i:])
        if m2:
            i += m2.end(); continue
        i += 1; continue
    if ch == "\r": cx = 0; i += 1; continue
    if ch == "\n": cy += 1; cx = 0; i += 1; continue
    if 0 <= cy < rows and 0 <= cx < cols: grid[cy][cx] = ch
    cx += 1; i += 1
for r in grid:
    print("".join(r).rstrip())
PYEOF

active_session () { t 10 "$COMUX" list-sessions | awk '/active/{print $3}'; }

# Baseline: OFF by default, so an existing layout does not shrink on upgrade.
python3 "$WORK/ptydrive.py" cap -- "$COMUX" >"$WORK/frame" 2>/dev/null
# NOTE `grep -q … && fail …` cannot be used here: when grep does not match — the SUCCESS
# case — the expression returns 1 and `set -e` kills the run with no message at all.
if head -1 "$WORK/frame" | grep -qE '^ *(cpu|mem|gpu|load) '; then
    fail "the top bar must be OFF by default, but row 0 carries a host readout"
fi

echo "top_bar = true" >"$XDG_CONFIG_HOME/copad/mux.toml"
t 10 "$COMUX" reload >/dev/null || fail "reload failed"
sleep 1
python3 "$WORK/ptydrive.py" cap -- "$COMUX" >"$WORK/frame" 2>/dev/null
head -1 "$WORK/frame" | grep -qE '(cpu|mem|gpu|load) ' \
    || fail "no host readout on row 0 after enabling it: $(head -1 "$WORK/frame")"
# It must not simply OVERWRITE the first content row — the sidebar has to have moved down.
sed -n '2p' "$WORK/frame" | grep -q 'spaces' \
    || fail "the sidebar did not move below the top bar: $(sed -n '2p' "$WORK/frame")"

# Now the part rendering cannot show: the sidebar's CLICK ZONES have to have moved too.
# Two sessions, and the click lands on the SECOND one's subtitle row — deliberately not its
# title row, which sits on a zone boundary where an off-by-one still resolves correctly and
# the test would pass against the bug.
t 10 "$COMUX" new-session e2etop >/dev/null || fail "could not create the second session"
t 10 "$COMUX" select-session 0 >/dev/null
python3 "$WORK/ptydrive.py" cap -- "$COMUX" >"$WORK/frame" 2>/dev/null
row="$(python3 - "$WORK/frame" <<'PYEOF'
import sys
lines = open(sys.argv[1]).read().splitlines()
for i, l in enumerate(lines):
    if "e2etop" in l:
        print(i + 2)  # 1-based, and the row AFTER the title is its branch subtitle
        break
else:
    print(0)
PYEOF
)"
[[ "$row" != "0" ]] || { cat "$WORK/frame" >&2; fail "could not find the second session in the sidebar"; }
before="$(active_session)"
python3 "$WORK/ptydrive.py" "$row" 5 -- "$COMUX" >/dev/null 2>&1
sleep 1
after="$(active_session)"
[[ "$before" != "$after" ]] \
    || { cat "$WORK/frame" >&2
         fail "clicking the second session's row (screen row $row) did not switch to it"; }

# A terminal too short to spare the row keeps its panes whatever the config says.
sed -i.bak 's/rows, cols = 24, 100/rows, cols = 6, 100/' "$WORK/ptydrive.py"
python3 "$WORK/ptydrive.py" cap -- "$COMUX" >"$WORK/frame" 2>/dev/null
if head -1 "$WORK/frame" | grep -qE '^ *(cpu|mem|gpu|load) '; then
    fail "the top bar must be suppressed on a terminal too short for it"
fi
sed -i.bak 's/rows, cols = 6, 100/rows, cols = 24, 100/' "$WORK/ptydrive.py"

rm -f "$XDG_CONFIG_HOME/copad/mux.toml"
t 10 "$COMUX" reload >/dev/null
ok "off by default; drawn and reloadable; the sidebar and its click zones moved with it; suppressed when short"

echo "18. dragging a split divider"
# The keyboard nudge (`Ctrl-b H/J/K/L`) moves a ratio by a fixed step; a POINTER has to land
# the divider under itself, which is a different operation (absolute, not relative) and is only
# checkable by actually dragging. The helper from step 17 already drives a real client, so this
# reuses it with motion events.
#
# The branch is addressed by PATH, not by an adjacent pane: `SplitTree::resize` resolves
# deepest-first, so a pane inside an outer branch's `first` child can also sit inside a NESTED
# branch on the same axis, and dragging the outer divider would move the inner one. The nested
# case below is what pins that.
pane_cols () {
    t 10 "$COMUX" list --json | python3 -c 'import json,sys
print(" ".join(str(p["cols"]) for p in json.load(sys.stdin)["panes"]))'
}
pane_rows () {
    t 10 "$COMUX" list --json | python3 -c 'import json,sys
print(" ".join(str(p["rows"]) for p in json.load(sys.stdin)["panes"]))'
}
drag () { # drag <from> <to> <fixed-other-axis> <vertical?>
    FROM="$1" TO="$2" FIX="$3" VERT="$4" CMX="$COMUX" python3 - <<'PYEOF' >/dev/null 2>&1
import os, pty, time, select, fcntl, termios, struct
rows, cols = 24, 100
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp(os.environ["CMX"], [os.environ["CMX"]])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
def drain(s):
    end = time.time() + s
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                if not os.read(fd, 65536): break
            except OSError: break
drain(6)
a, b, fix = int(os.environ["FROM"]), int(os.environ["TO"]), int(os.environ["FIX"])
vert = os.environ["VERT"] == "1"
def at(v):
    return (fix, v) if vert else (v, fix)
c, r = at(a)
os.write(fd, f"\x1b[<0;{c};{r}M".encode()); time.sleep(0.2)
step = 1 if b > a else -1
for v in range(a + step, b + step, step):
    c, r = at(v)
    os.write(fd, f"\x1b[<32;{c};{r}M".encode()); time.sleep(0.04)
c, r = at(b)
os.write(fd, f"\x1b[<0;{c};{r}m".encode())
drain(2)
os.write(fd, b"\x02d"); time.sleep(0.8)
try: os.close(fd)
except OSError: pass
try: os.waitpid(pid, 0)
except ChildProcessError: pass
PYEOF
}

first_tok="$(t 10 "$COMUX" list --json | python3 -c 'import json,sys
print(json.load(sys.stdin)["panes"][0]["token"])')"
t 10 "$COMUX" split --from "$first_tok" >/dev/null || fail "could not split for the drag test"
sleep 1
before_cols="$(pane_cols)"
# Find the vertical divider on screen rather than computing it, so the test also proves the
# rendered gap and the grab target are the same cell.
python3 "$WORK/ptydrive.py" cap -- "$COMUX" >"$WORK/frame" 2>/dev/null
dcol="$(python3 - "$WORK/frame" <<'PYEOF'
import sys
rows = open(sys.argv[1]).read().splitlines()
# The sidebar's own border is the LEFTmost such column; the pane divider is the next one.
cands = {}
for line in rows[2:20]:
    for i, ch in enumerate(line):
        if ch == "│":
            cands[i] = cands.get(i, 0) + 1
tall = sorted(c for c, n in cands.items() if n > 8)
print(tall[1] + 1 if len(tall) > 1 else 0)
PYEOF
)"
[[ "$dcol" != "0" ]] || { cat "$WORK/frame" >&2; fail "no pane divider was drawn to grab"; }
drag "$dcol" $((dcol + 18)) 3 0
sleep 1
after_cols="$(pane_cols)"
[[ "$before_cols" != "$after_cols" ]] \
    || fail "dragging the divider changed nothing ($before_cols)"
python3 - "$before_cols" "$after_cols" <<'PYEOF' || fail "the drag did not move the divider the way it was dragged"
import sys
b = [int(x) for x in sys.argv[1].split()]
a = [int(x) for x in sys.argv[2].split()]
assert a[0] > b[0], f"the left pane should have GROWN: {b} -> {a}"
assert a[1] < b[1], f"the right pane should have shrunk: {b} -> {a}"
assert sum(a) == sum(b), f"cells must be conserved: {b} -> {a}"
PYEOF

# A NESTED branch on the other axis: dragging its divider must move IT, not the outer one.
t 10 "$COMUX" split -v --from "$first_tok" >/dev/null || fail "could not nest a split"
sleep 1
outer_before="$(pane_cols)"
rows_before="$(pane_rows)"
drag 12 18 $((dcol / 2)) 1
sleep 1
[[ "$(pane_rows)" != "$rows_before" ]] \
    || fail "dragging the nested horizontal divider changed no heights ($rows_before)"
[[ "$(pane_cols)" == "$outer_before" ]] \
    || fail "dragging the NESTED divider also moved the outer one ($outer_before -> $(pane_cols))"

# Dragged past the edge, a pane must not collapse to nothing and strand its shell.
drag 18 23 $((dcol / 2)) 1
sleep 1
python3 - "$(pane_rows)" <<'PYEOF' || fail "a divider drag collapsed a pane"
import sys
assert all(int(v) > 0 for v in sys.argv[1].split()), sys.argv[1]
PYEOF
# A drag must not survive the view moving under it. Input is SHARED, so the active tab can
# change while a client holds a divider — and a ROUND TRIP is the case that defeats every
# "are we still on the tab we grabbed in" check, because it restores each field while the
# pointer has moved somewhere unrelated.
#
# Two round trips, run SEPARATELY because each masks the other: selecting a tab away and back,
# and creating a tab (which activates it) then closing it (which promotes a neighbour). The
# first version ran them together and the trailing `select-tab` bumped the generation for both,
# so the create/close path passed with its cancellation removed.
cat >"$WORK/dragdisturb.py" <<'PYEOF'
import os, pty, time, select, fcntl, termios, struct, subprocess, sys, threading
rows, cols = 24, 100
a, b = int(os.environ["DRAG_FROM"]), int(os.environ["DRAG_TO"])
disturb = [x.split(",") for x in os.environ["DISTURB"].split(";") if x]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp(os.environ["CMX"], [os.environ["CMX"]])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
# Drain CONTINUOUSLY, on its own thread, for the whole life of the client.
#
# Draining only BETWEEN writes is what this did first, and it made the test measure the wrong
# thing. The client writes to its terminal synchronously, so an undrained pty fills and blocks
# it mid-write — and a client blocked in `write` is not reading our input either. The mouse-DOWN
# then reaches the server AFTER the disturbance instead of before it, a fresh drag is started
# against the new view, and the release moves the divider: the test reports "a drag survived the
# view moving" when what actually happened is that the drag began afterwards. A real terminal
# always drains, so a reader thread models production rather than starving it. This surfaced
# when `reconcile_secs` started sending a periodic silent repaint — i.e. simply more output, so
# any future change that emits more would have broken it the same way.
alive = True
def reader():
    while alive:
        try:
            r, _, _ = select.select([fd], [], [], 0.1)
            if r and not os.read(fd, 65536):
                return
        except OSError:
            return
rt = threading.Thread(target=reader, daemon=True)
rt.start()
def drain(s):   # nothing to pump any more: the reader thread owns the fd. Just let time pass.
    time.sleep(s)
drain(6)
os.write(fd, f"\x1b[<0;{a};3M".encode()); time.sleep(0.3)
for argv in disturb:                       # another client moves the view, button still DOWN
    r = subprocess.run([os.environ["CMX"]] + argv, capture_output=True)
    if r.returncode != 0:
        print(f"disturb {argv} failed: {r.stderr!r}", file=sys.stderr)
        sys.exit(3)
    time.sleep(0.4)
drain(1)
os.write(fd, f"\x1b[<32;{b};3M".encode()); time.sleep(0.2)
os.write(fd, f"\x1b[<0;{b};3m".encode())
drain(2)
os.write(fd, b"\x02d"); time.sleep(0.8)
alive = False
rt.join(timeout=1)
try: os.close(fd)
except OSError: pass
try: os.waitpid(pid, 0)
except ChildProcessError: pass
PYEOF

find_divider () {
    python3 "$WORK/ptydrive.py" cap -- "$COMUX" >"$WORK/frame" 2>/dev/null
    python3 - "$WORK/frame" <<'PYEOF'
import sys
rows = open(sys.argv[1]).read().splitlines()
cands = {}
for line in rows[2:20]:
    for i, ch in enumerate(line):
        if ch == "│":
            cands[i] = cands.get(i, 0) + 1
tall = sorted(c for c, n in cands.items() if n > 8)
print(tall[1] + 1 if len(tall) > 1 else 0)
PYEOF
}

# Each case re-locates the divider: the drags above MOVED it, and clicking empty space starts
# no drag at all — which is how an earlier version of this test passed against the bug.
check_view_moved () { # check_view_moved <label> <disturb-spec>
    local d; d="$(find_divider)"
    [[ "$d" != "0" ]] || { cat "$WORK/frame" >&2; fail "no divider to grab for: $1"; }
    local rb cb; rb="$(pane_rows)"; cb="$(pane_cols)"
    DRAG_FROM="$d" DRAG_TO=$((d - 15)) DISTURB="$2" CMX="$COMUX" \
        python3 "$WORK/dragdisturb.py" >/dev/null 2>&1 \
        || fail "the disturbance itself failed for: $1"
    sleep 1
    [[ "$(pane_cols)" == "$cb" && "$(pane_rows)" == "$rb" ]] \
        || fail "a drag survived $1: $cb/$rb -> $(pane_cols)/$(pane_rows)"
}

# (a) create + close, with ONE tab, so closing promotes us back without a `select-tab` that
#     would bump the generation on its own and mask this path entirely.
# `close-tab` with no index opens an interactive picker, so the index is explicit: the tab
# `new-tab` just created is the last one, index 1 while only tab 0 existed.
check_view_moved "a tab being created and closed" "new-tab;close-tab,1"

# (b) select away and back, which needs a second tab to exist.
t 10 "$COMUX" new-tab >/dev/null || fail "could not create a second tab"
t 10 "$COMUX" select-tab 0 >/dev/null || fail "could not return to the first tab"
sleep 1
check_view_moved "the tab being selected away and back" "select-tab,1;select-tab,0"

ok "a dragged divider lands under the pointer, conserves cells, addresses the right branch, clamps, and dies when the view moves"

echo "19. a jump into a copad tab"
# `winfocus` can only activate an APPLICATION — a pid names the emulator, not one of its tabs
# — so a toast click lands on whichever copad tab happened to be active. Copad exports
# `COPAD_SOCKET` and `COPAD_PANEL_ID` into every tab's shell, so a client running inside one
# can name its exact tab; comux asks copad to focus it.
#
# Driven against a FAKE copad speaking the same line-JSON protocol. That covers comux's whole
# half — the client handshake, the server bookkeeping, the response, the dial and the reply
# parse. copad's own `panel.focus` handler is covered separately: the GTK one was driven against
# a real copad window on Linux (decision #115), the AppKit one on macOS.
cat >"$WORK/fakecopad.py" <<'PYEOF'
import json, os, socket, sys, threading, time
# mode: 1 = focused and raised, 0 = did not own the panel, 2 = focused but cannot raise
# itself (GTK under Wayland), which must still fall through to the compositor-level raise.
path, log, mode = sys.argv[1], sys.argv[2], sys.argv[3]
focused, raised = mode in ("1", "2"), mode == "1"
try: os.unlink(path)
except FileNotFoundError: pass
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(path); srv.listen(4)
def serve():
    while True:
        c, _ = srv.accept()
        f = c.makefile("rwb")
        try: req = json.loads(f.readline())
        except Exception: req = {}
        with open(log, "a") as fh: fh.write(json.dumps(req) + "\n")
        f.write((json.dumps({"id": req.get("id"), "ok": True,
                             "result": {"ok": True, "focused": focused,
                                        "raised": raised}}) + "\n").encode())
        f.flush(); c.close()
threading.Thread(target=serve, daemon=True).start()
time.sleep(120)
PYEOF
cat >"$WORK/attach.py" <<'PYEOF'
import os, pty, time, select, fcntl, termios, struct, sys
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp(sys.argv[1], ["comux"])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
end = time.time() + float(sys.argv[2])
while time.time() < end:
    r, _, _ = select.select([fd], [], [], 0.2)
    if r:
        try:
            if not os.read(fd, 65536): break
        except OSError: break
PYEOF

start_copad () { # start_copad <focused?>
    rm -f "$WORK/gui.sock" "$WORK/copadcalls.log"
    python3 "$WORK/fakecopad.py" "$WORK/gui.sock" "$WORK/copadcalls.log" "$1" >/dev/null 2>&1 &
    COPAD_FAKE_PID=$!
    sleep 1
}
attach_as_copad_tab () {
    COPAD_SOCKET="$WORK/gui.sock" COPAD_PANEL_ID="e2e-panel" \
        python3 "$WORK/attach.py" "$COMUX" 25 >/dev/null 2>&1 &
    COPAD_CLIENT_PID=$!
    sleep 5
}

start_copad 1
attach_as_copad_tab
# The response must NAME the tab. Without this the CLI has nothing to dial and silently
# degrades to application activation — which looks identical until you have two copad tabs.
t 10 "$COMUX" jump "$tok" --json >"$WORK/out" || fail "jump failed while attached"
python3 -c 'import json,sys
r = json.load(sys.stdin)
h = r.get("copad_host")
assert h and h[1] == "e2e-panel", r' <"$WORK/out" \
    || fail "the jump response did not name the copad tab: $(cat "$WORK/out")"
t 10 "$COMUX" jump "$tok" >/dev/null || fail "plain jump failed"
sleep 1
python3 -c 'import json,sys
line = open(sys.argv[1]).readline()
req = json.loads(line)
assert req.get("method") == "panel.focus", req
assert req.get("params", {}).get("panel_id") == "e2e-panel", req' "$WORK/copadcalls.log" \
    || fail "comux did not ask copad to focus its panel: $(cat "$WORK/copadcalls.log" 2>/dev/null)"

# A copad that does not own the panel answers `focused: false`. That is a SUCCESSFUL call
# that found nothing, and the CLI must fall through to the generic raise rather than treat it
# as done — otherwise the user is left looking at the wrong window.
kill "$COPAD_FAKE_PID" 2>/dev/null || true
start_copad 0
t 10 "$COMUX" jump "$tok" >/dev/null || fail "jump failed against a copad that refused focus"
[[ -s "$WORK/copadcalls.log" ]] || fail "the refusing copad was never called"

# A copad that focused the tab but could NOT raise itself — GTK under Wayland, where a client
# with no activation token has its `present()` accepted and ignored — must also fall through to
# the compositor-level raise. Suppressing it there trades the old bug (right window, wrong tab)
# for a worse one: the right tab, still behind whatever was frontmost, so the click looks inert.
kill "$COPAD_FAKE_PID" 2>/dev/null || true
start_copad 2
t 10 "$COMUX" jump "$tok" >/dev/null || fail "jump failed against a copad that cannot raise"
[[ -s "$WORK/copadcalls.log" ]] || fail "the non-raising copad was never called"

# A dead copad must not fail the jump, hang it, or print anything: the pane switch already
# happened server-side, and raising a window is best-effort by construction.
kill "$COPAD_FAKE_PID" 2>/dev/null || true
rm -f "$WORK/gui.sock"
t 10 "$COMUX" jump "$tok" >"$WORK/out" 2>&1 || fail "a dead copad must not fail the jump"
kill "$COPAD_CLIENT_PID" 2>/dev/null || true
sleep 1
# Once that client detaches, the panel id must be FORGOTten — a departed client's tab must
# not be focused by a later jump.
t 10 "$COMUX" jump "$tok" --json >"$WORK/out" || fail "jump failed after the client left"
python3 -c 'import json,sys
r = json.load(sys.stdin)
assert "copad_host" not in r, r' <"$WORK/out" \
    || fail "a detached client left its copad tab behind: $(cat "$WORK/out")"
ok "the jump names the copad tab, dials it, falls back when refused, unraised or dead, and forgets it on detach"

echo "20. the SSH fleet readout"
# One comux server per machine, reached by running `comux list-agents --json` there over
# ordinary ssh. Driven against a FAKE `ssh` on PATH, which covers everything except OpenSSH
# itself: config parsing, the remote command line, concurrency, and — the point of the whole
# feature — that a machine which does NOT answer is reported rather than dropped.
mkdir -p "$WORK/fleetbin"
cat >"$WORK/fleetbin/ssh" <<'SSHEOF'
#!/usr/bin/env bash
dest="${@: -2:1}"
cmd="${@: -1}"
echo "$dest :: $cmd" >> "$FAKE_SSH_LOG"
case "$dest" in
  me@build.local)
    echo '{"ok":true,"agents":[{"space":"api","tool":"claude","status":"blocked","for_secs":240,"detail":"Bash: run the tests","token":"t1"}]}' ;;
  gpu-box)
    echo "ssh: connect to host gpu-box port 22: Operation timed out" >&2; exit 255 ;;
  quiet-box)
    echo '{"ok":true,"agents":[]}' ;;
  big-box)
    # A reply far larger than a pipe buffer (~64 KiB). The first version of `query_one` left
    # both pipes undrained until the child exited, so ssh blocked in `write`, never exited,
    # and a perfectly reachable machine was killed at the deadline and reported as timed out —
    # failing precisely where the feature earns its keep, since the more agents a machine has
    # the bigger its reply.
    python3 - <<'PYBIG'
import json
pad = "x" * 300
print(json.dumps({"ok": True, "agents": [
    {"space": "s%d" % i, "tool": "claude", "status": "ready",
     "for_secs": i, "detail": pad, "token": "t%d" % i}
    for i in range(1200)]}))
PYBIG
    ;;
  *) echo '{"ok":false,"error":"no server running"}' ;;
esac
SSHEOF
chmod +x "$WORK/fleetbin/ssh"
cat >"$XDG_CONFIG_HOME/copad/mux.toml" <<'TOMLEOF'
[machines.build]
ssh = "me@build.local"
[machines.gpu]
ssh = "gpu-box"
socket = "/tmp/alt sock"
[machines.quiet]
ssh = "quiet-box"
[machines.big]
ssh = "big-box"
[machines.broken]
TOMLEOF
export FAKE_SSH_LOG="$WORK/ssh.log"
: >"$FAKE_SSH_LOG"
FLEET_PATH="$WORK/fleetbin:$PATH"

# `machines` must work with NO server involved — that is exactly when you want to ask where
# your agents are.
PATH="$FLEET_PATH" t 10 "$COMUX" machines >"$WORK/out" 2>"$WORK/err" || fail "comux machines failed"
grep -q 'me@build.local' "$WORK/out" || fail "machines did not list the fleet: $(cat "$WORK/out")"
# A machine with no ssh destination is DROPPED WITH A WARNING, never silently defaulted to its
# own name — a silent default would query the wrong host.
grep -q "broken" "$WORK/err" || fail "an unusable machine was dropped silently: $(cat "$WORK/err")"
grep -q "broken" "$WORK/out" && fail "an unusable machine must not be listed as usable"

set +e
PATH="$FLEET_PATH" t 30 "$COMUX" list-agents --fleet >"$WORK/out" 2>"$WORK/err"; code=$?
set -e
# A partial fleet is a partial answer: the reachable machines are still reported, and the exit
# code says the picture is incomplete so a script need not re-parse the text to find out.
[[ "$code" == "1" ]] || fail "an unreachable machine must make the fleet readout exit 1, got $code"
grep -q 'Bash: run the tests' "$WORK/out" || fail "the reachable machine's agent was not shown: $(cat "$WORK/out")"
grep -q 'UNREACHABLE' "$WORK/out" || fail "the unreachable machine was DROPPED instead of reported: $(cat "$WORK/out")"
grep -q 'Operation timed out' "$WORK/out" || fail "the unreachable machine gave no reason: $(cat "$WORK/out")"
# "no agents" and "could not reach" must be visibly different rows.
grep -q 'no agents' "$WORK/out" || fail "an idle machine must say so, not vanish: $(cat "$WORK/out")"

# The remote command line: a socket path with a SPACE must survive the remote login shell.
grep -q "COPAD_MUX_SOCK='/tmp/alt sock' comux list-agents --json" "$WORK/ssh.log" \
    || fail "the remote command did not quote the socket path: $(cat "$WORK/ssh.log")"
[[ "$(grep -c '::' "$WORK/ssh.log")" == "4" ]] \
    || fail "expected exactly 4 machines queried, got: $(cat "$WORK/ssh.log")"
# A reply bigger than a pipe buffer must ARRIVE, not deadlock into a bogus timeout.
grep -q 'big .*claude' "$WORK/out" \
    || fail "a large reply was not read back — the pipes are not drained while waiting: $(grep big "$WORK/out")"
grep -qE 'big +UNREACHABLE' "$WORK/out" \
    && fail "a large reply was reported unreachable — the pipe deadlock is back"

set +e
PATH="$FLEET_PATH" t 30 "$COMUX" list-agents --fleet --json >"$WORK/out" 2>/dev/null; code=$?
set -e
python3 -c 'import json,sys
d = json.load(sys.stdin)
assert d["ok"] is False, d
ms = d["machines"]
assert ms["build"]["agents"][0]["status"] == "blocked", ms
assert ms["quiet"]["agents"] == [], ms
assert len(ms["big"]["agents"]) == 1200, len(ms["big"].get("agents", []))
# Unreachable is its OWN key, not an empty agent list — a consumer must not be able to read a
# failure as an idle machine.
assert "unreachable" in ms["gpu"] and "agents" not in ms["gpu"], ms' <"$WORK/out" \
    || fail "the json fleet readout conflates unreachable with idle: $(cat "$WORK/out")"

# A machine whose ssh DESCENDANT holds the pipes open must still be abandoned at the deadline.
# Killing ssh does not close a write end its `ProxyCommand` inherited, so collecting the
# readers by joining them waits for the PROXY, not for our deadline: measured 60.8s against a
# 60s proxy where the budget was 10s. Uses the real ssh, with a proxy that never connects.
cat >"$XDG_CONFIG_HOME/copad/mux.toml" <<'TOMLEOF'
[machines.stuck]
ssh = "stuck-host"
TOMLEOF
cat >"$WORK/fleetbin/ssh" <<'SSHEOF'
#!/usr/bin/env bash
exec /usr/bin/ssh -F /dev/null -o BatchMode=yes -o "ProxyCommand=sleep 45" "$@"
SSHEOF
chmod +x "$WORK/fleetbin/ssh"
start=$(date +%s)
set +e
PATH="$FLEET_PATH" t 60 "$COMUX" list-agents --fleet >"$WORK/out" 2>/dev/null
set -e
elapsed=$(( $(date +%s) - start ))
grep -q 'timed out' "$WORK/out" || fail "a stalled proxy should time out: $(cat "$WORK/out")"
# The budget is 10s. Anything near the proxy's 45s means the readers are being waited on.
(( elapsed <= 20 )) \
    || fail "a descendant holding the pipes held the whole fleet for ${elapsed}s — the readers are not deadline-bounded"

# A machine that prints its REASON and then hangs holding the pipe open must still report the
# reason, not a bare deadline. The readers therefore publish as bytes arrive rather than only
# on EOF — otherwise "Permission denied" dies inside the abandoned reader thread and the user
# is told only that something timed out.
cat >"$XDG_CONFIG_HOME/copad/mux.toml" <<'TOMLEOF'
[machines.noisy]
ssh = "noisy-host"
TOMLEOF
cat >"$WORK/fleetbin/ssh" <<'SSHEOF'
#!/usr/bin/env bash
echo "Permission denied (publickey)." >&2
sleep 45
SSHEOF
chmod +x "$WORK/fleetbin/ssh"
set +e
PATH="$FLEET_PATH" t 60 "$COMUX" list-agents --fleet >"$WORK/out" 2>/dev/null
set -e
grep -q 'Permission denied' "$WORK/out" \
    || fail "a timeout threw away the reason the machine gave: $(cat "$WORK/out")"
grep -q 'timed out' "$WORK/out" || fail "the timeout itself went unreported: $(cat "$WORK/out")"

rm -f "$XDG_CONFIG_HOME/copad/mux.toml"
unset FAKE_SSH_LOG
ok "the fleet is queried in parallel; an unreachable machine is reported with a reason, never dropped; a stalled descendant cannot hold up the deadline"

echo "21. a HIDDEN pane's bell still reaches the rendered status bar"
# Only a VISIBLE pane's output recomposes the frame now (`App::drain_pane_dirty`): with dozens
# of panes, one background agent's spinner used to drive the whole visible screen to recompose
# at the frame rate. That filter is only sound because every visible value that is a live
# function of a HIDDEN pane gained its own comparison on the label cadence — and a bell is one
# of them (`App::refresh_bells`), since `bell()`/`bell_count()` read each pane's atomic counter
# at render time.
#
# So this asserts the exact combination: ring a bell in a pane on another TAB (truly off
# screen, unlike step 13's same-tab split, and unlike step 13 this reads a REAL rendered frame
# rather than `list --json`) and require the tab chip's `!` to appear. Without `refresh_bells`
# the bell has no way to reach the frame at all once hidden panes stop dirtying it.
#
# The bell has to ring while a client is ALREADY ATTACHED, and with no control traffic of its
# own. Both attaching and any mutating `ctl` verb dirty the frame by themselves, so a bell rung
# before the client connects — or delivered by a `send` — would repaint for reasons that have
# nothing to do with this fix, and the check would pass with `refresh_bells` deleted. Hence the
# self-timed bell: arm it in the pane, hide the pane, then attach and wait it out.
hidden_tok="$(t 10 "$COMUX" split -h)" || fail "could not create the pane that will ring"
t 10 "$COMUX" send "$hidden_tok" "(sleep 3; printf '\a') &" >/dev/null
t 10 "$COMUX" send "$hidden_tok" $'\n' >/dev/null
# Move it out of sight: a new tab becomes the active one, so everything on the old tab is
# hidden from the composed frame.
t 10 "$COMUX" new-tab >/dev/null || fail "new-tab failed"
# `cap` attaches, drains for 6s, then detaches — so the bell lands mid-drain and the label
# cadence has seconds to turn it into a repaint.
python3 "$WORK/ptydrive.py" cap -- "$COMUX" >"$WORK/frame" 2>/dev/null
if ! tail -1 "$WORK/frame" | grep -q '!'; then
    fail "a bell rung in a hidden tab never reached the status bar: $(tail -1 "$WORK/frame")"
fi
ok "a hidden tab's bell still repaints the chrome, without its output driving the frame"

echo "22. an idle client silently repaints itself, and never clears to do it"
# The render self-heal (`reconcile_secs`), verified where it has to work: a REAL client on a pty.
#
# Residue lives BELOW the client's buffer — ratatui's incremental diff believes a cell is already
# correct while the terminal shows something else — so no delta can ever repair it and the user
# ends up pressing `Ctrl-b r`. The fix is to re-emit the whole screen periodically. Nothing here
# can corrupt a real terminal's memory from outside, so what is asserted is the contract that
# makes the fix work: while NOTHING is happening the client keeps re-emitting the screen, and it
# does so WITHOUT a clear. The missing clear is the whole reason this can ship on by default —
# `COPAD_MUX_REDRAW_MS` did the same job for a year with `Clear(All)` in front of it and had to
# stay off because it flashed a blank frame every tick.
cat >"$WORK/idlewatch.py" <<'PYEOF'
import os, pty, sys, time, select, fcntl, termios, struct
# Settle, then watch an IDLE client for `window` seconds and report what it emitted.
window = float(sys.argv[1])
rows, cols = 24, 100
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp(os.environ["CMX"], [os.environ["CMX"]])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
def read_for(sec):
    out = b""; end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try: c = os.read(fd, 65536)
            except OSError: break
            if not c: break
            out += c
    return out
read_for(6)                      # attach, first full frame, agent sweep — all the startup churn
watched = read_for(window)       # nothing is touching this session now
os.write(fd, b"\x02d"); time.sleep(0.8)
try: os.close(fd)
except OSError: pass
try: os.waitpid(pid, 0)
except ChildProcessError: pass
txt = watched.decode("utf-8", "replace")
# `local` is the session name, drawn in the status-bar pill and the sidebar: a repaint re-emits
# the whole screen, so it comes back around every time.
clears = sum(txt.count(c) for c in ("\x1b[2J", "\x1b[3J", "\x1b[1J", "\x1b[0J", "\x1b[J"))
print(f"bytes={len(watched)} marker={txt.count('local')} clears={clears}")
PYEOF

echo "reconcile_secs = 1" >"$XDG_CONFIG_HOME/copad/mux.toml"
t 10 "$COMUX" reload >/dev/null || fail "could not turn the self-heal on"
on="$(CMX="$COMUX" t 40 python3 "$WORK/idlewatch.py" 4)" || fail "idlewatch failed with the self-heal on"
echo "reconcile_secs = 0" >"$XDG_CONFIG_HOME/copad/mux.toml"
t 10 "$COMUX" reload >/dev/null || fail "could not turn the self-heal off"
off="$(CMX="$COMUX" t 40 python3 "$WORK/idlewatch.py" 4)" || fail "idlewatch failed with the self-heal off"
rm -f "$XDG_CONFIG_HOME/copad/mux.toml"
t 10 "$COMUX" reload >/dev/null || fail "could not restore the default config"

python3 - "$on" "$off" <<'PYEOF' || fail "the idle self-heal did not behave: on=[$on] off=[$off]"
import sys
def parse(s):
    return {k: int(v) for k, v in (p.split("=") for p in s.split())}
on, off = parse(sys.argv[1]), parse(sys.argv[2])
# ON: the screen comes back repeatedly while nothing at all is happening.
assert on["marker"] >= 2, f"an idle client should keep re-emitting the screen: {on}"
# ...and never by clearing it first. This is the flicker-free property, not a detail.
assert on["clears"] == 0, f"a silent repaint must not clear the screen: {on}"
# OFF: nothing repaints, so the same idle window is near-silent. Compared as a ratio rather
# than against zero because a status-bar HH:MM rollover can legitimately land in the window.
assert off["marker"] == 0 or off["bytes"] * 4 < on["bytes"], (
    f"with reconcile_secs = 0 an idle client should stay quiet: on={on} off={off}"
)
print(f"on={on} off={off}")
PYEOF
ok "an idle client re-emits its screen on a timer, with no clear; reconcile_secs = 0 keeps it silent"

echo "23. --help / unknown flags / extra arguments never reach the server"
s_before="$(sessions_now)"
t 10 "$COMUX" new-session --help >"$WORK/out" || fail "new-session --help should exit 0"
grep -q "usage: comux new-session" "$WORK/out" || fail "new-session --help printed no usage: $(cat "$WORK/out")"
[[ "$(sessions_now)" == "$s_before" ]] || fail "new-session --help created a session"
rc=0; t 10 "$COMUX" new-session --bogus >/dev/null 2>&1 || rc=$?
[[ "$rc" == 2 ]] || fail "new-session --bogus: expected exit 2, got $rc"
[[ "$(sessions_now)" == "$s_before" ]] || fail "new-session --bogus created a session"
t 10 "$COMUX" list-tabs --json >"$WORK/tabs-before"
t 10 "$COMUX" rename-tab --help >/dev/null || fail "rename-tab --help should exit 0"
t 10 "$COMUX" list-tabs --json >"$WORK/tabs-after"
cmp -s "$WORK/tabs-before" "$WORK/tabs-after" || fail "rename-tab --help renamed a tab"
for args in "list --bogus" "list-sessions --bogus" "list-agents --owner x" "list extra" "health extra"; do
    rc=0; t 10 "$COMUX" $args >/dev/null 2>&1 || rc=$?
    [[ "$rc" == 2 ]] || fail "comux $args: expected exit 2, got $rc"
done
# `--json` under a usage error still answers in JSON on stdout.
rc=0; t 10 "$COMUX" list --bogus --json >"$WORK/out" 2>/dev/null || rc=$?
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is False and d["code"]=="usage", d' \
    <"$WORK/out" || fail "a --json usage error did not print a JSON error: $(cat "$WORK/out")"
# A body after `--` is literal, including the global `--json` switch.
t 10 "$COMUX" list --json >"$WORK/json"
tok="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["panes"][0]["token"])' <"$WORK/json")"
t 10 "$COMUX" send "$tok" -- --json-literal-e2e >/dev/null || fail "send -- --json-literal-e2e failed"
sleep 0.5
t 10 "$COMUX" capture-pane "$tok" >"$WORK/out"
grep -q -- "--json-literal-e2e" "$WORK/out" || fail "the text after -- was not sent verbatim"
t 10 "$COMUX" send "$tok" $'\x15' >/dev/null # clear the line again (Ctrl-U)
ok "help exits 0 and creates nothing; bad flags exit 2 (JSON under --json); -- passes text through"

echo "24. an index-less rename targets the caller's tab and session"
t 10 "$COMUX" list --json >"$WORK/json"
tokA="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["panes"][0]["token"])' <"$WORK/json")"
t 10 "$COMUX" new-session wu2-viewed >/dev/null || fail "could not create the viewed session"
# The user now looks at `wu2-viewed`; the caller's pane lives in the other session.
COPAD_MUX_PANE="$tokA" t 10 "$COMUX" rename-tab callers-tab >/dev/null || fail "caller rename-tab failed"
COPAD_MUX_PANE="$tokA" t 10 "$COMUX" rename-session callers-space >/dev/null || fail "caller rename-session failed"
t 10 "$COMUX" list-tabs --json >"$WORK/json"
python3 -c 'import json,sys; t=json.load(sys.stdin)["tabs"]; assert all(x["name"]=="" for x in t), t' <"$WORK/json" \
    || fail "the VIEWED session's tab was renamed: $(cat "$WORK/json")"
t 10 "$COMUX" list-sessions --json >"$WORK/json"
python3 - "$WORK/json" <<'PYEOF' || fail "rename-session hit the wrong session: $(cat "$WORK/json")"
import json, sys
s = json.load(open(sys.argv[1]))["sessions"]
names = {x["name"]: x["active"] for x in s}
assert names.get("callers-space") is False, s   # renamed, and NOT the one being viewed
assert names.get("wu2-viewed") is True, s       # the viewed session kept its name
PYEOF
t 10 "$COMUX" agents --json >/dev/null # (smoke) the server still answers
t 10 "$COMUX" list-sessions --json >"$WORK/json"
ia="$(python3 -c 'import json,sys; print([x["index"] for x in json.load(sys.stdin)["sessions"] if x["name"]=="callers-space"][0])' <"$WORK/json")"
t 10 "$COMUX" select-session "$ia" >/dev/null
t 10 "$COMUX" list-tabs --json >"$WORK/json"
grep -q '"name":"callers-tab"' "$WORK/json" || fail "the caller's tab was not renamed: $(cat "$WORK/json")"
rc=0; t 10 "$COMUX" rename-tab nowhere >/dev/null 2>&1 || rc=$?
[[ "$rc" == 2 ]] || fail "rename-tab outside a pane should exit 2, got $rc"
rc=0; COPAD_MUX_PANE="deadbeef-999" t 10 "$COMUX" rename-tab ghost >"$WORK/out" 2>&1 || rc=$?
[[ "$rc" == 1 ]] || fail "rename-tab from an unknown pane should fail (1), got $rc: $(cat "$WORK/out")"
t 10 "$COMUX" list-sessions --json >"$WORK/json"
iv="$(python3 -c 'import json,sys; print([x["index"] for x in json.load(sys.stdin)["sessions"] if x["name"]=="wu2-viewed"][0])' <"$WORK/json")"
t 10 "$COMUX" kill-session "$iv" >/dev/null || fail "cleanup kill-session failed"
COPAD_MUX_PANE="$tokA" t 10 "$COMUX" rename-session "" >/dev/null
COPAD_MUX_PANE="$tokA" t 10 "$COMUX" rename-tab "" >/dev/null
ok "the caller's tab and session were renamed while another was viewed; outside a pane exits 2"

echo "25. session/tab selectors, and create verbs that say what they made"
s25="$(sessions_now)"
t 10 "$COMUX" new-session sel-a --json >"$WORK/json" || fail "new-session --json failed"
python3 - "$WORK/json" <<'PYEOF' || fail "new-session --json did not name what it made: $(cat "$WORK/json")"
import json, sys
d = json.load(open(sys.argv[1]))
assert d["ok"] and d["session"]["name"] == "sel-a" and d["session"]["id"].startswith("s"), d
assert d["tab"]["id"].startswith(d["session"]["id"] + ":t"), d
assert "-" in d["pane"], d
PYEOF
sa="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["session"]["id"])' <"$WORK/json")"
t 10 "$COMUX" new-tab --json >"$WORK/json" || fail "new-tab --json failed"
ta="$(python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["session"]["id"]=="'"$sa"'", d; print(d["tab"]["id"])' <"$WORK/json")" \
    || fail "new-tab --json did not name its tab in the current session: $(cat "$WORK/json")"
t 10 "$COMUX" rename-tab -t "$ta" picked >/dev/null || fail "rename-tab -t <tab id> failed"
# Two sessions with ONE name: a name selector must refuse to guess.
t 10 "$COMUX" new-session twin >/dev/null
t 10 "$COMUX" new-session twin >/dev/null
rc=0; t 10 "$COMUX" select-session name:twin >"$WORK/out" 2>&1 || rc=$?
[[ "$rc" == 2 ]] || fail "an ambiguous session name should exit 2, got $rc: $(cat "$WORK/out")"
grep -q "matches 2" "$WORK/out" || fail "the ambiguity error should list the candidates: $(cat "$WORK/out")"
rc=0; t 10 "$COMUX" select-session --json name:twin >"$WORK/out" 2>/dev/null || rc=$?
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["code"]=="ambiguous", d' <"$WORK/out" \
    || fail "--json ambiguity should carry code=ambiguous: $(cat "$WORK/out")"
rc=0; t 10 "$COMUX" select-session name:nope >/dev/null 2>&1 || rc=$?
[[ "$rc" == 1 ]] || fail "an unknown session name should exit 1, got $rc"
# By name and by path: the view follows, and the named tab is the active one.
t 10 "$COMUX" select-session name:sel-a >/dev/null || fail "select-session name:sel-a failed"
t 10 "$COMUX" list-sessions --json >"$WORK/json"
python3 -c 'import json,sys; s=json.load(sys.stdin)["sessions"]; assert [x for x in s if x["active"]][0]["name"]=="sel-a", s' <"$WORK/json" \
    || fail "select-session name: did not switch: $(cat "$WORK/json")"
t 10 "$COMUX" select-tab "sel-a/picked" >/dev/null || fail "select-tab <session>/<tab name> failed"
# list-tabs --session reads ANOTHER session without switching to it.
t 10 "$COMUX" list-tabs --session "name:sel-a" --json >"$WORK/json"
python3 -c 'import json,sys; t=json.load(sys.stdin)["tabs"]; assert [x for x in t if x["active"]][0]["name"]=="picked", t' <"$WORK/json" \
    || fail "list-tabs --session did not show the selected tab: $(cat "$WORK/json")"
t 10 "$COMUX" select-session 0 >/dev/null
t 10 "$COMUX" list-tabs --session "$sa" --json >"$WORK/json"
[[ "$(count tabs)" == 2 ]] || fail "list-tabs --session $sa should list 2 tabs: $(cat "$WORK/json")"
# Close a tab of a BACKGROUND session by id; the viewed session is untouched.
v_before="$(tabs_now)"
t 10 "$COMUX" close-tab "$ta" >/dev/null || fail "close-tab <tab id> in a background session failed"
[[ "$(tabs_now)" == "$v_before" ]] || fail "closing a background tab changed the viewed session"
t 10 "$COMUX" list-tabs --session "$sa" --json >"$WORK/json"
[[ "$(count tabs)" == 1 ]] || fail "the background tab was not closed: $(cat "$WORK/json")"
for sel in "$sa" twin twin; do
    t 10 "$COMUX" list-sessions --json >"$WORK/json"
    id="$(python3 -c 'import json,sys; print([x["id"] for x in json.load(sys.stdin)["sessions"] if x["id"]=="'"$sel"'" or x["name"]=="'"$sel"'"][0])' <"$WORK/json")"
    t 10 "$COMUX" kill-session "$id" >/dev/null || fail "kill-session $id (by id) failed"
done
[[ "$(sessions_now)" == "$s25" ]] || fail "selector cleanup left sessions behind: $(cat "$WORK/json")"
ok "ids/names/paths resolve; ambiguous exits 2 with candidates; background tab closed by id; create verbs return ids"

echo "26. list-tree, and where list-agents says an agent is"
t 10 "$COMUX" list-tree --json >"$WORK/tree.json" || fail "list-tree --json failed"
t 10 "$COMUX" list-sessions --json >"$WORK/sessions.json"
python3 - "$WORK/tree.json" "$WORK/sessions.json" <<'PYEOF' || fail "list-tree disagrees with list-sessions: $(cat "$WORK/tree.json")"
import json, sys
tree = json.load(open(sys.argv[1]))["tree"]
sessions = json.load(open(sys.argv[2]))["sessions"]
assert [s["id"] for s in tree] == [s["id"] for s in sessions], (tree, sessions)
for t, s in zip(tree, sessions):
    assert len(t["tabs"]) == s["tabs"], (t, s)
    assert sum(len(x["panes"]) for x in t["tabs"]) == s["panes"], (t, s)
    for tab in t["tabs"]:
        assert tab["id"].startswith(t["id"] + ":t"), tab
        assert sum(p["focused"] for p in tab["panes"]) == 1, tab
        assert all("-" in p["token"] for p in tab["panes"]), tab
assert sum(s["active"] for s in tree) == 1, tree
PYEOF
first="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["tree"][0]["id"])' <"$WORK/tree.json")"
t 10 "$COMUX" list-tree --session "$first" --json >"$WORK/json"
[[ "$(count tree)" == 1 ]] || fail "list-tree --session should return one session: $(cat "$WORK/json")"
t 10 "$COMUX" list-tree >"$WORK/out" || fail "list-tree (human) failed"
grep -q "$first:t" "$WORK/out" || fail "the human tree shows no tab ids: $(cat "$WORK/out")"
# Every agent row's location must agree with the tree.
t 10 "$COMUX" list-agents --json >"$WORK/agents.json"
python3 - "$WORK/tree.json" "$WORK/agents.json" <<'PYEOF' || fail "list-agents' location fields disagree with list-tree"
import json, sys
tree = json.load(open(sys.argv[1]))["tree"]
agents = json.load(open(sys.argv[2]))["agents"]
where = {p["token"]: (t["id"], t["index"], p["index"]) for s in tree for t in s["tabs"] for p in t["panes"]}
for a in agents:
    assert where.get(a["token"]) == (a["tab_id"], a["tab_index"], a["pane_index"]), (a, where.get(a["token"]))
print(f"{len(agents)} agent row(s) checked")
PYEOF
ok "the tree matches the listings; agent rows name their tab and pane"

echo "27. edge-triggered wait-agent, and JSON from the wait verbs"
t 10 "$COMUX" new-session seqtest --json >"$WORK/json" || fail "could not create the seq session"
P="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pane"])' <"$WORK/json")"
t 10 "$COMUX" send "$P" "printf 'esc to interrupt\n'; $WORK/bin/claude 600" >/dev/null
t 10 "$COMUX" send "$P" $'\n' >/dev/null
S=""
for _ in $(seq 1 60); do
    t 10 "$COMUX" list-agents --json >"$WORK/json"
    S="$(python3 -c 'import json,sys; a=[x for x in json.load(sys.stdin)["agents"] if x["token"]=="'"$P"'" and x["status"]=="working"]; print(a[0]["seq"] if a else "")' <"$WORK/json")"
    [[ -n "$S" ]] && break
    sleep 0.25
done
[[ -n "$S" && "$S" -gt 0 ]] || fail "the seq fixture never read as working with a seq: $(cat "$WORK/json")"
# Level-triggered without --after-seq: the CURRENT status matches, and says its seq.
t 15 "$COMUX" wait-agent "$P" --status working --timeout 10 --json >"$WORK/out" || fail "level wait failed"
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] and d["agent"]["seq"]=='"$S"', d' <"$WORK/out" \
    || fail "wait-agent --json did not report the agent and its seq: $(cat "$WORK/out")"
# Edge-triggered: the status it is ALREADY in, reached at seq S, does not count.
rc=0; t 15 "$COMUX" wait-agent "$P" --status working --after-seq "$S" --timeout 2 --interval 100 --json >"$WORK/out" 2>/dev/null || rc=$?
[[ "$rc" == 124 ]] || fail "--after-seq matched a change it had already seen (exit $rc): $(cat "$WORK/out")"
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d=={"ok":False,"code":"timeout","error":"timed out"}, d' <"$WORK/out" \
    || fail "a --json timeout should print a timeout object: $(cat "$WORK/out")"
# ...and a LATER change does. The prompt text echoes onto the screen while the fixture runs,
# which the screen heuristic reads as blocked.
"$COMUX" wait-agent "$P" --status blocked --after-seq "$S" --timeout 20 --json >"$WORK/edge" 2>&1 &
EDGE_PID=$!
sleep 0.5
t 10 "$COMUX" send "$P" "Do you want to proceed" >/dev/null
rc=0; wait "$EDGE_PID" || rc=$?
[[ "$rc" == 0 ]] || fail "--after-seq missed the newer change (exit $rc): $(cat "$WORK/edge")"
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["agent"]["status"]=="blocked" and d["agent"]["seq"]>'"$S"', d' <"$WORK/edge" \
    || fail "the edge wait should report the newer seq: $(cat "$WORK/edge")"
# wait-output answers in JSON too, success and failure alike.
t 15 "$COMUX" wait-output "$P" "Do you want" --timeout 5 --json >"$WORK/out" || fail "wait-output --json failed"
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] and d["pane"]=="'"$P"'" and "Do you want" in d["line"], d' <"$WORK/out" \
    || fail "wait-output --json shape: $(cat "$WORK/out")"
rc=0; t 15 "$COMUX" wait-output no-such-pane x --timeout 3 --json >"$WORK/out" 2>/dev/null || rc=$?
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is False and d["code"]=="not_found", d' <"$WORK/out" \
    || fail "a failed wait-output --json should print an error object: $(cat "$WORK/out")"
# A plain verb against a dead socket still answers in JSON.
rc=0; COPAD_MUX_SOCK="$WORK/no-such-sock" t 10 "$COMUX" list --json >"$WORK/out" 2>/dev/null || rc=$?
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["code"]=="unavailable", d' <"$WORK/out" \
    || fail "an unreachable server should be a JSON error under --json: $(cat "$WORK/out")"
t 10 "$COMUX" list-sessions --json >"$WORK/json"
sid="$(python3 -c 'import json,sys; print([x["id"] for x in json.load(sys.stdin)["sessions"] if x["name"]=="seqtest"][0])' <"$WORK/json")"
t 12 "$COMUX" kill-session "$sid" >/dev/null || fail "seq cleanup failed"
ok "a status already seen is not an edge; a newer one is; waits and transport errors answer in JSON"

echo "28. send --enter, --paste and --file"
t 10 "$COMUX" new-session sendtest --json >"$WORK/json" || fail "could not create the send session"
P="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pane"])' <"$WORK/json")"
sleep 1 # let the shell reach its prompt
# --enter submits; the marker is assembled by the shell so the echoed command can't match.
t 10 "$COMUX" send "$P" --enter -- 'echo SENT-$((40+2))' >/dev/null || fail "send --enter failed"
t 15 "$COMUX" wait-output "$P" "SENT-42" --timeout 10 >/dev/null || fail "send --enter did not submit the line"
# A program that enables bracketed paste sees the delimiters; `cat -v` makes ESC visible as ^[.
t 10 "$COMUX" send "$P" --enter -- "printf '\\033[?2004h'; cat -v" >/dev/null
sleep 0.5
t 10 "$COMUX" send "$P" --paste --enter -- $'one\x1b[201~two' >/dev/null || fail "send --paste failed"
t 15 "$COMUX" wait-output "$P" '^[[200~onetwo^[[201~' --timeout 10 >/dev/null \
    || fail "a bracketed paste was not wrapped (or the embedded delimiter survived): $(t 10 "$COMUX" capture-pane "$P")"
t 10 "$COMUX" send "$P" $'\x03' >/dev/null # Ctrl-C out of cat
sleep 0.3
# With bracketed paste OFF there is nothing to wrap in.
t 10 "$COMUX" send "$P" --enter -- "printf '\\033[?2004l'; cat -v" >/dev/null
sleep 0.5
t 10 "$COMUX" send "$P" --paste --enter -- "plain-paste" >/dev/null
t 15 "$COMUX" wait-output "$P" 'plain-paste' --timeout 10 >/dev/null || fail "an unbracketed paste did not arrive"
t 10 "$COMUX" capture-pane "$P" >"$WORK/out"
tail -3 "$WORK/out" | grep -q '200~plain' && fail "a paste was bracketed for a program that did not ask: $(cat "$WORK/out")"
t 10 "$COMUX" send "$P" $'\x03' >/dev/null
sleep 0.3
# --file sends the file's contents.
printf 'echo FROMFILE-$((1+1))' >"$WORK/send.txt"
t 10 "$COMUX" send "$P" --file "$WORK/send.txt" --enter >/dev/null || fail "send --file failed"
t 15 "$COMUX" wait-output "$P" "FROMFILE-2" --timeout 10 >/dev/null || fail "send --file did not send the file"
t 10 "$COMUX" list-sessions --json >"$WORK/json"
sid="$(python3 -c 'import json,sys; print([x["id"] for x in json.load(sys.stdin)["sessions"] if x["name"]=="sendtest"][0])' <"$WORK/json")"
t 12 "$COMUX" kill-session "$sid" >/dev/null || fail "send cleanup failed"
ok "--enter submits; --paste brackets only for a program that enabled it, with embedded delimiters stripped; --file sends the file"

echo "29. tabs and panes made where nobody is looking move nobody's view"
t 10 "$COMUX" new-session placetest --json >"$WORK/json" || fail "could not create the placement session"
sp="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["session"]["id"])' <"$WORK/json")"
P0="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pane"])' <"$WORK/json")"
t 10 "$COMUX" select-session 0 >/dev/null
view() { t 10 "$COMUX" list-tree --json | python3 -c 'import json,sys; print([(s["id"],s["active"],[t["id"] for t in s["tabs"] if t["active"]]) for s in json.load(sys.stdin)["tree"]])'; }
v0="$(view)"
mkdir -p "$WORK/place dir"
t 10 "$COMUX" new-tab --session "name:placetest" --name bg --cwd "$WORK/place dir" --json >"$WORK/json" \
    || fail "new-tab --session failed: $(cat "$WORK/json")"
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["session"]["id"]=="'"$sp"'" and d["tab"]["name"]=="bg", d' <"$WORK/json" \
    || fail "new-tab --session did not land where asked: $(cat "$WORK/json")"
B="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pane"])' <"$WORK/json")"
btab="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["tab"]["id"])' <"$WORK/json")"
[[ "$(view)" == "$v0" ]] || fail "new-tab --session moved a view: $v0 -> $(view)"
# The shell started in --cwd.
t 10 "$COMUX" send "$B" --enter -- 'pwd; echo PWD-$((7*6))' >/dev/null
t 15 "$COMUX" wait-output "$B" "PWD-42" --timeout 10 >/dev/null || fail "the background tab's shell never ran"
t 10 "$COMUX" capture-pane "$B" | grep -qF "$WORK/place dir" || fail "new-tab --cwd was not honored: $(t 10 "$COMUX" capture-pane "$B")"
# Split a pane in a BACKGROUND tab of a BACKGROUND session.
t 10 "$COMUX" split --from "$B" --json >"$WORK/json" || fail "split --from a background pane failed: $(cat "$WORK/json")"
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["tab"]["id"]=="'"$btab"'", d' <"$WORK/json" \
    || fail "the background split landed elsewhere: $(cat "$WORK/json")"
[[ "$(view)" == "$v0" ]] || fail "a background split moved a view: $v0 -> $(view)"
# --no-focus without --session lands beside the CALLER, wherever the user is.
COPAD_MUX_PANE="$P0" t 10 "$COMUX" new-tab --no-focus --json >"$WORK/json" || fail "new-tab --no-focus failed"
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["session"]["id"]=="'"$sp"'", d' <"$WORK/json" \
    || fail "new-tab --no-focus did not land in the caller's session: $(cat "$WORK/json")"
[[ "$(view)" == "$v0" ]] || fail "new-tab --no-focus moved a view: $v0 -> $(view)"
t 10 "$COMUX" list-tabs --session "$sp" --json >"$WORK/json"
[[ "$(count tabs)" == 3 ]] || fail "placetest should have 3 tabs: $(cat "$WORK/json")"
rc=0; t 10 "$COMUX" new-tab --cwd "$WORK/nope" >/dev/null 2>&1 || rc=$?
[[ "$rc" == 2 ]] || fail "new-tab --cwd <missing dir> should be a usage error, got $rc"
t 12 "$COMUX" kill-session "$sp" >/dev/null || fail "placement cleanup failed"
ok "new-tab --session/--no-focus and a background split leave every view alone; --cwd honored"

echo "30. spawn: argv verbatim, in the background, gone on exit unless --keep"
t 10 "$COMUX" new-session spawntest --json >"$WORK/json" || fail "could not create the spawn session"
sp="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["session"]["id"])' <"$WORK/json")"
P0="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pane"])' <"$WORK/json")"
t 10 "$COMUX" select-session 0 >/dev/null
v0="$(view)"
tabs_in() { t 10 "$COMUX" list-tabs --session "$sp" --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["tabs"]))'; }
# Arguments a shell would mangle if the command were ever typed into one.
t 10 "$COMUX" spawn --session "$sp" --name worker --env SPAWN_FOO=bar --json -- \
    sh -c 'printf "<%s>" "$@"; echo; echo "ENV-$SPAWN_FOO"; sleep 60' x 'a b' '$HOME' "it's" >"$WORK/json" \
    || fail "spawn failed: $(cat "$WORK/json")"
python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["session"]["id"]=="'"$sp"'" and d["tab"]["name"]=="worker", d' <"$WORK/json" \
    || fail "spawn did not land where asked: $(cat "$WORK/json")"
W="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pane"])' <"$WORK/json")"
t 15 "$COMUX" wait-output "$W" "ENV-bar" --timeout 15 >/dev/null || fail "the spawned program never ran: $(t 10 "$COMUX" capture-pane "$W")"
t 10 "$COMUX" capture-pane "$W" | grep -qF "<a b><\$HOME><it's>" \
    || fail "spawn did not pass argv verbatim: $(t 10 "$COMUX" capture-pane "$W")"
[[ "$(view)" == "$v0" ]] || fail "spawn moved a view: $v0 -> $(view)"
[[ "$(tabs_in)" == 2 ]] || fail "spawntest should have 2 tabs"
# Without --session it lands beside the caller; and a program that exits takes its tab along.
COPAD_MUX_PANE="$P0" t 10 "$COMUX" spawn -- sh -c 'sleep 1' >"$WORK/out" || fail "spawn beside the caller failed"
grep -q -- "-" "$WORK/out" || fail "plain spawn should print the pane token: $(cat "$WORK/out")"
[[ "$(tabs_in)" == 3 ]] || fail "a caller-relative spawn did not land in the caller's session"
for _ in $(seq 1 40); do [[ "$(tabs_in)" == 2 ]] && break; sleep 0.25; done
[[ "$(tabs_in)" == 2 ]] || fail "a spawned program exited but its tab stayed"
# --keep leaves a shell behind, which still takes input.
t 10 "$COMUX" spawn --session "$sp" --keep --json -- sh -c 'echo KEPT-$((1+1))' >"$WORK/json" || fail "spawn --keep failed"
K="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pane"])' <"$WORK/json")"
t 15 "$COMUX" wait-output "$K" "KEPT-2" --timeout 15 >/dev/null || fail "the --keep program never ran"
sleep 1
t 10 "$COMUX" send "$K" --enter -- 'echo SHELL-$((2+3))' >/dev/null || fail "the --keep pane is gone"
t 15 "$COMUX" wait-output "$K" "SHELL-5" --timeout 15 >/dev/null || fail "--keep left no working shell: $(t 10 "$COMUX" capture-pane "$K")"
[[ "$(view)" == "$v0" ]] || fail "spawn --keep moved a view"
rc=0; t 10 "$COMUX" spawn --env COPAD_MUX_PANE=x -- true >/dev/null 2>&1 || rc=$?
[[ "$rc" == 2 ]] || fail "spawn --env COPAD_MUX_PANE=… should be refused with 2, got $rc"
t 12 "$COMUX" kill-session "$sp" >/dev/null || fail "spawn cleanup failed"
ok "argv arrives verbatim with --env; no view moves; the tab closes on exit, or keeps a working shell with --keep"

echo "31. inside a pane, an index counts in the CALLER's tab"
t 10 "$COMUX" new-session idxtest --json >"$WORK/json" || fail "could not create the index session"
sp="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["session"]["id"])' <"$WORK/json")"
A="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pane"])' <"$WORK/json")"
t 10 "$COMUX" split --from "$A" --no-focus --json >"$WORK/json" || fail "split in the index session failed"
S="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pane"])' <"$WORK/json")"
t 10 "$COMUX" select-session 0 >/dev/null
v0="$(view)"
on_screen="$(t 10 "$COMUX" list --json | python3 -c 'import json,sys; print([p["token"] for p in json.load(sys.stdin)["panes"]])')"
mine="$(COPAD_MUX_PANE="$A" t 10 "$COMUX" list --json | python3 -c 'import json,sys; print([p["token"] for p in json.load(sys.stdin)["panes"]])')"
[[ "$mine" == "['$A', '$S']" ]] || fail "list inside a background pane should list ITS tab: $mine"
active="$(COPAD_MUX_PANE="$A" t 10 "$COMUX" list --active --json | python3 -c 'import json,sys; print([p["token"] for p in json.load(sys.stdin)["panes"]])')"
[[ "$active" == "$on_screen" ]] || fail "list --active should list the tab on screen: $active vs $on_screen"
# send 1 / capture-pane --index 1 / wait-output --index 1 all reach the sibling list showed.
COPAD_MUX_PANE="$A" t 10 "$COMUX" send 1 --enter -- 'echo IDX-$((3*4))' >/dev/null || fail "send by index from a background pane failed"
t 15 "$COMUX" wait-output "$S" "IDX-12" --timeout 10 >/dev/null || fail "send 1 did not reach the caller's sibling: $(t 10 "$COMUX" capture-pane "$S")"
COPAD_MUX_PANE="$A" t 10 "$COMUX" capture-pane --index 1 --json >"$WORK/json"
python3 -c 'import json,sys; assert json.load(sys.stdin)["pane"]=="'"$S"'"' <"$WORK/json" || fail "capture-pane --index 1 read another pane: $(cat "$WORK/json")"
COPAD_MUX_PANE="$A" t 15 "$COMUX" wait-output --index 1 "IDX-12" --timeout 10 --json >"$WORK/json" || fail "wait-output --index from a background pane failed: $(cat "$WORK/json")"
python3 -c 'import json,sys; assert json.load(sys.stdin)["pane"]=="'"$S"'"' <"$WORK/json" || fail "wait-output --index pinned another pane"
# focus moves focus in that tab only; close 1 closes the sibling, not a pane on screen.
COPAD_MUX_PANE="$A" t 10 "$COMUX" focus 1 >/dev/null || fail "focus by index from a background pane failed"
COPAD_MUX_PANE="$A" t 10 "$COMUX" list --json | python3 -c 'import json,sys; assert json.load(sys.stdin)["focused"]==1' || fail "focus 1 did not focus the caller's sibling"
[[ "$(view)" == "$v0" ]] || fail "an index verb from a background pane moved a view: $v0 -> $(view)"
COPAD_MUX_PANE="$A" t 10 "$COMUX" close 1 >/dev/null || fail "close by index from a background pane failed"
mine="$(COPAD_MUX_PANE="$A" t 10 "$COMUX" list --json | python3 -c 'import json,sys; print([p["token"] for p in json.load(sys.stdin)["panes"]])')"
[[ "$mine" == "['$A']" ]] || fail "close 1 should have closed the sibling: $mine"
[[ "$(t 10 "$COMUX" list --json | python3 -c 'import json,sys; print([p["token"] for p in json.load(sys.stdin)["panes"]])')" == "$on_screen" ]] \
    || fail "close 1 from a background pane touched the tab on screen"
rc=0; COPAD_MUX_PANE="nope-1" t 10 "$COMUX" list >/dev/null 2>&1 || rc=$?
[[ "$rc" == 1 ]] || fail "list from an unknown caller pane should fail with 1, got $rc"
t 12 "$COMUX" kill-session "$sp" >/dev/null || fail "index cleanup failed"
ok "list/send/capture/wait/focus/close agree on the caller's tab; --active is the tab on screen; no view moves"

echo "32. the server is still responsive and shuts down cleanly"
t 10 "$COMUX" health >/dev/null || fail "health failed — the server did not survive the run"
t 15 "$COMUX" kill-server >/dev/null || fail "kill-server failed"
ok "healthy, then stopped"

echo
echo "PASS — comux close-tab / kill-session / notify / jump / capture-pane / wait-output / agents verified end to end"
