# coctl — Overview

**`coctl`** is the control CLI. It drives a running `copad` instance — every pane, tab, panel, webview, and plugin — over a Unix socket, and it also runs a few **local** readouts (usage, agent status) that don't need a running GUI at all and work fine over SSH.

## Synopsis

```
coctl [--socket <path>] [--json] <command> [subcommand] [args] [flags]
```

| Global flag | Effect |
| --- | --- |
| `--socket <path>` | Target a specific instance socket. Must appear **before** the subcommand. |
| `--json` | Emit JSON instead of human-readable output. May appear anywhere. |
| `--version` / `-V`, `--help` / `-h` | Standard. |

## Getting started

```bash
coctl ping                 # is an instance alive?
coctl tab new              # open a tab
coctl split vertical       # split the focused pane
coctl theme list           # list themes + the current one
coctl usage --oneline      # local token/cost readout (no GUI needed)
```

The full, categorized list of commands is in the [Command Reference](./reference.md).

## How coctl finds the instance

You rarely set `--socket` — copad injects its socket path into child processes, so a `coctl` you run inside a copad terminal already targets that instance. The resolution order is:

1. `--socket <path>` if given.
2. `$COPAD_SOCKET` — if it's currently connectable.
3. **Auto-discovery**, newest-first within each tier:
   - Hardened GUI sockets: `<runtime_dir>/gui-{PID}.sock` (Linux: `$XDG_RUNTIME_DIR/copad/gui-{PID}.sock`), only when the runtime dir is owner-only.
   - Legacy GUI sockets: `/tmp/copad-{PID}.sock` (macOS and pre-hardening Linux).
   - The daemon well-known path: `<runtime_dir>/socket`.
4. Fallback: `/tmp/copad.sock`.

The transport is newline-delimited JSON (request `{id, method, params}` → response `{id, ok, result}`), matched by UUID with a 15s read / 5s write timeout.

> **Debug tip.** Set `COPAD_DEBUG_SOCKET=1` to print `[coctl] using socket: <path>` to stderr, so you can see which instance a command hit.

## Commands that don't need the GUI

A few commands are dispatched **locally** and work with no running copad (and over SSH):

- `coctl usage …` — Claude/Codex token, cost, and rate-limit readouts.
- `coctl agent status …` — running agents and their state (shells out to `tmx`).
- `coctl background cache …` — build a wallpaper list file.
- `coctl update check` / `coctl update apply` — version management.

And `coctl event publish …` talks straight to the **daemon** (not a GUI socket), because event publishing is daemon-only.

## Environment variables

| Variable | Effect |
| --- | --- |
| `COPAD_SOCKET` | Preferred socket path (used only if connectable). Injected into copad's children. |
| `COPAD_DEBUG_SOCKET` | If set, print the chosen socket path to stderr. |
| `COPAD_TODO_DEFAULT_WORKSPACE` | Default workspace for `coctl todo …`. |
| `COPAD_GIT_DEFAULT_WORKSPACE` | Default workspace for `coctl git …`. |
| `COPAD_BOOKMARK_ROOT` | Override the bookmark capture root (default `~/docs/bookmarks/`). |
