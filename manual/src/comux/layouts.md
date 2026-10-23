# Layouts & Arranging Panes

Splitting is only half of working with panes. comux can also move them around, snap a tab into a preset shape, and
build a whole session from a file.

## Rearranging a tab

| Keys (after `Ctrl-b`) | CLI | Does |
| --- | --- | --- |
| `{` / `}` | `comux swap-pane -U` / `-D` | Trade places with the previous / next pane. Focus stays on the pane you moved. |
| `C-o` / `M-o` | `comux rotate-window` / `-D` | Every pane moves one slot. |
| `Space` | `comux select-layout next` | Cycle through the presets below. |
| `M-1` … `M-5` | `comux select-layout <name>` | One preset outright. |
| `E` | `comux select-layout -E` | Keep the shape, even out every split. |
| `B` | `comux break-pane` | Move the pane into a new tab of its own, right after this one. |
| `S-Left` / `S-Right` | `comux move-tab <position>` | Move the tab along the tab bar. |
| `q` | — | Number the panes; press a digit to jump there. |

The presets are tmux's:

| Preset | Shape |
| --- | --- |
| `even-horizontal` | every pane side by side, equal widths |
| `even-vertical` | every pane stacked, equal heights |
| `main-horizontal` | the first pane across the top (60%), the rest side by side below |
| `main-vertical` | the first pane on the left (60%), the rest stacked to its right |
| `tiled` | as close to a square grid as the pane count allows |

Swap a pane into the first slot (`{`) before choosing a `main-*` layout to make it the main one.

`select-layout -E` keeps your arrangement but evens it out: splitting a pane, then splitting the new pane, and so on
gives widths of 50%, 25%, 12.5%…; after `E` they are equal. Panes are counted along each split's direction, so
`left | (top / bottom)` stays half and half.

The CLI verbs act on **your** tab — the one the calling pane is in — or `--pane <token>`'s.

## Building a session from a file

Describe a workspace once and stand it up any time:

```toml
# dev.toml
name = "api"              # the session (default: the file's name)
cwd = "~/dev/api"         # where it starts (default: the file's directory)

[[tab]]
name = "code"
layout = "main-vertical"  # any preset; default tiled

[[tab.pane]]
run = "nvim ."

[[tab.pane]]
run = ["cargo build", "cargo watch -x test"]

[[tab.pane]]              # a plain shell

[[tab]]
name = "agents"
layout = "even-horizontal"

[[tab.pane]]
run = "claude"

[[tab.pane]]
cwd = "web"               # relative to the session's cwd
run = "codex"
```

```sh
comux apply-layout dev.toml            # build it and switch to it (attaches, from a plain shell)
comux apply-layout dev.toml --dry-run  # print what it would build
comux apply-layout dev.toml -d         # build it in the background
comux apply-layout dev.toml --name api-2 -d   # a second copy under another name
```

Every pane is a shell, and its `run` lines are **typed** into it, one per Enter — as tmuxinator does. Your aliases and
rc files apply, the lines land in the shell's history, and their output stays when they finish. Relative `cwd`s
resolve against the session's (or the tab's) directory, `~` is your home, and a directory that does not exist stops
the build before anything is created. `copad-mux/layout.example.toml` in the repository is a commented example.

## Kept command panes

`comux spawn --keep -- <command>` and `comux split --keep -- <command>` run a program in a pane and keep the pane when
it ends: the output stays, a line says how it exited (`exited 1`), and the pane becomes a shell.
