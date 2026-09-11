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

echo "15. the server is still responsive and shuts down cleanly"
t 10 "$COMUX" health >/dev/null || fail "health failed — the server did not survive the run"
t 15 "$COMUX" kill-server >/dev/null || fail "kill-server failed"
ok "healthy, then stopped"

echo
echo "PASS — comux close-tab / kill-session / notify / jump / capture-pane / wait-output / agents verified end to end"
