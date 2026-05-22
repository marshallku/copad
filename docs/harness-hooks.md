# Harness hooks ↔ nestty bus

How to wire Claude Code's hook scripts (`~/.claude/hooks/*.sh`,
`~/.claude/scripts/*.sh`) to the nestty event bus so triggers fire on
real harness events (commit blocked, codex review approved, session
stopped, …).

The trust boundary (decisions.md #37) is already enforced — only
triggers that opt in via `[triggers.security] accept_external = true`
receive events published from hook scripts. The pieces below are how
the hook scripts deliver those events.

## The sentinel approach

Hook scripts live in **your** dotfiles, so the patcher cannot guess the
right place to insert a publish call. You mark the call site with a
sentinel comment; the patcher just expands the sentinel into a real
`nestctl event publish` line. The patch is idempotent and reversible.

### Sentinel format

```
# NESTTY_HOOK_PUBLISH: <event-kind> [<json-payload>]
```

- `<event-kind>` is the bus event name (e.g. `claude.commit_blocked`).
  Must match the `[triggers.when] event_kind` of the trigger that
  receives it.
- `<json-payload>` is optional. Omit it for `{}`; provide a literal
  JSON object to ship fields the trigger can match on or interpolate.
  The payload is emitted **verbatim** to the patched line, so shell
  `${VAR}` expansion happens at hook-fire time. See the worked
  examples below.

### What the patcher writes

For each sentinel, `install-claude-hooks.sh` injects:

```
command -v nestctl >/dev/null && nestctl event publish <kind> --quiet "<payload>" &
# NESTTY_HOOK_PUBLISH_END
```

immediately after the sentinel line. Three properties matter:

- `command -v nestctl` short-circuits when nestctl isn't installed —
  the hook never breaks on a fresh machine.
- `--quiet` makes `nestctl` exit 0 on transport failure (nesttyd down,
  socket missing). Schema errors still surface — your sentinel won't
  silently misbehave.
- `&` makes the publish fire-and-forget so the hook continues
  immediately without waiting on the daemon.

## Recommended placements

Match the sentinel to the moment in the script that semantically
corresponds to the event. The patterns below are tied to the trigger
examples shipped in `examples/triggers/claude-hooks.toml`.

### `~/.claude/hooks/pre-commit-gate.sh` → `claude.commit_blocked`

The script is a PreToolUse hook that vetoes commit/push commands when
the session hasn't been cross-reviewed. The deny path ends with a
`jq -n --arg msg "$REASON" '{permissionDecision: "deny", ...}'` line.
Put the sentinel one line **above** that final jq:

```bash
# … existing logic that resolves $REASON …

# NESTTY_HOOK_PUBLISH: claude.commit_blocked {"reason":"$REASON","repo":"$REPO"}
jq -n --arg msg "$REASON" '{permissionDecision: "deny", message: $msg}'
```

After `install-claude-hooks.sh`:

```bash
# NESTTY_HOOK_PUBLISH: claude.commit_blocked {"reason":"$REASON","repo":"$REPO"}
command -v nestctl >/dev/null && nestctl event publish claude.commit_blocked --quiet "{\"reason\":\"$REASON\",\"repo\":\"$REPO\"}" &
# NESTTY_HOOK_PUBLISH_END
jq -n --arg msg "$REASON" '{permissionDecision: "deny", message: $msg}'
```

The `$REASON` / `$REPO` shell vars are expanded by the hook process at
fire time. The trigger receives a payload with concrete strings, ready
for `{event.reason}` interpolation in `notify.show`.

### `~/.claude/scripts/codex-review.sh` → `claude.review_approved`

The script's success path is the `VERDICT: APPROVED` branch that calls
`mark_repo_reviewed`. Put the sentinel right inside that branch:

```bash
case "$VERDICT_LINE" in
    "VERDICT: APPROVED")
        if [[ "$MODE" != "files" ]]; then
            mark_repo_reviewed
        fi
        # NESTTY_HOOK_PUBLISH: claude.review_approved {"session":"$SESSION","mode":"$MODE"}
        exit 0
        ;;
```

The trigger fires on every successful review — a tiny toast confirming
the codex pass landed before the commit gate releases.

### Other hooks (slice 2 candidates)

Trigger examples for these are not yet shipped, but the same sentinel
shape works:

| Hook script | Event kind | When |
|---|---|---|
| `~/.claude/hooks/track-edit.sh` | `claude.tool_used` | PostToolUse on every Edit/Write |
| `~/.claude/hooks/auto-handoff.sh` | `claude.session_stopped` | Stop hook, session wrap-up |
| `~/.claude/hooks/auto-cross-review.sh` | `claude.review_required` | Stop with dirty changes |
| `~/.claude/hooks/session-start.sh` | `claude.session_started` | SessionStart |

When a corresponding `examples/triggers/claude-*` trigger lands in
slice 2, the recommended placement docs here will expand.

## Running the patcher

```bash
# Default — scan ~/.claude/hooks and ~/.claude/scripts.
bash ~/dev/nestty/scripts/install-claude-hooks.sh

# Dry-run — print diffs without writing.
bash ~/dev/nestty/scripts/install-claude-hooks.sh --dry-run

# Remove all injected publish blocks (leaves sentinels in place for
# easy re-install).
bash ~/dev/nestty/scripts/install-claude-hooks.sh --uninstall

# Use a non-default hook layout.
bash ~/dev/nestty/scripts/install-claude-hooks.sh --hooks-dir ~/dotfiles/claude/hooks
```

The patcher's self-test (`--self-test`) runs in a tmpdir and never
touches `~/.claude`. Useful for CI or a smoke check after editing the
script.

## Manual patch path

If you don't want the patcher, the literal lines below are what gets
written. Copy them into your hook by hand:

```bash
# Just below the place the event fires, paste:
command -v nestctl >/dev/null && nestctl event publish \
    claude.commit_blocked --quiet \
    "{\"reason\":\"$REASON\",\"repo\":\"$REPO\"}" &
```

You don't need the sentinel comments at all if you're maintaining the
patch by hand — they only exist so the patcher can find and update the
line.

## Payload safety — when `$VAR` values may contain JSON-special chars

The patcher escapes the **literal** `"` and `\` characters from your
sentinel payload so the emitted bash line parses correctly. It cannot
escape the *runtime* expansion of `${VAR}` — if `$REASON` itself
contains a `"`, a `\`, a newline, or any other JSON-special character,
the resulting JSON is broken:

```bash
# Sentinel:
# NESTTY_HOOK_PUBLISH: claude.commit_blocked {"reason":"$REASON"}
# Patched line:
... --quiet "{\"reason\":\"$REASON\"}" &
# At fire time with REASON='bad "value':
... --quiet "{"reason":"bad "value"}" &   # broken JSON
# At fire time with REASON='C:\tmp':
... --quiet "{"reason":"C:\tmp"}" &       # `\t` becomes a tab char
```

The same hazard hits any runtime value that contains:

- `"` — breaks the JSON string boundary.
- `\` — bash treats `\X` inside `"..."` as an escape sequence
  (`\t`, `\n`, `\\` collapses, etc.), changing the byte stream.
- newlines / control characters — produce literal control bytes
  rather than JSON `\n` / `\t` escapes.

Enum-like values, integer ids, and ASCII-only identifiers (`$SESSION`,
`$VERDICT`, etc.) are safe to embed directly because they contain none
of the above. For anything that captures user-controlled or
filesystem-derived strings (Slack message text, Discord nicknames,
filesystem paths, multi-line outputs), construct the payload via a
JSON-safe builder:

**Option A — `jq` (recommended when available):**

```bash
# Sentinel:
# NESTTY_HOOK_PUBLISH: claude.commit_blocked $(jq -n --arg r "$REASON" --arg p "$REPO" '{reason:$r,repo:$p}')
# Patched line:
... --quiet "$(jq -n --arg r "$REASON" --arg p "$REPO" '{reason:$r,repo:$p}')" &
```

`jq -n --arg` quotes the value into a JSON string and escapes anything
inside. The `$(...)` substitution is preserved verbatim by the
patcher.

**Option B — a small bash helper (zero deps):**

Add this once at the top of your hook script:

```bash
# JSON-encode "k=v" pairs into a single object, escaping `"`/`\`/CR/LF.
nestty_json() {
    local out='{' first=1 kv k v esc
    while [[ $# -gt 0 ]]; do
        kv="$1"; shift
        k="${kv%%=*}"; v="${kv#*=}"
        esc="${v//\\/\\\\}"; esc="${esc//\"/\\\"}"
        esc="${esc//$'\n'/\\n}"; esc="${esc//$'\r'/\\r}"
        [[ $first -eq 1 ]] && first=0 || out+=,
        out+="\"$k\":\"$esc\""
    done
    out+='}'
    printf '%s' "$out"
}
```

Then:

```bash
# Sentinel:
# NESTTY_HOOK_PUBLISH: claude.commit_blocked $(nestty_json "reason=$REASON" "repo=$REPO")
```

Both keep the patch idempotent (the `$(...)` substitution is captured
by the patcher verbatim).

- The `jq` path is fully safe — every JSON-special and control char
  is escaped by `jq -n --arg`.
- The zero-deps `nestty_json` helper covers the common cases (`"`,
  `\`, LF, CR). Other control bytes (literal tab, NUL, etc.) end up
  in the JSON unescaped, which is a spec violation. Hooks that may
  capture raw control bytes (e.g. ANSI-stripped subprocess output)
  should prefer `jq` or extend `nestty_json` with the `printf
  '\\u%04x'` pattern for `$'\x00'..$'\x1f'`.

## Verifying the flow end-to-end

After patching, you can simulate without waiting for a real hook fire:

```bash
# 1. Confirm the trigger is loaded.
nestctl recent --since 1m --kind 'system.*'

# 2. Publish the event yourself.
nestctl event publish claude.commit_blocked --quiet \
    '{"reason":"manual test","repo":"demo"}'

# 3. Watch for the toast (notify-send / osascript fires it).
# Also check daemon logs for the trigger fire line:
journalctl --user -u nesttyd --since "10s ago" 2>/dev/null \
    || tail -n 20 /tmp/nesttyd*.log
# Expected: trigger "claude-commit-blocked-toast" fired action "notify.show"
```

If the toast doesn't appear:

- **No log line** → trigger not matching. Verify `[triggers.when]
  event_kind` matches the kind you published, and `[triggers.security]
  accept_external = true` is present.
- **Log line present but no toast** → `notify.show` subprocess
  failure. Run the same call directly: `nestctl call notify.show
  --params '{"title":"t","body":"b"}'` and check daemon logs for
  `notify subprocess` errors. The usual cause on a wlroots-based
  Wayland session is dunst crashing because `WAYLAND_DISPLAY` never
  reached the D-Bus activation env — see § "Graphical-session
  prerequisites" below for the two-line compositor fix.
- **`nestctl: command not found`** → install nestty
  (`./scripts/install-dev.sh`) or check PATH.

## SSH + daemon lifecycle

For the hook → toast loop to work in a fresh shell (especially an SSH
session), `nesttyd` must already be running. The systemd `--user` unit
shipped at `dist/systemd/nestty-daemon.service` (installed by
`install-dev.sh` by default) wires this up. macOS users get the
matching LaunchAgent plist at `dist/launchd/com.marshall.nestty.daemon.plist`
through `install-macos.sh`.

### Linux: linger choice

systemd starts the user-instance on the first login and stops it on
the last logout — by default. That means:

- **linger OFF (default)**: daemon comes up on first SSH (or graphical
  login) and dies when every login session exits. A fresh SSH after
  full logout starts a new daemon. Fine for most setups.
- **linger ON**: daemon starts at boot, survives logout, stays up
  across SSH disconnects. Required if you want hook events from a
  cron / background job that never had an interactive login.

Enable linger if you want boot-time start:

```bash
sudo loginctl enable-linger "$USER"
```

Verify with `loginctl show-user $USER --property=Linger` (`Linger=yes`).

`install-dev.sh` prints a reminder when linger is off so you don't
get blindsided by "daemon disappears after SSH disconnect".

### Cross-machine: SSH `RemoteForward`

When Claude Code runs on a remote box (and the hooks fire there), the
hook scripts need a daemon socket. Forward the workstation's socket
via SSH:

```sshconfig
# ~/.ssh/config
Host my-remote
    RemoteForward /run/user/1000/nestty/socket /run/user/1000/nestty/socket
```

`/run/user/1000` resolves to `$XDG_RUNTIME_DIR` for UID 1000; adjust if
your UID differs (`id -u`). After connecting:

- Hooks on `my-remote` see a Unix socket at the same path locally.
- Events publish over that socket to the workstation's daemon.
- Trust boundary tags them `External` origin (same as local hook
  events — the SSH transport is irrelevant to the trust model).
- Triggers with `accept_external = true` fire on the workstation;
  the toast appears on the local desktop.

If the forward fails (path occupied, permission denied), `nestctl`
just falls back to "no daemon" and `--quiet` exits 0 silently. Hooks
don't break.

### Troubleshooting

- **`systemctl --user status nestty-daemon` → "not loaded"** →
  `install-dev.sh` wasn't run with `--with-daemon` (or it ran on a
  system without systemd). Re-run, or manually copy
  `dist/systemd/nestty-daemon.service` to `~/.config/systemd/user/`.

- **"daemon dies on logout"** → linger off. Enable per above.

- **SSH login does NOT auto-start the daemon** → linger on (the user
  instance is already running, no first-login event to trigger the
  unit). Restart manually: `systemctl --user restart nestty-daemon`.
  OR: linger off but unit not enabled — `systemctl --user enable --now
  nestty-daemon`.

- **`nestctl event publish ...` from a remote SSH session times out**
  → `RemoteForward` not set in `~/.ssh/config`, or
  `$XDG_RUNTIME_DIR/nestty/socket` doesn't exist on the workstation.

## Graphical-session prerequisites (Linux / Wayland)

`notify.show` ultimately calls `notify-send` → libnotify → D-Bus
`org.freedesktop.Notifications`. When no notification daemon is
running, D-Bus auto-activates one (typically dunst). That
auto-activated daemon inherits its environment from the **D-Bus
activation env**, NOT from the compositor — so if the compositor
hasn't pushed `WAYLAND_DISPLAY` into the bus, dunst falls back to X11,
fails to open a display, and crashes. Symptoms:

```
nesttyd: notify.show failed: notifier exited exit status: 1:
  Failed to show notification:
  GDBus.Error:org.freedesktop.DBus.Error.NameHasNoOwner:
  Could not activate remote peer 'org.freedesktop.Notifications':
  startup job failed
```

The fix is two lines in your compositor autostart that propagate
`WAYLAND_DISPLAY` into both systemd `--user` (for `PartOf=
graphical-session.target` units) and D-Bus activation (for
dbus-activated services like dunst):

```hyprlang
# ~/.config/hypr/hyprland.conf — before any GUI autostart
exec-once = systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP HYPRLAND_INSTANCE_SIGNATURE
exec-once = dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP HYPRLAND_INSTANCE_SIGNATURE
```

Sway / river / Niri have the same need; replace
`HYPRLAND_INSTANCE_SIGNATURE` with the compositor's equivalent (or
drop it — only `WAYLAND_DISPLAY` and `XDG_CURRENT_DESKTOP` matter for
dunst). KDE Plasma and GNOME do this automatically; bare wlroots
compositors do not.

Verify after relogin:

```bash
systemctl --user show-environment | grep WAYLAND_DISPLAY    # must show wayland-N
dbus-send --session --print-reply --dest=org.freedesktop.DBus \
    /org/freedesktop/DBus org.freedesktop.DBus.GetConnectionUnixProcessID \
    string:org.freedesktop.Notifications                    # must return a PID
nestctl event publish notify.show '{"title":"t","body":"b"}' --quiet
# Toast should appear immediately.
```

The same root cause hits anything else dbus-activated from a fresh
nesttyd context (xdg-desktop-portal, gnome-keyring helpers). Wiring
the two `exec-once` lines once fixes them all.

## Trust boundary recap

Events published from these hooks are tagged `Origin::External` by the
daemon's `events.publish` handler. The fan-out filter in
`TriggerEngine::dispatch` drops External events for triggers without
`accept_external = true`. The privileged-action gate
(`allow_privileged`) is a separate opt-in for triggers that fire
`system.spawn`. See `docs/harness-integration.md` § Trust boundary and
`docs/decisions.md` #37 for the full design.

In practice: if you copy the trigger examples from
`examples/triggers/claude-hooks.toml`, both security flags are set
correctly. If you author your own trigger, remember the two-step
opt-in for the dangerous hook-event-fires-spawn combo.
