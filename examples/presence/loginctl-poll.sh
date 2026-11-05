#!/usr/bin/env bash
# Presence detection via systemd-logind's IdleHint, polled. Works on any systemd-managed session;
# most DEs flip IdleHint on lockscreen / DPMS off.
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
# Knob: POLL_SECS.

set -euo pipefail

POLL_SECS="${POLL_SECS:-30}"
COCTL="${COCTL:-coctl}"

session_id="${XDG_SESSION_ID:-}"
if [ -z "$session_id" ]; then
  # Fall back to the user's first graphical session when XDG_SESSION_ID is not in the unit's env.
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
