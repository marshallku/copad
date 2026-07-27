# copad

<img width="1200" height="502" alt="copad — a terminal built for orchestrating AI agents" src="https://github.com/user-attachments/assets/fb9996a7-f131-4265-84ae-cb2c6183bc50" />

> **The terminal as an orchestration surface — for your shells, your plugins, and your AI agents.**

copad is a cross-platform terminal emulator built on a shared Rust core with platform-native UIs — GTK4 on Linux, Swift/AppKit on macOS, SwiftUI on iOS. But it's more than an emulator: a workflow runtime (Event Bus, Action Registry, Context Service, Trigger Engine) and a plugin system turn the terminal into one programmable surface, where shells, AI agents, calendars, notes, Slack, and todos all compose and react to one another.

![License](https://img.shields.io/badge/license-MIT-blue)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%C2%B7%20macOS%20%C2%B7%20iOS-blue)
![Core](https://img.shields.io/badge/core-Rust%20edition%202024-orange)

**[Install](#install) · [Just comux](#just-comux-the-multiplexer-standalone) · [Configuration](#configuration) · [Docs](#documentation)**

## Highlights

- **Built for AI agents** — [`comux`](#multiplexer-comux) runs a whole team of `claude` / `codex` sessions in one terminal: live per-agent status, a desktop ping the moment one needs you, and a restart that brings every agent back *mid-conversation* — even after a reboot.
- **Programmable, not just configurable** — an event bus and trigger engine let plugins react to each other, while the `coctl` CLI and a Unix-socket API drive every pane, tab, and panel from a script.
- **Truly cross-platform** — one Rust core, native UIs on Linux, macOS (Metal-accelerated), and iOS.

## Features

### Terminal

- **GPU-rendered backgrounds** — wallpaper image composited behind the terminal with configurable tint and opacity; random rotation supported
- **Tabs + splits** — horizontal/vertical splits, drag-to-resize, focus tracking, drag-to-reorder tabs, double-click rename, collapsible icon-only tab bar
- **In-terminal search** — `Ctrl+Shift+F` (Linux) / `Cmd+F` (macOS), regex with case/whole-word toggle
- **10 built-in themes** — Catppuccin (Mocha/Latte/Frappé/Macchiato), Dracula, Nord, Tokyo Night, Gruvbox Dark, One Dark, Solarized Dark; hot-reload on config save
- **Dynamic font scaling** — `Ctrl+=`/`Ctrl+-`/`Ctrl+0` (Linux) / `Cmd+=`/`Cmd+-`/`Cmd+0` (macOS)
- **Custom keybindings** — bind any chord to a shell command (`spawn:`); the spawned command inherits `COPAD_SOCKET`, so `spawn:coctl …` reaches the binding instance's socket actions

### Multiplexer (`comux`)

**Run a team of AI coding agents in one terminal — see every agent's status at a glance, get pinged the moment one needs you, and detach without stopping any of them.**

`comux` is a standalone, tmux-style multiplexer: a single self-contained binary (no GTK, no daemon) with a persistent server/client split, so the server owns your panes and outlives the terminal that launched it. It's a complete multiplexer — but it's built for the workflow tmux never was: driving several `claude` / `codex` sessions at once.

> **Just want the multiplexer?** `comux` needs no other part of copad. It's a single static binary you can drop onto any machine — including a headless server over SSH — and use anywhere you'd reach for tmux:
>
> ```bash
> curl -fsSL https://raw.githubusercontent.com/marshallku/copad/master/install-comux.sh | bash
> ```

**Made for orchestrating agents**

- **Live agent status** — an always-on sidebar lists every agent pane across every session with a real `status · tool` readout (`working` / `ready` / `blocked`), so you always know who's busy and who's waiting on you
- **Turn notifications** — a native desktop toast fires the instant an agent finishes or blocks for input, *even while you're detached*; `Ctrl-b !` jumps to the blocked one, `Ctrl-b a` opens a notification center
- **Detach & resume** — close the client and the agents keep running on the server; reattach anytime and pick up exactly where you left off
- **Restart-proof — including the AI conversation** — kill the server or reboot the machine, and comux restores your whole layout *and* relaunches each agent mid-conversation via `claude --resume <id>` / `codex resume <id>`. Where tmux-resurrect brings back your shells, comux brings back the live chat
- **Subscription usage in the status bar** — per-window rate-limit utilization (Claude 5h + weekly, Codex weekly) rendered as threshold-colored bars

**A full multiplexer, too**

- **Splits, tabs, multi-session workspaces** — vim-style pane focus/resize, `Ctrl-b` prefix bindings, prefix-less `Alt`+`1`–`9`, named sessions
- **git worktree integration** — `comux worktree create <branch>` spins up a worktree plus a session inside it (tmux `twt` parity)
- **`Ctrl-f` fuzzy switcher** — jump across sessions and agents just by typing
- **Everything you'd expect** — scrollback/copy-mode, mouse (wheel forwarded to mouse-aware apps), shared multi-client, all configurable via `~/.config/copad/mux.toml`

### Panels

- **Terminal panel** — VTE4 on Linux, `alacritty_terminal` + custom AppKit/CoreText renderer on macOS; PTY handled internally on both platforms
- **WebView panel** — WebKitGTK 6.0 (Linux) / WKWebView (macOS) as a first-class panel; URL toolbar, DevTools toggle, side-by-side with terminals
- **Plugin panels** — HTML/JS panels loaded from `~/.config/copad/plugins/` with an injected `copad` JS bridge for socket calls and event subscriptions
- **Status bar** — Waybar-style 3-zone bar (left/center/right) populated by plugin modules

### Control API

- **`coctl` CLI** — full programmatic control over tabs, splits, panels, terminals, webviews, plugins, and the event stream
- **Unix socket** per GUI instance, newline-delimited JSON — `$XDG_RUNTIME_DIR/copad/gui-{PID}.sock` on Linux, `/tmp/copad-{PID}.sock` on macOS (hardened relocation pending); injected as `COPAD_SOCKET`, both forms auto-discovered by `coctl`
- **Event stream** — `event.subscribe` for live `terminal.output`, `panel.focused`, `tab.created`, `webview.navigated`, plus all bus events
- **Terminal agent API** — `terminal.read` / `state` / `exec` / `feed` / `history` / `context` for AI agents
- **Approval workflow** — `agent.approve` shows a modal and returns the user's choice
- **`claude.start`** — spawn a Claude Code session inside a tmux session in a target worktree

### Workflow Runtime

- **Event Bus** — pub/sub with glob patterns, bounded delivery, drop-newest overflow
- **Action Registry** — name → handler map; the same registry serves CLI dispatch, plugin RPC, and triggers
- **Context Service** — active panel, per-panel cwd cache, snapshots; exposed via `context.snapshot`
- **Trigger Engine** — declarative `[[triggers]]` in `config.toml` (`when.event_kind` glob + payload match, `condition` DSL, `action` + `params`, optional `await` for chained correlation, or `cron` for schedules); fires actions on bus events with `{event.*}` / `{context.*}` interpolation; hot-reloads with subscriber reconciliation

### First-party Plugins

`plugins/<name>/` — install with `./scripts/install-plugins.sh`. Each plugin directory holds the Rust crate (`Cargo.toml` + `src/`) and its runtime manifest/assets (`plugin.toml`, `panel.html`, `triggers.example.toml`) together. All plugins implement the service-plugin protocol (newline-JSON over stdio, supervised by copad).

| Plugin       | Purpose                                                                     |
| ------------ | --------------------------------------------------------------------------- |
| `kb`         | Grep + filename search and atomic read/append/ensure over `~/docs`          |
| `docs`       | KB panel — read + navigate `~/docs` over `dn`'s incremental indices         |
| `calendar`   | Google Calendar event polling with lead-time dedupe                         |
| `slack`      | Slack Socket Mode — mention/DM/reaction events + `chat.postMessage`         |
| `discord`    | Discord gateway — mention/DM/reaction events + `send_message`               |
| `jira`       | Jira Cloud polling (assigned/comments/transitions) + read/write actions     |
| `llm`        | Anthropic Messages API client with JSONL usage log                          |
| `todo`       | Markdown-checkbox todos in `~/docs/todos/<workspace>/` (vim/git compatible) |
| `git`        | Worktree create/remove + branch / status queries                            |
| `claude`     | Read-only views over `~/.claude` harness artifacts (handoffs, sessions)     |
| `pilot`      | Autonomous goal queue — drives detached Claude sessions via `csd`           |
| `web-bridge` | HTTP+WS cockpit (panes, attention, pilot queue) for browser / phone         |
| `bookmark`   | Bookmark capture with dedupe store                                          |
| `echo`       | Reference / E2E plugin                                                      |

### Platforms

- **Linux** — GTK4 + VTE4, full feature set
- **macOS** — Swift/AppKit + `alacritty_terminal`, rendered on a **Metal** GPU path by default (~5.5× cheaper main-thread render; CoreText kept as the `gpu = false` fallback). Full secondary platform: terminal, tabs, splits, search, themes, webview, plugins, status bar, keybindings, background images, AI agent API, daemon-client. See [`docs/macos-app.md`](./docs/macos-app.md).
- **iOS / mobile** — `copad-ios`, a SwiftUI + WKWebView native shell around the `web-bridge` PWA: attach to a terminal, agent presence/attention, and push, over Tailscale or an SSH tunnel. See [`docs/mobile-access.md`](./docs/mobile-access.md).

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

### Linux & macOS — GitHub Releases (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/marshallku/copad/master/install.sh | bash
```

One command for both platforms — it detects the OS and installs the matching release: on **Linux** the GTK app + `coctl`/`comux`; on **macOS** (Apple Silicon) `Copad.app` + `coctl`/`copadd`/`comux` + all first-party plugins + the `copadd` LaunchAgent (quarantine stripped for the ad-hoc-signed bundle).

Options (pass after `bash -s --` when piping): `--version vX.Y.Z` to pin a release, `--system` to install system-wide (`/usr/local/bin`, `/Applications`; requires sudo).

### Just comux (the multiplexer, standalone)

`comux` is a self-contained tmux-style multiplexer with no GTK/daemon dependency. To install only it:

```bash
curl -fsSL https://raw.githubusercontent.com/marshallku/copad/master/install-comux.sh | bash
```

Downloads a single `comux` binary (Linux x86_64 / macOS arm64) into `~/.local/bin` (`--system` for `/usr/local/bin`). The status-bar usage readout needs `coctl` on PATH; everything else works alone.

### Linux — From source

```bash
./scripts/install-dev.sh           # build + install everything to ~/.local/bin (no sudo)
./scripts/install-dev.sh --system  # /usr/local/bin instead of ~/.local/bin (requires sudo)
./scripts/install-dev.sh --restart # also pkill -x copad afterwards
```

Builds a release binary, installs the desktop entry, and lays down all first-party plugins via `install-plugins.sh`.

### macOS — Homebrew

```bash
brew install --cask marshallku/copad/copad
```

Installs `Copad.app` to `/Applications`, `coctl` + `copadd` into `$(brew --prefix)/bin`, and lays out the 14 macOS plugins, shell hooks, and the `copadd` LaunchAgent (auto-starts at login). Requires Apple Silicon. The tap repo is [marshallku/homebrew-copad](https://github.com/marshallku/homebrew-copad).

**Supported macOS versions:** 14 (Sonoma), 15 (Sequoia). On **macOS 26 (Tahoe) and later**, ad-hoc-signed releases break — Tahoe's tightened App Verification policy deletes ad-hoc-signed executables on first `launchd` spawn, which removes `copadd` and the plugin binaries. The proper fix — Developer ID signing of every binary + notarization of `Copad.app` — is implemented in the release CI (`.github/workflows/release.yml`) and **activates automatically once the signing secrets are configured** (see [docs/macos-signing-notarization.md](docs/macos-signing-notarization.md)); releases cut before that fall back to ad-hoc. Until a signed release is published, Tahoe users should install from source via `scripts/install-macos.sh` (next section), which signs with a locally-trusted self-signed identity (`scripts/codesign-dev.sh`) and survives Tahoe's policy.

### macOS — From source

```bash
./scripts/install-macos.sh             # ~/Applications + ~/.cargo/bin (no sudo)
./scripts/install-macos.sh --system    # /Applications + ~/.cargo/bin (sudo for /Applications)
./scripts/install-macos.sh --launch    # open Copad.app after installing
```

Builds `libcopad_ffi.a` (Rust staticlib) → links into the SwiftPM release build → stages and atomically installs `Copad.app` → installs `coctl` via `cargo install --path copad-cli`. Use this on Intel Macs (the Homebrew cask is arm64-only) or when iterating on the working tree.

### Plugins only

```bash
./scripts/install-plugins.sh           # install all first-party plugins
./scripts/install-plugins.sh todo git  # install just these
```

Restart copad after installing/updating plugins — `discover_plugins()` only runs at startup.

### Update

```bash
coctl update check    # check for new versions
coctl update apply    # download and install latest (Linux only — macOS users re-run install.sh, brew upgrade --cask, or install-macos.sh)
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
# image = "/path/to/wallpaper.jpg"   # static image (rotation replaces it at the first tick)
# rotate_interval = 300              # seconds between random wallpapers from the platform list; 0 = off
tint = 0.85       # tint overlay opacity (0.0–1.0)
opacity = 0.95    # background-image opacity

[tabs]
position = "left"   # top, bottom, left, right
collapsed = true    # start with tab bar collapsed (icon-only)
width = 200         # tab bar width for vertical positions

[theme]
name = "catppuccin-mocha"

[keybindings]
"ctrl+shift+g" = "spawn:~/scripts/wallpaper.sh --next"
"ctrl+shift+m" = "spawn:~/.local/bin/coctl background toggle"

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
├── copad-macos/               # Swift/AppKit + alacritty_terminal app (Copad.app)
├── copad-ios/                 # SwiftUI + WKWebView mobile shell over the web-bridge PWA
├── copad-term/                # Rust staticlib wrapping alacritty_terminal for the macOS renderer
├── copad-cli/                 # CLI control tool (binary: coctl)
├── copad-daemon/              # Background daemon (binary: copadd) — triggers, plugins, web-bridge
├── copad-mux/                 # Standalone terminal multiplexer (binary: comux)
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
