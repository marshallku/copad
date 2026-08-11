# copad — Overview

**copad** is the desktop terminal app: GTK4 + VTE4 on Linux, Swift/AppKit with a Metal-accelerated renderer on macOS. Beyond a normal emulator it carries a workflow runtime (event bus, action registry, context service, trigger engine) and a plugin system, and it's fully scriptable through the [`coctl`](../coctl/index.md) CLI.

## Launching copad

### Linux (`copad`)

Run `copad`, or launch it from your application menu. The GTK application id is `com.marshall.copad`; the default window is 1200×800 and forces a dark theme.

Make it your GNOME default terminal:

```bash
gsettings set org.gnome.desktop.default-applications.terminal exec copad
```

From source: `cargo run -p copad-linux` (append `-- --init-config` to write a default config, or `-- --config-path` to print the config path).

### macOS (`Copad.app`)

Open `Copad.app` from `/Applications` or `~/Applications` — via Spotlight, the Dock, or `open -a Copad`. For a dev loop from source, `cd copad-macos && ./run.sh` builds and opens a fresh debug bundle.

Supported: macOS 14 (Sonoma) and later — including 26 (Tahoe) from release v1.0.1, where every shipped binary is Developer ID signed and `Copad.app` is notarized. See the [installation notes](../installation.md#macos-via-homebrew).

---

## Features

- **Tabs** — drag to reorder, double-click to rename, a collapsible icon-only tab bar (toggle with `Ctrl+Shift+B`), and a configurable position (top / bottom / left / right) with a settable vertical width.
- **Splits** — horizontal and vertical split panes, drag-to-resize, focus tracking. Terminal and webview panels can mix in the same split tree.
- **Webview / panels** — open a blank webview pane, or an HTML/JS **plugin panel** with an injected `copad` JS bridge for socket calls and event subscriptions. WKWebView on macOS, WebKitGTK on Linux.
- **Background images** — a GPU-composited wallpaper behind the terminal with tint + opacity, random rotation from a directory or list, all controllable via `coctl background …`. See [Themes & Backgrounds](./themes-and-backgrounds.md).
- **Themes** — 10 built-ins, hot-reloading on save.
- **Search** — in-terminal regex search (`Ctrl+Shift+F` / `Cmd+F`), case toggle, wrap-around.
- **Font scaling** — `Ctrl+=` / `Ctrl+-` / `Ctrl+0` (Linux), `Cmd`-equivalents on macOS.
- **Command palette** — `Ctrl+Shift+P` / `Cmd+Shift+P` to filter and dispatch any registered action.
- **Agent cockpit** — `Ctrl+Shift+Y` (Linux) or `coctl call cockpit.open`: a pane list with live AI-agent status.
- **Programmable control** — the `coctl` CLI plus a per-instance Unix socket, an event bus, a trigger engine, and plugins.

See [Keybindings & Shortcuts](./keybindings.md) for the full list.

---

## The config file

copad reads `~/.config/copad/config.toml` on **both** platforms. It's entirely optional (every field has a default) and **hot-reloads** — save it and copad applies font, background, tint, tab position, keybinding, theme, and OSC 52 changes live.

```bash
copad --init-config     # write a default config.toml
copad --config-path     # print where it lives
```

The full key-by-key reference is in [Configuration](./configuration.md).

---

## Linux vs macOS

Both apps share the same config file and core, but a few things differ:

| Area | Linux (`copad`) | macOS (`Copad.app`) |
| --- | --- | --- |
| Engine | GTK4 + VTE4 | AppKit + `alacritty_terminal`, Metal GPU renderer (CoreText fallback) |
| Shortcut modifier | `Ctrl+Shift+…` | `Cmd+…` |
| New web tab | `Ctrl+Shift+U` | `Cmd+Shift+T` |
| `[window] background` (solid base color) | Supported | Ignored |
| `[window] blur` | No-op (compositor's job) | Supported (NSVisualEffectView) |
| `[background] image` = a directory | Supported (native rotation) | Falls back to the list file |
| `[security]` / `[renderer]` config sections | Not parsed | macOS-only, functional |
| Session layout restore on launch | Yes (`session.json`) | Not currently |
| `coctl update apply` | Supported | Re-run the installer instead |

See [Configuration](./configuration.md) for what each of those keys does.
