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

`comux focus` / `close` / `select-tab` / `select-session` with **no index** open a fuzzy picker over the live listing instead of erroring — see [Leave the argument out and pick](./index.md#leave-the-argument-out-and-pick).

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
| Close tab | `Ctrl-b &` | — |
| Rename tab | `Ctrl-b ,` | `comux rename-tab [index] <name>` |
| List tabs | — | `comux list-tabs` |

> **`Alt`/`Option`+number** needs your terminal set to send Option/Alt as Meta.

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
| New session (inline name prompt) | `Ctrl-b C` | `comux new-session [name]` |
| Rename session | `Ctrl-b $` | `comux rename-session [index] <name>` |
| Kill session (with y/n confirm) | `Ctrl-b X` | — |
| Next / previous session | `Ctrl-b )` / `Ctrl-b (` | — |
| Switch to a session | via sidebar / `Ctrl-f` | `comux select-session <index>` |
| List sessions | — | `comux list-sessions` |

### cwd inheritance

New sessions, tabs, and splits start in the **current directory**, like tmux's `-c '#{pane_current_path}'`:

```bash
cd ~/dev/myproject
comux new-session myproject      # the session's shell starts in ~/dev/myproject
```

`comux new-session` also **auto-starts the server** if none is running — so you can go from a plain shell straight into a named workspace in one command.

---

## The fuzzy switcher (`Ctrl-f`)

`Ctrl-f` (prefix-less) opens a popup with two tabs — **Sessions** and **Agents**:

| Key | Action |
| --- | --- |
| `←` / `→` | Switch between the Sessions and Agents tabs |
| `↑` / `↓`, or `Ctrl-p` / `Ctrl-n` | Move the selection |
| type any text | Fuzzy-filter the list |
| `Enter` | Switch to the session / jump to the agent |
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

Enter scrollback for the focused pane with `Ctrl-b [`:

| Key | Action |
| --- | --- |
| `k` / `↑` | Up one line |
| `j` / `↓` | Down one line |
| `PageUp` / `Ctrl-u` | Up half a page |
| `PageDown` / `Ctrl-d` | Down half a page |
| `g` | Jump to the top |
| `G` / `q` / `Esc` | Return to the live bottom and exit |

The **mouse wheel** scrolls the pane under the cursor — but if that pane's app has mouse reporting on (Claude Code, `nvim` with `set mouse`), the wheel is forwarded to the app instead, and an alt-screen pager (`less`, `man`) gets cursor-key presses. Only a plain shell scrolls comux's own scrollback.

### Copying text

A left-**drag** inside a pane runs comux's own selection (clamped to that pane, so the sidebar and other panes are never included) and, on release, copies that pane's text to your system clipboard via **OSC 52** — which works over SSH. A plain click just focuses.

> **Known v1 limits (deliberate):** no scrollback-drag, no block selection, and a VS16/ZWJ emoji copies as its base glyph (`❤️` → `❤`). There is no keyboard yank binding — selection is mouse-only.

---

## Detach & re-attach

| Action | Key |
| --- | --- |
| Detach this client | `Ctrl-b d` (or `Ctrl-b q`) |
| Re-attach | run `comux` again |
| Force a full repaint (clears drift/ghosting) | `Ctrl-b r` |

Detaching leaves the server — and every shell and agent — running. Several clients can attach at once; the view is sized to the smallest attached terminal (bigger ones letterbox), and input is shared. `Ctrl-b d` detaches only your client.
