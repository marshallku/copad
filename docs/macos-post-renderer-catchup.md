# macOS Post-Renderer Catch-up

Living doc tracking what's left after Phase 10a flipped the macOS default
to the alacritty backend (commit `e0ddf31`, decisions.md #36). Predecessor
plans (all considered done for their original scope):

- [`macos-parity-plan.md`](./macos-parity-plan.md) — Tiers 0–4 (original Linux feature parity)
- [`macos-daemon-migration-plan.md`](./macos-daemon-migration-plan.md) — PRs 1–8 (GUI ↔ daemon split)
- [`macos-renderer-migration-plan.md`](./macos-renderer-migration-plan.md) — Phases 1–10a (alacritty backend, default flip)

Order below is rough priority. A items unblock visible polish; B items
close Linux feature gaps that Linux has already shipped; C is the
single biggest cleanup; D / E are non-blocking.

---

## A. Renderer polish (alacritty backend)

Each was either deferred during a Phase 3–10a slice or surfaced during
dogfooding after the default flip. Severity ≈ how often the user hits it.

- [x] **`terminal.output` event** — wired in `AlacrittyTerminalViewController.swift` via a `sendInput` helper that wraps every keyboard / paste path (`insertText`, `doCommand`, control combos, command-key shortcuts, paste). Mirrors Linux's `copad-linux/src/tabs.rs` VTE `connect_commit` hook (the kind name "output" follows the terminal-widget perspective: bytes going OUT of the widget toward the PTY). Mouse-mode wheel forwarding intentionally bypasses the helper because VTE excludes mouse from `commit`. Programmatic `initialInput` (e.g. `claude.start` seeding) also bypasses — matches Linux's `feed_child` behavior. Verified: typed letter `"x"` → `{"text":"x"}`; Return → `{"text":"\r"}`; Up arrow → `{"text":"[A"}`.
- [x] **Mouse click/drag forwarding** for mouse-mode TUIs. CLICK + DRAG levels wired (commit follows wheel-forwarding `5420ef5` pattern). New FFI `copad_term_mouse_report_level` exposes the CLICK / DRAG / MOTION tier so the renderer can gate drag-as-motion correctly. `AlacrittyTerminalViewController` overrides left / right / middle button down+up+dragged with a shared `forwardMouseEvent` helper that encodes via SGR (preferred) or legacy/UTF8. Shift held continues to bypass forwarding (host selection wins). Motion events dedupe at grid-cell granularity. **MOTION-level (`\e[?1003h` bare-cursor motion)** ✅ — `AlacrittyRenderView` overrides `updateTrackingAreas` to install a single `[.mouseMoved, .activeInKeyWindow, .inVisibleRect]` `NSTrackingArea` (zero-rect, view bounds tracked via `.inVisibleRect`) and overrides `mouseMoved(with:)`. The tracking area is always installed (no install/teardown thrash as TUIs flip modes), but the handler early-returns unless `termHandle.mouseReportLevel == .motion`. Bare-motion reports use `button: 3` (xterm "no button" sentinel) so SGR-encoded output is `\e[<35;col;rowM` per spec.
- [x] **DSR / DA reply forwarding** — `copad-term` `CopadListener` now matches on `Event::PtyWrite` and forwards the bytes via `EventLoopSender::send(Msg::Input(…))` back to the child PTY. alacritty_terminal already formats DSR (`CSI 6n` → `\e[<row>;<col>R`) and DA (`CSI 0c` → `\e[?6c`); we were dropping both on the listener floor. Sender is late-bound (`set_sender` after `EventLoop::channel()`); PtyWrite events before injection drop silently (none can fire that early in practice). Linux unaffected (VTE handles DSR/DA in-widget).
- [x] **OSC 4 / 10 / 11 / 12 color queries** — `CopadListener` now holds an `Arc<Mutex<HashMap<usize, Rgb>>>` palette. `Event::ColorRequest(idx, format_reply)` arm looks up the index, calls the alacritty-provided formatter (which produces the OSC-formatted reply string), and forwards via the same `EventLoopSender` path A3 wired. New FFI `copad_term_set_palette_entry(handle, idx, r, g, b)`; Swift `CopadTermFFI.Handle.applyPaletteFromTheme(_:)` pushes the 16 ANSI colors plus `foreground` (256), `background` (257), and `accent`-as-cursor (258). Called from `AlacrittyTerminalViewController.startIfNeeded` (initial push) and `applyTheme` (hot-reload). Indices the host doesn't populate stay silent — apps fall back to built-in defaults rather than getting an alacritty palette color we don't actually render. `CopadListener::new` pre-seeds Catppuccin Mocha (matching `copad_core::theme::Theme::default()`) so any OSC query firing in the race window between `copad_term_create` returning and `applyPaletteFromTheme` running still gets a coherent answer; the host overwrite on `applyTheme` takes precedence.
- [x] **NSImage async loading** — both `TerminalViewController.applyBackground` (SwiftTerm path) and `AlacrittyTerminalViewController.applyBackground` (alacritty path) offload `NSImage(contentsOfFile:)` to `DispatchQueue.global(qos: .userInitiated)` and bounce the visual swap back to main. Per-controller monotonic `backgroundLoadToken` (bumped on every applyBackground / clearBackground) drops stale decodes when a newer load races ahead; same applies if `clearBackground` arrives mid-decode, the late callback finds a token mismatch and no-ops. First attempt (`f1352ef`) was reverted on a false-positive "wallpaper never appears" report; root cause turned out to be `TERM=xterm-ghostty` inheritance + `transparent_default_bg` default (both fixed separately in `c9a9d7f` / `0742209`). Re-landed verified against direct screencapture.
- [x] **Cmd+/- zoom on alacritty path** — added via a tiny `Zoomable` protocol (in `CopadPanel.swift`) that both terminal VCs conform to. `AlacrittyTerminalViewController` tracks `currentFontSize` / `configFontSize` / `currentFontFamily` (mirror of SwiftTerm path), implements `zoomIn` / `zoomOut` / `zoomReset` with the same step (±1) + clamp (6..72) values. Each step routes through `applyFontInternal`, which reuses the existing `setFont` → cell-metrics recompute → `termHandle.resize()` chain so the PTY gets SIGWINCH for shell re-wrap. `applyFont` (config hot-reload) now preserves an in-flight zoom level instead of clobbering it. `AppDelegate` zoom actions dispatch through `tabVC?.activeZoomable?` (polymorphic across the two backends; webview / plugin panes silently no-op).
- [x] **Block selection (Option+drag)** — `selectionKind(for:)` returns `.block` when `event.modifierFlags.contains(.option)` on a single click; matches Terminal.app + iTerm2 (catchup doc originally said "Cmd+Option" but native convention is Option-only — Cmd is reserved for URL-click). Double / triple click stay word / line, keyed off click count. New `COPAD_SELECTION_BLOCK = 3` constant + `SelectionType::Block` arm in `copad_term_selection_start`. `paintSelection` checks `sel.is_block` (already on the wire via `selection_range_for_ffi`) and paints a rectangle instead of the row-wrapped span. `selection_range_for_ffi` clips block endpoints by row only; row-wrapped clipping (column-edge rewrite to 0 / last_col) would expand the visible band to the viewport width since block is column-major. `selectionString` and copy/paste flows pick up Block automatically because they go through alacritty's `Selection::to_range` + `selection_to_string`.
- [x] **Cursor visibility polish on busy wallpapers** — `drawCursor` in `AlacrittyRenderView` now adds a 1-px `theme.background` outline around block / beam / underline variants whenever `imageBackgroundActive == true`. The dark frame is invisible against the regular background but guarantees the accent fill stays distinguishable from any wallpaper pixel underneath (Catppuccin mauve on a dark-purple wallpaper was the originating failure case). Non-key window's hollow-outline cursor is unchanged — the accent stroke is already its own contrast.
- [x] **`terminal.feed` / `terminal.exec` for alacritty backend** — added a narrow `TerminalCapable: CopadPanel` sub-protocol in `CopadPanel.swift` exposing the six socket-facing methods (`feedText`, `execCommand`, `terminalState`, `readScreen`, `history`, `context`). Both `TerminalViewController` (SwiftTerm) and `AlacrittyTerminalViewController` conform; WebView/plugin panels intentionally do not, so `panel as? TerminalCapable == nil` is the compile-time signal AppDelegate uses to emit `wrong_panel_type`. `PaneManager.activeTerminalPanel()` + `TabViewController.activeTerminalPanel` + `TabViewController.firstTerminalPanel()` provide the backend-agnostic accessors; the legacy `activeTerminal: TerminalViewController?` stays for SwiftTerm-only call sites (URL click handler, custom title setter, applyReportedCwd's SwiftTerm branch). AppDelegate gained `resolveTerminalPanel(params:vc:) -> Result<TerminalCapable, RPCError>` mirroring Linux's `resolve_terminal` in `copad-linux/src/socket.rs:1213` — id lookup → active panel → first-terminal fallback, with `not_found` / `wrong_panel_type` / `no_terminal` error codes verbatim. All six `terminal.*` cases route through it; webview/plugin active without an id now surfaces the proper error envelope instead of silent ok:true. **v1 limit:** alacritty `terminal.history` returns viewport-only content (same as `terminal.read`) — `copad-term` FFI doesn't expose scrollback yet. Shape stays identical to SwiftTerm's `terminal.history` (`{text, lines_requested, rows, cols}`) so callers don't fork on backend. Follow-up tracked below.
- [x] **`copad_term_history(handle, lines)` FFI for true alacritty scrollback** — added `copad_term_history(handle: *mut CopadHandle, lines: usize) -> *mut CopadString` in `copad-term/src/lib.rs` returning the last `lines` scrollback rows above the viewport top as plain text (`\n` between rows, NUL → space, no trailing newline). Internals extracted to `fn read_history_text(grid, cols, lines)` so the row-walking logic is unit-testable on a synthetic `Term<VoidListener>` driven by `<Term as Handler>::input` / `linefeed` / `carriage_return` (no PTY EventLoop). 6 unit tests in `mod history_tests` cover empty / zero-lines / clamping-to-history-size / oldest-first-ordering / NUL-padding / no-trailing-newline. C declaration mirrored in `Sources/CCopadTerm/include/copad_term.h`. Swift `CopadTermFFI.Handle.history(lines:) -> String?` follows the `selectionString` pattern (defer `copad_string_destroy`, copy bytes out) but returns `""` instead of `nil` for empty CopadStrings so the JSON shape stays stable. `AlacrittyTerminalViewController.history(lines:)` now routes through the FFI; viewport-rerun fallback removed. Shape unchanged: `{text, lines_requested, rows, cols}` — matches SwiftTerm, no `scrollback_supported` field.

---

## B. Linux-parity catch-up

Linux landed these; macOS hasn't ported yet.

> **Core-unify follow-up (commits `3e7aae8` → `d7d5eb8`).** B1 (Session persistence) graduated past a simple port — the Swift wire model + Linux Rust impl have since been consolidated into `copad-core/src/session.rs` and reached on macOS via `copad_ffi_session_*`. Same pattern applied to theme (Phase 1B), wallpaper rotation (1C), config-reload semantics (2A), and plugin manifest validation (2B). See [decisions.md #44](./decisions.md#44-core-unify-via-copad-ffi--shared-wire-formats--validation-between-linux-and-macos). Remaining B items below stay as straight ports — they don't have a shared schema worth lifting into core (yet).

- [x] **Session persistence** — `copad-macos/Sources/Copad/Session.swift` ports the Linux schema (`Snapshot` / `TabSnap` / tag-flat `SplitSnap` / lowercase `SplitOrientation`). `applicationWillTerminate` snapshots and writes (`Session.save`) or clears (`Session.clear` when no terminal tabs left); `applicationDidFinishLaunching` reads and replays via `TabViewController.restoreSession` before the daemon starts. File at `~/Library/Application Support/copad/session.json` (matches `copad_core::paths::state_dir()`'s macOS branch). v1 limits: divider position not tracked (`position: 0` sentinel, EqualSplitView re-equalizes on restore). alacritty backend live cwd via a 3-layer fallback (Linux/VTE parity): (1) **shell-hook reported cwd** — `~/.config/copad/shell-hooks/copad-cwd.{zsh,bash,fish}` installed by `install-macos.sh`; user adds `source ...` to their rc file once; hook calls `coctl call panel.report_cwd` on every chpwd, dispatched to the `panel.report_cwd` action registered in `AppDelegate`. (2) `proc_pidinfo(PROC_PIDVNODEPATHINFO)` on the PTY child PID — works on Apple-signed / entitled builds, EPERM on hardened-runtime un-entitled `install-macos.sh` builds. (3) spawn-time `initialCwd`. `currentCwd` reads them in that order. alacritty custom titles drop across the cycle (`setCustomTitle` only flows to SwiftTerm path).
- [x] **GUI in-process `notify.show` registration** — Swift `ActionRegistry.registerSilent("notify.show", …)` in `AppDelegate.applicationDidFinishLaunching`. Routes to `copad_ffi_notify_show(title, body, level)` which wraps `copad_core::notifier::platform_notifier()` (osascript on macOS) — same notifier the daemon uses, so behavior is identical whether `copadd` is up or not. Spawn runs on `DispatchQueue.global(qos: .userInitiated)` to keep the main thread off the ~10 ms osascript spawn; completion bounces back to main. LAST_ERROR read on the spawning thread before the main-queue hop. Params validation matches Linux (`title` required non-empty, `body` optional, `level` ∈ {info, warn, error}).
- [x] **Swift `BusEvent.origin` field** — `Origin` enum + `BusEvent.origin` in `EventBus.swift`; `broadcast(origin:)` defaults to `.internal`; `DaemonClient.handleLine` parses incoming `origin` field (`.internal` for absent / unknown — safe default matching serde); `inboundEventHandler` signature carries it through to `AppDelegate`. Closes the wire propagation gap from decisions.md #37 by also adding `origin: Origin` to `copad_core::protocol::Event` and threading it through both daemon→GUI forwarders (`gui_registry::forwarder_loop` + `socket.rs` event.subscribe loop) so daemon-side `External` tags survive the bridge.
- [x] **`copadd --version` short-circuit** — argv check moved to the first lines of `main()`, ahead of env_logger init and `prepare_socket_path`. `--version` / `-V` prints `copadd <CARGO_PKG_VERSION>` and exits 0; `--help` / `-h` prints usage + env-var reference (`COPAD_SOCKET` / `COPAD_HOST_TRIGGERS` / `COPAD_E2E_ACTIONS`) and exits 0. A second invocation while one daemon is bound no longer errors with `"socket already bound"` for these read-only flags.

---

## C. Phase 10b — remove SwiftTerm path ✅

Shipped 2026-06-05. Alacritty dogfood window: 2026-05-18 default flip
(`e0ddf31`) + ~3 weeks of daily use including the same-day Cycle 1–4
exercise (CTLine cache, damage rect, drag-drop, find UI). No regressions
surfaced; Cmd+F regression risk was closed by adding the alacritty find
bar in Cycle 4 BEFORE the SwiftTerm path was removed.

Deletions:
- `copad-macos/Sources/Copad/TerminalViewController.swift` — gone.
- `SwiftTerm` package dep in `Package.swift` — gone.
- `RendererBackend` enum in `Config.swift` — gone. `RendererSection.backend`
  is parsed but ignored so stale `[renderer] backend = "swiftterm"`
  configs don't fail to load.
- `PaneManager.makeTerminalPanel`: single-path now (always alacritty).
- `URLClickHelper.findURL(at:in:)` — gone (SwiftTerm-specific). Static
  helpers (`urlRegex`, `trimURLTrailingPunctuation`) retained — alacritty
  renderer uses them.
- `AppDelegate.installEditKeyMonitor` — gone (SwiftTerm-only Cmd/Option
  + Backspace/Delete bridge; alacritty handles these natively).
- `AppDelegate.performFindPanelAction` — single-path now (always
  alacritty's bar).
- `TabViewController.activeTerminal` (SwiftTerm-typed getter) — gone;
  use the protocol-typed `activeTerminalPanel`.

New protocol surface:
- `TerminalCapable.customTitle` + `setCustomTitle` so `tab.rename`
  still works on the alacritty backend (was SwiftTerm-only).

Cross-platform `panel.exited` event preserved — added
`copad_term_take_child_exit` FFI in the same commit so alacritty
panes broadcast `panel.exited` on shell termination (was
SwiftTerm-only via `processTerminated`). copad-core's
`ContextService` cleanup contract honored across both backends.

Pane auto-close on shell exit landed in a follow-up commit:
`PaneManager.wirePanel` now sets `AlacrittyRenderView.onChildExited`
to a closure that calls `closePanel(self)` when
`[terminal] close_on_exit = true` (default). The cascade up to tab
close / window close reuses the existing
`onLastPaneClosed` → `TabViewController.closeTab(at:)` chain.
`close_on_exit = false` keeps the dead-PTY viewport visible so
the user can read the exit message — Linux honors the same key in
`tabs.rs::handle_panel_exit`.

---

## D. Cross-platform work that lands for macOS automatically

Tracked in [`harness-integration.md`](./harness-integration.md); these are pure-Rust changes on the daemon, so the macOS daemon (auto-spawned via the LaunchAgent shipped in commit `b93bc0b`) picks them up for free.

- [x] **Step 10 Option A slice 2** — `plugins/claude/` first-party plugin shipping `claude.last_handoff` / `claude.list_sessions` / `claude.session_state` / `claude.list_dirty` actions. Read-only over the existing `~/.claude/` harness artifacts (`handoffs/latest.md`, `projects/<encoded-cwd>/<session-id>.jsonl`, `state/dirty-<session-id>.log`). Lazy activation (`activation = "onAction:claude.*"`) — supervisor spawns the process on first matching call. `COPAD_CLAUDE_DIR` env override for tests. 8 unit tests + direct stdio e2e covering all 4 actions. Cross-platform: same plugin binary serves Linux + macOS. `install-macos.sh` MACOS_PLUGINS list extended.
- [ ] **Step 11 Option I** — cron triggers (`[[triggers]]` cron field, scheduler, missed-run policy).
- [ ] **Steps 12–16** — life-assistant bridge, monitor panel, browser / codex adapters, `/handoff` + `/catchup` ↔ KB.

---

## E. Test hygiene

- [x] **`paths::tests::*` — 7 failures on macOS** root-caused: not actually an env-var parallel race. `daemon_socket_returns_none_for_untrusted_runtime_dir` assumed `runtime_dir()` honors `XDG_RUNTIME_DIR`, but `runtime_dir()`'s macOS branch is hard-wired to `~/Library/Caches/copad/` (a sandboxed user dir that `is_trusted_dir` accepts) and ignores XDG entirely. The test asserts `None` → panics on macOS → `ENV_LOCK.lock().unwrap()` poisons → every other env-touching test cascade-fails. Two-layer fix: `#[cfg(target_os = "linux")]` on the XDG-dependent test, and a `lock_env()` helper that does `unwrap_or_else(PoisonError::into_inner)` so any future test panic doesn't cascade. `cargo test -p copad-core` now reports 220 passed / 0 failed / 0 ignored on macOS for the first time. No new dev-deps.

---

## Notes on prioritization

- **Pick A1 (`terminal.output`) first** if AI-agent flows are the next dogfooding focus — it's the single biggest unlocked feature from the renderer flip.
- **Pick A2 (mouse click forwarding)** if tmux usage is heavy — it's the most user-visible deferred item from the wheel-forwarding commit.
- **Pick B1 (session persistence)** if the user is restarting Copad often (the lack of restore is felt every time).
- **C (SwiftTerm removal)** is a code-simplification win, not a feature; wait until the dogfooding window closes.
- **D items** are sequenced by harness-integration.md, not by this doc — track there.
