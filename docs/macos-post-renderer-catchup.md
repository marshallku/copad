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
- [ ] **DSR (Device Status Report) response** — nvim emits `"Did not detect DSR response from terminal"` on startup because we ignore `CSI 6n` (cursor position) and `CSI 0c` (terminal attributes). Two single-digit reply handlers in `nestty-term`'s input loop.
- [ ] **NSImage async loading** — wallpaper file open on main thread can stall during Gatekeeper / XProtect scan (Phase 3.5 known limitation surfaced during testing). Move `NSImage(contentsOfFile:)` to a background queue + progressive reveal once decode finishes.
- [x] **Cmd+/- zoom on alacritty path** — added via a tiny `Zoomable` protocol (in `NesttyPanel.swift`) that both terminal VCs conform to. `AlacrittyTerminalViewController` tracks `currentFontSize` / `configFontSize` / `currentFontFamily` (mirror of SwiftTerm path), implements `zoomIn` / `zoomOut` / `zoomReset` with the same step (±1) + clamp (6..72) values. Each step routes through `applyFontInternal`, which reuses the existing `setFont` → cell-metrics recompute → `termHandle.resize()` chain so the PTY gets SIGWINCH for shell re-wrap. `applyFont` (config hot-reload) now preserves an in-flight zoom level instead of clobbering it. `AppDelegate` zoom actions dispatch through `tabVC?.activeZoomable?` (polymorphic across the two backends; webview / plugin panes silently no-op).
- [ ] **Block selection (Cmd+Option+drag)** — iTerm2 convention. `alacritty_terminal::selection::SelectionType::Block` is already supported; renderer never picks it. Wire modifier-flag check in `mouseDown` / `mouseDragged`.
- [ ] **Cursor visibility polish on busy wallpapers** — pink accent block can be low-contrast against image backgrounds (Catppuccin mauve over a dark-purple wallpaper). Consider drop-shadow under the block fill, or a thin outer stroke on the focused-fill variant.

---

## B. Linux-parity catch-up

Linux landed these; macOS hasn't ported yet.

- [x] **Session persistence** — `nestty-macos/Sources/Nestty/Session.swift` ports the Linux schema (`Snapshot` / `TabSnap` / tag-flat `SplitSnap` / lowercase `SplitOrientation`). `applicationWillTerminate` snapshots and writes (`Session.save`) or clears (`Session.clear` when no terminal tabs left); `applicationDidFinishLaunching` reads and replays via `TabViewController.restoreSession` before the daemon starts. File at `~/Library/Application Support/nestty/session.json` (matches `nestty_core::paths::state_dir()`'s macOS branch). v1 limits: divider position not tracked (`position: 0` sentinel, EqualSplitView re-equalizes on restore), alacritty backend cwd is initialCwd-only (no OSC 7 surface), alacritty custom titles drop across the cycle (`setCustomTitle` only flows to SwiftTerm path).
- [ ] **GUI in-process `notify.show` registration** — daemon registers it (commit `895c85e` via `Notifier` trait + `osascript` subprocess); Swift's in-process `ActionRegistry` doesn't. `nestctl call notify.show` works only when the daemon is running. Mirror `nestty-linux/src/window.rs:218`'s `register_blocking_silent` in Swift, calling `NSUserNotification` (or `UserNotifications.UNUserNotificationCenter`).
- [ ] **Swift `BusEvent.origin` field** — trust-boundary parity. Origin tagging shipped on the Rust side (commit `d03a01a`, decisions.md #37); Swift's `BusEvent` struct + wire deserialize don't carry it. Limits Swift-side privileged-action gating. Listed as known gap in roadmap Phase 21 step 9.
- [ ] **`nesttyd --version` short-circuit** — daemon binds the socket before parsing argv, so a second invocation while one is running errors with `"socket already bound"` even for read-only flags. Parse `--version` / `--help` first, exit without bind.

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
