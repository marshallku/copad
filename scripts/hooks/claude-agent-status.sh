#!/usr/bin/env bash
# claude-agent-status.sh — publish copad agent-status events from Claude Code hooks so copad
# can toast when an agent finishes a turn or needs input.
#
# Point your Claude Code hooks at this script (see docs/harness-hooks.md
# § "Agent status → toast"):
#
#   Stop         → claude-agent-status.sh stopped
#   Notification → claude-agent-status.sh notification
#
# It reads the hook's JSON payload on stdin and publishes a `claude.<event>` bus event via
# `coctl event publish --quiet`. Pair it with the triggers in
# examples/triggers/claude-agent-status.toml.
#
#   - `Stop` has no "final stop" signal (a sibling Stop hook can continue the agent), so
#     `stopped` is occurrence-based: every Stop publishes.
#   - `Notification` fires for many reasons; filter on the structured `notification_type`,
#     never on localized message text.
#   - `$COPAD_PANEL_ID` is exported into each copad pane's shell and carried in the payload.
#   - The payload is built with `jq -n --arg` so a hostile `cwd`/`message` cannot break out
#     of the JSON string.

set -euo pipefail

# `notification_type` values that mean "the agent is waiting for the human". The others
# (auth_success, elicitation_complete/response, idle_prompt, agent_completed) are dropped.
AWAITING_TYPES_RE='^(permission_prompt|elicitation_dialog|agent_needs_input)$'

usage() {
    printf 'usage: %s <working|stopped|notification> [--self-test]\n' "${0##*/}" >&2
    exit 2
}

# Best-effort: `--quiet` exits 0 when the daemon is down and `|| true` covers other failures,
# so a publish problem never fails the host hook.
publish() {
    local kind="$1" payload="$2"
    coctl event publish "$kind" --quiet "$payload" >/dev/null 2>&1 || true
}

# Emit the right event for a hook payload read from stdin. Echoes the
# published "kind\tpayload" (or nothing when filtered out) so the
# self-test can assert without a live daemon.
emit() {
    local event="$1" input="$2"
    local cwd panel_id
    panel_id="${COPAD_PANEL_ID:-}"
    cwd="$(jq -r '.cwd // empty' <<<"$input" 2>/dev/null || true)"
    [ -n "$cwd" ] || cwd="$PWD"

    local session
    session="$(jq -r '.session_id // empty' <<<"$input" 2>/dev/null || true)"

    case "$event" in
    working)
        # Drives the cockpit's "working" state. Not `claude.session_started`, which is
        # reserved for SessionStart.
        local payload
        payload="$(jq -n \
            --arg panel_id "$panel_id" \
            --arg cwd "$cwd" \
            --arg session "$session" \
            '{panel_id: $panel_id, cwd: $cwd, session: $session}')"
        printf '%s\t%s\n' "claude.working" "$payload"
        ;;
    stopped)
        local payload
        payload="$(jq -n \
            --arg panel_id "$panel_id" \
            --arg cwd "$cwd" \
            --arg session "$session" \
            '{panel_id: $panel_id, cwd: $cwd, session: $session}')"
        printf '%s\t%s\n' "claude.session_stopped" "$payload"
        ;;
    notification)
        local nt msg payload
        nt="$(jq -r '.notification_type // empty' <<<"$input" 2>/dev/null || true)"
        [[ "$nt" =~ $AWAITING_TYPES_RE ]] || return 0
        msg="$(jq -r '.message // empty' <<<"$input" 2>/dev/null || true)"
        payload="$(jq -n \
            --arg panel_id "$panel_id" \
            --arg cwd "$cwd" \
            --arg session "$session" \
            --arg notification_type "$nt" \
            --arg message "$msg" \
            '{panel_id: $panel_id, cwd: $cwd, session: $session, notification_type: $notification_type, message: $message}')"
        printf '%s\t%s\n' "claude.awaiting_input" "$payload"
        ;;
    *)
        usage
        ;;
    esac
}

self_test() {
    command -v jq >/dev/null || { echo "self-test needs jq" >&2; exit 1; }
    local fails=0
    check() { # desc expected-kind-or-empty  actual-line
        local desc="$1" want="$2" got_kind="${3%%$'\t'*}"
        [ "$3" = "" ] && got_kind=""
        if [ "$got_kind" = "$want" ]; then
            printf '  ok   %s\n' "$desc"
        else
            printf '  FAIL %s (want kind=%q got=%q)\n' "$desc" "$want" "$got_kind"
            fails=$((fails + 1))
        fi
    }

    check_session() { # desc expected-session actual-line
        local desc="$1" want="$2"
        local got; got="$(printf '%s' "${3#*$'\t'}" | jq -r '.session // empty' 2>/dev/null)"
        if [ "$got" = "$want" ]; then printf '  ok   %s\n' "$desc"
        else printf '  FAIL %s (want session=%q got=%q)\n' "$desc" "$want" "$got"; fails=$((fails + 1)); fi
    }

    export COPAD_PANEL_ID="pane-test"
    check "working emits claude.working" "claude.working" \
        "$(emit working '{"cwd":"/tmp/x","session_id":"s1"}')"
    check_session "working carries session" "s1" \
        "$(emit working '{"cwd":"/tmp/x","session_id":"s1"}')"
    check "stop emits session_stopped" "claude.session_stopped" \
        "$(emit stopped '{"cwd":"/tmp/x","session_id":"s1"}')"
    check_session "stop carries session" "s1" \
        "$(emit stopped '{"cwd":"/tmp/x","session_id":"s1"}')"
    check "notification permission emits" "claude.awaiting_input" \
        "$(emit notification '{"cwd":"/tmp/x","notification_type":"permission_prompt","message":"allow?"}')"
    check_session "awaiting carries session" "s9" \
        "$(emit notification '{"session_id":"s9","notification_type":"permission_prompt"}')"
    check "notification elicitation emits" "claude.awaiting_input" \
        "$(emit notification '{"notification_type":"elicitation_dialog"}')"
    check "notification auth_success filtered" "" \
        "$(emit notification '{"notification_type":"auth_success"}')"
    check "notification idle_prompt filtered" "" \
        "$(emit notification '{"notification_type":"idle_prompt"}')"
    check "notification no type filtered" "" \
        "$(emit notification '{"message":"hi"}')"
    local hostile
    hostile="$(emit stopped '{"cwd":"/x\"; rm -rf /","session_id":"s"}')"
    if printf '%s' "${hostile#*$'\t'}" | jq -e . >/dev/null 2>&1; then
        printf '  ok   %s\n' "hostile cwd stays valid JSON"
    else
        printf '  FAIL %s\n' "hostile cwd broke JSON"; fails=$((fails + 1))
    fi

    # Gate tests run main() with a stubbed coctl on PATH to verify the panel-id/socket gate.
    local tmp stub captured
    tmp="$(mktemp -d)"; stub="$tmp/bin"; captured="$tmp/published"
    mkdir -p "$stub"
    printf '#!/bin/sh\n[ "$1" = event ] && [ "$2" = publish ] && echo "publish $3" >>"%s"\nexit 0\n' \
        "$captured" > "$stub/coctl"
    chmod +x "$stub/coctl"
    gate() { # desc  env-assignments  expect-published(1/0)
        local desc="$1" want="$3"
        : > "$captured"
        env PATH="$stub:$PATH" $2 bash "$0" stopped \
            <<<'{"cwd":"/t","session_id":"s"}' >/dev/null 2>&1
        local got=0; [ -s "$captured" ] && got=1
        if [ "$got" = "$want" ]; then printf '  ok   %s\n' "$desc"
        else printf '  FAIL %s (want published=%s got=%s)\n' "$desc" "$want" "$got"; fails=$((fails + 1)); fi
    }
    gate "gate: COPAD_SOCKET set, no PANEL_ID → publishes (SSH path)" \
        "COPAD_SOCKET=/x COPAD_PANEL_ID=" 1
    gate "gate: COPAD_PANEL_ID set, no SOCKET → publishes (local path)" \
        "COPAD_PANEL_ID=p COPAD_SOCKET=" 1
    gate "gate: neither set → no-op (outside copad)" \
        "COPAD_PANEL_ID= COPAD_SOCKET=" 0
    rm -rf "$tmp"

    if [ "$fails" -eq 0 ]; then
        echo "self-test: all passed"
    else
        echo "self-test: $fails failure(s)" >&2
        exit 1
    fi
}

main() {
    [ $# -ge 1 ] || usage
    if [ "$1" = "--self-test" ]; then
        self_test
        return
    fi
    local event="$1"
    # Act only when a copad daemon is reachable: inside a local pane ($COPAD_PANEL_ID) or over
    # SSH with a forwarded socket ($COPAD_SOCKET, docs/ssh-remote.md). $COPAD_PANEL_ID is not
    # forwarded over SSH, so requiring it would silently no-op the remote path.
    if [ -z "${COPAD_PANEL_ID:-}" ] && [ -z "${COPAD_SOCKET:-}" ]; then
        exit 0
    fi
    command -v coctl >/dev/null 2>&1 || exit 0
    command -v jq >/dev/null 2>&1 || exit 0

    local input line
    input="$(cat)"
    line="$(emit "$event" "$input")" || exit 0
    [ -n "$line" ] || exit 0
    publish "${line%%$'\t'*}" "${line#*$'\t'}"
}

main "$@"
