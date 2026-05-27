# Plugin Development Guide

## Overview

copad plugins extend the terminal with custom panels (HTML/JS UIs) and commands (shell scripts). Plugins live in `~/.config/copad/plugins/` and are discovered automatically at startup.

## Plugin Structure

```
~/.config/copad/plugins/my-plugin/
├── plugin.toml          # Manifest (required)
├── index.html           # Panel UI
├── styles.css           # Panel styles
├── main.js              # Panel logic
└── scripts/
    └── do-thing.sh      # Shell command
```

## Manifest (`plugin.toml`)

```toml
[plugin]
name = "my-plugin"        # Unique identifier (kebab-case)
title = "My Plugin"        # Display name
version = "0.1.0"
description = "What this plugin does"

# Panels are HTML UIs rendered in WebView tabs
[[panels]]
name = "main"                              # Panel identifier
title = "My Panel"                         # Tab title
file = "index.html"                        # HTML file to load (relative to plugin dir)
icon = "applications-system-symbolic"      # GTK icon name (optional)

# Commands are shell scripts callable via the socket API
[[commands]]
name = "do-thing"                          # Command identifier
exec = "bash scripts/do-thing.sh"          # Shell command to run
description = "Does a thing"               # Optional description

# Modules are widgets rendered in the status bar (data from shell, styled with CSS)
[[modules]]
name = "clock"                             # Module identifier
exec = "date '+%H:%M:%S'"                  # Shell command (stdout → module text)
interval = 1                               # Re-run interval in seconds (default: 10)
position = "right"                         # left, center, right
order = 100                                # Sort order within section (lower = first)
class = "clock"                            # CSS class for styling (optional)
```

### Multiple Panels

A plugin can define multiple panels:

```toml
[[panels]]
name = "main"
title = "Dashboard"
file = "index.html"

[[panels]]
name = "settings"
title = "Settings"
file = "settings.html"
icon = "preferences-system-symbolic"
```

Open a specific panel: `coctl plugin open my-plugin --panel settings`

## JS Bridge API

Every plugin panel gets a `window.copad` object injected automatically.

### `copad.panel`

Info about the current panel:

```javascript
copad.panel.id       // UUID of this panel instance
copad.panel.name     // Panel name from manifest (e.g., "main")
copad.panel.plugin   // Plugin name (e.g., "my-plugin")
```

### `copad.call(method, params?)`

Call any copad socket API method. Returns a Promise.

```javascript
// Get terminal state
const state = await copad.call("terminal.state");
console.log(state.cwd, state.cols, state.rows);

// Read terminal screen
const screen = await copad.call("terminal.read");
console.log(screen.text);

// Execute a command in the terminal
await copad.call("terminal.exec", { command: "ls -la" });

// List all panels
const panels = await copad.call("session.list");

// Open a webview
const { panel_id } = await copad.call("webview.open", { url: "https://example.com" });

// Create a new terminal tab
await copad.call("tab.new");

// List themes
const { themes, current } = await copad.call("theme.list");

// Run a plugin command
const result = await copad.call("plugin.my-plugin.greet", { name: "world" });
```

All [socket API methods](./architecture.md#socket-server-ipc) are available.

### `copad.on(type, callback)` / `copad.off(type, callback)`

Listen for copad events:

```javascript
// Listen for focus changes
copad.on("panel.focused", (data) => {
    console.log("Panel focused:", data.panel_id);
});

// Listen for terminal output
copad.on("terminal.output", (data) => {
    console.log("Output:", data.text);
});

// Wildcard: listen for all events
copad.on("*", (type, data) => {
    console.log(`Event: ${type}`, data);
});

// Remove a listener
const handler = (data) => { ... };
copad.on("panel.focused", handler);
copad.off("panel.focused", handler);
```

All [event types](./architecture.md#event-stream) are available.

## Theme CSS Variables

Plugin panels automatically receive CSS variables matching the active copad theme:

```css
:root {
    --copad-bg: #1e1e2e;
    --copad-fg: #cdd6f4;
    --copad-surface0: #313244;
    --copad-surface1: #45475a;
    --copad-surface2: #585b70;
    --copad-overlay0: #6c7086;
    --copad-text: #cdd6f4;
    --copad-subtext0: #a6adc8;
    --copad-subtext1: #bac2de;
    --copad-accent: #89b4fa;
    --copad-red: #f38ba8;
}
```

Use these in your CSS to match the terminal theme:

```css
.card {
    background: var(--copad-surface0);
    border: 1px solid var(--copad-overlay0);
    color: var(--copad-text);
    border-radius: 8px;
    padding: 16px;
}

button {
    background: var(--copad-accent);
    color: var(--copad-bg);
    border: none;
    border-radius: 4px;
    padding: 8px 16px;
    cursor: pointer;
}

button:hover {
    opacity: 0.9;
}
```

The base `body` style is also set automatically (background color, text color, system font, no margin/padding).

## Plugin Commands

Commands are shell scripts that run when called via `plugin.<name>.<command>`.

### Environment Variables

| Variable | Value |
|----------|-------|
| `COPAD_SOCKET` | Path to copad's Unix socket |
| `COPAD_PLUGIN_DIR` | Absolute path to the plugin directory |

### Input/Output

- **stdin**: JSON params from the caller
- **stdout**: JSON response (or plain text, wrapped as `{"output": "..."}`)
- **stderr**: Logged on failure
- **Exit code**: 0 = success, non-zero = error

### Example Command Script

```bash
#!/bin/bash
# scripts/greet.sh — reads params from stdin, writes JSON to stdout

PARAMS=$(cat)
NAME=$(echo "$PARAMS" | jq -r '.name // "world"')

echo "{\"message\": \"Hello, $NAME!\"}"
```

### Calling Commands

From CLI:
```bash
coctl plugin run my-plugin.greet --params '{"name": "copad"}'
```

From a plugin panel's JS:
```javascript
const result = await copad.call("plugin.my-plugin.greet", { name: "copad" });
console.log(result.message); // "Hello, copad!"
```

### Calling copad from Command Scripts

Commands can call back into copad via the socket:

```bash
#!/bin/bash
# Use coctl with the injected socket path
export COPAD_SOCKET="$COPAD_SOCKET"
coctl terminal exec "echo 'hello from plugin'"
```

## CLI

```bash
# List installed plugins
coctl plugin list

# Open a plugin panel (default panel: "main")
coctl plugin open my-plugin
coctl plugin open my-plugin --panel settings

# Run a plugin command
coctl plugin run my-plugin.greet --params '{"name": "world"}'
```

## GTK Icon Names

Common icons for the `icon` field in panel definitions:

| Icon Name | Use For |
|-----------|---------|
| `utilities-terminal-symbolic` | Terminal-related |
| `applications-system-symbolic` | System/settings |
| `preferences-system-symbolic` | Preferences |
| `folder-symbolic` | File management |
| `web-browser-symbolic` | Web content |
| `edit-find-symbolic` | Search |
| `document-open-symbolic` | Documents |
| `view-list-symbolic` | List views |
| `dialog-information-symbolic` | Info/status |
| `application-x-addon-symbolic` | Generic plugin (default) |

Use `gtk4-icon-browser` to explore all available icons on your system.

## Status Bar Modules

Plugins can contribute modules to the Waybar-style status bar. The bar is a WebView rendering CSS-styled modules, with data provided by shell scripts — similar to Waybar's custom modules.

### Module Manifest

```toml
[[modules]]
name = "clock"
exec = "date '+%H:%M:%S'"    # shell command, stdout → module text
interval = 1                   # re-run every N seconds
position = "right"             # left, center, right
order = 100                    # sort order (lower = first)
class = "clock"                # CSS class for styling
```

### Data Format

Module `exec` stdout supports two formats:

**Plain text** — used as-is:
```
23:45:01
```

**JSON** — with `text` and optional `tooltip`:
```json
{"text": "$12.34 | 62kout", "tooltip": "178 messages today\nModel: opus-4-6"}
```

### CSS Styling

Place a `style.css` in the plugin directory. It's injected into the bar alongside theme CSS variables.

```css
/* ~/.config/copad/plugins/my-plugin/style.css */
.clock {
    font-family: monospace;
    color: var(--copad-subtext0);
}

.claude-usage {
    color: var(--copad-accent);
    font-weight: bold;
}
```

Available CSS variables: `--copad-bg`, `--copad-fg`, `--copad-surface0/1/2`, `--copad-overlay0`, `--copad-text`, `--copad-subtext0/1`, `--copad-accent`, `--copad-red`.

### Environment Variables

Module scripts receive:

| Variable | Value |
|----------|-------|
| `COPAD_SOCKET` | Path to copad's Unix socket |
| `COPAD_PLUGIN_DIR` | Absolute path to the plugin directory |

Scripts can use `coctl` for copad integration (CWD, tab info, etc.).

### Config

```toml
[statusbar]
enabled = true          # Show/hide the status bar (default: true)
position = "bottom"     # "top" or "bottom" (default: "bottom")
height = 28             # Height in pixels (default: 28)
```

### CLI

```bash
coctl statusbar show
coctl statusbar hide
coctl statusbar toggle
```

### Socket API

| Command | Response |
|---------|----------|
| `statusbar.show` | `{visible: true}` |
| `statusbar.hide` | `{visible: false}` |
| `statusbar.toggle` | `{visible: bool}` |

### Architecture

The status bar is a single WebView that renders CSS-styled module containers. Rust periodically runs each module's `exec` command in a thread, then updates the corresponding DOM element via `evaluate_javascript()`. This gives full CSS styling power with lightweight shell-based data collection.

## Tips

- Plugin panels have `allow_file_access_from_file_urls` enabled, so you can load local CSS/JS/images with relative paths in your HTML
- DevTools are enabled — right-click and inspect to debug your plugin panel
- Use `copad.on("*", console.log)` during development to see all events
- Plugin discovery happens at startup — restart copad after adding a new plugin
- Commands run in a thread, so they won't block the UI even if slow
