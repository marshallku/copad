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
