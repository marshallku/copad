# copad

<img width="3440" height="1440" alt="image" src="https://github.com/user-attachments/assets/a1392646-1255-40ed-9722-ea8523a5c342" />

A cross-platform terminal emulator built around a shared Rust core and platform-native UIs. copad fuses the terminal with a workflow runtime — Event Bus, Action Registry, Context Service, Trigger Engine — and a plugin system, so calendars, notes, Slack, todos, and Claude Code spawns can compose with the editor as one orchestratable surface.

![License](https://img.shields.io/badge/license-MIT-blue)

## Features

### Terminal

- **GPU-rendered backgrounds** — wallpaper image composited behind the terminal with configurable tint and opacity; random rotation supported
- **Tabs + splits** — horizontal/vertical splits, drag-to-resize, focus tracking, drag-to-reorder tabs, double-click rename, collapsible icon-only tab bar
- **In-terminal search** — `Ctrl+Shift+F` (Linux) / `Cmd+F` (macOS), regex with case/whole-word toggle
- **10 built-in themes** — Catppuccin (Mocha/Latte/Frappé/Macchiato), Dracula, Nord, Tokyo Night, Gruvbox Dark, One Dark, Solarized Dark; hot-reload on config save
- **Dynamic font scaling** — `Ctrl+=`/`Ctrl+-`/`Ctrl+0` (Linux) / `Cmd+=`/`Cmd+-`/`Cmd+0` (macOS)
- **Custom keybindings** — bind any chord to a shell command (`spawn:`) or socket action (`action:`)

### Panels

- **Terminal panel** — VTE4 on Linux, SwiftTerm on macOS; PTY handled internally on both platforms
- **WebView panel** — WebKitGTK 6.0 (Linux) / WKWebView (macOS) as a first-class panel; URL toolbar, DevTools toggle, side-by-side with terminals
- **Plugin panels** — HTML/JS panels loaded from `~/.config/copad/plugins/` with an injected `copad` JS bridge for socket calls and event subscriptions
- **Status bar** — Waybar-style 3-zone bar (left/center/right) populated by plugin modules

### Control API

- **`coctl` CLI** — full programmatic control over tabs, splits, panels, terminals, webviews, plugins, and the event stream
- **Unix socket** at `/tmp/copad-{PID}.sock` (auto-discovered via `COPAD_SOCKET`), newline-delimited JSON
- **Event stream** — `event.subscribe` for live `terminal.output`, `panel.focused`, `tab.created`, `webview.navigated`, plus all bus events
- **Terminal agent API** — `terminal.read` / `state` / `exec` / `feed` / `history` / `context` for AI agents
- **Approval workflow** — `agent.approve` shows a modal and returns the user's choice
- **`claude.start`** — spawn a Claude Code session inside a tmux session in a target worktree

### Workflow Runtime

- **Event Bus** — pub/sub with glob patterns, bounded delivery, drop-newest overflow
- **Action Registry** — name → handler map; the same registry serves CLI dispatch, plugin RPC, and triggers
- **Context Service** — active panel, per-panel cwd cache, snapshots; exposed via `context.snapshot`
- **Trigger Engine** — declarative triggers in `config.toml` (`when`, `match`, `do`); fires actions on bus events with `{event.*}` / `{context.*}` interpolation; hot-reloads with subscriber reconciliation

### First-party Plugins

`plugins/<name>/` — install with `./scripts/install-plugins.sh`. Each plugin directory holds the Rust crate (`Cargo.toml` + `src/`) and its runtime manifest/assets (`plugin.toml`, `panel.html`, `triggers.example.toml`) together. All plugins implement the service-plugin protocol (newline-JSON over stdio, supervised by copad).

| Plugin     | Purpose                                                                     |
| ---------- | --------------------------------------------------------------------------- |
| `kb`       | Grep + filename search and atomic read/append/ensure over `~/docs`          |
| `calendar` | Google Calendar event polling with lead-time dedupe                         |
| `slack`    | Slack Socket Mode — mention/DM events + `chat.postMessage`                  |
| `llm`      | Anthropic Messages API client with JSONL usage log                          |
| `todo`     | Markdown-checkbox todos in `~/docs/todos/<workspace>/` (vim/git compatible) |
| `git`      | Worktree create/remove + branch / status queries                            |
| `discord`  | Discord integration                                                         |
| `bookmark` | Bookmarks plugin                                                            |
| `echo`     | Reference / E2E plugin                                                      |

### Platforms

- **Linux** — GTK4 + VTE4, full feature set
- **macOS** — Swift/AppKit + SwiftTerm, near-parity (terminal, tabs, splits, search, themes, webview, plugins, status bar, keybindings, background images, AI agent API). See [`docs/macos-parity-plan.md`](./docs/macos-parity-plan.md).

## Requirements

### Arch Linux

```bash
sudo pacman -S gtk4 vte4 webkitgtk-6.0 gst-plugins-good gst-plugins-bad
```

`gst-plugins-good`/`gst-plugins-bad` are required by WebKitGTK for media playback.

### Other Linux

Install GTK4, libvte-2.91-gtk4, and webkitgtk-6.0 from your distribution's package manager.

### macOS

Xcode Command Line Tools (Swift 6, macOS 14+) and Rust (for `coctl` and the FFI staticlib).

```bash
xcode-select --install
# https://rustup.rs for Rust
```

## Build & Run

```bash
# Build all crates
cargo build

# Run the terminal (Linux)
cargo run -p copad-linux

# Generate a default config file
cargo run -p copad-linux -- --init-config

# Control the running terminal via CLI
cargo run -p copad-cli -- <command>
```

For macOS dev iteration: `cd copad-macos && ./run.sh` (debug bundle, opened in place).

## Install

### Linux — GitHub Releases (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/marshallku/copad/master/install.sh | bash
```

Options: `--version vX.Y.Z` to pin a release, `--system` to install to `/usr/local/bin` (requires sudo).

### Linux — From source

```bash
./scripts/install-dev.sh           # build + install everything to ~/.local/bin (no sudo)
./scripts/install-dev.sh --system  # /usr/local/bin instead of ~/.local/bin (requires sudo)
./scripts/install-dev.sh --restart # also pkill -x copad afterwards
```

Builds a release binary, installs the desktop entry, and lays down all first-party plugins via `install-plugins.sh`.

### macOS — From source

```bash
./scripts/install-macos.sh             # ~/Applications + ~/.cargo/bin (no sudo)
./scripts/install-macos.sh --system    # /Applications + ~/.cargo/bin (sudo for /Applications)
./scripts/install-macos.sh --launch    # open Copad.app after installing
```

Builds `libcopad_ffi.a` (Rust staticlib) → links into the SwiftPM release build → stages and atomically installs `Copad.app` → installs `coctl` via `cargo install --path copad-cli`.

### Plugins only

```bash
./scripts/install-plugins.sh           # install all first-party plugins
./scripts/install-plugins.sh todo git  # install just these
```

Restart copad after installing/updating plugins — `discover_plugins()` only runs at startup.

### Update

```bash
coctl update check    # check for new versions
coctl update apply    # download and install latest (Linux only — macOS users re-run install-macos.sh)
```

### Daemon autostart (Linux)

`copadd` is the background daemon (trigger dispatch, plugin supervision, web-bridge). It runs as a **systemd user unit** — `install-dev.sh` and `install.sh` both install, enable, and start `copad-daemon.service` for you. The GUI (`copad`) is separate and is typically launched by your compositor (e.g. `exec-once = /home/marshall/.local/bin/copad` in `hyprland.conf`).

```bash
systemctl --user status copad-daemon    # inspect
journalctl --user -u copad-daemon -f    # tail logs
systemctl --user restart copad-daemon   # apply a new binary or env override
```

**When does it start on boot?**

| Scenario | Daemon starts on boot? |
|---|---|
| Display manager autologin (SDDM/GDM with `User=…`) | Yes — user session activates `systemd --user`, which starts the enabled unit |
| Manual login on TTY/greeter | Yes — at login |
| Headless boot, no login yet | No — daemon waits for first user session |
| All sessions logged out | Daemon stops with the last session |

For a **single-user desktop with autologin**, the default is enough.

**Want the daemon up from boot without any login, and surviving all logouts?** Enable linger:

```bash
sudo loginctl enable-linger $USER
```

With linger on:

- `systemd --user@<uid>` starts at boot regardless of login state.
- The daemon stays alive across logouts.
- SSH / web-bridge / remote-control reach a daemon that is already running, not one that starts on first contact.

`PATH` note: spawn-style keybindings (e.g. `spawn:~/copad-random-bg.sh --next`) shell out to `coctl`. If you installed to `~/.local/bin` and your Hyprland/systemd session `PATH` does not include it, the spawned child cannot find `coctl`. Fix once with:

```bash
mkdir -p ~/.config/environment.d
printf 'PATH=%s/.local/bin:${PATH}\n' "$HOME" > ~/.config/environment.d/10-local-bin.conf
# Re-login (or `systemctl --user import-environment PATH` + restart compositor) to apply.
```

## Configuration

Config file: `~/.config/copad/config.toml` (entirely optional — all fields have defaults).

```toml
[terminal]
shell = "/bin/zsh"
font_family = "JetBrainsMono Nerd Font Mono"
font_size = 14

[background]
# image = "/path/to/wallpaper.jpg"   # single image (takes priority over directory)
directory = "/path/to/wallpapers/"
tint = 0.85       # tint overlay opacity (0.0–1.0)
opacity = 0.95    # terminal opacity

[tabs]
position = "left"   # top, bottom, left, right
collapsed = true    # start with tab bar collapsed (icon-only)
width = 200         # tab bar width for vertical positions

[socket]
path = "/tmp/copad.sock"

[theme]
name = "catppuccin-mocha"

[keybindings]
"ctrl+shift+g" = "spawn:~/scripts/wallpaper.sh --next"
"ctrl+shift+m" = "action:background.toggle"

[security]   # macOS only, for now
osc52 = "deny"   # or "allow" — gates OSC 52 clipboard writes from the PTY
```

See [`docs/config.md`](./docs/config.md) for the full reference, and [`docs/workflow-runtime.md`](./docs/workflow-runtime.md) for `[[triggers]]` declarations.

## Project Structure

```
copad/
├── copad-core/                # Shared Rust library (config, protocol, event bus,
│                                 # action registry, context, triggers, themes, fs_atomic)
├── copad-ffi/                 # Rust staticlib for Swift FFI (macOS bridge)
├── copad-linux/               # GTK4 + VTE4 native terminal app (binary: copad)
├── copad-macos/               # Swift/AppKit + SwiftTerm app (Copad.app)
├── copad-cli/                 # CLI control tool (binary: coctl)
├── plugins/<name>/             # First-party service plugins. Each subdir holds the
│                                 # Rust crate (Cargo.toml + src/) and its manifest/assets
│                                 # (plugin.toml, panel.html, triggers.example.toml) together.
│                                 # Crate names remain `copad-plugin-<name>`.
├── examples/plugins/hello/     # Tutorial plugin: panel + bash command (no Rust crate)
├── scripts/                    # install-dev.sh, install-macos.sh, install-plugins.sh
└── docs/                       # Project documentation — start at docs/INDEX.md
```

## Documentation

Start at [`docs/INDEX.md`](./docs/INDEX.md). Highlights:

- [`architecture.md`](./docs/architecture.md) — crate layout, socket protocol, panel system
- [`workflow-runtime.md`](./docs/workflow-runtime.md) — Event Bus, Action Registry, Context Service, triggers
- [`plugins.md`](./docs/plugins.md) — plugin manifest, JS bridge API, service-plugin RPC
- [`service-plugins.md`](./docs/service-plugins.md) — long-running supervised subprocess design
- [`cli.md`](./docs/cli.md) — `coctl` reference
- [`linux-app.md`](./docs/linux-app.md) / [`macos-app.md`](./docs/macos-app.md) — platform internals
- [`troubleshooting.md`](./docs/troubleshooting.md) — known issues + fixes
- [`roadmap.md`](./docs/roadmap.md) — implementation phases

## License

MIT
