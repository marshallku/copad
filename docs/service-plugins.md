# Service Plugins: copad as a Plugin-First Personal Workflow Runtime

> Status: planning doc. Captures end-state vision, the architectural pivot to plugin-first, decisions and rationale, and the concrete roadmap.

## End picture

At maturity, copad is a **plugin host** for personal workflow automation. The binaries shipped (`copad`, `coctl`) own the runtime primitives only — Event Bus, Action Registry, Trigger Engine, Context Service, Plugin Loader. Every domain integration (KB, Calendar, Slack, Discord, Notion, LLM, …) lives outside `copad-core`/`copad-linux` as a service plugin: an independent process with its own release cadence, language choice, dependencies, and credentials. The user can swap KB backends (file-based vs Notion vs Obsidian-vault), pick which messenger they want ingested, and write their own integrations without rebuilding copad.

The pattern is identical to VSCode (extensions in a separate process), Neovim (remote plugins via msgpack-rpc), and the LSP ecosystem (servers as separate processes, JSON-RPC over stdio): a small core, a documented protocol, and pluggable everything else. Triggers in TOML config wire bus events to actions; whether the action is satisfied by a built-in handler or a service plugin is invisible to the user.

A working day in this end state:

1. `copad` starts. KB plugin spawns lazily on first `kb.*` call. Calendar plugin starts on `onStartup` because it needs to poll. Slack plugin starts on `onStartup` to keep its WebSocket alive.
2. 10 minutes before a meeting, Calendar plugin publishes `calendar.event_imminent`. A user-defined trigger fires `kb.ensure` (handled by KB plugin) which creates-or-returns `~/docs/meetings/<event_id>.md`. The follow-up step that opens that path in a WebView panel uses the chained-trigger primitive scheduled for Phase 14 — `kb.ensure.completed` events fan out on the bus and a downstream trigger consumes them to fire `webview.open`.
3. A Slack mention arrives. Slack plugin publishes `slack.mention`. A trigger writes the raw thread to `~/docs/.raw/slack/...` and asks the LLM plugin to write a derived summary at `~/docs/threads/<topic>.md`.
4. The user types `coctl draft-reply <topic>`. CLI invokes a registry action that asks the LLM plugin (with KB context as input) to produce a draft reply, opening it in the active terminal.

None of this requires changes to `copad-core`. New integrations are new plugins.

## Why this evolution

The Phase 8 work (`docs/workflow-runtime.md`) gave us the runtime — primitives that are conceptually identical to a plugin host. What was missing was the **service plugin** model: a long-running supervised process that publishes events and serves actions. The Phase 7 plugin system (`docs/plugins.md`) handles UI panels (HTML/JS in WebView) and shell-script commands (one-shot subprocess), but cannot host providers like a Calendar polling daemon or a Slack WebSocket gateway.

Without service plugins, every integration would need to live inside `copad-core` or `copad-linux`. Calendar OAuth flow, Slack WebSocket, Notion API, LLM credential management — all in core. That makes copad a kitchen-sink monolith, locks the user to whatever backends copad chooses (no Obsidian-vault KB, no per-user LLM provider), and makes contribution from outside the main repo very hard.

Plugin-first solves all of this and aligns with how every comparable mature tool (editors and IDEs) architects their integration surface.

## Context: what's already built

### Phase 8 runtime primitives (copad-core)

- `EventBus`: pub/sub. `subscribe(pattern)` (bounded, drop-newest), `subscribe_unbounded(pattern)` (lossless wire contract). 11 unit tests.
- `ActionRegistry`: `Send + Sync` `Fn(Value) -> Result<Value, ResponseError>` handlers, `Arc<dyn Fn>` for safe nested invoke. 12 unit tests.
- `ContextService`: live snapshot from bus events. 10 unit tests.
- `TriggerEngine`: TOML triggers, `{event.*}/{context.*}` interpolation, `covering_patterns` dedup, `TriggerSink` trait. 19 unit tests.

### Phase 7 plugin system (copad-linux)

- `~/.config/copad/plugins/<name>/plugin.toml` manifest with:
  - `[[panels]]` — HTML/JS in WebView, `copad.call()` / `copad.on()` JS bridge
  - `[[commands]]` — fork-per-call shell scripts; auto-registered as `plugin.<name>.<cmd>` socket method
  - `[[modules]]` — status bar widgets

### What's missing for service plugins

- Long-running supervised process model (current `[[commands]]` are one-shot)
- Plugin → bus event publish (current panels can only receive via `copad.on`)
- Explicit, manifest-declared action ownership (current is implicit via `plugin.<name>.<cmd>` autonaming)
- Lazy activation lifecycle (current panels activate on user open, commands on call — no abstraction over startup-vs-on-demand for service plugins)

## Decisions and rationale

### D1: Plugin-first, not core-first

**Decision.** KB, Calendar, Slack, Notion, LLM are service plugins, not modules in `copad-core` / `copad-linux`. The KB _action protocol_ lives in `copad-core` as documented contract; the _implementation_ is a plugin (`copad-plugin-kb`).

**Rationale.** This is the user's stated mental model (VSCode-style). It also matches how every successful editor/IDE architects integrations: VSCode (extensions in extension host process), Neovim (remote plugins), Zed (WASM extensions). Building integrations into core forecloses on user choice (KB backend swap), couples release cadence, and makes third-party contribution painful.

**Alternative considered:** ship KB in core first, refactor later. Rejected because "later" rarely happens and because the protocol decisions made in core become silent constraints on every future plugin. Better to design the boundary first.

### D2: Subprocess + stdin/stdout + newline-JSON

**Decision.** Service plugins are subprocesses. Communication via newline-delimited JSON over stdin/stdout, reusing the existing cmux V2 protocol (already used by socket clients, plugin commands).

**Rationale.** Industry standard:

- LSP: JSON-RPC 2.0 over stdio (the dominant pattern for editor extensions)
- VSCode language servers: stdio + JSON
- Neovim remote plugins: msgpack-rpc, transport-agnostic but stdio is canonical
- WASM (Zed) is the alternative but adds significant complexity (Wasmtime runtime, WIT compilation step) that personal-scale copad doesn't yet need

cmux V2 newline-delimited JSON is already proven in copad. Sticking with it avoids two protocols. LSP-style `Content-Length` header framing is unnecessary as long as JSON serializers don't emit raw newlines (serde_json doesn't).

**Sources:** LSP spec 3.17, VSCode Language Server Extension Guide, Neovim channel docs, Zed extension architecture (see Sources at end).

### D3: Lazy activation by default

**Decision.** Service plugins declare an `activation` event; copad spawns the process only when that event fires. Examples:

- `onStartup` — for plugins that must run from boot (Calendar polling, Slack WebSocket)
- `onAction:kb.*` — spawn on first `kb.*` action invocation
- `onEvent:slack.*` — spawn when something triggers an event in that namespace

**Rationale.** This is what VSCode does. Earlier in this design conversation I (Claude) argued for eager startup on the grounds that lazy adds complexity and 5-10 plugins is small enough. Internet research validated that VSCode chose lazy from the start, and progressively made it MORE lazy (1.74+ implicit activation from `contributes` declarations). Reasons:

- Startup time scales with N plugins
- Memory cost of plugins user doesn't currently use
- Crash blast radius — a plugin that's not running can't crash

The implementation cost of lazy is small: a state machine (`not-started` / `starting` / `ready`) + first-call request buffering. ~200 LOC.

**Alternative considered:** eager-on-startup. Rejected after research showed it's the wrong default at any non-trivial plugin count.

### D4: Manifest-declared capabilities, deterministic conflict resolution

**Decision.** Each `[[services]]` declares its full capability set IN THE MANIFEST: `provides = ["kb.search", "kb.read"]` and `subscribes = ["calendar.event_imminent"]`. The manifest is the source of truth. At plugin load time (BEFORE spawning any process), copad:

1. Walks every enabled plugin's manifest, building a global action-ownership table.
2. **Detects conflicts deterministically by lexical order of plugin name.** When two enabled plugins both declare `provides = ["kb.search"]`, the alphabetically-earlier plugin name wins; the loser's `kb.search` registration is skipped (its other declared actions still register normally). A warning is logged identifying the conflict and which plugin won.
3. Spawns plugins per their `activation` rule.
4. The plugin's `initialize` response is checked asymmetrically against the manifest, applied identically to BOTH `provides` AND `subscribes`:
   - **Subset OK (degraded mode):** the runtime may declare FEWER entries than the manifest. Use case: the plugin started but a backend dependency failed (e.g. `kb-search` library missing) — it can still serve `kb.read` and `kb.append`. copad only wires up what the runtime actually declared.
   - **Superset rejected:** the runtime may NOT declare entries beyond the manifest. Any extras are dropped with a warning; the plugin keeps running for its manifest-approved set. Reason: the pre-spawn ownership analysis (D4 step 1) must stay accurate. Letting plugins claim more at runtime would invalidate the conflict resolution copad already committed to.

   This applies symmetrically: provides-superset would let a plugin grab actions another plugin won (already resolved); subscribes-superset would force copad to forward event kinds the global subscription set didn't account for.

**Plugin identity is the manifest `[plugin].name` field** (consistent with the existing plugin contract in [plugins.md](./plugins.md)), NOT the directory. Lexical comparison is on that name. User controls precedence by:

1. Enabling/disabling plugins (the primary lever — most users will simply not install both).
2. Editing the manifest `[plugin].name` of a plugin to change its sort key (e.g. rename `[plugin].name` from `kb-obsidian` to `aaa-kb-obsidian` to force it to win).

Future enhancement: explicit `~/.config/copad/plugin-precedence.toml` if name-based control proves too indirect.

**Rationale.**

- **Manifest as source of truth** lets copad validate the whole plugin set BEFORE spawning anything. No race conditions, no "depends on which plugin started first" weirdness.
- **Lexical name ordering** is deterministic across runs and OS / filesystem variations. Filesystem mtime, install order, or process startup order would all be fragile.
- **Initialize response confirms manifest** so a plugin can't silently expand its capability set after manifest review. The plugin can declare LESS than its manifest at runtime (degraded mode) but can't claim more than the manifest authorized.

VSCode is more permissive at runtime (last-registered command wins) but VSCode also requires manifest declaration of contributions, so the failure mode is mostly identical: predictable from manifest inspection. copad's stricter "manifest is the truth" is safer for the personal-use single-machine scenario where a stale-process race is more annoying than helpful.

### D5: Initialization handshake (LSP style)

**Decision.** When copad spawns a service, the first exchange is an `initialize` request from copad (`{copad_version, protocol_version}`), and the service responds with its full capability snapshot (`{provides, subscribes, version}`). Then `initialized` notification flows. Only after this does normal RPC begin.

**Rationale.** LSP's pattern, validated for 8+ years across hundreds of language servers and clients. Benefits:

- Version negotiation: copad and plugin can refuse incompatible versions cleanly
- Capability discovery: copad doesn't infer; plugin declares
- Clear lifecycle phase boundary: setup vs running

Without this, `action.register` calls trickling in unordered creates ambiguity (when is the plugin "ready"?).

### D6: KB action protocol in copad-core, implementation in plugin

**Decision.** `copad-core` ships a `docs/kb-protocol.md` defining what `kb.search` / `kb.read` / `kb.append` / `kb.ensure` accept and return. **No KB code in copad-core.** A first-party `copad-plugin-kb` (separate Cargo crate or even separate repo) implements grep over `~/docs`. Other backends (Notion, Obsidian) are alternative plugins.

The KB `id` is a logical `<folder>/<filename>`-style path-like key, the same shape across every backend. FS backends use it as a relative path; non-FS backends translate it to their internal UUIDs / vault IDs. This shape is load-bearing for the rest of the protocol surface (parent-folder auto-create on `kb.ensure`, `.raw/` search exclusion, `kb.search.folder` prefix filter, caller-constructed ids like `meetings/{event.id}.md` in triggers) — those affordances only work if every backend agrees on the path-like shape. See [kb-protocol.md](./kb-protocol.md) Design constraints (2) and (3) for the precise contract.

**Rationale.** LSP's design split: the protocol defines what's possible; servers implement it. This decouples the contract from any specific implementation. `~/docs` is the user's chosen backend; making it a plugin means others can swap it without modifying core. And the contract becomes a stable boundary that triggers, AI agents, and command palette all rely on without caring who serves it.

### D7: Backward compatibility with existing `[[commands]]`

**Decision.** The `[[services]]` model is purely additive. Existing shell-script `[[commands]]` plugins keep working unchanged.

**Rationale.** Users have already invested in the existing plugin format. We're adding a new lifecycle option, not migrating commands. Old plugin = subprocess-per-call (current); new service plugin = supervised long-running process. Both can coexist in one plugin.toml.

### D8: Defer LLM as an action

**Decision.** No `claude.complete` (or similar) plugin shipped in the first vertical. The Calendar PoC opens the relevant note for the user; LLM-driven prep is a future plugin.

**Rationale.** LLM adds:

- Recurring API cost ($)
- Credential management complexity
- Network failure modes
- A new dependency on output quality

Calendar + KB without LLM is already useful — auto-opening the right note before a meeting is high signal. LLM amplifies but doesn't unlock the workflow. Better to ship the surface first, add LLM when the user feels its absence.

**Tradeoff (acknowledged):** "smart" demos arrive later. The system feels less impressive at first. Acceptable given copad is for the user's own use, not external demo.

### D9: Defer KB indexing upgrades; design contract to allow them

**Decision.** First KB plugin is grep + filename match over `~/docs`. No SQLite FTS, no embeddings. But the action protocol (D6) is shaped so that `kb.search` returns ranked `KbHit { id, score, snippet }` results — already compatible with FTS or vector search later. Internal storage of the plugin can change without breaking the protocol.

**Rationale.** Personal scale (~10k docs) is fine for grep. Indexing matters only when grep gets slow on every search. Building it now is premature — but designing the action contract NOT to preclude it is essential.

## Architecture

### Manifest extension

```toml
# ~/.config/copad/plugins/kb/plugin.toml

[plugin]
name = "kb"
title = "Knowledge Base"
version = "1.0.0"

[[services]]
name = "main"                      # service identifier within the plugin
exec = "copad-plugin-kb"            # binary in PATH or relative to plugin dir
activation = "onAction:kb.*"       # lazy: spawn on first kb.* action
restart = "on-crash"               # other: "never", "always"
provides = ["kb.search", "kb.read", "kb.append", "kb.ensure"]  # actions this service handles
subscribes = []                    # bus event-kind globs the service wants forwarded
# alt: activation = "onStartup"    # eager (calendar polling, slack gateway)
# alt: activation = "onEvent:foo.*"
```

Existing sections (`[[panels]]`, `[[commands]]`, `[[modules]]`) coexist unchanged.

### Initialization handshake

```
copad spawns the service binary.

copad → service (stdin):
{
  "id": "init-1",
  "method": "initialize",
  "params": {
    "copad_version": "0.x.y",
    "protocol_version": 1
  }
}

service → copad (stdout):
{
  "id": "init-1",
  "ok": true,
  "result": {
    "service_version": "1.0.0",
    "provides": ["kb.search", "kb.read", "kb.append", "kb.ensure"],
    "subscribes": []
  }
}

copad → service:
{
  "id": "init-2",
  "method": "initialized",
  "params": {}
}

# Normal RPC begins.
```

If `provides` conflicts with another loaded plugin, copad logs a warning and skips registration of the conflicting names; non-conflicting names still register.

### Bidirectional RPC (newline-JSON over stdio)

#### copad → service

| Method           | Params                                  | Notes                             |
| ---------------- | --------------------------------------- | --------------------------------- |
| `initialize`     | `{copad_version, protocol_version}`    | first message                     |
| `initialized`    | `{}`                                    | ack of init                       |
| `action.invoke`  | `{name, params}`                        | service is the registered handler |
| `event.dispatch` | `{kind, source, timestamp_ms, payload}` | matches a `subscribes` pattern    |
| `shutdown`       | `{}`                                    | clean stop request                |

#### service → copad

| Method          | Params             | Notes                                           |
| --------------- | ------------------ | ----------------------------------------------- |
| `event.publish` | `{kind, payload}`  | publishes to bus; copad fills source/timestamp |
| `action.invoke` | `{name, params}`   | call ANOTHER service's action                   |
| `log`           | `{level, message}` | stderr-style logging routed via copad          |

### Lifecycle and supervision

**States** per service: `Stopped` → `Starting` → `Running` → (`Crashed` | `Stopped`) → restart-or-stay-stopped.

**Activation events** trigger a transition `Stopped → Starting`:

- `onStartup` — fires immediately at copad boot
- `onAction:<glob>` — fires when an action matching the glob is invoked
- `onEvent:<glob>` — fires when an event matching the glob is published (rare)

**During `Starting`**, requests for the service are buffered (bounded, e.g. 64 deep). When `initialized` arrives, buffer drains in arrival order. If the service doesn't initialize within a timeout (5s default), it's marked failed; pending invokes return `ResponseError { code: "service_unavailable" }`.

**`restart` policies**:

- `on-crash` — restart only on non-zero exit; back off exponentially on repeated failures (1s, 2s, 4s, capped at 60s)
- `always` — restart even on clean exit
- `never` — log and stay stopped

## Roadmap

Each "Turn N.x" is one commit-sized unit (codex review + save.sh).

### Phase 9: Service Plugin Protocol & Host

**9.1 Protocol design + supervisor + mock echo plugin** (turn 1, this phase)

- Implement service-plugin manifest parsing in `copad-core::plugin` — `[[services]]` with `name`, `exec`, `activation`, `restart`, **`provides`**, **`subscribes`**.
- Add supervisor in `copad-linux` (spawn, monitor stdio, restart on policy)
- Wire initialization handshake: copad→service `initialize` carrying `{copad_version, protocol_version}`; service→copad reply with capability snapshot covering both `provides` and `subscribes`. Compare reply to manifest with the same asymmetric rule applied identically to both fields: subset OK (degraded), superset rejected with warn — plugin keeps serving its manifest-approved set so the pre-spawn ownership/subscription analysis stays accurate.
- Wire bidirectional RPC: copad→service `action.invoke` / `event.dispatch`; service→copad `event.publish` / `log`
- Lazy activation: `onStartup`, `onAction:<glob>`, `onEvent:<glob>`. Buffer requests during `Starting`. Init timeout → `service_unavailable`.
- Deterministic conflict resolution: walk all enabled plugin manifests at load time, build the global action-ownership table BEFORE spawning anything; on `provides` collision, the alphabetically-earlier `[plugin].name` wins, others skip just the conflicting entry (rest of their declarations register normally). Warn loudly with both names + the conflicting action.
- Mock plugin: a Rust binary `copad-plugin-echo` with `activation = "onStartup"`, registers action `echo.ping`, publishes `system.heartbeat` every 30s. Practically useful as a debug heartbeat. Verifies protocol shape.

**9.2 KB action protocol** (turn 2) — DONE, see [kb-protocol.md](./kb-protocol.md)

- ✅ `docs/kb-protocol.md` ships request/response shapes for `kb.search`/`kb.read`/`kb.append`/`kb.ensure` plus shared error codes. Backend-agnostic: hit `id` is the stable round-trip handle; `score` is relative-only; `path` is best-effort (FS backends populate, others null); `match_kind` is forward-compat for FTS5 / vector / semantic search.
- ✅ Folder conventions documented (`meetings/`, `people/`, `threads/`, `notes/`, `.raw/`).
- ✅ Forward-compat notes pin down which fields are reserved for backward-compat additions vs which require a protocol version bump.

**9.3 First-party KB plugin** (turn 3)

- `copad-plugin-kb` Rust binary: registers `kb.*` actions, grep + filename search over `~/docs`
- Lazy activation `onAction:kb.*`
- E2E: `coctl call kb.search "meeting"` returns hits

### Phase 10: Calendar (first vertical PoC)

**10.1 Calendar plugin scaffold** (turn 1)

- `copad-plugin-calendar` Rust binary
- Google Calendar OAuth flow (device-code or browser-redirect)
- Polling loop, publishes `calendar.event_imminent` at lead times configured in plugin's own settings file
- `activation = "onStartup"`

**10.2 Meeting-prep trigger** (turn 2)

- TOML trigger: on `calendar.event_imminent`, call `kb.ensure` to get-or-create `~/docs/meetings/<event_id>.md`. Auto-opening the panel is **scheduled for Phase 14** (chained `webview.open` after `kb.ensure.completed`). v1 ships note creation only — the user opens it.
- E2E: launch copad, fake or wait for a real calendar event, observe `~/docs/meetings/<event_id>.md` created/refreshed.

### Phase 11: Messenger ingestion

**11.1 Slack plugin** (Discord pattern is the same once Slack works)

- Slack OAuth + WebSocket (RTM/Events API)
- Publishes `slack.mention`, `slack.dm`, etc. with payload including thread URL
- Stores raw message JSON to `~/docs/.raw/slack/...` (fidelity)

**11.2 Raw archive + write actions** (shipped — see roadmap.md)

- `slack.raw` firehose event, `slack.post_message` write action, `.raw/slack/<team>/<event_id>.json` archive pattern via `kb.ensure`.

**11.3 Derived markdown ingestion trigger** (depends on Phase 14 chained-trigger primitive)

- TOML trigger: on `slack.mention`, call LLM action to summarize thread, write derived markdown to `~/docs/threads/<topic>.md`. Blocked on Phase 14 because the LLM result has to feed `kb.ensure` in the same workflow.

**11.4 Discord plugin** (Phase 20 — shipped both slices)

- Same external surface as Slack: `copad-plugin-discord auth` one-time CLI seeds keyring; `onStartup` activation runs the Gateway WebSocket whenever copad is up.
- Events: `discord.mention` (bot @-mention or @everyone), `discord.dm` (no `guild_id`), `discord.message` (guild channel, no mention), `discord.raw` (full firehose with verbatim MESSAGE_CREATE `d` object). Self-messages and bot-authored messages are filtered from the non-raw events to prevent feedback loops; raw still receives them so archive triggers see full fidelity.
- Filter precedence: a single MESSAGE_CREATE produces ONE non-raw event. Mention wins over DM wins over message — a DM that mentions the bot fires `discord.mention`, not `discord.dm`. A trigger that wants "every message including mentions" registers TWO triggers (one on `discord.message`, one on `discord.mention`) with the same action body — `payload_match` on the underlying message event would not work because mention-classified messages never reach `discord.message`.
- Action: `discord.send_message` (POST `/channels/{id}/messages`). Failure codes surface STRUCTURALLY as the action's top-level error code so triggers can match on them: `rate_limited` (HTTP 429, with Retry-After in the message body), `discord_<numeric>` (e.g. `discord_50001` for Missing Access; covers every Discord API error code from <https://discord.com/developers/docs/topics/opcodes-and-status-codes#json>), or `io_error` for transport-level failures with no Discord error body.
- Privileged MESSAGE_CONTENT intent (1<<15) is required for non-mention message bodies to arrive — toggled on the application's Bot tab. Without it, `event.content` is empty for messages that don't @-mention the bot, breaking keyword-match triggers. The plugin requests it unconditionally; Discord rejects IDENTIFY with close 4014 if the application doesn't have it enabled, and the gateway loop surfaces the close code with a hint.
- Channel scoping is a trigger-side concern (same posture as Slack). The plugin emits messages from every channel the bot can see; users narrow with `payload_match { channel_id = "..." }` or `payload_match { guild_id = "..." }`. See `plugins/discord/triggers.example.toml` for the canonical patterns (raw archive, mention → tasks, single-channel subscription, ask-and-wait DM reply via Phase 14.2 await).

### Phase 12: LLM plugin

**12.1 `copad-plugin-llm`** (shipped — see roadmap.md for details)

- Single primitive `llm.complete` for text generation; higher-level
  patterns (`summarize`, `draft_reply`) collapse into trigger config
  with different system prompts on top of the same call.
- Anthropic provider only for v1. Multi-provider abstraction
  deferred to 12.2 — adding it before a second provider is
  committed is premature.
- Credentials: `ANTHROPIC_API_KEY` env or keyring (Linux Secret
  Service / macOS Keychain), with plaintext 0600 fallback gated
  by `COPAD_LLM_REQUIRE_SECURE_STORE`. NOT the abandoned
  `~/.config/copad/secrets.toml` design — every credential-bearing
  plugin has converged on the keyring-or-plaintext pattern.
- `llm.usage` aggregates token counts from a JSONL log at
  `$XDG_DATA_HOME/copad/llm-usage-<account>.jsonl`. No USD cost
  computation — pricing tables would go stale fast; users compute
  cost in their own dashboards using `llm.usage` × current rates.
- `llm.auth_status` mirrors the slack/calendar shape.

**12.2 (deferred)** — multi-provider, streaming SSE, per-action
timeout override.

**12.3 (depends on Phase 14)** — Phase 11.3-style derived markdown
ingestion that composes `kb.search` + `kb.read` + `llm.complete`

- `kb.ensure`. Unblocked once Phase 14's chained-trigger
  primitive lands.

### Phase 13: KB indexing upgrade (when grep is slow)

- SQLite FTS5 sidecar index, rebuilt on file change (filesystem watcher)
- KB plugin internal change only — protocol unchanged

### Recommended execution order (Phases 14–18)

Phase numbers reflect topical organization. After inter-system connectivity analysis, the build sequence is **plugin-first, Phase 14 mid-stream**:

1. Phase 15.1 (Todo basics + UI) — daily-use surface, single-action triggers cover calendar→todo / jira→todo
2. Phase 17 (Git **worktree** plugin) — single-action `git.worktree_add` already useful via `coctl call`
3. Phase 18 (`claude.start` + tmux session) — small wrapper, manual invocation works without 14
4. Phase 16 (Jira plugin) — Slack pattern, single-action `jira.ticket_assigned → todo.create` flow lands
5. Phase 14.1 (chained `<action>.completed` events) — informed by 4 concrete consumers
6. Phase 15.2 (Todo "start" workflow chain) — `todo.start_requested → git.worktree_add.completed → claude.start` E2E
7. Phase 14.2 (async correlation primitive) — Slack ask → wait for reply → use payload
8. Backfill (Phase 11.3 derived slack markdown, Phase 12.3 LLM ingestion, Phase 10 meeting-note auto-open)

See roadmap.md for the full rationale.

### Phase 14: Composite / chained workflow primitive

Architectural piece, **scheduled mid-stream** (not first — see execution order above). Resolves the long-standing "Chained triggers / composite actions" Open Question (now scheduled, not deferred).

**14.1 Decision** (will commit during prototype):

- (a) Synthetic `<action>.completed` / `<action>.failed` events on every dispatch — downstream triggers match them. Most extensible.
- (b) Composite `workflow.<name>` actions — fixed-shape multi-step procedures in Rust.
- (c) Multi-step trigger TOML with `actions = [...]`.

Recommendation: **(a) + selective (b)**. (a) is the bus-native solution and naturally extends to async-correlation use cases (Slack send → wait for reply → next step). (b) for hand-rolled fixed sequences where TOML noise outweighs the chained-trigger flexibility. (c) loses to (a) on async; not chosen.

**14.2 Implementation plan**

- Action result fan-out: opt-in `register_with_completion_event` flag so high-frequency actions don't spam the bus.
- Async-correlation `await` extension on `[[triggers]]` (Phase 14.2 slice 1, shipped): `await = { event_kind, payload_match, timeout_seconds, on_timeout }`. After the trigger's action fires, the engine waits in a two-phase state machine — preflight-on-dispatch, promote-to-pending on `<action>.completed`, drop on `<action>.failed`. When a follow-up event matches `event_kind` + `payload_match` (interpolated against the original event), a synthesized `<trigger_name>.awaited` event is published; downstream triggers reference that kind. The matched payload is exposed under `event.await.*` (NOT merged into `event.*` directly — the dot-path interpolator extension in Phase 14.2 makes the namespace explicit). On timeout, `on_timeout = "abort"` (default) drops silently, `"fire_with_default"` emits the awaited event with `await: null` so downstream chains can branch on missing data.
- `workflow.<name>` namespace as a future escape hatch.

**14.3 Backfill**: re-enable Phase 11.3 derived markdown ingestion, Phase 12.1 trigger-fired LLM result handling, and the meeting-prep auto-open trigger that's been deferred since Phase 10 (chained `webview.open` after `kb.ensure` for `calendar.event_imminent`).

### Phase 15: Todo system (with UI panel)

User-explicit gap. Workflow entry point AND daily-use UI surface. **Ships in two slices** with Phase 14.1 sandwiched: 15.1 = file format + actions + UI + single-action example triggers (usable today); 15.2 = composite `start` workflow chain (depends on Phase 14.1).

**Packaging**: standalone `copad-plugin-todo` plugin — its own manifest, its own actions, its own UI panel via the existing `plugin.open` activation surface. SHARES the markdown-with-frontmatter file format with KB plugin's filesystem layout but registers its own actions and watcher; KB plugin's surface stays unchanged.

**Slice 1 — basics (single-action, current trigger engine)**:

- File format: markdown checkbox files at `~/docs/todos/<workspace>/<id>.md`, frontmatter carries `status` / `priority` / `due` / `linked_jira` / `linked_slack` / `tags` / `workspace`. File is source of truth — vim-edit + git-version compatible.
- Events via `copad-plugin-todo`'s file-watcher: `todo.created`, `todo.changed`, `todo.completed`, `todo.deleted`.
- Actions: `todo.create` / `todo.set_status` / `todo.list` / `todo.start`. `todo.start` emits `todo.start_requested` for slice 2.
- UI: Plugin Panel route (HTML/JS via existing `plugin_panel.rs`). 3-column kanban (Todo / Doing / Done) with HTML5 drag-and-drop calling `todo.set_status`; column position == status. The Doing column corresponds to `in_progress` and the `Start` button on a Todo card additionally publishes `todo.start_requested` (so the vision-flow-3 chain fires). Items with status `blocked` show in the Todo column with a red badge — there is intentionally no UI to set `blocked` because `vim`-edit of frontmatter is the supported workflow; dragging a blocked card out of Todo will commit the column's status (loses `blocked` by design). A "hide done" toggle (persisted in localStorage) hides the Done column. Default activation goes through `coctl plugin open todo`; keybinding is left to the user's `[keybindings]` config since `Ctrl+Shift+T` is already "new tab". Native GTK widget is the fallback if WebView UX proves insufficient.
- Example triggers: `calendar.event_imminent → todo.create`, `jira.ticket_assigned → todo.create` — both work with current single-action engine.

**Slice 2 — composite `start` workflow** (depends on Phase 14.1) — **shipped, partial**:

- `todo.start_requested → git.worktree_add → claude.start` chains via Phase 14.1's auto-published `<action>.completed` events. Three trigger rows in `examples/triggers/vision-flow-3.toml` (with-jira / without-jira branches for `git.worktree_add`, then `git.worktree_add.completed → claude.start`). Phase 18.2 shipped the prompt seeding piece — clicking Start in the Todo panel now gets a fresh copad tab in the new worktree with claude REPL ready AND the layered prompt already pasted + submitted (when the assembled prompt resolves to non-empty).
- **Layered prompt seeded into claude.start (Phase 18.2)**: `copad-plugin-todo::prompt::assemble` reads `~/docs/claude/global.md` (universal preamble), `~/docs/claude/workspaces/<ws>.md` (per-workspace), the Todo's `prompt` field (or title+body fallback), `linked_jira` key, and the full markdown of each `linked_kb` path; concatenates with section markers and surfaces as **`event.assembled_prompt`** on `todo.start_requested` (distinct from `event.prompt`, which carries the raw per-Todo frontmatter field). `git.worktree_add`'s optional `prompt` passthrough forwards it through the chain to `claude.start`, which delivers it via tmux load-buffer + paste-buffer + Enter once `capture-pane` polling sees claude-specific markers AND `pane_current_command` confirms a claude/node process. Edits to global.md / workspaces files between Todo creation and Start are picked up — the assembly is late-bound. `linked_kb` paths are containment-checked (lexical + symlink-ancestor walk) before reading, matching the KB plugin's posture.
- **Still deferred**: Jira summary enrichment via `jira.get_ticket` fan-in (Phase 16 + 14.2 async correlation). Today `linked_jira` shows up only as the raw ticket key in the assembled prompt.

### Phase 16: Jira plugin

Same shape as Slack — REST + auth + events + actions.

- **Auth**: API token (Atlassian Cloud), email + token combo. Keyring + plaintext fallback like other plugins.
- **Polling** (no public webhooks for desktop): `/rest/api/3/search` for assigned-to-me + watching tickets, default 300s interval (Jira rate limits aggressively).
- **Events**: `jira.ticket_assigned` / `jira.status_changed` / `jira.comment_added` / `jira.mention`. Payload includes `event_json` raw for archive symmetry with `slack.raw`.
- **Actions**: `jira.list_my_tickets` / `jira.create_ticket` / `jira.transition` / `jira.add_comment` / `jira.get_ticket`.
- **Integration with Phase 15**: `jira.ticket_assigned` → `todo.create` linked back to the ticket.

### Phase 17: Git workspace plugin (worktree-first)

Lightweight — local git only, no external API. **Worktrees, not plain branches**: keeps the original repo dir clean, supports concurrent parallel branches in different copad tabs (one tab per worktree), and `git worktree remove` cleanly tears them down when work is done. Branch-only would force stash/switch dance and lose the parallel-tabs property.

- **Workspaces**: `~/.config/copad/workspaces.toml` with `{name, path, default_base, worktree_root?}`.
- **Events**: file-watcher on `.git/HEAD` + `.git/refs/heads/` + `.git/worktrees/` per workspace → `git.worktree_created`, `git.worktree_removed`, `git.branch_created`, `git.branch_deleted`, `git.checkout`.
- **Actions**: `git.list_workspaces` / `git.list_worktrees` / `git.worktree_add {workspace, branch, base?}` / `git.worktree_remove {path, force?}` / `git.current_branch` / `git.status`.
- **Branch name sanitization** built in: `linked_jira="PROJ-456"` → worktree path `<worktree_root>/proj-456/`. Slash-hierarchies preserved as directories (`feat/PROJ-456`).
- **Composability test for Phase 14**: `todo.start_requested` chained to `git.worktree_add` (branch name derived from `event.linked_jira` if present, else `todo-<id>`).

### Phase 18: Claude Code spawn (with tmux session)

Closes the loop: workflow stages a worktree + context, then drops the user into Claude Code **inside a tmux session** so work persists across copad restarts and is reattachable.

- **Action `claude.start {workspace_path, prompt?, session_name?, resume_session?}`** (Phase 18.1 + 18.2 shipped — copad-internal socket action, not a stdio plugin): (1) opens new copad tab with `cwd=workspace_path`, (2) runs `tmux new-session -A -s <session_name>` (attach-or-create — re-running on the same worktree re-attaches the existing tmux), (3) inside tmux runs `claude` (or `claude --resume <id>`). When `prompt` is supplied, a background thread polls `tmux capture-pane` for claude readiness (8s timeout, 200ms cadence) and then delivers the prompt via tmux `load-buffer` + `paste-buffer -d` + `send-keys Enter` — multi-line + special-char safe through the paste buffer. `prompt` and `resume_session` are mutually exclusive (resume restores existing context; seeding new text on top would just confuse claude). Default `session_name` derived from worktree path components. Returns `{panel_id, tab, tmux_session, workspace_path}` immediately; prompt seeding is post-action best-effort and any failure logs to stderr.
- **Persistence wins from tmux**: detach the tab → kill copad → next restart → `claude.start` with same `session_name` reattaches the running claude. Long refactors / multi-step reasoning survive copad crashes.
- Built on `tab.new` + `terminal.exec` — no custom IPC with claude-code.
- **End-to-end Phase 14 chain test (Vision Flow 3)**: `todo.start_requested` → `git.worktree_add {prompt: "{event.assembled_prompt}"}` → `git.worktree_add.completed` (carries the forwarded prompt via the new passthrough field as `event.prompt` on the completion event) → `claude.start {workspace_path, prompt: "{event.prompt}"}` works today end-to-end. Re-clicking Start on the same Todo also works: `git.worktree_add` first scans `git worktree list --porcelain` for an existing entry on the target branch and echoes the recorded path without running `git worktree add` again — the chain reaches claude.start either way, tmux re-attaches the same session, and the prompt is re-pasted as a fresh turn. Jira summary fan-in still pending Phase 16 + 14.2.

## Open questions

- **Plugin distribution.** First-party plugins ship in the copad git repo as separate Cargo crates? Or fully external repos? Initially: same repo, separate crates. Distribution mechanism (registry, install command) is post-MVP.
- **Service plugin in non-Rust languages.** Protocol over stdio is language-agnostic, so a Python or Node plugin works. Need to publish a small "protocol client" library at some point. Defer until first non-Rust contributor needs it.
- ~~**Authentication / per-user secrets.** Calendar OAuth, Slack tokens. Where do they live? Probably `~/.config/copad/secrets.toml`...~~ **Resolved (Phase 10–12)**: every credential-bearing plugin uses the `keyring` crate (Linux Secret Service via D-Bus, macOS Keychain) with plaintext 0600 fallback at `$XDG_CONFIG_HOME/copad/<plugin>-token-<account>.json`. Account label scoped via env var (`COPAD_<PLUGIN>_ACCOUNT`). `<PLUGIN>_REQUIRE_SECURE_STORE=1` opt-in refuses plaintext fallback. The `~/.config/copad/secrets.toml` shared-file design was abandoned — per-plugin keyring entries are simpler and prevent one plugin's credentials from leaking through another's process boundary.
- **Multi-instance copad.** Currently socket per PID. Each copad spawns its own copies of all service plugins? Or shared daemons? For v1, per-instance. Revisit if plugin spawn cost matters.
- ~~**Chained triggers / composite actions.** The current trigger engine maps one event to one action...~~ **Scheduled for Phase 14** (no longer an open question). Three sketches still in play — (a) synthetic `<action>.completed` events with bus-native chaining, (b) hand-rolled `workflow.<name>` composite actions, (c) multi-step `actions = [...]` TOML. Recommended path is (a) primary + selective (b); see roadmap.md Phase 14 for the rationale and implementation plan. Resolving this unblocks Phase 11.3 (derived slack markdown), Phase 12.3 (LLM-backed ingestion), Phase 15.2 (todo `start` chain), and the Phase 10.2 `webview.open` follow-up. **Phase 14 lands mid-stream after the four single-action plugins (15.1, 17, 18, 16) ship — see "Recommended execution order" earlier in this doc.**

## Sources (research informing this plan)

- [VSCode: Our Approach to Extensibility](https://vscode-docs.readthedocs.io/en/stable/extensions/our-approach/)
- [VSCode: Activation Events](https://code.visualstudio.com/api/references/activation-events)
- [VSCode: Language Server Extension Guide](https://code.visualstudio.com/api/language-extensions/language-server-extension-guide)
- [LSP Specification 3.17](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)
- [Neovim RPC and Channels](https://neovim.io/doc/user/channel.html)
- [Zed: Life of a Zed Extension](https://zed.dev/blog/zed-decoded-extensions)
- [Zed: Extensions System](https://deepwiki.com/zed-industries/zed/13-extensions-system)
- [Supervisord: Subprocesses](https://supervisord.org/subprocess.html)

## Cross-references inside the repo

- Overall workflow-runtime vision: [workflow-runtime.md](./workflow-runtime.md)
- Existing plugin system (UI panels + shell commands): [plugins.md](./plugins.md)
- Numbered architectural decisions: [decisions.md](./decisions.md)
- Phase checklist: [roadmap.md](./roadmap.md)
- copad-core module reference: [core-lib.md](./core-lib.md)
