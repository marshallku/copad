# Troubleshooting

Common issues and their fixes, grouped by tool.

## comux

**The status-bar usage gauge is blank.**
The readout only shows when the terminal is at least 100 columns wide, so widen the window. It's resolved in-process (no `coctl` needed), but it still requires valid local credentials to read — a Claude OAuth token in `~/.claude/.credentials.json` and/or a Codex rollout file. Confirm the same data with `coctl usage --limits` if you have `coctl`. The poller can be disabled entirely with `usage = off` in `~/.config/copad/mux.toml` or `COPAD_MUX_USAGE=0`.

**My agents came back as fresh chats after a restart.**
Agent resume needs `restore_agent_sessions = true` (the default) and a discoverable session id (`~/.claude/sessions/<pid>.json` for Claude, the held-open rollout file for Codex). One-shot invocations (`claude -p …`) are intentionally not resumed. See [Persistence & Agent Resume](./comux/persistence.md).

**A server started over SSH makes `claude` think it's remote.**
The server freezes its birth environment. comux scrubs volatile session vars (`SSH_*`, `DISPLAY`, …) and re-injects them per-client, but local-only privileges follow the server's kernel session — birth the server locally to fix them. See [Environment refresh](./comux/environment.md#environment-refresh). Silence the SSH warning with `COPAD_MUX_QUIET_SSH=1`.

**`comux server stop`/`restart` says "server still shutting down".**
It waits up to 5s for the old socket to disappear. If it doesn't, force it: `pkill -x comux`.

**Rendering drift / ghosting after a resize or nested emulator.**
Press `Ctrl-b r` to force a full repaint. As a last resort for persistent outer-emulator drift, set `COPAD_MUX_REDRAW_MS=<ms>` to enable a periodic self-healing repaint (off by default because it flashes a blank frame each tick).

**`Alt`/`Option`+number doesn't switch tabs.**
Your terminal must be set to send Option/Alt as Meta. Otherwise use `Ctrl-b 1`…`9`.

**I want a completely fresh comux, no restored sessions.**
Set `persist = false` in `mux.toml`, or delete the state file: `rm ~/.local/state/copad/mux-session.json`.

**Config changes aren't taking effect.**
Run `comux reload` for live-reloadable settings. Three settings are boot-fixed and need `comux server restart`: `persist`, `autosave_secs`, `update_environment`.

---

## copad (GUI)

**The GTK app won't start / missing libraries (Linux).**
copad needs `gtk4`, `vte4` (`libvte-2.91-gtk4`), `webkitgtk-6.0`, and `gst-plugins-good` / `gst-plugins-bad`. On Arch: `sudo pacman -S gtk4 vte4 webkitgtk-6.0 gst-plugins-good gst-plugins-bad`.

**A stale system copy shadows my fresh build (Linux).**
If both `~/.local/bin/copad` and `/usr/local/bin/copad` exist, `PATH` usually resolves `/usr/local/bin` first, so an old system copy can silently shadow a newer user-local build (and desktop-entry launches use the system copy too). Remove the stale one, or install consistently with `scripts/install-dev.sh` (user) vs `--system`.

**Background image doesn't show (Linux).**
Background images require VTE transparency (`set_clear_background(false)`), handled internally — but make sure the image path resolves. Use `coctl background set <path>` to test, and `coctl background cache` to (re)build a list file.

**`Ctrl+Shift+U` (new web tab) does nothing (Linux).**
It collides with IBus Unicode input. Rebind the web-tab shortcut in `[keybindings]`, or open web tabs from the tab-bar "+" popover.

**The macOS app won't launch on macOS 26 (Tahoe).**
Ad-hoc-signed releases break on Tahoe. Install from source with `scripts/install-macos.sh` (a self-signed identity survives Tahoe) until a Developer-ID-signed release ships.

**Config edits aren't applied.**
`config.toml` hot-reloads on save (font, background, tint, tab position, keybindings, theme, OSC 52). If it's still not taking effect, confirm you're editing the right file: `copad --config-path`.

---

## coctl

**`coctl` can't find the running instance.**
It auto-discovers the newest GUI socket, then the daemon socket. To debug, set `COPAD_DEBUG_SOCKET=1` and re-run — it prints the socket it chose. You can also target one explicitly: `coctl --socket <path> …`. Remember `--socket` must come **before** the subcommand.

**`coctl <plugin> …` returns an error about the plugin.**
That plugin isn't installed, or (for `slack`/`jira`/`calendar`/`discord`) it's missing credentials. Check with `coctl plugin list` and `coctl <plugin> auth-status`, and see [Using Plugins](./plugins.md).

**`coctl update apply` says it's not supported.**
It's Linux-only. On macOS re-run the `install.sh` one-liner, `brew upgrade --cask marshallku/copad/copad`, or `scripts/install-macos.sh`.

---

## Still stuck?

The repository's `docs/troubleshooting.md` carries a much deeper, developer-oriented list of gotchas and their root causes. For anything not covered here, open an issue at <https://github.com/marshallku/copad/issues>.
