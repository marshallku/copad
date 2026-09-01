# Environment Variables

comux reads a handful of `COPAD_MUX_*` environment variables. Most duplicate a `mux.toml` setting so you can override behavior for a single launch without editing config.

| Variable | Effect |
| --- | --- |
| `COPAD_MUX_SOCK` | Override the control/attach socket path. Default: `<runtime_dir>/sock`, where `runtime_dir` is `$XDG_RUNTIME_DIR/copad-mux-$USER` (else `$TMPDIR/…`, else `/tmp/copad-mux-$USER`). Injected into panes. |
| `COPAD_MUX_STATE` | Override the session state file. Default: `$XDG_STATE_HOME/copad/mux-session.json`, else `~/.local/state/copad/mux-session.json`. |
| `COPAD_MUX_NOTIFY` | Force desktop toasts on/off — **wins over `notify` in config**. Off: `0`/`off`/`false`/`no`. On: `1`/`on`/`true`/`yes`. |
| `COPAD_MUX_USAGE` | `COPAD_MUX_USAGE=0` disables the status-bar usage poller entirely (same as `usage = off`). |
| `COPAD_MUX_UPDATE_CHECK` | `COPAD_MUX_UPDATE_CHECK=0` disables the background GitHub-release update check (same as `update_check = false`). |
| `COPAD_MUX_REDRAW_MS` | Set to a millisecond value to re-enable the periodic self-healing full repaint. **Default off** (the periodic repaint flashes a blank frame each tick, so it's kept as an escape hatch for outer-emulator drift only). |
| `COPAD_MUX_QUIET_SSH` | `COPAD_MUX_QUIET_SSH=1` silences the "you're on SSH" advisory printed when `SSH_CONNECTION` is set. |
| `COPAD_MUX` | Set to `1` by the server inside every pane. Marks a shell as already inside a comux pane (so `worktree create` from within comux doesn't spawn a nested client). Load-bearing — do not set or add it to `update_environment`. |

Related non-`COPAD_MUX_*` variables comux consults: `XDG_RUNTIME_DIR` / `TMPDIR` / `USER` (runtime dir), `XDG_CONFIG_HOME` / `HOME` (config), `XDG_STATE_HOME` / `HOME` (state), `SHELL` (pane shell), and `SSH_CONNECTION` (SSH advisory).

> Note: `COPAD_UPDATE_CHECK` (no `_MUX_`) is a **separate** variable used by the copad GUI/daemon, not by comux.

---

## Environment refresh

This is the subtle one, and it matters if you ever start a comux server over SSH.

The persistent server **freezes the environment it was born in**, and every pane inherits that snapshot. So a server first started over SSH would leave every pane carrying `SSH_CONNECTION` / `SSH_TTY` forever — which makes `claude` inside a pane think it's remote, breaking Claude-in-Chrome and other local-only features even after you attach locally.

To fix this, comux **scrubs the volatile session variables** (the `update_environment` list — `SSH_*`, `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`, …) from the daemon at startup, and **re-injects them into each new pane from the client that spawns it**. This happens per-client (a `Hello` → `Env` handshake), so multiple clients share correctly. The first/boot pane keeps the server's birth values, so there's no regression there.

- Customize the scrub list via `update_environment` in `mux.toml`. It's read only at boot — changing it needs `comux server restart`.
- Load-bearing names (`PATH`, `HOME`, `SHELL`, …) are refused.
- **Local-only privileges** (polkit `shutdown`, macOS GUI-app access) follow the server's kernel session and can only be fixed by birthing the server locally — so comux warns when you spawn a server from an SSH session. Silence that warning with `COPAD_MUX_QUIET_SSH=1`.

---

## Never inherited: agent session markers

Environment refresh solves one half of the frozen-environment problem. The other half needs the opposite rule — some variables must be scrubbed and **never put back**.

`CLAUDE_CODE_CHILD_SESSION=1` marks a process that Claude Code itself launched. An interactive `claude` that carries it is classified as a nested child session: its transcript is never written to `~/.claude/projects/…`, and it never appears in `claude --resume`. Start a comux server from inside a Claude Code session and — before this was fixed — the marker froze into the daemon and reached every pane, so every conversation held in comux was silently discarded. Other artifacts (`tool-results/`, `file-history/`) still got written, which is what made it hard to notice until you tried to resume.

`update_environment` cannot fix this: it re-injects from the attaching client, so attaching from inside Claude Code would just hand the marker back. So these names live in a separate list, `never_inherit`:

- scrubbed from the daemon at startup, on **every** launch path (auto-spawned or a hand-run `comux server`);
- **not** carried into the boot pane either — unlike `update_environment`, no birth value is kept;
- never accepted from an attaching client (they're kept out of the refresh whitelist, so a client is never even asked for them).

Built-in list — Claude Code's own session-scoped variables:

```
CLAUDE_CODE_CHILD_SESSION   CLAUDECODE   CLAUDE_CODE_SESSION_ID
CLAUDE_CODE_BRIDGE_SESSION_ID   CLAUDE_CODE_ENTRYPOINT
```

Your own `never_inherit` in `mux.toml` is **added** to that list rather than replacing it, so adding a marker of your own can't quietly re-enable the leak:

```toml
never_inherit = ["MY_AGENT_SESSION_ID"]
```

Sandbox markers (`CODEX_SANDBOX*`) are deliberately **not** scrubbed — a process running inside a sandbox has to be able to see that it is. Load-bearing names (`PATH`, `HOME`, `SHELL`, …) are refused, as with `update_environment`. Like `update_environment`, the list is read at boot: changing it needs `comux server restart`. A pane that genuinely wants one of these values can export it from its shell rc.

> **Already affected?** Any server started before this fix still holds the marker. `comux server restart` clears it (the scrub runs on the restarted server regardless of the shell you restart from — you don't need a "clean" terminal). On an unfixed version, restart from a shell outside Claude Code, or `unset CLAUDE_CODE_CHILD_SESSION` in the pane before launching `claude`. Conversations already lost were never written to disk and cannot be recovered.
