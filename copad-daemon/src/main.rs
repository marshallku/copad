//! `copadd` binary entry. Hosts the daemon-side `ActionRegistry`
//! (builtins + plugins via `ServiceSupervisor`) and the GUI registry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use copad_core::action_registry::{ActionRegistry, internal_error, invalid_params};
use copad_core::config::CopadConfig;
use copad_core::context::{ContextService, Presence};
use copad_core::paths;
use copad_core::plugin::LoadedPlugin;
use copad_core::protocol::ResponseError;
use copad_core::thread_pool::ThreadPool;
use copad_core::trigger::{Trigger, TriggerEngine, TriggerSink};
use copad_daemon::daemon_trigger_sink::DaemonTriggerSink;
use copad_daemon::gui_registry::GuiRegistry;
use copad_daemon::plugin_exec::{ShellError, spawn_plugin_shell};
use copad_daemon::service_supervisor::ServiceSupervisor;
use copad_daemon::socket::{self, DaemonState, LEGACY_DISPATCH_METHODS, SocketPrep, new_event_bus};
use copad_daemon::trigger_pump::PumpState;
use copad_daemon::trigger_sink::TRIGGER_ONLY_RESERVED_METHODS;
use serde_json::json;

/// `plugin.<name>.<cmd>` inherits the supervisor's 120s action_timeout;
/// the inner timeout is below that so the watchdog's kill+reap path
/// always wins the race over the registry's outer 120s recv_timeout.
const PLUGIN_CMD_TIMEOUT: Duration = Duration::from_secs(90);

/// Statusbar modules tick at 10s default. Generous-but-bounded so a
/// runaway module can't pile up across ticks.
const MODULE_RUN_TIMEOUT: Duration = Duration::from_secs(8);

const ENV_E2E_ACTIONS: &str = "COPADD_E2E_TEST_ACTIONS";
const ENV_POOL_WORKERS: &str = "COPADD_POOL_WORKERS";
const ENV_POOL_QUEUE: &str = "COPADD_POOL_QUEUE";
/// Daemon-side TriggerEngine dispatch. Stages B+C delivered the
/// atomic cut-over so a registered GUI gracefully releases its own
/// in-process engine when the daemon advertises `host_triggers=true`
/// in the `gui.register` ack — no risk of double-dispatch on the
/// shared trigger set. Default flipped to ON when slice 1 of the
/// `claude` harness loop landed (decisions.md #39): with no
/// `COPADD_HOST_TRIGGERS` set, the daemon dispatches; set it to
/// `0`/`false`/`no` to disable and let the GUI engine stay
/// authoritative (only useful for the standalone-GUI path).
const ENV_HOST_TRIGGERS: &str = "COPADD_HOST_TRIGGERS";

const PUMP_TICK: Duration = Duration::from_millis(50);

/// Daemon config file mtime poll interval. Two seconds is a fair
/// trade-off: faster than user perception for trigger reloads, but
/// slow enough that we don't churn the syscall table when nobody is
/// editing.
const WATCHER_TICK: Duration = Duration::from_secs(2);

fn print_help() {
    println!(
        "copadd {version} — copad daemon

USAGE:
    copadd [OPTIONS]

OPTIONS:
    -h, --help       Show this help and exit
    -V, --version    Show version and exit

ENVIRONMENT:
    COPAD_SOCKET           Override the daemon socket path
    COPAD_HOST_TRIGGERS    Enable / disable trigger pump (default on; off|0|false to disable)
    COPAD_E2E_ACTIONS      Register end-to-end test actions (1|true to enable)",
        version = env!("CARGO_PKG_VERSION"),
    );
}

fn main() -> ExitCode {
    // Short-circuit read-only flags BEFORE env_logger init / socket bind.
    // Without this, `copadd --version` invoked while a daemon is running
    // would fail with "socket already bound" from `prepare_socket_path`
    // (decisions.md #40 notes auto-start happens behind the user's back —
    // a second invocation for `--version` is the normal way to check it).
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("copadd {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return ExitCode::SUCCESS;
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let socket_path: PathBuf = paths::socket_path();
    log::info!("copadd starting; socket={}", socket_path.display());

    match socket::prepare_socket_path(&socket_path) {
        SocketPrep::Fresh => log::debug!("socket path fresh"),
        SocketPrep::StaleCleared => log::info!("removed stale socket file"),
        SocketPrep::InUse => {
            log::error!(
                "socket {} already bound by another copadd; refusing to start",
                socket_path.display()
            );
            return ExitCode::from(2);
        }
        SocketPrep::Error(msg) => {
            log::error!("socket prep failed: {msg}");
            return ExitCode::from(1);
        }
        SocketPrep::NotSocket => {
            log::error!(
                "path {} exists but is not a Unix socket; refusing to unlink (set COPAD_SOCKET to a fresh path)",
                socket_path.display()
            );
            return ExitCode::from(3);
        }
    }

    let event_bus = new_event_bus();
    let pool = build_pool();
    let actions =
        Arc::new(ActionRegistry::with_completion_bus(event_bus.clone())).with_pool(pool.clone());
    let plugins = discover_and_sort_plugins();
    let host_triggers = env_flag_default_on(std::env::var(ENV_HOST_TRIGGERS).ok().as_deref());
    register_builtins(&actions, &plugins, host_triggers);
    register_plugin_commands(&actions, &plugins, &socket_path);
    if env_flag_enabled(ENV_E2E_ACTIONS) {
        register_e2e_actions(&actions);
    }
    // Phase 22.4 — goal registry + tick scheduler. Registry is shared
    // between action handlers (CRUD) and the scheduler thread (read +
    // publish tick.started).
    let (goal_registry, goal_tick_stop) = register_goal_actions(&actions, &event_bus);
    let _goal_tick_thread = spawn_goal_tick_thread(
        goal_registry.clone(),
        event_bus.clone(),
        goal_tick_stop.clone(),
    );
    let _ = goal_registry;
    let _ = goal_tick_stop;
    // Phase 22.5 — agent + mission registries. Both are daemon-owned
    // (CRUD-only at v1; wake-condition auto-firing lands in 22.7).
    let _agent_registry = register_agent_actions(&actions);
    let _mission_registry = register_mission_actions(&actions, &event_bus);
    // Phase 22.6 — approval registry + 30s expiry sweeper.
    let (approval_registry, approval_sweep_stop) = register_approval_actions(&actions, &event_bus);
    let _approval_sweep_thread = spawn_approval_sweeper(
        approval_registry.clone(),
        event_bus.clone(),
        approval_sweep_stop.clone(),
    );
    let _ = approval_registry;
    let _ = approval_sweep_stop;
    // Phase 22.6 — runledger write-through subscriber + events.replay action.
    let runledger_stop = Arc::new(AtomicBool::new(false));
    let _runledger_thread =
        register_runledger_actions(&actions, &event_bus, runledger_stop.clone());
    let _ = runledger_stop;

    // GuiRegistry is built before the trigger sink so both share the
    // same registry instance — the sink's fallthrough worker resolves
    // a registered primary GUI via `gui.route(action, None)`.
    let gui = GuiRegistry::new();
    let context = Arc::new(ContextService::new());
    let (triggers_cfg, initial_mtime) = load_triggers_config();
    let cached_triggers = Arc::new(Mutex::new(triggers_cfg.clone()));
    let engine = build_trigger_engine(&actions, &gui, &context, &event_bus, &triggers_cfg);
    // PumpState — and the bus subscriptions it owns — only exists when
    // the daemon is dispatch-authoritative. With host_triggers=false
    // the engine holds the trigger set internally but no receivers are
    // created, so daemon bus traffic does not accumulate.
    let pump_state: Option<Arc<Mutex<PumpState>>> = if host_triggers {
        Some(build_pump_state(&event_bus, &triggers_cfg))
    } else {
        None
    };
    log::info!(
        "trigger engine: {} configured | {} bus pattern(s) | dispatch={}",
        triggers_cfg.len(),
        pump_state
            .as_ref()
            .map(|p| p.lock().unwrap().trigger_subs_len())
            .unwrap_or(0),
        if host_triggers { "ON" } else { "OFF" }
    );

    // Bind before activating plugins so a bind failure can't orphan
    // eagerly-spawned children.
    let listener = match socket::bind_listener(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            log::error!("bind({}): {e}", socket_path.display());
            return ExitCode::from(1);
        }
    };

    let supervisor_guard: Arc<ServiceSupervisor> =
        activate_supervisor(&actions, &event_bus, &plugins, socket_path.clone());

    let pump_stop = Arc::new(AtomicBool::new(false));
    let pump_thread = pump_state.as_ref().map(|p| {
        spawn_pump_thread(
            p.clone(),
            engine.clone(),
            context.clone(),
            pump_stop.clone(),
        )
    });

    // Config-file watcher runs unconditionally — daemon's own engine
    // tracks `[[triggers]]` edits even with no GUI attached (headless
    // case). When `host_triggers=false`, the watcher updates engine
    // state without touching bus subscriptions (no PumpState exists);
    // when `=true`, it follows the GUI's hot-reload ordering
    // (set_triggers → pump_all → reconcile). `initial_mtime` was
    // captured during `load_triggers_config()` so an edit landing in
    // the window between main()'s load and the watcher's first tick
    // is detected on that first tick.
    let watcher_stop = Arc::new(AtomicBool::new(false));
    let watcher_thread = spawn_config_watcher(
        engine.clone(),
        pump_state.clone(),
        context.clone(),
        event_bus.clone(),
        cached_triggers.clone(),
        initial_mtime,
        watcher_stop.clone(),
    );

    let state = DaemonState::new(
        actions,
        gui,
        event_bus.clone(),
        plugins,
        socket_path.clone(),
        host_triggers,
    );

    log::info!("copadd listening on {}", socket_path.display());
    socket::run_accept_loop(listener, state);

    pump_stop.store(true, Ordering::SeqCst);
    watcher_stop.store(true, Ordering::SeqCst);
    if let Some(handle) = pump_thread
        && let Err(panic) = handle.join()
    {
        log::error!("trigger pump thread panicked: {panic:?}");
    }
    if let Err(panic) = watcher_thread.join() {
        log::error!("config watcher thread panicked: {panic:?}");
    }

    // Arc::drop does not call shutdown_all; we must invoke it explicitly
    // for cooperative plugin shutdown before unlinking the socket.
    log::info!("shutting down supervised plugins");
    supervisor_guard.shutdown_all();
    // Explicit pool shutdown breaks any registry↔handler↔supervisor Arc
    // cycle that would otherwise prevent the pool's Drop from running.
    pool.shutdown();

    socket::cleanup_socket(&socket_path);
    log::info!("copadd shut down");
    ExitCode::SUCCESS
}

/// Returns the loaded triggers AND the mtime sampled at load time.
/// The watcher seeds its baseline from this mtime so an edit landing
/// between main()'s load and the watcher's first tick is detected on
/// that first tick rather than ignored until the next edit.
fn load_triggers_config() -> (Vec<Trigger>, Option<std::time::SystemTime>) {
    let path = CopadConfig::config_path();
    let mtime = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());
    match CopadConfig::load_from(&path) {
        Ok(cfg) => (cfg.triggers, mtime),
        Err(e) => {
            log::warn!("trigger config load failed: {e:?}; starting daemon with empty trigger set");
            (Vec::new(), mtime)
        }
    }
}

/// Build the engine + register `context.snapshot`. Does NOT create
/// the PumpState — that's deferred to host-triggers-on mode so we
/// don't accumulate unbounded trigger subscriptions with no pump
/// thread to drain them.
fn build_trigger_engine(
    actions: &Arc<ActionRegistry>,
    gui: &Arc<GuiRegistry>,
    context: &Arc<ContextService>,
    event_bus: &Arc<copad_core::event_bus::EventBus>,
    triggers_cfg: &[Trigger],
) -> Arc<TriggerEngine> {
    let sink: Arc<dyn TriggerSink> = Arc::new(DaemonTriggerSink::new(
        actions.clone(),
        gui.clone(),
        event_bus.clone(),
    ));
    let engine = Arc::new(TriggerEngine::with_publish_bus(sink, event_bus.clone()));
    engine.set_triggers(triggers_cfg.to_vec());
    let ctx_for_snapshot = context.clone();
    actions.register_silent("context.snapshot", move |_| {
        serde_json::to_value(ctx_for_snapshot.snapshot())
            .map_err(|e| internal_error(format!("context snapshot serialize: {e}")))
    });
    let ctx_for_presence_set = context.clone();
    let bus_for_presence = event_bus.clone();
    actions.register_silent("presence.set", move |params| {
        let state_str = params
            .get("state")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                invalid_params("presence.set requires `state` (\"active\" or \"away\")")
            })?;
        let new_presence = match state_str {
            "active" => Presence::Active,
            "away" => Presence::Away,
            other => {
                return Err(invalid_params(format!(
                    "presence.set `state` must be \"active\" or \"away\", got {other:?}"
                )));
            }
        };
        let prev = ctx_for_presence_set.set_presence(new_presence);
        if prev != new_presence {
            bus_for_presence.publish(copad_core::event_bus::Event::new(
                "presence.changed",
                "daemon",
                serde_json::json!({
                    "previous": prev.as_str(),
                    "current": new_presence.as_str(),
                }),
            ));
        }
        Ok(serde_json::json!({
            "previous": prev.as_str(),
            "current": new_presence.as_str(),
        }))
    });
    let ctx_for_presence_get = context.clone();
    actions.register_silent("presence.get", move |_params| {
        Ok(serde_json::Value::String(
            ctx_for_presence_get.presence().as_str().to_string(),
        ))
    });
    // `event.history` mirrors the GUI's registration. Both processes
    // host their own EventBus (bridge-forwarded events land on both),
    // so a daemon-routed `coctl recent` returns the daemon's view
    // and a GUI-routed call returns the GUI's; for plugin events the
    // two are largely interchangeable. Registered silent so its own
    // `.completed` doesn't inflate the next call's result.
    let bus_for_history = event_bus.clone();
    actions.register_silent("event.history", move |params| {
        if let Some(v) = params.get("since_ms")
            && !v.is_null()
            && v.as_u64().is_none()
        {
            return Err(invalid_params(
                "event.history `since_ms` must be a non-negative integer",
            ));
        }
        if let Some(v) = params.get("kind")
            && !v.is_null()
            && v.as_str().is_none()
        {
            return Err(invalid_params("event.history `kind` must be a string glob"));
        }
        let since_ms = params.get("since_ms").and_then(|v| v.as_u64());
        let kind = params
            .get("kind")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let events = bus_for_history.history(since_ms, kind.as_deref());
        let arr: Vec<serde_json::Value> = events
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "type": e.kind,
                    "data": e.payload,
                    "source": e.source,
                    "timestamp_ms": e.timestamp_ms,
                })
            })
            .collect();
        Ok(serde_json::json!({ "events": arr }))
    });
    register_notify_show(
        actions,
        copad_core::notifier::platform_notifier().map(Arc::from),
    );
    engine
}

/// `notify.show` — desktop toast. Registered as `blocking_silent` so
/// the ~10 ms `notify-send` subprocess runs on the action thread pool
/// instead of stalling the trigger pump, and so its own `.completed`
/// event doesn't fan-out (the toast IS the user signal). The same
/// registration also runs on the GUI's in-process registry — see
/// `copad-linux/src/window.rs` — so triggers fire regardless of
/// whether the daemon hosts the engine or the GUI's `LiveTriggerSink`
/// path resolves the action. `notifier` is plumbed as an arg so tests
/// can inject a `NoopNotifier` without spawning real subprocesses.
fn register_notify_show(
    actions: &Arc<copad_core::action_registry::ActionRegistry>,
    notifier: Option<Arc<dyn copad_core::notifier::Notifier>>,
) {
    actions.register_blocking_silent("notify.show", move |params| {
        let title = match params.get("title").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return Err(invalid_params(
                    "notify.show requires non-empty `title` string",
                ));
            }
        };
        let body = match params.get("body") {
            Some(v) if v.is_null() => String::new(),
            None => String::new(),
            Some(v) => match v.as_str() {
                Some(s) => s.to_string(),
                None => {
                    return Err(invalid_params("notify.show `body` must be a string"));
                }
            },
        };
        let level: copad_core::notifier::Level = match params.get("level") {
            None | Some(serde_json::Value::Null) => copad_core::notifier::Level::default(),
            Some(v) => serde_json::from_value(v.clone()).map_err(|_| {
                invalid_params("notify.show `level` must be one of `info`, `warn`, `error`")
            })?,
        };
        match &notifier {
            Some(n) => match n.notify(&title, &body, level) {
                Ok(()) => Ok(serde_json::json!({ "shown": true })),
                Err(e) => {
                    log::warn!("notify.show failed: {e}");
                    Err(internal_error(format!("notify subprocess: {e}")))
                }
            },
            None => {
                // Platform has no concrete Notifier yet (only Linux and
                // macOS are wired). Drop the toast with a debug-level
                // log; downstream chains don't need to fail.
                log::debug!("notify.show: no Notifier for this platform; dropping");
                Ok(serde_json::json!({ "shown": false, "reason": "no_notifier" }))
            }
        }
    });
}

/// Build + reconcile PumpState. Only call this when the daemon is
/// host_triggers-enabled; the pump thread that drains the
/// subscriptions is spawned alongside.
fn build_pump_state(
    event_bus: &Arc<copad_core::event_bus::EventBus>,
    triggers_cfg: &[Trigger],
) -> Arc<Mutex<PumpState>> {
    let mut pump = PumpState::new(event_bus);
    pump.reconcile_triggers(event_bus, triggers_cfg);
    Arc::new(Mutex::new(pump))
}

fn spawn_pump_thread(
    pump: Arc<Mutex<PumpState>>,
    engine: Arc<TriggerEngine>,
    context: Arc<ContextService>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("copad-trigger-pump".into())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(PUMP_TICK);
                pump.lock().unwrap().pump_all(&context, &engine);
                engine.sweep_pending_awaits();
            }
        })
        .expect("spawn pump thread")
}

#[allow(clippy::too_many_arguments)]
fn spawn_config_watcher(
    engine: Arc<TriggerEngine>,
    pump_state: Option<Arc<Mutex<PumpState>>>,
    context: Arc<ContextService>,
    event_bus: Arc<copad_core::event_bus::EventBus>,
    cached_triggers: Arc<Mutex<Vec<Trigger>>>,
    initial_mtime: Option<std::time::SystemTime>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("copad-config-watcher".into())
        .spawn(move || {
            config_watcher_loop(
                engine,
                pump_state,
                context,
                event_bus,
                cached_triggers,
                initial_mtime,
                stop,
                &CopadConfig::config_path(),
            );
        })
        .expect("spawn config watcher thread")
}

#[allow(clippy::too_many_arguments)]
fn config_watcher_loop(
    engine: Arc<TriggerEngine>,
    pump_state: Option<Arc<Mutex<PumpState>>>,
    context: Arc<ContextService>,
    event_bus: Arc<copad_core::event_bus::EventBus>,
    cached_triggers: Arc<Mutex<Vec<Trigger>>>,
    initial_mtime: Option<std::time::SystemTime>,
    stop: Arc<AtomicBool>,
    path: &Path,
) {
    // Seed from the mtime sampled at the time of the initial config
    // load, NOT from a fresh sample on watcher startup. The latter
    // would silently swallow any edit that landed between main()'s
    // load and the watcher entering this function.
    let mut last_mtime = initial_mtime;
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(WATCHER_TICK);
        let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
        if mtime == last_mtime {
            continue;
        }
        last_mtime = mtime;
        match CopadConfig::load_from(path) {
            Ok(cfg) => apply_reloaded_triggers(
                &engine,
                pump_state.as_ref(),
                &context,
                &event_bus,
                &cached_triggers,
                cfg.triggers,
            ),
            Err(e) => log::warn!(
                "config watcher: parse error on reload: {e:?}; keeping previous trigger set"
            ),
        }
    }
}

/// Hot-reload contract:
/// - Always: `engine.set_triggers(new)` + refresh cached_triggers.
/// - When `pump_state` is `Some` (host_triggers=true): mirror the
///   GUI's `watch_config` ordering — `pump_all` on OLD subscribers
///   to flush pending events, then `reconcile_triggers`. Skipping
///   `pump_all` would discard pending events the new trigger set
///   would have matched during a pattern-narrowing reload.
/// - When `pump_state` is `None` (host_triggers=false): bus
///   subscriptions don't exist at all — nothing to reconcile and
///   nothing to flush. The engine's internal trigger list is the
///   only thing that updates.
fn apply_reloaded_triggers(
    engine: &Arc<TriggerEngine>,
    pump_state: Option<&Arc<Mutex<PumpState>>>,
    context: &Arc<ContextService>,
    event_bus: &Arc<copad_core::event_bus::EventBus>,
    cached_triggers: &Arc<Mutex<Vec<Trigger>>>,
    new_triggers: Vec<Trigger>,
) {
    engine.set_triggers(new_triggers.clone());
    if let Some(ps) = pump_state {
        let mut ps = ps.lock().unwrap();
        ps.pump_all(context, engine);
        ps.reconcile_triggers(event_bus, &new_triggers);
    }
    *cached_triggers.lock().unwrap() = new_triggers;
    log::info!(
        "trigger config reloaded ({} triggers)",
        cached_triggers.lock().unwrap().len()
    );
}

fn build_pool() -> Arc<ThreadPool> {
    let default_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_mul(2))
        .unwrap_or(8)
        .clamp(4, 16);
    // Clamp env overrides to a sane band so a typo (e.g. `WORKERS=10000`)
    // can't OS-exhaust the daemon on startup.
    let workers = env_usize(ENV_POOL_WORKERS)
        .unwrap_or(default_workers)
        .clamp(1, 256);
    let queue_cap = env_usize(ENV_POOL_QUEUE).unwrap_or(64).clamp(1, 4096);
    log::info!("action pool: workers={workers} queue_cap={queue_cap}");
    ThreadPool::new(workers, queue_cap)
}

fn env_usize(var: &str) -> Option<usize> {
    let raw = std::env::var(var).ok()?;
    match raw.trim().parse::<usize>() {
        Ok(0) => {
            log::warn!("ignoring {var}={raw} (must be >= 1)");
            None
        }
        Ok(n) => Some(n),
        Err(e) => {
            log::warn!("ignoring {var}={raw}: parse error: {e}");
            None
        }
    }
}

/// Test-only actions enabled via `COPADD_E2E_TEST_ACTIONS=1`. Keep these
/// gated so they never appear in normal daemon runs.
/// Phase 22.4 — Goal driver actions + tick scheduler.
///
/// Registers `goal.create / list / get / pause / resume / answer /
/// cancel` against the daemon's GoalRegistry. Spawns a background
/// thread that ticks every 60s: picks the next runnable goal,
/// publishes `goal.tick.started {goal_id, prompt}` so the GUI can
/// dispatch `claude.start`, and waits for `goal.tick.completed
/// {goal_id, raw_output}` to apply the parsed result.
///
/// Storage: `<state_dir>/goals/<id>/state.json` + `roadmap.md`.
fn register_goal_actions(
    actions: &Arc<ActionRegistry>,
    bus: &Arc<copad_core::event_bus::EventBus>,
) -> (Arc<copad_core::goal::GoalRegistry>, Arc<AtomicBool>) {
    use copad_core::event_bus::Event as BusEvent;
    use copad_core::goal::{GoalRegistry, GoalStatus, parse_tick_output};

    let goals_root = copad_core::paths::state_dir().join("goals");
    let goals = Arc::new(GoalRegistry::new(goals_root));

    let goals_for_create = goals.clone();
    let bus_for_create = bus.clone();
    actions.register_blocking("goal.create", move |params| {
        let title = params
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("goal.create requires non-empty 'title' string"))?
            .to_string();
        let project = params
            .get("project")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("goal.create requires non-empty 'project' string"))?
            .to_string();
        let project_path = params
            .get("project_path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("goal.create requires non-empty 'project_path' string"))?
            .to_string();
        let roadmap_template = params
            .get("roadmap")
            .and_then(|v| v.as_str())
            .map(String::from);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let goal = goals_for_create
            .create(
                &title,
                &project,
                &project_path,
                roadmap_template.as_deref(),
                now_ms,
            )
            .map_err(internal_error)?;
        bus_for_create.publish(BusEvent::new(
            "goal.created",
            "copad-daemon",
            json!({"id": goal.id, "title": goal.title, "project": goal.project}),
        ));
        serde_json::to_value(&goal).map_err(|e| internal_error(format!("serialize goal: {e}")))
    });

    let goals_for_list = goals.clone();
    actions.register_silent("goal.list", move |params| {
        let project = params
            .get("project")
            .and_then(|v| v.as_str())
            .map(String::from);
        let status: Option<GoalStatus> = match params.get("status") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => serde_json::from_value(v.clone()).map_err(|_| {
                invalid_params("'status' must be one of running/paused/blocked/done/cancelled")
            })?,
        };
        let goals = goals_for_list.list_for(project.as_deref(), status);
        Ok(json!({ "goals": goals }))
    });

    let goals_for_get = goals.clone();
    actions.register_silent("goal.get", move |params| {
        let id = require_id_param(&params, "goal.get")?;
        match goals_for_get.get(&id) {
            Some(g) => {
                serde_json::to_value(&g).map_err(|e| internal_error(format!("serialize goal: {e}")))
            }
            None => Err(copad_core::protocol::ResponseError {
                code: "not_found".into(),
                message: format!("goal not found: {id}"),
            }),
        }
    });

    let goals_for_pause = goals.clone();
    let bus_for_pause = bus.clone();
    actions.register_blocking("goal.pause", move |params| {
        let id = require_id_param(&params, "goal.pause")?;
        let goal = goals_for_pause.pause(&id).map_err(internal_error)?;
        bus_for_pause.publish(BusEvent::new(
            "goal.paused",
            "copad-daemon",
            json!({"goal_id": goal.id}),
        ));
        serde_json::to_value(&goal).map_err(|e| internal_error(format!("serialize goal: {e}")))
    });

    let goals_for_resume = goals.clone();
    let bus_for_resume = bus.clone();
    actions.register_blocking("goal.resume", move |params| {
        let id = require_id_param(&params, "goal.resume")?;
        let goal = goals_for_resume.resume(&id).map_err(internal_error)?;
        bus_for_resume.publish(BusEvent::new(
            "goal.resumed",
            "copad-daemon",
            json!({"goal_id": goal.id}),
        ));
        serde_json::to_value(&goal).map_err(|e| internal_error(format!("serialize goal: {e}")))
    });

    let goals_for_answer = goals.clone();
    let bus_for_answer = bus.clone();
    actions.register_blocking("goal.answer", move |params| {
        let id = require_id_param(&params, "goal.answer")?;
        let answer = params
            .get("answer")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("goal.answer requires non-empty 'answer' string"))?
            .to_string();
        let goal = goals_for_answer
            .answer(&id, &answer)
            .map_err(internal_error)?;
        bus_for_answer.publish(BusEvent::new(
            "goal.unblocked",
            "copad-daemon",
            json!({"goal_id": goal.id}),
        ));
        serde_json::to_value(&goal).map_err(|e| internal_error(format!("serialize goal: {e}")))
    });

    let goals_for_cancel = goals.clone();
    let bus_for_cancel = bus.clone();
    actions.register_blocking("goal.cancel", move |params| {
        let id = require_id_param(&params, "goal.cancel")?;
        let goal = goals_for_cancel.cancel(&id).map_err(internal_error)?;
        bus_for_cancel.publish(BusEvent::new(
            "goal.cancelled",
            "copad-daemon",
            json!({"goal_id": goal.id}),
        ));
        serde_json::to_value(&goal).map_err(|e| internal_error(format!("serialize goal: {e}")))
    });

    // Daemon-side bridge for the tick result loop: GUI publishes
    // `goal.tick.completed {goal_id, raw_output, panel_id?}` after the
    // claude tab exits; we parse + apply + emit the state-change event.
    let goals_for_tick_apply = goals.clone();
    let bus_for_tick_apply = bus.clone();
    actions.register_blocking("goal.tick.apply", move |params| {
        let id = require_id_param(&params, "goal.tick.apply")?;
        let raw_output = params
            .get("raw_output")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let result = parse_tick_output(raw_output);
        let outcome_str = result.outcome.as_str().to_string();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let goal = goals_for_tick_apply
            .apply_tick_result(&id, result, now_ms)
            .map_err(internal_error)?;
        bus_for_tick_apply.publish(BusEvent::new(
            "goal.tick.completed",
            "copad-daemon",
            json!({
                "goal_id": goal.id,
                "outcome": outcome_str,
                "status": goal.status.as_str(),
            }),
        ));
        if goal.status == GoalStatus::Blocked && goal.blocked_question.is_some() {
            bus_for_tick_apply.publish(BusEvent::new(
                "goal.blocked",
                "copad-daemon",
                json!({"goal_id": goal.id, "question": goal.blocked_question}),
            ));
        }
        if goal.status == GoalStatus::Done {
            bus_for_tick_apply.publish(BusEvent::new(
                "goal.done",
                "copad-daemon",
                json!({"goal_id": goal.id}),
            ));
        }
        serde_json::to_value(&goal).map_err(|e| internal_error(format!("serialize goal: {e}")))
    });

    let goals_for_tick_mark = goals.clone();
    actions.register_blocking_silent("goal.tick.mark_in_flight", move |params| {
        let id = require_id_param(&params, "goal.tick.mark_in_flight")?;
        let panel_id = params
            .get("panel_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("'panel_id' required"))?
            .to_string();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let goal = goals_for_tick_mark
            .mark_in_flight(&id, &panel_id, now_ms)
            .map_err(internal_error)?;
        serde_json::to_value(&goal).map_err(|e| internal_error(format!("serialize goal: {e}")))
    });

    let tick_stop = Arc::new(AtomicBool::new(false));
    (goals, tick_stop)
}

/// Phase 22.5 — Agent actions (CRUD + memory append). Read-mostly:
/// builtin profiles ship embedded; user overrides land at
/// `~/.local/state/copad/agents/<id>/`.
fn register_agent_actions(actions: &Arc<ActionRegistry>) -> Arc<copad_core::agent::AgentRegistry> {
    use copad_core::agent::AgentRegistry;

    let root = copad_core::paths::state_dir().join("agents");
    let registry = Arc::new(AgentRegistry::new(root));

    let r1 = registry.clone();
    actions.register_silent("agent.list", move |_| Ok(json!({ "agents": r1.list() })));

    let r2 = registry.clone();
    actions.register_silent("agent.get", move |params| {
        let id = require_id_param(&params, "agent.get")?;
        match r2.get(&id) {
            Some(a) => serde_json::to_value(&a)
                .map_err(|e| internal_error(format!("serialize agent: {e}"))),
            None => Err(copad_core::protocol::ResponseError {
                code: "not_found".into(),
                message: format!("agent not found: {id}"),
            }),
        }
    });

    let r3 = registry.clone();
    actions.register_silent("agent.show_memory", move |params| {
        let id = require_id_param(&params, "agent.show_memory")?;
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let agent = r3
            .get(&id)
            .ok_or_else(|| copad_core::protocol::ResponseError {
                code: "not_found".into(),
                message: format!("agent not found: {id}"),
            })?;
        let total = agent.memory.len();
        let tail: Vec<_> = agent.memory.into_iter().rev().take(limit).collect();
        Ok(json!({ "entries": tail, "total": total }))
    });

    let r4 = registry.clone();
    actions.register_blocking("agent.append_memory", move |params| {
        let id = require_id_param(&params, "agent.append_memory")?;
        let kind = params
            .get("kind")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("'kind' required (non-empty string)"))?
            .to_string();
        let body = params
            .get("body")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("'body' required (string)"))?
            .to_string();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let entry = r4
            .append_memory(&id, &kind, &body, now_ms)
            .map_err(internal_error)?;
        serde_json::to_value(&entry).map_err(|e| internal_error(format!("serialize entry: {e}")))
    });

    registry
}

/// Phase 22.5 — Mission actions (CRUD + state transitions). Wake-
/// condition auto-firing arrives with 22.7 + Brain dispatcher.
fn register_mission_actions(
    actions: &Arc<ActionRegistry>,
    bus: &Arc<copad_core::event_bus::EventBus>,
) -> Arc<copad_core::mission::MissionRegistry> {
    use copad_core::event_bus::Event as BusEvent;
    use copad_core::mission::{
        AgentAssignment, MissionBudget, MissionRegistry, MissionState, WakeCondition,
    };

    let root = copad_core::paths::state_dir().join("missions");
    let registry = Arc::new(MissionRegistry::new(root));

    let r1 = registry.clone();
    let bus1 = bus.clone();
    actions.register_blocking("mission.submit", move |params| {
        let title = params
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("'title' required (non-empty string)"))?
            .to_string();
        let objective = params
            .get("objective")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("'objective' required (non-empty string)"))?
            .to_string();
        let project = params
            .get("project")
            .and_then(|v| v.as_str())
            .map(String::from);
        let urgency = params.get("urgency").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        let cadence = params
            .get("cadence")
            .and_then(|v| v.as_str())
            .map(String::from);
        let assigned_agents: Vec<AgentAssignment> = match params.get("assigned_agents") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| invalid_params(format!("'assigned_agents' must be an array: {e}")))?,
        };
        let budget: MissionBudget = match params.get("budget") {
            None | Some(serde_json::Value::Null) => MissionBudget::default(),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| invalid_params(format!("'budget' invalid: {e}")))?,
        };
        let wake_conditions: Vec<WakeCondition> = match params.get("wake_conditions") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| invalid_params(format!("'wake_conditions' invalid: {e}")))?,
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let mission = r1
            .submit(
                &title,
                &objective,
                project.as_deref(),
                assigned_agents,
                budget,
                wake_conditions,
                cadence.as_deref(),
                urgency,
                now_ms,
            )
            .map_err(internal_error)?;
        bus1.publish(BusEvent::new(
            "mission.created",
            "copad-daemon",
            json!({"id": mission.id, "title": mission.title, "project": mission.project}),
        ));
        serde_json::to_value(&mission)
            .map_err(|e| internal_error(format!("serialize mission: {e}")))
    });

    let r2 = registry.clone();
    actions.register_silent("mission.list", move |params| {
        let project = params
            .get("project")
            .and_then(|v| v.as_str())
            .map(String::from);
        let state: Option<MissionState> = match params.get("state") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => serde_json::from_value(v.clone()).map_err(|_| {
                invalid_params("'state' must be one of pending/active/paused/done/aborted")
            })?,
        };
        Ok(json!({ "missions": r2.list_for(project.as_deref(), state) }))
    });

    let r3 = registry.clone();
    actions.register_silent("mission.get", move |params| {
        let id = require_id_param(&params, "mission.get")?;
        match r3.get(&id) {
            Some(m) => serde_json::to_value(&m)
                .map_err(|e| internal_error(format!("serialize mission: {e}"))),
            None => Err(copad_core::protocol::ResponseError {
                code: "not_found".into(),
                message: format!("mission not found: {id}"),
            }),
        }
    });

    // Macro-pattern transitions: pause/resume/abort + redirect/assign + turn_started/turn_completed.
    let r4 = registry.clone();
    let bus4 = bus.clone();
    actions.register_blocking("mission.pause", move |params| {
        let id = require_id_param(&params, "mission.pause")?;
        let m = r4.pause(&id).map_err(internal_error)?;
        bus4.publish(BusEvent::new(
            "mission.paused",
            "copad-daemon",
            json!({"mission_id": m.id}),
        ));
        serde_json::to_value(&m).map_err(|e| internal_error(format!("serialize mission: {e}")))
    });

    let r5 = registry.clone();
    let bus5 = bus.clone();
    actions.register_blocking("mission.resume", move |params| {
        let id = require_id_param(&params, "mission.resume")?;
        let m = r5.resume(&id).map_err(internal_error)?;
        bus5.publish(BusEvent::new(
            "mission.resumed",
            "copad-daemon",
            json!({"mission_id": m.id}),
        ));
        serde_json::to_value(&m).map_err(|e| internal_error(format!("serialize mission: {e}")))
    });

    let r6 = registry.clone();
    let bus6 = bus.clone();
    actions.register_blocking("mission.abort", move |params| {
        let id = require_id_param(&params, "mission.abort")?;
        let m = r6.abort(&id).map_err(internal_error)?;
        bus6.publish(BusEvent::new(
            "mission.aborted",
            "copad-daemon",
            json!({"mission_id": m.id}),
        ));
        serde_json::to_value(&m).map_err(|e| internal_error(format!("serialize mission: {e}")))
    });

    let r7 = registry.clone();
    actions.register_blocking("mission.redirect_objective", move |params| {
        let id = require_id_param(&params, "mission.redirect_objective")?;
        let new_objective = params
            .get("new_objective")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("'new_objective' required (non-empty string)"))?;
        let m = r7
            .redirect_objective(&id, new_objective)
            .map_err(internal_error)?;
        serde_json::to_value(&m).map_err(|e| internal_error(format!("serialize mission: {e}")))
    });

    let r8 = registry.clone();
    actions.register_blocking("mission.assign_agent", move |params| {
        let id = require_id_param(&params, "mission.assign_agent")?;
        let agent_id = params
            .get("agent_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("'agent_id' required (non-empty string)"))?;
        let role = params.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let m = r8
            .assign_agent(&id, agent_id, role)
            .map_err(internal_error)?;
        serde_json::to_value(&m).map_err(|e| internal_error(format!("serialize mission: {e}")))
    });

    let r9 = registry.clone();
    let bus9 = bus.clone();
    actions.register_blocking("mission.turn_started", move |params| {
        let id = require_id_param(&params, "mission.turn_started")?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let m = r9.turn_started(&id, now_ms).map_err(internal_error)?;
        bus9.publish(BusEvent::new(
            "mission.turn_started",
            "copad-daemon",
            json!({"mission_id": m.id, "turn_count": m.turn_count}),
        ));
        serde_json::to_value(&m).map_err(|e| internal_error(format!("serialize mission: {e}")))
    });

    let r10 = registry.clone();
    let bus10 = bus.clone();
    actions.register_blocking("mission.turn_completed", move |params| {
        let id = require_id_param(&params, "mission.turn_completed")?;
        let decision = params
            .get("decision")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let detail = params.get("detail").and_then(|v| v.as_str()).unwrap_or("");
        let m = r10
            .turn_completed(&id, decision, detail)
            .map_err(internal_error)?;
        bus10.publish(BusEvent::new(
            "mission.turn_completed",
            "copad-daemon",
            json!({"mission_id": m.id, "decision": decision, "state": m.state.as_str()}),
        ));
        serde_json::to_value(&m).map_err(|e| internal_error(format!("serialize mission: {e}")))
    });

    registry
}

/// Phase 22.6 — Approval gate. CRUD + grant/deny actions.
fn register_approval_actions(
    actions: &Arc<ActionRegistry>,
    bus: &Arc<copad_core::event_bus::EventBus>,
) -> (Arc<copad_core::approval::ApprovalRegistry>, Arc<AtomicBool>) {
    use copad_core::approval::{ApprovalRegistry, ApprovalState};
    use copad_core::event_bus::Event as BusEvent;

    let root = copad_core::paths::state_dir().join("approvals");
    let registry = Arc::new(ApprovalRegistry::new(root));

    let r1 = registry.clone();
    let bus1 = bus.clone();
    actions.register_blocking("approval.request", move |params| {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_params("'action' required (non-empty string)"))?
            .to_string();
        let params_preview = params
            .get("params_preview")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let rationale = params
            .get("rationale")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mission_id = params
            .get("mission_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let agent_id = params
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let project = params
            .get("project")
            .and_then(|v| v.as_str())
            .map(String::from);
        let ttl_secs = params
            .get("ttl_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(300);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let appr = r1
            .request(
                &action,
                params_preview,
                &rationale,
                mission_id.as_deref(),
                agent_id.as_deref(),
                project.as_deref(),
                ttl_secs,
                now_ms,
            )
            .map_err(internal_error)?;
        bus1.publish(BusEvent::new(
            "approval.requested",
            "copad-daemon",
            json!({
                "id": appr.id,
                "action": appr.action,
                "params_preview": appr.params_preview,
                "rationale": appr.rationale,
                "expires_at_ms": appr.expires_at_ms,
            }),
        ));
        serde_json::to_value(&appr).map_err(|e| internal_error(format!("serialize: {e}")))
    });

    let r2 = registry.clone();
    actions.register_silent("approval.list", move |params| {
        let state: Option<ApprovalState> = match params.get("state") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|_| invalid_params("'state' must be pending/granted/denied/expired"))?,
        };
        let project = params
            .get("project")
            .and_then(|v| v.as_str())
            .map(String::from);
        Ok(json!({ "approvals": r2.list_for(state, project.as_deref()) }))
    });

    let r3 = registry.clone();
    actions.register_silent("approval.get", move |params| {
        let id = require_id_param(&params, "approval.get")?;
        match r3.get(&id) {
            Some(a) => {
                serde_json::to_value(&a).map_err(|e| internal_error(format!("serialize: {e}")))
            }
            None => Err(copad_core::protocol::ResponseError {
                code: "not_found".into(),
                message: format!("approval not found: {id}"),
            }),
        }
    });

    let r4 = registry.clone();
    let bus4 = bus.clone();
    actions.register_blocking("approval.grant", move |params| {
        let id = require_id_param(&params, "approval.grant")?;
        let by = params.get("by").and_then(|v| v.as_str()).unwrap_or("user");
        let note = params.get("note").and_then(|v| v.as_str());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let appr = r4.grant(&id, by, note, now_ms).map_err(internal_error)?;
        bus4.publish(BusEvent::new(
            "approval.granted",
            "copad-daemon",
            json!({"id": appr.id, "by": appr.decided_by}),
        ));
        serde_json::to_value(&appr).map_err(|e| internal_error(format!("serialize: {e}")))
    });

    let r5 = registry.clone();
    let bus5 = bus.clone();
    actions.register_blocking("approval.deny", move |params| {
        let id = require_id_param(&params, "approval.deny")?;
        let by = params.get("by").and_then(|v| v.as_str()).unwrap_or("user");
        let reason = params.get("reason").and_then(|v| v.as_str());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let appr = r5.deny(&id, by, reason, now_ms).map_err(internal_error)?;
        bus5.publish(BusEvent::new(
            "approval.denied",
            "copad-daemon",
            json!({"id": appr.id, "by": appr.decided_by, "reason": appr.decision_note}),
        ));
        serde_json::to_value(&appr).map_err(|e| internal_error(format!("serialize: {e}")))
    });

    let stop = Arc::new(AtomicBool::new(false));
    (registry, stop)
}

/// 30s sweeper thread for approval TTL expiry. Same pattern as the
/// goal tick scheduler — gets ripped out once cron triggers ship.
fn spawn_approval_sweeper(
    registry: Arc<copad_core::approval::ApprovalRegistry>,
    bus: Arc<copad_core::event_bus::EventBus>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    use copad_core::event_bus::Event as BusEvent;
    thread::Builder::new()
        .name("copad-approval-sweep".into())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                for _ in 0..30 {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                for appr in registry.sweep_expired(now_ms) {
                    bus.publish(BusEvent::new(
                        "approval.expired",
                        "copad-daemon",
                        json!({"id": appr.id}),
                    ));
                }
            }
        })
        .expect("spawn copad-approval-sweep thread")
}

/// Phase 22.6 — Runledger subscriber + replay action. Spawns a thread
/// that drains a bus subscription into the monthly JSONL ledger.
fn register_runledger_actions(
    actions: &Arc<ActionRegistry>,
    bus: &Arc<copad_core::event_bus::EventBus>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    use copad_core::event_bus::RecvOutcome;
    use copad_core::runledger::Runledger;

    let root = copad_core::paths::state_dir().join("runledger");
    let runledger = Arc::new(Runledger::new(root));

    let r1 = runledger.clone();
    actions.register_silent("events.replay", move |params| {
        let since_ms = params.get("since_ms").and_then(|v| v.as_i64()).unwrap_or(0);
        let kinds: Option<Vec<String>> = match params.get("kinds") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Array(arr)) => Some(
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
            ),
            Some(_) => return Err(invalid_params("'kinds' must be an array of strings")),
        };
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let entries = r1
            .replay(since_ms, kinds.as_deref(), limit)
            .map_err(internal_error)?;
        let total = entries.len();
        Ok(json!({ "entries": entries, "total": total }))
    });

    let r2 = runledger.clone();
    let rx = bus.subscribe_unbounded("*");
    thread::Builder::new()
        .name("copad-runledger".into())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match rx.recv_timeout(Duration::from_secs(1)) {
                    RecvOutcome::Event(e) => {
                        if let Err(err) = r2.append(&e) {
                            log::warn!("runledger append failed: {err}");
                        }
                    }
                    RecvOutcome::Timeout => {}
                    RecvOutcome::Disconnected => return,
                }
            }
        })
        .expect("spawn copad-runledger thread")
}

fn require_id_param(
    params: &serde_json::Value,
    method: &str,
) -> Result<String, copad_core::protocol::ResponseError> {
    params
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| invalid_params(format!("{method} requires non-empty 'id' string")))
}

/// 60-second tick scheduler — picks `next_runnable()` from the
/// GoalRegistry, builds the prompt (roadmap.md + last 5 history
/// entries + JSON instruction), and publishes `goal.tick.started
/// {goal_id, prompt, workspace_path}`. The GUI subscribes and
/// dispatches `claude.start`; daemon's `goal.tick.apply` handler
/// closes the loop when the GUI publishes `goal.tick.completed`.
fn spawn_goal_tick_thread(
    goals: Arc<copad_core::goal::GoalRegistry>,
    bus: Arc<copad_core::event_bus::EventBus>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    use copad_core::event_bus::Event as BusEvent;

    const TICK_INTERVAL: Duration = Duration::from_secs(60);
    thread::Builder::new()
        .name("copad-goal-tick".into())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                // Sleep first so the first tick lands at +60s (gives the
                // GUI time to register on cold boot).
                let sleep_chunks = 60u64;
                for _ in 0..sleep_chunks {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
                let _ = TICK_INTERVAL;
                let Some(goal) = goals.next_runnable() else {
                    continue;
                };
                let roadmap = goals.read_roadmap(&goal.id).unwrap_or_default();
                let history_tail: String = goal
                    .history
                    .iter()
                    .rev()
                    .take(5)
                    .rev()
                    .map(|h| format!("- {}: {}", h.outcome, h.detail))
                    .collect::<Vec<_>>()
                    .join("\n");
                let prompt = format!(
                    "You are working on goal `{title}` for project `{project}`.\n\n\
                     ## Current roadmap (`roadmap.md`)\n\n{roadmap}\n\n\
                     ## Recent ticks\n{history}\n\n\
                     ## Your task\n\
                     Take one concrete step toward the goal. When done, RESPOND \
                     WITH a fenced JSON block as the LAST thing you write:\n\n\
                     ```json\n\
                     {{\"next_action\": \"<one of: record_progress, ask_player, \
                     invoke_specialist, self_schedule, complete>\", \
                     \"detail\": \"<short description>\"}}\n\
                     ```\n",
                    title = goal.title,
                    project = goal.project,
                    roadmap = roadmap,
                    history = if history_tail.is_empty() {
                        "(none yet)".to_string()
                    } else {
                        history_tail
                    },
                );
                bus.publish(BusEvent::new(
                    "goal.tick.started",
                    "copad-daemon",
                    json!({
                        "goal_id": goal.id,
                        "title": goal.title,
                        "project": goal.project,
                        "workspace_path": goal.project_path,
                        "prompt": prompt,
                    }),
                ));
            }
        })
        .expect("spawn copad-goal-tick thread")
}

fn register_e2e_actions(actions: &Arc<ActionRegistry>) {
    log::warn!("e2e test actions enabled (COPADD_E2E_TEST_ACTIONS=1)");
    actions.register_blocking("__test.slow_blocking", |params| {
        let ms = params.get("ms").and_then(|v| v.as_u64()).unwrap_or(200);
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(json!({ "slept_ms": ms }))
    });
}

fn register_plugin_commands(
    actions: &Arc<ActionRegistry>,
    plugins: &Arc<Vec<LoadedPlugin>>,
    socket_path: &Path,
) {
    let socket_str = socket_path.to_string_lossy().into_owned();
    // Iterate only the WINNING entry per unique plugin name (sorted
    // slice → resolve_by_name returns the last-by-dir entry). Without
    // this dedup, a losing duplicate with a command name the winner
    // does NOT have would leak into the dispatch table; only collisions
    // on the same `<name>.<cmd>` method get HashMap-overwritten.
    let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let winners: Vec<&LoadedPlugin> = plugins
        .iter()
        .rev()
        .filter(|p| seen_names.insert(p.manifest.plugin.name.as_str()))
        .collect();
    for plugin in winners.iter() {
        let plugin_name = plugin.manifest.plugin.name.clone();
        for cmd in &plugin.manifest.commands {
            // A dot in the command name would create a 4+ segment
            // method that breaks `plugin.<name>.<cmd>` parsing for
            // downstream consumers (the trigger engine, the CLI).
            if cmd.name.contains('.') {
                log::warn!(
                    "plugin {} command `{}` contains a dot; skipping registration",
                    plugin_name,
                    cmd.name
                );
                continue;
            }
            let method = format!("plugin.{}.{}", plugin_name, cmd.name);
            let exec = cmd.exec.clone();
            let dir = plugin.dir.clone();
            let socket = socket_str.clone();
            actions.register_blocking(method, move |params| {
                run_plugin_shell(
                    &dir,
                    &exec,
                    &params.to_string(),
                    &socket,
                    PLUGIN_CMD_TIMEOUT,
                )
                .map(parse_plugin_stdout)
                .map_err(map_shell_error)
            });
        }
    }
    let plugins_for_module = plugins.clone();
    let socket_for_module = socket_str;
    actions.register_blocking_silent("_module.run", move |params| {
        let plugin_name = params
            .get("plugin")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("missing 'plugin' field"))?;
        let module_name = params
            .get("module")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("missing 'module' field"))?;
        // `resolve_by_name` picks the sorted-last (winner) entry,
        // matching `register_plugin_commands`' winners-only set.
        let plugin = copad_core::plugin::resolve_by_name(&plugins_for_module, plugin_name)
            .ok_or_else(|| ResponseError {
                code: "not_found".into(),
                message: format!("plugin not found: {plugin_name}"),
            })?;
        let module = plugin
            .manifest
            .modules
            .iter()
            .find(|m| m.name == module_name)
            .ok_or_else(|| ResponseError {
                code: "not_found".into(),
                message: format!("module '{module_name}' not in plugin '{plugin_name}'"),
            })?;
        let out = run_plugin_shell(
            &plugin.dir,
            &module.exec,
            "",
            &socket_for_module,
            MODULE_RUN_TIMEOUT,
        )
        .map_err(map_shell_error)?;
        Ok(json!({
            "stdout": out.stdout,
            "exit_code": out.exit_code,
        }))
    });
}

fn run_plugin_shell(
    dir: &Path,
    exec: &str,
    stdin_payload: &str,
    socket_path: &str,
    timeout: Duration,
) -> Result<copad_daemon::plugin_exec::ShellOutput, ShellError> {
    let mut env = HashMap::new();
    env.insert("COPAD_SOCKET".into(), socket_path.into());
    env.insert(
        "COPAD_PLUGIN_DIR".into(),
        dir.to_string_lossy().into_owned(),
    );
    spawn_plugin_shell(dir, exec, stdin_payload.as_bytes(), &env, timeout)
}

/// Mirrors the legacy GUI handler's contract: JSON stdout is returned
/// verbatim; otherwise wrap the trimmed text under `{ "output": ... }`
/// so the caller always receives a JSON object.
fn parse_plugin_stdout(out: copad_daemon::plugin_exec::ShellOutput) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(&out.stdout)
        .unwrap_or_else(|_| json!({ "output": out.stdout.trim() }))
}

fn map_shell_error(err: ShellError) -> ResponseError {
    match err {
        ShellError::NonZero(out) => ResponseError {
            code: "plugin_command_failed".into(),
            message: format!(
                "exit {}: {}",
                out.exit_code,
                out.stderr.trim().lines().next().unwrap_or("")
            ),
        },
        ShellError::Timeout { after, .. } => ResponseError {
            code: "plugin_timeout".into(),
            message: format!("plugin shell did not complete within {after:?}"),
        },
        ShellError::Spawn(msg) | ShellError::Wait(msg) => ResponseError {
            code: "plugin_spawn_failed".into(),
            message: msg,
        },
    }
}

fn register_builtins(
    actions: &Arc<ActionRegistry>,
    plugins: &Arc<Vec<copad_core::plugin::LoadedPlugin>>,
    host_triggers: bool,
) {
    actions.register_silent("system.ping", |_| Ok(json!({ "status": "ok" })));
    actions.register("system.log", |params| {
        let msg = params
            .get("message")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| params.to_string());
        eprintln!("[system.log] {msg}");
        Ok(json!({}))
    });
    let actions_for_info = actions.clone();
    actions.register_silent("daemon.info", move |_| {
        let stats = actions_for_info.pool_stats();
        serde_json::to_value(serde_json::json!({
            "daemon": "copadd",
            "version": env!("CARGO_PKG_VERSION"),
            "host_plugins": true,
            "host_triggers": host_triggers,
            "pool": stats.map(|s| serde_json::json!({
                "workers": s.workers,
                "capacity": s.capacity,
                "active": s.active,
                "queued": s.queued,
            })),
        }))
        .map_err(|e| internal_error(format!("daemon.info serialization failed: {e}")))
    });
    actions.register("theme.list", |_| {
        let themes: Vec<&str> = copad_core::theme::Theme::list().to_vec();
        // `current` is GUI-state (per-window). Daemon reports null; GUI
        // resolves its own current theme through GUI-owned routing later.
        Ok(json!({ "themes": themes, "current": serde_json::Value::Null }))
    });
    let plugins_for_list = plugins.clone();
    actions.register("plugin.list", move |_| {
        let body: Vec<_> = plugins_for_list
            .iter()
            .map(|p| {
                let m = &p.manifest;
                json!({
                    "name": m.plugin.name,
                    "title": m.plugin.title,
                    "version": m.plugin.version,
                    "description": m.plugin.description,
                    "panels": m.panels.iter().map(|pd| json!({
                        "name": pd.name,
                        "title": pd.title,
                        "file": pd.file,
                        "icon": pd.icon,
                    })).collect::<Vec<_>>(),
                    "commands": m.commands.iter().map(|c| json!({
                        "name": c.name,
                        "exec": c.exec,
                        "description": c.description,
                    })).collect::<Vec<_>>(),
                    "modules": m.modules.iter().map(|md| json!({
                        "name": md.name,
                        "exec": md.exec,
                        "interval": md.interval,
                        "position": md.position,
                        "order": md.order,
                        "class": md.class,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(json!({ "plugins": body }))
    });
}

/// Accepts `1`, `true`, `yes` (case-insensitive). Everything else,
/// including `0` / `false` / empty / unset, disables.
fn env_flag_enabled(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => false,
    }
}

/// Mirror of `env_flag_enabled` for flags whose default is ON.
/// `None` (env unset) = enabled; `Some("0" | "false" | "no" | "")`
/// (case-insensitive, trimmed) = disabled; every other value is
/// treated as enabled. We bias enable-on-garbage so a typo doesn't
/// silently turn the feature off — opt-out has to be intentional.
///
/// Takes a pre-extracted optional value rather than the env var name
/// itself so tests can exercise the parser without mutating the
/// process-global environment (cargo runs tests in parallel; concurrent
/// `set_var`/`remove_var` is unsound on glibc per the `env` module's
/// safety contract — codex C1 round 1).
fn env_flag_default_on(value: Option<&str>) -> bool {
    match value {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | ""
        ),
        None => true,
    }
}

/// `manifest.plugin.name` is the dispatch key for `plugin.<name>.<cmd>`
/// and statusbar `_module.run`. Sort the once-discovered list so two
/// daemons on the same machine register the same set in the same order
/// (deterministic last-write-wins on duplicates). Warns about dupes so
/// the user can fix the manifest.
pub fn discover_and_sort_plugins() -> Arc<Vec<copad_core::plugin::LoadedPlugin>> {
    let plugins = copad_core::plugin::discover_sorted_plugins();
    // After sort: equal names are adjacent and ordered by dir.
    // `register_blocking` does last-write-wins on HashMap insertion,
    // so the entry registered LAST (largest dir) is the active one.
    // `copad_core::plugin::resolve_by_name` picks the same winner.
    let mut prev: Option<&str> = None;
    for p in &plugins {
        let name = p.manifest.plugin.name.as_str();
        if Some(name) == prev {
            log::warn!(
                "duplicate plugin manifest name `{}` at {}; the entry sorted last by dir wins `plugin.{}.<cmd>` resolution",
                name,
                p.dir.display(),
                name
            );
        }
        prev = Some(name);
    }
    log::info!(
        "discovered {} plugin manifest(s); spawning onStartup services",
        plugins.len()
    );
    for p in &plugins {
        log::info!(
            "plugin: {} v{}",
            p.manifest.plugin.name,
            p.manifest.plugin.version
        );
    }
    Arc::new(plugins)
}

fn activate_supervisor(
    actions: &Arc<ActionRegistry>,
    event_bus: &Arc<copad_core::event_bus::EventBus>,
    plugins: &Arc<Vec<copad_core::plugin::LoadedPlugin>>,
    socket_path: PathBuf,
) -> Arc<ServiceSupervisor> {
    let reserved: Vec<&str> = LEGACY_DISPATCH_METHODS
        .iter()
        .copied()
        .chain(TRIGGER_ONLY_RESERVED_METHODS.iter().copied())
        .collect();
    ServiceSupervisor::new(
        event_bus.clone(),
        actions.clone(),
        plugins,
        env!("CARGO_PKG_VERSION"),
        &reserved,
        socket_path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use copad_core::event_bus::EventBus;
    use copad_core::trigger::{SecurityBlock, WhenSpec};
    use serde_json::Value;

    fn mk_trigger(name: &str, kind: &str) -> Trigger {
        Trigger {
            name: name.into(),
            when: WhenSpec {
                event_kind: kind.into(),
                payload_match: serde_json::Map::new(),
            },
            action: "system.log".into(),
            params: Value::Null,
            condition: None,
            r#await: None,
            security: SecurityBlock::default(),
        }
    }

    fn mk_pump_bundle() -> (
        Arc<EventBus>,
        Arc<TriggerEngine>,
        Arc<Mutex<PumpState>>,
        Arc<ContextService>,
    ) {
        let bus = Arc::new(EventBus::new());
        let actions = Arc::new(ActionRegistry::new());
        let sink: Arc<dyn copad_core::trigger::TriggerSink> =
            actions as Arc<dyn copad_core::trigger::TriggerSink>;
        let engine = Arc::new(TriggerEngine::with_publish_bus(sink, bus.clone()));
        let pump = Arc::new(Mutex::new(PumpState::new(&bus)));
        let ctx = Arc::new(ContextService::new());
        (bus, engine, pump, ctx)
    }

    #[test]
    fn apply_reloaded_triggers_replaces_engine_and_reconciles() {
        let (bus, engine, pump, ctx) = mk_pump_bundle();
        let cached = Arc::new(Mutex::new(Vec::<Trigger>::new()));
        let new = vec![
            mk_trigger("a", "panel.focused"),
            mk_trigger("b", "terminal.cwd_changed"),
        ];
        apply_reloaded_triggers(&engine, Some(&pump), &ctx, &bus, &cached, new.clone());
        assert_eq!(engine.count(), 2);
        assert_eq!(pump.lock().unwrap().trigger_subs_len(), 2);
        assert_eq!(cached.lock().unwrap().len(), 2);
    }

    #[test]
    fn apply_reloaded_triggers_without_pump_only_updates_engine() {
        // host_triggers=false path — no PumpState exists, so the
        // engine's internal trigger list updates but no bus
        // subscriptions are touched (and none should accumulate).
        let (bus, engine, _pump, ctx) = mk_pump_bundle();
        let cached = Arc::new(Mutex::new(Vec::<Trigger>::new()));
        apply_reloaded_triggers(
            &engine,
            None,
            &ctx,
            &bus,
            &cached,
            vec![mk_trigger("a", "panel.focused")],
        );
        assert_eq!(engine.count(), 1);
    }

    #[test]
    fn config_watcher_picks_up_mtime_change() {
        let dir = std::env::temp_dir().join(format!(
            "copad-watch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("config.toml");
        // Initial: empty triggers
        std::fs::write(&path, "").expect("write initial");

        let (bus, engine, _pump, ctx) = mk_pump_bundle();
        let cached = Arc::new(Mutex::new(Vec::<Trigger>::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let path_clone = path.clone();
        let engine_clone = engine.clone();
        let ctx_clone = ctx.clone();
        let bus_clone = bus.clone();
        let cached_clone = cached.clone();
        let stop_clone = stop.clone();
        let initial_mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());
        let handle = thread::spawn(move || {
            config_watcher_loop(
                engine_clone,
                None,
                ctx_clone,
                bus_clone,
                cached_clone,
                initial_mtime,
                stop_clone,
                &path_clone,
            );
        });

        // Sleep past the first tick, then rewrite with a trigger. The
        // 2s WATCHER_TICK makes this a 5s test — slow but adequate for
        // verifying the poll loop end-to-end.
        thread::sleep(Duration::from_millis(2500));
        std::fs::write(
            &path,
            r#"
[[triggers]]
name = "added"
action = "system.log"
params = { message = "hi" }
[triggers.when]
event_kind = "panel.focused"
"#,
        )
        .expect("write update");

        thread::sleep(Duration::from_millis(2500));
        assert_eq!(
            engine.count(),
            1,
            "watcher should have picked up the new trigger"
        );
        assert_eq!(cached.lock().unwrap().len(), 1);

        stop.store(true, Ordering::SeqCst);
        // join with a generous timeout via a polling check
        for _ in 0..30 {
            if handle.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        handle.join().expect("watcher thread joined");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    // ---- notify.show registration ----

    fn fresh_registry() -> Arc<copad_core::action_registry::ActionRegistry> {
        Arc::new(copad_core::action_registry::ActionRegistry::new())
    }

    #[test]
    fn notify_show_rejects_missing_title() {
        let actions = fresh_registry();
        let notifier = Arc::new(copad_core::notifier::NoopNotifier::default());
        register_notify_show(&actions, Some(notifier.clone()));
        let err = actions
            .invoke("notify.show", serde_json::json!({"body": "hi"}))
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(notifier.captured.lock().unwrap().is_empty());
    }

    #[test]
    fn notify_show_rejects_empty_title() {
        let actions = fresh_registry();
        let notifier = Arc::new(copad_core::notifier::NoopNotifier::default());
        register_notify_show(&actions, Some(notifier.clone()));
        let err = actions
            .invoke("notify.show", serde_json::json!({"title": "", "body": "x"}))
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(notifier.captured.lock().unwrap().is_empty());
    }

    #[test]
    fn notify_show_rejects_bad_level_string() {
        let actions = fresh_registry();
        let notifier = Arc::new(copad_core::notifier::NoopNotifier::default());
        register_notify_show(&actions, Some(notifier.clone()));
        let err = actions
            .invoke(
                "notify.show",
                serde_json::json!({"title": "t", "body": "b", "level": "loud"}),
            )
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(notifier.captured.lock().unwrap().is_empty());
    }

    #[test]
    fn notify_show_invokes_notifier_with_defaults() {
        // Blocking action returns `{"queued": true}` synchronously; the
        // handler runs on the action thread pool. Use try_dispatch with
        // a blocking callback so the test can read the captured side
        // effect deterministically.
        let actions = fresh_registry();
        let notifier = Arc::new(copad_core::notifier::NoopNotifier::default());
        register_notify_show(&actions, Some(notifier.clone()));
        let (tx, rx) = std::sync::mpsc::channel();
        actions.try_dispatch(
            "notify.show",
            serde_json::json!({"title": "hello", "body": "world"}),
            Box::new(move |r| {
                tx.send(r).ok();
            }),
        );
        let result = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("handler ran");
        assert!(result.is_ok(), "got error: {result:?}");
        let captured = notifier.captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let (title, body, level) = &captured[0];
        assert_eq!(title, "hello");
        assert_eq!(body, "world");
        assert_eq!(*level, copad_core::notifier::Level::Info);
    }

    #[test]
    fn notify_show_accepts_level_warn_and_error() {
        let actions = fresh_registry();
        let notifier = Arc::new(copad_core::notifier::NoopNotifier::default());
        register_notify_show(&actions, Some(notifier.clone()));
        for level_str in ["warn", "error"] {
            let (tx, rx) = std::sync::mpsc::channel();
            actions.try_dispatch(
                "notify.show",
                serde_json::json!({"title": "t", "body": "b", "level": level_str}),
                Box::new(move |r| {
                    tx.send(r).ok();
                }),
            );
            rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        }
        let captured = notifier.captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].2, copad_core::notifier::Level::Warn);
        assert_eq!(captured[1].2, copad_core::notifier::Level::Error);
    }

    #[test]
    fn notify_show_drops_when_no_platform_notifier() {
        let actions = fresh_registry();
        register_notify_show(&actions, None);
        let (tx, rx) = std::sync::mpsc::channel();
        actions.try_dispatch(
            "notify.show",
            serde_json::json!({"title": "t", "body": "b"}),
            Box::new(move |r| {
                tx.send(r).ok();
            }),
        );
        let result = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let value = result.expect("handler should return Ok even with no notifier");
        assert_eq!(value["shown"], false);
        assert_eq!(value["reason"], "no_notifier");
    }

    #[test]
    fn notify_show_runs_on_blocking_pool_and_does_not_fan_out_completion() {
        // Regression guard: blocking-silent means the subprocess runs
        // on the action thread pool (not the calling thread) AND no
        // `<action>.completed` event spams the bus. Build a registry
        // with a completion bus + subscribe to `notify.show.completed`
        // before invoking.
        let bus = Arc::new(copad_core::event_bus::EventBus::new());
        let actions =
            Arc::new(copad_core::action_registry::ActionRegistry::with_completion_bus(bus.clone()));
        let completed_rx = bus.subscribe("notify.show.completed");
        let notifier = Arc::new(copad_core::notifier::NoopNotifier::default());
        register_notify_show(&actions, Some(notifier.clone()));
        assert!(actions.has("notify.show"));
        assert!(actions.is_blocking("notify.show"));

        let (tx, rx) = std::sync::mpsc::channel();
        actions.try_dispatch(
            "notify.show",
            serde_json::json!({"title": "t", "body": "b"}),
            Box::new(move |r| {
                tx.send(r).ok();
            }),
        );
        rx.recv_timeout(Duration::from_secs(2))
            .expect("handler ran")
            .expect("handler returned Ok");
        // notifier was called…
        assert_eq!(notifier.captured.lock().unwrap().len(), 1);
        // …but completion event did NOT fan out. Sleep a beat in case
        // the bus tx is asynchronous, then assert no event arrived.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            matches!(
                completed_rx.recv_timeout(Duration::from_millis(50)),
                copad_core::event_bus::RecvOutcome::Timeout
            ),
            "silent action must not publish .completed"
        );
    }

    // ---- env_flag_default_on ----
    // Pure parser: takes a pre-extracted `Option<&str>` (caller resolves
    // `env::var`). No environment mutation in tests; safe under
    // parallel cargo execution. See the function doc-comment for the
    // codex C1 round 1 context that drove this shape.

    #[test]
    fn env_flag_default_on_when_unset() {
        assert!(env_flag_default_on(None));
    }

    #[test]
    fn env_flag_default_on_accepts_disable_tokens() {
        for v in ["0", "false", "FALSE", "no", "No", " false ", ""] {
            assert!(!env_flag_default_on(Some(v)), "value {v:?} should disable");
        }
    }

    #[test]
    fn env_flag_default_on_treats_garbage_as_enabled() {
        // Bias toward enabled-on-typo so a misspelled "fasle" or
        // "tru" doesn't silently turn off harness triggers.
        for v in ["1", "true", "yes", "on", "tru", "fasle", "garbage"] {
            assert!(env_flag_default_on(Some(v)), "value {v:?} should enable");
        }
    }
}
