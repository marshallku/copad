# Troubleshooting

## Build Issues

### Missing vte4 system library

```
error: could not find system library 'vte-2.91-gtk4'
```

**Fix:** `sudo pacman -S vte4`

### Missing gtk4 system library

**Fix:** `sudo pacman -S gtk4`

### `load_from_string` not found on CssProvider

The method is gated behind a feature flag.
**Fix:** Add `features = ["gnome_46"]` to gtk4 dependency in Cargo.toml.

### Cargo binary name collision

```
warning: output filename collision at target/debug/copad
```

copad-linux and copad-cli both output `copad`.
**Fix:** CLI binary renamed to `coctl` in copad-cli/Cargo.toml.

## Runtime Issues

### Wayland protocol error (Error 71)

```
Gdk-Message: Error 71 (Protocol error)
```

**Fix:** Set `GDK_BACKEND=x11` in environment or in main.rs.

### GBM buffer error

```
Failed to create GBM buffer of size 841x1352: Invalid argument
```

**Fix:** Set `WEBKIT_DISABLE_DMABUF_RENDERER=1` (only relevant if using WebKit components).

### Terminal shows in light mode

**Cause:** Transparent VTE background with no image loaded shows the system theme underneath.
**Fix:**

1. Force dark theme: `settings.set_gtk_application_prefer_dark_theme(true)` in `app.rs`
2. Window CSS `window { background-color: <theme.background> }` provides the solid fallback color now that VTE is permanently transparent (no more conditional opaque bg).

### Background images not showing (solid color only)

Multiple possible causes:

1. **Config `directory` is commented out**: Check `~/.config/copad/config.toml`. The `directory` field must be uncommented. A `#` before the key comments it out.

2. **Surface is opaque**: the window-level `BackgroundLayer` paints behind everything, so any opaque widget above it hides the image. Required transparent surfaces: VTE (`set_clear_background(false)` + `RGBA(0,0,0,0)`), WebKit (`webview.set_background_color(RGBA(0,0,0,0))`), notebook header / statusbar / `html, body` in plugin CSS — all transparent. If you add a new chrome widget and the image disappears under it, that widget needs the same treatment.

3. **Image loading fails silently**: The original `GtkPicture::set_file()` loads asynchronously and fails silently. Fixed by using `gdk::Texture::from_file()` for synchronous loading with error reporting.

4. **Tint too opaque**: Tint at 0.9 makes images nearly invisible (90% opaque dark overlay). Lower to 0.85 or less.

5. **GTK single-instance**: If an old copad is running, new launches activate the old instance and exit immediately (exit code 0, no output). Kill all instances first: `killall copad`.

### App exits immediately with no error

**Cause:** GTK single-instance behavior. Another copad instance already owns the GTK app ID `com.marshall.copad`.
**Fix:** `killall copad` then relaunch.

### log:: messages not visible

**Cause (resolved Step 5a):** `copad-linux` used to skip `env_logger::init()`, so `log::info!` / `log::warn!` were silent. We now initialize `env_logger` with `default_filter_or("warn")` in `main()`. Set `RUST_LOG=info` to surface gui_client register/reconnect, or `RUST_LOG=debug` for the full reconnect cadence. GTK does not capture stderr on console launches, so the diagnostics appear when running `copad` from a terminal. Desktop-entry launches may still hide stderr depending on the session — use `journalctl --user -f` if needed.

### Terminal shows only one line (collapsed height)

**Cause:** `GtkOverlay` sizes based on its child widget. The window-level root overlay's child is the (hideable) `bg_picture` from `BackgroundLayer`, so when no image is set the base child has zero natural size and the overlay collapses unless an overlay is marked as size-driver.
**Fix:** Call `root_overlay.set_measure_overlay(&layout, true)` so the actual UI layout drives the overlay's measurement regardless of bg image state. The `TerminalPanel`'s own (search-bar) overlay is already measured by its terminal child.

### WebKit web process crashes on many sites

```
GStreamer element autoaudiosink not found. Please install it
GLib-GObject-CRITICAL: invalid (NULL) pointer instance
WebProcess CRASHED
```

**Cause:** Missing GStreamer plugins. WebKitGTK requires GStreamer for media handling, and crashes when the plugins are absent — even on pages that don't play media.
**Fix:** `sudo pacman -S gst-plugins-good gst-plugins-bad`

### D-Bus: `register_object` API mismatch

**Cause:** gio 0.20 uses builder pattern, not positional args.
**Fix:** Use `connection.register_object(path, &interface_info).method_call(closure).build()`.

### Plugin/webview panel frozen on last frame after Hyprland workspace switch — known upstream limitation

**Symptom:** Plugin panel (or any `webkit6::WebView`) renders fine on first show. User switches to a different Hyprland workspace, then comes back. Panel is stuck on the last frame — appears alive (backend healthy, WebProcess alive, IPC responsive) but doesn't repaint. Right-click → "Inspect Element" revives instantly. Focusing another window and coming back also revives it.

**Status: known upstream limitation in WebKitGTK 6.0 ↔ Hyprland interaction. Not fixable in copad-side code.**

**Reproduction outside copad:** Spawn the official WebKitGTK reference browser:
```
/usr/lib/webkitgtk-6.0/MiniBrowser https://www.google.com
```
on Hyprland and switch workspaces. Same freeze. This is zero copad code, so the bug is upstream.

**What was ruled out empirically (rounds 1–5, all reverted):**
- Round 1 — `webview.connect_map(|wv| wv.evaluate_javascript("0"))`: signal never fires; Hyprland uses scene-graph hide without `wl_surface.unmap`.
- Round 2 — toplevel `is-active` notify + `evaluate_javascript("0")`: hook fires correctly per stderr capture; nudge insufficient.
- Round 3 — `is-active` + `GdkToplevel:state` notify with `queue_draw()` on both: hooks fire, queue_draw runs (verified via stderr); panel still freezes.
- Round 4 — same hooks + `set_visible(false); set_visible(true)` on `GDK_TOPLEVEL_STATE_SUSPENDED` rising-edge clear: hook fires, toggle runs; freeze persists.
- Round 5 — same hooks + full `webview.reload()` on suspended-clear: freeze persists, AND reload destroys panel state on every workspace return (bad UX, net negative).
- Environment variables that did NOT help: `WEBKIT_DISABLE_DMABUF_RENDERER=1`, `GSK_RENDERER=cairo`, `WEBKIT_DISABLE_COMPOSITING_MODE=1`, `__EGL_VENDOR_LIBRARY_FILENAMES=…/50_mesa.json` (forcing Mesa EGL on NVIDIA).
- Hardware: reproduces on NVIDIA RTX 3060 Ti (driver 595.71.05) AND on a separate integrated-graphics laptop. Not GPU-vendor-specific.
- Compositor versions: Hyprland 0.54.3 (no longer wlroots-based) + WebKitGTK 2.52.3.

**Why no application-level fix worked:** The freeze is in WebKit's compositor frame-production path after the wl_surface gets the SUSPENDED bit and then has it cleared. The bit DOES toggle on Hyprland (verified via `connect_state_notify` logs), but WebKit's render scheduler doesn't resume pushing frames on bit-clear unless an actual input event (pointer, dev-tools attach via JS pump from inspector init) drives it. There is no public WebKitGTK 6.0 API to tell the WebProcess "visibility changed, resume rendering."

**User-facing workaround:** Click anywhere in the panel after coming back from a workspace, OR focus another window then refocus copad, OR right-click → Inspect Element. All three paths cause WebKit's compositor to resume.

**Automated cure on Hyprland — `window.restored` + `system.spawn` trigger (Phase WR-1/WR-2):**

If you're on Hyprland specifically, two separate `hyprctl dispatch resizewindowpixel` calls (a 1px nudge) empirically unfreeze the panel — the underlying mechanism by which this works where `--batch` doesn't is not fully characterized; the behavior reproduces reliably across cycles. On the dual-monitor setup we tested, only same-monitor workspace cycles trigger the freeze; cross-monitor switches did not. copad exposes the building blocks:

- `window.restored` event fires when the toplevel's `GDK_TOPLEVEL_STATE_SUSPENDED` bit clears — i.e. you're returning to the workspace copad lives on.
- `system.spawn` is a trigger-only action (NOT reachable from `coctl call`, by design) that exec's an argv vector fire-and-forget.

Drop this into `~/.config/copad/config.toml`:

```toml
[[triggers]]
name = "hyprland-webkit-cure"
action = "system.spawn"
params = { argv = ["sh", "-c", "hyprctl dispatch resizewindowpixel '1 0,class:com.marshall.copad' && hyprctl dispatch -- resizewindowpixel '-1 0,class:com.marshall.copad'"] }

[triggers.when]
event_kind = "window.restored"

# `system.spawn` is a privileged action and triggers must
# explicitly opt in. See `docs/harness-integration.md` § Trust
# boundary.
[triggers.security]
allow_privileged = true
```

**Two empirical decisions baked into that snippet** (observed behaviors on Hyprland 0.54.3 — the underlying mechanism for the second one is not fully characterized, but the behavior reproduced reliably across dozens of cycles):

- `resizewindowpixel` with `class:com.marshall.copad` selector, NOT `resizeactive`. The trigger fires on workspace return regardless of which window has focus on that workspace — the user often returns with focus on whatever they were last using on that workspace, not copad. `resizeactive` would resize that other window and the freeze stays put. `resizewindowpixel,class:` is focus-agnostic.
- Two separate `hyprctl dispatch` calls chained with `&&`, NOT `hyprctl --batch "...; ..."`. `--batch` consistently fails to cure; two separate IPC calls consistently do.

The second `hyprctl` invocation uses `hyprctl dispatch -- resizewindowpixel '-1 0,class:com.marshall.copad'` because `-1 0,...` begins with `-` and hyprctl's CLI parser would otherwise treat `-1` as a flag — the `--` is what forces end-of-options.

**Why `sh -c` here is safe — and when it would NOT be:** `system.spawn` doesn't auto-wrap argv in a shell, so by default `{event.*}` and `{context.*}` interpolations land as literal argv elements where shell metacharacters can't be re-parsed. That default safety is what protects the bare-argv form. Once the user EXPLICITLY chooses `["sh", "-c", "<string>"]`, every interpolated value spliced into that string IS shell-evaluated, so the bare-argv guarantee no longer applies — every interpolation source must be audited individually. The snippet above is safe only because it satisfies BOTH (a) the trigger doesn't interpolate any `{event.X}` or `{context.X}` value into the shell string (every argv element is a literal) AND (b) `window.restored` itself emits an empty `{}` payload, so even a typo'd `{event.X}` would resolve to a literal token rather than attacker-controlled data. Do NOT copy this `sh -c` pattern to triggers that interpolate ANY field (event payload OR context fields like `{context.active_cwd}`) into the shell string — a trigger on e.g. `slack.mention` carrying a user-controlled `text` field, or even one referencing a directory path the user happens to have, would let a Slack message or a malicious dir name run arbitrary code. Use the bare argv form (`argv = ["program", "arg1", ...]`) whenever the trigger interpolates anything.

A ready-to-copy snippet lives at [`examples/triggers/hyprland-webkit-fix.toml`](../examples/triggers/hyprland-webkit-fix.toml).

This is a workaround that papers over the upstream bug — if you're not on Hyprland, the trigger no-ops (other compositors don't toggle SUSPENDED on workspace switch the same way), and there's no copad-side state to roll back when WebKit/Hyprland publish a real fix.

**Possible future paths (not pursued):**
- File upstream issue at `bugs.webkit.org` and `github.com/hyprwm/Hyprland` with the MiniBrowser reproducer.
- Wait for an upstream fix in WebKitGTK or Hyprland.
- Replace the panel rendering layer (move away from WebKit) — large scope.

**Distinct from cold-boot blank panel** (different mechanism — see commit `bb9c1f1` prewarm).

The diagnostic signal hooks (`load_changed` / `load_failed` / `web_process_terminated`) added in commit `78ebdb1` remain in `plugin_panel.rs` because they are general-purpose, not specific to this freeze.

---

### `notify.show` toast silently fails under systemd-managed copadd (Wayland)

**Symptom:** `coctl event publish claude.review_approved` returns
`{"queued": true}`, daemon log shows the trigger firing
`notify.show`, but no toast appears. Journal contains:

```
GDBus.Error:org.freedesktop.DBus.Error.NameHasNoOwner:
  Could not activate remote peer 'org.freedesktop.Notifications':
  startup job failed

dunst[*]: WARNING: Cannot open X11 display.
dunst[*]: CRITICAL: Couldn't initialize X11 output. Aborting...
```

**Mechanism:** `notify.show` runs `notify-send` as a subprocess →
libnotify → D-Bus `org.freedesktop.Notifications`. If no notification
daemon is registered on the bus, D-Bus auto-activates one (dunst on
Arch). The dbus-activated daemon inherits its env from the **D-Bus
activation env**, not from the compositor — so on a wlroots-based
Wayland session (Hyprland, Sway, river, Niri) where the compositor
doesn't push `WAYLAND_DISPLAY` into the bus by default, dunst tries
its X11 backend, finds no `DISPLAY` either, and exits 1.

**Fix:** Add two lines to the compositor autostart so it propagates
the display env into both systemd `--user` AND the D-Bus activation
context:

```hyprlang
# ~/.config/hypr/hyprland.conf
exec-once = systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP HYPRLAND_INSTANCE_SIGNATURE
exec-once = dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP HYPRLAND_INSTANCE_SIGNATURE
```

KDE Plasma and GNOME do this automatically; bare wlroots compositors
don't. After re-login, `systemctl --user show-environment | grep
WAYLAND_DISPLAY` must show the wayland-N socket and the toast path
works. Full write-up in `docs/harness-hooks.md` § "Graphical-session
prerequisites".

### Harness Discord trigger fires but no message lands in the channel

**Symptom:** `coctl recent --kind discord.send_message.completed`
shows the completion event, but the Discord channel stays empty.

**Cause:** Most common is wrong channel id. The id is a literal in
user config (`~/.config/copad/config.toml`) — trigger interpolation
does NOT do `${ENV_VAR}` expansion. The id must be a Discord
**channel id**, not a server id, message id, or application id. In
the Discord client: Settings → Advanced → Developer Mode ON, then
right-click the channel → Copy Channel ID. Less common: the bot
isn't a member of the server holding that channel, or the bot lacks
`Send Messages` in that specific channel (server-wide permission is
overridden by channel-level role/permission grants).

**Diagnose:**

```
coctl recent --kind discord.send_message.failed   # if .failed exists, error_code is the API response
coctl call discord.send_message --params '{"channel_id":"<your-id>","content":"manual probe"}'
```

If the manual probe also lands a `.failed` with `403` / `50001`,
it's a permission/membership issue — re-invite the bot via OAuth2
URL Generator with the right scopes (`bot` + `Send Messages` at
minimum). If `404` / `10003`, the channel id is wrong.

### web-bridge plugin exits immediately with "refusing to start"

**Symptom:** journal `[plugin:web-bridge::main:stderr]` shows
`COPAD_WEB_BRIDGE_TOKEN is not set` or `is too short`, then the
supervisor logs `service web-bridge::main exited`.

**Cause:** web-bridge fail-closes if the token env is missing or
shorter than 32 characters. The token gates every `/api/*` and `/ws/*`
request and the WS subprotocol — without it the plugin's HTTP surface
would be open on `127.0.0.1:7575` to anyone with local access (and
worse if you bind a Tailscale IP). Refusing to start is the correct
posture.

**Fix (one-shot, in-memory):** generate a 64-char token, inject into
the user-instance env, restart:

```
TOKEN=$(openssl rand -hex 32)
systemctl --user set-environment COPAD_WEB_BRIDGE_TOKEN="$TOKEN"
systemctl --user restart copad-daemon
```

`set-environment` is memory-only — values evaporate at reboot or
session restart and the plugin will fail to start again.

**Fix (persistent, recommended):** drop the secrets into a systemd
unit drop-in keyed to `copad-daemon.service`. The drop-in is read
on every `daemon-reload` + restart, survives reboots, and stays
unit-scoped so other services don't accidentally inherit the
secrets via `import-environment`:

```
mkdir -p ~/.config/systemd/user/copad-daemon.service.d
cat > ~/.config/systemd/user/copad-daemon.service.d/web-bridge-env.conf <<EOF
[Service]
Environment=COPAD_WEB_BRIDGE_TOKEN=$(openssl rand -hex 32)
# Generated by scripts/gen-vapid-keys.sh — paste the two key lines:
Environment=COPAD_WEB_BRIDGE_VAPID_PRIVATE=<paste private>
Environment=COPAD_WEB_BRIDGE_VAPID_PUBLIC=<paste public>
Environment=COPAD_WEB_BRIDGE_VAPID_SUBJECT=mailto:copad@localhost
EOF
chmod 600 ~/.config/systemd/user/copad-daemon.service.d/web-bridge-env.conf
systemctl --user daemon-reload
systemctl --user restart copad-daemon
```

Rotation: edit the file in place + `daemon-reload + restart`.
Phone-side: a fresh token clears every browser's stored bearer; a
fresh VAPID key pair invalidates every existing push subscription
(re-subscribe from the dashboard).

Why a drop-in and not `~/.config/copad/outputs.env`: the installed
`copad-daemon.service` doesn't declare `EnvironmentFile=`, so a
file at that path is never read. `Environment=` lines inside a
`.service.d/*.conf` drop-in ARE read on every start, with the file
permissions enforced (0600 keeps the token unreadable to other
users on the box).

### web-bridge tmux overview is empty even with active tmux sessions

**Symptom:** `/api/tmux/panes` returns `[]`, the SPA's "tmux panes"
section shows "no tmux panes" placeholder, but `tmux ls` on the shell
lists active sessions.

**Cause one (most common):** `tmux` is not on the daemon process's
PATH. copad-daemon is started by `systemctl --user`, which has a
narrower PATH than your interactive shell. If `which tmux` from your
shell returns `~/.local/bin/tmux` or another non-system path, the
daemon's child plugins won't find it.

**Fix:** verify with `systemctl --user show-environment | grep PATH`.
Add the missing dir via `systemctl --user set-environment
PATH=$PATH:$HOME/.local/bin && systemctl --user restart copad-daemon`.
For persistence, drop the right `Environment=PATH=…` line into the
user unit (or rely on `systemctl --user import-environment PATH` from
your shell rc).

**Cause two:** No tmux server is actually running (despite the user
thinking otherwise — `tmux ls` from a stale shell can mislead). The
endpoint returns `[]` instead of erroring; this is deliberate so the
SPA renders the empty state cleanly.

### web-bridge attach mode shows a black xterm screen forever

**Symptom:** Tapping a tmux pane card transitions to attach mode but
the xterm viewport stays black; no PTY output ever arrives.

**Cause one:** xterm.js failed to load (CDN unreachable / browser
blocked the `<script>`). Open browser devtools → Network tab → look
for `xterm.js` 200. If 0/blocked: check ad-blockers + CSP. The
`xterm.css` block is loaded via `<link>` so a black bg without xterm
DOM nodes points to the JS failing.

**Cause two:** the `tmux attach-session` child failed (the target
session was killed between `list-panes` and `attach`, or `tmux` not
on PATH — see the previous entry). The plugin logs the error to
stderr; check `journalctl --user -u copad-daemon --since '2 min ago'
| grep web-bridge`. The fix is to refresh the overview (the WS push
will drop the dead pane on the next tick).

**Cause three:** PTY child exited but the WS stayed open (rare).
Click "← overview" to disconnect; the next attach spawns a fresh
PTY. If this recurs, daemon log will show `attach session ended`
followed by the underlying portable-pty error.

### web-bridge dashboard shows "copad GUI is not running" banner

**Symptom:** the dashboard loads, presence toggle works, recent events
feed updates — but the pane list is empty and the input textarea is
disabled with a banner reading "copad GUI is not running — only
presence + events work".

**Cause:** `terminal.read` / `terminal.feed` / `tab.list` /
`session.list` are GUI-owned methods routed by the daemon through
`GuiRegistry`. With no GUI registered (no `copad` process attached
to `copadd`), those methods return `no_gui`. The plugin maps
`no_gui` to HTTP 503 and the UI banner. Presence + event endpoints
are daemon-owned, so those still work.

**Fix:** start the copad GUI on the host so `copadd` has a
registered client:

- Locally: launch copad as usual (desktop entry / shell).
- Cold-boot via SSH-only: the user's Hyprland config adds
  `exec-once = /home/marshall/.local/bin/copad` so after autologin
  the GUI is attached automatically. Without autologin + greetd, a
  fresh boot reached only via SSH has no graphical session at all —
  copad cannot start without a Wayland/X display, and `no_gui` is
  the correct failure mode. See decisions.md #42 and harness-integration.md
  for the case-(3) discussion.

### web-bridge sees zero events on `/ws/events`

**Symptom:** the dashboard's "recent events" feed stays empty, but
`coctl event subscribe` from a shell receives `presence.changed`
fine.

**Cause (one known instance):** the original `daemon_client::subscribe`
used `tokio::net::UnixStream + into_split`. Connect + write succeeded,
but the read side never produced any line — not even the daemon's
`{"status":"subscribed"}` ack. The same socket protocol works with
`std::os::unix::net::UnixStream` (which `coctl client.rs` uses).
Current code runs the subscribe loop inside `tokio::task::spawn_blocking`
with the sync UnixStream and works correctly; if you swap it back to
tokio async be ready to chase this. RPC (one-shot request/response)
on tokio async UnixStream works fine — the bug is specific to the
long-lived subscribe path.

### `coctl presence away` set but Discord still silent

**Symptom:** `coctl presence status` correctly prints `away`, the
local `notify.show` toast fires, but no `discord.send_message.*`
event appears.

**Cause:** The presence-gated trigger isn't in user config. Slice 1B
ships **two** `[[triggers]]` blocks per harness event in
`examples/triggers/claude-hooks.toml` — the first runs `notify.show`
unconditionally, the second runs `discord.send_message` with
`condition = 'context.presence == "away"'`. Copying only the toast
block (the pre-slice-1B copy) means presence has nothing to gate.
Re-copy the discord blocks from the current example file, replace
`REPLACE_WITH_YOUR_CHANNEL_ID` with your channel id, and either
restart `copadd` or wait ~2s for the config watcher to pick up
the change.

### Pilot goals stall without a gate after a claude upgrade (marker drift)

**Symptom:** pilot goals sit in `running` forever (or hit the re-prompt
budget and go `stalled`) while the underlying tmux session is visibly
waiting on a plan-approval / permission / trust prompt that pilot never
reports as a gate.

**Cause:** `csd`'s gate detection matches capture-pane substrings pinned
to claude's TUI wording, which can change in any release. When the
wording shifts, the gate is invisible to `csd state` — the session waits
on a prompt nobody answers.

**Signal:** since `csd` 0.2.0 every `spawn`/`state`/`ps` JSON carries a
`marker_warning` field when the installed claude version is not in the
marker-verified set, and pilot publishes **`pilot.marker_warning
{id, warning}`** (once per distinct warning per daemon run) the moment a
goal spawns on a drifted version. Wire it to a toast like the other
harness events:

```toml
[[triggers]]
event_kind = "pilot.marker_warning"
action = "notify.show"
[triggers.args]
title = "pilot: claude marker drift"
body = "{event.warning}"
```

**Fix:** re-verify the markers on the new release (`csd` repo:
`./scripts/e2e.sh` exercises question / plan / permission / trust gates
live) and extend `verified_versions` in `csd`'s `src/backend.rs` — or,
after manual verification, silence locally with
`CSD_VERIFIED_VERSIONS=<version>` in the daemon environment.

---

## macOS App Issues

### SwiftTerm: `processTerminated` never called after shell exits

**Cause:** SwiftTerm's `LocalProcess.childProcessRead` detects PTY EOF and calls `childStopped()`, which cancels the internal `childMonitor` DispatchSource before it can fire. The `processTerminated` call in the EOF handler is commented out in SwiftTerm source.

**Fix:** Install a separate `DispatchSource.makeProcessSource` after `startProcess()` returns (in `CopadTerminalView.installExitMonitor()`). This source is not affected by `childStopped()` and fires independently when the process exits.

```swift
func installExitMonitor() {
    let pid = process.shellPid
    guard pid > 0 else { return }
    let src = DispatchSource.makeProcessSource(identifier: pid, eventMask: .exit, queue: .main)
    src.setEventHandler { [weak self, weak src] in
        src?.cancel()
        guard let self else { return }
        processDelegate?.processTerminated(source: self, exitCode: nil)
    }
    exitMonitor = src
    src.activate()
}
```

### macOS split panes: new pane gets wrong initial size

**Cause 1 (`layout()` approach):** NSSplitView calls `resizeSubviews` (which sets subview frames) before calling `layout()`. By the time `layout()` fires, the wrong frames are already committed. Calling `setPosition` in `layout()` fires too late — if the terminal view already has a large frame from before the rebuild, NSSplitView uses that as the basis for proportional sizing.

**Cause 2 (`asyncAfter` approach):** The 50ms delay is unreliable — layout may not have resolved yet, or a subsequent split may have started before the timer fires, applying stale positions.

**Fix:** Use `NSSplitViewDelegate.splitView(_:resizeSubviewsWithOldSize:)`. This delegate method is called by NSSplitView at the exact moment it needs to determine subview frames. Set frames directly here and set `initialSizeSet = true` after the first call to fall back to `adjustSubviews()` for subsequent resizes (preserving user drag behaviour).

### macOS: `becomeFirstResponder` cannot be overridden in SwiftTerm subclass

**Cause:** `MacTerminalView.becomeFirstResponder` is declared `public` but not `open`, so it cannot be overridden by code outside the SwiftTerm module.

**Fix:** Use `NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown)` in `PaneManager` to detect which pane was clicked and update `activePane` accordingly.

### macOS: `terminal.output` event not implementable

**Cause:** SwiftTerm's `feed(byteArray:)` is declared in an extension of `TerminalView` (not `open`), so it cannot be overridden by subclasses outside the module. There is no other public hook for intercepting raw PTY output bytes.

**Status:** Not implemented. Shell integration signals (`terminal.shell_precmd` / `terminal.shell_preexec`) are sent via socket commands from the shell script directly instead of OSC 133 parsing.

### macOS: OSC 52 clipboard write was unconditional (security regression)

**Cause:** SwiftTerm's `LocalProcessTerminalView.clipboardCopy(source:content:)` is declared `public` (not `open`) and unconditionally writes the OSC 52 payload to `NSPasteboard.general`. Because the method is `public`, subclasses outside the SwiftTerm module cannot override it. Pre-fix, any program in a pane could silently overwrite the user's clipboard.

**Fix:** `CopadTerminalView` installs a custom `CopadTerminalDelegate` proxy into SwiftTerm's `terminalDelegate` slot. The proxy forwards `sizeChanged` / `setTerminalTitle` / `hostCurrentDirectoryUpdate` / `send` / `scrolled` / `rangeChanged` to the host's public methods (so PTY winsize, title updates, OSC 7, key input, etc. continue to work) and applies an `OSC52Policy` gate on `clipboardCopy`. `requestOpenLink` / `bell` / `iTermContent` are left to the protocol-extension defaults — overriding them would change behavior with no benefit.

The policy is read from `[security] osc52` in config (`"deny"` default, `"allow"` opts back into legacy behavior). Hot-reload propagates through `applyConfig` → `paneManager.applyOSC52Policy` so live panes pick up the change without restart.

VTE on Linux already disables OSC 52 by default, so this fix is macOS-only.

### macOS: Nerd Font icons show as boxes or render broken

**Cause 1 — Font not found by family name:** `NSFont(name:size:)` only accepts PostScript names and full names (e.g. `JetBrains Mono Regular`), not bare family names like `JetBrainsMono Nerd Font Mono`. When the lookup fails, the terminal falls back to the system monospace font which has no Nerd Font PUA glyphs.

**Fix:** Font resolution now uses a multi-step strategy: PostScript name → `NSFontManager` exact family lookup → case-insensitive family lookup → `NSFontDescriptor` → system fallback. Both PostScript names and family names now work reliably.

**Cause 2 — Using non-Mono Nerd Font variant:** Standard Nerd Font variants (e.g. `JetBrainsMono Nerd Font`) render icons as 2-column wide glyphs. SwiftTerm's Unicode width table does not include PUA codepoints (U+E000–U+F8FF), so it treats them as 1-column, causing icons to overflow into the adjacent cell.

**Fix:** Use the **Mono** variant of your Nerd Font (e.g. `JetBrainsMono Nerd Font Mono`). Mono variants explicitly set all icons to 1-column width.

```toml
[terminal]
font_family = "JetBrainsMono Nerd Font Mono"
```

### macOS: Powerline glyphs, Claude Code banner, and Nerd Font icons render as `_` (esp. inside tmux)

**Cause:** Copad.app launched from Finder / Spotlight / Dock inherits launchd's empty env (`launchctl getenv LANG` returns nothing). `/etc/zprofile` does set `LANG=C.UTF-8` for **login** shells, but tmux pane shells and other non-login children skip it — and tmux's per-client UTF-8 detection at `attach` time uses the launching shell's locale, not its own pane shell's. Without a UTF-8 locale at that probe point, tmux client comes up with `utf8=0` and the outer terminal (whichever app is attached) renders Unicode glyphs as `_` placeholders, even though the byte stream itself is valid UTF-8.

Ghostty avoids this by injecting `LANG` into every PTY child it spawns. Copad did not — so the symptom appeared inside Copad but was absent in Ghostty for the same tmux session.

Korean text often still rendered because once a UTF-8 byte sequence makes it through, AppKit / CoreText can still draw it via cascading font fallback. The failure mode bites Nerd Font icons / powerline triangles / the Claude Code `✻` because those need *both* the byte to arrive intact AND a font fallback hop — and the byte-pass already failed at the tmux layer.

**Fix:** `copad-term::copad_term_create` injects `LANG=C.UTF-8` into the PTY child env when none of `LANG` / `LC_ALL` / `LC_CTYPE` is already set in the parent (Copad.app) process. Same default Ghostty uses. Explicit user locale set via `launchctl setenv` or a wrapper script wins (the conditional only fires when nothing is present).

Verified by the user: adding `export LANG=C.UTF-8` to `.zshrc` made the rendering work; this fix moves the same injection into the PTY spawn so users don't need the rc-file workaround.

Same pattern was already present in `plugins/web-bridge/src/main.rs:1162` for tmux attach — the main PTY spawn was the only place still missing it.

### macOS: Background `opacity` config change not reflected at runtime

**Cause:** `Config.swift` only parsed `path` and `tint` from the `[background]` section. The `opacity` field was silently ignored, and the `applyBackground` signature only accepted `path` and `tint`. Hot-reload therefore never changed the image layer's alpha.

**Fix:** Added `backgroundOpacity: Double` to `CopadConfig`, parse `("background", "opacity")` in `Config.parse`, and propagated an `opacity` parameter through the full call chain: `CopadPanel.applyBackground(path:tint:opacity:)` → `TerminalViewController` (sets `backgroundView?.alphaValue`) → `WebViewController` (no-op) → `PaneManager` → `TabViewController` (stores `currentBackgroundOpacity`) → `AppDelegate` initial apply and `background.set` socket command.

Also added `("background", "image")` as an alias for `("background", "path")` to match the documented config key.

### macOS: OSC 7 CWD URI includes hostname

**Cause:** OSC 7 delivers a `file://hostname/path` URI (e.g. `file://Marshalls-MacBook-Pro.local/Users/marshallku`). Simply stripping `file://` leaves the hostname in the path.

**Fix:** Use `URL(string: directory).path` to correctly extract only the POSIX path component, discarding the scheme and hostname.

### macOS: Web tab opens with no URL bar — only "Open a URL to get started"

**Cause:** `WebViewController.loadView` set `view = wv` (the bare `WKWebView`), so the only way to navigate was the `webview.navigate` socket command. Linux's `WebViewPanel` ships a Catppuccin-themed toolbar (back / forward / reload / URL entry / devtools) above the webview; macOS lacked the entire toolbar.

**Fix:** Wrap the `WKWebView` in an `NSView` container with an `NSStackView` toolbar above it. Toolbar buttons use SF Symbols (`chevron.left`, `chevron.right`, `arrow.clockwise`, `wrench.and.screwdriver`) and call existing `goBack` / `goForward` / `reload` / `toggleDevTools`. URL `NSTextField` fires its action on Enter and routes through `navigate(to:)`, which already handles the `https://` prefixing.

Back/forward enabled state and URL field text sync via KVO on `WKWebView.canGoBack` / `canGoForward` / `url` — `WKWebView` is KVO-compliant for these. The URL sync skips updates while the field's editor is the first responder so it doesn't clobber what the user is typing. On a blank tab (no `startURL`), `viewDidAppear` focuses the URL field so the user can type immediately.

### macOS: TCC re-prompts for Accessibility/Input Monitoring on every rebuild

**Cause:** `swift build` emits an ad-hoc, linker-signed binary (`codesign -dv` shows `adhoc,linker-signed`, `TeamIdentifier=not set`, `Internal requirements=none`). Every rebuild produces a fresh cdhash, and TCC keys its grants on the binary's *designated requirement* — which collapses to cdhash when there's no real signature. macOS therefore sees each build as a different app and re-prompts for every previously-granted permission.

**Fix:** `scripts/codesign-dev.sh` creates a self-signed `Copad Dev` code-signing cert in the user's login keychain (once, via `openssl req` + `security import`) and re-signs the bundle with that identity on every build. Both `scripts/install-macos.sh` and `copad-macos/run.sh` call it automatically.

After the first rebuild the Keychain Access dialog asks once for permission to use the new key — click **Always Allow**. From then on TCC binds grants to the cert identity, so permissions persist across rebuilds. To start over (wipe the cert), delete the `Copad Dev` entry under *login* in Keychain Access; the next build regenerates it.

The cert is self-signed and not trusted by Gatekeeper — that's intentional. TCC doesn't require trust, only signature validity. Apple Developer ID would be overkill for local dev.

### macOS: slack plugin hangs at `initialize`, copadd kills it after 5s

**Symptom.** copadd log shows `service slack::main failed to start: did not reply to initialize within 5s` repeatedly. Plugin stderr only emits up to `[slack] token store: keyring …` then nothing — the process is alive but blocked.

**Cause.** macOS Keychain ACLs are bound to the writing binary's code-signing identity (cdhash). Every dev rebuild produces a new cdhash, so the next `get_password` triggers a "Allow this app to access?" prompt. A copadd-spawned plugin runs in a background context with no UI to surface that prompt — the call blocks forever, copadd hits its init deadline and SIGKILLs. The `security` CLI doesn't see this because it's Apple-signed and routed through a different code path that bypasses the prompt.

`-A` ("allow all applications") via `security add-generic-password -A` widens the ACL but does **not** fix this case — on modern macOS the partition list (a newer constraint layered on top of ACL) still blocks unsigned/ad-hoc-signed binaries, and the partition list can only be edited with the keychain password (`-k`), which we can't supply programmatically from a background process.

**Fix.** `plugins/slack/src/config.rs` defaults `Config::use_keychain` to `false` on macOS. `open_store` then skips the keyring probe entirely and goes straight to `PlaintextStore` at `~/.config/copad/slack-tokens-default.json` (mode 0600). Same single-user-laptop protection envelope as the previous fallback path, but instant — no 2s probe wait and no `failed to start` cycle. Opt back in with `COPAD_SLACK_USE_KEYCHAIN=1` if the binary is on a stable build *and* the user has manually arranged the ACL.

The `KeyringStore::open` 2-second probe timeout stays as a safety net for the opt-in path: any future hang surfaces as a clear timeout message and falls back to plaintext, rather than the 5-second init-deadline silent SIGKILL.

### macOS: slack reaction/mention events don't reach the panel even though Socket Mode is "connected"

**Symptom.** Plugin logs `[slack] socket mode connected` and the panel's FEED tab stays empty no matter how many reactions or mentions are sent. `apps.connections.open` and `auth.test` both return `ok: true`.

**Cause.** Slack's Socket Mode is just the *transport* — what event types actually arrive over the WebSocket is controlled by the Slack App's **Event Subscriptions** page. If "Enable Events" is OFF, or `Subscribe to bot events` is empty, Slack accepts the WebSocket connect but sends zero frames (not even `hello` in some configurations).

**Trap.** The Event Subscriptions UI requires a `Request URL` for the legacy HTTP webhook flow. Even when Socket Mode is ON (per the Socket Mode page), the UI may still show `Request URL` as required and refuse to enable Save Changes. Toggling "Enable Events" without being able to Save means refreshing the page reverts the toggle, leaving the user stuck in a loop.

**Fix.** Edit the **App Manifest** directly (Settings → App Manifest in api.slack.com/apps) — bypasses the UI's stuck Request-URL check. Add:

```yaml
settings:
  event_subscriptions:
    bot_events:
      - app_mention
      - message.im
      - reaction_added
  socket_mode_enabled: true
```

Save the manifest, then reinstall the app from the **Install App** sidebar entry so the new scopes/events take effect. Frames should start flowing on the next plugin reconnect.
