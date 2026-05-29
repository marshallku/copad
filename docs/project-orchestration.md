# Project Orchestration — Phase 22.2 / 22.4 / 22.5 / 22.6 / 22.7

> Status: design. Phases 22.2–22.7 implement the surfaces sketched here. Decision: [#48](./decisions.md#48-copad-native-port-of-project-orchestration-spine).

The user runs both copad and `~/dev/life-assistant` today; the Go server hosts the mission / goal / agent / workflow / approval surface and the React SPA renders it. Phase 22 commits to porting that surface into copad as a native Rust + plugin stack, with **no runtime dependency** on the life-assistant server (decision [#48](./decisions.md#48-copad-native-port-of-project-orchestration-spine)). The Go server keeps the daily-life modules — Discord bot, Toss / Yahoo polling, KMA weather, news digest, Google OAuth + calendar polling, finance / portfolio / expense, investment loop — which need 24/7 uptime and external-API auth chains that don't belong on a workstation tool.

## 1. Substrate map — what already exists in `copad-core`

The key insight from the design discussion: copad-core already provides ~80% of the infrastructure life-assistant's project-orchestration spine needs. The port is mostly **data model + filesystem layout + glue actions + UI panel**, not "rebuild a Go server in Rust."

| life-assistant surface | copad-core / copadd primitive | Coverage |
|---|---|---|
| `runledger` (monthly JSONL append, 5-min dedup, EventFilter reader) | `EventBus` + ring buffer + `coctl recent` | In-memory present; durable JSONL persistence added in 22.6 |
| `dashboard` HTTP CommandService (Submit with write-ahead order + idempotency) | `copadd` socket dispatch + `ActionRegistry` | Single-writer daemon → no flock complexity needed |
| `missionsched.Scheduler` (30s tick: wake checks, approval expiry) | `TriggerEngine` cron triggers (Phase 21 Step 11) + event-kind subscriptions | Cron slot may need to land first; temporary timer fallback in 22.4 |
| `approval` action gates | `[security] accept_external` / `allow_privileged` + `register_privileged` | Approval-required variant added in 22.6 |
| `brain` Claude/Codex subprocess pool | `claude.start` (Phase 18.1) + `system.spawn` + tmux glue | Generalized for codex + model routing in 22.7 |
| `ProjectResolver` (name/alias → path) | `ContextService.pane_context.git_remote` + new `ProjectRegistry` | Added in 22.2 |
| Actor attribution (Discord user / web operator / system scheduler) | `event_bus::Origin` enum + decision #37 | Already wired |
| Mission / goal / approval flock + atomic-write | `copadd` is single writer; sequential socket dispatch serializes | Free — no port needed |
| Plugin / subprocess supervision (Brain retry, plugin lifecycle) | `ServiceSupervisor` + retry policy | Already wired |

What does **not** map (and stays Go-server-bound, by design): Discord WebSocket listener, slash-command dispatch, Toss Invest Playwright keepalive, Yahoo Finance scrape, KMA weather, news feed parsing, Apple Health webhook receiver, Google OAuth refresh chain, Discord channel routing for notifications.

## 2. Data model

Rust struct sketches. Field-level fidelity to life-assistant's Go structs where it makes sense; renames where it doesn't.

```rust
// copad-core::project
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,                  // canonical id
    pub path: PathBuf,                 // filesystem root
    pub subpath: Option<PathBuf>,      // working subdir inside path
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub git_remote: Option<String>,    // "owner/repo" — explicit override, else inferred at runtime
}

pub struct ProjectRegistry { /* loaded from config.toml [[projects]] */ }
impl ProjectRegistry {
    pub fn resolve_by_git_remote(&self, owner_repo: &str) -> Option<&Project>;
    pub fn resolve_by_name(&self, name_or_alias: &str) -> Option<&Project>;
    pub fn resolve_by_cwd(&self, cwd: &Path) -> Option<&Project>;
}

// copad-core::workflow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSpec {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub require_project: bool,
    #[serde(default)]
    pub form_fields: Vec<FormField>,
    pub default_team: Option<String>,
    pub default_model: Option<String>,
    pub prompt: String,               // template with {field_name} placeholders
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormField {
    pub name: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: FieldKind,              // text | textarea | select | checkbox | url
    pub placeholder: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub options: Vec<String>,         // for kind=select
}

// copad-core::goal
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub status: GoalStatus,           // running | paused | blocked | done | cancelled
    pub project: Option<String>,      // ProjectRegistry name; None = global
    pub created_at: DateTime<Utc>,
    pub last_tick_at: Option<DateTime<Utc>>,
    pub last_tick_result: Option<String>,
    pub blocked_question: Option<String>,
    pub blocked_answer: Option<String>,
    #[serde(default)]
    pub no_progress_count: u32,       // 3-strike → auto-blocked
    #[serde(default)]
    pub history: Vec<TickRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickRecord {
    pub at: DateTime<Utc>,
    pub outcome: TickOutcome,         // progress | ask_player | self_schedule | complete | no_progress
    pub detail: String,
    pub run_id: String,
}

// copad-core::agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: String,
    pub profile_md: String,           // markdown persona description
    pub autonomy: AutonomyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyConfig {
    pub default_model: String,
    pub allowed_actions: Vec<String>,
    pub forbidden_actions: Vec<String>,
    pub max_autonomy_level: AutonomyLevel,  // observe | suggest | act_with_approval | act_freely
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub kind: MemoryKind,             // observation | instruction | summary
    pub at: DateTime<Utc>,
    pub mission_id: Option<String>,
    pub content: String,
}

// copad-core::mission
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    pub id: String,
    pub title: String,
    pub objective: String,
    pub project: String,              // ProjectRegistry name
    #[serde(default)]
    pub assigned_agents: Vec<AgentAssignment>,
    pub state: MissionState,          // active | blocked_on_user | blocked_on_agent | completed | aborted
    pub urgency: Urgency,             // urgent | normal | backburner
    pub cadence: Cadence,
    pub budget: Budget,
    pub wake_conditions: Vec<WakeCondition>,
    #[serde(default)]
    pub paused: bool,
    pub created_at: DateTime<Utc>,
    pub created_by: String,           // actor string
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAssignment {
    pub agent_id: String,
    pub role: String,                 // "implementer", "reviewer", "orchestrator", etc.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub max_wakes_per_day: u32,
    pub max_cost_per_day_usd: f64,
    #[serde(default)]
    pub daily_wakes_consumed: u32,
    #[serde(default)]
    pub daily_cost_consumed_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum WakeCondition {
    OnTime { cron: String },                                  // cron expression
    OnEvent { kind: String, payload_match: serde_json::Value },// arbitrary trigger payload match
}

// copad-core::approval
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub mission_id: Option<String>,
    pub agent_id: Option<String>,
    pub action: String,               // e.g. "system.spawn"
    pub params_preview: serde_json::Value,
    pub rationale: String,
    pub state: ApprovalState,         // pending | granted | denied | expired
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by: Option<String>,
}

// copad-core::pipeline (22.7)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub name: String,
    pub roles: Vec<String>,           // role names referenced in stages
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    pub name: String,
    #[serde(default)]
    pub parallel: bool,
    pub roles: Vec<RoleAssignment>,
    pub output: Option<PathBuf>,      // relative to mission workspace
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleAssignment {
    pub role: String,
    pub model: Option<String>,        // overrides Role.model
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub model: String,                // "opus" | "sonnet" | "haiku" | "codex-*"
    pub prompt_template: String,
    #[serde(default)]
    pub tools: Vec<String>,
}
```

## 3. Filesystem layout

Two roots: config (read-mostly, user-editable) and state (write-heavy, daemon-managed). XDG-respecting; macOS variants use `~/Library/Application Support/copad/` and `~/Library/Caches/copad/` per existing `copad_core::paths` patterns.

```
~/.config/copad/                       # config (user-editable, version-controlled friendly)
├── config.toml                        # gains [[projects]] blocks (22.2)
├── workflows/                         # WorkflowSpec YAML, one file per spec (22.2)
│   ├── ship.yaml
│   ├── cross-review.yaml
│   ├── debug.yaml
│   └── ...
└── pipeline/                          # team / role definitions (22.7)
    ├── teams/
    │   ├── fullstack.yaml
    │   └── ...
    └── roles/
        ├── architect.yaml
        ├── api-dev.yaml
        └── ...

~/.local/state/copad/                  # state (daemon-managed, machine-local)
├── goals/<goal-id>/                   # 22.4
│   ├── state.json
│   └── roadmap.md
├── agents/<agent-id>/                 # 22.5 — user-overridable; 22.5 ships seeds via include_str!
│   ├── profile.md
│   ├── autonomy.yaml
│   ├── memory.jsonl                   # append-only
│   └── summary.md                     # rolling, regenerated
├── missions/<mission-id>/             # 22.5
│   ├── manifest.yaml
│   ├── timeline.md                    # turn-by-turn append
│   └── workspace/                     # mission-scoped scratch (artifacts, intermediate outputs)
├── approvals/<approval-id>.yaml       # 22.6
└── runledger/                         # 22.6 — durable event log
    └── <YYYY-MM>.jsonl                # monthly rotation
```

Notes:
- `<id>` slugs are short ULID-flavor (`m-`, `g-`, `a-`, `appr-` prefixes) — short enough for CLI use, sortable, collision-resistant.
- Mission `workspace/` is path-contained; mission actions enforce that file references stay inside it (mirrors life-assistant's artifact containment).
- Existing `copad_core::paths::state_dir()` returns the platform-appropriate root; subdirs land underneath it.

## 4. Action surface

All actions register through `ActionRegistry`. Sync where the call is cheap; `register_blocking` for anything that hits a subprocess or large file read.

**Project (22.2):**
- `project.list` → `{ projects: [Project, ...] }`
- `project.resolve { git_remote? | cwd? | name? }` → `{ project: Project | null }`

**Workflow (22.2):**
- `workflow.list` → `{ workflows: [WorkflowSpec, ...] }`
- `workflow.get { id }` → `WorkflowSpec`
- `workflow.run { id, project?, values: {<field_name>: <value>, ...} }` → `{ run_id, tab_id }`
  - Internally: validate form fields, resolve project (require_project), substitute prompt template, `tabs.new`, `claude.start { workspace_path, initial_input, model }`. If `default_team` set and Pipeline (22.7) shipped, route through pipeline runner.

**Goal (22.4):**
- `goal.create { title, project?, roadmap? }` → `Goal`
- `goal.list { project?, status? }` → `{ goals: [Goal, ...] }`
- `goal.get { id }` → `Goal`
- `goal.pause { id }` / `goal.resume { id }` / `goal.cancel { id }`
- `goal.answer { id, answer }` — unblocks; status returns to `running` and `blocked_answer` set
- `goal.update_roadmap { id, roadmap }` — overwrite roadmap.md (for ad-hoc edits)

**Agent (22.5):**
- `agent.list` → `{ agents: [AgentProfile, ...] }`
- `agent.get { id }` → `AgentProfile`
- `agent.show_memory { id, limit?, since? }` → `{ entries: [MemoryEntry, ...] }`
- `agent.append_memory { id, kind, content, mission_id? }` (internal; called by wake handler)

**Mission (22.5):**
- `mission.submit { title, objective, project, assigned_agents, wake_conditions, budget }` → `Mission`
- `mission.list { project?, state? }` → `{ missions: [Mission, ...] }`
- `mission.get { id }` → `Mission`
- `mission.pause { id }` / `mission.resume { id }`
- `mission.redirect_objective { id, new_objective }` — mid-flight pivot; appended to timeline
- `mission.assign_agent { id, agent_id, role }` / `mission.unassign_agent { id, agent_id }`
- `mission.abort { id, reason? }`

**Approval (22.6):**
- `approval.list { state?, mission? }` → `{ approvals: [Approval, ...] }`
- `approval.get { id }` → `Approval`
- `approval.grant { id, by, note? }` / `approval.deny { id, by, reason? }`

**Brain (22.7):**
- `codex.start { workspace_path, initial_input?, model?, session_name?, resume_session? }` — mirrors `claude.start` (Phase 18.1); spawns codex CLI.
- (Existing) `claude.start` gains `model` parameter; `model` matching `"codex-*"` routes to `codex.start` internally — caller speaks one action, dispatcher routes.

**Runledger (22.6):**
- `events.replay { since: ts | duration, kinds?: [String], limit?: u32 }` → streamed event records from monthly JSONL files
- `coctl runledger query --since 1h --kind mission.*` — CLI front

## 5. Brain dispatcher generalization (22.7)

`claude.start` is the canonical workflow runner today. The generalization lets a single action surface drive both Claude and Codex with model-aware routing:

```rust
// claude.start handler (22.7 update)
fn handle_claude_start(req: ClaudeStartReq) -> Result<ClaudeStartResp> {
    let model = req.model.as_deref().unwrap_or("opus");
    if model.starts_with("codex-") || codex_models().contains(model) {
        return dispatch_codex(req);   // forwards to codex.start handler
    }
    spawn_claude_cli(req)
}
```

Pipeline runner (also 22.7) sits on top: for a workflow with `default_team`, the runner walks `Team.stages`, materializes per-role prompts (template + previous stage outputs), and dispatches `claude.start` for each `RoleAssignment` — same action, model-routed.

Stage outputs are written to files under the mission/run workspace; subsequent stages reference them via `{stage_outputs.architect}` placeholders in role prompt templates. Parallel stages fan out via existing async dispatch; sequential stages chain on completion events.

> **Dispatch body — the deferred autonomous loop ([decision #49](./decisions.md#49-agent-session-dispatch-via-a-standalone-csd-cli--subscription-seat-driver-consumed-by-copad)).** The spine above (and 22.4/22.5/22.6) stores goals/missions but never autonomously *runs* an agent turn — every slice deferred dispatch. That body is a **standalone `csd` CLI** (claude/codex session driver, `~/dev/csd`), **not** copad-core code: it spawns an interactive agent in detached `tmux` (subscription-seat billing — the only flat-rate path for a heavy user; `claude -p` is metered from 2026-06-15) and exposes a hybrid state detector (JSONL + capture-pane + plan file) as JSON. copad **consumes** `csd` (the `tmx agents --json` pattern): the panel reads `csd ps --json` for visibility; the daemon-side loop shells out to `csd` to dispatch turns. `csd` is one pluggable backend alongside the GUI `claude.start` (interactive tab) and a possible metered `-p` backend. The PoC (claude v2.1.157) verified both clarifying-question detection and plan-mode approval→execute over `tmux`. Implementation details live in the `csd` repo, not here. See #49 for the billing research, the flat-rate-vs-structured tradeoff, and the explicit non-goals.

## 6. Wake conditions ↔ TriggerEngine mapping

life-assistant's mission `wake_conditions` map to copad TriggerEngine entries with one-to-one semantics:

| WakeCondition variant | Trigger registration |
|---|---|
| `OnTime { cron }` | `TriggerEngine::register_cron(cron, kind="mission.wake", payload={mission_id})` (depends on Phase 21 Step 11 cron triggers) |
| `OnEvent { kind, payload_match }` | `TriggerEngine::register_kind(kind, payload_match, action="mission.wake_handler", payload={mission_id})` |

The wake handler is a single action (`mission.wake_handler`) registered by `copad-core::mission`. It receives `{mission_id}`, loads the manifest, selects an orchestrator agent, builds the prompt from objective + assigned_agents + recent timeline events + memory summary, and dispatches `claude.start` (model routed per agent's `autonomy.default_model`). The turn JSON response (next_action, detail, target_agent?) writes to `timeline.md` and may emit `mission.state_changed`.

Approval expiry is also TriggerEngine-driven: `register_cron("*/30 * * * * *", "approval.expire_sweep")` — handler scans `~/.local/state/copad/approvals/`, transitions pending+expired entries, emits `approval.expired`.

Goal ticks use the same primitive: `register_cron("*/1 * * * *", "goal.tick_next_runnable")` — handler picks `next_runnable()` and dispatches one claude turn.

## 7. Runledger JSONL persistence (22.6)

EventBus today is in-memory only (ring buffer for `coctl recent`). 22.6 adds durable persistence:

- New write-through subscriber registered at daemon startup: subscribes to `Pattern::All`, appends each event as one JSON line to `~/.local/state/copad/runledger/<YYYY-MM>.jsonl`.
- Each line: `{ event_id, ts, kind, origin, payload }`. event_id is ULID (sortable). origin from existing `Origin` field.
- Monthly rotation: handler opens current month's file on first event, switches on month boundary.
- Read API: `events.replay` streams matching events from one or more monthly files.
- Existing ring buffer untouched — durable layer is additive.
- 5-min dedup (life-assistant has it) **not** ported initially — copad daemon is single-writer so duplicate emits are an upstream bug, not a runtime concern. Revisit if cross-instance reconciliation ever becomes a thing.

## 8. Project entity + git_remote resolution

The `[[projects]]` block in `~/.config/copad/config.toml`:

```toml
[[projects]]
name = "copad"
path = "/home/marshall/dev/copad"
git_remote = "marshallku/copad"   # explicit; else inferred at first resolve from `git remote get-url origin`
aliases = ["copad-app"]

[[projects]]
name = "life-assistant"
path = "/home/marshall/dev/life-assistant"
git_remote = "marshallku/life-assistant"
```

Resolution order at panel rendering / workflow dispatch:

1. Active pane's `pane_context.git_remote` (from Phase 22.1) → `ProjectRegistry::resolve_by_git_remote()`
2. Active pane's `pane_context.cwd` → `resolve_by_cwd()` (walks up `path` ancestor chain)
3. Explicit `project` parameter on the action call
4. None — degraded mode; panel shows project picker, actions reject `require_project` workflows

If a project is defined but its `git_remote` is `None`, the daemon shells `git -C <path> remote get-url origin 2>/dev/null | sed -nE 's#.*[:/]([^/]+/[^/.]+)(\.git)?$#\1#p'` once at startup and caches the result. Failure is fine (`git_remote` stays None; resolution falls back to cwd).

## 9. Per-slice acceptance (mirrors roadmap checkboxes — kept here for design-doc completeness)

### 22.2 Project + Workflow MVP

Functional gates:
- `coctl project list` returns configured projects, including `git_remote` (cached or inferred).
- `coctl workflow run --id ship --project copad --values '{"branch":"feat/x"}'` opens a new copad tab, runs `claude.start` in the project's path with the substituted prompt.
- `copad-plugin-projects` panel auto-resolves project on pane focus change (via `pane.context_changed` subscription) without manual reload.
- Workflow specs migrated from life-assistant repo run without modification (form_field schema is wire-compatible).

Non-goals for 22.2:
- Goals / missions / agents / approvals panels (later slices)
- `default_team` actual routing (lands in 22.7; ignored in 22.2 specs)

### 22.4 Goal driver

Functional gates:
- Creating a goal via `goal.create` writes `state.json` + `roadmap.md` and the 1-min tick picks it up on next firing.
- Tick result parsing handles all five `next_action` variants without panicking on schema variance.
- 3-strike no_progress auto-transitions to blocked with `blocked_question = "No progress detected after 3 consecutive ticks"`.
- `goal.answer` unblocks and resets `no_progress_count`.

Cron dependency: if Phase 21 Step 11 cron-triggers isn't shipped, 22.4 includes a temporary `tokio::time::interval`-driven scheduler in `copadd` and removes it when cron lands.

### 22.5 Agent + Mission

Functional gates:
- 5 seed agent profiles ship via `include_str!`; copying one to `~/.local/state/copad/agents/<id>/` overrides at startup.
- `mission.submit` creates manifest + registers wake_conditions with TriggerEngine.
- Wake handler dispatches `claude.start` with full context (objective + assigned_agents + recent memory) and persists turn output to `timeline.md`.
- `mission.pause` deregisters wake triggers; `mission.resume` re-registers.
- Memory append uses `O_APPEND` single-syscall write (same pattern as `copad-plugin-kb`'s `kb.append`).

### 22.6 Approval + Runledger

Functional gates:
- A privileged action call without approval creates a pending Approval, emits `approval.requested`, and returns `ApprovalPending` error to the caller.
- `approval.grant` transitions to granted and emits `approval.granted`. Original caller (e.g., mission wake handler) retries on event.
- Expiry sweep transitions pending+expired to `expired` exactly once.
- Monthly JSONL gets one event per line, valid JSON; `events.replay --since 1h` returns matches.
- Rotation across month boundary preserves both files; replay across the boundary stitches them.

### 22.7 Pipeline + Brain dispatcher

Functional gates:
- Team YAML loads at startup; project-local override (`<project>/.copad/teams/<name>.yaml`) takes precedence over user config takes precedence over builtin.
- Workflow with `default_team` set routes through pipeline runner; per-stage outputs flow as `{stage_outputs.<stage>}` into subsequent role prompts.
- `claude.start { model: "codex-medium" }` dispatches via codex CLI; existing claude variants unchanged.
- Parallel stages fan out concurrently and rendezvous on stage completion.

## 10. What stays on the life-assistant Go server (permanently)

Per decision [#48](./decisions.md#48-copad-native-port-of-project-orchestration-spine), the workstation-vs-server split is hard. The following modules never get ported:

- **Discord bot listener + slash commands** (`internal/bot/`) — WebSocket connection requires 24/7 uptime; copad is closed at end-of-day.
- **Toss Invest API + Playwright keepalive** (`internal/tossauth/`, `internal/tossinvest/`, `internal/trading/`) — push-approval auth flow requires background session refresh; unofficial API drift requires maintenance attention that doesn't belong on a workstation tool.
- **Yahoo Finance scrape** (`internal/finance/`) — geoblock + user-agent rotation; quota burst protection only makes sense single-instance.
- **KMA weather** (`internal/weather/`) — Korean weather scrape; daily cron.
- **News digest** (`internal/newsdigest/`) — RSS feed polling + LLM-summarize pipeline; cron-driven.
- **Google OAuth refresh + calendar polling** (`internal/google/`) — long-lived auth chain better managed by one server.
- **Finance / portfolio / expense / investment loop** (`internal/finance/`, `internal/portfolio/`, `internal/expense/`, `internal/investment/`, plus the 5x-daily investment loop plugins) — cross-API orchestration with rate-limit budgets that need single-instance ownership.
- **Apple Health webhook receiver** (part of `internal/api/`) — HTTP listener for iOS Shortcuts pushes; LAN-only but always-on.
- **Cron infrastructure backing the above** (`internal/scheduler/`) — `robfig/cron` driven, hot-reloaded `jobs.json`, builtin morning summary. Workstation cron triggers (Phase 21 Step 11) cover copad's own tick needs; the Go scheduler keeps these external-API-bound jobs.

Phase 21 step 12's `lifeassistant` event-publisher plugin idea coexists: it's a one-way push from the Go server to copad's bus (e.g., `lifeassistant.job_completed` when a server-side cron finishes), consumed by copad triggers. **One-way push does not constitute a runtime dependency** — copad keeps working when the server is down; the bridge just goes quiet for server-originated events. Decision #48 explicitly preserves this track.

## Related docs

- [decisions.md #48](./decisions.md#48-copad-native-port-of-project-orchestration-spine) — the commit-to-port decision
- [decisions.md #47](./decisions.md#47-life-assistant-absorption-stance--selective-native-reimplementation-not-embedding-phase-223) — the superseded "selective absorption" stance, kept for traceability
- [roadmap.md § Phase 22](./roadmap.md#phase-22-context-aware-workstation-hub) — slice checklists
- [kb-panel.md](./kb-panel.md) — orthogonal Phase 22.3 track
- [context-bridge.md](./context-bridge.md) — Phase 22.1 (the `pane_context.git_remote` signal this design hinges on)
- [workflow-runtime.md](./workflow-runtime.md) — EventBus / ActionRegistry / TriggerEngine / ContextService primitives
- [harness-integration.md § step 11/12](./harness-integration.md) — cron triggers + the `lifeassistant` event-publisher bridge
- [decisions.md #37](./decisions.md) — Origin + `[security]` model (load-bearing for approval gate trust)
- [decisions.md #38](./decisions.md) — `notify.show` action (used by trigger-driven user notifications)
