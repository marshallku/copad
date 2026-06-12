# Configuration

Path: `~/.config/copad/config.toml`

## Generate Default Config

```bash
copad --init-config
```

## Print Config Path

```bash
copad --config-path
```

## Full Example

```toml
[terminal]
shell = "/bin/zsh"
font_family = "JetBrainsMono Nerd Font Mono"
font_size = 14

[background]
# image = "/path/to/wallpaper.jpg"  # Static image (rotation replaces it at the first tick)
# rotate_interval = 300  # Seconds between random wallpapers; 0 (default) = no auto-rotation
tint = 0.85         # Tint overlay opacity on the IMAGE (0.0 = no tint, 1.0 = fully opaque)
opacity = 0.95      # Background-image opacity (only takes effect when an image is set)

[window]
# opacity = 0.85   # 0.0 = fully transparent, 1.0 = fully opaque (default)
# blur = true      # macOS only: blur the desktop behind the window (Ghostty-style)

[tabs]
position = "left"   # top, bottom, left, right
# collapsed = true  # start with tab bar collapsed (icon-only)
# width = 200       # tab bar width in pixels (vertical tabs)

[theme]
name = "catppuccin-mocha"
```

## Sections

### [terminal]

| Key           | Default                        | Description         |
| ------------- | ------------------------------ | ------------------- |
| `shell`       | `$SHELL` or `/bin/sh`          | Shell to spawn      |
| `font_family` | `JetBrainsMono Nerd Font Mono` | Font family         |
| `font_size`   | `14`                           | Font size in points |

### [background]

`tint` + `opacity` only take effect when a background image is set. For window-level transparency (no image), use `[window]` below.

| Key               | Default      | Description                                                |
| ----------------- | ------------ | ---------------------------------------------------------- |
| `image`           | — (optional) | Static image file path, applied at startup                 |
| `rotate_interval` | `0`          | Seconds between random wallpapers from the platform list file; `0` disables auto-rotation |
| `tint`            | `0.9`        | Tint overlay opacity on the image (0.0=transparent, 1.0=opaque) |
| `opacity`         | `0.95`       | Background-image opacity                                   |

**Rotation.** With `rotate_interval > 0` copad picks a random image from the platform list
file at startup and every interval — no external daemon needed. The shared mode flag (flipped
by `coctl background toggle`) pauses rotation across all instances; on Linux every instance
also watches the flag file (inotify), so a toggle against one instance clears/re-applies the
wallpaper on all of them immediately — the watcher is armed even at `rotate_interval = 0`,
matching the retired script's broadcast behavior. A static `image` is applied
first and then replaced by the first rotation pick, so with rotation enabled it acts as a
pre-list fallback. Manual `coctl background set`/`next` restarts the countdown. `coctl
background delete-current` removes the displayed wallpaper from disk and the list, then
rotates — it refuses when the displayed image was set manually (or via `image`) rather than
picked from the list. Each GUI instance rotates independently ("current" is per instance;
keybinding-spawned `coctl` inherits that instance's socket).

Platform list-file + mode-flag locations:

- **Linux**: list `~/.cache/terminal-wallpapers.txt`, mode flag `~/.cache/copad-bg-mode`.
- **macOS**: list `~/Library/Caches/copad/wallpapers.txt` (XDG `~/.cache/terminal-wallpapers.txt`
  as a cross-platform fallback), mode flag `~/Library/Caches/copad/bg-mode`. Rotation,
  `next`/`toggle`, and `delete-current` all work natively (the in-process timer is a
  `DispatchSourceTimer`; `delete-current` drops the entry from both the native and fallback lists).

### [window]

Window-level transparency for the terminal itself (Ghostty model). Distinct from `[background]`, which only affects an optional background-image layer.

| Key          | Default          | Description                                                                          |
| ------------ | ---------------- | ------------------------------------------------------------------------------------ |
| `opacity`    | `1.0`            | Window + terminal default-bg cell opacity (0.0 = fully transparent, 1.0 = opaque)    |
| `background` | theme background | Linux only: solid `#rrggbb` base color blended with the desktop at `opacity`. A dark value keeps text readable on a bright wallpaper. |
| `blur`       | `false`          | macOS only: blur the desktop behind the window (NSVisualEffectView). On Linux, blur is the compositor's job (e.g. Hyprland `decoration:blur`), so this key is a no-op. |

On macOS, `opacity < 1.0` sets `NSWindow.isOpaque = false`; default-bg cells render with `theme.background.alpha = opacity` so the desktop / blurred surface behind shows through. ANSI-colored cells, reverse-video cells, and text glyphs stay fully opaque. Tab bar and status bar pick up the same alpha so chrome stays cohesive.

On Linux, `opacity` drives the GTK4 window's `background-color` alpha (`rgba(background, opacity)`, where `background` defaults to `theme.background`). VTE already paints with a transparent default background, so text glyphs stay opaque while the gaps reveal the desktop. `blur` is a no-op: on Wayland, blur belongs to the compositor — enable `decoration:blur` in Hyprland and it blurs behind the translucent window automatically. Tab pills and status-bar borders keep their theme color by design (matches macOS, where pills stay opaque).

`opacity` is the **fraction of `background` that is maintained** over the desktop: every pixel is `background × opacity + desktop × (1 − opacity)`. So a dark `background` at e.g. `opacity = 0.85` keeps a stable dark base under the text regardless of how bright the wallpaper is — the desktop can only bleed through the remaining `1 − opacity`. Lower the opacity for more see-through; raise it (or darken `background`) for more readability.

**Layers are independent (Linux).** The window backdrop (`[window] opacity` / `background`), the background image (`[background] opacity`), and the tint (`[background] tint`) each carry their own alpha and stack back-to-front: `desktop → backdrop → image → tint → text`. The backdrop is always painted *behind* the image, so you can hold a strong dark base (`[window] opacity = 0.9`) under a faint image (`[background] opacity = 0.3`) — raising one does not affect the other. An opaque image still covers everything above the backdrop; lower `[background] opacity` to let the backdrop (and some desktop) show through.

> If you previously dimmed copad with a Hyprland `windowrule = opacity …, class:copad`, remove it once you set `[window] opacity` — otherwise the compositor alpha stacks on top of the app alpha and dims the text too (the app-level knob keeps glyphs opaque; the windowrule does not).

### [tabs]

| Key         | Default | Description                                        |
| ----------- | ------- | -------------------------------------------------- |
| `position`  | `top`   | Tab bar position: `top`, `bottom`, `left`, `right` |
| `collapsed` | `true`  | Start with tab bar in collapsed (icon-only) mode   |
| `width`     | `200`   | Tab bar width in pixels (vertical tabs only)       |

### Socket path (not a config key)

There is no `[socket]` config section — the GUI socket path is derived per instance
(`$XDG_RUNTIME_DIR/copad/gui-{PID}.sock` on Linux; `/tmp/copad-{PID}.sock` on macOS, whose
hardened relocation is still pending) and injected into child shells as `COPAD_SOCKET`;
the daemon uses its well-known path from `copad_core::paths`. Override the target per call
with `coctl --socket <path>` or the `COPAD_SOCKET` env var.

### [theme]

| Key    | Default            | Description |
| ------ | ------------------ | ----------- |
| `name` | `catppuccin-mocha` | Theme name  |

**Available themes**: `catppuccin-mocha`, `catppuccin-latte`, `catppuccin-frappe`, `catppuccin-macchiato`, `dracula`, `nord`, `tokyo-night`, `gruvbox-dark`, `one-dark`, `solarized-dark`

Theme changes hot-reload on config save. The theme applies to the terminal palette, tab bar, search bar, webview URL bar, and window background.

### [keybindings]

Map key combinations to shell commands. Commands prefixed with `spawn:` run in the background. Custom keybindings take priority over built-in shortcuts.

```toml
[keybindings]
"ctrl+shift+g" = "spawn:~/my-script.sh --next"
"ctrl+shift+m" = "spawn:~/my-script.sh --toggle"
```

**Key format:** `modifier+modifier+key` — modifiers: `ctrl`, `shift`, `alt`. Key names follow GDK naming (e.g. `a`, `b`, `bracketright`, `f1`).

**Environment:** Spawned commands inherit `COPAD_SOCKET` so scripts can communicate back to the running copad instance via socket.

**Note:** Custom bindings override built-in shortcuts. For example, binding `ctrl+shift+b` replaces the default tab bar toggle.

### [[projects]] (Phase 22.2)

Project entries — one `[[projects]]` block per project. Drives `coctl project list` / `project.resolve` and `workflow.run` workspace resolution. (The `copad-plugin-projects` GTK panel that originally rendered these was retired in Phase 24.5 — see [decision #50](./decisions.md#50-projects-panel-retired--web-bridge-is-the-single-orchestration-cockpit); the orchestration cockpit moves to `web-bridge`.)

```toml
[[projects]]
name = "copad"
path = "/home/marshall/dev/copad"
git_remote = "marshallku/copad"     # optional — inferred at startup via `git remote get-url origin` if absent
aliases = ["copad-app"]              # optional — alternate names for `project.resolve --name`

[[projects]]
name = "monorepo"
path = "/home/marshall/dev/mono"
subpath = "apps/web"                 # optional — workflow tabs open at <path>/<subpath>
description = "Frontend monorepo"    # optional — human-readable label
```

| Key          | Type           | Required | Description                                                                    |
| ------------ | -------------- | -------- | ------------------------------------------------------------------------------ |
| `name`       | string         | yes      | Canonical project id (must be unique)                                          |
| `path`       | string (path)  | yes      | Filesystem root — workflow tabs open here unless `subpath` is set              |
| `subpath`    | string (path)  | no       | Working subdir inside `path` (monorepo packages)                               |
| `description`| string         | no       | Free-text human-readable label                                                 |
| `aliases`    | array<string>  | no       | Alternate names accepted by `project.resolve --name`                           |
| `git_remote` | string         | no       | Canonical `owner/repo` — inferred via `git remote get-url origin` if omitted    |

### Workflow specs — `~/.config/copad/workflows/*.yaml` (Phase 22.2)

Each YAML file in `~/.config/copad/workflows/` defines one workflow that `coctl workflow list` exposes and `workflow.run` can dispatch (the retired projects panel originally rendered these; the cockpit moves to `web-bridge`). Schema:

```yaml
id: ship
name: /ship
description: Run /ship — tests, codex review gate, commit, push, PR.
require_project: true            # if true, the caller (coctl / workflow.run) must resolve a project before run
timeout_secs: 1200               # optional — emits workflow.timed_out (no kill in v1)
fresh_session: true              # mirrors life-assistant's ship contract
form_fields:                     # optional — field schema for callers that render a form (e.g. the web-bridge cockpit)
  - name: branch
    label: Git branch
    type: text                   # text | textarea | select
    required: true
    placeholder: "feat/x"
    max_length: 200              # optional length cap
    pattern: "^[a-z][a-z0-9/_-]*$"  # optional regex validator
prompt: |
  /ship for branch {branch} in project {project}
```

Available placeholders in `prompt`: `{field_name}` (each form_fields entry), `{project}` (project name), `{project.path}`, `{project.subpath}`, `{workspace_path}` (resolved final path passed to `claude.start`). Unknown placeholders error.

`default_team` and `default_model` are accepted and stored but ignored at v1 dispatch — Phase 22.7's pipeline router + Brain dispatcher generalization activates them.

`install-dev.sh` seeds 7 starter specs (`ship` / `cross-review` / `debug` / `verify` / `handoff` / `mentor` / `catchup`) on first install via skip-if-exists copy from `examples/workflows/`. User edits are preserved across re-installs.

### [security] (macOS only, for now)

Trust-boundary policies for behaviors driven by PTY output.

```toml
[security]
osc52 = "deny"   # or "allow"
```

| Key     | Default | Values            | Effect                                                                                                                                                  |
| ------- | ------- | ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `osc52` | `deny`  | `deny` / `allow`  | Whether to honor OSC 52 clipboard writes from the PTY. `deny` drops the payload and logs one line to stderr; `allow` writes to `NSPasteboard.general`. |

**Why `deny` by default:** SwiftTerm's `LocalProcessTerminalView.clipboardCopy` writes to the macOS pasteboard unconditionally and is `public` (not `open`), so we cannot override it. copad installs a delegate proxy that consults this policy before any pasteboard access. VTE on Linux already defaults to deny; this aligns the macOS default with that.

Hot-reloads (no restart needed for live panes).

## Notes

- All fields have defaults; config file is optional
- Missing sections are filled with defaults via `#[serde(default)]`
- Config hot-reloads automatically via file watcher (font, background, tint, tab position, keybindings)
