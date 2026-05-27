# Core Library (copad-core)

Shared Rust library used by all platform targets.

## Modules

### config.rs

TOML config at `~/.config/copad/config.toml`.

```rust
CopadConfig {
    terminal: TerminalConfig { shell, font_family, font_size },
    background: BackgroundConfig { directory, interval, tint, opacity },
    socket: SocketConfig { path },
    theme: ThemeConfig { name },
}
```

Key methods:

- `CopadConfig::load()` — reads config file, returns defaults if missing
- `CopadConfig::write_default()` — creates default config file
- `CopadConfig::config_path()` — returns `~/.config/copad/config.toml`

Defaults:

- shell: `$SHELL` or `/bin/sh`
- font: JetBrainsMono Nerd Font Mono, size 14
- tint: 0.9, opacity: 0.95
- socket: `/tmp/copad.sock`
- theme: `catppuccin-mocha`

### background.rs

Background image cache manager.

```rust
BackgroundManager {
    directory: Option<PathBuf>,
    cache_file: PathBuf,        // ~/.cache/copad/wallpapers.txt
    current: Option<PathBuf>,
    cached_images: Vec<PathBuf>,
}
```

Key methods:

- `load_cache()` — reads cache file, rebuilds if empty or missing
- `rebuild_cache()` — scans directory for image files (jpg, jpeg, png, webp, bmp)
- `next()` — picks random image, avoids current. Uses `rand::seq::IndexedRandom` (rand 0.9 API)
- `delete_current()` — removes current from cache, updates cache file

### protocol.rs

cmux V2 compatible newline-delimited JSON protocol.

```rust
Request { id: String, method: String, params: serde_json::Value }
Response { id: String, ok: bool, result: Option<Value>, error: Option<ResponseError> }
ResponseError { code: String, message: String }
```

Used by copad-cli for socket communication.

### error.rs

```rust
enum CopadError { Io, Config, Protocol }
type Result<T> = std::result::Result<T, CopadError>;
```

### trigger.rs

Config-driven `event → action` automation. Pure primitive — no bus subscription, no config loading; the platform layer pumps events into `dispatch()`. See [workflow-runtime.md](./workflow-runtime.md) for the broader trigger model.

```rust
Trigger { name, when: WhenSpec, action, params: Value, condition: Option<String>, await: Option<AwaitClause> }
WhenSpec { event_kind: String, payload_match: Map<String, Value> }
AwaitClause { event_kind: String, payload_match: Map<String, Value>, timeout_seconds: u64, on_timeout: TimeoutPolicy }
TimeoutPolicy = Abort | FireWithDefault
TriggerEngine::new(sink)                           // ActionRegistry impls TriggerSink
TriggerEngine::with_publish_bus(sink, Arc<EventBus>)  // Phase 14.2: required for await semantics
TriggerEngine::set_triggers(Vec<Trigger>)          // hot-reload; ALSO clears in-flight await state (preflight + pending)
TriggerEngine::dispatch(&Event, Option<&Context>) -> usize
TriggerEngine::sweep_pending_awaits()              // periodic timeout sweep, caller invokes from a timer
TriggerEngine::count() / names() / pending_await_count() / preflight_await_count()
```

**`when` matching:**
- `event_kind` is a glob — same semantics as `event_bus::pattern_matches` (`*`, `foo.*`, exact).
- All other keys under `[when]` (TOML `#[serde(flatten)]`) are required payload-field equality matches. Missing field or different value → trigger does not fire.

**`condition` (Phase 10.2):** optional boolean expression evaluated AFTER `when` matches. Grammar lives in `condition.rs`; supports `== != < <= > >= && || !` + parens, `event.X.Y` / `context.X` references, and string/number/bool/null literals. Compiled to AST once at `set_triggers` time; parse failures drop only that trigger (others still load). Eval failures (type mismatch on ordering, etc.) are logged and treated as "trigger does not match" — never fires the action on a misconfigured condition.

**Param interpolation (`{token}` in any string in `params`):**
- `{event.foo}` → `event.payload["foo"]` value (scalar JSON → string; null → "null"; objects/arrays → `Display` of `serde_json::Value`).
- **Dot-path access** (Phase 14.2): `{event.foo.bar.baz}` walks nested JSON objects. Used by `await`-driven workflows where the matched payload lands under `event.await.<field>` on the synthesized `<trigger_name>.awaited` event. Non-object hops return `None`, leaving the literal `{token}` in place — same fail-loud posture as flat tokens.
- `{context.active_panel}` / `{context.active_cwd}` → from the `Context` snapshot the dispatcher passes in.
- Unresolvable tokens are kept as literal `{token}` so misconfigured triggers fail loudly in their target action rather than silently substituting empty.
- Unclosed `{` is preserved verbatim.
- Walks nested arrays/objects; non-string scalars pass through unchanged.

**`await` (Phase 14.2 slice 1):** turns a single trigger into a multi-step state machine.

```toml
[[triggers]]
name = "ask-jira"
action = "slack.post_message"
params = { channel = "U_USER", text = "Jira key for {event.title}?" }
[triggers.when]
event_kind = "todo.start_requested"
[triggers.await]
event_kind = "slack.dm"
payload_match = { user = "{event.user}" }
timeout_seconds = 300
on_timeout = "abort"   # or "fire_with_default"
```

When the trigger fires its action, the engine registers a **preflight** entry. On `<action>.completed` (Phase 14.1 fan-out) the preflight promotes to **pending**; on `<action>.failed` it drops. While pending, every dispatched event is checked against `await.event_kind` + interpolated `payload_match`. On match, the engine publishes a synthesized `<trigger_name>.awaited` event whose payload is the original event's payload PLUS the matched event's payload nested under `await:`. Downstream triggers reference `<trigger_name>.awaited` to continue the chain.

Important constraints:
- **Subscriptions** (copad-linux's `TriggerSubscriptions::reconcile`) must subscribe to `await.event_kind`, `<action>.completed`, and `<action>.failed` for await-bearing triggers — otherwise the engine never sees those events. Done automatically in copad-linux.
- **Pump ordering** (copad-linux's `drain_into`) drains receivers into a Vec then stable-sorts so `.completed` / `.failed` events run BEFORE `await.event_kind` events queued in the same tick. Without this, an awaited reply in the same tick as the completion that should promote it could be dropped.
- **FIFO scope is per action name only** — neither per-trigger nor per-invocation. Two triggers using the same action share a queue; even repeated firings of one trigger may mis-correlate completions to preflights if completions arrive out of dispatch order. Closing fully needs per-invocation correlation tokens on `<X>.completed`/`.failed` (slice-2 follow-up).
- **Volatile state**: both `preflight_awaits` and `pending_awaits` clear on `set_triggers()` and on process restart. The clears use separate locks from the trigger-list swap, and `dispatch` clones the trigger snapshot before registering preflights, so in-flight dispatches that captured the OLD snapshot can register old-generation preflights AFTER the clears — old `<trigger_name>.awaited` events may still fire for events in flight at swap time. Hot-reload is best-effort cleanup, not a hard isolation boundary. Acceptable for typical minute-scale awaits with user-authored config.
- **Legacy match-arm actions** that don't fire `<action>.completed` (copad-internal socket actions outside `ActionRegistry`) leave preflights stranded; they expire via `sweep_pending_awaits` and either drop silently (Abort) or emit the awaited event with `await: null` (FireWithDefault).

**Error handling:** registry-action failures (sync `Err` returned by the sink) are logged via `log::warn!` inside `dispatch` and never propagate. Fallthrough-action failures (sink returns `Ok` synchronously but the legacy command later fails) are surfaced ASYNCHRONOUSLY — see the `LiveTriggerSink` reply-consumer thread. One bad trigger cannot poison the dispatcher or block other triggers.

**Hot reload:** `set_triggers()` replaces the trigger list under a write lock (atomic per-list — concurrent dispatch sees old or new, never half-applied). The await-pool clears use separate locks; combined with `dispatch`'s cloned snapshot, this means stale preflights from in-flight old-snapshot dispatches can persist briefly after the swap. See "Volatile state" above for the full semantics.

**`covering_patterns(patterns)`:** helper used by the platform layer to compute the minimal cover of trigger `event_kind` patterns before subscribing to the bus. `*` covers all; `foo.*` covers `foo.X`, `foo.X.Y`, and `foo.X.*`. Without this, declaring overlapping kinds would cause the same event to land in multiple subscriptions and trigger every matching action once per delivery.

**Reach via `TriggerSink` trait:** `TriggerEngine` invokes through an `Arc<dyn TriggerSink>` (default impl on `ActionRegistry`). Platforms can plug in a wider sink — copad-linux uses `LiveTriggerSink` which tries the registry first and falls through to `socket::dispatch` for legacy match-arm commands, so triggers can fire any command handled by `socket::dispatch` (`tab.*`, `terminal.exec`, `webview.*`, `plugin.*`, …) without per-command migration. Exception: `event.subscribe` is special-cased earlier in `socket::start_server` (it owns the connection for the lifetime of the stream) and is intentionally not reachable from triggers — its semantics don't fit a fire-once trigger action. **Async error path for fallthrough:** the sink hands `socket::dispatch` a clone of a shared reply channel and a dedicated consumer thread `eprintln!`s any `ok=false` response (typos, unknown methods, runtime errors) to stderr — format: `[copad] trigger fallthrough id=... failed: <code>: <msg>`. Per-event `fired` count over-counts on fallthrough — it counts queueing as success — but misconfigured trigger actions are still visible on stderr. Registry actions retain full SYNC error semantics; migration of a hot action into the registry recovers the synchronous `fired` accounting too.

### context.rs

Live snapshot of "what the user is currently doing." Reads from the Event Bus. See [workflow-runtime.md](./workflow-runtime.md) for the broader Context Service design and how triggers / AI agent / command palette consume it.

```rust
Context { active_panel: Option<String>, active_cwd: Option<PathBuf> }
ContextService::new()
ContextService::apply_event(&Event)
ContextService::snapshot() -> Context
ContextService::active_panel() -> Option<String>
ContextService::active_cwd() -> Option<PathBuf>
```

**v1 fields:** `active_panel` and `active_cwd` only — these are the two with confirmed event-stream sources right now. Future fields (`recent_commits`, `upcoming_events`, `unread_mentions`, `open_documents`, …) land alongside their providers.

**Drive pattern (caller side):** `ContextService` is platform-agnostic and does not own a thread. The caller subscribes to `EventBus` and feeds events into `apply_event`:

```rust
let rx = bus.subscribe("*");
glib::timeout_add_local(50ms, move || {
    while let Some(event) = rx.try_recv() { ctx.apply_event(&event); }
    Continue
});
```

**Consumed event kinds:** `panel.focused` (set active panel), `panel.exited` (clear that panel's state, and active if it was), `terminal.cwd_changed` (record cwd in per-panel map). All other kinds are ignored — `apply_event` is safe to call with the firehose subscription `"*"`.

**Why not `tab.closed`?** Its cross-platform payload contract is `{index}` only (see [architecture.md](./architecture.md) event-stream table). copad-linux currently emits a superset that includes `panel_id`, but acting on that would couple the core primitive to Linux-incidental behavior and silently no-op on macOS. Cleanup relies on `panel.exited`, which both platforms emit consistently with `panel_id` on shell process exit. (If a tab is closed without the shell exiting, the per-panel cwd entry lingers — bounded by UUID space, GC'd on process restart.)

**Per-panel cwd cache:** focus switching reflects cached cwd of the newly-focused panel immediately, without waiting for that panel to re-emit `terminal.cwd_changed`.

**Thread safety:** `RwLock<Inner>`. Multiple threads can call `snapshot` / `active_*` concurrently; `apply_event` takes a short write lock. Malformed payloads are ignored, never panic.

### action_registry.rs

Name → handler map for all invocable actions (see [workflow-runtime.md](./workflow-runtime.md)). v1 is synchronous; async registration will be added when the first service provider (Calendar, Slack) needs non-blocking I/O.

```rust
ActionRegistry::new()
ActionRegistry::register(name, |params| -> Result<Value, ResponseError>)
ActionRegistry::invoke(name, params) -> Result<Value, ResponseError>
ActionRegistry::has(name) -> bool
ActionRegistry::names() -> Vec<String>   // sorted
ActionRegistry::len() / is_empty()
```

**Errors:** returns `copad_core::protocol::ResponseError` so socket dispatch can wrap it directly in a `Response::error(...)`. Error helpers:

- `action_registry::invalid_params(msg)` — `code: "invalid_params"`
- `action_registry::internal_error(msg)` — `code: "internal_error"`
- Unknown action — `code: "action_not_found"` (produced by `invoke()`)

**Thread safety:** backed by `RwLock<HashMap>`. Multiple threads can invoke concurrently; registration takes a short write lock. Handlers must be `Fn(Value) -> ActionResult + Send + Sync + 'static` — state capture via `Arc<Mutex<T>>` or `Arc<AtomicX>`.

**Not wired yet:** this is a pure primitive. `copad-linux/socket.rs`'s `dispatch()` still uses its hard-coded match. Migration is incremental — new commands register through the registry; legacy commands move over one at a time.

### event_bus.rs

In-process pub/sub hub for the workflow runtime (see [workflow-runtime.md](./workflow-runtime.md)). All internal event sources (shell signals, VTE output, service providers) publish through this bus; the socket `event.subscribe` stream becomes a projection of it.

```rust
Event { kind: String, source: String, timestamp_ms: u64, payload: Value }
EventBus::new() / with_default_buffer(n)
EventBus::publish(event)
EventBus::subscribe(pattern) -> EventReceiver               // bounded (default)
EventBus::subscribe_with_buffer(pattern, n) -> EventReceiver // bounded, custom size
EventBus::subscribe_unbounded(pattern) -> EventReceiver     // lossless, for wire streams
EventReceiver::try_recv() -> Option<Event>
EventReceiver::recv() -> Option<Event>
```

**Pattern matching:** `*` matches all, `foo.*` matches any kind starting with `foo.` (deep — `foo.bar.baz` matches), otherwise exact string match.

**Delivery modes:**
- **Bounded** (`subscribe` / `subscribe_with_buffer`, default 256): `sync_channel` + `try_send`. On full buffer, the new event is dropped for that subscriber with a warn log — publisher never blocks. Right choice for in-process consumers that poll (plugin panels, UI bridges).
- **Unbounded** (`subscribe_unbounded`): plain `mpsc::channel`. Never drops; memory grows if consumer stalls. Required for external wire contracts (e.g. socket `event.subscribe`) where event loss would violate the client API. The caller must drain promptly or risk unbounded memory.

Disconnected subscribers are pruned lazily on the next publish (both modes).

**Thread safety:** `EventBus` is `Sync`; any thread can publish. Receivers are single-consumer (not `Clone`) — platform UIs drain via `try_recv` on their main thread (GTK: `glib::timeout_add_local`; AppKit: DispatchSource).
