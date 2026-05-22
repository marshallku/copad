# Architecture

## Overview

nestty is a cross-platform custom terminal emulator built with a shared Rust core and platform-native UIs. Originally planned with Tauri v2 + React, but pivoted to native UIs due to Tauri IPC input latency (see [decisions.md](./decisions.md)).

## Crate Layout

```
nestty/
├── Cargo.toml              # Workspace root (resolver = "2", edition = "2024")
├── plugins/                  # First-party plugins. Each subdir holds the Rust crate
│   │                            # (`Cargo.toml` + `src/`) AND its runtime manifest /
│   │                            # assets (`plugin.toml`, `panel.html`, `triggers.example.toml`)
│   │                            # side-by-side. Crate names remain `nestty-plugin-<name>`.
│   ├── echo/                 # Mock service plugin (verifies protocol shape)
│   │   └── src/main.rs            # newline-JSON over stdio: echo.ping + system.heartbeat
│   ├── kb/                   # First-party KB plugin (grep + filename over ~/docs)
│   │   └── src/                    # main.rs (RPC loop), kb.rs (4 actions + atomic IO)
│   ├── calendar/             # First-party Google Calendar plugin (Unix only — Linux + macOS)
│   │   └── src/                    # main.rs (RPC + auth subcommand), config.rs (env),
│   │                                # store.rs (keyring + plaintext fallback), oauth.rs
│   │                                # (device-code flow + refresh), gcal.rs (events.list),
│   │                                # poller.rs (lead-time dedupe), event.rs (payload mapping)
│   ├── slack/                # First-party Slack Socket Mode plugin (Unix only)
│   │   └── src/                    # main.rs (RPC + auth subcommand), config.rs (env),
│   │                                # store.rs (two-token keyring + plaintext fallback),
│   │                                # socket_mode.rs (apps.connections.open + tungstenite
│   │                                # WebSocket loop + reconnect + chat.postMessage),
│   │                                # events.rs (Slack → slack.mention / slack.dm /
│   │                                # slack.raw mapping with filtering)
│   ├── llm/                  # First-party LLM plugin (Anthropic provider, Unix only)
│   │   └── src/                    # main.rs (RPC + auth subcommand), config.rs (env),
│   │                                # store.rs (single-token keyring + plaintext fallback),
│   │                                # anthropic.rs (Messages API client), usage.rs
│   │                                # (JSONL append-only usage log + aggregation)
│   ├── todo/                 # First-party Todo plugin (markdown-checkbox files at
│   │   │                            # ~/docs/todos/<workspace>/<id>.md, vim/git compatible)
│   │   └── src/                    # main.rs (RPC loop + actions), config.rs (env),
│   │                                # todo.rs (Todo struct + frontmatter parse/render +
│   │                                # surgical update_status_in_text), store.rs (atomic
│   │                                # create via nestty_core::fs_atomic::rename_no_replace —
│   │                                # Linux renameat2(RENAME_NOREPLACE) / macOS
│   │                                # renamex_np(RENAME_EXCL); list_all, set_status,
│   │                                # delete; mirrors KB security posture), watcher.rs
│   │                                # (poll-based diff emitting todo.created/changed/
│   │                                # completed/deleted)
│   ├── git/                  # First-party git workspace + worktree plugin
│   │   │                            # (lightweight: argv-vector shell-outs to `git`, no
│   │   │                            # external API, no keyring; cross-platform Linux+macOS)
│   │   └── src/                    # main.rs (RPC + actions + worktree_add.completed event),
│   │                                # config.rs (~/.config/nestty/workspaces.toml loader with
│   │                                # canonicalization), git.rs (current_branch, list_worktrees
│   │                                # porcelain v2 parser, worktree_add/remove, status v2 parser,
│   │                                # validate_branch_name)
│   └── web-bridge/           # First-party HTTP+WS broker plugin (Slice 3.0 remote-harness).
│       │                            # Service-plugin model (stdio RPC minimal) + own axum
│       │                            # tokio listener + raw daemon-socket client (RPC + subscribe)
│       │                            # via NESTTY_SOCKET (supervisor-injected). Bearer-token
│       │                            # auth (Authorization header / Sec-WebSocket-Protocol).
│       │                            # Localhost bind by default — external access (Tailscale /
│       │                            # SSH tunnel / cloudflared) is the user's perimeter.
│       │── src/main.rs        # stdio handshake + token validation + tokio runtime spawn
│       │── src/daemon_client.rs   # rpc() async UnixStream; subscribe() sync UnixStream via
│       │                            # spawn_blocking (matches nestctl wire model)
│       │── plugin.toml        # onStartup, provides=[], env docs
│       └── static/index.html  # mobile-first vanilla SPA — include_str!'d into binary
# claude.start: nestty-internal socket action (lives in nestty-linux/src/socket.rs).
# Spawns a tab whose terminal cwd is the worktree, feeds
# `tmux new-session -A -s <name> 'claude [--resume <id>]'` into it.
# Returns {panel_id, tab, tmux_session, workspace_path}.
# Slice 1 ships without `prompt` seeding (interactive REPL stdin is
# tricky); pass `prompt` and you'll get not_implemented.
├── nestty-core/            # Shared Rust library
│   └── src/
│       ├── lib.rs              # Module declarations
│       ├── config.rs           # TOML config loading/defaults
│       ├── plugin.rs           # Plugin manifest types + discovery
│       ├── protocol.rs         # cmux V2 JSON protocol types
│       ├── error.rs            # Error types (thiserror)
│       ├── event_bus.rs        # Pub/sub bus with glob patterns + subscriber receivers
│       ├── action_registry.rs  # Name → handler map (sync + blocking variants)
│       ├── context.rs          # ContextService — active panel, cwd cache, snapshots
│       ├── trigger.rs          # TriggerEngine + TriggerSink trait + condition matching
│       ├── condition.rs        # Trigger condition DSL parser/evaluator
│       ├── theme.rs            # 10 built-in Catppuccin/Solarized/etc. palettes
│       └── fs_atomic.rs        # Cross-platform atomic-create-or-fail rename
│                               #   (Linux: renameat2(RENAME_NOREPLACE);
│                               #    macOS: renamex_np(RENAME_EXCL))
├── nestty-linux/           # GTK4 + VTE4 native terminal
│   ├── src/
│   │   ├── main.rs          # Entry point, CLI flags (--init-config, --config-path)
│   │   ├── app.rs           # GtkApplication setup, dark theme
│   │   ├── window.rs        # ApplicationWindow, root Overlay, D-Bus polling
│   │   ├── background.rs    # Window-level BackgroundLayer (image + tint)
│   │   ├── terminal.rs      # VTE terminal (always transparent, composites over BackgroundLayer)
│   │   ├── tabs.rs          # Tab manager (Notebook, tab bar, keyboard shortcuts)
│   │   ├── split.rs         # Split pane tree (SplitNode, TabContent)
│   │   ├── search.rs        # In-terminal search bar (Ctrl+Shift+F, VTE regex search)
│   │   ├── panel.rs         # Panel trait + PanelVariant enum
│   │   ├── webview.rs       # WebView panel (WebKitGTK 6.0)
│   │   ├── plugin_panel.rs  # Plugin panel (WebView + JS bridge)
│   │   ├── service_supervisor.rs  # Service plugin host: spawn/restart, init handshake, RPC
│   │   ├── statusbar.rs     # Waybar-style status bar (WebView + plugin modules)
│   │   └── socket.rs        # Unix socket server + command dispatcher
│   ├── com.marshall.nestty.desktop  # Desktop entry — basename matches app_id (Wayland window↔launcher mapping)
│   ├── icons/hicolor/<size>/apps/nestty.png  # Pre-built theme icons (16,22,24,32,48,64,128,256,512)
│   └── install.sh           # Build + install script
├── nestty-cli/             # CLI control tool (binary: nestctl)
│   └── src/
│       ├── main.rs          # Entry point, output formatting
│       ├── commands.rs      # clap subcommands (session, background, tab, split, event, webview)
│       └── client.rs        # Unix socket client
└── nestty-macos/           # Swift/AppKit native terminal (Phases 1–3 complete)
    ├── Package.swift        # Swift Package Manager config (Swift 6, macOS 14+, SwiftTerm dep)
    └── Sources/Nestty/
        ├── NesttyApp.swift            # @main entry point
        ├── AppDelegate.swift        # NSApplicationDelegate, menu bar, socket command routing
        ├── TabViewController.swift  # Tab list manager, PaneManager array
        ├── TabBarView.swift         # Custom tab bar + add-panel popover
        ├── PaneManager.swift        # Split-pane tree for a single tab
        ├── SplitNode.swift          # N-ary split tree (any NesttyPanel leaves)
        ├── NesttyPanel.swift          # Common protocol for terminal + webview panels
        ├── TerminalViewController.swift  # SwiftTerm wrapper, shell, delegates
        ├── WebViewController.swift  # WKWebView wrapper, NesttyPanel impl
        ├── EventBus.swift           # Event broadcast hub + per-subscriber channel
        ├── SocketServer.swift       # POSIX Unix socket server (async completion handler)
        ├── Config.swift             # TOML config parser (shell, font, theme, background)
        └── Theme.swift              # 10 built-in themes (mirrors nestty-core/theme.rs)
```

## Tech Stack

| Component       | Technology                                                                                                                             |
| --------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Core library    | Rust (shared across platforms)                                                                                                         |
| Linux terminal  | GTK4 + VTE4 (VTE handles PTY internally, zero IPC overhead)                                                                            |
| macOS terminal  | Swift/AppKit + SwiftTerm (LocalProcessTerminalView)                                                                                    |
| CLI tool        | clap (Rust)                                                                                                                            |
| Config          | TOML (`~/.config/nestty/config.toml`)                                                                                                  |
| IPC             | Unix domain socket, cmux V2 newline-delimited JSON                                                                                     |
| Background mgmt | File cache at `~/.cache/terminal-wallpapers.txt` (Linux) or `~/Library/Caches/nestty/wallpapers.txt` (macOS, falls back to Linux path) |
| Theme           | Catppuccin Mocha (hardcoded palette)                                                                                                   |

## Key Dependencies

### nestty-core

- `serde 1` + `serde_json 1` + `toml 0.8` - Serialization
- `dirs 6` - XDG directories
- `thiserror 2` - Error types

### nestty-linux

- `gtk4 0.9` (features: `gnome_46`) - UI framework
- `vte4 0.8` - Terminal widget (libvte-2.91-gtk4)
- `webkit6 0.4` - WebView panel (WebKitGTK 6.0)
- `env_logger 0.11` - Logging

### nestty-cli

- `clap 4` (features: `derive`) - Argument parsing
- `uuid 1` - Request IDs

## Socket Server (IPC)

nestty runs a Unix domain socket server for programmatic control alongside D-Bus.

**Path**: `/tmp/nestty-{PID}.sock` (per-process, discovered via `NESTTY_SOCKET` env var)

**Protocol**: Newline-delimited JSON (`Request` → `Response`, defined in `nestty-core/protocol.rs`)

**Architecture**:

```
nestctl ──Unix socket──► socket server (per-client thread)
                                │
                          mpsc::channel
                                │
                          glib::timeout_add_local (50ms poll on GTK thread)
                                │
                          dispatch() ──► TabManager / TerminalPanel
                                │
                          oneshot response ──► socket thread ──► client
```

**Supported commands**: `system.ping`, `system.log`, `context.snapshot`, `background.set`, `background.clear`, `background.set_tint`, `background.next`, `background.toggle`, `tab.new`, `tab.close`, `tab.list`, `tab.info`, `tab.rename`, `tabs.toggle_bar`, `split.horizontal`, `split.vertical`, `session.list`, `session.info`, `event.subscribe`, `terminal.read`, `terminal.state`, `terminal.exec`, `terminal.feed`, `terminal.history`, `terminal.context`, `agent.approve`, `claude.start`, `theme.list`, `plugin.list`, `plugin.open`, `plugin.<name>.<cmd>`, `webview.open`, `webview.navigate`, `webview.back`, `webview.forward`, `webview.reload`, `webview.execute_js`, `webview.get_content`, `webview.screenshot`, `webview.query`, `webview.query_all`, `webview.get_styles`, `webview.click`, `webview.fill`, `webview.scroll`, `webview.page_info`, `webview.devtools`, `statusbar.show`, `statusbar.hide`, `statusbar.toggle`. Plus any action declared by a service plugin via `[[services]] provides` (e.g. `echo.ping`, `kb.search`) — registered in the same `ActionRegistry` and reachable through socket dispatch's registry-first lookup.

**Cleanup**: Socket file removed on window destroy.

## Event Stream

Clients can subscribe to real-time events via `event.subscribe`. The socket stays open and streams newline-delimited JSON events.

**Protocol**: Send `{"id":"...","method":"event.subscribe","params":{}}`, receive `{"id":"...","ok":true,"result":{"status":"subscribed"}}`, then receive event lines indefinitely.

**Event format**: `{"event":"<event_type>","data":{...}}`

**Event types**:
| Event | Data | Trigger |
|-------|------|---------|
| `panel.focused` | `{panel_id}` | Panel gains focus |
| `panel.title_changed` | `{panel_id, title}` | Terminal window title changes |
| `panel.exited` | `{panel_id, tab}` | Shell process exits |
| `tab.created` | `{tab, panel_id, panel_type}` | New tab opened |
| `tab.closed` | `{index}` | Tab closed |
| `terminal.output` | `{panel_id, text}` | Terminal receives output (high frequency) |
| `terminal.cwd_changed` | `{panel_id, cwd}` | Terminal CWD changes (OSC 7) |
| `terminal.shell_precmd` | `{panel_id}` | Shell prompt ready (precmd) |
| `terminal.shell_preexec` | `{panel_id}` | Command about to execute (preexec) |
| `terminal.notification` | `{panel_id, summary, body}` | OSC 9/777 notification received |
| `webview.loaded` | `{panel_id}` | WebView finishes loading |
| `webview.title_changed` | `{panel_id, title}` | WebView title changes |
| `webview.navigated` | `{panel_id, url}` | WebView URI changes |
| `tab.renamed` | `{panel_id, title}` | Tab renamed |

**Usage**: `nestctl event subscribe` — prints events as JSON lines to stdout.

## Query API

**`session.list`**: Returns all panels across all tabs with `[{id, type, title, tab, focused, url?}]`. WebView panels include `url`.

**`session.info`** (`{id}`): Returns detailed panel info. Terminal: `{id, type, title, tab, focused, cols, rows, cursor: [row, col]}`. WebView: `{id, type, title, tab, focused, url}`.

**`tab.info`**: Returns extended tab info: `{count, current, tabs: [{index, panel_count, title}]}`.

## Terminal Agent API

Commands for programmatic terminal interaction (AI agent integration).

| Command            | Params                                                    | Response                                                  |
| ------------------ | --------------------------------------------------------- | --------------------------------------------------------- |
| `terminal.read`    | `id?`, `start_row?`, `start_col?`, `end_row?`, `end_col?` | `{text, cursor: [row, col], rows, cols}`                  |
| `terminal.state`   | `id?`                                                     | `{cols, rows, cursor: [row, col], cwd, title}`            |
| `terminal.exec`    | `id?`, `command`                                          | Sends command + newline to terminal PTY                   |
| `terminal.feed`    | `id?`, `text`                                             | Sends raw text to terminal PTY (no newline)               |
| `terminal.history` | `id?`, `lines?` (default 100)                             | `{text, lines_requested, rows, cols}` — scrollback buffer |
| `terminal.context` | `id?`, `history_lines?` (default 50)                      | `{state, screen, history}` — combined context             |

All commands default to the active terminal panel when `id` is omitted.

**CLI usage**:

```bash
nestctl terminal state
nestctl terminal read --start-row 0 --end-row 5
nestctl terminal exec "ls -la"
nestctl terminal feed $'\x03'  # Send Ctrl+C
nestctl terminal history --lines 200
nestctl terminal context --history-lines 100
```

## Approval Workflow

AI agents can request user approval before taking actions.

| Command         | Params                          | Response                    |
| --------------- | ------------------------------- | --------------------------- |
| `agent.approve` | `message`, `title?`, `actions?` | `{approved, action, index}` |

Shows a modal GTK dialog and blocks until the user responds. The `actions` param is an array of button labels (default: `["Approve", "Deny"]`). The first action (index 0) is treated as "approved".

**CLI usage**:

```bash
nestctl agent approve "Delete 15 files from /tmp?"
nestctl agent approve "Deploy to production?" --title "Deploy" --actions "Deploy,Cancel"
```

## Panel System

nestty supports multiple panel types via the `PanelVariant` enum:

- **Terminal** (`TerminalPanel`): VTE4 terminal with shell, background images, search
- **WebView** (`WebViewPanel`): WebKitGTK 6.0 browser panel with JS execution, URL toolbar (back/forward/reload/URL entry/DevTools toggle)

- **Plugin** (`PluginPanel`): WebView-based custom panel loaded from plugin HTML with injected `nestty` JS bridge

The `Panel` trait provides a common interface (`widget()`, `title()`, `panel_type()`, `grab_focus()`, `id()`). `PanelVariant` delegates to the inner type and provides `as_terminal()` / `as_webview()` / `as_plugin()` accessors.

### Tab Bar Controls

The tab bar has two modes: **collapsed** (icon-only, default) and **expanded** (icon + label + close button). Toggle with `Ctrl+Shift+B` or the toggle button in the tab bar. Tabs can be renamed by double-clicking the tab label or via socket API. Custom titles suppress auto-title updates from terminal/webview.

**Auto-expand**: When going from 1 to 2 tabs, the tab bar auto-expands. Once the user manually toggles, that preference is preserved. The tab bar is never fully hidden — collapsed mode shows panel type icons and a toggle button.

| Command           | Params        | Behavior                                       |
| ----------------- | ------------- | ---------------------------------------------- |
| `tabs.toggle_bar` | —             | Toggle tab bar visibility, returns `{visible}` |
| `tab.rename`      | `id`, `title` | Rename a tab by panel ID                       |

### WebView API

| Command               | Params                                     | Behavior                                          |
| --------------------- | ------------------------------------------ | ------------------------------------------------- |
| `webview.open`        | `url`, `mode?` (tab/split_h/split_v)       | Create webview panel, return panel_id             |
| `webview.navigate`    | `id`, `url`                                | Navigate existing webview                         |
| `webview.back`        | `id`                                       | Go back in history                                |
| `webview.forward`     | `id`                                       | Go forward in history                             |
| `webview.reload`      | `id`                                       | Reload page                                       |
| `webview.execute_js`  | `id`, `code`                               | Run JS, return result (async)                     |
| `webview.get_content` | `id`, `format?` (text/html)                | Get page content via JS (async)                   |
| `webview.screenshot`  | `id`, `path?`                              | Take screenshot (base64 PNG or save to file)      |
| `webview.query`       | `id`, `selector`                           | Query single DOM element (tag, text, rect, attrs) |
| `webview.query_all`   | `id`, `selector`, `limit?`                 | Query all matching elements                       |
| `webview.get_styles`  | `id`, `selector`, `properties`             | Get computed CSS styles for element               |
| `webview.click`       | `id`, `selector`                           | Click a DOM element                               |
| `webview.fill`        | `id`, `selector`, `value`                  | Type text into an input element                   |
| `webview.scroll`      | `id`, `selector?`, `x?`, `y?`              | Scroll to position or element                     |
| `webview.page_info`   | `id`                                       | Page metadata (title, dimensions, element counts) |
| `webview.devtools`    | `id`, `action?` (show/close/attach/detach) | Control WebKit DevTools inspector                 |

`webview.execute_js`, `webview.get_content`, `webview.screenshot`, and all DOM query/interaction commands use async dispatch — the reply sender is captured by the WebKit callback and sent when execution completes. DOM commands use pre-built JS snippets from `webview::js` module.

## Plugin System

Plugins extend nestty with custom panels (HTML/JS UIs) and commands (shell scripts).

**Plugin directory**: `~/.config/nestty/plugins/<plugin-name>/`

**Manifest** (`plugin.toml`):

```toml
[plugin]
name = "my-plugin"
title = "My Plugin"
version = "0.1.0"
description = "Example plugin"

[[panels]]
name = "main"
title = "My Panel"
file = "index.html"
icon = "applications-system-symbolic"

[[commands]]
name = "do-thing"
exec = "bash scripts/do-thing.sh"
description = "Does a thing"
```

**Architecture**: Plugin panels are WebViews (`PluginPanel`) loading local HTML files with an injected `nestty` JS bridge. The bridge uses WebKitGTK's `register_script_message_handler_with_reply` so `nestty.call()` returns a Promise that resolves with the dispatch result. Events are forwarded to the webview via `evaluate_javascript`.

**JS Bridge API** (injected into plugin webviews):

```javascript
window.nestty = {
    panel: { id, name, plugin },
    async call(method, params = {}) { ... },  // Call any nestty socket method
    on(type, callback) { ... },               // Listen for events
    off(type, callback) { ... },
};
```

**Theme CSS variables** are injected via `UserStyleSheet`: `--nestty-bg`, `--nestty-fg`, `--nestty-surface0/1/2`, `--nestty-overlay0`, `--nestty-text`, `--nestty-subtext0/1`, `--nestty-accent`, `--nestty-red`.

**Plugin modules** are small HTML widgets rendered in the status bar. Plugins declare `[[modules]]` in their manifest with `name`, `file`, `position` (left/center/right), and `order`. All modules are aggregated into a single WebView bar with its own `nestty` JS bridge.

**Plugin commands** run shell scripts in a thread with `NESTTY_SOCKET` and `NESTTY_PLUGIN_DIR` env vars. Params are piped as JSON to stdin, stdout is parsed as JSON for the response.

### Service plugins (long-running supervised subprocess)

`[[services]]` extends the per-call `[[commands]]` model with a long-running supervised subprocess that speaks newline-JSON over stdio. See [service-plugins.md](./service-plugins.md) for end-state vision and decisions.

```toml
[[services]]
name = "main"
exec = "nestty-plugin-echo"          # PATH or relative to plugin dir
activation = "onStartup"           # | "onAction:kb.*" | "onEvent:slack.*"
restart = "on-crash"               # | "always" | "never"
provides = ["echo.ping"]           # actions this service handles
subscribes = []                    # bus event-kind globs forwarded as event.dispatch
```

**Lifecycle.** Supervisor in `nestty-linux::service_supervisor` walks every enabled plugin's manifest in lexical `[plugin].name` order BEFORE spawning anything, builds the global action-ownership table, resolves `provides` conflicts (lexical-name winner takes the action; loser keeps its other registrations). Activation rules drive spawn timing: `onStartup` eager-spawns at boot; `onAction:` activates on first matching action call (request buffered up to 64 deep during `Starting`); `onEvent:` activates on first matching bus event.

**Init handshake.** nestty sends `initialize` with `{nestty_version, protocol_version}` (5s default timeout). Service replies with `{service_version, provides, subscribes}`. Asymmetric validation: every runtime entry must appear in the manifest (superset → drop with warn, subset → degraded mode OK). The negotiated runtime `provides` set is recorded BEFORE the state flips to `Running`, and `invoke_remote` gates dispatch against it — manifest-approved actions the runtime didn't claim return `service_degraded`, never reaching the running service. On init timeout, the supervisor SIGKILLs the recorded PID (best-effort) instead of relying on stdin EOF cooperation. Then nestty sends `initialized` notification, drains buffered invocations, and spawns one bus forwarder per accepted `subscribes` glob.

**Bidirectional RPC** over newline-JSON, both directions:

| Direction        | Method           | Notes                                                                                           |
| ---------------- | ---------------- | ----------------------------------------------------------------------------------------------- |
| nestty → service | `initialize`     | first message, awaits reply                                                                     |
| nestty → service | `initialized`    | notification (no id), ack of init                                                               |
| nestty → service | `action.invoke`  | service is the registered handler                                                               |
| nestty → service | `event.dispatch` | matches a `subscribes` pattern                                                                  |
| service → nestty | `event.publish`  | publishes to bus; nestty fills source/timestamp                                                 |
| service → nestty | `action.invoke`  | call ANOTHER service's action; runs on a worker thread to keep the reader free for nested calls |
| service → nestty | `log`            | stderr-style logging routed via nestty                                                          |

**Restart.** Exponential backoff on crash: 1s → 2s → 4s … capped at 60s. Reset to 1s on successful init. Policies: `on-crash` (default), `always`, `never`.

**Threading per running service.** Writer thread (drains outgoing channel into child stdin), reader thread (parses child stdout, dispatches frames), stderr-tail thread (logs), wait thread (observes exit, triggers restart). Plus one forwarder thread per accepted `subscribes` pattern bridging the bus into the outgoing channel.

**E2E verification** uses `nestty-plugin-echo` (workspace member): registers `echo.ping`, publishes `system.heartbeat` every `NESTTY_ECHO_HEARTBEAT_SECS` seconds (default 30). `nestctl call echo.ping --params '{...}'` round-trips params through socket → registry → service. `nestctl event subscribe` shows the heartbeat. `pkill -KILL nestty-plugin-echo` triggers supervisor restart, after which the next `echo.ping` works again.

| Command               | Params                               | Behavior                                    |
| --------------------- | ------------------------------------ | ------------------------------------------- |
| `plugin.list`         | —                                    | List installed plugins with panels/commands |
| `plugin.open`         | `plugin`, `panel?` (default: "main") | Open a plugin panel in a new tab            |
| `plugin.<name>.<cmd>` | arbitrary JSON                       | Run a plugin shell command                  |

**CLI usage**:

```bash
nestctl plugin list
nestctl plugin open my-plugin
nestctl plugin open my-plugin --panel settings
nestctl plugin run my-plugin.do-thing --params '{"key": "value"}'
```

## App Icon

Single master at `assets/icons/nestty.png` (1024x1024, palette PNG). `scripts/build-icons.sh` regenerates the per-platform artifacts from it:

- `nestty-linux/icons/hicolor/<size>x<size>/apps/nestty.png` — sizes 16/22/24/32/48/64/128/256/512. Installed under `share/icons/hicolor/...` by `install-dev.sh` and the end-user `install.sh`. The desktop entry's `Icon=nestty` plus `gtk4::Window::set_default_icon_name("nestty")` in `app.rs` make the WM/launcher pick them up.
- The desktop entry is named `com.marshall.nestty.desktop` (matches the GTK `application_id`) and carries `StartupWMClass=com.marshall.nestty`. Wayland compositors (Hyprland/GNOME Shell/KWin) match windows to launcher entries by `app_id ↔ desktop filename`; X11 falls back to `WM_CLASS ↔ StartupWMClass`. Without one of these, the WM shows a generic icon on the running window even when the launcher has the branded one.
- `nestty-macos/Resources/AppIcon.icns` — multi-resolution `.icns` carrying PNG-encoded entries for icp4..icp6, ic07..ic14 (16, 32, 64, 128, 256, 512, 1024 plus retina @2x variants). Copied into `Contents/Resources/AppIcon.icns` by `scripts/install-macos.sh` and `nestty-macos/run.sh`; `Info.plist`'s `CFBundleIconFile=AppIcon` ties it in.

Both bundles are checked in so a fresh clone builds with icons even on hosts without ImageMagick. Run the build script and commit the regenerated files whenever the master changes.

## System Prerequisites

### Arch Linux

```bash
sudo pacman -S gtk4 vte4 webkitgtk-6.0 gst-plugins-good gst-plugins-bad
```

- `gst-plugins-good` / `gst-plugins-bad`: Required by WebKitGTK for media playback. Without these, the WebKit web process crashes on many sites.

### macOS

- Xcode with Swift 6
