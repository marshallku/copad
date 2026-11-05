#!/bin/sh
# copadd launch wrapper (macOS LaunchAgent ProgramArguments target).
#
# Plugin bearer tokens (COPAD_WEB_BRIDGE_TOKEN, ...) must reach copadd's environment, but the
# plist is repo-tracked and world-readable (644), so secrets cannot live there. This sources
# an operator-owned secrets file (mode 600) that install-macos.sh generates once and never
# overwrites. launchd sets HOME for gui/<uid> agents.
#
# launchd hands agents a minimal PATH, but plugins shell out to Homebrew tools (web-bridge
# runs `tmux`, git plugins run `git`), so Homebrew bin dirs go first; without them the mobile
# pane list comes back empty. The dirs copadd itself may live in are prepended too:
# ~/.local/bin (install.sh) and ~/.cargo/bin (install-macos.sh).
PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
export PATH

# launchd agents inherit no LANG/LC_*; without a UTF-8 locale tmux rewrites the TAB
# separators in web-bridge's `list-panes -F` to `_` and the mobile pane list parses to zero
# rows. An explicit override is respected.
: "${LANG:=en_US.UTF-8}"
export LANG

set -a
[ -f "$HOME/.config/copad/secrets.env" ] && . "$HOME/.config/copad/secrets.env"
set +a

# An installer pins the path it just installed to ~/.config/copad/copadd.path (overwritten
# every install), so switching between --user and --system installs launches the fresh
# binary, not a stale copy in another bin dir. Otherwise search the known install dirs, then
# PATH: hardcoding one would put a KeepAlive agent into a restart loop on the others.
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
