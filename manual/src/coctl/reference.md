# Command Reference

Every `coctl` command, grouped by area. Positional arguments are shown in `<angle>` brackets; `--flags` are options. Commands marked **local** don't need a running copad and work over SSH.

- [Core](#core) · [Session](#session) · [Tab](#tab) · [Split & Pane](#split--pane) · [Terminal](#terminal) · [WebView](#webview) · [Background](#background) · [Theme & Status bar](#theme--status-bar) · [Agent](#agent) · [Usage](#usage) · [Events](#events) · [Project & Workflow](#project--workflow)
- Plugin wrappers: [Plugin](#plugin) · [Todo](#todo) · [Git](#git) · [Bookmark](#bookmark) · [Pomodoro](#pomodoro) · [Jira](#jira) · [Slack](#slack) · [Calendar](#calendar)

---

## Core

| Command | Description |
| --- | --- |
| `coctl ping` | Ping the running instance. |
| `coctl context [--full]` | Workflow context. `--json --full` aggregates panel + cwd, git, todos, calendar, and messenger auth; bare `--json` returns the raw `{active_panel, active_cwd}` snapshot. |
| `coctl call <method> [--params '<json>']` | Escape hatch — invoke any registry action by name. `--params` defaults to `"{}"`. |

```bash
coctl ping
coctl context --full
coctl call system.list_actions
coctl call kb.search --params '{"query":"socket"}'
coctl call cockpit.open        # open the agent cockpit (Linux)
```

---

## Session

Panels are the GUI's internal term for panes/tabs.

| Command | Description |
| --- | --- |
| `coctl session list` | List all panels across all tabs. |
| `coctl session info <id>` | Detailed info for a panel. |
| `coctl presence away` / `active` / `status` | Set or read your presence (drives external-sink triggers). |

---

## Tab

| Command | Description |
| --- | --- |
| `coctl tab new` | Create a tab. |
| `coctl tab close` | Close the focused tab/panel. |
| `coctl tab list` | List tabs. |
| `coctl tab info` | Extended info with panel counts. |
| `coctl tab switch <index>` | Switch to a tab by zero-based index. |
| `coctl tab toggle-bar` | Toggle the tab bar collapsed/expanded. |
| `coctl tab rename --id <id> <title>` | Rename a tab by panel id. |

```bash
coctl tab switch 2
coctl tab rename --id 7c1d "build logs"
```

---

## Split & Pane

| Command | Description |
| --- | --- |
| `coctl split horizontal` | Split the focused pane horizontally. |
| `coctl split vertical` | Split it vertically. |
| `coctl pane focus-next` | Focus the next pane (like `Ctrl+Shift+N`). |
| `coctl pane focus-prev` | Focus the previous pane. |

---

## Terminal

Read from and write to a terminal pane. All accept `--id <id>` (defaults to the active terminal).

| Command | Description |
| --- | --- |
| `coctl terminal read [--start-row N --start-col N --end-row N --end-col N]` | Read the visible screen, or a range. |
| `coctl terminal state` | Cursor, dimensions, cwd, title. |
| `coctl terminal exec <command>` | Run a command (text + newline). |
| `coctl terminal feed <text>` | Send raw text (no newline). |
| `coctl terminal history [--lines 100]` | Read scrollback. |
| `coctl terminal context [--history-lines 50]` | State + screen + scrollback combined. |

```bash
coctl terminal exec --id 4b1c "git status"
coctl terminal read --start-row 0 --end-row 20
coctl terminal context --history-lines 100
```

---

## WebView

Open and drive webview panels. All subcommands except `open` need `--id <id>`.

| Command | Description |
| --- | --- |
| `coctl webview open <url> [--mode tab\|split_h\|split_v]` | Open a URL in a new webview panel (`--mode` default `tab`). |
| `coctl webview navigate --id <id> <url>` | Navigate an existing webview. |
| `coctl webview back` / `forward` / `reload` `--id <id>` | History / reload. |
| `coctl webview exec-js --id <id> <code>` | Run JavaScript, return the result. |
| `coctl webview get-content --id <id> [--format text\|html]` | Get page content. |
| `coctl webview screenshot --id <id> [--path <file>]` | Screenshot (base64 PNG, or saved to `--path`). |
| `coctl webview query --id <id> <selector>` | Query one DOM element. |
| `coctl webview query-all --id <id> <selector> [--limit 50]` | Query all matches. |
| `coctl webview get-styles --id <id> <selector> <properties>` | Computed CSS (`properties` comma-separated). |
| `coctl webview click --id <id> <selector>` | Click an element. |
| `coctl webview fill --id <id> <selector> <value>` | Type into an input. |
| `coctl webview scroll --id <id> [--selector <sel>] [--x 0] [--y 0]` | Scroll to a selector or coordinates. |
| `coctl webview page-info --id <id>` | Page metadata. |
| `coctl webview devtools --id <id> [show\|close\|attach\|detach]` | Toggle DevTools (default `show`). |

```bash
coctl webview open https://example.com --mode split_v
coctl webview exec-js --id 9a2f "document.title"
coctl webview get-styles --id 9a2f "h1" "color,font-size"
coctl webview screenshot --id 9a2f --path /tmp/shot.png
```

---

## Background

See [Themes & Backgrounds](../copad/themes-and-backgrounds.md) for the full walkthrough.

| Command | Description |
| --- | --- |
| `coctl background set <path>` | Set the background image. |
| `coctl background clear` | Clear it. |
| `coctl background set-tint <opacity>` | Set the tint (0.0–1.0). |
| `coctl background next` | Next random background. |
| `coctl background toggle` | Toggle visibility. |
| `coctl background delete-current` | Delete the current list-picked wallpaper and rotate. |
| `coctl background cache [--path <dir>] [--output <file>] [--recursive] [--force]` | **local** — build the wallpaper list file. |

---

## Theme & Status bar

| Command | Description |
| --- | --- |
| `coctl theme list` | List themes and the current one. |
| `coctl statusbar show` / `hide` / `toggle` | Control the status bar. |

---

## Agent

| Command | Description |
| --- | --- |
| `coctl agent status [--oneline] [--json]` | **local** — running Claude/Codex agents and their state (via `tmx`; works over SSH). `--oneline` gives a compact `▶2 busy ⏸1 waiting · copad, docs`. |
| `coctl agent approve <message> [--title <t>] [--actions "A,B"]` | Show a blocking approval dialog. `--title` default `"Agent Action"`; `--actions` comma-separated (first = approve). |

```bash
coctl agent status --oneline
coctl agent approve "Delete 3 branches?" --title "Cleanup" --actions "Yes,No"
```

---

## Usage

**local** — aggregates Claude + Codex token/cost from local session logs. See below for `--limits`.

```
coctl usage [--window today|all] [--since <dur>] [--tool claude|codex] [--oneline] [--limits] [--json]
```

| Flag | Effect |
| --- | --- |
| `--window today\|all` | `today` (local midnight → now) or `all`. Default `today`. Ignored when `--since` is given. |
| `--since <dur>` | Rolling window ending now (`5h`, `30m`, `2d`). Overrides `--window`. |
| `--tool claude\|codex` | Restrict to one tool (default: both). |
| `--oneline` | Compact single line (for a tmux status bar). |
| `--limits` | Show **subscription rate-limit utilization** instead of token/cost. Different source: Claude via a live OAuth usage call, Codex from the newest rollout file. Ignores `--window`/`--since`. Caches last-good values (shown `~`-prefixed) for up to 3h if a live fetch fails. |

```bash
coctl usage
coctl usage --oneline
coctl usage --since 5h --tool claude
coctl usage --limits --json
```

---

## Events

| Command | Description |
| --- | --- |
| `coctl event subscribe` | Stream terminal/bus events as JSON lines to stdout. |
| `coctl event publish <kind> [<payload>] [--quiet]` | **daemon-direct** — publish an event to fire `[[triggers]]` from scripts. `<kind>` must not end in `.completed`/`.failed`. `<payload>` is optional JSON (default `{}`). `--quiet` exits 0 if the daemon is unreachable. Events are tagged `External` and only reach triggers with `[security] accept_external = true`. |
| `coctl recent [--since <dur>] [--kind <glob>]` | Recent events (`--since` default `1h`; `--kind` a glob like `jira.*`). |
| `coctl runledger query [--since-ms N] [--kinds '<glob,glob>'] [--limit N]` | Replay events from the durable ledger. |

```bash
coctl event subscribe
coctl event publish panel.focused '{"panel_id":"abc"}'
coctl recent --since 2h --kind 'jira.*'
```

---

## Project & Workflow

| Command | Description |
| --- | --- |
| `coctl project list` | List configured projects (from `[[projects]]`). |
| `coctl project resolve [--name <n>] [--cwd <path>] [--git-remote <owner/repo>] [--active]` | Resolve a project by name/alias, cwd ancestry, git remote, or the active pane. |
| `coctl workflow list` | List workflows. |
| `coctl workflow get <id>` | Full spec for a workflow (positional id). |
| `coctl workflow run --id <id> [--project <name>] [--values '<json>'] [--value name=val …]` | Dispatch a workflow (note: `run` takes `--id`, but `get` takes a positional id). |

```bash
coctl project resolve --git-remote marshallku/copad
coctl workflow run --id ship --project copad --value version=1.2.0
```

---

## Plugin

| Command | Description |
| --- | --- |
| `coctl plugin list` | List installed plugins with their panels and commands. |
| `coctl plugin open <plugin> [--panel main]` | Open a plugin panel in a new tab. |
| `coctl plugin run <plugin>.<command> [--params '{}']` | Run a plugin command. |

```bash
coctl plugin list
coctl plugin open kb --panel search
coctl plugin run my-plugin.greet --params '{"name":"Marshall"}'
```

The remaining commands are ergonomic wrappers around specific plugins. They only work if that plugin is installed (see [Using Plugins](../plugins.md)), and several need credentials.

## Todo

Ids accept a unique prefix; ambiguous ids error with candidates (disambiguate with `--workspace`). Workspace defaults to `COPAD_TODO_DEFAULT_WORKSPACE`.

```bash
coctl todo create "Fix socket race" --priority high --tags bug,socket
coctl todo list --status open --tag loop
coctl todo done 3f
coctl todo doing 3f
coctl todo update 3f --append-subtask "write a test"
coctl todo show 3f
```

Also: `todo set --status <s>`, `todo block`, `todo start`, `todo delete`, `todo loop <id> [--copy]`.

## Git

Workspace defaulting: `--workspace` → `COPAD_GIT_DEFAULT_WORKSPACE` → cwd-derived.

```bash
coctl git workspaces
coctl git worktrees
coctl git wt add feature/login --sanitize-jira
coctl git wt remove <path> --force
coctl git branch
coctl git status                # cwd-derived workspace
```

## Bookmark

Captures URLs into `~/docs/bookmarks/` (override with `COPAD_BOOKMARK_ROOT`).

```bash
coctl bookmark add https://example.com/post --tags read,rust
coctl bookmark list --tag rust --limit 20
coctl bookmark show a1b2
coctl bookmark delete a1b2
```

## Pomodoro

```bash
coctl pomodoro start --minutes 50 --label "docs"
coctl pomodoro pause
coctl pomodoro resume
coctl pomodoro toggle
coctl pomodoro reset
coctl pomodoro status
coctl pomodoro set-durations --work 25 --break 5 --long-break 15 --rounds 4
```

## Jira

Needs Jira credentials.

```bash
coctl jira mine --status "In Progress" --project PROJ
coctl jira ticket PROJ-123
coctl jira transition PROJ-123 "Done"
coctl jira comment PROJ-123 "Deployed to prod"
coctl jira auth-status
```

## Slack

Needs a Slack token.

```bash
coctl slack send '#general' "Deploy done"
coctl slack send C0123ABCD "reply" --thread-ts 1700123456.789012
coctl slack get C0123ABCD 1700123456.789012
coctl slack auth-status
```

## Calendar

Needs Google Calendar OAuth.

```bash
coctl calendar today
coctl calendar next --within 4
coctl calendar event abc123
coctl calendar auth-status
```
