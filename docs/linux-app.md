# Linux App (copad-linux)

## Entry Point (`main.rs`)

CLI flags handled before GTK launch:

- `--init-config` — writes default config to `~/.config/copad/config.toml`
- `--config-path` — prints config file path

## Application (`app.rs`)

- GTK Application ID: `com.marshall.copad`
- Forces dark theme on startup via `set_gtk_application_prefer_dark_theme(true)`
- Loads config with `CopadConfig::load()`, falls back to defaults

## Window (`window.rs`)

- Default size: 1200x800
- CSS: `window { background-color: <theme.background>; }` (theme-driven; falls back to Catppuccin Mocha base)
- Creates a `BackgroundLayer` (window-level bg image + tint) and wraps `[statusbar, notebook, statusbar?]` in a `gtk4::Overlay`. Stack: `bg_picture` (base) → `tint_overlay` (overlay) → layout (overlay). Every panel above renders transparently so the same image shows through every tab.
- Creates a single `TerminalTab` and adds it to the notebook
- Registers D-Bus service and polls for commands every 50ms via `glib::timeout_add_local`

### D-Bus Command Loop

```
D-Bus callback (any thread) → mpsc::channel → glib::timeout_add_local (GTK main thread) → widget updates
```

This pattern is required because GTK widgets are not `Send+Sync` and can only be accessed from the main thread.

## Background (`background.rs`)

`BackgroundLayer` owns the window-level image + tint **and the window's `background-color` CSS**. Lives once per window and is shared via `Rc`. Mounted as the base child of the root `gtk4::Overlay` in `window.rs`.

```rust
pub struct BackgroundLayer {
    pub bg_picture: gtk4::Picture,   // GtkPicture, content-fit: cover, can_target=false
    pub tint_overlay: gtk4::Box,     // CSS rgba, can_target=false
    // … private state cells: image/tint/window opacity, theme bg, window_css provider
}
```

API: `new(config, window_css, theme_bg)`, `set_image(path)`, `clear_image()`, `set_tint(opacity)`, `apply_config(config, theme_bg)`.

`can_target=false` on both layers so clicks pass through to the panels above them. The socket `background.*` commands operate on this single layer (no longer on the active terminal panel), and `apply_config` is invoked once from `watch_config` on hot reload.

The layer also owns the window's `background-color` provider: `[window] opacity` drives its alpha (`rgba(theme_bg, opacity)`), painted as the always-present base **behind** the image. The backdrop, the image (`background.opacity`), and the tint (`background.tint`) are three independent layers — none scales the others, so a strong dark base can sit under a faint image (decision #58, reversing #57's macOS-parity coupling). `refresh_window_backdrop()` runs only on a `[window] opacity` / theme-color change (via `apply_config`), not on image add/remove, since the backdrop no longer depends on image state.

## Terminal (`terminal.rs`)

### TerminalPanel Struct

```rust
pub struct TerminalPanel {
    pub overlay: gtk4::Overlay,      // hosts the search bar above the terminal
    pub terminal: vte4::Terminal,
    pub child_pid: Rc<Cell<i32>>,
    pub search_bar: SearchBar,
}
```

### Overlay Stack (bottom to top)

```
terminal (VTE Terminal)        ← child of overlay
  └─ search_bar (Box, valign=End) ← overlay (hidden by default)
```

The terminal is **always** transparent (`set_clear_background(false)` + `RGBA(0,0,0,0)` bg color) so the window-level `BackgroundLayer` shows through whether or not an image is set. Solid theme background still appears via the window's CSS when no image is loaded.

### Font Scaling

- Keyboard: `Ctrl+=` zoom in, `Ctrl+-` zoom out, `Ctrl+0` reset
- Range: 0.3x to 3.0x, step 0.1
- Uses `terminal.set_font_scale()`

### Color Palette

Theme palette (Catppuccin Mocha by default), 16-color:

- Foreground: `#cdd6f4`
- Background: forced `rgba(0,0,0,0)`; the `BackgroundLayer` (or window CSS fallback) supplies the visible color
- See `parse_color()` function

### Shell Spawn

VTE handles PTY internally via `terminal.spawn_async()`. No custom PTY management needed on Linux.

On child exit, the window closes automatically via `connect_child_exited`.

## Tabs (`tabs.rs`)

### TabManager

Manages `gtk4::Notebook` with `TabContent` entries (split pane trees).

- Tab position configurable via `[tabs] position` in config (`top`, `bottom`, `left`, `right`)
- Tab bar has collapsed (icon-only) and expanded modes, toggled via Ctrl+Shift+B
- Tab position and collapsed state hot-reload on config change

### Keyboard Shortcuts

All built-in shortcuts use `Ctrl+Shift` — Ctrl-only keys pass through to terminal/webview.

| Shortcut                            | Action                              |
| ----------------------------------- | ----------------------------------- |
| `Ctrl+Shift+B`                      | Toggle tab bar (collapsed/expanded) |
| `Ctrl+Shift+F`                      | Toggle search bar                   |
| `Ctrl+Shift+T`                      | New tab                             |
| `Ctrl+Shift+W`                      | Close focused pane/tab              |
| `Ctrl+Shift+C`                      | Copy (terminal)                     |
| `Ctrl+Shift+V`                      | Paste (terminal)                    |
| `Ctrl+Shift+E`                      | Split horizontal                    |
| `Ctrl+Shift+O`                      | Split vertical                      |
| `Ctrl+Shift+N` / `Ctrl+Shift+Right` | Focus next pane                     |
| `Ctrl+Shift+Left`                   | Focus previous pane                 |
| `Ctrl+Shift+P`                      | Command palette (filter-and-dispatch any registered action) |
| `Ctrl+Shift+Tab`                    | Next tab                            |
| `Ctrl+Shift+1-9`                    | Switch to tab N                     |
| `Ctrl+Click` (on URL)               | Open URL in default browser (OSC 8 hyperlinks + http(s) regex) |

### Session Persistence

On window close, the current tab/split layout and each terminal's cwd are saved to `~/.local/state/copad/session.json` (XDG state dir). On next launch, that layout is auto-restored — same tabs, same split structure, each shell respawned in its last directory. Closing the last terminal tab clears the file so the next launch starts fresh. WebView and plugin panels are elided from v1 (terminal-only). Paned split positions reset to 50/50.

### Custom Keybindings

Custom keybindings can be configured in `[keybindings]` section of `config.toml`. They are checked before built-in shortcuts, so they can override defaults. Spawned commands receive `COPAD_SOCKET` environment variable. See [config.md](./config.md#keybindings) for details.

## Search (`search.rs`)

In-terminal text search using VTE's built-in regex search API.

- **Toggle:** `Ctrl+Shift+F` opens/closes the search bar (overlay at bottom of terminal)
- **Search:** Uses `vte4::Regex::for_search()` with PCRE2, applied via `terminal.search_set_regex()`
- **Navigation:** `Enter` = next match, `Shift+Enter` = previous match
- **Close:** `Escape` closes search and returns focus to terminal
- **Case sensitivity:** Toggle button, default is case-insensitive (`PCRE2_CASELESS`)
- **Reopen behavior:** Previous search text is preserved but fully selected, so typing immediately replaces it
- **Wrap around:** Enabled by default

## Split Panes (`split.rs`)

Binary tree of `SplitNode` (Leaf = terminal, Branch = `gtk4::Paned` with two children). Each tab has a `TabContent` with a root `SplitNode` and a stable container `gtk4::Box`.

## Installation

```bash
# Build + install
./copad-linux/install.sh

# Or manually
cargo build --release -p copad-linux
sudo install -Dm755 target/release/copad /usr/local/bin/copad
sudo install -Dm644 copad-linux/com.marshall.copad.desktop \
    /usr/share/applications/com.marshall.copad.desktop

# Set as default terminal (GNOME)
gsettings set org.gnome.desktop.default-applications.terminal exec copad
```
