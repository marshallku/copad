# Sessions, Tabs & Panes

comux organizes your work in three levels, mirroring tmux:

- **Session** — a whole isolated workspace of tabs and panes. Switching sessions preserves each one's state. (tmux calls this a "session" too.)
- **Tab** — a window within a session, holding one split layout. (tmux "window".)
- **Pane** — a single shell inside a tab's split layout.

Every action below has both a **keybinding** (pressed after the `Ctrl-b` prefix, unless noted) and a **CLI form**. The defaults shown here can be remapped — see [Keybindings](./keybindings.md) and [Configuration](./configuration.md).

---

## Panes

Split the focused pane and move around inside a tab:

| Action | Key | CLI |
| --- | --- | --- |
| Split left/right (side by side) | `Ctrl-b %` | `comux split -h` |
| Split top/bottom (stacked) | `Ctrl-b "` | `comux split -v` |
| Focus next pane | `Ctrl-b o` | `comux focus <index>` |
| Focus left / down / up / right | `Ctrl-b h`/`j`/`k`/`l` (or arrows) | — |
| Focus a pane by index | — | `comux focus 2` |
| Resize left / down / up / right | `Ctrl-b H`/`J`/`K`/`L` | `comux resize <index> <dir>` |
| Close the focused pane | `Ctrl-b x` | `comux close <index>` |

`comux focus` / `close` / `select-tab` / `close-tab` / `select-session` / `kill-session` with **no index** open a fuzzy picker over the live listing instead of erroring — see [Leave the argument out and pick](./index.md#leave-the-argument-out-and-pick).

Directional focus also has **prefix-less** bindings so you don't need to reach for `Ctrl-b` every time: `Ctrl+Shift+h/j/k/l` or `Ctrl+Shift+Arrow`.

Send text straight into a pane from a script:

```bash
comux list                       # find the pane index
comux send 1 "git status"        # inject text (as if typed) into pane 1
```

---

## Tabs

| Action | Key | CLI |
| --- | --- | --- |
| New tab | `Ctrl-b c` | `comux new-tab` |
| Next / previous tab | `Ctrl-b n` / `Ctrl-b p` | — |
| Jump to tab 1–9 | `Ctrl-b 1`…`9`, or prefix-less `Alt+1`…`9` | `comux select-tab <index>` |
| Close tab | `Ctrl-b &` | `comux close-tab [index]` |
| Rename tab | `Ctrl-b ,` | `comux rename-tab [index] <name>` |
| List tabs | — | `comux list-tabs` |

> **`Alt`/`Option`+number** needs your terminal set to send Option/Alt as Meta.

`comux close-tab` closes a tab and reaps its shells, like `Ctrl-b &`. A session always keeps at
least one tab, so closing the last one is refused — kill the session instead.

### Naming tabs

A tab shows a process-derived label by default. Give it a custom name and that name wins in every label style, titles the tab's agent rows in the sidebar, and survives restarts:

```bash
comux rename-tab "build logs"    # rename the active tab
comux rename-tab 2 "tests"       # rename tab 2
comux rename-tab ""              # clear back to the process/index label
```

In the TUI, `Ctrl-b ,` opens an inline rename prompt.

---

## Sessions

A session is a self-contained workspace. Create several — say one per project — and switch between them without losing any state.

| Action | Key | CLI |
| --- | --- | --- |
| New session (pick a directory, then a name) | `Ctrl-b C` | `comux new-session [name] [--cwd DIR] [-d]` |
| Rename session | `Ctrl-b $` | `comux rename-session [index] <name>` |
| Kill session (with y/n confirm) | `Ctrl-b X` | `comux kill-session [index]` |
| Next / previous session | `Ctrl-b )` / `Ctrl-b (` | — |
| Switch to a session | via sidebar / `Ctrl-f` | `comux select-session <index>` |
| List sessions | — | `comux list-sessions` |

`comux kill-session` is the CLI form of `Ctrl-b X` — but where the key opens a `y`/`n` confirm, the
CLI kills straight away (tmux `kill-session` behaves the same). The mux always keeps at least one
session, so killing the last one is refused.

### Picking the directory (`Ctrl-b C`)

`Ctrl-b C` opens a directory browser at `~`. The first row, `.  create session here`, picks the
directory you are in; `..` goes up; every other row is a subdirectory.

| Key | Does |
| --- | --- |
| type | narrow the list (prefix matches first; start with `.` to see hidden directories) |
| `/…` or `~/…` | type a path; Enter jumps there |
| `↑` `↓` / `Ctrl-n` `Ctrl-p` | move |
| `Enter` | enter the directory (or, on `.`, go on to the name) |
| `→` / `Tab` | enter the directory |
| `←` / `Backspace` on an empty filter | go up |
| `Esc` | cancel |

The name prompt that follows is pre-filled with the directory's name — Enter accepts it, and Esc
goes back to the browser.

### Where shells start

A session remembers the directory it was **created in** (tmux's session path). New tabs in it
(`Ctrl-b c`) always open there — a `cd` in one tab does not change where the next tab lands.
Splits start in the directory of the pane being split, like tmux's `-c '#{pane_current_path}'`.

From the CLI, a new session starts where you run the command:

```bash
cd ~/dev/myproject
comux new-session myproject                # starts in ~/dev/myproject
comux new-session api --cwd ~/dev/api -d   # starts in ~/dev/api, without switching to it
```

`comux list-sessions --json` reports each session's start directory as `dir`.

`comux new-session` also **auto-starts the server** if none is running — so you can go from a plain shell straight into a named workspace in one command.

---

## The fuzzy switcher (`Ctrl-f`)

`Ctrl-f` (prefix-less) opens a popup with three lists — **Sessions**, **Agents** and **Tabs**
(the current session's tabs). `Ctrl-b f` (or `Ctrl-b w`, tmux's `choose-window`) opens it
straight on **Tabs** with the tab you used before this one selected, so `Ctrl-b f Enter` flips
between two tabs like Alt-Tab. A wide terminal previews the selected tab's focused pane.

| Key | Action |
| --- | --- |
| `←` / `→` | Switch between the Sessions, Agents and Tabs lists |
| `↑` / `↓`, or `Ctrl-p` / `Ctrl-n` | Move the selection |
| type any text | Fuzzy-filter the list |
| `Enter` | Switch to the session / jump to the agent / go to the tab |
| `Ctrl-r` or `F2` | Rename the selected session inline |
| `Esc` or `Ctrl-f` | Close |

This is the fastest way to hop between many sessions or straight to whichever agent needs you.

---

## The sidebar

An always-on left sidebar (toggle with `Ctrl-b s`) shows two groups:

- **spaces** — your sessions, each with its **git-branch subtitle** read live from `.git/HEAD`. When they overflow, it windows around the active one with a `+N more · Ctrl-f` hint.
- **agents** — every agent pane across all sessions, with `status · tool` (e.g. `working · Edit`).

Make the sidebar keyboard-navigable with `Ctrl-b e`: `↑↓`/`jk` move, `←→`/`hl` switch between the spaces and agents groups, `Enter` selects, `Esc` exits.

With the mouse enabled (the default), you can also click directly:

- a **status-bar tab chip** → switch tabs
- a **sidebar `spaces` row** → switch sessions
- a **sidebar `agents` row** → jump to that agent's pane
- **right-click on the chrome** → a context menu (rename/close/new for tabs, rename/new/kill for sessions, jump/rename for agents)

---

## Scrollback (copy-mode)

Enter copy mode for the focused pane with `Ctrl-b [`. It speaks tmux's `copy-mode-vi` — `hjkl`, `w`/`b`/`e`,
`f`/`t` jumps, `/` and `?` search with every match lit up as you type, `v`/`V`/`Ctrl-v` character, line and rectangle
selections, `o` for the other end, `y` to copy and leave. The full table is in
[Keybindings](./keybindings.md#copy-mode-after-ctrl-b-).

The **mouse wheel** scrolls the pane under the cursor — but if that pane's app has mouse reporting on (Claude Code, `nvim` with `set mouse`), the wheel is forwarded to the app instead, and an alt-screen pager (`less`, `man`) gets cursor-key presses. Only a plain shell scrolls comux's own scrollback.

### Copying text

A left-**drag** inside a pane runs comux's own selection (clamped to that pane, so the sidebar and other panes are never included) and, on release, copies that pane's text to your system clipboard via **OSC 52** — which works over SSH. **Double-click** copies the word under the pointer and **triple-click** the whole line. A plain click just focuses. For anything beyond the screen — or a rectangle — use copy mode.

> **Known limits (deliberate):** a mouse drag stays within the visible screen, and a VS16/ZWJ emoji copies as its base glyph (`❤️` → `❤`).

Programs **inside** a pane can set the clipboard too: comux relays their OSC 52 writes out to every attached client, which re-emits them to its own terminal (tmux `set-clipboard`). So `nvim` with `set clipboard=unnamedplus` and an osc52 provider, or any `yank`-style helper, lands on the clipboard of the machine you are sitting at — including over SSH. Set `osc52 = false` in `mux.toml` to stop a background pane from clobbering your clipboard. Clipboard **reads** (`OSC 52 ... ?`) are never answered, so no program in a pane can exfiltrate what you have copied.

Either path needs your terminal to accept OSC 52: kitty, WezTerm, Alacritty and Ghostty do by default; **iTerm2** needs *Settings → General → Selection → "Applications in terminal may access clipboard"*; macOS Terminal.app does not support it at all. An enclosing `tmux`/`screen` swallows the sequence unless it has `set -g set-clipboard on`.

---

## Detach & re-attach

| Action | Key |
| --- | --- |
| Detach this client | `Ctrl-b d` |
| Re-attach | run `comux` again |
| Force a full repaint (clears drift/ghosting) | `Ctrl-b r` |

Detaching leaves the server — and every shell and agent — running. Several clients can attach at once; the view is sized to the smallest attached terminal (bigger ones letterbox), and input is shared. `Ctrl-b d` detaches only your client.
