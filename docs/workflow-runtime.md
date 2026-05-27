# Workflow Runtime

copad's long-term identity: a personal workflow runtime that happens to surface through a terminal. Services (calendar, messengers, docs, knowledge base), triggers, and the AI agent all plug into three shared abstractions in `copad-core` so that adding an integration is `1 new source + N actions`, not `1 new source × N consumers of wiring`.

## Why three abstractions

Without shared primitives, each new service needs its own:

- event stream plumbing (source → socket → subscribers)
- action invocation surface (socket command, CLI subcommand, plugin JS call, AI tool)
- "what is the user doing right now" logic

Three abstractions collapse this fan-out:

| Abstraction | Collapses |
|---|---|
| **Event Bus** | All inbound signals (shell, VTE, calendar ticks, slack events, git hooks, timers) |
| **Action Registry** | All outbound effects (spawn tab, send message, create event, query doc) |
| **Context Service** | All "current state" queries (cwd, active tab, next meeting, recent mentions) |

Every new integration registers sources with the bus, actions with the registry, and optionally contributes to context. Triggers, UI panels, plugins, and the AI agent all _consume_ these three — nothing else.

## Event Bus

Pub/sub hub in `copad-core`. All lifetime events flow through it; the socket `event.subscribe` stream becomes a thin projection to external clients rather than the event system itself.

### Event shape

```rust
pub struct Event {
    pub kind: String,          // "terminal.cwd_changed", "calendar.event_imminent", "slack.mention"
    pub source: String,        // "shell", "calendar", "slack", …
    pub timestamp: SystemTime,
    pub payload: serde_json::Value,
}
```

`kind` is a dotted string, not an enum — plugins and external integrations must publish new event kinds without modifying the core crate.

### Publish / subscribe

```rust
pub trait EventBus: Send + Sync {
    fn publish(&self, event: Event);
    fn subscribe(&self, pattern: &str) -> EventReceiver;        // glob on kind, e.g. "calendar.*"
    fn subscribe_filtered(&self, f: Box<dyn Fn(&Event) -> bool + Send>) -> EventReceiver;
}
```

Delivery is non-blocking via `mpsc` channels per subscriber (already the pattern used in `copad-macos/EventBus.swift`). Two modes:

- **Bounded (`subscribe` / `subscribe_with_buffer`):** `sync_channel` + `try_send`. Slow subscribers never block publishers; when the buffer is full the incoming event is dropped for that subscriber with a warn log. Default for in-process consumers (plugin panels, UI bridges).
- **Unbounded (`subscribe_unbounded`):** plain `mpsc::channel`. Never drops. Required for external wire contracts like the socket `event.subscribe` projection where dropping would silently violate the client API. Caller must drain promptly.

Disconnected subscribers are cleaned up lazily on the next publish in both modes.

### Relationship to existing systems

- Existing socket `event.subscribe` remains the external API; internally it becomes `bus.subscribe_unbounded("*")` (lossless — see delivery modes above) and serializes events to JSON over the socket.
- VTE signal handlers, focus controllers, tab manager — all refactored to publish through the bus instead of calling socket broadcast directly.
- Platform UIs subscribe to relevant kinds on the GTK / main thread via the existing `mpsc → glib::timeout_add_local` bridging pattern.

## Action Registry

Name → handler map. Every capability copad exposes (today's socket commands, plugin shell commands, future service actions) registers here. The socket dispatcher, CLI, plugin `copad.call()`, keybindings, triggers, and the AI agent all resolve through the same registry.

### Action shape

```rust
pub struct ActionSpec {
    pub name: String,               // "tab.new", "calendar.create_event", "plugin.notes.search"
    pub title: String,              // human-readable for command palette
    pub params_schema: JsonSchema,  // for AI agent tool use + command palette validation
    pub context_scope: Scope,       // requires active terminal id, global, plugin-scoped, …
}

pub trait ActionHandler: Send + Sync {
    fn invoke(&self, params: Value, ctx: &ActionContext) -> BoxFuture<Result<Value, ActionError>>;
}
```

Handlers are async so that service calls (HTTP, WebSocket) are non-blocking without reinventing completion-based wrappers per call site.

### Invocation sources

| Source | Path |
|---|---|
| Socket command | dispatcher looks up `method` in registry |
| CLI (`coctl`) | subcommands map to action names |
| Plugin JS bridge | `copad.call(name, params)` |
| Keybinding | config maps key → `{action, params}` |
| Trigger | config rule's `action` field |
| AI agent | registry emits JSON Schemas as tool definitions |
| Command palette (future) | fuzzy search over registry titles |

Existing socket commands migrate _incrementally_. The dispatcher keeps its hard-coded match for a while; new commands register through the registry from day one. No big-bang refactor.

**Trigger reach (current):** the `TriggerSink` trait is the seam — default impl on `ActionRegistry` covers registered actions; copad-linux's `LiveTriggerSink` extends reach by falling through to `socket::dispatch` for legacy match-arm commands. Net effect: every command handled by `socket::dispatch` (`tab.*`, `terminal.exec`, `webview.*`, `plugin.*`, …) is trigger-reachable today. Exception: `event.subscribe` is special-cased earlier in the socket server (it owns the connection for the lifetime of a stream) and is not a meaningful trigger sink.

**Completion-event fan-out (Phase 14.1):** when constructed with `ActionRegistry::with_completion_bus(bus)`, every dispatch (invoke / try_invoke / try_dispatch) auto-publishes `<action>.completed` (Ok, payload = action's return `Value`) on success and `<action>.failed` (Err, payload = `{code, message}`) on failure. Source field `copad.action`. Sync handlers publish from the caller thread inline before the `Responder` runs (or before invoke/try_invoke return); blocking handlers publish from the worker thread. **Scope caveat**: only actions REGISTERED through `ActionRegistry` get fan-out — legacy commands still living in `socket::dispatch`'s match-arm fallthrough (`tab.*`, `terminal.exec`, `webview.*`, `plugin.<name>.<cmd>`) bypass the registry on miss and therefore don't emit completion events today. Plugin-provided actions (`kb.*`, `git.*`, `slack.*`, `todo.*`, `llm.*`, `calendar.*`) and migrated copad-internal actions (`system.log`) all chain. **Timing caveat**: the bus-level ordering guarantee is "publish first, then the upstream continuation returns." When the downstream chained trigger actually fires depends on the platform pump cadence — copad-linux drains trigger subscriptions once per GTK tick, so a completion event published while processing a tick is typically picked up on the NEXT tick. Same-tick chaining is not guaranteed; semantically-correct chaining is. High-frequency built-ins (`system.ping`, `context.snapshot`) opt out via `register_silent` so their completion events don't dwarf real workflow events on the bus.

**Beyond Phase 8 — plugin-first evolution:** the natural next layer is hosting external integrations (Calendar, Slack, KB, LLM, Notion) as **service plugins** rather than copad-core modules. The runtime primitives this doc describes are conceptually a plugin host already; the missing piece is a long-running supervised-subprocess model with a documented stdio RPC protocol. The headline rule from that protocol — for cross-reference here — is that each service's `[[services]]` manifest entry declares both `provides = [action names]` and `subscribes = [event-kind globs]` as the source of truth, and the runtime `initialize` reply is checked asymmetrically against the manifest (subset OK as degraded mode for both fields; superset rejected with a warning, plugin keeps serving its manifest-approved set). Conflict between two plugins claiming the same `provides` entry resolves by lexical `[plugin].name`. See [service-plugins.md](./service-plugins.md) for the end-state vision, all the decisions and rationale, and the Phase 9–18 roadmap. Fallthrough surfaces failures asynchronously via a reply-consumer thread (`eprintln!` to stderr) — the trigger pump can't block on the reply because it runs on the GTK main thread that would later process the queued command. Migrating a hot action into the registry recovers full sync error semantics and accurate `fired` accounting.

## Context Service

Centralized read model of "what is the user doing right now." Services contribute, any consumer can query. This is what makes triggers and the AI agent feel aware rather than blind.

### Shape

```rust
pub struct Context {
    pub active_panel: Option<String>,         // panel_id of focused panel
    pub active_cwd: Option<PathBuf>,          // cwd of active_panel (per-panel cache)
    pub presence: Presence,                   // Active | Away — manual toggle
}
```

v1 carries only fields with confirmed event-stream sources or daemon-state writers. The grand-vision shape (recent commits, upcoming calendar events, unread mentions, open documents) lives in this doc as design intent — providers land incrementally as they prove their event-stream is reliable.

`presence` is daemon-state, not event-derived: `coctl presence away|active` flips it via the `presence.set` socket method, and the daemon publishes `presence.changed { previous, current }` on each non-noop flip so triggers + GUI status indicators can react. Default on daemon boot is `Active`. Surface in trigger conditions: `context.presence == "away"` (double-quoted; serialized form is lowercase string). See decisions.md #41.

### Contributors

Services register as providers that either subscribe to bus events or poll on a schedule, writing into their slot of the Context:

```rust
pub trait ContextProvider: Send + Sync {
    fn name(&self) -> &str;
    fn refresh(&self, ctx: &mut Context);
}
```

The preferred pattern: every provider subscribes to its own event kinds on the bus and updates the Context incrementally. Polling is the fallback for services without push (e.g., polling Calendar every 2 minutes).

### Consumers

- **Triggers**: `when = { event_starts_in = "5m" }` resolves against `upcoming_events`
- **AI agent**: Phase 6's `terminal.context` grows into `workflow.context` with all fields
- **Command palette**: context-aware suggestions (e.g., show Notion docs matching cwd's project name)
- **Status bar plugins**: "next meeting in 23m" module reads from context

## Triggers (config-driven automation)

Triggers are the user-facing feature that makes the three abstractions worth their weight. A trigger is `(event pattern or schedule) → action`, defined declaratively in config and hot-reloadable (copad already has config hot-reload).

```toml
[[triggers]]
name = "meeting-prep"
when = { event_kind = "calendar.event_imminent", minutes = 10 }
# `kb.ensure` is registered by the KB service plugin (Phase 9.3); see
# service-plugins.md. v1 fires kb.ensure only — the meeting note path is
# created/refreshed but the user opens it themselves. Auto-opening in a
# WebView panel needs the chained-trigger primitive **scheduled for
# Phase 14** (see roadmap.md): once `<action>.completed` events fan out
# on the bus, this trigger gets a follow-up that consumes
# `kb.ensure.completed` and fires `webview.open`.
action = "kb.ensure"
params = { id = "meetings/{event.id}.md", default_template = "# {event.title}\n\n" }

[[triggers]]
name = "slack-alert"
# `slack.mention` is published by the Slack service plugin (Phase 11);
# see service-plugins.md. The action below is a built-in core action
# that already exists on the legacy match-arm dispatch path.
when = { event_kind = "slack.mention", channel = "alerts" }
action = "terminal.exec"
params = { command = "echo {event.text} >> ~/pings.log" }
```

Parameter interpolation (`{event.foo}`, `{context.bar}`) handles dynamic action arguments. Conditional firing — "skip if I declined", "skip the weekly 1:1" — is expressed by an optional `condition` clause on each trigger, evaluated AFTER `when` matches. The expression DSL supports `== != < <= > >= && || !` plus parens, references like `event.X.Y` / `context.X`, and string/number/bool/null literals. Conditions are compiled once at config load; a parse failure drops THAT trigger only. See `copad-core/src/condition.rs` for the full grammar and Phase 10.2 in roadmap.md for the rollout history.

### Async correlation (`await` clauses)

A trigger may declare an `await` clause to hold a chain until a follow-up event arrives. The engine uses a two-phase state machine:

1. **Preflight** — when an await-bearing trigger fires its action, an entry lands in `preflight_awaits`. Promotion waits for `<action>.completed` because `LiveTriggerSink` returns `Ok({queued: true})` synchronously for blocking/plugin actions BEFORE the action actually runs. Arming on the sink's bare `Ok` would queue an await even when the action later fails async.
2. **Pending** — on `<action>.completed`, the entry promotes to `pending_awaits` and watches for a follow-up event matching the configured `event_kind` + `payload_match`. On match, the engine publishes a synthesized `<trigger_name>.awaited` event for downstream chains.

**Trust boundary**: only events whose `source == "copad.action"` (the action registry's completion fan-out, see `action_registry::COMPLETION_EVENT_SOURCE`) advance the state machine. An unrelated bus producer that publishes a `.completed`-suffixed event cannot mutate await state.

**Invocation correlation hazard (FIFO scope, slice-2 follow-up)**: the match key is the action name `X` only — neither `<X>.completed` nor `<X>.failed` carries an originating trigger or invocation id, so we cannot per-invocation correlate. FIFO scope is therefore "across all preflights for action X — regardless of trigger AND regardless of invocation":

- Two different await-bearing triggers that share an action share one FIFO queue. A completion from trigger B can promote trigger A's preflight if A queued first.
- A single trigger fired multiple times concurrently can mis-correlate. If trigger T fires three times (preflights p1, p2, p3) and completions arrive in order c1, c2, c3, FIFO matches them correctly. If completions arrive c2, c1, c3, FIFO promotes p1 with c2's payload, p2 with c1's, p3 with c3's — wrong invocation correlation, but each preflight does end up promoted exactly once.

In practice most use cases have at most one in-flight invocation per action at a time (Slack-ask-and-wait, for example, isn't repeatedly fired). The mis-correlation only hurts when the SAME action is dispatched multiple times in fast succession AND awaited follow-up payloads need to match the specific invocation. Closing this fully requires per-invocation correlation tokens in `<X>.completed`/`.failed` payloads (`action_registry` change).

**Single-consumption matching**: an incoming event resolves at most one pending entry. When two pendings share identical match criteria, only the oldest fires — the runner-up keeps waiting. Broadcasting one reply to multiple concurrent prompts would silently double-fire downstream chains.

**Timeouts**: the deadline is set at preflight registration and carries unchanged through promotion. Total wait (preflight + pending) shares one window. `on_timeout = abort` drops silently; `on_timeout = fire_with_default` synthesizes the awaited event with `null` so downstream chains run with a missing-data fallback.

**Hot-reload**: `set_triggers` swaps the trigger list under a single `RwLock` write (atomic per-list — concurrent dispatch sees old or new, never half-applied) and then clears both await pools under separate locks. `dispatch` clones the trigger snapshot before registering preflights, so any number of in-flight dispatches that captured the OLD snapshot can still register old-generation preflights after the clears. Hot-reload is best-effort cleanup, not a hard isolation boundary — old `<trigger_name>.awaited` events may still fire for events already in flight at swap time. Acceptable because triggers are user-authored config, not a hot data path. Volatile state is restart-loses; reload-loses is the same contract.

## Mapping to existing code

| Current | Becomes |
|---|---|
| `copad-linux/socket.rs` event broadcast | thin `EventBus::publish` caller; external subscribers consume through bus |
| `copad-linux/socket.rs` `dispatch()` match | Action Registry lookup (with hard-coded commands migrating incrementally) |
| Phase 6 `terminal.context` | one slot of Context Service output |
| Plugin `copad.call()` | already an Action Registry consumer — no change in surface |
| Plugin `copad.on()` | already an Event Bus consumer |
| `copad-macos/EventBus.swift` | platform-native mirror of the core bus — already shaped correctly |

Most existing features are already shaped correctly; the refactor is about unifying the internal plumbing that they all share.

## First vertical PoC (superseded — now plugin-first)

The original PoC sketched here described a Google Calendar **provider** built into `copad-core` plus a built-in `workflow.meeting_prep` action. Both have moved to the plugin-first plan. Concretely:

1. `calendar.event_imminent` is published by `copad-plugin-calendar` (Phase 10), not a core module.
2. `Context.upcoming_events` would be contributed by the same plugin via context-provider extension (still TBD; v1 keeps Context to active panel + cwd).
3. The meeting-prep workflow Phase 10 ships is a TOML trigger calling `kb.ensure` (handled by `copad-plugin-kb`, Phase 9.3) only — `~/docs/meetings/<event_id>.md` is created/refreshed and the user opens it themselves. Auto-opening the panel via a chained `webview.open` is scheduled for Phase 14 (composite/chained workflow primitive); see roadmap.md.
4. End-to-end demo for Phase 10: 10 minutes before a real meeting, the kb plugin creates / refreshes the matching note. (Auto-opening lands as a follow-up trigger config update once the chain mechanism exists.)

See [service-plugins.md](./service-plugins.md) Phase 10 for the full plan. This section stays here as a record of the design intent that motivated the runtime primitives.

## Non-goals (v1)

- ~~No DSL for trigger conditions beyond equality and time windows.~~ **Phase 10.2 shipped** a small boolean-expression DSL (`condition` field) for negation, ordering, and AND/OR composition. More complex predicates (function calls, list membership, regex) still belong in plugins.
- No multi-user / shared state. Single user, single machine.
- No natural-language trigger authoring. TOML files only for now.
- Knowledge base semantic search is _later_ — built on top of Context Service, not a parallel system.

## Scope guardrails

- **Never build service clients from scratch** when a mature web UI exists. Embed in a WebView panel instead (`webkit6` on Linux, `WKWebView` on macOS — the existing `PanelVariant::WebView`).
- **Implement native event streams** only for persistent push (WebSocket gateways, webhooks). Everything else polls via provider.
- **Service plugins, not core modules.** Calendar / Slack / Notion / KB / LLM all land as service plugins (subprocess + stdio + newline-JSON, manifest-declared capabilities, lazy activation by default). See [service-plugins.md](./service-plugins.md). The "knowledge base layer" guardrail above is now Phase 9.3 (KB plugin, grep + filename) plus Phase 13 (FTS / embedding upgrade) — same staging, different packaging.
