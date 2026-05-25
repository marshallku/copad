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

- [x] **`terminal.output` event** — wired in `AlacrittyTerminalViewController.swift` via a `sendInput` helper that wraps every keyboard / paste path (`insertText`, `doCommand`, control combos, command-key shortcuts, paste). Mirrors Linux's `nestty-linux/src/tabs.rs` VTE `connect_commit` hook (the kind name "output" follows the terminal-widget perspective: bytes going OUT of the widget toward the PTY). Mouse-mode wheel forwarding intentionally bypasses the helper because VTE excludes mouse from `commit`. Programmatic `initialInput` (e.g. `claude.start` seeding) also bypasses — matches Linux's `feed_child` behavior. Verified: typed letter `"x"` → `{"text":"x"}`; Return → `{"text":"\r"}`; Up arrow → `{"text":"[A"}`.
- [x] **Mouse click/drag forwarding** for mouse-mode TUIs. CLICK + DRAG levels wired (commit follows wheel-forwarding `5420ef5` pattern). New FFI `nestty_term_mouse_report_level` exposes the CLICK / DRAG / MOTION tier so the renderer can gate drag-as-motion correctly. `AlacrittyTerminalViewController` overrides left / right / middle button down+up+dragged with a shared `forwardMouseEvent` helper that encodes via SGR (preferred) or legacy/UTF8. Shift held continues to bypass forwarding (host selection wins). Motion events dedupe at grid-cell granularity. **MOTION-level (`\e[?1003h` bare-cursor motion)** is not wired — rare in practice (ncurses hover-trackers); needs `acceptsMouseMovedEvents` plumbing if a TUI ever asks for it.
- [x] **DSR / DA reply forwarding** — `nestty-term` `NesttyListener` now matches on `Event::PtyWrite` and forwards the bytes via `EventLoopSender::send(Msg::Input(…))` back to the child PTY. alacritty_terminal already formats DSR (`CSI 6n` → `\e[<row>;<col>R`) and DA (`CSI 0c` → `\e[?6c`); we were dropping both on the listener floor. Sender is late-bound (`set_sender` after `EventLoop::channel()`); PtyWrite events before injection drop silently (none can fire that early in practice). Linux unaffected (VTE handles DSR/DA in-widget).
- [x] **OSC 4 / 10 / 11 / 12 color queries** — `NesttyListener` now holds an `Arc<Mutex<HashMap<usize, Rgb>>>` palette. `Event::ColorRequest(idx, format_reply)` arm looks up the index, calls the alacritty-provided formatter (which produces the OSC-formatted reply string), and forwards via the same `EventLoopSender` path A3 wired. New FFI `nestty_term_set_palette_entry(handle, idx, r, g, b)`; Swift `NesttyTermFFI.Handle.applyPaletteFromTheme(_:)` pushes the 16 ANSI colors plus `foreground` (256), `background` (257), and `accent`-as-cursor (258). Called from `AlacrittyTerminalViewController.startIfNeeded` (initial push) and `applyTheme` (hot-reload). Indices the host doesn't populate stay silent — apps fall back to built-in defaults rather than getting an alacritty palette color we don't actually render. `NesttyListener::new` pre-seeds Catppuccin Mocha (matching `nestty_core::theme::Theme::default()`) so any OSC query firing in the race window between `nestty_term_create` returning and `applyPaletteFromTheme` running still gets a coherent answer; the host overwrite on `applyTheme` takes precedence.
- [ ] **NSImage async loading** — wallpaper file open on main thread can stall during Gatekeeper / XProtect scan (Phase 3.5 known limitation surfaced during testing). Move `NSImage(contentsOfFile:)` to a background queue + progressive reveal once decode finishes.
- [x] **Cmd+/- zoom on alacritty path** — added via a tiny `Zoomable` protocol (in `NesttyPanel.swift`) that both terminal VCs conform to. `AlacrittyTerminalViewController` tracks `currentFontSize` / `configFontSize` / `currentFontFamily` (mirror of SwiftTerm path), implements `zoomIn` / `zoomOut` / `zoomReset` with the same step (±1) + clamp (6..72) values. Each step routes through `applyFontInternal`, which reuses the existing `setFont` → cell-metrics recompute → `termHandle.resize()` chain so the PTY gets SIGWINCH for shell re-wrap. `applyFont` (config hot-reload) now preserves an in-flight zoom level instead of clobbering it. `AppDelegate` zoom actions dispatch through `tabVC?.activeZoomable?` (polymorphic across the two backends; webview / plugin panes silently no-op).
- [ ] **Block selection (Cmd+Option+drag)** — iTerm2 convention. `alacritty_terminal::selection::SelectionType::Block` is already supported; renderer never picks it. Wire modifier-flag check in `mouseDown` / `mouseDragged`.
- [x] **Cursor visibility polish on busy wallpapers** — `drawCursor` in `AlacrittyRenderView` now adds a 1-px `theme.background` outline around block / beam / underline variants whenever `imageBackgroundActive == true`. The dark frame is invisible against the regular background but guarantees the accent fill stays distinguishable from any wallpaper pixel underneath (Catppuccin mauve on a dark-purple wallpaper was the originating failure case). Non-key window's hollow-outline cursor is unchanged — the accent stroke is already its own contrast.

---

## B. Linux-parity catch-up

Linux landed these; macOS hasn't ported yet.

> **Core-unify follow-up (commits `3e7aae8` → `d7d5eb8`).** B1 (Session persistence) graduated past a simple port — the Swift wire model + Linux Rust impl have since been consolidated into `nestty-core/src/session.rs` and reached on macOS via `nestty_ffi_session_*`. Same pattern applied to theme (Phase 1B), wallpaper rotation (1C), config-reload semantics (2A), and plugin manifest validation (2B). See [decisions.md #44](./decisions.md#44-core-unify-via-nestty-ffi--shared-wire-formats--validation-between-linux-and-macos). Remaining B items below stay as straight ports — they don't have a shared schema worth lifting into core (yet).

- [x] **Session persistence** — `nestty-macos/Sources/Nestty/Session.swift` ports the Linux schema (`Snapshot` / `TabSnap` / tag-flat `SplitSnap` / lowercase `SplitOrientation`). `applicationWillTerminate` snapshots and writes (`Session.save`) or clears (`Session.clear` when no terminal tabs left); `applicationDidFinishLaunching` reads and replays via `TabViewController.restoreSession` before the daemon starts. File at `~/Library/Application Support/nestty/session.json` (matches `nestty_core::paths::state_dir()`'s macOS branch). v1 limits: divider position not tracked (`position: 0` sentinel, EqualSplitView re-equalizes on restore), alacritty backend cwd is initialCwd-only (no OSC 7 surface), alacritty custom titles drop across the cycle (`setCustomTitle` only flows to SwiftTerm path).
- [x] **GUI in-process `notify.show` registration** — Swift `ActionRegistry.registerSilent("notify.show", …)` in `AppDelegate.applicationDidFinishLaunching`. Routes to `nestty_ffi_notify_show(title, body, level)` which wraps `nestty_core::notifier::platform_notifier()` (osascript on macOS) — same notifier the daemon uses, so behavior is identical whether `nesttyd` is up or not. Spawn runs on `DispatchQueue.global(qos: .userInitiated)` to keep the main thread off the ~10 ms osascript spawn; completion bounces back to main. LAST_ERROR read on the spawning thread before the main-queue hop. Params validation matches Linux (`title` required non-empty, `body` optional, `level` ∈ {info, warn, error}).
- [x] **Swift `BusEvent.origin` field** — `Origin` enum + `BusEvent.origin` in `EventBus.swift`; `broadcast(origin:)` defaults to `.internal`; `DaemonClient.handleLine` parses incoming `origin` field (`.internal` for absent / unknown — safe default matching serde); `inboundEventHandler` signature carries it through to `AppDelegate`. Closes the wire propagation gap from decisions.md #37 by also adding `origin: Origin` to `nestty_core::protocol::Event` and threading it through both daemon→GUI forwarders (`gui_registry::forwarder_loop` + `socket.rs` event.subscribe loop) so daemon-side `External` tags survive the bridge.
- [x] **`nesttyd --version` short-circuit** — argv check moved to the first lines of `main()`, ahead of env_logger init and `prepare_socket_path`. `--version` / `-V` prints `nesttyd <CARGO_PKG_VERSION>` and exits 0; `--help` / `-h` prints usage + env-var reference (`NESTTY_SOCKET` / `NESTTY_HOST_TRIGGERS` / `NESTTY_E2E_ACTIONS`) and exits 0. A second invocation while one daemon is bound no longer errors with `"socket already bound"` for these read-only flags.

---

## C. Phase 10b — remove SwiftTerm path

Once the alacritty backend has accumulated enough dogfooding time without
regressions (target: 2–4 weeks of daily use post-Phase-10a), delete the
SwiftTerm path entirely:

- `nestty-macos/Sources/Nestty/TerminalViewController.swift`
- `SwiftTerm` package dependency in `nestty-macos/Package.swift`
- `RendererBackend.swiftterm` enum case in `nestty-macos/Sources/Nestty/Config.swift` (and the fallback branch in `RendererBackend.parse`)
- Backend-switching branch in `PaneManager.makeTerminalPanel`
- The `"swiftterm"` mention in the install-macos.sh footer + any decision docs that point users at the fallback

Gate before deleting: `swift build -c release` clean, `cargo test -p nestty-term --tests` green, and at least one explicit ping in this doc saying "Phase 10b unblocked".

---

## D. Cross-platform work that lands for macOS automatically

Tracked in [`harness-integration.md`](./harness-integration.md); these are pure-Rust changes on the daemon, so the macOS daemon (auto-spawned via the LaunchAgent shipped in commit `b93bc0b`) picks them up for free.

- [ ] **Step 10 Option A slice 2** — `claude` plugin: `claude.session_state` / `claude.list_dirty` / `claude.last_handoff` / `claude.list_sessions` actions. Event publish from hooks already wired by `install-claude-hooks.sh` (commit `adc7b0c`).
- [ ] **Step 11 Option I** — cron triggers (`[[triggers]]` cron field, scheduler, missed-run policy).
- [ ] **Steps 12–16** — life-assistant bridge, monitor panel, browser / codex adapters, `/handoff` + `/catchup` ↔ KB.

---

## E. Test hygiene

- [ ] **`paths::tests::*` — 7 failures on macOS** with `--test-threads=1` and parallel. Env-var parallel race; pre-existing on `master`, blocks `cargo test -p nestty-core --lib` from being green on macOS CI. Either gate the env-touching tests behind `serial_test::serial` (extra dev-dep) or shell out to a subprocess per test for the env-scoped checks (no new deps).

---

## Notes on prioritization

- **Pick A1 (`terminal.output`) first** if AI-agent flows are the next dogfooding focus — it's the single biggest unlocked feature from the renderer flip.
- **Pick A2 (mouse click forwarding)** if tmux usage is heavy — it's the most user-visible deferred item from the wheel-forwarding commit.
- **Pick B1 (session persistence)** if the user is restarting Nestty often (the lack of restore is felt every time).
- **C (SwiftTerm removal)** is a code-simplification win, not a feature; wait until the dogfooding window closes.
- **D items** are sequenced by harness-integration.md, not by this doc — track there.
