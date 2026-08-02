# Keybindings

comux keybindings come in two kinds:

- **Prefix bindings** — press the prefix (`Ctrl-b` by default), release, then the command key. Like tmux.
- **Prefix-less (global) bindings** — a single chord, no prefix. Like tmux's `bind -n`.

All of these are remappable in `mux.toml` — see [Customizing bindings](#customizing-bindings) and [Configuration](./configuration.md).

## Chord notation

| Notation | Means |
| --- | --- |
| `C-` | Ctrl |
| `M-` | Alt / Meta (Option on macOS) |
| `S-` | Shift |
| An uppercase letter | Implies Shift — `X` is the same as `S-x` |

---

## Prefix-less (global) bindings

| Action | Default chord(s) |
| --- | --- |
| Enter the prefix | `C-b` |
| Fuzzy switcher popup | `C-f` |
| Jump to tab 1–9 | `M-1` … `M-9` |
| Focus pane left | `C-S-h`, `C-S-Left` |
| Focus pane down | `C-S-j`, `C-S-Down` |
| Focus pane up | `C-S-k`, `C-S-Up` |
| Focus pane right | `C-S-l`, `C-S-Right` |

---

## Prefix bindings (after `Ctrl-b`)

### Panes & splits

| Action | Chord |
| --- | --- |
| Split left/right (side by side) | `%` |
| Split top/bottom (stacked) | `"` |
| Focus next pane | `o` |
| Focus left / down / up / right | `h` / `j` / `k` / `l` (or arrows) |
| Resize left / down / up / right | `H` / `J` / `K` / `L` |
| Close pane | `x` |

### Tabs

| Action | Chord |
| --- | --- |
| New tab | `c` |
| Next / previous tab | `n` / `p` |
| Close tab | `&` |
| Rename tab (empty clears) | `,` |
| Select tab 1–9 | `1` … `9` |

### Sessions

| Action | Chord |
| --- | --- |
| New session (inline name prompt) | `C` |
| New worktree (branch prompt → git worktree + session) | `W` |
| Rename session | `$` |
| Kill session (y/n confirm) | `X` |
| Next / previous session | `)` / `(` |

### Sidebar & miscellaneous

| Action | Chord |
| --- | --- |
| Toggle sidebar | `s` |
| Focus sidebar (keyboard nav) | `e` |
| Notification center | `a` |
| Jump to a blocked (attention) agent | `!` |
| Detach this client | `d` (or `q`) |
| Enter scrollback (copy-mode) | `[` |
| Force full repaint (tmux `refresh-client`) | `r` |

---

## Modal keys (inside popups)

These are fixed keys within their respective modes, not remappable prefix bindings.

### Copy-mode / scrollback (after `Ctrl-b [`)

`k`/`↑` up · `j`/`↓` down · `PageUp`/`Ctrl-u` up half-page · `PageDown`/`Ctrl-d` down half-page · `g` top · `G`/`q`/`Esc` back to live and exit.

### Fuzzy switcher (`Ctrl-f`)

`↑`/`Ctrl-p`, `↓`/`Ctrl-n` move · `←` Sessions tab · `→` Agents tab · `Enter` select/jump · `Ctrl-r`/`F2` rename selected session · `Esc`/`Ctrl-f` close · `Backspace` delete filter char · any printable char extends the filter.

### Sidebar keyboard-focus (`Ctrl-b e`)

`j`/`↓`, `k`/`↑` move · `h`/`←` → Sessions group · `l`/`→` → Agents group · `Enter` select · `Esc`/`q` exit.

### Notification center (`Ctrl-b a`)

`j`/`↓`, `k`/`↑` move · `Enter` jump · `d` dismiss one · `D` clear all · `Esc`/`q` close.

### Kill-session confirm

`y`/`Y` confirms; anything else cancels.

---

## Customizing bindings

Two tables in `mux.toml` remap bindings:

- `[keys]` — the **prefix** table (chords pressed after the prefix)
- `[global]` — **prefix-less** bindings

Each entry is `action = chord` or `action = [chord, chord, …]`. **Overriding an action replaces its entire default chord set** — so if you want to keep a default and add another, list them all.

```toml
prefix = "C-a"          # use Ctrl-a as the prefix instead of Ctrl-b

[keys]
split-down  = '"'       # keep the default
split-right = "|"       # remap the side-by-side split to |
new-tab     = ["c", "t"]  # both c and t create a tab

[global]
popup = "C-Space"       # open the fuzzy switcher with Ctrl-Space instead of Ctrl-f
```

### Recognized action names

`split-right`, `split-down`, `new-tab`, `next-tab`, `prev-tab`, `close-tab`, `rename-tab`, `new-session`, `new-worktree`, `rename-session`, `next-session`, `prev-session`, `kill-session`, `notification-center`, `jump-attention`, `detach`, `close-pane`, `toggle-sidebar`, `scrollback`, `focus-next`, `focus-left`, `focus-down`, `focus-up`, `focus-right`, `resize-left`, `resize-down`, `resize-up`, `resize-right`, `popup` (the fuzzy switcher), `redraw`, `focus-sidebar`, `prefix`, and `tab-1` … `tab-9`.

An unknown action name warns once on startup and is ignored. A `[global]` binding equal to the prefix chord is dropped (with a warning) so prefix entry always wins. Keybinding changes are **live-reloadable** — edit `mux.toml` and run `comux reload`.
