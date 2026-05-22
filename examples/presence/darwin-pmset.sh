#!/usr/bin/env bash
# Presence detection on macOS, polling HIDIdleTime via ioreg.
#
# macOS doesn't expose a per-user "idle" hint as clean as logind's
# IdleHint, but `ioreg -c IOHIDSystem` reports HIDIdleTime in
# nanoseconds since the last keyboard/mouse event. We poll it.
#
# Run as a LaunchAgent. Drop a plist at
# ~/Library/LaunchAgents/com.you.nestty-presence.plist with:
#
#   <key>ProgramArguments</key>
#   <array>
#     <string>/Users/you/.local/bin/nestty-presence-darwin</string>
#   </array>
#   <key>RunAtLoad</key><true/>
#   <key>KeepAlive</key><true/>
#
# Then `launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.you.nestty-presence.plist`.
#
# Threshold knob: TIMEOUT_SECS. Polls every POLL_SECS.

set -euo pipefail

TIMEOUT_SECS="${TIMEOUT_SECS:-300}"
POLL_SECS="${POLL_SECS:-15}"
NESTCTL="${NESTCTL:-nestctl}"

last=""
while :; do
  # HIDIdleTime is the first one ioreg lists for IOHIDSystem; it's
  # nanoseconds. Convert to seconds with /1e9.
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
    "$NESTCTL" presence "$target" >/dev/null || true
    last="$target"
  fi
  sleep "$POLL_SECS"
done
