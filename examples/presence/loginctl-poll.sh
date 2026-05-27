#!/usr/bin/env bash
# Presence detection via systemd-logind's IdleHint, polled.
#
# Works on any systemd-managed session regardless of desktop
# environment — logind tracks per-session IdleHint based on the
# graphical layer's report (most DEs flip it on lockscreen / DPMS
# off). Polling is the dumb-but-portable path; a D-Bus subscription
# is the upgrade if this gets too jittery for you.
#
# Drop on PATH, run as a systemd --user service:
#
#   [Unit]
#   Description=copad presence — loginctl IdleHint poller
#   After=graphical-session.target
#   PartOf=graphical-session.target
#
#   [Service]
#   Type=simple
#   ExecStart=%h/.local/bin/copad-presence-loginctl
#   Restart=on-failure
#
#   [Install]
#   WantedBy=graphical-session.target
#
# Then `systemctl --user enable --now copad-presence-loginctl`.
#
# Threshold knob: POLL_SECS. Tune for jitter vs latency.

set -euo pipefail

POLL_SECS="${POLL_SECS:-30}"
COCTL="${COCTL:-coctl}"

session_id="${XDG_SESSION_ID:-}"
if [ -z "$session_id" ]; then
  # Fall back to the user's first graphical session if XDG_SESSION_ID
  # isn't in the unit's env (it usually IS for --user units).
  # loginctl list-sessions columns are: SESSION UID USER SEAT TTY
  session_id="$(loginctl list-sessions --no-legend \
    | awk -v user="$USER" '$3 == user && $4 == "seat0" {print $1; exit}')"
fi
if [ -z "$session_id" ]; then
  echo "copad-presence-loginctl: cannot resolve a graphical session" >&2
  exit 1
fi

last=""
while :; do
  hint="$(loginctl show-session "$session_id" --property=IdleHint --value 2>/dev/null || echo "")"
  case "$hint" in
    yes) target="away" ;;
    no)  target="active" ;;
    *)   target="" ;;
  esac
  if [ -n "$target" ] && [ "$target" != "$last" ]; then
    "$COCTL" presence "$target" >/dev/null || true
    last="$target"
  fi
  sleep "$POLL_SECS"
done
