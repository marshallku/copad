#!/usr/bin/env bash
# Presence detection via swayidle (any wlroots compositor: Sway,
# Hyprland, river, wayfire, …).
#
# Drop this script somewhere on PATH (e.g. ~/.local/bin/), make it
# executable, then either:
#
#   - Hyprland: add `exec-once = ~/.local/bin/nestty-presence-swayidle`
#     to ~/.config/hypr/hyprland.conf.
#   - Sway: same idea in ~/.config/sway/config (`exec`).
#   - systemd --user: drop a oneshot unit that ExecStarts this.
#
# Threshold knob: TIMEOUT_SECS. swayidle's `before-sleep` is also
# wired so a suspend → resume cycle re-syncs presence on wake.

set -euo pipefail

TIMEOUT_SECS="${TIMEOUT_SECS:-300}"
NESTCTL="${NESTCTL:-nestctl}"

# `-w` waits for each command to finish before continuing, which
# matters if you also chain a lock command into the same swayidle.
exec swayidle -w \
  timeout "$TIMEOUT_SECS" "$NESTCTL presence away" \
                  resume  "$NESTCTL presence active" \
  before-sleep "$NESTCTL presence away"
