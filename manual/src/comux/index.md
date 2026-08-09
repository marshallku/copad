# comux — Overview

**comux** is a standalone terminal multiplexer built for orchestrating AI agents. If you know tmux, you already know comux: a prefix key, sessions, tabs, split panes, a status bar, detach/re-attach. What comux adds is a first-class understanding of the `claude` / `codex` agents you run inside it — live per-agent status, desktop notifications the moment one needs you, and a restart that brings every agent back *mid-conversation*.

- Binary: **`comux`** · Crate/dir: `copad-mux` · Config: `~/.config/copad/mux.toml`
- Installs as a **single self-contained binary** — see [comux only](../installation.md#comux-only).

## The server/client model

comux is split into a **persistent server** and a **thin client**:

- The **server** owns all state and PTYs. It survives the terminal that launched it — close your terminal, log out and back in, even reboot, and (with persistence on) your sessions come back.
- The **client** just renders the current view and forwards your input. Several clients can attach to the same server at once.

You don't manage these separately in normal use — bare `comux` connects to the server if it's running and spawns it if it isn't, then attaches.

## Invocation forms

```bash
comux                    # attach a client, spawning the server if needed
comux attach             # same as bare comux
comux server             # run the server in the foreground (headless)
comux server start       # ensure the server is running
comux server stop        # shut the server down (final save)
comux server restart     # stop + start; restores the saved layout, agents resume
comux server status      # "running (<path>)" (exit 0) or "not running" (exit 1)
comux help               # usage
```

### Control commands — with or without `ctl`

Every control verb works two ways, tmux-style:

```bash
comux ctl new-session work     # explicit
comux new-session work         # shorthand — identical
```

Any verb comux doesn't recognize as a built-in is treated as a control command. Add `--json` to any of them to get the raw JSON response instead of the human-readable output.

The full verb list:

| Verb (aliases) | Arguments | What it does |
| --- | --- | --- |
| `list` | — | List panes of the active tab |
| `split` | `-h` (default) \| `-v`/`down` | Split the focused pane |
| `resize` | `<index> <left\|right\|up\|down>` | Grow a pane toward a direction |
| `focus` | `[index]` | Focus a pane (no index → fuzzy picker) |
| `close` | `[index]` | Close a pane (no index → fuzzy picker) |
| `send` / `send-keys` | `<index> <text…>` | Inject text as input into a pane |
| `list-tabs` / `tabs` | — | List the workspace's tabs |
| `new-tab` | — | Create + activate a tab |
| `select-tab` | `[index]` | Activate a tab by index (no index → fuzzy picker) |
| `rename-tab` | `[index] <name…>` | Rename a tab (no index = active; `""` clears) |
| `list-sessions` / `sessions` | — | List sessions |
| `new-session` | `[name…]` | Create + switch to a session (starts the server if needed) |
| `rename-session` / `rename` | `[index] <name…>` | Rename a session (no index = active) |
| `select-session` | `[index]` | Switch to a session (no index → fuzzy picker) |
| `worktree create` (`new`/`add`) | `<branch> [--from <ref>] [--no-attach] [--json]` | Create a git worktree + a session in it |
| `worktree list` (`ls`) | `[--plain\|--json]` | List worktrees (flags which have a live session) |
| `worktree rm` (`remove`) | `[path\|branch] [-f] [-d] [--json]` | Remove a worktree (no target → fuzzy picker) |
| `reload` / `source-file` | — | Re-read `mux.toml` on the running server |
| `kill-server` | — | Shut the server down |

### Leave the argument out and pick

Every verb above whose argument is written `[index]` / `[path|branch]` opens a **fuzzy picker** when you omit it, so you never have to run `list` first and copy an index or a path back:

```bash
comux worktree rm      # lists the repo's removable worktrees — type to narrow, Enter to remove
comux select-session   # same for sessions; also select-tab, focus, close
```

Type to filter (fzf-style subsequence, matching the name *and* the dim detail column), `↑`/`↓` or `Ctrl-n`/`Ctrl-p` to move, `Enter` to pick, `Esc` (or `Ctrl-c`) to cancel — cancelling exits `130` and does nothing. The picker draws inline below your prompt and erases itself on exit, so your scrollback stays clean.

It only opens when it can: with `--json`, or when stderr isn't a terminal (a pipe, a script, CI), the old usage error stands so nothing ever blocks waiting for a prompt no one can answer. A *malformed* index (`comux focus abc`) is still an error too — the picker is for an argument you left out, not one you got wrong.

> See [Sessions, Tabs & Panes](./sessions-tabs-panes.md) for how these map to the in-app keys, and [Git Worktrees](./worktrees.md) for the worktree workflow.

## What makes comux different from tmux

- **Agent awareness.** comux reads each agent pane's real status (Claude via `~/.claude/sessions/<pid>.json`, Codex/others via screen-text) and shows `working` / `ready` / `blocked` in the sidebar and status bar.
- **Turn notifications.** A native desktop toast fires when an agent finishes a turn or starts waiting for input — even while you're detached. `Ctrl-b !` jumps to a blocked agent; `Ctrl-b a` opens a notification center.
- **Agents resume mid-conversation.** On restart, a restored agent pane doesn't start a fresh chat — it reconnects to its live session (`claude --resume <id>` / `codex resume <id>`). See [Persistence & Agent Resume](./persistence.md).
- **Git worktrees as a first-class verb.** `comux worktree create <branch>` makes a sibling worktree, runs a per-repo hook, and drops you into a session there. See [Git Worktrees](./worktrees.md).
- **A rich, always-on status bar** with a usage/limits carousel, agent counts, and an attention indicator. See [The Status Bar](./status-bar.md).

## Zero-config, then configure

Everything works out of the box. When you want to customize keybindings, the prefix, the sidebar, the status bar, persistence, or worktree hooks, drop a `~/.config/copad/mux.toml` — see [Configuration](./configuration.md). Live-reloadable settings apply with `comux reload`, no restart needed.
