#!/bin/sh
# copadd launch wrapper (macOS LaunchAgent ProgramArguments target).
#
# Why a wrapper instead of running copadd directly: plugin bearer tokens
# (COPAD_WEB_BRIDGE_TOKEN, …) must reach copadd's environment so its plugin
# supervisor injects them into plugins — but the LaunchAgent plist is
# repo-tracked and world-readable (mode 644), so secrets must NOT live there.
# This wrapper sources an operator-owned secrets file (mode 600) that
# install-macos.sh generates once and NEVER overwrites, so the token survives
# reinstalls. launchd sets HOME for gui/<uid> agents, so $HOME resolves here.
# launchd hands a gui/<uid> agent a minimal PATH (/usr/bin:/bin:/usr/sbin:/sbin).
# copadd's plugins shell out to Homebrew-installed tools — the web-bridge runs
# `tmux` (for the mobile live-attach / send), git plugins run `git`, etc. — so
# put the Homebrew bin dirs (Apple Silicon + Intel) ahead of the minimal PATH.
# Without this, `tmux` isn't found and the mobile pane list comes back empty.
# Also prepend the two dirs copadd itself may live in: install.sh (prebuilt
# release) drops it in ~/.local/bin, install-macos.sh (from source, via
# `cargo install`) in ~/.cargo/bin.
PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
export PATH

# launchd agents inherit no LANG/LC_*, and without a UTF-8 locale tmux
# sanitizes non-printable bytes in format output — it rewrites the TAB field
# separators in web-bridge's `list-panes -F` to `_`, so the mobile pane list
# parses to zero rows. Default to a UTF-8 locale (respect an explicit override).
: "${LANG:=en_US.UTF-8}"
export LANG

set -a
[ -f "$HOME/.config/copad/secrets.env" ] && . "$HOME/.config/copad/secrets.env"
set +a

# Resolve copadd. An installer pins the exact path it just installed to
# ~/.config/copad/copadd.path (overwritten every install), so a switch between
# a --user (~/.local/bin) and --system (/usr/local/bin) install always launches
# the just-installed binary rather than a stale copy left in another bin dir.
# Fall back to a search across the known install dirs, then PATH — hardcoding
# one would put a launchd KeepAlive agent into a restart loop on the others.
COPADD=""
if [ -f "$HOME/.config/copad/copadd.path" ]; then
    pinned="$(cat "$HOME/.config/copad/copadd.path" 2>/dev/null || true)"
    [ -n "$pinned" ] && [ -x "$pinned" ] && COPADD="$pinned"
fi
if [ -z "$COPADD" ]; then
    for cand in "$HOME/.local/bin/copadd" "$HOME/.cargo/bin/copadd" /usr/local/bin/copadd /opt/homebrew/bin/copadd; do
        if [ -x "$cand" ]; then COPADD="$cand"; break; fi
    done
fi
[ -z "$COPADD" ] && COPADD="$(command -v copadd 2>/dev/null || true)"
if [ -z "$COPADD" ]; then
    echo "copadd-launch: copadd not found in ~/.local/bin, ~/.cargo/bin, /usr/local/bin, /opt/homebrew/bin, or PATH" >&2
    exit 127
fi
exec "$COPADD" "$@"
