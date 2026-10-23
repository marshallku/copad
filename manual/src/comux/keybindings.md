# Keybindings

comux keybindings come in two kinds:

- **Prefix bindings** — press the prefix (`Ctrl-b` by default), release, then the command key. Like tmux. While the prefix is armed, a red **`^b`** flag appears in the [status bar](./status-bar.md#the-prefix-flag) so you can tell it registered.
- **Prefix-less (global) bindings** — a single chord, no prefix. Like tmux's `bind -n`.

All of these are remappable in `mux.toml` — see [Customizing bindings](#customizing-bindings) and [Configuration](./configuration.md). `Ctrl-b ?` lists every binding of the keymap you are actually running.

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

A program that needs one of these keys (readline's `Ctrl-f`, vim's page-down) can have them back: `Ctrl-b Ctrl-g` toggles **lock mode** (a red `LOCKED` chip), in which the prefix-less bindings stand down. The prefix itself keeps working — that is how you unlock.

---

## Prefix bindings (after `Ctrl-b`)

### Panes & splits

| Action | Chord |
| --- | --- |
| Split left/right (side by side) | `%` |
| Split top/bottom (stacked) | `"` |
| Focus next pane | `o` |
| Focus left / down / up / right | `h` / `j` / `k` / `l` (or arrows) |
| Back to the previously focused pane (tmux `last-pane`) | `` ` `` |
| Number the panes; a digit jumps there (tmux `display-panes`) | `q` |
| Resize left / down / up / right — **repeatable**: `Ctrl-b H H H` | `H` / `J` / `K` / `L` |
| Zoom the pane to the whole tab, and back | `z` |
| Close pane (asks `y/n`; a tab's only pane closes the tab) | `x` |
| Type into every pane of the tab (synchronize; red `SYNC` chip) | `S` |
| Send the prefix key itself to the program (nested tmux, readline) | `C-b` (the prefix, twice) |

### Arranging panes

See [Layouts & Arranging Panes](./layouts.md).

| Action | Chord |
| --- | --- |
| Swap with the previous / next pane | `{` / `}` |
| Rotate every pane one slot (and back) | `C-o` / `M-o` |
| Next preset layout | `Space` |
| even-horizontal / even-vertical / main-horizontal / main-vertical / tiled | `M-1` / `M-2` / `M-3` / `M-4` / `M-5` |
| Keep the shape, even out every split | `E` |
| Move the pane into a new tab of its own (tmux `break-pane`) | `B` |

### Tabs

| Action | Chord |
| --- | --- |
| New tab | `c` |
| Next / previous tab | `n` / `p` |
| Move the tab left / right along the bar | `S-Left` / `S-Right` |
| Close tab (asks `y/n`) | `&` |
| Rename tab (empty clears) | `,` |
| Select tab 1–9 | `1` … `9` |

### Sessions & navigation

| Action | Chord |
| --- | --- |
| New session (directory browser, then a name) | `C` |
| New worktree (branch prompt → git worktree + session) | `W` |
| Rename session | `$` |
| Kill session (y/n confirm) | `X` |
| Next / previous session | `)` / `(` |
| Toggle to the previous session/tab (Alt-Tab) | `;`, `Tab` |
| Back / forward along the navigation history | `<` / `>` |
| Pin the pane to slot 1–9 / go to a pinned slot | `m` then a digit / `'` then a digit |

### Sidebar & miscellaneous

| Action | Chord |
| --- | --- |
| Toggle sidebar | `s` |
| Focus sidebar (keyboard nav) | `e` |
| Resume a past Claude/Codex conversation | `R` |
| Notification center | `a` |
| Jump to a blocked (attention) agent | `!` |
| Jump to the newest unread notification | `u` |
| Detach this client | `d` |
| Enter copy mode (scrollback) | `[` |
| Force full repaint (tmux `refresh-client`) | `r` |
| Every binding of the live keymap | `?` |
| Lock mode on / off | `C-g` |

---

## Modal keys (inside popups)

These are fixed keys within their respective modes, not remappable prefix bindings.

### Copy mode (after `Ctrl-b [`)

tmux's `copy-mode-vi` table, with vim's `v` for a character selection:

| Keys | Does |
| --- | --- |
| `h` `j` `k` `l` / arrows | move |
| `w` `b` `e` | next word / previous word / end of word (stopping at punctuation) |
| `W` `B` `E` | the same by whitespace-separated WORDs |
| `0` `^` `$` | line start / first non-blank / line end |
| `H` `M` `L` | top / middle / bottom of the screen |
| `{` `}` | previous / next paragraph |
| `%` | matching bracket |
| `f` `F` `t` `T` + a character, then `;` `,` | jump to (or just before) it on the line, again / reversed |
| `g` / `G` | top of history / leave |
| `Ctrl-u` `Ctrl-d`, `Ctrl-b` `Ctrl-f`, `PgUp` `PgDn` | half page / full page |
| `Ctrl-y` `Ctrl-e`, `K` `J` | scroll one line |
| `/` `?` then `n` `N` | search down / up (case-insensitive); every match on screen lights up as you type |
| `*` `#` | search the word under the cursor |
| `v` / `Space` | start a character selection |
| `V` | line selection |
| `Ctrl-v` | rectangle selection (pressing another of `v` `V` `Ctrl-v` reshapes the selection in place) |
| `o` | jump to the other end of the selection |
| `y` / `Enter` | copy and leave |
| `Esc` | clear the selection, or leave |
| `q` / `Ctrl-c` | leave |

### Mouse

Drag in a pane to select and copy. **Double-click** copies the word under the pointer, **triple-click** the whole (wrapped) line — in a pane whose program does not take the mouse itself.

### Pane numbers (`Ctrl-b q`)

A digit within two seconds focuses that pane; any other key just closes the numbers.

### Fuzzy switcher (`Ctrl-f`)

`↑`/`Ctrl-p`, `↓`/`Ctrl-n` move · `←` Sessions tab · `→` Agents tab · `Enter` select/jump · `Ctrl-r`/`F2` rename selected session · `Esc`/`Ctrl-f` close · `Backspace` delete filter char · any printable char extends the filter.

### Sidebar keyboard-focus (`Ctrl-b e`)

`j`/`↓`, `k`/`↑` move · `h`/`←` → Sessions group · `l`/`→` → Agents group · `Enter` select · `Esc`/`q` exit.

### Resume picker (`Ctrl-b R`)

`↑`/`Ctrl-p`, `↓`/`Ctrl-n` move · `Enter` resume (or jump, if that conversation is already open in a pane) · `Ctrl-a` show/hide non-interactive runs · `Ctrl-r` rescan now · `Esc` close · `Backspace` delete filter char · any printable char extends the filter. See [Persistence & Agent Resume](./persistence.md#finding-a-conversation-you-closed-ctrl-b-r).

### Notification center (`Ctrl-b a`)

`j`/`↓`, `k`/`↑` move · `Enter` jump · `d` dismiss one · `D` clear all · `Esc`/`q` close.

### Confirm prompts (kill session, close pane / tab)

`y`/`Y` confirms; anything else cancels. Set `confirm_close = false` to close panes and tabs without asking.

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
detach      = ["d", "q"]  # q detaches too (it numbers the panes by default)

[global]
popup = "C-Space"       # open the fuzzy switcher with Ctrl-Space instead of Ctrl-f
```

`repeat_time` (milliseconds, default 500, `0` = off) is how long the resize keys stay live after one press, so `Ctrl-b H H H` nudges three times.

### Recognized action names

`split-right`, `split-down`, `new-tab`, `next-tab`, `prev-tab`, `close-tab`, `rename-tab`, `move-tab-left`, `move-tab-right`, `new-session`, `new-worktree`, `rename-session`, `next-session`, `prev-session`, `kill-session`, `notification-center`, `jump-attention`, `jump-unread`, `detach`, `close-pane`, `toggle-sidebar`, `scrollback`, `focus-next`, `focus-left`, `focus-down`, `focus-up`, `focus-right`, `last-pane`, `display-panes`, `resize-left`, `resize-down`, `resize-up`, `resize-right`, `zoom-pane`, `swap-pane-up`, `swap-pane-down`, `rotate-panes`, `rotate-panes-back`, `next-layout`, `layout-even-horizontal`, `layout-even-vertical`, `layout-main-horizontal`, `layout-main-vertical`, `layout-tiled`, `spread-panes`, `break-pane`, `synchronize-panes`, `toggle-lock`, `popup` (the fuzzy switcher), `redraw`, `focus-sidebar`, `resume-picker`, `nav-back`, `nav-forward`, `nav-last`, `mark`, `goto-mark`, `key-help`, `prefix`, `send-prefix`, and `tab-1` … `tab-9`.

An unknown action name warns once on startup and is ignored. A `[global]` binding equal to the prefix chord is dropped (with a warning) so prefix entry always wins. Keybinding changes are **live-reloadable** — edit `mux.toml` and run `comux reload`.
