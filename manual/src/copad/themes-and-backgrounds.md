# Themes & Backgrounds

## Themes

copad ships 10 built-in themes. Set one by name:

```toml
[theme]
name = "catppuccin-mocha"
```

| Theme name | |
| --- | --- |
| `catppuccin-mocha` (default) | `dracula` |
| `catppuccin-latte` | `nord` |
| `catppuccin-frappe` | `tokyo-night` |
| `catppuccin-macchiato` | `gruvbox-dark` |
| `one-dark` | `solarized-dark` |

The theme hot-reloads on save and colors the terminal palette, the tab bar, the search bar, the webview URL bar, and the window background. An unknown name falls back to Catppuccin Mocha.

List themes and see the active one:

```bash
coctl theme list
```

---

## Background images

copad composites a wallpaper behind the terminal (GPU-accelerated), with a color tint and opacity on top. There are two ways to source the image.

### A single static image

```toml
[background]
image = "~/Pictures/wall.png"
tint = 0.85
opacity = 0.95
```

### A rotating source

Point `image` at a **directory** (Linux) and copad picks randomly from it, or use a **list file** (both platforms):

```toml
[background]
image = "~/Pictures/wallpapers"    # Linux: a directory → random rotation
extensions = ["jpg", "jpeg", "png", "webp"]
recursive = true                   # include subdirectories
rotate_interval = 900              # auto-rotate every 15 min (0 = manual only)
tint = 0.85
tint_color = "#1e1e2e"
opacity = 0.95
```

> On macOS a directory `image` isn't supported and falls back to the list file. Build a list with `coctl background cache` (below).

**How the source is resolved:** if `image` is a directory, copad scans it and ignores `list`; otherwise it reads the list file (`list` if set, else the platform cache), and a plain-file `image` is applied as a static wallpaper on top.

### The tint and opacity layers

On Linux the wallpaper is one of several independent alpha layers, stacked `desktop → backdrop → image → tint → text`:

- **`opacity`** (0–1) — how opaque the wallpaper image itself is.
- **`tint`** (0–1) — how strongly `tint_color` overlays the image. `0.0` = no tint, `1.0` = fully covered by the tint color. The default `0.85` keeps text readable over busy images.
- **`tint_color`** — the hex color of that overlay (default `#1e1e2e`, matching Catppuccin Mocha).

> **VTE transparency (Linux).** Background images require `terminal.set_clear_background(false)` internally — this is handled for you, but it's why transparency and wallpapers are a Linux/VTE-specific feature path.

---

## Controlling backgrounds at runtime

`coctl background …` drives the wallpaper without editing config (changes are live):

```bash
coctl background set ~/Pictures/wall.png     # set a specific image
coctl background set-tint 0.35               # change the tint (0.0–1.0)
coctl background next                         # jump to the next random wallpaper
coctl background toggle                        # show/hide the background
coctl background delete-current               # delete the current list-picked wallpaper + rotate
coctl background clear                         # remove the background image
```

### Building a wallpaper list

`coctl background cache` scans a directory and writes the list file that `[background] list` reads:

```bash
coctl background cache --path ~/Pictures/walls --recursive --force
```

- `--path <dir>` — the directory to scan (defaults to `[background] image` when that names a directory).
- `--output <file>` — where to write the list (defaults to `[background] list`, else `~/.cache/terminal-wallpapers.txt`).
- `--recursive` — descend into subdirectories.
- `--force` — overwrite an existing list file.

---

## Window transparency

Separate from background images, `[window]` gives you Ghostty-style window transparency:

```toml
[window]
opacity = 0.9          # translucent window + terminal default background
background = "#1e1e2e" # Linux: solid base color blended with the desktop
blur = true            # macOS: blur the desktop behind the window
```

`background` is Linux-only; `blur` is macOS-only. On Linux, desktop blur is your compositor's job (e.g. Hyprland `decoration:blur`).
