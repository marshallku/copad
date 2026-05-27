# CLI Tool (copad-cli)

Binary name: `coctl`

## Usage

```bash
coctl [--socket <path>] [--json] <command>
```

- `--socket` — override socket path (default: `$COPAD_SOCKET` or `/tmp/copad-{PID}.sock`)
- `--json` — output in JSON format

## Commands

### System

- `coctl ping` — ping running instance
- `coctl context [--full]` — workflow context. **Default (human mode)** aggregates: active panel + cwd, resolved workspace + git status (branch, ahead/behind, dirty), open + in-progress todos for that workspace, calendar events in the next 2h, slack/discord auth state. Each section degrades to `(unavailable)` independently when its action call fails. **`--json`** (without `--full`) returns the raw `context.snapshot` shape (`{active_panel, active_cwd}`) verbatim, for backward compatibility with scripts already piping it. **`--json --full`** emits the aggregate as a single JSON object — useful for scripting "what's the user's current cross-plugin state?" without N round-trips. Workspace resolution mirrors the `coctl git` cwd-derive (longest-prefix match against `path` or `worktree_root`, both canonicalized); when cwd doesn't match any workspace, workspace-bound sections (git, todos) are simply skipped — the CLI doesn't pretend the user is in a workspace they're not.

### Session

- `coctl session list` — list all panels across all tabs
- `coctl session info <id>` — detailed info for a panel

### Background

- `coctl background set <path>` — set background image (path is canonicalized)
- `coctl background clear` — clear background image
- `coctl background set-tint <opacity>` — set tint opacity (0.0–1.0)
- `coctl background next` — switch to next random background
- `coctl background toggle` — toggle background visibility

### Tab

- `coctl tab new` — create a new tab
- `coctl tab close` — close the focused tab/panel
- `coctl tab list` — list tabs
- `coctl tab info` — extended tab info with panel counts
- `coctl tab toggle-bar` — toggle tab bar collapsed/expanded
- `coctl tab rename --id <id> <title>` — rename a tab by panel ID

### Split

- `coctl split horizontal` — split focused pane horizontally
- `coctl split vertical` — split focused pane vertically

### Event Stream

- `coctl event subscribe` — subscribe to terminal events (streams JSON lines to stdout)

### WebView

- `coctl webview open <url> [--mode tab|split_h|split_v]` — open URL in new webview panel
- `coctl webview navigate --id <id> <url>` — navigate existing webview
- `coctl webview back --id <id>` — go back in history
- `coctl webview forward --id <id>` — go forward in history
- `coctl webview reload --id <id>` — reload page
- `coctl webview exec-js --id <id> <code>` — execute JavaScript, return result
- `coctl webview get-content --id <id> [--format text|html]` — get page content
- `coctl webview screenshot --id <id> [--path <file>]` — screenshot (base64 PNG or save to file)
- `coctl webview query --id <id> <selector>` — query single DOM element
- `coctl webview query-all --id <id> <selector> [--limit 50]` — query all matching elements
- `coctl webview get-styles --id <id> <selector> <properties>` — get computed CSS styles
- `coctl webview click --id <id> <selector>` — click a DOM element
- `coctl webview fill --id <id> <selector> <value>` — type text into an input
- `coctl webview scroll --id <id> [--selector <sel>] [--x 0] [--y 0]` — scroll to position or element
- `coctl webview page-info --id <id>` — get page metadata (title, dimensions, element counts)
- `coctl webview devtools --id <id> [action]` — toggle DevTools inspector (show/close/attach/detach)

### Terminal

- `coctl terminal read [--id <id>] [--start-row N --end-row N ...]` — read visible screen text (or range)
- `coctl terminal state [--id <id>]` — get terminal state (cursor, dimensions, CWD, title)
- `coctl terminal exec [--id <id>] <command>` — execute command (sends text + newline)
- `coctl terminal feed [--id <id>] <text>` — send raw text to terminal (no newline)
- `coctl terminal history [--id <id>] [--lines 100]` — read scrollback history
- `coctl terminal context [--id <id>] [--history-lines 50]` — get combined context (state + screen + scrollback)

### Agent

- `coctl agent approve <message> [--title <title>] [--actions "Approve,Deny"]` — show approval dialog, block until user responds

### Plugin

- `coctl plugin list` — list installed plugins with panels and commands
- `coctl plugin open <plugin> [--panel main]` — open a plugin panel in a new tab
- `coctl plugin run <plugin>.<command> [--params '{}']` — run a plugin shell command

### Todo (Phase 19.1a)

Ergonomic wrapper over the `todo.*` action surface. Every subcommand is sugar over `coctl call todo.<name> --params '...'`; no new IPC. Use `--json` for raw payloads (scriptable), default mode for human-readable rendering.

- `coctl todo create <title> [--body <text>] [--workspace <ws>] [--priority <low|normal|high>] [--due <iso>] [--linked-jira <KEY>] [--tags <a,b,c>]` — wraps `todo.create`. Workspace defaults to `COPAD_TODO_DEFAULT_WORKSPACE` env var, else the plugin's own default.
- `coctl todo list [--status <open|in_progress|done|blocked>] [--workspace <ws>] [--tag <name>] [--due-before <iso>] [--hide-done]` — wraps `todo.list`. Default render: `[icon] <id>  <priority>  <title>  ·  ws=<...> tags=<...>`. Status icons: `[ ]` open, `[~]` in_progress, `[x]` done, `[!]` blocked.
- `coctl todo update <id> [--title <t>] [--body <text>] [--append-subtask <text>] [--priority <p>] [--due <iso>] [--linked-jira <KEY>] [--linked-kb <a,b,c>] [--tags <a,b,c>] [--prompt <text>] [--workspace <ws>]` — wraps `todo.update`. Every flag optional (omitted = unchanged). `--body` and `--append-subtask` are mutually exclusive; the latter is forwarded to the action's `append_subtask` param so the body read-modify-write happens server-side inside the action handler — no client-side preflight that would widen the race window relative to a server-side merge. Total transport is the standard id-resolution `todo.list` preflight + the `todo.update` call (same as every other id-taking subcommand). `--append-subtask` rejects an empty string. Empty-string args for `--due` / `--linked-jira` / `--prompt` clear the field; empty-string for `--linked-kb` / `--tags` clears the array (action's null-or-string convention).
- `coctl todo set <id> --status <s> [--workspace <ws>]` — wraps `todo.set_status`. Status must be `open|in_progress|done|blocked`. `--workspace` scopes id resolution when the same id exists in multiple workspaces.
- `coctl todo done <id> [--workspace <ws>]` / `doing <id> [...]` / `block <id> [...]` — shorthands for `set --status done|in_progress|blocked`.
- `coctl todo start <id> [--workspace <ws>]` — wraps `todo.start` (publishes `todo.start_requested` for the vision-flow-3 chain).
- `coctl todo delete <id> [--workspace <ws>]` — wraps `todo.delete`.
- `coctl todo show <id> [--workspace <ws>]` (Phase 19.2b) — full Todo + linked-entity expansion. Composes `todo.list` (workspace-filtered, then id-pick) + `kb.read` for each `linked_kb` entry. Renders title / status / priority / tags / body / prompt + a 5-line preview per linked KB note (frontmatter stripped). `linked_jira` shows the key verbatim until Phase 16's `jira.get_ticket` lands; `linked_slack` permalinks render as-is. `--json` returns the aggregate as one object (todo payload + per-kb resolution status).
- `coctl todo loop <id> [--workspace <ws>] [--copy]` — wraps `todo.render_loop_prompt`. Renders the autonomous-loop prompt for the given todo (LOOP NAME / WORKSPACE / TODO ID slots pre-substituted from the todo's metadata; the rendered protocol addresses the todo by id+workspace, so a tag is not required for this path). Auto-tags the todo with `loop` so `list --tag loop` finds it later and so the parallel manual-fill template (`~/.claude/loop-template.md`, which looks up by `(workspace, tag=loop, title)`) can resume it without re-tagging. The action returns `loop_tag_added: true` only when this call performed the mutation. Default writes the prompt to stdout. `--copy` pipes the prompt to `wl-copy` (Wayland) or `xclip -selection clipboard` (X11) instead, with stdout fallback when neither tool is available. The corresponding panel surface is the `🔁 loop` button on each non-done todo card; clicking it copies the same prompt via `navigator.clipboard` (with `execCommand("copy")` fallback).

**ID prefix matching**: every `<id>` argument accepts a unique prefix. The CLI preflights `todo.list` to find candidates and resolves the workspace alongside, so a todo in a non-default workspace works without the user passing `--workspace`. Todo ids are workspace-scoped (not globally unique) — if the same id exists in multiple workspaces, the CLI errors out with the candidate list and the user disambiguates via `--workspace <ws>` (or a longer prefix). Exact-id collisions are NOT silently resolved.

### Git (Phase 19.1b)

Ergonomic wrapper over the `git.*` action surface. Every subcommand is sugar over `coctl call git.<name> --params '...'`; no new IPC.

- `coctl git workspaces` — list configured workspaces (`git.list_workspaces`). Default render: `<name>  <branch>  wt=<count>  <path>`.
- `coctl git worktrees [--workspace <ws>]` — list worktrees for a workspace (`git.list_worktrees`). Default render: `<head8>  <branch>  <path> [tags]` where tags include `locked` / `prunable`.
- `coctl git wt add <branch> [--workspace <ws>] [--sanitize-jira]` — create a worktree (`git.worktree_add`). `--sanitize-jira` matches the Phase 15.2 vision-flow-3 trigger contract (lowercase + slash-preserve before branch validation).
- `coctl git wt remove <path> [--force]` — remove a worktree (`git.worktree_remove`). `path` must be under a configured workspace's `path` or `worktree_root`.
- `coctl git branch [--workspace <ws>]` — print the current branch of a workspace's primary checkout (`git.current_branch`).
- `coctl git status [--workspace <ws> | --path <path>]` — working-tree status (`git.status`). Renders `<branch> → <upstream> <ahead>↑<behind>↓  clean/dirty` plus staged/unstaged/untracked counts when dirty.

**Workspace defaulting** (every command except `workspaces`, `wt remove`, `status --path`): explicit `--workspace` flag → `COPAD_GIT_DEFAULT_WORKSPACE` env → cwd-derived (preflights `git.list_workspaces` and matches the longest prefix of the cwd against either the workspace's `path` OR its `worktree_root`, so `cd` into a created worktree under `<repo>-worktrees/<branch>` resolves correctly) → single-config-entry → error with the candidate list. The cwd-derive is the killer ergonomic — `cd` into a worktree, run `coctl git status`, get the right answer.

### Bookmark (Phase 19.3 — BM-1)

Ergonomic wrapper over the `bookmark.*` action surface. Captures URLs into `~/docs/bookmarks/YYYY-MM/<urlhash8>-<slug>.md` (override root with `COPAD_BOOKMARK_ROOT`). Filesystem is the source of truth — no on-disk index, vim/git-edit safe. Every subcommand is sugar over `coctl call bookmark.<name>`.

- `coctl bookmark add <url> [--title <t>] [--tags a,b] [--source <s>]` — wraps `bookmark.add`. Canonicalizes the URL server-side: tracking params stripped (`utm_*`, `gclid`, `fbclid`, `mc_cid`, `mc_eid`, `ref`), fragment dropped, host lowercased. Idempotent — re-adding the same canonical URL returns the existing entry with `existed: true`. ID is the first 8 hex chars of `sha1(canonical_url)`. Status starts at `queued`; BM-2 will add the fetch worker that transitions to `extracted`/`failed`.
- `coctl bookmark list [--status <s>] [--tag <t>] [--since <iso>] [--limit <n>]` — wraps `bookmark.list`. Default render: two lines per entry — `<id>  [<status>]  <title>` then `           <url>  tags=...`. Sorted newest-first by `captured_at`.
- `coctl bookmark show <id|url>` — wraps `bookmark.show`. Accepts a urlhash8 prefix (≥1 hex char) OR a full http(s) URL — the CLI auto-routes URL-shaped inputs as `{url: ...}`. Renders frontmatter + body.
- `coctl bookmark delete <id|url>` — wraps `bookmark.delete`. Same id-or-URL resolution as `show`.

**ID resolution**: `<id>` is a prefix of the 8-char `urlhash8` identifier. Ambiguous prefix errors with the candidate list; pass a longer prefix or the full URL to disambiguate. Bookmarks are NOT workspace-scoped, so `urlhash8` collisions across the whole tree are the only ambiguity surface.

**Out of scope for BM-1** (queued for later phases): async fetch + readability extraction (BM-2), keyword-based linking writing `linked_kb` frontmatter (BM-3), HTML panel (BM-4), harness `/bookmark` slash skill + offline `bookmark drain` for inbox (BM-5), embeddings-based similarity (BM-6, on demand only).

### Theme

- `coctl theme list` — list available themes and current theme

### Update

- `coctl update check` — check for newer version
- `coctl update apply [--version <tag>]` — download and install latest version

## Protocol

Uses cmux V2 newline-delimited JSON over Unix domain socket.

Request:

```json
{ "id": "<uuid>", "method": "background.next", "params": {} }
```

Response:

```json
{"id": "<uuid>", "ok": true, "result": {...}}
```

## Socket Client (`client.rs`)

- Connects to Unix socket
- 15s read timeout, 5s write timeout
- Sends JSON request, reads matching response by ID
- Matches request/response by UUID
