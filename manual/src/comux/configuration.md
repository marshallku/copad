# Configuration (`mux.toml`)

comux reads `~/.config/copad/mux.toml` (precisely: `$XDG_CONFIG_HOME/copad/mux.toml`, else `~/.config/copad/mux.toml`). The file is entirely optional — **zero config behaves exactly like a fresh comux**. Any omitted key falls back to its built-in default; a bad value warns once on startup and falls back.

The design follows tmux's `tmux.conf`: your file is overlay-merged onto the defaults. See `copad-mux/mux.example.toml` in the repo for a fully-commented starting point.

## Live reload

Most settings apply on the running server without a restart:

```bash
comux reload          # re-read mux.toml on the live server (alias: comux source-file)
```

`reload` re-reads the file, swaps the config in place, and prints the config path plus any parse warnings — it never breaks the running mux. Keybindings, mouse, `osc52`, `sidebar_width`, all `usage_*`, `tab_labels`, `notify`, and worktree settings apply on the next frame.

**Four settings are fixed at server boot** and need `comux server restart` instead: `persist`, `autosave_secs`, `update_environment`, and `never_inherit`. (`restore_processes` / `restore_agent_sessions` are read at save time, so `reload` does update them for the next save.)

---

## Top-level options

| Key | Default | Allowed / range | Notes |
| --- | --- | --- | --- |
| `prefix` | `"C-b"` | any chord | The prefix key |
| `mouse` | `true` | bool | Wheel-scroll, click-to-focus, click chrome to navigate |
| `osc52` | `true` | bool | Relay a pane program's OSC 52 clipboard write to the attached clients (tmux `set-clipboard`). Clipboard *reads* are never answered |
| `notify` | `true` | bool | Agent turn-completion desktop toasts (`COPAD_MUX_NOTIFY` wins) |
| `sidebar` | `true` | bool | Show the spaces+agents sidebar by default |
| `sidebar_width` | `24` | `8`–`80` | Sidebar columns |
| `sidebar_min_cols` | `80` | `40`–`400` (forced ≥ `sidebar_width + 20`) | Hide the sidebar below this terminal width |
| `scroll_step` | `3` | `1`–`50` | Lines per mouse-wheel notch |
| `sort_by` | `"created"` | `created` / `alphabetical` / `recent` / `activity` | Session order in sidebar, switcher, and `)(`/cycle |
| `tab_labels` | `"number"` | `number` / `name` / `both` | What each status-bar tab chip shows |
| `update_check` | `true` | bool | GitHub-release check + `⬆ x.y.z` hint (`COPAD_MUX_UPDATE_CHECK=0` disables) |
| `persist` | `true` | bool | Restore the saved layout on server start *(boot-fixed)* |
| `autosave_secs` | `15` | `0` disables; else `5`–`3600` | Periodic save interval *(boot-fixed)* |
| `restore_processes` | AI agents (see below) | list of basenames; `[]` = bare shells | Programs re-run on restore |
| `restore_agent_sessions` | `true` | bool | Resume agent conversations on restore |
| `update_environment` | volatile session vars (see below) | list of var names; `[]` disables | Vars refreshed into new panes from the attaching client *(boot-fixed)* |
| `never_inherit` | agent session markers (see below) | list of var names; **added** to the default | Vars scrubbed from the daemon and never given to a pane *(boot-fixed)* |

`restore_processes` default:

```toml
restore_processes = ["claude", "codex", "aider", "cursor", "gemini",
                     "opencode", "droid", "copilot", "qwen", "crush"]
```

`update_environment` default (see [Environment Variables](./environment.md#environment-refresh) for why):

```toml
update_environment = ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "XDG_SESSION_TYPE",
                      "DBUS_SESSION_BUS_ADDRESS", "SSH_ASKPASS", "SSH_AUTH_SOCK",
                      "SSH_AGENT_PID", "SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY",
                      "WINDOWID", "KRB5CCNAME", "TERM_PROGRAM"]
```

`never_inherit` built-in list (see [Never inherited](./environment.md#never-inherited-agent-session-markers) for why) — the opposite rule to `update_environment`: scrubbed at boot, kept out of the boot pane, and never refreshed from a client:

```toml
never_inherit = ["CLAUDE_CODE_CHILD_SESSION", "CLAUDECODE", "CLAUDE_CODE_SESSION_ID",
                 "CLAUDE_CODE_BRIDGE_SESSION_ID", "CLAUDE_CODE_ENTRYPOINT"]
```

Unlike every other list option, yours is **added** to that default instead of replacing it — replacing would let `never_inherit = ["MY_MARKER"]` silently drop `CLAUDE_CODE_CHILD_SESSION` and bring back the lost-transcript bug. A name in both lists is dropped from `update_environment` with a warning (`never_inherit` wins, otherwise the next attach would refresh it right back).

Load-bearing names (`PATH`, `HOME`, `SHELL`, `USER`, `TERM`, `PWD`, `COPAD_MUX`, …) are refused in either list if you add them.

---

## Usage / limits options

These control the status-bar rate-limit readout. Full explanation with screenshots-in-prose is in [The Status Bar](./status-bar.md#the-usage--limits-readout).

| Key | Default | Allowed |
| --- | --- | --- |
| `usage` | `"bar"` | `bar` / `text` / `off` |
| `usage_windows` | all three | list of `claude-5h`, `claude-wk`, `codex-wk` (aliases `claude`, `codex`); `[]` hides it |
| `usage_bar_width` | `8` | `3`–`30` |
| `usage_layout` | `"paged"` | `paged` / `inline` |
| `usage_page_unit` | `"window"` | `window` / `provider` / `metric` |
| `usage_reset` | `"relative"` | `relative` / `absolute` / `off` |
| `usage_rotate_secs` | `0` | `0`–`3600` (`0` = manual only) |

---

## `[keys]` and `[global]` — keybindings

`[keys]` remaps **prefix** bindings; `[global]` remaps **prefix-less** ones. Each entry is `action = chord` or `action = [chord, …]`, and overriding an action **replaces its whole default chord set**. See [Keybindings → Customizing bindings](./keybindings.md#customizing-bindings) for the full action-name list and examples.

```toml
prefix = "C-a"

[keys]
split-right = "|"
split-down  = "-"
new-tab     = ["c", "t"]

[global]
popup = "C-Space"
```

---

## `[worktree]` and `[worktree.scripts]`

| Key | Default | Notes |
| --- | --- | --- |
| `naming` | `"{repo}-{branch}"` | Worktree dir name pattern. Tokens: `{repo}` (main worktree dir name), `{branch}` (with `/` → `-`) |

`[worktree.scripts]` maps a repo's **main-worktree path** to a post-create shell command, run via `bash -c` in the new worktree with `$WORKTREE_PATH` set:

```toml
[worktree]
naming = "{repo}-{branch}"

[worktree.scripts]
"~/dev/copad"   = "mise trust && cargo fetch"
"~/dev/web-app" = "mise trust && yarn"
```

See [Git Worktrees](./worktrees.md) for the full workflow.

---

## A complete example `mux.toml`

```toml
# ---- prefix & input ----
prefix      = "C-a"
mouse       = true
osc52       = true
scroll_step = 5

# ---- sidebar ----
sidebar          = true
sidebar_width    = 28
sidebar_min_cols = 90
sort_by          = "activity"

# ---- status bar ----
notify            = true
tab_labels        = "both"
usage             = "bar"
usage_layout      = "paged"
usage_page_unit   = "provider"
usage_reset       = "relative"
usage_rotate_secs = 8
usage_bar_width   = 10
update_check      = true

# ---- persistence ----
persist                 = true
autosave_secs           = 15
restore_processes       = ["claude", "codex", "nvim"]
restore_agent_sessions  = true

# ---- keybindings ----
[keys]
split-right = "|"
split-down  = "-"

[global]
popup = "C-Space"

# ---- worktrees ----
[worktree]
naming = "{repo}-{branch}"

[worktree.scripts]
"~/dev/copad" = "mise trust && cargo fetch"
```
