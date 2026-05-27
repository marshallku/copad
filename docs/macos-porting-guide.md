# macOS Porting Guide

**Heads-up:** `copad-macos/` is *not* a stub. The root `CLAUDE.md` and
project memory still call it one, but that's stale — the macOS app
already runs at near-parity with `copad-linux/`. Renderer migration
(SwiftTerm → `alacritty_terminal`) flipped the default in commit
`e0ddf31` (Phase 10a). Daemon-first migration (PRs 1–8, see
`docs/macos-daemon-migration-plan.md`) is shipped. 10 first-party plugins
build + install. Status bar, command palette, session persistence — all
landed.

This document tells you (a) what *actually* exists, (b) how to drive the
build/dev loop locally, and (c) where the remaining work is queued, so
you can pick up where the Linux machine left off without re-discovering
everything. Source of truth for current behavior is `docs/macos-app.md`
and the three plan docs in `docs/INDEX.md`. Read those for any non-trivial
change.

---

## 1. Current macOS state

Tree shape: `copad-macos/Package.swift` (SwiftPM, macOS 14+, Swift 6) +
`copad-macos/run.sh` (dev launcher) + `copad-macos/Sources/` containing
three SPM targets:

- **`Copad/`** — 31 Swift files, ~7800 LOC. The full app: `CopadApp` /
  `AppDelegate` / tab+split tree (`TabViewController`, `PaneManager`,
  `SplitNode`, `CopadPanel`) / two terminal backends
  (`TerminalViewController` = SwiftTerm fallback,
  `AlacrittyTerminalViewController` = `copad-term` default) /
  `WebViewController` / `PluginPanelController` / `SocketServer` /
  `DaemonClient` / `ActionRegistry` / `ContextService` / `EventBus` /
  `CommandPalette` / `Session` / `StatusBarView` + `StatusModuleRunner` /
  `BackgroundRotator` / `Keybindings` / `ClaudeStart` / `Config` +
  `ConfigWatcher` / `Theme` / `URLClickHelper` / `WebViewJS` /
  `FFIBridge` (libcopad_ffi) / `CopadTermFFI` (libcopad_term) /
  `CopadPaths` / `PluginManifest` plus a few small helpers
  (`FileLock`, `UnixSocket`, `SendableBox`). Use `ls
  copad-macos/Sources/Copad/` for the live list.
- **`CCopadFFI/`** — clang module wrapping `libcopad_ffi.a` (trigger
  engine + JSON round-trip). Header + module.modulemap + a `dummy.c`
  to force SPM to emit an object so linker settings flow through.
- **`CCopadTerm/`** — same shape, wrapping `libcopad_term.a`
  (alacritty_terminal grid + damage snapshot for the custom renderer).

How it builds today is `./copad-macos/run.sh` (debug, dev iteration)
or `./scripts/install-macos.sh` (release install to `~/Applications`).
Both wrap the same two-step pipeline because SwiftPM can't call cargo
as a prebuild step:

```
cargo build --release -p copad-ffi -p copad-term     # → target/release/libcopad_{ffi,term}.a
swift build -c release                                  # links the .a via Package.swift's linkerSettings
```

`swift build` alone after `cargo clean` will fail at link with
undefined symbols. The cargo step is the source of truth for build order.

---

## 2. Architecture for the macOS porter

The cross-platform contract is "shared Rust core, native UI". For each
Linux feature, this is its macOS equivalent and current status.

| Concern | Linux source | macOS source | Status |
|---|---|---|---|
| PTY + terminal grid | VTE 0.84 owns PTY (`copad-linux/src/tabs.rs`) | `AlacrittyTerminalViewController` via `libcopad_term.a` (default); SwiftTerm `TerminalViewController` (fallback) | done; SwiftTerm removal pending (Phase 10b) |
| Renderer | VTE | Custom AppKit/CoreText draw + `CADisplayLink` damage gate | done (decision #36) |
| Socket server | `copad-linux/src/socket.rs` (1961 LOC) | `SocketServer.swift` + `AppDelegate.handleCommand` | done |
| Daemon (`copadd`) | wired (`83c5122`) | `DaemonClient.swift` auto-spawns + connects | done |
| Tabs + splits | `tabs.rs` (2050 LOC), `split.rs` (376 LOC) | `TabViewController` + `PaneManager` + `SplitNode` + `EqualSplitView` | done |
| Webview pane | `webview.rs` (526 LOC, webkit6) | `WebViewController` (WKWebView) + `WebViewJS` | done |
| Plugin HTML/JS panel | `plugin_panel.rs` (411 LOC) | `PluginPanelController` w/ `WKScriptMessageHandlerWithReply` | done |
| Trigger engine | `copad-core::trigger` (Rust SoT); Linux wires `LiveTriggerSink` in `window.rs` | FFI via `copad-ffi` + `FFIBridge.CopadEngine` (Swift trampoline) | done (PR 5c) |
| Action registry | `copad-core::action_registry` | `ActionRegistry.swift` (no `register_blocking` yet) | done for parity scope |
| ContextService | `copad-core::context` | `ContextService.swift` mirrors apply rules | done (PR 9) |
| Background images | `background.rs` (184 LOC) | `BackgroundRotator.swift` | done |
| Status bar | `statusbar.rs` (427 LOC) | `StatusBarView` + `StatusModuleRunner` | done (Tier 4.2); `top` position deferred |
| Command palette | `command_palette.rs` (332 LOC) | `CommandPalette.swift` | done |
| Session persistence | `session.rs` (201 LOC) | `Session.swift` | done |
| Theme | `copad-core::theme` | `Theme.swift` (10 themes; 8→16-bit RGB conversion for SwiftTerm) | done |
| Plugin supervisor | `copad-daemon::ServiceSupervisor` (single SoT) | daemon owns it; Swift's prior native supervisor was deleted in PR 5 (commit `2913441`) | done |
| Custom keybindings | `tabs.rs:1413` `spawn_command` | `Keybindings.swift` via `NSEvent.addLocalMonitorForEvents` | done |
| `notify.show` | core `Notifier` trait; both daemon + GUI register | daemon registers (osascript); GUI in-process **not yet wired** | gap (catchup §B) |
| `BusEvent.origin` | shipped (`d03a01a`) | Swift struct missing the field | gap (catchup §B) |
| `terminal.output` event | VTE `connect_commit` | `AlacrittyTerminalViewController.sendInput` helper | done on alacritty path; impossible on SwiftTerm |
| D-Bus | `com.marshall.copad` session bus | n/a — Unix socket only | out of scope (don't add) |

The "out of scope" row matters. Nothing on the macOS side talks D-Bus.
The `coctl` ↔ daemon and GUI ↔ daemon paths are the only IPC, and both
run over `~/Library/Caches/copad/socket` (mode 0600).

---

## 3. Build & dev loop on macOS

**Fastest iteration:**

```bash
cd copad-macos
./run.sh         # cargo + swift + bundle .app + open -n
```

`run.sh` re-stages `Info.plist`, code-signs via `scripts/codesign-dev.sh`,
`pkill -x Copad`, and `open -n`s a fresh debug bundle.

**Just build:**

```bash
(cd .. && cargo build --release -p copad-ffi -p copad-term) && swift build -c release
```

If you only changed Swift you can skip the cargo step. If you changed
Rust in `copad-ffi/` or `copad-term/`, you still need both — SwiftPM
caches the link result.

**Real install to `/Applications`-style:**

```bash
./scripts/install-macos.sh              # ~/Applications + ~/.cargo/bin (no sudo)
./scripts/install-macos.sh --system     # /Applications (sudo)
./scripts/install-macos.sh --launch     # open after install
./scripts/install-macos.sh --no-build   # if .build/release/Copad already exists
./scripts/install-macos.sh --no-plugins # skip the 10-plugin cargo+install loop
./scripts/install-macos.sh --no-copadd # skip daemon install
./scripts/install-macos.sh --no-daemon  # skip LaunchAgent install (still installs copadd binary)
```

This is the dogfood path. The script cargo-builds both staticlibs + all
plugins, swift-builds the app, stages in tmp, signs, atomic `mv` into
`$APP_DEST`, `cargo install --path copad-cli` + `--path copad-daemon`,
copies each plugin manifest + binary into `~/Library/Application
Support/copad/plugins/<name>/`, and writes + bootstraps
`~/Library/LaunchAgents/com.marshall.copad.daemon.plist`.

**coctl / copadd alone:**

```bash
cargo install --path copad-cli       # → ~/.cargo/bin/coctl
cargo install --path copad-daemon    # → ~/.cargo/bin/copadd
```

`cargo install copad-cli` and `cargo install --path .` from the repo
root both fail. Always pass `--path <crate-dir>`.

**Code signing:** `scripts/codesign-dev.sh` creates a stable self-signed
`Copad Dev` cert in your login keychain on first run, then re-signs
every build with the same identity. Without it, every rebuild gets a
fresh cdhash and TCC re-prompts for Accessibility / Input Monitoring.
Both `run.sh` and `install-macos.sh` call it automatically.

**Universal binary:** currently host-arch only.
`cargo build --target {aarch64,x86_64}-apple-darwin … && lipo -create …
-output target/release/libcopad_ffi.a` is the recipe; deferred until a
real x86_64 user appears.

---

## 4. IPC contract + macOS paths

JSON-RPC over Unix socket. `copad-core::protocol` is the cross-platform
SoT (newline-delimited JSON, `{method, params, id?}` request /
`{ok, result|error, id}` response). `coctl` is platform-neutral.

**macOS paths:**

| Purpose | Path |
|---|---|
| GUI per-process socket | `/tmp/copad-{PID}.sock` (mode 0600) |
| Daemon socket | `~/Library/Caches/copad/socket` (mode 0600) |
| Daemon PID file | `~/Library/Caches/copad/daemon.pid` |
| LaunchAgent plist | `~/Library/LaunchAgents/com.marshall.copad.daemon.plist` |
| Daemon logs | `~/Library/Logs/copad-daemon.{out,err}.log` |
| Config | `~/.config/copad/config.toml` (same as Linux — dotfile sharing) |
| Session state | `~/Library/Application Support/copad/session.json` |
| Plugin install dir | `~/Library/Application Support/copad/plugins/<name>/` |
| Plugin XDG fallback | `~/.config/copad/plugins/<name>/` (read also; macOS-root wins) |
| Wallpapers list | `~/Library/Caches/copad/wallpapers.txt` (XDG fallback: `~/.cache/terminal-wallpapers.txt`) |
| Background mode flag | `~/Library/Caches/copad/bg-mode` |

`copadd`'s socket path is computed by
`copad-core::paths::daemon_socket_path()`. `coctl` auto-discovers it.
GUI's `DaemonClient.connectAndRegister`:

1. `connect(2)` to the daemon socket.
2. On `ECONNREFUSED`/`ENOENT`, take the single-flight lock
   (`~/Library/Caches/copad/.spawn-lock` via `FileLock.swift`),
   `posix_spawn` `copadd`, wait ~1s for socket, retry.
3. Send `gui.register` with `bridge_id`, GUI env, capabilities.

If `copadd` is not in `$PATH` for the shell that launched `Copad.app`,
auto-spawn fails gracefully — status bar disappears, plugins go quiet,
GUI keeps working. Pre-install `copadd` via cargo to avoid this.

---

## 5. Plugin system on macOS

All 10 first-party plugins build + install on macOS. Status:

| Plugin | Status | Notes |
|---|---|---|
| `echo` | works | Protocol sanity-check. |
| `git` | works | No platform-specific deps; shells out to `git`. |
| `llm` | works (read) | `keyring` `apple-native` → Keychain. Write path needs real API key. |
| `calendar` | works (graceful) | OAuth device-code via `copad-plugin-calendar auth`. Without creds, RPC returns `not_authenticated`. |
| `kb` / `todo` / `bookmark` | works | `renameat2(RENAME_NOREPLACE)` was Linux-only; PR 6 extracted `copad_core::fs_atomic` which uses `renamex_np(RENAME_EXCL)` on macOS. |
| `slack` / `discord` / `jira` | installs, needs auth | Same `keyring` story. Each has an `auth` subcommand for token setup. |

`MACOS_PLUGINS` in `scripts/install-macos.sh` is currently
`(echo git llm calendar kb todo bookmark slack discord jira)`. Adding a
plugin = append to that array; the loop does `cargo build --release -p
copad-plugin-<name>` and copies manifest + binary into the plugin dir.

**`web-bridge` is not in `MACOS_PLUGINS` yet** — recent addition
(commits b8a2226 etc.). Review the plugin's `Cargo.toml` for
Linux-only deps before adding; the panel + Web Push code is portable.

**Webview substitution:** `webkit6` (Linux) and `WKWebView` (macOS) are
interchangeable at the plugin-bridge level. JS contract
`window.copad.call(method, params) → Promise` is byte-for-byte
identical; both sides use reply-capable handlers
(`WebKitUserContentManager` reply on Linux,
`WKScriptMessageHandlerWithReply` on macOS). The bridge JS is built by
`PluginPanelController.swift` matching `copad-linux/src/plugin_panel.rs:74`'s
`build_bridge_js`. No `#[cfg]` guards needed in plugin code.

**Discovery order** (both Swift `PluginManifestStore.discover()` and
daemon Rust `ServiceSupervisor::discover`):

1. `~/Library/Application Support/copad/plugins/<name>/plugin.toml`
2. `~/.config/copad/plugins/<name>/plugin.toml`

macOS path wins on conflict.

---

## 6. Phased work plan (live backlog)

The macOS app is past the original parity plan. Active queue is in
`docs/macos-post-renderer-catchup.md`. Suggested ordering:

**A. Renderer polish (alacritty backend)** — small to medium PRs:

1. **DSR (Device Status Report)** — nvim warns at startup because
   `CSI 6n` (cursor pos) and `CSI 0c` (attrs) are ignored. Two reply
   handlers in `copad-term`'s input loop. ~1 hour.
2. **NSImage async loading** — wallpaper open on main thread can stall
   during Gatekeeper/XProtect scan. Move `NSImage(contentsOfFile:)` to
   a background queue + progressive reveal.
3. **Block selection (Cmd+Option+drag)** — `SelectionType::Block`
   exists in alacritty_terminal but renderer never picks it. Wire
   modifier check in `mouseDown`/`mouseDragged` in
   `AlacrittyTerminalViewController`.
4. **Cursor visibility on busy wallpapers** — drop-shadow or thin outer
   stroke on focused fill variant.
5. **MOTION-level mouse forwarding (`\e[?1003h`)** — needs
   `acceptsMouseMovedEvents` plumbing. Rare in practice.

**B. Linux-parity catch-up** — Linux landed these, macOS hasn't:

1. **GUI in-process `notify.show`** — daemon has it; GUI doesn't.
   `coctl call notify.show` works only when daemon is up. Mirror
   `copad-linux/src/window.rs:218`'s `register_blocking_silent` in
   Swift, call `UNUserNotificationCenter`. ~2 hours.
2. **Swift `BusEvent.origin` field** — trust-boundary parity. Origin
   tagging shipped on Rust side (`d03a01a`, decision #37); Swift's
   `BusEvent` struct doesn't carry it. Limits Swift-side privileged-action
   gating.
3. **`copadd --version` short-circuit** — daemon binds socket before
   parsing argv, so a second invocation while one is running errors
   out even for `--version`. Parse `--version`/`--help` first.

**C. Phase 10b — remove SwiftTerm path:** after 2–4 weeks daily-use
dogfooding on alacritty with no regressions, delete the SwiftTerm path
entirely. File list in `docs/macos-post-renderer-catchup.md` §C. Biggest
code-simplification win available; do it when there's confidence.

**D. Cross-platform daemon work** — pure-Rust changes in `copad-daemon`
or plugins; macOS daemon auto-spawned via LaunchAgent picks them up for
free. Tracked in `docs/harness-integration.md` and `docs/service-plugins.md`.
No Swift work required.

**E. Test hygiene** — `paths::tests::*` has 7 env-var-race failures on
macOS in `cargo test -p copad-core --lib`. Either `serial_test::serial`
gate or subprocess-per-test.

**Recommended starting point:** **A1 (DSR)** for a tiny first commit, or
**B1 (`notify.show` in-process)** for a real cross-platform parity fix
that exercises FFI + registry + system APIs in one PR. The biggest-risk
item to start with cold is the SwiftTerm path (C) — leave it last.

---

## 7. Known gotchas and decisions already made

Read before touching the relevant subsystem. Linked to canonical doc.

- **OSC 52 clipboard write** was unconditional on macOS until Tier 0.3.
  `CopadTerminalDelegate` proxy gates `clipboardCopy` on
  `[security] osc52` (default `deny`). Mirrored on alacritty path.
  See `docs/troubleshooting.md`.
- **SwiftTerm `becomeFirstResponder` non-overridable** — `public` not
  `open`. Same for `mouseUp` (URL click), `clipboardCopy` (OSC 52),
  `feed(byteArray:)` (terminal.output). Workaround pattern is
  `NSEvent.addLocalMonitorForEvents(...)`. The alacritty path doesn't
  have any of these because the renderer is ours.
- **SwiftTerm `processTerminated` never called** — upstream bug. Fixed
  by a separate `DispatchSource.makeProcessSource` in
  `TerminalViewController.installExitMonitor`. Doesn't apply to
  alacritty.
- **TCC re-prompts on every rebuild** — fixed by
  `scripts/codesign-dev.sh`. Don't bypass; you'll waste 5 minutes
  re-granting Accessibility per rebuild.
- **`startProcess(environment:)` replaces parent env** — manually inherit
  + append `TERM`, `COLORTERM`, `COPAD_SOCKET`. See
  `TerminalViewController.startIfNeeded()`.
- **`startShellIfNeeded()` timing** — must call after
  `layoutSubtreeIfNeeded()` or SwiftTerm computes cols/rows = 0.
- **`NSSplitView` subview layout** — direct children need
  `translatesAutoresizingMaskIntoConstraints = true` + autoresizing
  mask `[.width, .height]`. Auto Layout fights NSSplitView. See
  `EqualSplitView` in `PaneManager.swift`.
- **OSC 7 URI parsing** — `file://hostname/path`; parse via
  `URL(string:).path` to drop hostname.
- **`Keychain` (`keyring` apple-native)** verified end-to-end on
  `kb`/`todo`/`bookmark`/`llm`/`calendar`/`slack`/`discord`/`jira`/`git`.
  First write may produce a Keychain prompt if binary path or signing
  identity changed.
- **Decision #31** (`docs/decisions.md`) — why we built our own renderer
  instead of forking SwiftTerm or waiting for libghostty. Required
  context if you ever think "why not just upgrade SwiftTerm".
- **Decisions #12, #13, #14, #15** — macOS split-pane layout +
  NSSplitViewDelegate + async socket via DispatchSemaphore + `CopadPanel`
  protocol. The "why" behind a lot of the current architecture.
- **`docs/macos-daemon-migration-plan.md` PR 4a/4b** — non-obvious
  wire-shape compat constraints if you touch `EventBus` ↔ daemon
  forwarding (`bridge_id`, origin gaps, context bridging, allowlists).

---

## 8. Explicitly out of scope

- **D-Bus integration** — Linux-only. Mac uses Unix socket only.
- **`terminal.output` on SwiftTerm path** — non-overridable extension
  method. Implemented on alacritty path via `sendInput` helper.
- **Hyprland-specific behavior** — workspace switch panel freeze,
  Wayland subsurface tricks. Mac uses NSWindow.
- **GTK4/VTE-specific features** — `Gtk.Builder`, GResource, CSS theming,
  GtkAccessible.
- **Tabs position `left` / `right`** — needs vertical TabBarView
  90-degree rotation; low ROI, deferred on Linux too.
- **App Sandbox / Hardened Runtime / notarization** — currently ad-hoc
  signed via `codesign-dev.sh`. Real notarization is a future
  deployment task, not a parity task.

---

## 9. Testing checklist

After porting or landing something, run through the relevant subset.
Manual verification matters more than unit tests at this layer.

**Daemon round-trip**
- [ ] `Copad.app` launches, status bar visible at window bottom.
- [ ] `coctl call system.ping` → `{"status":"ok"}`.
- [ ] `coctl call system.list_actions` lists registered actions.
- [ ] `coctl recent` shows recent daemon events.
- [ ] Quit `Copad`, daemon log keeps growing (`KeepAlive=true`).

**Plugin alive**
- [ ] `coctl call echo.ping --params '{"hi":"there"}'`
      → `{"echoed":{"hi":"there"},"from":"copad-plugin-echo"}`.
- [ ] `coctl call git.list_workspaces` returns `[]` or your workspaces.
- [ ] `coctl call llm.auth_status` → `{store_kind: "keyring", ...}`
      (confirms Keychain reachable).

**Terminal pane**
- [ ] New tab (Cmd+T) → shell at `~/`.
- [ ] `vim file.txt`, type a few CJK chars via IME → preedit visible
      inline. (The original blocker behind decision #31.)
- [ ] `tmux` / `htop` — mouse click + drag selects inside the TUI.
- [ ] OSC 8 hyperlinks (`ls --hyperlink` from gnu-coreutils via brew)
      open in default browser on Cmd+click.
- [ ] Plain `https://...` in shell output opens on Cmd+click.

**Splits + tabs**
- [ ] `Cmd+D` horizontal split, `Cmd+Shift+D` vertical. Equal sizing.
      Manual divider drag works.
- [ ] `Cmd+Shift+]` / `Cmd+Shift+[` cycles focus across panes.
- [ ] Quit shell (Ctrl+D) → pane removed, neighbor expands.

**Webview pane**
- [ ] `coctl call webview.open --params '{"url":"https://example.com"}'`
      opens a tab with toolbar (back / forward / reload / URL field /
      devtools).
- [ ] `coctl call webview.execute_js --params '{"code":"document.title"}'`
      → `"Example Domain"`.
- [ ] `coctl call webview.screenshot` returns base64 PNG.

**Plugin panel**
- [ ] `coctl call plugin.open --params '{"name":"todo","panel":"main"}'`
      opens the plugin's HTML panel.

**Triggers (Vision Flow 3 critical path)**
- [ ] Add to `~/.config/copad/config.toml`:
      `[[triggers]] name = "test" action = "system.list_actions"`
      `[triggers.when] event_kind = "echo.ping.completed"`
- [ ] `coctl call echo.ping --params '{"hi":"x"}'` — daemon log shows
      `event echo.ping.completed fired 1 trigger(s)`.

**Session persistence**
- [ ] 3 tabs with different cwds. `Cmd+Q`. Re-open `Copad.app`. All 3
      restored at the same cwd. (Split positions re-equalize to 50/50
      — divider position not tracked in v1.)

**Config hot-reload**
- [ ] Edit `[theme] name` in config, save. Recolors within ~200ms.
- [ ] Change `[background] tint`, save. Image alpha updates live.

**Keychain write**
- [ ] `ANTHROPIC_API_KEY=sk-... copad-plugin-llm auth` — first run may
      produce a Keychain prompt. After approve,
      `coctl call llm.auth_status` → `authenticated: true`.

---

## 10. When in doubt

- `docs/macos-app.md` — most actively-maintained macOS doc. Per-file
  behavior reference.
- `docs/macos-parity-plan.md` — Tier 0–4 plan, PR 1–9 summaries.
- `docs/macos-daemon-migration-plan.md` — daemon-first split. Read for
  `DaemonClient` / `EventBus` / daemon-forward changes.
- `docs/macos-renderer-migration-plan.md` — alacritty renderer. Read
  for `AlacrittyTerminalViewController` or `copad-term` changes.
- `docs/macos-post-renderer-catchup.md` — active backlog.
- `docs/troubleshooting.md` macOS section — re-discovering fixes is
  wasted time.
- `docs/decisions.md` — #12–15, #31, #36 are the most macOS-load-bearing.

If you find a workaround the docs don't mention, add a
`docs/troubleshooting.md` entry and an entry here in section 7. That's
the rule from the repo root `CLAUDE.md`: always update docs when making
changes.
