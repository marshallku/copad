# Quick Start

This page gets you productive in a few minutes. Two independent tracks — pick whichever you installed.

- [Track A: comux (the multiplexer)](#track-a-comux)
- [Track B: copad (the desktop terminal)](#track-b-copad)

---

## Track A: comux

Start (or attach to) the persistent server:

```bash
comux
```

The server keeps running even after you close the launching terminal, so your shells and agents survive. Inside comux, everything is driven by a **prefix key** — `Ctrl-b` by default — followed by a command key (exactly like tmux).

### Your first five minutes

| Do this | Result |
| --- | --- |
| `Ctrl-b c` | Create a new tab |
| `Ctrl-b "` | Split the current pane top/bottom |
| `Ctrl-b %` | Split left/right |
| `Ctrl-b` then arrow keys / `h j k l` | Move focus between panes |
| `Ctrl-b C` | Create a new **session** (a whole isolated workspace) — prompts for a name |
| `Ctrl-b s` | Toggle the sidebar (sessions on top, agents below) |
| `Ctrl-b d` | Detach — leaves everything running; run `comux` again to re-attach |

Run an agent in a pane and comux tracks it automatically:

```bash
claude          # its status shows in the sidebar and status bar
```

When the agent finishes a turn or needs your input, comux fires a **desktop notification** — even while you're detached — and `Ctrl-b !` jumps you straight to it.

### From the command line

Every in-app action also has a CLI form (tmux-style — the `ctl` is optional):

```bash
comux new-session work          # create + switch to a session named "work"
comux worktree create feat/x    # create a git worktree + a session in it
comux list-sessions             # list sessions
comux server status             # is the server running?
comux server restart            # restart it (restores your whole layout; agents resume)
```

Next: [Sessions, Tabs & Panes](./comux/sessions-tabs-panes.md) · [Keybindings](./comux/keybindings.md)

---

## Track B: copad

Launch the app:

- **Linux:** run `copad`, or launch it from your app menu. To make it your default terminal in GNOME: `gsettings set org.gnome.desktop.default-applications.terminal exec copad`
- **macOS:** open `Copad.app` from Spotlight, the Dock, or `open -a Copad`.

### Create your config

copad works with zero config, but to customize it, generate a starter file:

```bash
copad --init-config     # writes ~/.config/copad/config.toml
copad --config-path     # prints where the config lives
```

The file hot-reloads — save it and copad picks up theme, font, background, and keybinding changes live.

### A first taste

```toml
# ~/.config/copad/config.toml
[terminal]
font_family = "JetBrainsMono Nerd Font Mono"
font_size = 14

[theme]
name = "catppuccin-mocha"     # one of 10 built-ins

[tabs]
position = "top"              # top | bottom | left | right
```

### Driving it from a script

With the app running, `coctl` controls it:

```bash
coctl ping                                # is it alive?
coctl tab new                             # open a tab
coctl split vertical                      # split the focused pane
coctl webview open https://example.com    # open a web panel
coctl theme list                          # list themes + the current one
coctl usage --oneline                     # your Claude/Codex token + cost readout
```

Next: [copad Overview](./copad/index.md) · [Configuration](./copad/configuration.md) · [coctl Reference](./coctl/reference.md)
