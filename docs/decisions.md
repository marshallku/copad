# Technical Decisions

## 1. Tauri v2 Abandoned → Native Platform UIs

**Problem:** Tauri IPC introduced noticeable input latency in the terminal. Every keypress went through JS → Tauri invoke → Rust → PTY, and output went PTY → Rust → Tauri event → JS → xterm.js. The round-trip was perceptible.

**Decision:** Switched to platform-native UIs with a shared Rust core:

- Linux: GTK4 + VTE4 (VTE handles PTY internally, zero IPC overhead)
- macOS: Swift/AppKit (SwiftTerm or Ghostty embedding, TBD)

**Tradeoff:** More code per platform, but terminal responsiveness is non-negotiable.

## 2. VTE Handles PTY on Linux

**Rationale:** VTE has its own optimized PTY management. Using `portable-pty` alongside VTE would mean double PTY handling. Let VTE do what it does best.

**Consequence:** `copad-core/pty.rs` and `state.rs` were removed — both platforms handle PTY natively (VTE on Linux, SwiftTerm on macOS).

## 3. Unix Socket for IPC (D-Bus Removed)

**Original:** D-Bus was used for background control (SetBackground, ClearBackground, SetTint). A Unix socket server was later added for richer control (50+ commands).

**Decision:** Removed D-Bus entirely. The socket API is the sole IPC mechanism on all platforms. D-Bus only had 3 background methods, all duplicated by socket commands. No external consumers existed.

**GTK thread safety:** Socket server threads use `mpsc::channel` + `glib::timeout_add_local(50ms)` polling to safely dispatch commands on the GTK main thread.

## 4. Window-level Background Compositing

**Stack:** `bg_picture` (window-overlay child) → `tint_overlay` (overlay) → layout box (overlay) → notebook → panels (terminal / plugin webview / external webview, all transparent)

`BackgroundLayer` (in `copad-linux/src/background.rs`) lives at the window level. The root `gtk4::Overlay` has the `bg_picture` as its base child and adds the tint plus the actual UI layout as overlays. Every panel sits over this single image so the background is consistent across tabs (terminals, todo, etc.) instead of being painted per-terminal.

**Critical details to keep the layer visible through every panel:**

1. VTE: `terminal.set_clear_background(false)` + bg color `RGBA(0,0,0,0)` (always — not conditional on whether an image is loaded). VTE otherwise paints its own opaque background and hides the layer.
2. WebKit (plugin + external webview): `webview.set_background_color(RGBA(0,0,0,0))` so blank pages don't paint opaque white over the layer.
3. CSS: `notebook header`, `notebook > stack`, `.copad-statusbar` are all `background-color: transparent`. Plugin user CSS sets `html, body { background-color: transparent }`.
4. `bg_picture` and `tint_overlay` use `set_can_target(false)` so input events pass through to the panels above.

When no image is configured, `bg_picture` is hidden and the window's CSS `window { background-color: <theme.background> }` provides the solid theme color underneath.

**Why moved here from per-`TerminalPanel`:** the previous design only rendered the image inside the first terminal's overlay, so opening a non-terminal panel (todo plugin, webview) hid the image entirely, and split terminals each rendered their own copy with independent positioning. Window-level layer fixes both.

## 5. Binary Names: copad + coctl

**Problem:** Both copad-linux and copad-cli had `[[bin]] name = "copad"`, causing Cargo output filename collision.

**Decision:** CLI binary renamed to `coctl` (follows kubectl, sysctl naming convention).

## 6. Theme System

**Design:** Themes are defined as `Theme` structs in `copad-core/theme.rs` with semantic color slots (foreground, background, 16-color palette, surface/overlay/accent UI colors). 10 built-in themes are embedded. All UI components (terminal, tab bar, search bar, webview URL bar, window background) use theme colors via CSS generation functions.

**Config:** `[theme] name = "catppuccin-mocha"` selects the active theme. Hot-reloads on config change.

**Built-in themes:** catppuccin-mocha (default), catppuccin-latte, catppuccin-frappe, catppuccin-macchiato, dracula, nord, tokyo-night, gruvbox-dark, one-dark, solarized-dark.

## 7. cmux V2 Protocol for Socket Communication

**Format:** Newline-delimited JSON with UUID request IDs.
**Reference:** ~/dev/cmux/ (Marshall's macOS terminal multiplexer)

This protocol is used by both coctl and the copad-linux socket server. D-Bus remains for system integration (background control), while the socket API handles all rich control (tabs, splits, webview, terminal agent, approval workflow).

## 8. Forced Dark Theme

**Problem:** When VTE background is transparent (for bg images) and no image is loaded yet, the system GTK theme shows through. On light themes this makes the terminal white.

**Fix:** Force dark theme in `app.rs` via `set_gtk_application_prefer_dark_theme(true)` + CSS `window { background-color: #1e1e2e; }`.

## 9. Rust Edition 2024

Using the latest Rust edition. No compatibility concerns since the project is new.

## 10. In-Terminal Search via VTE Regex

**Problem:** Popular terminals (Ghostty, Kitty) lack built-in Ctrl+F search, requiring piping through external tools.

**Decision:** Implemented search using VTE4's built-in `search_set_regex` / `search_find_next` / `search_find_previous` with PCRE2 regex. Search bar is a `gtk4::Box` overlay at the bottom of each terminal panel.

**UX details:**

- Search text is preserved when closing, but fully selected on reopen (type to replace, Enter to reuse)
- `glib::idle_add_local_once` is needed for `select_region` — GTK4 Entry ignores selection before focus is fully settled

## 12. macOS Split Panes: NSSplitViewDelegate for Equal Initial Sizing

**Problem:** Getting `NSSplitView` to start at exactly 50/50 on initial layout is unreliable. Two failed approaches:

1. `DispatchQueue.main.asyncAfter(deadline: .now() + 0.05)` + `setPosition`: timing is unpredictable. The timer may fire before layout resolves (position ignored) or after a subsequent split has already started.
2. `override func layout()` + `setPosition`: NSSplitView calls `resizeSubviews` (which commits subview frames) before calling `layout()`. By the time `layout()` fires, the wrong frames are already in place.

**Decision:** Use `NSSplitViewDelegate.splitView(_:resizeSubviewsWithOldSize:)`. This delegate method is the exact hook where NSSplitView asks "how should I size my subviews?" — set frames directly here. An `initialSizeSet` flag ensures this only runs once per `EqualSplitView` instance; subsequent calls fall back to `adjustSubviews()` to allow user dragging.

## 13. macOS Split Panes: Hierarchical (Not Flat) Splitting

**Problem:** When splitting a pane that is already part of a split, two approaches are possible:

- **Flat:** Add the new pane as a sibling in the parent branch → all siblings resize equally. If you have [A|B] and split A, result is [A|newPane|B] with each pane at 33%.
- **Hierarchical:** Replace A's leaf with a new 2-child branch → only A's space is divided. If you have [A|B] and split A, result is [(A|newPane)|B] with A and newPane each at 25%, B untouched at 50%.

**Decision:** Always use hierarchical splitting. The flat approach is surprising because splitting one pane causes other panes to shrink. "Split this pane in half" is a more intuitive mental model than "add a pane to this group."

**Implementation:** `SplitNode.splitting(_:with:orientation:)` always wraps the target leaf in a new 2-child branch, regardless of the parent branch's orientation. `removing(_:)` collapses a branch to its single remaining child when a pane is closed.

## 14. macOS: Async Socket Handler via DispatchSemaphore + ResultBox

**Problem:** Some socket commands (e.g. `webview.execute_js`, `webview.get_content`) get their results from WKWebView callbacks, which run on the main thread asynchronously after the initial dispatch. The socket thread needs to block until the result is available.

**Decision:** Changed `SocketServer.commandHandler` from a synchronous `(method, params) -> Any?` signature to a completion-based `(method, params, completion: (Any?) -> Void) -> Void`. The socket thread blocks on a `DispatchSemaphore`. The main thread calls completion (possibly from a WKWebView callback), which stores the value in a `ResultBox: @unchecked Sendable` and signals the semaphore.

**Why `ResultBox`:** Swift 6 strict concurrency rejects capturing a `var` local in an `@MainActor` closure sent to another thread. A `final class` box with `@unchecked Sendable` is safe because the semaphore serializes all access — the socket thread never reads until after the signal.

## 15. macOS: CopadPanel Protocol for Mixed Terminal+WebView Splits

**Problem:** `SplitNode` and `PaneManager` were typed to `TerminalViewController`. Adding WebView panels required either a union type or polymorphism.

**Decision:** Introduced `CopadPanel: AnyObject` protocol with common interface (`view`, `currentTitle`, `startIfNeeded()`, `applyBackground`, etc.). `SplitNode` uses `case leaf(any CopadPanel)`. Identity comparison uses `ObjectIdentifier` since `any CopadPanel` is not `Equatable`.

**Tradeoff:** `any CopadPanel` existentials have a small overhead vs. concrete types, but panel operations are infrequent (split/close/focus) so the overhead is negligible.

## 11. Configurable Tab Position

**Decision:** Tab bar position (`top`, `bottom`, `left`, `right`) is configurable via `[tabs] position` in config. Uses `gtk4::Notebook::set_tab_pos()`. Hot-reloads on config change.

**Rationale:** Vertical tabs (left/right) make better use of widescreen displays and are preferred by some users.

## 16. Project Scope: Personal Workflow Runtime, Not Just Terminal

**Problem:** Original framing was "terminal-centric programmable workspace" — a terminal with extensions for browser panels and AI. As integration scope grew to encompass calendars, messengers, knowledge bases, and trigger-driven automation, the "terminal with some extras" framing became inadequate. Every new integration was adding ad-hoc wiring between its source, the UI surfaces that render it, and the actions that operate on it.

**Decision:** Reframe copad as a **personal workflow runtime** that surfaces through a terminal. `copad-core` gains three central abstractions — Event Bus, Action Registry, Context Service — and existing features (socket event stream, plugin system, AI agent integration) consume them rather than reimplementing per-feature wiring.

**Tradeoff:** Larger architectural surface and more upfront design. The alternative — adding each integration ad-hoc — produces n×m wiring between n sources and m consumers (UI panels, triggers, AI agent, future KB indexer). The three abstractions turn this into n + m.

**Scope guardrails:**

- Do not build service clients from scratch when a mature web UI exists. Embed in the existing WebView panel (`webkit6` on Linux, `WKWebView` on macOS).
- Implement native event streams only for persistent push (WebSocket gateways, webhooks). Everything else polls via provider.
- Knowledge base is the last layer, built on the three abstractions — not a parallel system.

**See:** [workflow-runtime.md](./workflow-runtime.md) for the abstraction design and first vertical PoC plan.

## 17. Plugin-First for External Integrations (Post-Phase 8)

**Problem:** ADR 16 reframed copad as a personal workflow runtime, with the implicit assumption that integrations like Calendar, Slack, KB, Notion, and LLM would land as modules in `copad-core`/`copad-linux`. As the integration surface grew it became clear this would make copad a kitchen-sink monolith, lock the user to specific backends (e.g. KB always means `~/docs` if KB is built in), and make third-party contributions painful. The user explicitly raised the comparison to VSCode-style extensions.

**Decision:** Every external integration is a **service plugin** — a long-running supervised subprocess that speaks newline-delimited JSON over stdin/stdout and registers itself with `copad-core` via a manifest-declared `[[services]]` section. The KB action protocol (and similar contracts) lives in `copad-core` as documented protocol; implementations live in plugins. `copad-core` and `copad-linux` own the runtime primitives only — Event Bus, Action Registry, Trigger Engine, Context Service, Plugin Loader.

**Tradeoff:** First-call latency of a few hundred milliseconds (lazy plugin spawn) and IPC overhead per action call vs in-process performance. Acceptable at personal scale; the gain is extensibility, swappability of backends, crash isolation, and language-agnostic plugin authoring. Subprocess + stdio is the dominant pattern across the editor/IDE ecosystem (VSCode language servers, Neovim remote plugins, LSP) so it carries proven integration patterns.

**Key sub-decisions** (with research validation):
- **Lazy activation** like VSCode `activationEvents`. Initial instinct toward eager startup was wrong — research showed mature systems uniformly chose lazy.
- **LSP-style initialization handshake** for capability and version negotiation.
- **Manifest-declared `provides`/`subscribes` as source of truth + lexical-name conflict resolution** at load time. The runtime `initialize` response is checked asymmetrically against the manifest — applied identically to BOTH `provides` AND `subscribes`: subset allowed (degraded mode — copad wires up only what runtime declared); superset rejected with warning (extras dropped, plugin keeps running for manifest-approved set). The pre-spawn ownership analysis stays accurate. Two enabled plugins with overlapping `provides` resolve via alphabetical `[plugin].name` ordering (the existing canonical plugin identifier from [plugins.md](./plugins.md)) — deterministic across runs and filesystems. User controls precedence by enabling/disabling plugins, or by editing the manifest `[plugin].name` if a finer override is needed.
- **Subprocess + stdio + newline-JSON**, NOT WASM yet. WASM (Zed's choice) adds Wasmtime runtime and WIT compilation barriers that personal-scale copad doesn't yet need.

**See:** [service-plugins.md](./service-plugins.md) for full vision, decisions, rationale, research sources, and the Phase 9–18 roadmap.

## 18. Service Plugin Supervisor Threading: One Reader, One Writer, Workers for Recursive Calls

**Problem:** A service plugin can call back into copad via `action.invoke` — a registered action handler that the registry might dispatch to *another* service plugin (or even back to the same one). If the reader thread that just received that inbound `action.invoke` synchronously calls `registry.invoke`, and that handler resolves through `invoke_remote` on the same service, we deadlock: the response we're blocking on is the response that this very reader thread is responsible for delivering.

**Decision:** Per running service, supervise with three OS threads — one writer (drains outgoing channel into child stdin), one reader (parses child stdout, dispatches frames), one stderr-tail (logs). On every inbound `action.invoke` request, the reader spawns a short-lived worker thread that runs `registry.invoke` and sends the response. Notifications (`event.publish`, `log`) stay on the reader thread because they don't recurse.

For the action-handler side: `invoke_remote` blocks the calling thread on a oneshot channel up to the action timeout. Since the calling thread is the dispatcher (socket→GTK timer or trigger sink worker), this is acceptable. The supervisor's response routing is decoupled because `dispatch_invocation` spawns its own worker that owns the reply channel.

**Tradeoff:** Higher thread count per service (3 + 1 wait + 1 per `subscribes` glob, plus transient workers per inbound recursive call). Justified at personal scale (a handful of services). The alternative — a single-threaded event loop with futures — would let us avoid threads but adds an async runtime dependency to `copad-linux` that nothing else needs yet.

**See:** `copad-linux/src/service_supervisor.rs` for the implementation.

## 19. Bounded Worker Pool for ActionRegistry (Phase 9.4)

**Problem:** `ActionRegistry::try_dispatch` was spawning a fresh OS thread per blocking handler call. Combined with the daemon's per-connection handler threads and the GUI client's per-Invoke workers from Step 4b, a burst of slow plugin calls + concurrent webview operations could blow the process thread count. Three rounds of codex-plan pressure-test surfaced that the *caller policy* — how long to wait when the pool is saturated — couldn't be uniform: daemon connection threads tolerate brief backpressure, but GTK pump / heartbeat reader paths cannot block at all without breaking the live-ness contract Step 4b established.

**Decision:** Replace the unbounded spawn with a bounded `ThreadPool` (crossbeam-channel, configurable workers + queue). Jobs implement a `Cancelable` trait — `run()` on a worker, `cancel()` synchronously on the caller's thread when the queue is full. Saturation surfaces as a new `overloaded` error code; `cancel()` also publishes `<action>.failed` so triggers waiting on completion don't hang. v1 wires the pool only from `copadd/main.rs`; `copad-linux`'s in-process registry and `gui_client.rs` reader stay on the legacy spawn path (explicit scope-out, follow-up sub-steps).

**Tradeoff:** Fixed upper bound on concurrency means burst load rejects requests during saturation rather than degrading via thread thrash. Acceptable because the alternative (spawn fallback under saturation) defeats the bound entirely. Explicit `pool.shutdown()` in `main.rs` guards against an Arc cycle (registry → handler closure → supervisor → registry) that could prevent automatic `Drop`. The `Cancelable` trait abstraction (vs raw `FnOnce`) is the only way the registry can keep ownership of its `Responder` across both `run` and `cancel` paths — boxing a `FnOnce` into a generic `Job` would orphan the responder on rejection.

**See:** `copad-core/src/thread_pool.rs` + `copad-core/src/action_registry.rs` (`DispatchJob` impl).

## 20. Daemon Is the Sole Plugin Host (Step 5b)

**Problem:** After Step 5a flipped the daemon-attached default for the GUI, the in-process `ServiceSupervisor` in `copad-linux` and the daemon's own optional supervisor (gated by `COPADD_HOST_PLUGINS`) both still existed. Running them simultaneously double-spawned plugins; running neither broke functionality. Keeping the GUI as the host meant the daemon-attached connection was decoration — anything CLI-facing or trigger-facing still bypassed it. Worse, an SSH session running copadd alone (no GUI) had no plugin host at all, defeating the headless story the daemon was meant to enable.

**Decision:** The daemon is the **sole** plugin host. Three rounds of codex-plan landed on this integrated scope:

1. `copadd/main.rs` always activates the supervisor — no env-var gate.
2. `copad-linux/src/window.rs` no longer constructs `ServiceSupervisor`. Plugin manifest discovery stays (needed for panel rendering, statusbar modules, command lists) but the lifecycle disappears.
3. A daemon→GUI **event bridge** preserves chained-workflow triggers: each registered GUI client gets a per-connection forwarder thread (`gui_registry::start_event_forwarder`) that drains the daemon's `EventBus` and writes wire `Event { type, data }` lines on the existing socket. The GUI's `gui_client` reader bridges those back into the local `EventBus` so the in-GUI `TriggerEngine` (which still owns triggers in Step 5b — daemon-side triggers are a 5b.2 follow-up) sees `<action>.completed` events from daemon-hosted plugins.
4. The GUI's per-instance socket dispatcher now **forwards** unmatched methods (anything not in the GUI-owned legacy match arms, not in the local `ActionRegistry`) to the daemon over a bounded `ThreadPool`. Worker-isolated so the GTK timer never blocks on a slow plugin reply; saturation surfaces as `overloaded` (same vocabulary as Phase 9.4); daemon-absent surfaces as a new `no_daemon` error code.
5. **Out of scope:** trigger engine relocation (5b.2), context service relocation (5b.2), ~~statusbar `[[modules]]` shell execution (5b.3)~~ — DONE in 5b.3, ~~legacy `plugin.<name>.<cmd>` shell command execution (5b.3)~~ — DONE in 5b.3, ~~GUI per-instance socket permission hardening (5b.4)~~ — DONE in 5b.4.

**Tradeoff:** The chained-trigger preservation requires a new "GUI auto-subscribes-all events" mechanism, doubling daemon event traffic per connected GUI. Acceptable at personal scale because (a) the wire is a local Unix socket, (b) auto-subscribe-all is what the protocol § Resolved decisions #1 spec promised back in Step 4. Forwarding unmatched methods means a service-plugin RPC initiated from inside a copad child shell now traverses three Unix-socket hops (coctl → GUI socket → daemon socket → plugin) instead of one. Local domain, but ~3× latency over the legacy in-process call. Acceptable for v1; Step 5b.3 (where coctl learns to talk to the daemon directly) collapses this back.

The C1 from plan-round-3 — `gui.register` ack vs first forwarded event race — is closed by deliberately starting `start_event_forwarder` AFTER the registration `Response` has been queued (`socket.rs::handle_gui_register`). C3 — GUI socket security regression — is deferred to 5b.4 as a known-equivalent-to-today posture rather than fixed in this step.

**See:** `copad-daemon/src/gui_registry.rs` (`start_event_forwarder`, `forwarder_loop`), `copad-linux/src/daemon_forward.rs` (the forward pool + `ForwardJob`), and the e2e step "event bridge — completion forwarded to GUI" in `scripts/e2e-daemon-client.sh`.

## 21. Bridge Echo Prevention via `Event::bridge_id` (Step 5b.2 Stage B)

**Problem:** Step 5b.2 introduces a bidirectional GUI↔daemon event bridge so the daemon-side TriggerEngine can see GUI-published events. Naive implementations either form an echo loop (daemon publishes → daemon→GUI forwarder sends → GUI republishes → GUI→daemon forwarder sends → ...) or break existing semantics. The daemon→GUI bridge already preserves `source` because the trigger engine's `try_promote_or_drop_preflight` gates on `source == "copad.action"`; rewriting `source` at the bridge boundary (the obvious "easy fix") would silently break chained-trigger preservation.

**Decision:** Each `Event` gains a non-serialized `bridge_id: Option<u64>` field. `#[serde(skip)]` keeps it out of every wire frame so plugins, coctl, and the existing daemon→GUI `WireEvent` shape are unchanged. The bus's `publish_bridged(event, bridge_id)` thin wrapper stamps the field; outgoing forwarders on both sides skip events whose `bridge_id.is_some()`. The id itself is a per-process monotonic u64 from `next_bridge_id()` — wraparound takes ~10⁹ years, not worth a leak-safe variant.

Four rounds of codex-plan locked in the exact contract: source-tagging was rejected (round-2 C2 — breaks await promotion), wire-frame extension was rejected (would require parser changes and a wider compat surface), the `bridge_id` field with `serde(skip)` was the only option that left every existing path untouched.

The `_bus.publish` ingest method is **wire-only**, not in `ActionRegistry`. Three reasons codex round-3 confirmed: (1) generic socket clients can call any registry method via raw `Request { method }`, which would bypass the registered-GUI auth convention; (2) registry methods auto-publish `<name>.completed` / `.failed` events — for `_bus.publish` that fans out a synthetic completion for every forwarded GUI event, polluting the bus; (3) auth lives at the connection layer (`registered_client_id.is_some()`), which the registry can't see. Special-case in `handle_connection` before `dispatch(...)` is the right placement.

The GUI's outgoing forwarder is **gated on `host_triggers=true`** in the daemon's `gui.register` ack — not on a GUI-side env var. This is the cut-over signal that Stage C will use: when daemon dispatches, GUI's local engine clears. The default (`COPADD_HOST_TRIGGERS` unset) preserves today's GUI-authoritative behavior end-to-end.

**Tradeoff:** A per-process counter means a stale event observed across a daemon restart could collide (each daemon starts at 1). Not a real concern because bridge crossings are session-scoped — neither side retains stale events past a disconnect. Tracking the id on every `Event` clone costs ~16 bytes per event; trivial. Stage B intentionally defers the GUI-engine clear (cut-over) to Stage C — Stage B alone with `COPADD_HOST_TRIGGERS=1` double-fires (both engines dispatch), documented as known limitation under an opt-in env flag.

**See:** `copad-core/src/event_bus.rs` (`next_bridge_id`, `publish_bridged`, `Event::bridge_id`), `copad-daemon/src/socket.rs` (`handle_bus_publish`, `DaemonState.host_triggers`), `copad-daemon/src/gui_registry.rs` (`forwarder_loop` echo gate), `copad-linux/src/gui_client.rs` (`start_gui_event_forwarder` + `ForwarderGuard`), and e2e step 9 in `scripts/e2e-daemon-client.sh`.

## 22. Atomic Cut-Over and Daemon Config Watcher (Step 5b.2 Stage C)

**Problem:** Stage B left a documented double-fire under `COPADD_HOST_TRIGGERS=1` — the GUI's local TriggerEngine kept dispatching alongside the daemon's. Stage C resolves the cut-over without leaving recovery gaps. Two independent requirements:

1. When the daemon advertises `host_triggers=true` in `gui.register`, the GUI must empty its local engine atomically AND refuse later `watch_config` reloads (which would re-arm it). When the daemon crashes mid-session, the GUI must restore local authority before the reconnect backoff begins — otherwise events fire with no subscriber.
2. The daemon's own engine needs runtime config tracking even with no GUI attached (headless `copadd`). A user editing `~/.config/copad/config.toml` should see the change take effect without restarting the daemon.

**Decision (cut-over):** Three round-by-round codex critiques narrowed the design to:

- A `mpsc::channel::<bool>()` from `gui_client::run` to the GTK 50 ms timer. On register-ack success the run sends `ack.host_triggers`. A `HostTriggersGuard` drop guard mirrors `ForwarderGuard`'s pattern and sends `false` on every `run()` exit (success or error), restoring local authority on daemon disconnect.
- The 50 ms timer's cut-over consumer runs **BEFORE** `pump_all` so a queued `true` clears the engine on the SAME tick rather than letting one more dispatch through.
- The consumer is **edge-triggered**: it only applies when the queued value differs from the last applied state (`Cell<bool>` per-closure). Otherwise every normal reconnect with `host_triggers=false` would call `set_triggers(cached)` and reset preflight/pending await state — same hazard as today's hot-reload but fired on a connection bounce.
- A persistent `Arc<AtomicBool> host_triggers_active` is the source of truth. `watch_config` consults it: when `true`, the disk-reload still updates `cached_triggers` (so a later disconnect restores the FRESHEST config, not the startup snapshot) but skips `set_triggers` + `reconcile_triggers` for the triggers field. Theme, statusbar, background, keybindings still reload normally.
- The `start_gui_event_forwarder` (Stage B) is started **BEFORE** the cut-over signal is sent (codex round-1 C1). Otherwise the GTK timer could clear local subscriptions while the forwarder hasn't subscribed yet, leaving an event-loss window where neither engine fires.
- Tradeoff: `set_triggers` clears in-flight await state on every transition. Documented as acceptable — same as today's hot-reload semantics; trigger configs at personal scale rarely run long chains across reconnect events.
- Post-disconnect window (≤ 50 ms between `Drop` sending `false` and the timer applying it) where the local engine is still empty AND the daemon is gone — events in that window fire with no subscriber. Negligible in practice; only relevant if the daemon crashes mid-session under `COPADD_HOST_TRIGGERS=1`.

**Decision (watcher):** Daemon-side config watcher is a 2 s mtime poll thread spawned in `main()`, always running regardless of `host_triggers`:

- `load_triggers_config()` captures the file mtime at the same instant as the initial load and passes it to the watcher. Without that, an edit landing between `main()`'s load and the watcher's first tick would be silently treated as the baseline (codex round-2 C1).
- `apply_reloaded_triggers` mirrors the GUI's `watch_config` ordering when host_triggers=true: `engine.set_triggers(new)` → `pump_state.pump_all(ctx, engine)` on OLD subscribers (flush pending events into the soon-to-be-replaced set) → `pump_state.reconcile_triggers(bus, &new)`. Skipping `pump_all` would discard pending events the new trigger set would have matched during a pattern-narrowing reload.
- When host_triggers=false, **no PumpState exists** (codex round-3 C1 — extended scrutiny uncovered a pre-existing Stage A bug). The watcher only updates `engine.set_triggers` and refreshes `cached_triggers`; no `reconcile_triggers` means no `subscribe_unbounded` receivers are created with nothing to drain them, so the daemon bus doesn't accumulate events under the default daemon configuration.
- `notify` crate dep rejected; 2 s mtime poll is enough for trigger reloads and avoids the wider compat surface.

**Tradeoff:** Tying the cut-over to the daemon's register-ack advertisement means each disconnect/reconnect re-traverses the cut-over → restore cycle. Each transition costs one `set_triggers` (clears preflight/pending await state). Users running long-chain workflows during daemon flap would lose those workflows. Accepted because (a) personal-scale trigger configs are short, (b) the daemon flap itself is the more pressing fault and would already break in-flight workflows for other reasons (forwarded events stop arriving), (c) the alternative — preserve await state across cut-over — adds significant state-machine complexity.

The watcher running even when `host_triggers=false` is intentional: a future Stage D could turn `host_triggers` into a runtime toggle (e.g. on a daemon mode switch) and the watcher already has the engine state ready.

**See:** `copad-linux/src/window.rs` (`apply_host_triggers`, `drain_to_latest`, `watch_config` skip gate, 50 ms timer cut-over consumer), `copad-linux/src/gui_client.rs` (`HostTriggersGuard`), `copad-daemon/src/main.rs` (`config_watcher_loop`, `apply_reloaded_triggers`, `build_pump_state`), and e2e step 10 in `scripts/e2e-daemon-client.sh`.

## 23. `events.publish` Public Surface + `SO_PEERCRED` Source Stamping (Step 5b.2 Stage D)

**Problem:** Headless `copadd` runs need a way for external scripts to fire events onto the daemon bus — without that, the entire trigger engine relocation (Stages A-C) has no headless entrypoint. The first instinct was to expose the bus through the existing `ActionRegistry` (e.g., add a `publish` action), but two rounds of codex-plan revealed the wider trust gaps that approach opens.

**Decision:** `events.publish` is a **wire-only** socket method (special-cased in `handle_connection` before generic `dispatch`). NOT in `ActionRegistry`. Three concrete reasons:

1. Generic socket clients can call any registry method via raw `Request { method }`. If `events.publish` were in the registry, every connection-level trust gate (the registered-GUI convention, the future per-method ACL) would be a fiction — the registry routes by name without inspecting the connection.
2. Every registry-routed action auto-publishes `<name>.completed` / `.failed` events. For `events.publish` that would mean a synthetic completion fan-out for every external event — polluting the bus with metadata about the metadata.
3. Trust gates live at the connection layer (peer credentials, registered-GUI flag), which the registry's method-match path has no visibility into.

The public surface accepts `{ kind: String, payload?: Value }` and **daemon-controls `source` and `timestamp_ms`**. `source = format!("client.{pid}")` is stamped via `SO_PEERCRED` on Linux; non-Linux returns `None` → `"client.unknown"`. `timestamp_ms` is filled by `BusEvent::new` from the daemon clock. The caller has no way to set either field. This is what makes spoofing the action-registry completion gate impossible: `try_promote_or_drop_preflight` reads top-level `source` and trusts only `copad.action`. Since `events.publish` cannot ever produce that top-level value, no chained workflow can be hijacked.

`_bus.publish` (Stage B) and `events.publish` (Stage D) are two separate methods, not one with a registered-GUI branch. The split avoids a complex "if registered, trust the source field, else stamp it" branch with subtle trust gaps:
- `_bus.publish` = bridge variant (registered GUI relays its own events, source/timestamp from caller, `bridge_id` set to prevent echo).
- `events.publish` = public variant (no registration, daemon stamps source/timestamp, no `bridge_id`).

`coctl event publish <kind> [json-payload]` uses `paths::daemon_socket_path()` directly, bypassing `discover_socket`'s GUI-first preference. Connecting to a GUI socket would return `unknown_method`. The publish subcommand also parses the payload locally before opening the socket so malformed JSON fails with a clear `invalid_argument` instead of being forwarded as a confusing daemon-side error.

**Tradeoff:** `SO_PEERCRED` returns the peer's pid at connect time, not call time. Pid reuse across process death (bounded by Linux's `kernel.pid_max`) could briefly misattribute the source — acceptable for the log-correlation use case. macOS uses `LOCAL_PEERCRED` and would need its own plumbing; for now the macOS daemon stub returns `None` and source becomes `"client.unknown"`. No rate-limiting on `events.publish` — same trust band as today's `coctl call`, where 0600 socket reachability is the only guard. Documented as known; rate-limit is a follow-up. Payload size cap (64 KiB) was considered but deferred: the daemon's `reader.lines()` has no upper bound regardless, so a cap at the application layer wouldn't meaningfully protect daemon memory. Line-size hardening is a separate commit.

The decision to leave trigger condition / interpolation reading payload-source first (an existing pre-Stage-D semantic, preserved) is a user-config concern: if a trigger author writes `condition = "event.source == 'foo'"`, they're reading payload, not provenance. The daemon's trust gates (`try_promote_or_drop_preflight`) operate on top-level fields, which the daemon controls; user-defined conditions on `event.source` would need to verify payload absence to gate on daemon-controlled provenance. Documented in the user-facing trigger config docs (forthcoming).

**See:** `copad-daemon/src/socket.rs` (`peer_pid`, `handle_events_publish`), `copad-cli/src/commands.rs` + `copad-cli/src/main.rs` (`EventCommand::Publish`, `dispatch_publish`), and e2e step 11 in `scripts/e2e-daemon-client.sh`.

## 24. Curated GUI Env Whitelist for `system.spawn` (Step 5b.2 Stage E)

**Problem:** Stage A relocated `system.spawn` from the GUI's `LiveTriggerSink` to the daemon's `DaemonTriggerSink`, but the child process now inherits the daemon's process env, not the GUI's. Hyprland trigger configs (the user's primary `system.spawn` use case — `hyprctl dispatch` calls) depend on `HYPRLAND_INSTANCE_SIGNATURE`, which the daemon doesn't have because `copadd` was started by `systemd --user` or the login shell before the compositor ran. Wayland/X11 tooling (`notify-send`, `pactl`) needs `DISPLAY` / `WAYLAND_DISPLAY` / `DBUS_SESSION_BUS_ADDRESS` for the same reason.

**Decision:** A curated whitelist of session env keys flows from the GUI to the daemon at `gui.register` time. The daemon-side filter is the trust boundary; the GUI-side curation is UX-only.

The 7-key whitelist (`HYPRLAND_INSTANCE_SIGNATURE`, `DISPLAY`, `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, `XDG_SESSION_TYPE`, `XDG_CURRENT_DESKTOP`, `DBUS_SESSION_BUS_ADDRESS`) covers Hyprland + standard Wayland/X11 + session-bus tooling. Anything outside the list — `PATH`, `LD_PRELOAD`, `OPENAI_API_KEY`, terminal aliases — is dropped at `filter_gui_env` BEFORE the env reaches `GuiClient.gui_env`. Codex round-1 C1 surfaced the subtle but important point: GUI-side filtering alone is not load-bearing because any registered client could be a mock (e.g., the e2e test infra itself), and the daemon cannot distinguish a legitimate `copad-linux` from a malicious script.

`DaemonTriggerSink::handle_system_spawn` merges primary's env via `Command::envs(gui_env)`. Rust's `Command::new` inherits the parent's env by default; `envs` OVERRIDES matching keys without clearing unlisted ones. So `PATH`, `HOME`, `USER` from the daemon persist while `DISPLAY` etc. are pulled from the GUI's filtered map. Critically, `env_clear` is NOT called — that would strip `PATH` and break `/usr/bin/env`-style triggers.

`GuiRegistry::primary_gui_env()` returns `Option<HashMap<String, String>>`. Lock order `clients → primary` mirrors `route()`'s order to avoid an AB-BA deadlock against any caller that already holds `clients` (codex round-1 C2). When no GUI is registered (pure headless mode), the accessor returns `None` and the sink falls back to pure daemon env — the pre-Stage-E behavior, preserved.

**Tradeoff:** Capturing env once at register time, not at spawn time. If the user restarts their Hyprland session while `copadd` stays running (rare), the cached signature is stale until reconnect. Refresh-on-register is the simplest path and matches user expectation (session vars are write-once). Pid-reuse semantics on `peer_pid` (Stage D) have the same property: connect-time snapshot wins. macOS daemon stub returns empty `gui_env` map; when the macOS shell eventually ships, it'll need its own env-capture wire.

Curated whitelist drift is the maintenance hazard. New keys added to the list need an audit: each one is a vector if someone misconfigures a trigger to spawn shell-quoted user input that interpolates an env var.

**See:** `copad-daemon/src/gui_registry.rs` (`GUI_ENV_ALLOWED_KEYS`, `filter_gui_env`, `GuiClient.gui_env`, `primary_gui_env`), `copad-linux/src/gui_client.rs` (`GUI_ENV_CURATED_KEYS`, `capture_gui_env`), `copad-daemon/src/daemon_trigger_sink.rs` (`handle_system_spawn` env merge), and e2e step 12 in `scripts/e2e-daemon-client.sh`.

## 25. Daemon `event.subscribe` Bus Projection (Phase 8 closing)

**Problem:** The GUI special-cased `event.subscribe` as a `bus.subscribe_unbounded("*")` projection (copad-linux/src/socket.rs), but the daemon returned `unknown_method`. `coctl event subscribe` against the daemon socket failed, and service-plugin authors expecting the documented `event.subscribe { patterns: [...] }` shape had no daemon-side handler.

**Decision:** Mirror the GUI's projection at the daemon's `handle_connection`. One `bus.subscribe_unbounded("*")` per connection, filtering deferred to the handler via `pattern_matches` OR'd across `params.patterns`. `params.patterns` is documented protocol; `None`/`[]` means "all".

Single-subscriber + handler-side filter beats N-subscribers + N-forwarder-threads because cross-pattern event ordering is trivially preserved (FIFO from one receiver) and the bus's `pattern_matches` is sub-microsecond per event. The tradeoff is wasted CPU on the daemon when a narrow pattern is requested — acceptable at typical event rates (≪ 1k/sec); profile-driven optimization can switch to bus-level multi-pattern subscribers later if needed.

**Disconnect detection during quiet bus periods (codex round 2 C1):** the GUI's projection has a latent leak — `rx.recv()` blocks indefinitely with no event, so a client that disconnects during a quiet stretch keeps the connection thread + unbounded bus subscriber alive until the next event lands. The daemon's implementation closes this with `recv_timeout(15s)` + a no-op `writer_tx.send(String::new())` on timeout. The writer thread writes an empty line on the wire, which probes EPIPE: closed socket → writeln fails → writer thread exits → writer_rx drops → subscriber's `send` returns Err → handler returns. `coctl`'s subscribe reader (`copad-cli/src/client.rs:69`) already skips empty lines, so the keep-alive is wire-compatible.

**Registered-GUI rejection (codex round 2 I2):** registered GUI connections already receive events via `start_event_forwarder` (Stage A). Running both pumps on one socket duplicates every event. The daemon rejects `event.subscribe` on a registered connection with `error.code = "invalid_request"` and instructs the caller to use `gui.subscribe`/`gui.unsubscribe`. `docs/gui-daemon-protocol.md` reconciled to spell out the exception explicitly.

**Subscribe-before-ack ordering (codex review C1):** the bus subscription is created BEFORE the ack is queued on `writer_tx`. If the ack were queued first, a publisher on a separate connection could publish a matching event between the client receiving the ack and the daemon's reader thread reaching `subscribe_unbounded("*")` — a lost event that violates the "lossless projection" contract. Ordering is `bus.subscribe_unbounded → send ack → enter recv loop`; the receiver is now active by the time the client can act on the ack.

**Tradeoff:** Once `event.subscribe` is active the connection is subscribe-only — the reader thread enters the recv loop and never returns to the request loop. Same contract as the GUI's projection. A second connection is required for further RPC. The "lossless" delivery contract from workflow-runtime.md (`subscribe_unbounded`) means a stuck client lets the bus subscriber's mpsc grow unbounded; the upstream `writer_tx(512)` caps it (full writer_tx blocks the subscriber loop, which lets further events accumulate in the unbounded receiver while the writer is wedged). Pathological case recoverable by killing the client — kernel eventually fails the writer's write → chain unwinds.

**See:** `copad-daemon/src/socket.rs` (`run_event_subscribe`, `parse_subscribe_patterns`, `SUBSCRIBE_KEEPALIVE`), and e2e step 13 in `scripts/e2e-daemon-client.sh`.

## 26. Completion-Event Fan-Out for Legacy `socket::dispatch` Arms (Phase 8 closing)

**Problem:** `ActionRegistry::with_completion_bus` auto-publishes `<action>.completed`/`<action>.failed` for every action it handles, but copad-linux's `socket::dispatch` legacy match-arm fallthrough (`tab.*`, `background.*`, `terminal.exec`, `webview.*`, `agent.approve`, `claude.start`, `plugin.list`, …) bypasses the registry on miss and never published. Triggers chaining off `tab.new.completed` got silence. Same for daemon-side: when `dispatch_via_gui` proxied a legacy method to a registered GUI, neither side published.

**Decision:** Publish completion at the source of execution.
- **GUI side**: every legacy match arm funnels its reply through `SocketCommand::reply_with_completion(bus, resp)`, which calls `publish_legacy_completion` then forwards to `cmd.reply.send`. Callback-deferred handlers (webview JS exec via `run_js_command`, agent.approve dialog) capture `bus`/`method`/`silent` clones before moving `cmd.reply` into the closure and call the free function `publish_legacy_completion` directly.
- **Daemon side**: `dispatch_via_gui` and `DaemonTriggerSink::fallthrough_worker` call `publish_legacy_completion` on the daemon bus AFTER `client.invoke` returns. Failure paths — `no_gui`, `unknown_client`, invoke timeout, GUI returning ok=false — emit `.failed` (codex review C2).

**Duplicate avoidance via `SocketCommand.silent_completion`:** the daemon→GUI bridge re-publishes daemon-bus events on the GUI bus, so daemon-proxied actions would double-publish at GUI if both sides published unconditionally. `gui_client::handle_invoke` sets `silent_completion = true` on the SocketCommand it constructs from a daemon Invoke; the GUI's wrapper skips local publish for those, and only the bridged daemon-published event lands on the GUI bus. Direct coctl→GUI calls leave `silent_completion = false` so GUI-local triggers still fire (codex review C1 round 2).

**`daemon_forward::forward` skipped** (codex review C2 round 1): commands the GUI doesn't know how to handle locally are proxied to the daemon, where the registry publishes natively and the bridge brings the event back to GUI. Wrapping forward would dupe; the wrapper is intentionally absent on that path.

**`LEGACY_SILENT_METHODS`** = `["terminal.read", "terminal.state", "terminal.history", "terminal.context", "tab.list", "tab.info", "session.list", "session.info"]` — read-only / agent-polled paths whose "completion" is the response itself. Publishing would flood the bus without enabling meaningful chained triggers. Mirror of `register_silent`'s semantics on the legacy surface. Consulted by both `SocketCommand::reply_with_completion` (GUI) and `dispatch_via_gui`'s publish helper (daemon).

**Trust framing (codex review I4):** the daemon stamping `source = "copad.action"` on a GUI-invoked action's completion event represents "the daemon vouches that this GUI-owned action returned this response," NOT "the daemon forwards GUI-provided provenance." `_bus.publish`'s rejection of `.completed`/`.failed` kinds + `copad.action` source stays intact — that gate prevents GUIs from spoofing arbitrary completion events. The daemon-side publish here is at the trust boundary INSIDE the daemon (post-route, post-invoke, post-response receipt), not a bridge surface.

**Known boundary**: when `host_triggers=true`, the GUI's `TriggerEngine` is cleared and the GUI bus completion events from direct coctl→GUI calls have no consumer. Symmetrically, the daemon's `TriggerEngine` doesn't see coctl→GUI-direct completions (the GUI→daemon forwarder allowlist excludes `.completed`). Users running triggers should connect through the daemon socket (the default for `coctl` discovery) so the completion event lands where the trigger engine is reading.

**See:** `copad-daemon/src/socket.rs` (`LEGACY_SILENT_METHODS`, `is_legacy_silent`, `SocketCommand::reply_with_completion`, `publish_legacy_completion`, `dispatch_via_gui` publish call), `copad-daemon/src/daemon_trigger_sink.rs` (`fallthrough_worker` publish call), `copad-linux/src/socket.rs` (legacy match arms calling `cmd.reply_with_completion`, webview callbacks + agent.approve calling `publish_legacy_completion`), `copad-linux/src/gui_client.rs` (`silent_completion=true` on daemon-Invoke SocketCommand), and e2e step 14 in `scripts/e2e-daemon-client.sh`.

## 27. Per-Frame Memory Caps + GUI-Client Bounded Writer (5b.2 Stage B follow-ups)

**Problem:** Two hazards codex flagged on the Step 5b.2 reviews that landed as deferred follow-ups, not blockers:
- `BufRead::lines()` on both daemon (`copad-daemon/src/socket.rs` `handle_connection`) and GUI (`copad-linux/src/socket.rs` `start_server`) calls `read_until` internally with no upper bound. A peer (legitimately misbehaving or hostile) that streams bytes without `\n` would force the daemon/GUI to buffer the entire stream in memory until either OOM or socket close.
- The GUI-side daemon-client (`copad-linux/src/gui_client.rs` `spawn`/`run`) uses `mpsc::channel::<String>()` (unbounded) for its writer queue, while the daemon-side equivalent was bounded to `sync_channel(512)` in Step 5b R2 INFO. A wedged daemon socket reader would stall the GUI's writer thread, and the outgoing event forwarder + RPC reply path would accumulate strings without limit.

**Decision:**
- `MAX_FRAME_BYTES = 1 MiB` constant + `pub fn read_line_capped(reader, buf) -> io::Result<Option<String>>` helper in `copad-daemon/src/socket.rs`. Returns `Ok(None)` on EOF, `Ok(Some(s))` on a full line, `Err(InvalidData)` AS SOON AS the running total would exceed the cap. No resync attempt — codex review C1 surfaced that a peer streaming bytes without `\n` would let any resync loop block on `fill_buf` indefinitely, defeating the cap's purpose. The helper fail-fasts; both callers (daemon `handle_connection`, GUI `start_server`) send a wire-level `frame_too_large` reply (id `""`) and then close the connection. The reasoning: a 1 MiB+ unterminated frame is either a misbehaving client or an attack, and the trust band (0600 socket, one user) doesn't justify a partial recovery path that adds blocking risk.
- GUI-client writer changed to `mpsc::sync_channel::<String>(512)` — matches the daemon side. `register`, `handle_ping`, `write_overloaded`, `handle_invoke`, the outgoing event forwarder, and the test channels all switched from `Sender<String>` to `SyncSender<String>`. Recovery semantics unchanged from the daemon side: writer thread `writeln` fails on a dead socket → exits → writer_rx drops → `send` returns Err → forwarder/jobs surface as `Disconnected`.

**Tradeoff:** 1 MiB cap is well above any legitimate JSON frame we ship today (webview content reads ~256 KiB max, screenshots base64 below ~1 MiB but typically under). Chunked transfer of very large payloads would need to be split across multiple actions — explicit at the cap. The 512 writer buffer matches the daemon side's empirically-validated bound; same recovery loop.

**Test coverage:** 4 new unit tests on `read_line_capped`:
- `returns_full_line` — two consecutive frames, then EOF
- `rejects_oversized_frame_fail_fast` — 1 MiB+32 bytes of `x` without `\n`; asserts `InvalidData` returns promptly (would block forever if any resync logic existed)
- `rejects_overflow_even_when_newline_eventually_arrives` — payload at cap+1 followed by `\n`; asserts the cap is enforced strictly (the helper doesn't peek past the cap to confirm)
- `handles_no_trailing_newline_at_eof` — final partial frame at EOF

The oversized tests use a worker thread to write the payload over `UnixStream::pair` because Linux unix-socket buffers (~208 KiB default) deadlock a synchronous `write_all` if the reader isn't draining concurrently — the test's own write strategy is a microcosm of the production concern.

**See:** `copad-daemon/src/socket.rs` (`MAX_FRAME_BYTES`, `read_line_capped`, capped reader in `handle_connection`), `copad-linux/src/socket.rs` (capped reader in `start_server`), `copad-linux/src/gui_client.rs` (`GUI_CLIENT_WRITER_BUFFER`, all `SyncSender<String>` sites, capped reader in the daemon→GUI receive loop AND `await_register_ack`).

## 28. Supervisor Waiter Pool (Phase 9.5 follow-up)

**Problem:** Phase 9.4 introduced a bounded `ThreadPool` for `ActionRegistry` blocking handlers, but the corresponding waiter on the supervisor side — `ServiceSupervisor::dispatch_invocation`'s `thread::spawn(move || resp_rx.recv_timeout(120s))` — was left as per-call thread spawning. Roadmap entry 240 flagged this as a known limitation: under burst load (many concurrent service invocations) the daemon could accumulate one OS thread per in-flight invoke, each pinned for up to `action_timeout` (default 120s). Not a hot spot today because trigger configs rate-limit invocations, but unbounded growth is a latent footgun for chained workflows that fan out.

**Decision:** Counter-based admission control over per-call `thread::spawn`. `ServiceSupervisor.waiter_active: Arc<AtomicUsize>` tracks concurrent in-flight waiters; `waiter_max` (default 64, env-configurable via `COPADD_WAITER_MAX`) caps the count. Each admitted invocation:
1. `fetch_add` on the counter; if pre-increment value ≥ max, decrement back and reply `overloaded` synchronously (no invoke sent).
2. Insert the `pending_responses` entry.
3. Send `action.invoke` synchronously on the caller's thread.
4. `thread::spawn` a waiter that holds a `WaiterPermit` drop-guard; the permit decrements the counter on thread exit (success, timeout, or panic).

We considered using the existing `copad_core::thread_pool::ThreadPool` (the same primitive that backs the Phase 9.4 ActionRegistry pool), but rejected it after two codex rounds: the pool's bounded queue introduces a "queued but not running" state where a waiter job sits with `resp_rx` ready to receive while no thread is parked to forward to `reply`. A fast service response (or a synthetic send-failure response) can land in `resp_rx` buffered until a worker frees, by which time the caller's `invoke_remote(action_timeout)` has already returned `action_timeout`. The counter primitive avoids this by spawning a thread on every admit — there is no queueing window.

Tradeoff: per-call thread::spawn cost (~30 µs) instead of pool worker reuse. For waiters that spend their lifetime blocked on `recv_timeout`, reuse saves no real work — the cost being saved would be µs of spawn overhead on work units that take ms-to-seconds. Worth it for the simpler ordering story.

**Ordering invariant (resolved across three codex review iterations):**
- v1 (initial): `handle.send(invoke)` BEFORE pool admission. Saturation → caller sees `overloaded` while the service had already executed the action.
- v2: move the send INSIDE the pool worker. Queued workers stale-send long after the caller's `invoke_remote` already returned `action_timeout` and the workflow retried.
- v3 (counter-based, final): drop the pool's queue entirely. Counter admission + immediate `thread::spawn`. Saturation rejects without sending; admission spawns a thread instantly so there is no "queued but not running" state where a response can be buffered into an unattended `resp_rx`.

Under the final design, the SERVICE sees each invoke exactly once across the full saturation × caller-timeout × send-failure matrix. Caller's `reply_rx.recv_timeout(action_timeout)` and waiter's `resp_rx.recv_timeout(action_timeout)` operate on the same clock (both start near `dispatch_invocation` entry); a service reply propagates through reader → resp_tx → resp_rx → waiter → reply → reply_rx. If caller times out first, waiter's `reply.send` returns Err harmlessly. If `handle.send` fails after admission, the caller's thread short-circuits: removes the pending_responses entry, replies the error directly, and lets the permit drop (slot released for the next caller).

`daemon.info` does NOT yet surface the waiter counter / max — could be added; not blocking.

**Test coverage:** 3 unit tests on the admission primitives — `waiter_permit_decrements_on_drop`, `waiter_permit_decrements_once_even_under_panic` (panic-unwind preserves the slot release), and `env_waiter_max_falls_back_to_default_on_invalid` (env parse + zero rejection). The full dispatch_invocation path is exercised by the existing e2e (step 5 heartbeat survival, step 7 pool saturation, step 8 plugin RPC round-trip) which transitively cover the counter admission and waiter-thread lifecycle.

**See:** `copad-daemon/src/service_supervisor.rs` (`waiter_active` counter, `waiter_max` cap, `WaiterPermit` drop-guard, `env_waiter_max` helper, `dispatch_invocation` admission + send + spawn).

## 29. Command Palette (Phase 8 closing)

**Problem:** Phase 8 closed `ActionRegistry` + completion-event fan-out, but the user still had no in-GUI way to enumerate or fire registered/legacy actions. The roadmap-declared affordance was a Ctrl+Shift+P modal — a fuzzy filter over the registry.

**Decision:** Build a minimal GTK4 modal palette in `copad-linux/src/command_palette.rs`:
- Modal `Window` (transient on the main window) containing a `SearchEntry` + scrolled `ListBox`.
- Action surface = `actions.names()` (GUI registry — `system.ping`, `system.log`, `context.snapshot` today) ∪ `LEGACY_DISPATCH_METHODS` (~45 entries re-exported from `copad_daemon::socket`).
- Substring filter (case-insensitive, whitespace-trimmed). Fuzzy matching is a follow-up; substring is sufficient for ≤100 entries.
- Enter on the SearchEntry dispatches the currently selected ListBox row through the existing `dispatch_tx` SocketCommand pump. Empty params (`{}`); actions that need params will surface `invalid_params` via the normal reply path — documented v1 limitation.
- Up/Down navigates the list while focus stays in the SearchEntry. Esc closes (handled both via a capture-phase `EventControllerKey` and via SearchEntry's `stop-search` signal as a safety net — without the capture phase the SearchEntry's built-in Esc handler ate the event and the palette wouldn't dismiss).

**Destructive-action confirmation (codex review C3 round 1):** `tab.close` with empty params would close the active terminal — accidental data loss if the user typed it + hit Enter to dispatch. Added a `DESTRUCTIVE_ACTIONS: &["tab.close"]` const; before dispatching one of those, show a `gtk4::AlertDialog` with `[Cancel, Confirm]` where Cancel is BOTH default and cancel button (codex review C1 round 2 — Confirm-as-default would let a second stray Enter complete the destruction). The user must explicitly select Confirm to proceed.

**Focus restoration (codex review I1 round 2):** the palette captures `mgr.active_panel()` before opening and calls `panel.grab_focus()` on close (Esc, Cancel, or post-dispatch) so typing returns to the previously focused terminal/webview.

**User-keybinding precedence:** `Ctrl+Shift+P` is a built-in default, but `copad`'s `check_custom_keybinding` runs BEFORE the built-in match — so a user `config.toml` entry like `"ctrl+shift+p" = "spawn:..."` shadows the palette binding. This is intentional: the custom-keybindings feature exists specifically to let users override defaults. Users who want the palette and an existing Ctrl+Shift+P spawn binding move their custom binding to a different key.

**Visible action surface limitation (codex review C2 round 1):** the palette only enumerates GUI-reachable actions today. Daemon-hosted plugin actions (`kb.search`, `slack.send_message`, etc.) ARE dispatchable through socket dispatch's `daemon_forward` fallback, but they're registered in `copadd`'s registry, not the GUI's — and the GUI has no `actions.list` RPC to enumerate them. Documented v2 follow-up. The 48 listed entries already cover the dominant interactive workflow surface.

**Tradeoff:** No param prompt in v1. Adding a form builder for actions that need params would double the diff and pull in an opinion about the form-rendering layer. v2 can wire a second-stage form (or just let the user `coctl call <method> --params '{...}'` for parametric actions).

**See:** `copad-linux/src/command_palette.rs` (full implementation + 5 unit tests on `filter_actions`), `copad-linux/src/tabs.rs` (Ctrl+Shift+P key arm + Ctrl+Shift+Left for prev-pane), `copad-linux/src/window.rs` (TabManager::new wired with actions registry).


## 30. URL click-to-open (Phase 7 closing)

**Problem:** Plain-text URLs in terminal output were not clickable. The roadmap-declared affordance was Ctrl+Click on a URL to open it in the default browser, plus support for OSC 8 hyperlinks (where visible text can differ from the target URL).

**Decision:** Implement in a dedicated `copad-linux/src/url_click.rs` module installed once per `TerminalPanel` from `tabs.rs::create_panel`. Two match paths feed the same launch handler:

1. **Regex URL detection** via VTE's `match_add_regex` + `check_match_at`. Pattern: `(?i:https?://[^\s<>'"]+)` with `PCRE2_MULTILINE` only. Trailing punctuation (`.,;:!?)]}>`) is stripped post-match by `normalize_url` rather than excluded from the regex — keeps the pattern simple and the trim rule auditable.

2. **OSC 8 hyperlinks** via `set_allow_hyperlink(true)` + `check_hyperlink_at(x, y)`. Wins over the regex tag because OSC 8 emitters set the visible label independently of the URL target (`]8;;https://x\click here]8;;\` — the regex would only see "click here").

**Critical implementation detail — flags + jit:** Initial implementation used `PCRE2_MULTILINE | PCRE2_CASELESS` plus `regex.jit(PCRE2_JIT_COMPLETE)`. `check_match_at` returned `(None, -1)` for every coordinate, even with a trivial pattern like `[a-z]+` — `cargo test` passed, but the live API silently returned no matches. After tracing gnome-console's C source for reference, the working setup is:

- compile flags = `PCRE2_MULTILINE` only (no `PCRE2_CASELESS`)
- inline `(?i:...)` group for case-insensitive matching
- no JIT call

The exact failure mode is undiagnosed (vte4 0.8.0 + VTE 0.84 ABI quirk vs. silent PCRE2 flag rejection vs. JIT interaction). Anchoring to gnome-console's flag set is the safest path until a public reproducer exists.

**Scheme allow-list (Ctrl+click).** `normalize_url` rejects anything other than `http://` and `https://`. The check applies uniformly to OSC 8 hyperlink targets AND regex hits — an OSC 8 emitter could otherwise inject `javascript:` or `file://` payloads through arbitrary terminal output.

**Ctrl+Click gate.** Plain click on a URL would steal text selection — gnome-terminal/foot/kitty all gate on Ctrl. The `GestureClick` is registered with `PropagationPhase::Capture` and `connect_pressed` so VTE's selection gesture doesn't claim the sequence first; on a successful URL launch the gesture state is set to `Claimed` so VTE doesn't see the click.

**`gtk4::UriLauncher` over `xdg-open`.** GIO MIME default-handler resolution, no subprocess `Child` to leak, parent-window hint for any error dialogs.

**OSC 52 status (roadmap Phase 7 item 125):** already closed elsewhere. macOS Tier 0.3 gates `clipboardCopy` via `[security] osc52`; Linux VTE 0.84 is deny-by-default and exposes no toggle property in vte4 0.8.0. No Linux code change needed — roadmap entry updated to back-link to Tier 0.3.

**See:** `copad-linux/src/url_click.rs` (full implementation + 6 unit tests on `normalize_url`), `copad-linux/src/tabs.rs::create_panel` (install site), `copad-linux/src/main.rs` (`mod url_click;`).


## 31. macOS terminal emulator core — migrate off SwiftTerm to alacritty_terminal + custom renderer

**Problem:** SwiftTerm hit four architectural blockers we cannot fix without forking:

1. **IME composition broken** — `MacTerminalView.setMarkedText`, `hasMarkedText`, `markedRange` are stubs (`// nothing`, `false`, `NSRange.empty`). Korean/Japanese users can't see what they're composing until commit. Source: `copad-macos/.build/checkouts/SwiftTerm/Sources/SwiftTerm/Mac/MacTerminalView.swift:847,877,885`.

2. **Cursor invisibility with image background** — `CaretView` is an NSView overlay; pinning `caretColor` (tried theme.accent → theme.foreground → NSColor.white) works in shell but fails against busy wallpapers in TUIs (Claude Code/Ink-based). Sibling-NSView opaque backdrop synced via 30Hz timer was attempted (`syncCaretBackdrop` poll loop, z-order managed via `addSubview(_:positioned:.below:relativeTo:)`) — stderr confirmed creation + sync, but user reported zero visible rectangle on screen (layer-compositing path we couldn't crack).

3. **Reverse-video over transparent bg renders as transparent** — same class of bug WezTerm #1076 / Microsoft Terminal #7014 / Zed PR #17611 all document. Zed's fix decouples logical ANSI bg from rendered transparency via a sub-layer; SwiftTerm has no such separation.

4. **No smart-cursor / cursor_text_color=background / transparent_background_colors equivalents.** SwiftTerm's render pipeline is monolithic `drawRect` over a full bounds region; per-cell custom drawing hooks don't exist.

**Options considered:**

- **A. Fork SwiftTerm.** Weeks per blocker + permanent rebase burden; architectural limits (NSView caret overlay, no per-cell hook) survive the fork.
- **B. Replace with `alacritty_terminal` Rust crate + own AppKit/CoreText (Metal later) renderer.** Zed's pattern. Estimated 3-4 months for parity (codex consultation, single dev). We own all the painful surfaces (IME, cursor, transparency, future ligatures/decorations/images).
- **C. Wait for libghostty.** `libghostty-vt` is shipping (Zig API merged May 2026, C API in progress, ~6 months to tagged version; libghostty-swift framework on the roadmap but further out). When ready, this is the lowest-effort highest-quality option — Ghostty core proven by millions of DAU, designed-from-start for embedding. But not usable today and timeline is "if ready".
- **D. Custom from-scratch emulator.** What iTerm2/Kitty/Ghostty did. 6-12 months. Right call only if we need full control AND have time AND can't reuse a maintained core.

**Decision: B (alacritty_terminal + custom renderer), via codex-validated hybrid path** — keep SwiftTerm as production renderer; build alacritty renderer behind a runtime/dev flag; migrate by vertical slices: PTY/grid → plain text render → cursor → selection → IME → scrollback → colors/transparency → advanced features. Flip default only after parity passes.

**Why not E (wait for libghostty):** even at the optimistic 6-month timeline, libghostty-swift framework is later than libghostty-vt. We'd be back to "wait or build a renderer ourselves" anyway, just with a different emulator core (which is the smaller half of the work). The renderer is what we're really committing to build. If libghostty-swift ships in time and is better than alacritty_terminal, the renderer survives and the emulator backend is a thin swap.

**Why not D (from-scratch emulator):** Alacritty's terminal core is mature, spec-compliant, and battle-tested in production at scale. Reinventing the VT parser + grid + scrollback is months of work for negative differentiation. Risk: Alacritty maintainers explicitly state `alacritty_terminal` is "not for external use" (alacritty/alacritty#2132) — Zed accepts that risk and effectively maintains their integration; we will too, pinning specific versions and being prepared to fork the crate if upstream diverges.

**Short-term SwiftTerm stopgaps that stay in the tree until migration:**

- `CopadTerminalView.cursorStyleChanged` bar→block clamp when `nativeBackgroundColor == .clear` (so 2px bar/underline cursors don't become invisible against image)
- `applyCaretColors` pins `caretColor = theme.accent`, `caretTextColor = theme.background` (avoids `NSColor.selectedControlColor` ghost-gray-on-blur)
- `Keybindings.matches` + `CommandPalette.matchesCommandPaletteShortcut` use `keyCode` rather than `charactersIgnoringModifiers` (IME-immune key matching; commit `04e622c`)

**Known limitations documented for users of the SwiftTerm phase:**

- Cursor may be invisible when image background is active + a TUI with busy color palette is running (Claude Code with bright wallpaper). Workaround: increase `[background] tint`, lower `opacity`, or temporarily clear the background.
- Korean/Japanese IME composition does not show preedit text in-cell during composition. The final character commits correctly. Workaround: compose in another app and paste, or rely on muscle memory.

**See:** `docs/macos-renderer-migration-plan.md` for the phased plan, FFI design, and slice-by-slice scope.


## 31. Session persistence (Phase 7 closing)

**Problem:** Closing copad meant losing the current tab/split layout — no auto-restore of where the user was working. The roadmap-declared affordance was XDG-state-backed session persistence with auto-save on close and auto-restore on next launch.

**Decision:** Implement Linux-only in `copad-linux/src/session.rs` with a typed JSON schema and an explicit lifecycle wired from `window.rs`. Schema (versioned, strict — no best-effort parsing of mismatched versions):

```
Session { version: u32 = 1, tabs: Vec<TabSnap>, current_tab: usize }
TabSnap { custom_title: Option<String>, root: SplitSnap }
SplitSnap { Terminal { cwd } | Branch { orientation, position, first, second } }
```

**Persisted on `window.connect_close_request`.** `TabManager::snapshot_session()` walks the live SplitNode tree, building SplitSnap. WebView/Plugin panels are elided (their state — page URL, scroll, plugin-internal — is out of v1 scope); a Branch with one surviving child collapses to that child. If the snapshot has zero terminal tabs (all-elided / all-closed), `session::clear()` removes the file so a stale snapshot doesn't survive an "all tabs closed" exit. Atomic write: temp file + `rename`.

**`current_tab` remap (codex C2 round 1).** The notebook's `current_page()` indexes against the original tab list including elided ones. The persisted index has to point into the surviving terminal-only list, so the snapshot loop maps the active notebook index across elisions before storing it.

**Restored on startup.** `window.rs` reads `session::load()` BEFORE seeding any default tab. If `Some(session)` with non-empty tabs: `TabManager::restore_session()` rebuilds tabs and splits via the existing `add_tab_with_cwd` + `TabContent::split` primitives. Split tree restoration is a recursive walk: the new panel created at each Branch level gets the leftmost-Terminal cwd of the snap's `second` subtree (`session::leftmost_cwd`), so each sub-leaf eventually lands in its persisted cwd. `TabManager::new()` was refactored to NOT create a default tab — `window.rs` now decides between restore vs default-add explicitly, eliminating the phantom-empty-tab race (codex C1 round 0).

**cwd cascade (codex C3 round 1).** Reading `last_cwd` alone misses `cd` changes in shells that don't emit OSC 7 (older bash, some POSIX shells). `TerminalPanel::current_cwd()` is a new helper consulting in order: `terminal.current_directory_uri()` (OSC 7) → `/proc/<pid>/cwd` (proc fs) → `last_cwd` (final fallback for the shell-exited edge case). Both `state()` (existing socket query) and snapshot now go through it.

**URI percent-decoding fix (codex C1 round 2).** OSC 7 paths arrive URI-encoded (`file://host/home/me/My%20Project`). The prior `normalize_osc7_uri()` only stripped the host portion; the persisted cwd would contain a literal `%20`, and `spawn_async` would fail to chdir into a directory that doesn't exist on disk. Path is now decoded with `glib::Uri::unescape_string` (falls back to raw on decode failure so we never drop a value).

**Split position not restored.** The snap stores `paned.position()` but restore doesn't re-apply it — uses `TabContent::split`'s default `set_paned_position_deferred` (50/50). Restoring exact pane sizes requires either reaching into the new Paned post-split or threading position through `split()`; documented v2 polish, not worth carrying in v1.

**Custom tab title.** Stored on the TabSnap, not keyed by panel id (codex C5 round 0 — restored panels get new UUIDs). At restore time `rename_tab(root_panel.id(), &title)` re-applies it to the first panel of the restored tab.

**WebView/Plugin elision.** v1 ships terminal-only because:
- WebView state = page URL + scroll + cookies (security surface).
- Plugin state = plugin-internal, requires a plugin protocol extension.
Both are documented v2 work. The fallback behavior is "next launch has a smaller tab set than the previous session" — acceptable for v1.

**See:** `copad-linux/src/session.rs` (schema + 5 unit tests), `copad-linux/src/tabs.rs::snapshot_session` / `restore_session` / `restore_split`, `copad-linux/src/terminal.rs::current_cwd` + percent-decode in `normalize_osc7_uri` (+1 test), `copad-linux/src/window.rs` (close_request + restore-or-add at startup).


## 32. `action_result` interpolation in `payload_match` (Phase 14.2 deferred slice 1)

**Problem:** The await clause's `payload_match` could only reference the originating event's fields (`{event.<x>}`). Chaining "post to Slack → wait for a reply on the SAME thread" required the response payload's `thread_ts` to flow back into the await's match — but `LiveTriggerSink` returns `Ok({queued: true})` synchronously for blocking/legacy actions, so the real result wasn't available at register time. The 14.2 slice 1 doc explicitly deferred this.

**Decision:** Move `payload_match` interpolation from register-time to promotion-time. The `.completed` event published by `ActionRegistry` already carries the action's real return payload — capture it during `try_promote_or_drop_preflight` and use it as the `action_result` namespace alongside `event.*` when interpolating.

**State machine refit:**

- `PreflightAwait` now stores `payload_match_template: Map<String, Value>` (the un-interpolated form) plus the full `original_event: Event` (vs. just `original_payload` before — the kind/source/timestamp fields are needed for re-interpolation).
- `try_promote_or_drop_preflight`: on `.completed`, capture `event.payload.clone()` as `action_result`, then run `interpolate_value_typed(v, &original_event, None, Some(&action_result))` for every template entry. The fully-resolved match goes into `PendingAwait.payload_match`; the action_result is also stored on PendingAwait for the synthesized `<trigger>.awaited` event downstream.
- `build_awaited_payload` gains a third arg (`action_result: Option<&Value>`); when present, it lands as `action_result:` on the synthesized payload, parallel to `await:`. Downstream triggers thus read `{event.action_result.<field>}` as a regular nested-payload lookup.

**Interpolator extension:**

- `resolve_token` / `resolve_token_value` gain an `action_result: Option<&Value>` arg; both branch on `token.strip_prefix("action_result.")` before falling back to the existing `event.` and `context.` paths.
- `interpolate_value` / `interpolate_value_typed` / `interpolate_string` thread the param through; public callers of `interpolate_value` (e.g. `Trigger::interpolate`) go via the existing no-action-result variant, so the public API is unchanged.

**Posture decisions:**

- **Token-not-found preserves the literal.** If the `.completed` payload has no `ts` field, `{action_result.ts}` resolves to the literal string `"{action_result.ts}"`, the pending match against any real ts fails, and the pending stays until timeout. Better than coercing to `null` and firing on a garbage match. (+1 test `action_result_token_missing_field_keeps_match_open`.)
- **`sweep_pending_awaits` for FireWithDefault**: preflight expiry has no action_result available (action never completed) — pass `None`. Pending expiry has one — pass `Some`. The synthesized `*.awaited` event therefore carries `action_result:` when it makes sense and omits it when it doesn't. **`None` actively removes the key** (codex review C1 round 2): if the firing event is itself an upstream `*.awaited` synthesized event, its payload already has an `action_result:` — without an explicit `remove`, that stale field would leak into the downstream timeout event's payload and `{event.action_result.*}` would read the wrong action's result.
- **No persistence across copad restart.** Both PreflightAwait and PendingAwait remain RAM-only. The earlier 14.2 deferred slice 2 (persistent journal) is unblocked by this change but not implemented here — minute-scale awaits are unaffected.
- **Context captured at register, replayed at promotion (codex review C1 round 1).** Pre-refactor, `register_preflight_await` interpolated `payload_match` synchronously with the live `Context`, so `{context.active_panel}` resolved at dispatch time. Moving interpolation to promotion meant `None`-context regressed existing templates. PreflightAwait now clones `Context` at register and replays it at promotion. The semantic remains "captured at dispatch" — between dispatch and `.completed` the active panel could change, but the trigger's intent is "match the panel that fired me", not "match whatever is active at promotion".

**Test additions:**

- `payload_match_interpolates_action_result_token` — full happy path: post → capture `ts` from `.completed` → ignore non-matching reply → fire on matching reply, verify the synthesized event carries both `await:` and `action_result:`.
- `action_result_token_missing_field_keeps_match_open` — fail-loud literal preservation.
- `payload_match_interpolates_context_token_captured_at_register` — regression coverage for codex C1 round 1.
- `timeout_with_default_does_not_leak_upstream_action_result` — regression coverage for codex C1 round 2.

**See:** `copad-core/src/trigger.rs::try_promote_or_drop_preflight` (promotion-time interpolation), `copad-core/src/trigger.rs::build_awaited_payload` (new action_result arg), `copad-core/src/trigger.rs::resolve_token` / `resolve_token_value` (action_result branch). The interpolator refactor preserves the no-action_result code path for `Trigger::interpolate` so action `params` interpolation is unaffected — only the awaited-event pathway sees action_result.


## 33. Git workspace file-watcher events (Phase 17.2)

**Problem:** Phase 17.1 shipped CRUD actions for git workspaces and worktrees but no live "something changed in the repo" signal. Status bar widgets, a future git panel, and triggers that want to react to branch/worktree state changes without polling actions all need an event channel.

**Decision:** Add a polling watcher per configured workspace in `plugins/git/src/watcher.rs`. Each watcher thread snapshots `.git/HEAD` (raw line), `.git/refs/heads/**` (recursive loose-ref names), and `.git/worktrees/*` (immediate subdirs) at a fixed interval and emits diffs to the plugin's writer channel as `event.publish` frames.

**Polling, not `notify`:**

- Dependency-free — no new crate, no platform conditional. The `notify` crate would add inotify (Linux) + FSEvents (macOS) backends each with their own pitfalls (FSEvents coalesces edits inside a directory tree, inotify watches per-fd hit a kernel limit fast on many workspaces).
- For status-bar / live-indicator use, 2 s lag is the same order as the user's own click cadence.
- Cheap: a snapshot is `read_to_string(.git/HEAD)` + recursive readdir of refs/heads/ + readdir of worktrees/ — bytes-level fs traffic per workspace per poll.
- The cost of "not real-time" is paid only by these advisory events. `git.worktree_add` and friends still publish their own `<action>.completed` via Phase 14.1's registry fan-out, so chained triggers (Vision Flow 3) hit the bus the instant the action returns.

**Posture decisions:**

- **Loose refs only.** `.git/packed-refs` is intentionally NOT scanned. Branches that exist only there are pre-established as of `git gc` time and don't represent user-initiated changes within the watching window. The trade-off: rare, but a `git gc` running mid-session could collapse loose refs into packed-refs and the watcher would emit spurious `branch_deleted` for them. Acceptable for v1.
- **HEAD-cleared suppresses `git.checkout`.** If `.git/HEAD` becomes unreadable (transient race during operations), the snapshot's `head` is `None`. The diff explicitly skips emitting `checkout {head: null}` — "branch went away" is the `branch_deleted` signal's job. Avoids noisy null-HEAD events during transient races. Tested.
- **First-snapshot baseline.** The watcher loop's first iteration is the baseline; no events fire until the second poll. So a branch created between copad start and the first snapshot is "already there" from the watcher's perspective. Right contract for a polling watcher.
- **Sorted diffs.** Multiple changes within one poll interval are emitted in deterministic order: HEAD first, then alphabetically-sorted creates, deletes, worktree creates, worktree deletes. Avoids racy test assertions and lets downstream triggers rely on a stable order if they care.

**Threading model:** one detached `thread::spawn` per workspace. No clean shutdown — when the plugin's main thread exits (stdin EOF or SIGTERM from supervisor), the OS reaps the process and the watcher threads die with it. A stop flag (`AtomicBool`) is plumbed through so the loop short-circuits between sleeps, but the plugin never actively sets it today; it's wired for a future shutdown-notification handler.

**Init handshake gate (codex review I1 round 1):** Watcher threads sleep on an `initialized: Arc<AtomicBool>` until `handle_frame` flips it on the `initialized` notification, BEFORE taking the baseline snapshot. Without the gate, a `COPAD_GIT_POLL_MS=250` setup with slow plugin startup could publish an event before the supervisor's `initialize` → `initialized` handshake completes, and the host would drop it as out-of-protocol.

**Gitdir resolution for secondary worktrees (codex review C1 round 1):** `Config` validation accepts ANY valid git working tree via `git rev-parse --is-inside-work-tree`, including secondary worktrees where `.git` is a FILE (gitlink: `gitdir: <primary>/.git/worktrees/<name>`) rather than a directory. The naive `<path>/.git/HEAD` read would silently fail for those, and the watcher would emit nothing for a valid workspace. Snapshot now delegates to `git rev-parse --git-dir` (per-worktree gitdir, where HEAD lives) + `--git-common-dir` (shared across worktrees, where refs/heads/ and worktrees/ live). Two `git` shell-outs per snapshot — cheap; could be cached but a 2s cadence makes caching premature. E2E test `snapshot_secondary_worktree_resolves_via_git_rev_parse` verifies `.git`-as-file resolution against a real repo.

**Activation flipped to `onStartup`:** Phase 17.1 ran the plugin lazily (`onAction:git.*`) because actions were the only surface. With watchers, the plugin must be alive whenever copad runs so events flow between action calls. Cheap: a workspace-less config (or zero workspaces) spawns no threads and just sits at stdin.

**Configurability:** `COPAD_GIT_POLL_MS` overrides the default 2000 ms; values below 250 ms are clamped to protect against accidental tight loops. No "interval = 0 = disabled" mode — to disable, remove the workspace entries or `kill -9` the plugin.

**See:** `plugins/git/src/watcher.rs` (snapshot + diff + spawn + 9 unit tests including a real-`git` E2E), `plugins/git/src/main.rs` (`watcher::spawn(...)` after Config load), `plugins/git/plugin.toml` (version bump to 0.2.0, `activation = "onStartup"`, description mentions emitted events).


## 34. coctl jira / slack / calendar subcommands (Phase 19.1c)

**Problem:** Phase 19.1 partial — `coctl todo` (19.1a) and `coctl git` (19.1b) shipped, but daily-use plugins (Jira / Slack / Calendar) still required the generic `coctl call jira.list_my_tickets --params '{}'` shape. JSON-string params are the wrong tool for "check what's assigned to me from the terminal."

**Decision:** Add three thin clap wrappers under `copad-cli/src/plugin_cmds/`, each following the established 19.1a/b pattern (`Subcommand` enum + `dispatch()` + per-arm `call_and_render` against the existing action surface). No new IPC, no new actions.

- **`jira`**: `mine` / `ticket <key>` / `transition <key> <status>` / `comment <key> <text>` / `auth-status`. The `mine` table aligns key + status widths from the response (not fixed) so long-named projects don't push summaries off the screen. `ticket` plucks the four headline fields plus a one-paragraph ADF render so a quick check doesn't require `--json` + `jq`.
- **`slack`**: `send <channel> <text> [--thread-ts]` / `get <channel> <ts>` / `auth-status`. **No `auth login` subcommand** — token capture is a security boundary that the plugin's own interactive `copad-plugin-slack auth` binary owns (keyring write, env-paste UX). Wrapping it through coctl would proxy secrets through an extra hop without value.
- **`calendar`**: `today` / `next [--within Nh]` / `event <id>` / `auth-status`. `today` asks the action for 24h and filters client-side to the local calendar date; without that filter, a tomorrow-00:30 event would leak in because `lookahead_hours` is a duration, not a calendar-day boundary. `chrono` (clock + serde features) added to copad-cli deps for the date math — same crate the calendar plugin already uses internally.

**Posture decisions:**

- **Renderers stay terse.** The `mine` view is `key  status  summary` (3 columns, no truncation); `ticket` is 4 labeled lines + a paragraph; `today` is `<start → end>  <title>` with optional `@ location`. Heavier formatting (icons, color, ANSI tables) lives in a future v2; pipeable plain text is the higher-value default.
- **ADF rendered inline, not via the plugin.** The CLI knows how to walk one paragraph of Atlassian Document Format and pluck `text` nodes. Importing `copad-plugin-jira` as a dep just for `event::adf_to_plain_text` would invert the crate ownership; the inline reader covers the 80% case (descriptions, comment previews) and uses `--json` as the escape hatch for the rest.
- **`--json` everywhere.** Every subcommand emits the raw action payload under `--json`, matching the established `call_and_render` contract. Pipeable + scriptable surface for harness use (`coctl --json jira mine | jq '.tickets[] | select(.status == "In Review")'`).

**Test coverage:** the dispatch + clap surface compiles, but the only meaningful unit-testable logic is the pure render helpers (`adf_first_paragraph`, `format_event_when`). 5 tests across the two helpers cover the path-not-found cases, all-day events, and same-day range compaction. Renderer output formatting itself is verified by hand with `--json` diff (no snapshot tests yet; not worth the ceremony for ~210 LOC modules).

**See:** `copad-cli/src/plugin_cmds/jira.rs`, `slack.rs`, `calendar.rs`; wired through `copad-cli/src/commands.rs` (enum + `unreachable!` arms in `method`/`params`) and `copad-cli/src/main.rs` (dispatch interception before generic path).


## 35. EventBus ring buffer + coctl recent (Phase 19.X)

**Problem:** `coctl event subscribe` exists for live tailing, but "what happened in the last hour while I was AFK?" had no answer. Subscribing live then bombarding triggers retrospectively is the wrong shape; a bounded server-side history is the right primitive. Phase 19.X tracked this as a precondition for the `coctl recent` subcommand.

**Decision:** Add a bounded `VecDeque<Event>` to `EventBus` (default cap 500), expose `bus.history(since_ms, kind_glob)` for in-process readers, and project it through a new `event.history` socket action that takes `{since_ms?, kind?}`.

- **Separate mutex** for the history buffer vs. the subscribers list. The publishing thread acquires `history` briefly to push, then `subscribers` to fan out — a `history()` reader contending the history mutex can't block fan-out, and a slow subscriber can't block history pushes.
- **Push BEFORE distributing.** A subscriber callback that synchronously re-publishes (e.g. registry completion-event fan-out) shouldn't end up with its derived events recorded before the source event. Push the incoming event first, then iterate subscribers.
- **`register_silent` for the action.** `event.history`'s own `.completed` event would otherwise land in the very buffer it just read, inflating every subsequent call's result by one and confusing the "since this timestamp" filter on the next tick.
- **Wire shape (codex C1 round 1):** `{type, data, source, timestamp_ms}` — matches `event.subscribe`'s `Event` shape in `copad-core/src/protocol.rs` (`type` not `kind`, `data` not `payload`) plus an extra `timestamp_ms` so catch-up consumers can render a per-event clock. Initial implementation used `{kind, payload, …}` which would have forced consumers to translate; caught at review.
- **`since_ms` / `kind` strict-typed (codex I1 round 1):** invalid types (number where string expected, etc.) return `invalid_params` instead of silently disabling the filter, matching `event.subscribe`'s reject-on-bad-`patterns` posture.
- **UTF-8 safe CLI truncation (codex C2 round 1):** the human renderer's payload preview truncates by char count, not byte count, so a Korean/Japanese/emoji payload whose 120th byte lands inside a multibyte sequence doesn't panic. Regression test in `payload_preview_does_not_panic_on_multibyte_boundary`.
- **ANSI/OSC control bytes JSON-escaped in CLI render (post-ship visual fix):** `short_value` initially used `format!("\"{s}\"")` which wrote string bodies verbatim through `Display`. An event payload containing raw `\x1b]11;rgb:0/0/0\x1b\\` (OSC 11 set-background) printed unescaped and reconfigured the host VTE's background to opaque black, defeating the window-level background image. Replaced with `serde_json::to_string(v)` which JSON-escapes all variants uniformly (`` instead of literal ESC). Caught by visual e2e — the test suite never wrote ANSI bytes into a fixture. Regression test `payload_preview_escapes_ansi_control_bytes`.

**CLI:** `coctl recent [--since 2h] [--kind jira.*]`. Duration parser accepts `Nh|Nm|Ns|Nd` and bare integer seconds; `--kind` is the same glob `event.subscribe` accepts (`*`, `prefix.*`, exact match). Default human render is `HH:MM:SS  <kind>  <key=val key=val …>` with string values quoted + 40-char truncated; full payload available via `--json`.

**Capacity choice (500):** Most workflow flows are minute-scale, so 500 covers ≥ an hour of typical bus traffic comfortably. Plugin-heavy bursts (e.g. a Jira poll cycle adding 50 events in 30 s) still fit comfortably. The cap is `EventBus::with_capacities`-configurable for future tuning but currently hardcoded — env-var configurability is a v2 polish if real workloads need it.

**Posture:** No persistence across copad restart. The history is a debugging / catch-up affordance, not a durable audit log — pairing it with the existing `~/.cache/copad/event-log.jsonl` ingest path would conflict on retention policy (the cache is bounded by disk, the ring is bounded by count). v1 keeps both surfaces separate.

**Test coverage:** 6 EventBus tests (`history_records_recent_in_arrival_order`, `history_drops_oldest_when_capacity_exceeded`, `history_filters_by_since_ms`, `history_filters_by_kind_glob`, `history_min_capacity_clamped_to_one`, plus the existing pattern tests carry into the glob path). 5 CLI tests cover `parse_duration_seconds` (units + garbage) and `payload_preview` (flat object, long-string truncation, non-object fallback).


## 36. macOS default renderer = alacritty (Phase 10a)

**Problem:** Phases 3 – 6 reached behavioral parity between the alacritty-backed pane (`AlacrittyTerminalViewController` + `copad-term` FFI) and the SwiftTerm path on the slice that matters for daily use: text + cursor + colors + image bg + transparency + Zed materialize + damage-gated CADisplayLink + italic/strike/blink + mouse selection + Cmd+A/C/V + OSC 52 policy gate + OSC 8 hyperlinks + plain URL Cmd+click + scrollback + mouse wheel + Cmd nav + IME preedit + theme/font/security hot-reload. With parity in place, leaving `swiftterm` as the silent default meant new sessions never dogfooded the new path.

**Decision:** Flip `[renderer] backend` default from `swiftterm` → `alacritty` in `RendererBackend.parse` and `CopadConfig.defaults`. Keep SwiftTerm in the codebase as an explicit opt-in fallback (`backend = "swiftterm"`); a Phase 10b removal lands once dogfooding turns up no daily blocker.

- **Why a one-flag fallback, not removal yet:** the alacritty path has known limitations — mouse-mode TUI forwarding (`vim set mouse=a`, `less`, `htop`), no Cmd+/- zoom, no block selection, and a few coordinate edge cases that won't surface until real workloads hit them. The fallback path keeps users unblocked while we iterate.
- **Per-pane semantics, not global:** `rendererBackend` is read at pane-construction time. Flipping the config doesn't re-spawn open panes — they keep their original backend. Matches what every other per-pane config (theme/font/osc52) does on this codebase.
- **Hot-reload prerequisite:** Phase 4b left `applyTheme` / `applyFont` unwired on the alacritty path, which would've been a regression vs SwiftTerm post-flip. Wired both in the preceding commit so editing config feels the same on either backend.

**Posture:** Don't remove SwiftTerm until a confidence period passes (probably a few weeks of daily use with no fallback escapes). When removing, prune `SwiftTerm` Package.swift dep, `CopadTerminalView`, `TerminalViewController`, `URLClickHelper` (currently shared between paths but can absorb into the alacritty renderer or get its own helper module), `applyClampedCursorStyle` and other SwiftTerm-only patches, and the `swiftterm` enum case.

**See:** `copad-core/src/event_bus.rs` (`history` field + `history()` method + `with_capacities` constructor), `copad-linux/src/window.rs` (silent `event.history` registration), `copad-cli/src/plugin_cmds/recent.rs` (CLI + duration parser + payload renderer).


## 37. Trust boundary — origin tagging + `[security]` opt-in (harness-integration step 8-9)

**Problem:** With `coctl event publish` (existing surface) routing socket-driven events onto the bus, any same-UID process holding `COPAD_SOCKET` — including SSH-`RemoteForward`ed hook scripts on a remote box — could synthesize an event. If a trigger binds `system.spawn` to that event kind, this becomes arbitrary same-UID code-exec. Option A (`claude` plugin) wires Claude Code hook scripts directly to `coctl event publish`, so closing this is a prerequisite to landing the claude adapter.

**Decision:** Two-axis opt-in on the trigger TOML, gated at the `TriggerEngine::dispatch` fan-out level (not at any sink trait). One axis says "I accept external events"; the other says "I accept invoking a privileged action". Default-deny on both — existing TOML without a `[security]` block parses cleanly and behaves identically to before for `Internal` events, but cannot fire on socket-published events or on `system.spawn`.

- **Origin as Event field, not publish argument.** `event_bus::Origin { Internal, External }` lives on `event_bus::Event` so that `TriggerEngine::dispatch(&Event, …)` reads it off the dispatched event. Serde `#[default]` = `Internal` for forward-compat with serialized payloads that don't carry the field. The earlier wire-only `protocol::EventOrigin` (dead code stamping `data._origin`) was deleted.
- **Chokepoint is `handle_events_publish` in `copad-daemon/src/socket.rs`.** This is the one path that flips origin to `External`. Every other publisher — plugin stdio, action completion fan-out, time-based wakeups, bridge-relayed events, GUI `_bus.publish` — defaults to `Internal`. Codex round 1 flagged that the existing handler had no origin tagging; round 2 confirmed it as the only socket-driven publish path.
- **Gate at engine fan-out, not at `TriggerSink::dispatch_action`.** Three reasons: (a) the trait has four implementors (`ActionRegistry`, `DaemonTriggerSink`, `LiveTriggerSink`, `FfiSink`); changing the signature would ripple through macOS / Linux / tests for no real win. (b) Privileged-action policy is global; threading it through every sink risks one impl forgetting to check. (c) The gate runs BEFORE the user-supplied `condition` expression, so a misconfigured condition that always returns true cannot subvert the opt-in.
- **`is_privileged_action` is a static `matches!` (codex C4 round 1).** Initial privileged set = just `system.spawn`. It's intercepted before `ActionRegistry` lookup in `daemon_trigger_sink::handle_system_spawn`, so marking it privileged on the registry alone would not have gated the canonical dangerous path. Engine-level static check catches it regardless of sink. `ActionRegistry::register_privileged` for registry-marked actions is a follow-up.
- **`--quiet` only silences transport.** `coctl event publish --quiet` exits 0 when the daemon socket is missing or unreachable so hook scripts don't break when copadd is down. Schema errors (invalid JSON, reserved `.completed`/`.failed` kind) still exit non-zero — a hook author wrote a publish call that can't ever succeed, and silencing forever would hide the bug.

**Known gaps (followups intentionally scoped out)** — flagged during `/codex-plan` round 2 + `/cross-review` rounds 3-4:

- **Bridge wire origin propagation.** `protocol::Event` (daemon↔GUI bridge) has no origin field. External events crossing to the GUI bus arrive as `Internal`. Acceptable: the dangerous action (`system.spawn`) lives daemon-side, so the daemon-side gate is sufficient for Option A's threat model. A Linux GUI-side trigger that calls a privileged action on a hook-originated event would *not* be gated; document and ticket.
- **External events excluded from await state machine entirely.** Codex round 3 flagged the laundering vector: an external event satisfying an internal trigger's `await` clause would synthesize an `Internal`-tagged `<trigger>.awaited` event, letting downstream triggers without `accept_external` chain on external-derived data. Conservative fix in this slice: at `TriggerEngine::dispatch`, skip both `try_promote_or_drop_preflight` and `try_match_pending_awaits` for `External`-origin events. Regression test: `external_event_cannot_satisfy_pending_await`. Trade-off: a legitimate external-accepting trigger (`accept_external = true`) cannot complete via externally-published follow-up events either; only internal follow-ups satisfy its awaits. Round 4 flagged this trade-off; user-confirmed acceptable. Proper fix is per-pending origin storage (`PendingAwait.origin` + propagate to synthesized `.awaited`) — that's the "causal taint" follow-up below.
- **Causal taint for `.completed` events.** Registry `<action>.completed` events default to `Internal` regardless of originating trigger's input event origin. Same fix shape as `.awaited`: completion-stamper inherits origin from the firing trigger's input.
- **macOS FFI / Swift `BusEvent` lack origin.** Out of scope while macOS shell stays a stub.
- **Registry-marked privileged actions.** `ActionRegistry::register_privileged` is not wired. Static list only.

**Test coverage:** 4 EventBus tests (`fresh_event_defaults_to_internal_origin`, `with_origin_overrides_default`, `origin_round_trips_through_serde`, `origin_deserializes_as_internal_when_missing`). 9 trigger tests (`external_origin_event_is_dropped_when_accept_external_false`, `external_origin_event_fires_when_accept_external_true`, `internal_origin_event_fires_without_accept_external`, `privileged_action_dropped_when_allow_privileged_false`, `privileged_action_fires_when_allow_privileged_true`, `external_and_privileged_require_both_opt_ins`, `security_block_defaults_when_absent_in_toml`, `security_block_parses_when_present`, `is_privileged_action_recognizes_system_spawn`, `external_event_cannot_satisfy_pending_await`). 1 daemon socket test extended (`events_publish_works_without_register_and_assigns_source` now asserts `Origin::External`).

**See:** `copad-core/src/event_bus.rs` (`Origin` enum + `Event.origin` field + `with_origin`), `copad-core/src/trigger.rs` (`SecurityBlock` struct, `is_privileged_action`, fan-out gates in `dispatch`), `copad-daemon/src/socket.rs::handle_events_publish` (External tagging), `copad-cli/src/main.rs::dispatch_publish` (`--quiet` flag), `docs/harness-integration.md` § Trust boundary (full design + known-gaps callout).


## 38. `notify.show` action — subprocess-backed desktop toasts (harness-integration step 8 remainder)

**Problem:** Trust boundary (#37) opened the path for harness hooks to publish bus events, but there's no action a trigger can fire to surface them to the user. `harness-integration.md`'s commit-blocked-toast example, the calendar-imminent alert, the discord-mention chime — all need a cross-platform "show a desktop notification" primitive. Without it, the harness loop has no visible output.

**Decision:** Add a `Notifier` trait in `copad-core` with subprocess-backed implementations (`notify-send` on Linux via libnotify, `osascript` on macOS), register `notify.show` on both the daemon's and the GUI's in-process `ActionRegistry`, and mark it `register_blocking_silent` so the ~10 ms subprocess runs on the action thread pool without stalling the trigger pump.

- **Subprocess over zbus / direct D-Bus** — `harness-integration.md`'s original design called for zbus primary + notify-send fallback. Reversed: subprocess is the only path. Reasons: (a) `notify-send` is universally available on every Linux desktop copad targets (Arch, Debian/Ubuntu, Fedora) and is the canonical libnotify entry point; (b) the human-driven trigger rate makes ~1 ms fork cost irrelevant; (c) avoids adding ~1 MB of zbus + serde-into-DBus boilerplate for a one-action surface; (d) macOS already needs subprocess (`osascript`) — keeping the impl symmetric is a code-shape win. If burst rates from high-fan-out triggers (life-assistant, Option H) ever prove the subprocess overhead matters, swap the Linux impl behind the same trait without touching the action surface.
- **`register_blocking_silent` for the action.** Codex round-1 flagged a real risk: `register_silent` alone would run the subprocess on the calling thread (GUI's GTK tick or daemon's trigger pump), stalling 10 ms per fire. Blocking variant routes the handler off the dispatch thread — on the daemon side, the bounded action thread pool (`ActionRegistry::with_pool`) consumes the job; on the GUI side, the GUI's `ActionRegistry` is constructed without `with_pool`, so blocking actions fall back to `std::thread::spawn` (one-off thread per call). Both paths keep the dispatch thread free; the daemon path additionally bounds concurrency. Silent suppresses `.completed` fan-out because the toast IS the user signal — emitting a bus event for every notification would spam downstream subscribers without value.
- **Registered on BOTH daemon and GUI in-process registries.** Codex C1 round 1: the Linux GUI still hosts its own `TriggerEngine` when `COPADD_HOST_TRIGGERS=OFF` (the current default). A daemon-only registration would mean GUI-resolved triggers see `unknown_action` on `notify.show`. Both registries call the same Linux Notifier via `copad_core::notifier::platform_notifier()` so the toast fires regardless of which engine path resolves the trigger.
- **macOS escaping via `osascript -e 'on run argv'` wrapper.** Codex C3 round 1: splicing `title`/`body` into AppleScript source with manual `"` / `\` escapes is fragile, and trigger params can contain arbitrary user-controlled strings (Slack message bodies, Discord nicknames). The `on run argv` wrapper accepts title/body as `osascript` argv values — AppleScript treats them as opaque AS string objects, no escape needed regardless of payload. Wrapper:
  ```
  osascript -e 'on run argv\n  display notification (item 2 of argv) with title (item 1 of argv)\nend run' "TITLE" "BODY"
  ```
- **Hard-truncate at 256 / 4096 bytes** for title / body before subprocess. A runaway trigger that interpolates a 50 KB Slack body has no business as a toast; truncating respects UTF-8 char boundaries (`is_char_boundary` scan) so non-ASCII payloads don't panic on the slice. Ellipsis appended visibly.
- **Failure surface — log-only by design.** Subprocess wait + exit-status propagation surfaces honest errors via `NotifyError::Spawn` / `NonZeroExit { stderr }`. The handler logs `warn!` with the error and returns `Err(internal_error)` to the registry. Because the action is `register_blocking_silent`, `ActionRegistry` suppresses **both** `<action>.completed` AND `<action>.failed` fan-out (silent = no completion bus traffic in either direction). Net result: notification subprocess failures are visible in daemon logs (`journalctl --user -u copadd` or `/tmp/copadd*.log`) but downstream triggers cannot chain on `notify.show.failed` to retry or fall back. Reasoning: chaining on toast failure is a hypothetical use case; in practice if the toast didn't fire the user wasn't paying attention to the screen anyway, and trying a second toast on the same broken `notify-send` would just fail again. If a real chain need ever surfaces (e.g. fall back to email on libnotify failure), the right fix is to drop the silent qualifier (`register_blocking` instead) plus filtering completion events at subscribers, not a new registry surface. Codex round 1 caught the original brief's incorrect "failed event fires" claim — the code was right; the documentation was wrong.
- **Test coverage via `NoopNotifier` injection.** Daemon's `register_notify_show(actions, notifier)` takes the Notifier as an `Option<Arc<dyn Notifier>>` parameter so tests can wire a `NoopNotifier` that captures `(title, body, level)` triples without spawning real subprocesses. 9 unit tests in `copad-core::notifier::tests` (Level mapping, serde, UTF-8 truncation, capture); 7 daemon tests covering param validation (missing/empty title, bad level, body=null fallback), default-level resolution, all-level acceptance, no-platform-notifier graceful drop, and the regression guard that `<action>.completed` does NOT fan out (blocking-silent contract).

**Out of scope (followups):** icon attachment, action buttons, urgency timing controls, persistent notification history. Triggers that need rich notifications can shell out via `system.spawn` (now privileged-gated, see #37). Configuring the user's notification daemon (dunst/mako settings) is also out — `copad` is a producer, not a notification server.

**See:** `copad-core/src/notifier.rs` (trait + Linux/macOS impls + `NoopNotifier` + `platform_notifier()` factory + 9 unit tests), `copad-daemon/src/main.rs::register_notify_show` (action handler + 7 tests in `mod tests`), `copad-linux/src/window.rs` (GUI in-process mirror registration), `docs/harness-integration.md` § Cross-cutting needs (subprocess decision documented).


## 39. `host_triggers` default flipped ON (harness-integration Option A slice 1A)

**Problem:** With `COPADD_HOST_TRIGGERS=OFF` as the default, daemon-side `TriggerEngine` was idle by design — Stage A (#5b.2 in #step roadmap) shipped the engine, but waiting on cut-over evidence before flipping default. Stages B+C delivered atomic cut-over (registered GUIs release their in-process engine when daemon's `gui.register` ack carries `host_triggers=true`), so the original "wait until proven" gate has been satisfied for ~weeks of dogfooding. With trust boundary (#37) + `notify.show` (#38) in place, the missing piece for the first visible harness loop was: a `coctl event publish claude.commit_blocked` call must actually fire a configured trigger end-to-end. With `host_triggers=OFF`, the daemon dispatched zero triggers — every harness publish went to the bus, sat in the ring buffer, and was ignored.

**Decision:** Flip `COPADD_HOST_TRIGGERS` default from OFF to ON, with deliberate opt-out semantics. Concretely:

- `env unset` → ON.
- `env in {"0", "false", "no", ""}` (case-insensitive, trimmed) → OFF.
- `env = any other value` (including `"on"`, `"yes"`, `"true"`, garbage like `"fasle"`) → ON.

The garbage-bias-enabled rule is deliberate: a typo like `COPADD_HOST_TRIGGERS=fasle` silently turning off harness flow would be a confusing failure mode. The disable list is small and intentional; opting out has to be unambiguous.

- **Pure parser, not env-mutating tests.** Codex C1 round 1 caught: the first version threaded the env var name into `env_flag_default_on(var: &str)` and tested by mutating process env via `set_var/remove_var`. Cargo runs tests in parallel; `std::env::set_var` is unsound under concurrent access per the std module's safety contract (glibc internals lack a write lock around `environ`). Refactored to `env_flag_default_on(value: Option<&str>) -> bool` — pure, no env reads in the function body. Caller resolves `std::env::var(ENV_HOST_TRIGGERS).ok().as_deref()` once at startup. Tests pass `None` / `Some("0")` / `Some("fasle")` directly. No env mutation anywhere.
- **Hot reload doesn't re-consult the flag.** Codex round 2 verified: `spawn_config_watcher` receives an already-built `engine` and `pump_state`; it operates on those instances without re-reading `COPADD_HOST_TRIGGERS`. So the flag is a startup-only decision — toggling the env mid-flight requires a daemon restart, by design (and matches every other env-driven setting).
- **`examples/triggers/claude-hooks.toml`** ships alongside the flip. Two trigger blocks: `claude-commit-blocked-toast` (fires on the hook-published external event, displays a warn-level `notify.show`) and `claude-review-approved-toast`. Both carry `[triggers.security] accept_external = true` — the schema path `Trigger.security` parses from the nested table (verified by #37's `security_block_parses_when_present` test). Comments in the file document the hook-script copy-paste lines users add to their own `~/.claude/scripts/*.sh`. NOT auto-applied to user dotfiles.

**Plugin actions (`claude.last_handoff`, `claude.list_dirty`) deferred to slice 2.** Codex round 1 pushed back hard: the first visible harness loop (external event → toast) does NOT need the plugin crate. The trust boundary + notify.show + a trigger example covers the whole path. The plugin actions are data-surfacing for a future monitor panel (Option B), not on the critical path for slice 1. Slicing here keeps the diff small and reviewable.

**Live verification:** Daemon log line `trigger engine: 8 configured | 11 bus pattern(s) | dispatch=ON` (was `dispatch=OFF` before the flip with the same config + same 8 trigger count, just 0 bus patterns because PumpState wasn't constructed). End-to-end: with user-config trigger installed, `coctl event publish claude.commit_blocked --quiet '{"reason":"test"}'` fires the trigger and the configured `notify.show` action surfaces a desktop toast.

**Out of scope (for slice 1A; tracked in slice 2 backlog):**
- `plugins/claude/` Rust crate + `claude.last_handoff` + `claude.list_dirty` actions.
- More trigger examples (`session-stopped-todo`, etc.).
- Hook script auto-patching (intentional: dotfiles are user territory).
- macOS install script — Linux path validated; macOS daemon-host path is part of step 7 (systemd unit + launchd plist).

**Test coverage:** 3 unit tests on the pure parser (unset enables, disable-token list, garbage enables). Existing tests for hot reload + trigger dispatch unchanged.

**See:** `copad-daemon/src/main.rs` (`env_flag_default_on` parser + call-site swap + 3 unit tests in `mod tests`), `examples/triggers/claude-hooks.toml` (trigger blocks + hook-script copy-paste guide), `docs/harness-integration.md` § Sequencing step 10 (Option A slice 1A marked done).


## 40. `copadd` auto-start via systemd `--user` / launchd (harness-integration step 7)

**Problem:** Through #37 / #38 / #39, the harness loop wires up: hook → `coctl event publish` → trust-boundary gate → trigger → `notify.show` → toast. But `copadd` itself had no production install path — running it required `nohup ~/.local/bin/copadd &` from a live shell, and a reboot or fresh SSH session reached a daemon-less host where every `--quiet`-flagged publish silently no-op'd. User asked: "이게 있어야 그냥 ssh로 세션 시작해도 바로 작업할 수 있는 거 아니야?" — exactly. Step 7 closes that gap.

**Decision:** Ship two service templates + wire install scripts to drop them and enable the unit:

- **Linux**: `dist/systemd/copad-daemon.service` (user-level unit). `install-dev.sh` (default `--with-daemon` on) copies it to `~/.config/systemd/user/`, runs `daemon-reload` + `enable` + `restart`. The explicit `restart` (rather than `enable --now`) covers re-installs where the binary changed but the unit was already active — `--now` is a no-op for a running service and would silently keep the old binary executing. `ExecStart` carries a `COPADD_BIN_PATH` placeholder rewritten at copy time so `--user` and `--system` installs both point at the right path. systemd restarts on crash (`Restart=on-failure`, `RestartSec=10`), pipes logs through the journal at `RUST_LOG=info`.

- **macOS**: `dist/launchd/com.marshall.copad.daemon.plist`. `install-macos.sh` (default `--with-daemon` on) rewrites `HOME_PLACEHOLDER` tokens to `$HOME`, drops the plist at `~/Library/LaunchAgents/`, runs `launchctl bootout` (idempotent) then `launchctl bootstrap gui/$UID`. `KeepAlive=true` + `RunAtLoad=true` gives the same crash-restart + login-start semantics. `ProcessType=Interactive` matches Aqua-app scheduler priority so trigger pump latency stays human-acceptable.

**Key decisions in the unit/plist text:**
- **`Restart=on-failure` not `always`** — exit 0 (graceful `systemctl stop`) shouldn't restart. Mirrors KeepAlive's "always running" but only on unexpected exit; deliberate manual stop is honored.
- **`After=default.target` (Linux)** — wait until the user session has `XDG_RUNTIME_DIR` mounted + DBus address set before the daemon tries to bind its socket / talk to plugins.
- **`%h` placeholders in systemd, `HOME_PLACEHOLDER` rewrite in plist** — systemd resolves `%h` at unit load; launchd doesn't expand `~` and has no equivalent specifier, so the install script must substitute. Same shape — different mechanism per platform.
- **`LimitNOFILE=4096`** on Linux — defense in depth against a runaway plugin fork-bomb landing through the supervisor. The daemon's own thread pool is already bounded; this is the kernel-level ceiling.
- **Logs go to journal (Linux) / `~/Library/Logs/copad-daemon.{out,err}.log` (macOS)** — matches what users already know to look at on each platform. `tail` works on macOS; `journalctl --user -u copad-daemon -f` works on Linux. Avoid forcing a custom log path on either.

**Bundled `copadd` install fix.** `install-dev.sh` previously only installed `copad` + `coctl` — the daemon binary was an orphan that users had to `cp target/release/copadd ~/.local/bin/` by hand. The script now bundles it (the for-loop iterates `copad coctl copadd`), so a single `install-dev.sh` invocation gives the full GUI + CLI + daemon set. Pre-existing manual installs are silently overwritten; the daemon owns no on-disk state beyond the socket file (recreated on bind), so no state migration is needed.

**Linger left as user policy.** `loginctl enable-linger <user>` is documented + reminded by `install-dev.sh` when it's OFF, but not auto-enabled. The choice ON vs OFF is meaningful — boot-time start (linger ON) vs first-login start (linger OFF). Each has trade-offs: linger ON wastes resources for users who don't open SSH while logged out; linger OFF means cron / background jobs can't publish events to the daemon. Document the choice; user opts in if they need it.

**SSH `RemoteForward` documented** in `docs/harness-hooks.md` § "SSH + daemon lifecycle". Cross-machine hook flow: workstation runs `copadd`, remote box forwards `$XDG_RUNTIME_DIR/copad/socket` via SSH config, hooks on remote publish events that land on the workstation's bus, triggers fire there, toasts appear on the workstation desktop. The trust boundary (#37) handles SSH-forwarded events the same as local socket publishes — `External` origin uniformly — so no special-case SSH plumbing was needed in the daemon.

**Live verification (Linux):**
- `systemd-analyze --user verify dist/systemd/copad-daemon.service` clean (no output).
- `install-dev.sh --with-daemon` against a running prior daemon: killed the manual `copadd`, copied the unit, `daemon-reload` + `enable` + `restart` brought up a new one. `coctl ping` returned OK against the systemd-managed instance. Re-install verified PID change (`MainPID` 2147258 → 2210576).
- `kill -9 $(systemctl --user show -p MainPID --value copad-daemon)` triggered the 10s back-off, then systemd respawned. Verified by polling `systemctl --user is-active`.

**macOS** — plist passed `xmllint` + Python's `plistlib.loads`. First install on a real macOS box is left to the user (same trust-the-template model as `install-macos.sh`'s existing flow).

**Out of scope (followups):** `install-macos.sh` currently uses `cargo install --path copad-daemon`, which always lands the binary at `~/.cargo/bin/copadd` regardless of the `--system` flag (`--system` only flips the `.app` destination between `~/Applications` and `/Applications`). The plist matches that — `ProgramArguments` hardcodes `~/.cargo/bin/copadd`. If a future macOS install path migrates copadd to a global location (`/usr/local/bin` or `/opt/`), the plist needs a parallel path placeholder. Auto-enable linger on Linux — declined; per-user policy.

**See:** `dist/systemd/copad-daemon.service`, `dist/launchd/com.marshall.copad.daemon.plist`, `scripts/install-dev.sh` (`--with-daemon`/`--no-daemon` + `copadd` in the binary loop), `scripts/install-macos.sh` (same flag pair + LaunchAgent install block), `docs/harness-hooks.md` § "SSH + daemon lifecycle".


## 41. Presence-aware harness routing (harness-integration Slice 1 — Discord)

**Problem:** After #39 (host_triggers ON) + #40 (daemon auto-start), the harness loop `hook → publish → trigger → toast` works locally. But the toast is invisible the moment the user leaves the keyboard — a Claude session that blocks on commit review while the user is at lunch is silent until they walk back to the desk. The grand vision was a `notify-webhook` first-party plugin (universal HTTP POST sink, secret URL via `${ENV_VAR}` expansion), explicitly so the second sink (ntfy/Slack/PagerDuty) would cost near-zero to add — the Rule-of-Three abstraction priced in up front.

**Decision:** Cancel the `notify-webhook` plugin scaffolding for Slice 1. Reuse the existing `plugins/discord/` plugin (already shipping v0.3.0 with a Gateway WebSocket + `discord.send_message` action + bot-token keyring) and gate it behind a new `Context.presence` field. Concretely:

- **`copad-core/src/context.rs`** — new `Presence { Active, Away }` enum (serde `rename_all = "lowercase"`). `Context` gains a `presence: Presence` field. `ContextService` exposes `presence() -> Presence` + `set_presence(Presence) -> Presence` (returns previous, so callers can skip no-op broadcasts).
- **`copad-core/src/condition.rs`** — `context.presence` joins `context.active_panel` / `context.active_cwd` as a supported reference root. Resolves to the lowercase string `"active"` / `"away"`. Grammar unchanged; only the resolver matches a new field name. Codex round 1 caught the bare-root version (`presence == "away"`) violating the existing `context.X` / `event.X`-only root rule.
- **`copad-daemon/src/main.rs`** — registers two new silent actions next to `context.snapshot`: `presence.set { state: "active" | "away" } -> { previous, current }` and `presence.get { } -> "active" | "away"`. On a non-noop `set`, publishes `presence.changed { previous, current }` on the bus (source `daemon`) so downstream triggers / GUI indicators can react. `presence.get` returns a bare JSON string (not `{ state: "..." }`) so `coctl presence status` lands a single word on stdout — shell-script friendly.
- **`copad-cli`** — `coctl presence away | active | status`. The first two POST to `presence.set`; `status` POSTs to `presence.get` and the existing `print_result` handles the bare-string response naturally.
- **User config** — each presence-gated trigger is a **second `[[triggers]]` block** for the same event_kind, with `action = "discord.send_message"` + `condition = 'context.presence == "away"'`. The local `notify.show` trigger stays without condition, so toasts fire unconditionally. `examples/triggers/claude-hooks.toml` ships the pattern with `REPLACE_WITH_YOUR_CHANNEL_ID` placeholders + setup notes pointing at `plugins/discord/plugin.toml` for bot auth.

**Why not the abstract sink plugin (Rule of Three):**

- Discord plugin already exists, already authed, already keyring-managed, already gateway-connected. Building a parallel `notify-webhook` sink would duplicate that path for zero immediate gain — the user wanted Discord, not ntfy.
- `discord.send_message` is a richer action surface than raw webhook (failure code shape per Discord API, bot-aware routing, ratelimit handling). A first-class plugin owning that complexity is the right home.
- Universal sink abstraction was a guess about a future second sink. We don't have it yet. Rule of Three says wait for the second real case — ntfy / Slack incoming webhook / PagerDuty — and let the second integration reveal the actual common shape. Premature abstraction would have shipped a string-template engine that the second sink might not even want.
- Trigger interpolation already does NOT expand `${ENV_VAR}` — channel id is a literal string in user config. Discord channel id is not a secret (it's user-discoverable in any client with Developer Mode on); bot token is, and that's already in keyring via `copad-plugin-discord auth`. So the "secrets in env" motivation for the abstract sink was solving a non-problem in the discord path.

**Trade-offs accepted:**

- Second sink (ntfy) adds friction — when it lands, we'll need to either (a) build the abstract sink then, or (b) ship a `plugins/notify-ntfy/` mirror with the same gating idiom (`condition = 'context.presence == "away"'`). Option (b) wins until we see what differs between Discord and ntfy in the actual payload shape needs.
- Presence is process-memory only — daemon restart resets to `Active`. Acceptable for v1; persistence is a follow-up if user reports surprising silence after a daemon respawn.
- Presence is **manual toggle only** — Hyprland idle / copad window-focus auto-detect is Slice 2 (driven by a `presence.suggest` event source feeding into the same `set_presence` path).

**Live verification (e2e, 2026-05-23, session 265e30d2):**
- Phase 1 — `presence=active` baseline + `coctl event publish claude.review_approved` → `claude.review_approved` event landed on bus, **zero** `discord.send_message.completed` events. Gate works.
- Phase 2 — `coctl presence away` → `presence.changed { previous: "active", current: "away" }` on bus, `coctl presence status` prints `away` to stdout. Re-publish → `discord.send_message.completed { channel_id: 1507255807132176394, message_id: 1507424078598766792 }` on bus, message arrived in the user's Discord channel (visual confirmation).
- Phase 3 — `coctl presence active` → `presence.changed { previous: "away", current: "active" }`, re-publish → zero new `discord.send_message.completed`. Gate flips both directions cleanly.
- Edge — `coctl event publish claude.commit_blocked --quiet '{}'` (empty payload) processes without panic. Missing `event.reason` / `event.repo` resolve to literal-tokens in the interpolated content per existing `trigger::interpolate_string` semantics.
- All 207 + 114 + 7 workspace unit tests pass (`copad-core` + `copad-daemon` + `copad-linux`). `cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` clean.

**Out of scope (Slice 2/3 backlog):**
- Presence auto-detect (Hyprland idle hint + copad window-focus event). Source: `hyprctl activeworkspace` watcher OR `swayidle`-style daemon. Sinks into `presence.set` via the same socket method.
- HTTP/WS dashboard server (`plugins/web-bridge/`). Two-way control surface — pane capture live stream + `tmux send-keys` injection from remote browser. Slice 3.
- Universal sink abstraction (`plugins/notify-webhook/`). Defer until second-sink case (ntfy/Slack/etc.) materializes — design from the diff between the two actual integrations, not from speculation.
- Webhook retry / back-off / dead-letter queue. Discord plugin's existing `discord.send_message.failed` event suffices for first-version observability.
- Presence persistence across daemon restarts.

**See:** `copad-core/src/context.rs` (`Presence` enum + `Context.presence` + `ContextService::set_presence/presence`), `copad-core/src/condition.rs` (`context.presence` resolver arm + doc comment), `copad-daemon/src/main.rs` (`presence.set` / `presence.get` action registrations next to `context.snapshot`), `copad-cli/src/commands.rs` (`PresenceCommand` enum + method/params dispatch), `examples/triggers/claude-hooks.toml` (second trigger block per event with `condition = 'context.presence == "away"'`), session intent `~/docs/sources/sessions/copad/2026-05-23-session-265e30d2.md`.


## 42. `plugins/web-bridge/` — HTTP+WS broker as a first-party plugin (harness-integration Slice 3.0)

**Problem:** Slice 1B closed the outbound half of the remote-harness loop (Discord alerts when `presence == away`), but coming back the other way still required SSH into the box and `coctl` from the phone — a real-world phone interface needs *both* a live view of what copad is doing AND a way to send a one-liner back without a separate ssh session. Slice 3 builds that as the inbound half. The plan, after codex-plan round 1 + 2, was a first-class plugin under `plugins/web-bridge/` that brokers copadd's socket surface (terminal.* / session.list / presence.set / event.subscribe) over HTTP+WebSocket.

**Architecture decision — option (δ) over (α), (β), (γ).** Four architectures were on the table:

- **(α) Standalone binary + systemd `--user` unit.** `plugins/web-bridge/` Cargo crate but no `plugin.toml`; install scripts grow to ship a unit. Lifecycle owned by systemd. Single-OS (Linux) without parallel launchd handling.
- **(β) Daemon-embedded axum listener thread.** Smallest LOC; HTTP/WS deps and remote-control attack surface land permanently in `copadd`. Codex round 2 flagged this hard against: a daemon with no plugin isolation owns the bridge forever.
- **(γ) New plugin-model variant.** Extend `plugin.toml` with a long-running-listener service type that skips stdio RPC. Cleanest abstraction, but pays a `copad-core` model change for the first listener-plugin instance. Codex: abstraction before evidence.
- **(δ) Service plugin with both stdio RPC and own HTTP listener + raw daemon-socket client.** web-bridge IS a service plugin like discord/todo, implements the existing stdio RPC handshake minimally (initialize handshake + idle), AND in the same process runs an axum HTTP+WS listener, AND opens raw daemon-socket connections (mirroring `coctl`'s sync `UnixStream` client) so requests flow through the normal daemon `dispatch()` path including `GuiRegistry` routing.

Codex round 2 hard-recommended (δ). Discord plugin already proves the "stdio RPC + long-running outbound network thread" pattern: `plugins/discord/src/main.rs:7/130/166/186/218` (RPC loop + Gateway WebSocket spawn). Generic clients hit `dispatch()` → `GuiRegistry` cleanly with no `gui.register` required ([socket.rs:537/927](/home/marshall/dev/copad/copad-daemon/src/socket.rs:537)); Unix socket 0600 is the only auth gate.

**Two corrections vs. naïve (δ):**

1. **`COPAD_SOCKET` env inject is NOT a one-liner.** `ServiceSupervisor::new` had no socket-path field. The fix threads `socket_path: PathBuf` through the constructor, stores it on the struct, and injects `COPAD_SOCKET=<path>` into the child env alongside the existing plugin metadata env (`COPAD_PLUGIN_NAME` / `COPAD_PLUGIN_DIR` / `COPAD_SERVICE_NAME`). 10 test call-sites updated with a placeholder path. Future listener-style plugins (ntfy bridge, prometheus exporter, …) get the same env for free.

2. **`event.subscribe` takes over its socket connection** ([socket.rs:500](/home/marshall/dev/copad/copad-daemon/src/socket.rs:500)). A single connection cannot serve both RPC and subscription. The plugin uses TWO daemon-socket connections: one per-request RPC (open-write-read-close) and one long-lived subscription (open-write-read-forever). Both connections from the same plugin process; the daemon has no `target_client_id` to distinguish, but it doesn't need to — each is a generic client.

**Implementation shape:**

- `plugins/web-bridge/` — Cargo crate (`copad-plugin-web-bridge`), `plugin.toml` (`onStartup`, `provides=[]`), `src/main.rs` (stdio handshake + tokio runtime spawn on dedicated thread), `src/daemon_client.rs` (RPC + subscribe), `static/index.html` (inlined SPA via `include_str!`).
- Auth: `Authorization: Bearer <token>` for `/api/*`, `Sec-WebSocket-Protocol: bearer.<token>` for `/ws/*`. **Query-string token is never accepted** — leak path via referrer/access logs. Token must be ≥32 chars; missing/short → plugin exits before binding (fail-closed). `COPAD_WEB_BRIDGE_TOKEN` env. `COPAD_WEB_BRIDGE_BIND` env (default `127.0.0.1:7575`).
- HTTP endpoints: `GET /` (SPA), `GET /healthz` (public), `GET/POST /api/presence`, `GET /api/panes` (wraps `session.list`), `GET /api/panes/:id/recent?lines=N` (wraps `terminal.history`, clamps 1..=200), `POST /api/panes/:id/input` (wraps `terminal.feed`), `GET /api/events?since_ms=N&kind=...` (wraps `event.history`).
- WS `/ws/events`: holds one daemon `event.subscribe` connection with patterns `["presence.*", "claude.*", "discord.send_message.*", "notify.show.*"]`; forwards each event line through a `tokio::sync::mpsc(64)` channel to the WS client. Slow clients (channel full) drop the connection. RFC6455-compliant subprotocol echo (`bearer.<token>` echoed back so browsers don't close on handshake).
- UI: vanilla HTML/CSS/JS (~270 LOC), mobile-first, dark theme. Header with presence toggle. Pane list + recent-output viewer + input textarea + send button. Live event feed sidebar (WS). `no_gui` banner when copad GUI isn't running (presence + events still work; pane control disabled).

**Subscribe implementation note (sync over `spawn_blocking`).** The initial implementation used `tokio::net::UnixStream + into_split` for the subscribe connection. Connect + write succeeded, but the read side never observed any line (not even the daemon's `{"status":"subscribed"}` ack). The same socket protocol works fine with `std::os::unix::net::UnixStream` (which is exactly what `coctl-cli/src/client.rs` uses). Rather than chase a tokio I/O scheduling issue mid-slice, the subscribe path now runs inside `tokio::task::spawn_blocking` with the std sync UnixStream — same pattern as the validated CLI client. RPC (one-shot request/response) still uses tokio async UnixStream because *that* path works. Root-causing the async subscribe failure is a follow-up task.

**Live verification (e2e, 2026-05-23, dev box):**

- `curl -H "Authorization: Bearer <token>" http://127.0.0.1:7575/api/presence` → `{"state":"active"}`.
- `curl -X POST -d '{"state":"away"}'` → `{"current":"away","previous":"active"}`; `coctl presence status` confirms `away`. Reverse flip works.
- `curl http://127.0.0.1:7575/api/panes` (with bearer) → real panel array `[{focused, id, tab, title, type}]` (GUI was running). Without bearer → 401.
- `claude.review_approved` publish during `presence==away` → `discord.send_message.completed` lands in test channel (Slice 1B routing intact through the new HTTP entry point).
- WS handshake to `/ws/events` with `Sec-WebSocket-Protocol: bearer.<token>` → HTTP 101 + `sec-websocket-protocol` echo. Holding the connection open while flipping presence yielded 3 WS frames: `{status:"subscribed"}` ack + 2 `presence.changed` events.
- Query-string token (`?token=…`) on `/api/presence` or `/ws/events` → 401. WS without `bearer.*` subprotocol → 401.
- `cargo fmt --all` clean. `cargo clippy --workspace --all-targets -- -D warnings` clean. 9 unit tests (bearer middleware + WS subprotocol parser + constant-time eq).

**Trade-offs accepted:**

- Path A only (web-native chat-style UI). Full PTY xterm.js + mobile keyboard toolbar is Slice 3.1, layered on the same plugin. Decision: ship the Discord-reply-style closed loop fast, see what's actually missing before adding a 60-cell xterm.js viewer on a phone screen.
- Single registered GUI assumption (primary). Multi-GUI `target_client_id` routing is a follow-up — when the user runs multiple copad windows simultaneously this matters.
- No retry/back-off on daemon-socket connection failures. WS subscription drop → reconnect is the user's responsibility (the dashboard's JS already reconnects every 2 s on close).
- Token rotation = env-var change + daemon restart. No revocation UI.
- HTTPS termination is the user's perimeter responsibility — Tailscale, cloudflared, or `ssh -L`. The plugin only binds localhost by default.

**Out of scope (Slice 3.1+ backlog):**

- Full PTY xterm.js + mobile keyboard toolbar (`Esc Tab Ctrl / | ↑↓←→`).
- Pane stream via `terminal.output` event WS (currently UI polls `/api/panes/:id/recent` on demand).
- PWA manifest + service worker + Web Push.
- Multi-pane split view.
- `terminal.exec` (output-waiting) endpoint.
- Multi-GUI explicit `target_client_id` routing.

**See:** `plugins/web-bridge/` (crate + plugin.toml + static SPA), `copad-daemon/src/service_supervisor.rs` (COPAD_SOCKET env inject + socket_path field), `copad-daemon/src/main.rs` (`activate_supervisor` socket_path threading), `docs/harness-integration.md` § A Slice 3.0, session intents `~/docs/sources/sessions/copad/2026-05-23-session-3a87a228.md`.


## 43. `plugins/web-bridge` Slice 3.1 — tmux as data model + xterm.js attach (harness-integration Slice 3.1)

**Problem:** Slice 3.0 shipped a Discord-reply-style chat UI but it was unusable for the actual remote-harness loop. User feedback: "어떤 명령들이 진행되는지도 안 보이고 상태도 안 보이고 아무것도 보이는 게 없어서 아예 쓸 수가 없는 상태." Snapshot-only `terminal.history` polling can't surface what's happening in real time, and Path A's chat input is too narrow for actual interactive work (vim, claude session, tmux).

**Architecture pivot.** The first try at Slice 3.1 (full xterm.js attach to copad's PTY) hit codex-plan round 1 hard blockers (decisions.md #42 trade-off list): `terminal.output` is a keystroke-publish event not PTY child output, no `terminal.resize` socket method, GUI-to-daemon event forwarding explicitly excludes terminal events, and the event wire is JSON not raw bytes. A real "copad PTY in the browser" path would need 2-3 weeks of daemon + GUI surface work.

**Pivot: tmux is the data model.** 99% of the user's actual work runs inside tmux (they built `~/dev/tmx` for it). Web-bridge shells out to tmux directly — `tmux list-panes -a` for overview state, `capture-pane` for per-pane previews, `attach-session` (inside a portable-pty PTY pair) for live bidirectional xterm.js, `paste-buffer` for one-shot text injection. No copad/daemon changes; tmux's multi-client model handles the shared view between copad's GUI client and the mobile web-bridge attach.

**Codex round 1 of this revised plan caught 7 design issues** — all reflected:
- `pane != window`: tmux windows can hold multiple panes. Switched to `%pane_id` as primary key (`tmux list-panes -a`, not `list-windows`).
- `attach-session` is session-oriented; per-window attach is not a real model. Slice 3.1 attaches to the session and pre-positions via `tmux select-pane -t %N` so the new client lands on the right pane; multi-client view is shared with the existing copad GUI client (acceptable for "two views of one work" use case; grouped sessions for independent views are Slice 3.2).
- `send-keys "<text>"` is unsafe for multiline / quoted / special-char input. Switched to `tmux load-buffer -b <name> -` (stdin) + `paste-buffer -p -d -b <name> -t <target>` — same pattern `copad-linux::socket::handle_claude_start` (socket.rs:1710) uses for prompt seeding.
- WS Text frames for PTY bytes corrupt multi-byte sequences at chunk boundaries. Slice 3.1 uses **WS Binary frames** for PTY (both directions); **WS Text frames** carry JSON control (`{type:"resize",rows,cols}` only for now).
- Dropping PTY bytes on backpressure corrupts xterm state permanently. Bounded `tokio::sync::mpsc<Vec<u8>>(256)` (~4 MiB worst case at 16 KiB chunks) — when full, close the WS rather than drop. NEVER drop terminal bytes.
- Overview diff protocol has rename/close/cwd-change edge cases. Slice 3.1 pushes a full snapshot every 5 s instead — simple, correct, no tombstones needed for a refresh-rate use case.
- `/api/tmux/panes` had an N+1 risk (per-session list-windows + per-window capture-pane). Slice 3.1 does `list-panes -a` in ONE shell-out, plus per-pane `capture-pane` (still N but proportional, not N×M). 2 s in-plugin cache.

**Implementation shape:**
- New `plugins/web-bridge/src/tmux.rs` module: `list_panes` (single `-F` shell-out), `capture_pane(pane_id, last_n)`, `send_text(target, text)` via load-buffer + paste-buffer, `find_pane` lookup helper.
- New HTTP endpoints: `GET /api/tmux/panes` (composite list + per-pane previews with 2 s TTL cache), `POST /api/tmux/send` (target + text).
- New WS routes: `GET /ws/tmux/overview` (5 s timer → full snapshot JSON push; bails on client close via select), `GET /ws/tmux/attach/:pane_id` (Binary frames bidirectional PTY + Text frames for resize control).
- Attach lifecycle: pre-spawn `tmux select-pane -t :pane_id` to position; spawn `tmux attach-session -t <session>` inside portable-pty PtyPair; reader on `spawn_blocking` task → `mpsc(256)` → WS Binary; writer wrapped in `Arc<Mutex<>>` driven from `spawn_blocking` per client byte burst; resize routed through a dedicated `spawn_blocking` task that owns the `Box<dyn MasterPty + Send>` (the trait is `Send + !Sync` so it can't cross awaits).
- SPA: two modes (`overview` | `attach`). Overview = card grid (`tmux panes` section + Slice 3.0 `copad panels` section as fallback) + recent-events strip + presence toggle. Attach = `xterm.js` (CDN `@xterm/xterm@5.5.0` + `@xterm/addon-fit@0.10.0`, no SRI — accepted as Slice 3.2 follow-up) + mobile keyboard toolbar (`Esc Tab Ctrl(sticky) / | ↑↓←→ ^C ^D ^Z`).

**Live verification (e2e, 2026-05-23):**
- `tmux ls` showed 7 sessions × multiple windows; `curl /api/tmux/panes` returned **19 panes** with cwd + last 5 lines (ANSI escapes preserved via `capture-pane -e`).
- WS `/ws/tmux/overview` opened, received 2 snapshots in 7 s window (initial + one timer fire), 19 panes each.
- WS `/ws/tmux/attach/%1`: HTTP 101 + Sec-WebSocket-Protocol echo + 5 binary frames (~6.2 KB total) on the first second. Sending a WS Close frame from the client → `tmux list-clients` count returned to baseline (no leaked attach client).
- `POST /api/tmux/send` with `{"target":"%1","text":"echo copad-web-bridge-e2e\n"}` → `tmux capture-pane -t %1` showed the literal string at the prompt. Bracketed-paste behavior intentional: the text appears but the shell does not auto-execute, matching the `paste-buffer -p` safety contract.
- 17 unit tests pass (5 new in tmux::tests). `cargo fmt` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean. `cargo build --release` clean.

**Trade-offs accepted:**
- Shared view with existing copad GUI client (selecting pane on mobile changes copad's active pane too). Acceptable for "I left my desk; checking from phone" — Slice 3.2 may add grouped sessions for independent per-client views.
- Non-tmux copad pane attach is still Slice 3.0-style snapshot-only. A user not running tmux loses the live experience — recommended workflow is `tmux new-session` before stepping away.
- xterm.js CDN without SRI in Slice 3.1. User accepted internet-on-mobile assumption. Adding SRI hashes is a single follow-up commit (compute sha384 of the pinned CDN files, paste into the `integrity=` attribute) — wasn't included in 3.1 because the placeholder hashes were wrong and brokering the actual hash computation mid-implementation would have stalled.
- Slow-client close on overflow uses WS code 1000 (normal close) from axum's default close path, not 1011 with `slow_client` reason. Explicit 1011 + custom reason requires custom close-frame emission; the `try_send → break` pattern produces functional disconnect-on-overflow but the close code is generic.
- `paste-buffer -p` (bracketed paste) means `/api/tmux/send` text doesn't auto-execute on Enter — user must press Enter in the receiving shell. Safety-first default; an `auto_execute: true` field could send `\r` via `send-keys -t target Enter` separately if needed later.

**Out of scope (Slice 3.2+ backlog):**
- tmux control mode (`tmux -CC`) for iTerm2-grade multi-window streaming on one connection.
- Grouped/independent sessions per attached client (each gets their own visible window/pane).
- Non-tmux copad pane attach via daemon-side PTY child output forwarding.
- xterm.js SRI hashes.
- Explicit WS code 1011 + `slow_client` reason on overflow.
- PWA manifest + service worker + Web Push.
- One-shot send UI in overview mode. The endpoint exists; SPA only exposes attach-mode input for v1.

**See:** `plugins/web-bridge/src/tmux.rs` (new module), `plugins/web-bridge/src/main.rs` (4 new endpoints + AppError::Custom variant + AppState.tmux_cache + handle_ws_tmux_overview/attach + run_attach lifecycle), `plugins/web-bridge/static/index.html` (mode-switching SPA + xterm.js + keyboard toolbar), session intent `~/docs/sources/sessions/copad/2026-05-23-session-30a6e5d6.md`.

## 44. Core-unify via `copad-ffi` — shared wire formats + validation between Linux and macOS

**Problem:** macOS was hand-mirroring Linux logic in several places: theme palette data (~270 LOC switch over 10 hardcoded themes), session snapshot persistence (~200 LOC of Codable + atomic-write code that just had to keep its JSON wire shape aligned with `copad-linux::session::Session` by manual review), wallpaper rotation file reader + entropy + mode flag (~80 LOC duplicating `copad-linux::socket::select_random_image/is_bg_active/toggle_bg_mode`), config-reload semantics (macOS reset-to-defaults on parse failure vs Linux preserve-previous), plugin manifest TOML parser (~150 LOC re-parsing what `copad-core::plugin::PluginManifest` already deserialized canonically). Every Linux change to one of these surfaces required a parallel macOS edit — drift was a matter of when, not if.

**Approach: single source of truth in `copad-core`, exposed via `copad-ffi`.** Five vertical-slice phases, each ending with a build/test gate + codex cross-review + commit. The `copad-ffi` crate (already used for `TriggerEngine` / `ActionRegistry` / `EventBus` since macOS daemon-migration PR 5c) becomes the canonical bridge for any shared-data surface — `copad-term` stays terminal-and-PTY only.

**Phases (commits):**

- **1B Theme (`3e7aae8`)** — `copad_ffi_theme_get(name)` / `copad_ffi_theme_list()` over `copad_core::theme::Theme`. macOS `Theme.swift` drops the hardcoded switch + 10 static palette vars; new private `Wire: Decodable` (hex string fields) → existing `RGBColor`/`CopadTheme` UI model at decode time. AppDelegate fallback uses new `CopadTheme.default` (force-unwrap intentional — failing loud is better than black-on-black if FFI ever breaks).
- **1A Session (`372ae3d`)** — moves `copad-linux/src/session.rs` body into `copad-core/src/session.rs`; Linux `session.rs` becomes a 6-line `pub use copad_core::session::*;` so all `crate::session::*` call sites in `window.rs` / `tabs.rs` keep compiling. Argless `copad_ffi_session_load / save / clear` — the path (`paths::state_dir() / "session.json"`) is resolved in core so the Swift wrapper doesn't have to thread it. Swift `Session.swift` keeps its in-memory `Snapshot/TabSnap/SplitSnap/SplitOrientation` Codable types (PaneManager builds + consumes them in-process); only the load/save/clear functions route through FFI.
- **1C Background (`98ea889`)** — new `copad-core/src/background.rs` with `BackgroundPaths { primary_list, fallback_list?, mode_file }` struct + `pick_random / is_active / toggle` free functions. Paths stay platform-native: Linux passes its legacy XDG paths (`~/.cache/terminal-wallpapers.txt`, `~/.cache/copad-bg-mode`); macOS passes its native paths (`~/Library/Caches/copad/wallpapers.txt` primary, `~/.cache/terminal-wallpapers.txt` fallback, `~/Library/Caches/copad/bg-mode`). FFI takes paths as args; internal `cstr_to_pathbuf` helper handles NULL / invalid-UTF-8.
- **2A Config hot-reload semantics (`8cdb745`)** — macOS `CopadConfig.load() -> CopadConfig` becomes `throws -> CopadConfig`. Initial load (`applicationDidFinishLaunching`) uses `(try? …) ?? .defaults` (no previous to preserve — defaults beat refusing to launch on a typo). Hot reload (`handleConfigChange`) uses `do/catch` and `return`s on throw, preserving the live UI. Matches Linux `window.rs::connect_changed`'s early-return. **No FFI** for this phase — macOS `CopadConfig` has fields core's `copad_core::config::CopadConfig` doesn't model (`osc52`, `rendererBackend`, `transparentDefaultBg`, `background.path` alias, `[tabs] position` constrained to top/bottom). Routing through a core parser would drop them. Swift TOMLKit stays as the macOS parser; this is policy unification, not code unification.
- **2B Plugin manifest (`d7d5eb8`)** — `copad_core::plugin::PluginManifest` (+ children) gains `derive(Serialize)`. `Activation` / `RestartPolicy` enums use **custom** `Serialize` impls that emit raw strings (`"onStartup"` / `"onAction:kb.*"` / `"on-crash"`) so the JSON output is byte-for-byte what the Swift consumer's `String activation` / `String restart` fields expect — codex round 2 C2 caught that an auto-derived enum serialization would emit `{"type":"OnAction","glob":"kb.*"}` which Swift can't decode. New `validate_toml(path)` / `validate_toml_str(toml_str)` functions are the canonical validator; both Linux `discover_plugins` (daemon-side) and macOS `PluginManifestStore.parse(at:)` (GUI-side via FFI) go through the same path. **Discovery stays Swift-side** — directory walk, duplicate-name winner pick (sort by (name, dir.path), sorted-last wins), and `dir: URL` retention for relative `services.exec` / `panels.file` resolution are all macOS-specific and codex round 2 C4 explicitly flagged scope-creep risk on moving them.

**Pre-implementation pressure-test (codex-plan):** Plan went through 2 rounds with codex before any code was written. Round 1 caught 4 CRITICAL mistakes — biggest was the false premise that `copad-ffi` didn't exist (I was about to propose renaming `copad-term`); it did, with 36 already-exported symbols and the ownership/error/string-free conventions already established (`copad_ffi_free_string`, thread-local `LAST_ERROR`). Round 2 caught 3 more — biggest was that auto-derived `Activation` JSON would silently break the macOS Swift consumer. The plan as initially written would have shipped code that compiled but didn't actually unify anything; ending up with 2-3 days of follow-up to fix wire shapes that round 2 forced into the original implementation instead.

**FFI conventions (re-affirmed by this work):**

- Owned strings cross as `*mut c_char` from `CString::into_raw`; caller frees with `copad_ffi_free_string`. NULL means "no result" (load returned nothing) OR "error, see `copad_ffi_last_error`" — the disambiguation is per-function and documented.
- Boolean-ish returns use `i32` with `1`/`0`/`-1` (true/false/error-with-last-error).
- Argless API where the path is canonical (`session.json` lives at `state_dir()`); explicit path args where the caller has to make a choice (background rotation paths are intentionally per-platform).
- Compound payloads cross as JSON; serde is canonical, Swift consumers have a private `Wire: Decodable` (or directly-aligned `Decodable`) that maps to the in-memory UI model.

**Verification gate at each phase:** `cargo build -p copad-{core,ffi} --release`, `cargo test -p copad-core <module>`, `cargo clippy -p copad-{core,ffi} -- -D warnings`, `swift build -c release`, `swiftformat`, FFI symbol `nm` check on `libcopad_ffi.a` and `Copad.app/Contents/MacOS/Copad`, `./scripts/install-macos.sh` clean, codex `--uncommitted` cross-review APPROVED.

**Trade-offs accepted:**

- No Swift `testTarget` added yet (codex round 2 I4). Wire-compat assertions live as Rust-side `cargo test session/background/plugin/theme` round-trip tests; the Swift side gets compile-time `Decodable` checks but no runtime fixture roundtrip. If a future schema bump breaks Swift, it'll be caught at first launch instead of at test time.
- Phase 2A is the only phase that didn't actually move code (just policy). Recorded as core-unify because the user-visible behavior now matches between the two platforms, even though the implementations stay separate.
- macOS `CopadConfig` deliberately stays out of core. The platform-extension fields are real (renderer backend, osc52 policy, transparency, tab position constraint) and forcing them into the shared schema would either bloat the Linux config struct or require a "macOS extensions" sub-table that complicates `coctl get-config` on Linux.

**Volume:** ~610 LOC moved into core (theme.rs already counted), ~318 LOC removed from macOS Swift, 11 new FFI functions across 5 surfaces. Five commits between `3e7aae8` and `d7d5eb8`.

**See:** `copad-core/src/{background,session}.rs` (new modules), `copad-core/src/{plugin,theme}.rs` (Serialize additions for plugin, no change for theme), `copad-ffi/src/lib.rs` (5 new FFI surfaces appended), `copad-macos/Sources/CCopadFFI/include/copad_ffi.h` (matching declarations), `copad-macos/Sources/Copad/{Theme,Session,BackgroundRotator,PluginManifest,Config,AppDelegate}.swift` (thinned to FFI wrappers or semantics-aligned).

## 45. macOS distribution via Homebrew tap — single cask, arm64, ad-hoc signed

**Problem:** macOS users had only `scripts/install-macos.sh` (build-from-source: `swift build -c release` + `cargo install --path copad-cli` + `cargo install --path copad-daemon` + per-plugin cargo build × 10 + LaunchAgent + shell hooks). Cold build is ~10 min and requires Rust + Xcode CLT + a clone of the repo. There was no path for a user who just wants the app to run.

**Decision:** Ship a Homebrew tap at `marshallku/homebrew-copad` with one **cask** (not a formula + cask split). `brew install --cask marshallku/copad/copad` lands everything `install-macos.sh` does: `Copad.app` → `/Applications`, `coctl` + `copadd` → `$(brew --prefix)/bin`, 10 plugin binaries + manifests → `~/Library/Application Support/copad/plugins/<name>/`, shell hooks → `~/.config/copad/shell-hooks/`, LaunchAgent plist → `~/Library/LaunchAgents/`. The release artifact is a single pre-built tarball produced by `.github/workflows/release.yml`'s new `build-macos` job (macos-15 runner / arm64 — the macos-14 image was tried first but its Swift 5.10 cannot parse the Package.swift's `swift-tools-version 6.0`).

**Why a single cask and not formula + cask:** The conventional split is "cask = GUI, formula = CLI". Here it's structurally wrong: `copadd` (daemon) is required for status bar + plugin runtime + workflow triggers, plugins live in a per-user dir the GUI reads at launch, and `coctl` only makes sense if `copadd` is up. Splitting would let a user `brew install --cask copad` (GUI only) and get a half-broken install where the status bar warns about a missing daemon. One cask, one install, all-or-nothing.

**Why arm64-only for now:** macos-15 arm64 GH runner (with `maxim-lobanov/setup-xcode@v1` + `xcode-version: latest-stable`, Swift 6.1+; macos-14 defaulted to Swift 5.10 and couldn't parse the Package.swift manifest, macos-15's default Xcode then couldn't compile the SE-0439 trailing-comma usage) produces the artifact; macos-13 x86_64 would double CI time (~20–30 min added) and the user base for Intel Macs on a brand-new tool is negligible. Intel users fall back to `scripts/install-macos.sh` from source. `depends_on arch: :arm64` in the cask makes the failure mode explicit (`brew install` refuses with a clear message) instead of a runtime crash. Revisit when there's actual Intel-user demand.

**Why ad-hoc signed (not Developer ID + notarization):** Apple Developer is $99/yr and the notarization round-trip adds latency to every release. Homebrew Cask removes the quarantine xattr on install (`xattr -dr com.apple.quarantine`), so Gatekeeper lets an ad-hoc-signed app launch on first run via brew. Users who download the `.tar.gz` directly from GitHub Releases still need to manually clear quarantine — that's a known cost. Decision #45 is "brew is the recommended macOS install path"; direct-download is a fallback for source-builders or CI consumers. Upgrade to Developer ID when (a) macOS 15+ further tightens ad-hoc enforcement enough to break the brew path, or (b) we want a `.dmg` download UX that matches non-brew users' expectations.

**LaunchAgent plist — why the cask writes it inline rather than reusing `dist/launchd/com.marshall.copad.daemon.plist`:** The in-repo plist uses `HOME_PLACEHOLDER` because `install-macos.sh` installs `copadd` to `$HOME/.cargo/bin/copadd` (a path launchd can't expand via `~`). The cask installs `copadd` via the `binary` stanza, which symlinks into `HOMEBREW_PREFIX/bin/` — a different path entirely, and one that varies by Mac (arm64: `/opt/homebrew/bin/`). The cask's `postflight` block writes a plist fresh on each install with `#{HOMEBREW_PREFIX}/bin/copadd` substituted at install time. The two installers (script vs cask) ending up with semantically equivalent plists but pointing at different absolute paths is acceptable — both are correct for their own install layout.

**Tarball layout (canonical):** `build-macos` in `.github/workflows/release.yml` emits `copad-v<ver>-aarch64-apple-darwin.tar.gz` with this structure at the root (no wrapping dir):

```
Copad.app/                          (ad-hoc signed)
coctl
copadd
plugins/<name>/{copad-plugin-<name>, plugin.toml, panel.html?, triggers.example.toml?}
shell-hooks/copad-cwd.{bash,zsh,fish}
com.marshall.copad.daemon.plist     (HOME_PLACEHOLDER unsubstituted — cask ignores; only install-macos.sh consumes)
```

The cask postflight reads from `staged_path` and lays this out into the per-user destinations. The plist at the tarball root is dead weight for the cask path; keeping it in the tarball is cheap and means a hypothetical "download tarball and run a script" install path could reuse the same artifact.

**Uninstall contract:** `brew uninstall --cask copad` boots out the LaunchAgent (via `launchctl:` stanza, which kills the daemon), quits the GUI app (`quit:` Apple Event to `com.marshall.copad`), deletes the plist (since brew doesn't auto-track files written in postflight), and removes the cask-installed artifacts. **User state survives** (`~/Library/Application Support/copad`, `~/.config/copad`, etc.) until `brew uninstall --cask --zap copad`, which trashes them via the `zap trash:` stanza.

**Trade-offs accepted:**

- No `brew test` integration. The cask isn't tested by Homebrew CI; we rely on the release.yml build job to produce a sane tarball + a manual `brew install --cask` smoke test before publishing the sha256 update to the tap. If the tarball is structurally broken, the cask install fails at postflight, not at install time — the failure mode is "GUI installed but daemon never starts" rather than "install errored." Mitigation: keep the postflight Ruby small and fail-loud (writes are atomic, missing source dirs propagate as exceptions).
- No formula path for headless `coctl` use. A user who wants only the CLI (e.g., scripting against a remote `copadd`) still has to install the full cask. Acceptable for now — the CLI is small and the daemon is already on the same machine in every real use case.
- The tap repo lives at `marshallku/homebrew-copad`, not in `homebrew-cask` proper. Submitting to the official cask repo requires version stability + community traction; not where we are yet. Personal tap is the standard staging ground.

**See:** `.github/workflows/release.yml` (`build-macos` job), `marshallku/homebrew-copad/Casks/copad.rb`, `scripts/install-macos.sh` (source-build counterpart that this decision does not replace), `dist/launchd/com.marshall.copad.daemon.plist` (script-install plist with `HOME_PLACEHOLDER`).

**Post-ship discovery — macOS 26 (Tahoe) breaks the brew path:** First user (the author, on macOS 26.3.1 Build 25D2128) hit a hard wall during the v0.2.0 smoke-install. Tahoe's tightened App Verification deletes ad-hoc-signed executables on `launchd` spawn — `copadd` is removed from `/opt/homebrew/Caskroom/copad/0.2.0/copadd` the first time launchd tries to exec it via the LaunchAgent, and plugin binaries get the same treatment when `copadd` tries to fork+exec them. Stripping `com.apple.quarantine` in the cask postflight (commit `f4fc093` in `homebrew-copad`) was a real fix for the plugin-spawn ENOENT — but only partially. The quarantine strip is necessary; it's not sufficient on Tahoe. Tahoe's underlying verification check fires even with `com.apple.provenance` only — there's no xattr you can strip to bypass it.

The trade-off baked into decision #45 ("ad-hoc + brew quarantine strip is enough; revisit when 15+ tightens") was correct for the version the decision was written against (Sonoma/Sequoia) — but **macOS 26 tightened it faster than predicted.** The brew cask currently:
- **Works** on macOS 14 (Sonoma) and 15 (Sequoia) — verified via codepath, ad-hoc signing has historically run cleanly under Cask's quarantine-strip on these.
- **Does NOT work** on macOS 26 (Tahoe) and presumably later — `launchd` exec deletes the binary; the GUI app likely faces the same fate the moment a user double-clicks it.

The README now documents this — Tahoe users are pointed at `scripts/install-macos.sh`, which uses `scripts/codesign-dev.sh` to sign with a trusted self-signed cert in the user's login keychain. Tahoe spares trusted-identity-signed binaries from the deletion path; the in-keychain trust + designated-requirement match is what TCC has always cared about, and Tahoe's verification path piggybacks on the same trust anchors.

**Proper fix (deferred):** Apple Developer ID ($99/yr) + notarize the release artifacts in CI. The `build-macos` workflow gets a `codesign --sign "Developer ID Application: ..."` step + `xcrun notarytool submit --wait`. Probably wait until either (a) the project has Tahoe users actually filing issues, or (b) Sonoma/Sequoia hit their EOL window and Tahoe-share crosses ~50%. Until then the dual-path (brew for legacy macOS, install-macos.sh for Tahoe+) is acceptable — the cost of $99/yr + notarization plumbing outweighs the current single-Tahoe-user impact.
## 46. Context bridge — `coctl event publish` from shell precmd (local-only, Phase 22.1)

**Problem.** Phase 22 needs the active pane's context (host, cwd, git remote, branch, tmux session, foreground command) to flow into copad's bus so the dossier panel can re-render whenever the user changes directory or switches branches. The shell knows all of this; the question is what transport to use to get it onto copad's EventBus.

**Decision.** For local copad-spawned shells, the precmd hook calls `coctl event publish pane.context_changed '<json>'` with a flat `PaneContext` payload — no OSC capture, no HMAC, no prompt-boundary state machine. SSH / cross-host coverage is **explicitly scoped out** of Phase 22; the OSC-based design that would have enabled it is preserved in [context-bridge.md § Out of scope but designed for revival](./context-bridge.md) for if/when SSH support returns. Implementation details in [docs/context-bridge.md](./context-bridge.md).

**Why `coctl event publish` over alternatives:**

- **OSC bytes through PTY** (the original design): the only mechanism that survives SSH and tmux pass-through unchanged — but `vte4` 0.8.x exposes no custom-OSC subscribe API in its Rust bindings, so capture would need a VTE FFI shim, and the trust boundary needs OSC 133 prompt-boundary gating + HMAC + per-session secret distribution. All justified when SSH is in scope. For local-only, the shell already has `$COPAD_SOCKET` and `$COPAD_PANEL_ID` injected, so an in-process IPC is strictly simpler.
- **inotify on FS / git hooks**: doesn't cover cwd changes that aren't filesystem events; storms on monorepos.
- **`coctl event publish`**: shell already has socket + panel id env vars; per-prompt cost is ~10-30ms (one `coctl` invocation, detached to background subshell so it doesn't block the prompt); no new wire format; no crypto.

**Clarification (load-bearing for future debug):** `coctl event publish` dials the daemon socket at `daemon_socket_path()` directly — it does NOT use `$COPAD_SOCKET` as the transport. The env var is presence-only ("am I in a copad shell?") for the hook to gate emission on. Daemon availability is therefore a hard prerequisite; with `copadd` down, the hook silently no-ops.

**Trust boundary.** `events.publish` over the daemon socket carries `SO_PEERCRED` source stamping (decision #23), and the daemon marks the resulting events `Origin::External` (decision #37). That gating model is the entire application-layer security model — there is no HMAC, no per-session secret, no state machine. Consequence: **any same-UID process can publish `pane.context_changed` with an arbitrary `panel_id`.** The dossier panel (Phase 22.2) treats this as best-effort display data.

**Load-bearing future-work caveat (trigger interpolation surface):** The current `TriggerEngine` exposes `context.active_panel` / `context.active_cwd` / `context.presence` to conditions and payload-match interpolation, **but NOT `context.pane_context.*` fields**. When a future engine extension adds that surfacing, `accept_external = true` alone won't be sufficient: it gates the *firing* event's origin, not the provenance of context state. A same-UID-spoofed `pane.context_changed` could poison `ContextService`, then a later `Origin::Internal` event with `accept_external` defaulted to `false` could interpolate the poisoned state. The future extension must enforce origin-aware reads of context fields, not just origin-aware firing.

**Explicitly deferred (preserved in revival design):**

- OSC capture from PTY stream (the only way to handle non-copad-spawned remote shells over SSH).
- HMAC-SHA256 with per-session secret + `$COPAD_CONTEXT_SECRET` env injection.
- OSC 133 prompt-boundary state machine — the only mechanism that genuinely distinguishes "emitted at prompt time" from "emitted mid-execution" (env-var-inherited HMAC does not).
- Bidirectional protocol (copad → shell pushes).
- `SSH SendEnv` / `AcceptEnv` configuration for secret distribution.

**See:** [docs/context-bridge.md](./context-bridge.md) (full design including revival section), [docs/roadmap.md § Phase 22.1](./roadmap.md#phase-22-context-aware-workstation-hub) (shipped checklist).

## 47. life-assistant absorption stance — selective native reimplementation, not embedding (Phase 22.3)

> **Superseded by [#48](#48-copad-native-port-of-project-orchestration-spine) (2026-05-29).** This decision committed to "absorption-by-reimplementation, applied selectively, with a Rule-of-Three gate before each per-module port." The follow-up survey of `~/dev/life-assistant`'s `internal/dashboard/` + ProjectResolver + mission/goal/workflow/agent surface area showed the project-orchestration spine has enough internal coupling that gating each module independently would force the user to keep `life-assistant` running for missing pieces — which is exactly the multi-app dependency Phase 22 was designed to eliminate. #48 commits to porting the spine (mission / goal / agent / approval / workflow / pipeline / runledger) upfront in Phase 22.2–22.7, and keeps the Rule-of-Three gate only for non-spine items (`notes` and anything later). The keep-on-server / obsolete-by-copad lists below remain valid; the change is in the porting cadence + commitment level, not the scope boundaries. Body kept verbatim for traceability.

**Problem.** `~/dev/life-assistant` (Go server + React/Vite SPA dashboard) has grown into a personal-automation hub covering agent state, goals, missions, pipelines, scheduled jobs, a run ledger, notes, plus a long tail of Discord-bot / finance-feed / external-polling modules. As Phase 22's vision pushes copad toward "the only tool the user opens for a dev workday," the question is what to do about that overlapping surface. Three rejected options surfaced during planning:

1. **Embed the SPA in a copad WebView panel** — rejected. The SPA isn't standalone; it's bound to the Go server's endpoints, auth, and storage. Pointing a WebView at the dashboard origin gives a viewer-only window with no copad-side reactivity.
2. **Migrate the whole Go server into copad as a daemon plugin** — rejected. Wrong language (Go vs Rust workspace), wrong runtime (server is designed to run 24/7 receiving external webhooks; copad is a workstation tool), and most modules don't even belong on the workstation (Discord-bot listener, finance feeds, weather/news pollers).
3. **Leave them as separate apps** — rejected. The user explicitly wants single-tool coverage of the dev workday on the workstation. Two-app status quo is the failure mode Phase 22 is designed to fix.

**Decision: absorption-by-reimplementation, applied selectively.**

- **Absorb into copad** (workstation-local, dev-workflow-shaped): `agent`, `brain`, `goal`, `mission`, `pipeline`, `runledger`, `scheduler`, `notes`. Each becomes either a copad plugin (`plugins/<name>/`) or a copad-core extension, sharing the existing EventBus / ActionRegistry / TriggerEngine instead of running a parallel Go runtime. Each port lands as its own phase (Phase 23.x or later, one per module), only after the design in 22.3's inventory doc and a Rule-of-Three usage threshold.
- **Keep on the life-assistant server**: `discord`, `bot`, `tossauth`, `tossinvest`, `trading`, `expense`, `finance`, `investment`, `portfolio`, `weather`, `newsdigest`, `google` polling, plus the cron infrastructure backing the news/weather/finance jobs. These need 24/7 uptime, external webhook reception, or finance-feed authentication — none of which fits a workstation tool that the user closes at end-of-day.
- **Obsolete-by-copad** (don't port; already covered): anything covered by existing copad plugins — `calendar` (Phase 10), `kb` (Phase 9.3), `todo` (Phase 15).

**Coordination with Phase 21 step 12** (existing 30-LOC `lifeassistant` plugin idea). That plugin is an *event publisher* — a server-side scheduler completion publishes `lifeassistant.job_completed` onto the copad bus, consumed by triggers. Absorption is *independent*: as workstation-local modules migrate, the server's responsibility for those modules shrinks, but the bridge keeps firing for the modules that stay on the server. No conflict; the two tracks coexist.

**Phase 22.3 produces design only.** No life-assistant Go code is touched in this phase. The `inventory + per-module port design` document lives at `docs/life-assistant-absorption.md` (to be created in the 22.3 work session). Actual ports are subsequent phases (Phase 23.1 onward).

**Trade-offs accepted:**

- Some short-term duplication: the user runs both apps until enough modules have been ported to retire the server pieces. Worth it because lift-and-shifting the whole Go server would have committed copad to long-tail polling/webhook responsibilities it shouldn't own.
- Rust reimplementation of Go logic costs upfront work per module. Mitigated by the Rule-of-Three gate — modules that don't get used three times don't get ported.
- `agent` and `brain` are the largest, most copad-flavored absorbs; design quality on those is the gating risk for the whole track. The inventory doc must produce a defensible action surface for them or the absorption stalls.

**See:** [docs/roadmap.md § Phase 22.3](./roadmap.md#phase-22-context-aware-workstation-hub), [docs/harness-integration.md § step 12](./harness-integration.md) (the coexisting thin-bridge track), forthcoming `docs/life-assistant-absorption.md` (inventory + per-module designs).

## 48. Copad-native port of project-orchestration spine

**Supersedes [#47](#47-life-assistant-absorption-stance--selective-native-reimplementation-not-embedding-phase-223).** Keeps #47's scope boundaries (which modules absorb / stay-on-server / are obsolete-by-copad) but escalates the absorption cadence: instead of "design-only Phase 22.3 + Rule-of-Three before each per-module port," Phase 22.2–22.7 commits to porting the **project-orchestration spine** (mission, goal, agent, approval, workflow, pipeline, runledger persistence) into `copad-core` upfront, with no `life-assistant` runtime dependency.

**Problem.** During Phase 22 planning the option of bridging to life-assistant's HTTP API (`/api/missions`, `/api/goals`, `/api/workflows`, …) surfaced as a cheaper alternative to porting — ~2 weeks of work vs ~5–7 weeks for the native port. Surveys of both codebases ruled it out:

1. **`copad-core` substrate already covers ~80% of life-assistant's infra layer.** `copad-core::event_bus::EventBus` + `coctl event publish` ↔ life-assistant `runledger` + `dashboard` HTTP submit. `TriggerEngine` ↔ `missionsched` + `approval` expiry sweep. `ActionRegistry` + `register_privileged` ↔ life-assistant `CommandService.Submit` + policy gates. `ContextService` + `pane_context.git_remote` ↔ life-assistant `ProjectResolver`. `Origin` field + `[security] accept_external` ↔ life-assistant Discord-user / web-operator / system-scheduler actor attribution. `ServiceSupervisor` ↔ life-assistant Brain dispatcher's subprocess pool. `claude.start` (Phase 18.1) is already the Brain dispatcher's main job; generalizing it for codex + model-routing is a few hundred LOC, not a full re-port.
2. **Daemon single-writer model eliminates flock complexity.** life-assistant uses flock on mission manifests + atomic-rename + per-user lock files to serialize writes from multiple HTTP handlers. `copadd` is one process; the socket dispatcher serializes naturally. Most of the Go server's "write-ahead order + idempotency + 24h dedup" machinery isn't even needed in the copad port — it exists to prevent races that can't happen here.
3. **Multi-machine portability is load-bearing.** The user runs copad on multiple machines (laptop outdoor, desktop, work). life-assistant is bound to a single home server. A bridge approach makes copad mute on every machine that can't reach the server. The native port makes copad self-sufficient — open it on any machine with `claude` CLI installed and project orchestration just works.
4. **Bridge-API approach forces N pinning points.** Every life-assistant API endpoint becomes a contract. Schema changes on the server propagate to copad as breakages. Doubling the API surface for "things copad needs" makes the server harder to evolve, not easier. The port has one pinning point (`claude.start` subprocess contract) and inherits the rest from copad-core primitives.
5. **LOC delta after substrate reuse is small.** Honest estimate: ~8.5k Rust LOC across the 6 slices for the spine, vs the ~30k Go LOC the equivalent modules represent in life-assistant — because copad-core already provides EventBus, registry, triggers, supervisor, single-writer serialization, context resolution, and the subprocess action surface. The port is mostly data-model translation + filesystem-layout decisions + glue actions + UI panel, not "rebuild a Go server in Rust."

**Decision.**

- **Phase 22.2 (Project + Workflow MVP)** + **22.4 (Goal driver)** + **22.5 (Agent + Mission)** + **22.6 (Approval + Runledger persistence)** + **22.7 (Pipeline + Brain dispatcher generalization)** port the project-orchestration spine into `copad-core`. Detailed surface design lives at [docs/project-orchestration.md](./project-orchestration.md).
- **Phase 22.3 (KB Panel)** is orthogonal — read+navigate UI over `~/docs` driven by `dn` indices + existing `copad-plugin-kb` actions. Ships in parallel with 22.2 (no dependency on the spine port). Detailed design at [docs/kb-panel.md](./kb-panel.md).
- **Phase 23 — life-assistant trim** opens after Phase 22.7 dogfoods stably (~2 weeks of daily use). One-shot data migration script translates `~/bots/<user>/{missions,goals,agents,approvals}/` → `~/.local/share/copad/`. life-assistant's `internal/{mission,goal,agent,approval,pipeline,brain}` deprecates; the SPA loses those pages. Server scope reduces to daily-life ops modules permanently.
- **Permanently on the Go server** (no port planned, ever): Discord bot listener + slash commands, Toss Invest API + Playwright keepalive, Yahoo Finance scrape, KMA weather, news digest, Google OAuth + calendar polling, finance / portfolio / expense / investment loop, all the cron infrastructure backing those. These need 24/7 uptime, external webhook reception, finance-feed authentication chains — none fit a workstation tool that gets closed at end-of-day.
- **Phase 21 step 12 `lifeassistant` event-publisher plugin idea coexists unchanged.** It's a one-way push from the server's daily-ops scheduler into copad's bus (`lifeassistant.job_completed` for a Discord-DM-arrived signal, e.g.). One-way push does not constitute a runtime dependency of copad on life-assistant — the bridge is best-effort; copad keeps working when the server is down. Independent track from #48's port.

**Why not full absorption (including daily-life modules)?** Embedding Discord-bot listener + Toss session keepalive + finance-feed polling + Google OAuth refresh in copad would commit the workstation tool to 24/7 background work and external-API authentication chains that don't fit the "user closes copad at end-of-day" model. Splitting along the workstation-vs-server axis is the right cut.

**Why commit upfront instead of Rule-of-Three per-module?**

- **Internal coupling.** Mission needs agent (assigned_agents) needs approval (action gate) needs workflow (dispatch shape) needs pipeline (team/role) needs brain (subprocess spawner). A Rule-of-Three gate that lands mission first and approval six months later forces the user to keep life-assistant running for the unimplemented half — exactly the multi-app dependency Phase 22 is designed to fix.
- **Cost is amortizable.** ~8.5k LOC across 5–7 weeks is a tractable budget when each slice ships independently usable value (22.2 alone gives workflow execution; 22.4 alone gives goal management; 22.5 alone gives agent + mission). No big-bang merge.
- **Substrate-debt avoidance.** Bridge approach would force `copad-plugin-projects` to speak HTTP to life-assistant and Origin/Trust to copad's TriggerEngine and `pane_context` to ContextService — three different state systems. Native port lets the panel speak one language (copad bus events + ActionRegistry) end-to-end.

**Rule-of-Three still applies — but only to non-spine items.** `internal/notes` (life-assistant) is filesystem-shaped and already partly covered by `copad-plugin-kb` + `dn`; port it only if a real workflow demands it. Same for any future life-assistant module the user adds — they earn a copad port by being used three times, not by existing.

**Trade-offs accepted:**

- **5–7 week implementation window** (vs ~2 weeks for the bridge). Amortized across 6 slices that each ship usable value; not a single blocking merge.
- **Some data duplication during the dogfood window.** User runs both apps until Phase 23 migration runs. Acceptable because both write to disjoint disk layouts (`~/bots/<user>/` vs `~/.local/share/copad/`) — no conflict, just two stores.
- **Loss of life-assistant SPA's mission/goal/workflow pages post-migration.** Acceptable because `copad-plugin-projects` reaches feature parity by the end of 22.7, with per-pane scoping and in-terminal action affordances the SPA can't match.
- **Engineering-curated agent seed roster** ships in 22.5 — `architect`, `api-dev`, `frontend-dev`, `reviewer`, `critic`. Life-assistant's domain-specific seeds (`sarah-cfo`, `tom-cto`, `nina-researcher`, `leo-home-ops`, `amy-exec-asst`, `orchestrator-default`) are deliberately not ported. They belong to life-assistant's daily-life ops scope. Users who want them keep using life-assistant's UI for those personas — which is fine because they're not in the project-orchestration path.
- **Brain dispatcher generalization is design-load on 22.7.** `claude.start` (Phase 18.1) becomes the canonical spawner; `model` param routes to claude/codex variants; pipeline runner drives multi-stage dispatch. If the generalization proves leaky, 22.7 absorbs the rework cost — flagged as the highest-risk slice.

**See:** [docs/roadmap.md § Phase 22](./roadmap.md#phase-22-context-aware-workstation-hub) (slice checklist), [docs/project-orchestration.md](./project-orchestration.md) (substrate + data-model design), [docs/kb-panel.md](./kb-panel.md) (the parallel 22.3 track), [#47](#47-life-assistant-absorption-stance--selective-native-reimplementation-not-embedding-phase-223) (the superseded stance).

## 49. Agent session dispatch via a standalone `csd` CLI — subscription-seat driver, consumed by copad

**Relation to [#48](#48-copad-native-port-of-project-orchestration-spine).** #48 ported the orchestration spine (goal / mission / agent / approval / pipeline) into `copad-core`, but every autonomous-dispatch piece was deferred at every slice ("no autonomous `claude.start` from the tick thread" in 22.4, "no wake-firing" in 22.5, "execution orchestration deferred" in 22.7). The spine is a brain with **no body** — it can store goals and missions but nothing actually drives an agent turn and reads the result back. #49 decides what that body is and where it lives. It does **not** supersede #48; it fills #48's deferred dispatch hole.

**Problem — the billing boundary (researched against primary sources, 2026-05-30).** The user is a heavy interactive Claude (Max) user and wants `claude -p` ergonomics (scriptable, multi-turn, structured) with subscription-seat economics. That combination is not available:

- **Interactive `claude` REPL** (subscription login, no `ANTHROPIC_API_KEY`) → **flat-rate subscription**, unchanged.
- **`claude -p` / Agent SDK / `claude setup-token`+SDK** → as of **2026-06-15**, a **separate monthly "Agent SDK credit"** (Pro $20 / Max-5x $100 / Max-20x $200), then **full API rates** on overage.
- **No officially-supported programmatic interface bills as flat-rate.** Sources: support.claude.com/articles/15036540, code.claude.com/docs/en/authentication.

For a heavy user the metered path is economically dead: interactive use already saturates the subscription, and programmatic work draws a small separate credit then bills API on top. So flat-rate is the only viable substrate for high-volume autonomous work — and flat-rate means the **interactive REPL**.

**The forced tradeoff.** You cannot get both flat-rate billing and clean structured interaction:

| | flat-rate (subscription) | clean structured events |
| --- | --- | --- |
| path | interactive `claude` REPL only | `claude -p --output-format stream-json` only |
| cost | ~0 marginal (already paid) | Agent SDK credit → API rates |
| plan-approval / questions / tool-permissions | TUI gate — detect + inject keystrokes | structured events, programmatic response |
| robustness | brittle (TUI-dependent), gray-area | supported |

The user's stated worry (handling plan-mode approval + mid-task questions) is cleanly solved only on the metered side; on the flat-rate side it must be driven through the TUI.

**Empirical validation (PoC, claude v2.1.157, 2026-05-30).** Driving an interactive `claude` in a detached `tmux` session was tested end-to-end and **both interactions work**:

- **Subscription billing confirmed** — the TUI footer shows the Max rate-limit windows (`5h.. 7d..`); the `$` line is notional, not a charge.
- **Clarifying-question detection** — reliable from the session JSONL (`~/.claude/projects/<slug>/<id>.jsonl`): last `assistant` event with `stop_reason=end_turn`, text ending in `?`, no later `user` event.
- **Plan-ready detection** — the JSONL **lags** here (the approval gate is a TUI interrupt; `ExitPlanMode` is loaded lazily via ToolSearch and not committed until approved). Reliable signals are the **plan file** (`~/.claude/plans/plan-*.md`) plus **capture-pane** markers ("Here is Claude's plan" / "Would you like to proceed?").
- **Approval + execution** — sending the menu keystroke approved the plan and the work executed and completed.
- **Input injection** — `tmux send-keys -l` works (`paste-buffer` did not); keystrokes sent before the TUI is ready are dropped, so send→verify-echo→retry is required.

Net: a **hybrid detector** is needed — JSONL (questions, responses, normal turns) + capture-pane (TUI-interrupt gates) + filesystem (plan files, work products). More brittle than `-p` stream-json, but tractable, and the only flat-rate path.

**Decision.**

- Build the session-driving capability as a **standalone CLI named `csd`** (claude/codex session driver) in its **own repo `~/dev/csd`** (Rust, lib+bin), **not** inside `copad-core` and **not** inside `coctl`. It owns: spawning an interactive agent in detached `tmux` (subscription seat), input injection with readiness/retry, and the hybrid state detector emitting JSON. Backend-agnostic by design — `claude` first, `codex` later — mirroring life-assistant's already-swappable backend.
- **copad consumes `csd`** via shell-out + JSON, exactly the [#43](#43-pluginsweb-bridge-slice-31--tmux-as-data-model--xtermjs-attach-harness-integration-slice-31) tmx pattern (`tmx agents --json`): the projects panel / `web-bridge` reads `csd ps --json` for visibility, and the autonomous loop (in `copadd`, when built) shells out to `csd` to dispatch goal / mission turns. `csd` becomes a **pluggable dispatch backend** alongside the existing GUI `claude.start` (interactive, user-visible tab) and a possible future metered `-p` backend.
- **`csd` implementation details are out of scope for copad docs** — they live in the `csd` repo. copad docs record only the boundary, the consumption contract, and the non-goals.

**What was researched (so it is not re-derived).**

- Billing split (primary sources, above).
- The flat-rate-vs-structured tradeoff (above).
- The end-to-end PoC (above) — flat-rate interactive driving is proven viable.
- copad headless capability — `copadd` runs independently of the GUI (own ActionRegistry / EventBus / socket); `claude.start` is GUI-bound only for *tab spawning*; daemon-side dispatch is feasible (~hundreds of LOC reusing the headless `tmux` primitives already in `plugins/web-bridge/src/tmux.rs`).
- life-assistant's Brain dispatcher as the proven reference pattern (`-p --output-format json --resume/--session-id --permission-mode --max-turns`, deterministic session UUIDs, per-project worktree locks, `BLOCKED` detection) — already abstracted to allow a codex backend.

**What copad will NOT do (non-goals).**

- **Not** reimplement session-driving inside `copad-core` or `coctl`. `csd` is standalone; the daemon / `coctl` shell out to it. (A thin `coctl` passthrough is optional sugar, not where the logic lives.)
- **Not** make the metered `-p` / stream-json path the default autonomous substrate. It stays available as a backend option for clean structured interaction when per-token cost is acceptable, but the heavy-use autonomous path is subscription-seat `tmux` driving.
- **Not** couple the 24/7 life-assistant server to copad. `csd` being standalone means the server can consume it directly; neither depends on the other.
- **Not** reinvent `~/.claude` readers or agent dashboards in copad — `tmx` owns observation, `csd` owns driving, copad **visualizes** both via JSON shell-out.
- **Not** treat the projects-panel `workflow.run` launcher as the product value — it is a dispatch primitive, not a `tmx`-competing launcher. The value is autonomous-body + visibility.

**Open risks (carry forward).**

- **Gray-area loophole.** Driving the interactive REPL programmatically to obtain flat-rate billing is undocumented and uncontemplated by the billing model. Anthropic could reclassify it as programmatic (metered) or disallow automated interactive driving. Mitigation: keep `csd`'s dispatch backend swappable so a forced move to `-p` is a config flip, not a rewrite.
- **Shared rate limits.** An autonomous fleet draws from the **same** interactive subscription limits as the user's own (heavy) interactive use — they compete for capacity. Mitigation under consideration: a dedicated second subscription seat for the fleet.
- **Release-dependent TUI markers.** capture-pane gate markers (plan approval, permission prompts) shift between `claude` releases and must be re-verified per version; the JSONL lags live state by a tool-call.

**See:** [#48](#48-copad-native-port-of-project-orchestration-spine) (the spine this is the body for), [docs/project-orchestration.md](./project-orchestration.md) (dispatch section), [docs/roadmap.md § Phase 22.4 / 22.7](./roadmap.md#phase-22-context-aware-workstation-hub) (the deferred-dispatch notes this fills), [#43](#43-pluginsweb-bridge-slice-31--tmux-as-data-model--xtermjs-attach-harness-integration-slice-31) (the tmx shell-out + JSON consumption pattern `csd` reuses).

## 50. Projects panel retired — `web-bridge` is the single orchestration cockpit

**Retires the `copad-plugin-projects` panel surface built across Phase 22.2–22.7 (under [#48](#48-copad-native-port-of-project-orchestration-spine)).** Only the GTK panel UI is removed; the `copad-core` orchestration data model (goal / mission / agent / approval / pipeline / runledger) stays — it is the state the cockpit reads and the daemon loop drives.

**Problem.** Dogfooding showed the projects panel has no standalone value in its built form:

- **workflows** section — a launcher slower than `tmx` (click → expand → fill form → Run, vs `tmx`'s keyboard selector).
- **runs** section — volatile (`workflow.started` events, cleared on restart) and duplicates `tmx agents`.
- **goals / missions / approvals** sections — empty CRUD lists, because nothing drives the spine (autonomous dispatch was deferred at every slice; the body lands as `csd` in Phase 24, [#49](#49-agent-session-dispatch-via-a-standalone-csd-cli--subscription-seat-driver-consumed-by-copad)).

All of the panel's value is downstream of the body, and even with the body it would be a *rebuild* (visibility + human-in-the-loop), not the current control-panel form. So the form built in 22.2–22.7 is effectively dead weight.

**Decision.** Retire the GTK projects panel. The single orchestration **cockpit** is `web-bridge` (already an HTTP/WS server, [#43](#43-pluginsweb-bridge-slice-31--tmux-as-data-model--xtermjs-attach-harness-integration-slice-31)): it renders goal / mission / `csd` state and routes answer/approve, reachable from the workstation browser **and** a phone.

- **One cockpit, not two.** A GTK panel and a web SPA would be two UIs over the same state. `web-bridge` already exists and works remotely; a GTK panel adds a second surface to maintain for no capability the web surface lacks.
- **The orchestration need is "answer from anywhere."** Autonomous agents block-and-ask while the user is away; a phone-reachable surface is the requirement — inherently web, not GTK.
- **`tmx` launches + observes, `csd` drives, the cockpit only visualizes + routes replies.** A heavy GTK panel was the wrong shape for that thin role.

**Removed vs kept.**
- **Removed:** `plugins/projects/` (project-header / workflows / runs / goals / missions / approvals). Action `workflow.run` stays as a callable dispatch primitive, not a launcher UI.
- **Kept:** the `copad-core` spine (state store + daemon-driven body); the **KB panel** (22.3) — in-app-useful, unaffected.

**Trade-off accepted.** Cockpit interaction requires a browser (local or phone) rather than an in-app GTK pane. Acceptable: the user is `tmux`-heavy and found the in-app panel dead weight, and the browser surface is the one that also solves remote/away access.

**See:** [#49](#49-agent-session-dispatch-via-a-standalone-csd-cli--subscription-seat-driver-consumed-by-copad) (the dispatch body the cockpit visualizes), [#43](#43-pluginsweb-bridge-slice-31--tmux-as-data-model--xtermjs-attach-harness-integration-slice-31) (the `web-bridge` server this becomes the cockpit on), [#48](#48-copad-native-port-of-project-orchestration-spine) (the spine whose panel surface this retires), [docs/roadmap.md § Phase 24.5 / 24.6](./roadmap.md#phase-24-csd-integration--autonomous-loop-the-body-for-the-224227-spine).
