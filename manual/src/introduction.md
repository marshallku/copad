# copad Manual

> **The terminal as an orchestration surface — for your shells, your plugins, and your AI agents.**

**copad** is a cross-platform terminal built on a shared Rust core with platform-native UIs — GTK4 on Linux, Swift/AppKit on macOS, and a SwiftUI shell on iOS. It's more than an emulator: an event bus, action registry, trigger engine, and plugin system turn the terminal into one programmable surface where shells, AI agents, calendars, notes, Slack, and todos compose and react to one another.

This manual is the **end-user guide**. If you're looking for the internals — architecture, design decisions, the roadmap — those live in the repository's `docs/` directory, not here.

## The three binaries

copad ships as a small family of tools. You can use them together or entirely on their own.

| Binary | What it is | Start here |
| --- | --- | --- |
| **`comux`** | A standalone agent-orchestration terminal **multiplexer** (like tmux, built for running teams of `claude` / `codex` sessions). Installs as a single binary with no other dependencies. | [comux Overview](./comux/index.md) |
| **`copad`** | The **desktop terminal** app — GTK4 on Linux, `Copad.app` on macOS. Tabs, splits, webview panels, background images, themes, plugins. | [copad Overview](./copad/index.md) |
| **`coctl`** | The **control CLI** — drives every pane, tab, panel, and plugin of a running `copad` from a script, plus local readouts like `coctl usage`. | [coctl Overview](./coctl/index.md) |

There is also **`copadd`**, a background daemon that powers agent notifications, the cockpit, triggers, and update checks. You rarely invoke it directly — the installers set it up as a systemd `--user` unit (Linux) or LaunchAgent (macOS).

## Which one do I want?

- **"I just want a better tmux for my AI agents."** → Install **comux** alone. One command, one binary, no GUI. See [Installation](./installation.md#comux-only).
- **"I want the full graphical terminal."** → Install **copad** (which also brings `coctl`, `comux`, and `copadd`). See [Installation](./installation.md#full-copad).
- **"I want to script my terminal."** → You already have **coctl** if you installed copad. See the [Command Reference](./coctl/reference.md).

## Platforms at a glance

| | Linux | macOS | iOS |
| --- | --- | --- | --- |
| `copad` desktop app | ✅ GTK4 + VTE4 | ✅ AppKit + Metal renderer | — |
| `comux` multiplexer | ✅ (glibc + static musl) | ✅ (Apple Silicon) | — |
| `coctl` CLI | ✅ | ✅ | — |
| Mobile client | — | — | ✅ SwiftUI shell over the web-bridge PWA |

Ready to install? → **[Installation](./installation.md)**
