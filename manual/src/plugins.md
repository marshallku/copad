# Using Plugins

copad's plugin system lets HTML/JS panels and background services extend the terminal — a knowledge base, a git panel, calendar/Slack/Discord/Jira integrations, a pomodoro timer, and more. This page covers **installing and using** first-party plugins. (Writing your own is covered in the repo's `docs/plugins.md`.)

## First-party plugins

| Plugin | What it does |
| --- | --- |
| `kb` | Grep + filename search and atomic read/append over `~/docs` (knowledge base). |
| `docs` | A KB panel — browse `~/docs` with backlinks, a tag tree, related notes, and recent edits. |
| `git` | Worktree create/remove + branch and status queries. |
| `todo` | Markdown-checkbox todos in `~/docs/todos/<workspace>/`. |
| `calendar` | Google Calendar event polling with lead-time reminders. |
| `slack` | Slack Socket Mode — mention/DM/reaction events + post messages. |
| `discord` | Discord gateway — mention/DM/reaction events + send messages. |
| `jira` | Jira Cloud polling + read/write actions. |
| `llm` | Anthropic Messages API client with a usage log. |
| `claude` | Read-only views over `~/.claude` harness artifacts. |
| `pilot` | An autonomous goal queue driving detached Claude sessions. |
| `pomodoro` | A focus timer in the status bar with per-transition toasts. |
| `web-bridge` | An HTTP+WS cockpit (panes, attention, pilot queue) for a browser or phone. |
| `bookmark` | Bookmark capture with a dedupe store. |
| `echo` | A reference / end-to-end test plugin (ping). |

> Some plugins need credentials to be useful (`slack` bot token, `discord` bot token, `jira`, `calendar` Google OAuth). Without them they return RPC errors gracefully rather than crashing — check status with `coctl <plugin> auth-status` where available.

## Where plugins are installed

| Platform | Path |
| --- | --- |
| Linux | `~/.config/copad/plugins/<name>/` |
| macOS | `~/Library/Application Support/copad/plugins/<name>/` |

Each plugin directory holds a `plugin.toml` manifest plus any panel assets (`panel.html`, CSS, JS). For plugins with a background service, the built binary is symlinked in alongside the manifest.

## Installing plugins

The full copad installer bundles plugins on macOS. On Linux (and for source builds), install them with `scripts/install-plugins.sh` after a release build:

```bash
cargo build --release --workspace          # build the plugin binaries first
./scripts/install-plugins.sh               # install every first-party plugin
./scripts/install-plugins.sh todo git      # or just these two
```

The script copies each plugin's manifest and assets, symlinks its binary, and (for service plugins) warns if a binary hasn't been built yet.

> **Restart copad after installing.** Plugin discovery (`discover_plugins()`) only runs at startup. If you see an error like `service X is not running and X.action cannot trigger its activation (OnStartup)`, you're running an old copad that hasn't picked up the newly installed plugin — restart it.

## Using plugins

From the CLI:

```bash
coctl plugin list                                   # list installed plugins, panels, commands
coctl plugin open kb --panel search                 # open a plugin panel in a new tab
coctl plugin open kb                                 # open its default panel ("main")
coctl plugin run my-plugin.greet --params '{"name":"world"}'   # run a command
coctl call echo.ping --params '{"hi":"there"}'      # verify a plugin is alive
```

Several plugins also have dedicated `coctl` wrappers with friendlier ergonomics — `coctl todo`, `coctl git`, `coctl bookmark`, `coctl pomodoro`, `coctl jira`, `coctl slack`, `coctl calendar`. See the [Command Reference](./coctl/reference.md#plugin).

You can bind any of these to a copad keyboard shortcut:

```toml
# ~/.config/copad/config.toml
[keybindings]
"ctrl+shift+g" = "spawn:coctl plugin open git"
"ctrl+shift+k" = "spawn:coctl plugin open docs"
```
