#!/usr/bin/env bash
# Presence detection on macOS, polling HIDIdleTime (nanoseconds since the last keyboard/mouse
# event) from `ioreg -c IOHIDSystem`.
#
# Run as a LaunchAgent. Drop a plist at
# ~/Library/LaunchAgents/com.you.copad-presence.plist with:
#
#   <key>ProgramArguments</key>
#   <array>
#     <string>/Users/you/.local/bin/copad-presence-darwin</string>
#   </array>
#   <key>RunAtLoad</key><true/>
#   <key>KeepAlive</key><true/>
#
# Then `launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.you.copad-presence.plist`.
#
# Threshold knob: TIMEOUT_SECS. Polls every POLL_SECS.

set -euo pipefail

TIMEOUT_SECS="${TIMEOUT_SECS:-300}"
POLL_SECS="${POLL_SECS:-15}"
COCTL="${COCTL:-coctl}"

last=""
while :; do
  # HIDIdleTime is the first match for IOHIDSystem, in nanoseconds.
  idle_ns="$(ioreg -c IOHIDSystem | awk '/HIDIdleTime/ {print $NF; exit}')"
  if [ -z "$idle_ns" ]; then
    sleep "$POLL_SECS"
    continue
  fi
  idle_secs=$(( idle_ns / 1000000000 ))
  if [ "$idle_secs" -ge "$TIMEOUT_SECS" ]; then
    target="away"
  else
    target="active"
  fi
  if [ "$target" != "$last" ]; then
    "$COCTL" presence "$target" >/dev/null || true
    last="$target"
  fi
  sleep "$POLL_SECS"
done
