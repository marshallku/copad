# Configuration (`config.toml`)

copad reads `~/.config/copad/config.toml` on **both** Linux and macOS. (Resolution: `$XDG_CONFIG_HOME/copad/config.toml` if set; otherwise `~/.config/copad/config.toml` — on macOS this deliberately overrides `~/Library/Application Support` so the Swift app, the daemon, and `coctl` all read the same file.)

Every section and key is optional — missing ones fall back to defaults. The file **hot-reloads** on save.

```bash
copad --init-config     # generate a starter file with all defaults
copad --config-path     # print the resolved path
```

---

## `[terminal]`

| Key | Default | Notes |
| --- | --- | --- |
| `shell` | `$SHELL`, else `/bin/sh` | Path to the shell to launch |
| `font_family` | `"JetBrainsMono Nerd Font Mono"` | Any installed font family |
| `font_size` | `14` | Points |
| `close_on_exit` | `true` | `false` keeps the dead-PTY viewport visible; close the pane manually |

```toml
[terminal]
shell = "/usr/bin/fish"
font_family = "JetBrainsMono Nerd Font Mono"
font_size = 14
```

---

## `[theme]`

| Key | Default | Allowed |
| --- | --- | --- |
| `name` | `"catppuccin-mocha"` | one of the 10 built-ins below |

The 10 built-in themes: `catppuccin-mocha`, `catppuccin-latte`, `catppuccin-frappe`, `catppuccin-macchiato`, `dracula`, `nord`, `tokyo-night`, `gruvbox-dark`, `one-dark`, `solarized-dark`. An unknown name falls back to Catppuccin Mocha. Themes hot-reload and apply to the terminal palette, tab bar, search bar, webview URL bar, and window background. See [Themes & Backgrounds](./themes-and-backgrounds.md).

```toml
[theme]
name = "tokyo-night"
```

---

## `[background]`

A wallpaper behind the terminal. `tint` and `opacity` only take effect when an image is set. Full walkthrough in [Themes & Backgrounds](./themes-and-backgrounds.md).

| Key | Default | Notes |
| --- | --- | --- |
| `image` (alias `path`) | *(unset)* | A wallpaper **file**, or a **directory** to rotate through (directory source is **Linux-only**). Tilde-expanded. |
| `list` | *(platform cache)* | A list file, one image path per line. Used when `image` is unset or names a plain file. |
| `extensions` | `["jpg", "jpeg", "png", "webp"]` | Accepted extensions when `image` is a directory. Case-insensitive; empty list = accept everything. |
| `recursive` | `false` | Descend into subdirectories of a directory `image` (max depth 16) |
| `tint` | `0.85` | `0.0` (no tint) – `1.0` (fully opaque) overlay |
| `tint_color` | `"#1e1e2e"` | Hex color of the tint overlay |
| `opacity` | `0.95` | Background-image opacity (only when an image is set) |
| `rotate_interval` | `0` | Seconds between random wallpapers; `0` = no auto-rotation |

```toml
[background]
image = "~/Pictures/wallpapers"    # a directory → random rotation (Linux)
extensions = ["jpg", "png"]
tint = 0.85
tint_color = "#1e1e2e"
opacity = 0.95
rotate_interval = 900              # rotate every 15 minutes
```

---

## `[window]`

Window-level transparency (the Ghostty model), distinct from `[background]`.

| Key | Default | Notes |
| --- | --- | --- |
| `opacity` | `1.0` | `0.0` (transparent) – `1.0` (opaque) — drives window + terminal default-bg alpha |
| `background` | *(theme background)* | **Linux only**: solid `#rrggbb` base color blended with the desktop at `opacity` |
| `blur` | `false` | **macOS only**: blur the desktop behind the window. No-op on Linux (the compositor's job) |

```toml
[window]
opacity = 0.92
blur = true            # macOS
```

---

## `[tabs]`

| Key | Default | Notes |
| --- | --- | --- |
| `position` | `"top"` | `top` / `bottom` / `left` / `right` |
| `width` | `120` | Tab-bar width in pixels — vertical tabs (`left`/`right`) only |
| `collapsed` | `true` | Start collapsed (icon-only); toggle at runtime with `Ctrl+Shift+B` |

```toml
[tabs]
position = "left"
width = 160
collapsed = false
```

---

## `[statusbar]`

| Key | Default | Notes |
| --- | --- | --- |
| `enabled` | `true` | Show the status bar |
| `position` | `"bottom"` | `top` / `bottom` |
| `height` | `28` | Pixels |

---

## `[keybindings]`

Map a chord to an action. See [Keybindings & Shortcuts → Customizing](./keybindings.md#customizing-keybindings) for the format and examples. In short: `"modifier+modifier+key" = "spawn:<cmd>"` (run a shell command) or `"action:<method> [k=v …]"` (dispatch an in-process action). Custom bindings are checked first, so they override the built-ins.

```toml
[keybindings]
"ctrl+shift+g" = "spawn:coctl plugin open git"
"ctrl+shift+1" = "spawn:coctl tab switch 0"
```

---

## `[[triggers]]`

Declarative event → action automation (an array of tables). Each has a `name`, `action`, `params`, and a `[triggers.when]` table with an `event_kind` glob and optional payload match / condition / cron. This is the surface of the workflow runtime; see the repo's `docs/workflow-runtime.md` for the full grammar.

---

## `[[projects]]`

Register projects that `coctl project …` and workflows resolve against. One block each:

| Key | Required | Notes |
| --- | --- | --- |
| `name` | yes | Unique canonical name |
| `path` | yes | Filesystem path |
| `subpath` | no | |
| `description` | no | |
| `aliases` | no | Array of alternative names |
| `git_remote` | no | `owner/repo` (inferred from `git remote get-url origin` if omitted) |

```toml
[[projects]]
name = "copad"
path = "~/dev/copad"
aliases = ["cp"]
git_remote = "marshallku/copad"
```

---

## macOS-only sections

These are parsed by the macOS Swift app and ignored on Linux (VTE already denies OSC 52 by default).

### `[security]`

| Key | Default | Effect |
| --- | --- | --- |
| `osc52` | `"deny"` | `allow` honors OSC 52 clipboard writes from the PTY; `deny` drops and logs them. Hot-reloads. |

### `[renderer]`

| Key | Default | Effect |
| --- | --- | --- |
| `transparent_default_bg` | `false` (auto-`true` when a background image is set) | Let the wallpaper show through cells with no explicit ANSI background |
| `gpu` | `true` | The Metal render path. `gpu = false` uses the CoreText painter. Read at pane creation; auto-falls back to CoreText if no Metal device is available. |

---

## File & cache locations

| What | Linux | macOS |
| --- | --- | --- |
| Config | `~/.config/copad/config.toml` | `~/.config/copad/config.toml` |
| Plugins | `~/.config/copad/plugins/<name>/` | `~/Library/Application Support/copad/plugins/<name>/` |
| Workflows | `~/.config/copad/workflows/*.yaml` | same |
| GUI socket (per instance) | `$XDG_RUNTIME_DIR/copad/gui-{PID}.sock` | `/tmp/copad-{PID}.sock` |
| Daemon socket | well-known path (systemd `--user`) | `~/Library/Caches/copad/socket` |
| Wallpaper list | `~/.cache/terminal-wallpapers.txt` | `~/Library/Caches/copad/wallpapers.txt` |
| Session layout | `~/.local/state/copad/session.json` | *(not currently)* |

The GUI socket path is injected into child processes as `COPAD_SOCKET` — which is how `coctl` finds the running instance automatically.
