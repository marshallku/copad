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
use copad_daemon::cron_scheduler::CronScheduler;
use copad_daemon::daemon_trigger_sink::DaemonTriggerSink;
use copad_daemon::gui_registry::GuiRegistry;
use copad_daemon::plugin_exec::{ShellError, spawn_plugin_shell};
use copad_daemon::service_supervisor::ServiceSupervisor;
use copad_daemon::socket::{
    self, BROWSER_RESERVED_METHODS, DaemonState, LEGACY_DISPATCH_METHODS, SocketPrep, new_event_bus,
};
use copad_daemon::trigger_pump::PumpState;
use copad_daemon::trigger_sink::TRIGGER_ONLY_RESERVED_METHODS;
use serde_json::json;

/// Below the supervisor's 120s action_timeout so the watchdog's kill+reap wins the race.
const PLUGIN_CMD_TIMEOUT: Duration = Duration::from_secs(90);

/// Statusbar modules tick every 10s by default; bounded so a runaway module can't pile up.
const MODULE_RUN_TIMEOUT: Duration = Duration::from_secs(8);

const ENV_E2E_ACTIONS: &str = "COPADD_E2E_TEST_ACTIONS";
const ENV_POOL_WORKERS: &str = "COPADD_POOL_WORKERS";
const ENV_POOL_QUEUE: &str = "COPADD_POOL_QUEUE";
/// Daemon-side TriggerEngine dispatch (decisions.md #39). Default ON: a registered GUI releases
/// its own in-process engine when the `gui.register` ack advertises `host_triggers=true`, so
/// there is no double-dispatch. Set `0`/`false`/`no` to keep the GUI engine authoritative
/// (standalone-GUI path).
const ENV_HOST_TRIGGERS: &str = "COPADD_HOST_TRIGGERS";

const PUMP_TICK: Duration = Duration::from_millis(50);

/// Config mtime poll interval.
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
    // Handle read-only flags BEFORE env_logger init / socket bind: `copadd --version` against a
    // running daemon would otherwise fail with "socket already bound" (decisions.md #40).
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
    let runledger_stop = Arc::new(AtomicBool::new(false));
    let _runledger_thread =
        register_runledger_actions(&actions, &event_bus, runledger_stop.clone());
    let _ = runledger_stop;

    // Built before the trigger sink so both share it: the sink's fallthrough worker resolves the
    // primary GUI via `gui.route(action, None)`.
    let gui = GuiRegistry::new();
    let context = Arc::new(ContextService::new());
    let (triggers_cfg, initial_mtime) = load_triggers_config();
    let cached_triggers = Arc::new(Mutex::new(triggers_cfg.clone()));
    let engine = build_trigger_engine(&actions, &gui, &context, &event_bus, &triggers_cfg);
    // Daily release check; runs regardless of trigger hosting. `COPAD_UPDATE_CHECK=0` disables.
    copad_daemon::update_check::spawn(
        event_bus.clone(),
        copad_core::notifier::platform_notifier().map(Arc::from),
    );
    // Only exists when dispatch-authoritative; with host_triggers=false no receivers are created,
    // so bus traffic does not accumulate.
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

    // Bind before activating plugins so a bind failure can't orphan eagerly spawned children.
    let listener = match socket::bind_listener(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            log::error!("bind({}): {e}", socket_path.display());
            return ExitCode::from(1);
        }
    };

    // macOS-only: kill plugin process groups left by a prior daemon that exited without
    // `shutdown_all` (panic/SIGKILL); Linux relies on PDEATHSIG. Runs BEFORE the supervisor
    // spawns plugins so a recycled pid isn't killed.
    #[cfg(target_os = "macos")]
    copad_daemon::service_supervisor::sweep_orphan_pids();

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

    // Daemon-only. With host_triggers=false the GUI owns dispatch and a daemon cron firer would
    // double-fire; skipped and logged so the operator knows cron triggers won't fire.
    let (cron_scheduler, cron_thread) = if host_triggers {
        let sched = Arc::new(CronScheduler::new(engine.clone(), context.clone()));
        sched.reload(&cached_triggers.lock().unwrap());
        let handle = sched.spawn();
        (Some(sched), Some(handle))
    } else {
        if cached_triggers.lock().unwrap().iter().any(|t| t.is_cron()) {
            log::warn!(
                "cron triggers present but host_triggers=false — \
                 scheduler disabled (set COPADD_HOST_TRIGGERS=1 to enable)"
            );
        }
        (None, None)
    };

    // Runs unconditionally so the engine tracks `[[triggers]]` edits even headless. With
    // host_triggers=false it updates the engine only (no PumpState); with true it follows the
    // GUI's hot-reload order (set_triggers -> pump_all -> reconcile). `initial_mtime` comes from
    // `load_triggers_config()` so an edit before the first tick is caught.
    let watcher_stop = Arc::new(AtomicBool::new(false));
    let watcher_thread = spawn_config_watcher(
        engine.clone(),
        pump_state.clone(),
        context.clone(),
        event_bus.clone(),
        cached_triggers.clone(),
        initial_mtime,
        cron_scheduler.clone(),
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
    if let Some(sched) = cron_scheduler.as_ref() {
        sched.shutdown();
    }
    if let Some(handle) = pump_thread
        && let Err(panic) = handle.join()
    {
        log::error!("trigger pump thread panicked: {panic:?}");
    }
    if let Err(panic) = watcher_thread.join() {
        log::error!("config watcher thread panicked: {panic:?}");
    }
    if let Some(handle) = cron_thread
        && let Err(panic) = handle.join()
    {
        log::error!("cron scheduler thread panicked: {panic:?}");
    }

    // Arc::drop does not call shutdown_all; run it before unlinking the socket.
    log::info!("shutting down supervised plugins");
    supervisor_guard.shutdown_all();
    // Explicit shutdown breaks any registry/handler/supervisor Arc cycle that would block Drop.
    pool.shutdown();

    socket::cleanup_socket(&socket_path);
    log::info!("copadd shut down");
    ExitCode::SUCCESS
}

/// Also returns the mtime sampled at load; the watcher seeds its baseline from it so an edit
/// before its first tick is detected.
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

/// Builds the engine and registers `context.snapshot`. Does NOT create the PumpState: that waits
/// for host-triggers-on mode, so subscriptions don't accumulate with no pump to drain them.
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
    // `event.history` mirrors the GUI's registration; each process hosts its own EventBus, so a
    // daemon-routed `coctl recent` returns the daemon's view. Silent so its own `.completed`
    // doesn't inflate the next call's result.
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

/// `notify.show` — desktop toast. `blocking_silent` so the `notify-send` subprocess runs on the
/// action pool, not the trigger pump, and its `.completed` event doesn't fan out. Also registered
/// on the GUI's in-process registry (`copad-linux/src/window.rs`). `notifier` is an argument so
/// tests can inject a `NoopNotifier`.
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
                // No Notifier on this platform (only Linux and macOS are wired); drop the toast.
                log::debug!("notify.show: no Notifier for this platform; dropping");
                Ok(serde_json::json!({ "shown": false, "reason": "no_notifier" }))
            }
        }
    });
}

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
    cron_scheduler: Option<Arc<CronScheduler>>,
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
                cron_scheduler,
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
    cron_scheduler: Option<Arc<CronScheduler>>,
    stop: Arc<AtomicBool>,
    path: &Path,
) {
    // Seed from the mtime of the initial config load, NOT a fresh sample here: that would
    // swallow an edit landing between main()'s load and this function.
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
                cron_scheduler.as_ref(),
                cfg.triggers,
            ),
            Err(e) => log::warn!(
                "config watcher: parse error on reload: {e:?}; keeping previous trigger set"
            ),
        }
    }
}

/// Hot-reload contract: always `engine.set_triggers(new)` and refresh cached_triggers. With
/// `pump_state` (host_triggers=true), mirror the GUI's `watch_config` ordering: `pump_all` on the
/// OLD subscribers first, then `reconcile_triggers`; skipping `pump_all` would discard pending
/// events the new set would match after a pattern-narrowing reload. Without it there are no bus
/// subscriptions, so only the engine's trigger list updates.
#[allow(clippy::too_many_arguments)]
fn apply_reloaded_triggers(
    engine: &Arc<TriggerEngine>,
    pump_state: Option<&Arc<Mutex<PumpState>>>,
    context: &Arc<ContextService>,
    event_bus: &Arc<copad_core::event_bus::EventBus>,
    cached_triggers: &Arc<Mutex<Vec<Trigger>>>,
    cron_scheduler: Option<&Arc<CronScheduler>>,
    new_triggers: Vec<Trigger>,
) {
    engine.set_triggers(new_triggers.clone());
    if let Some(ps) = pump_state {
        let mut ps = ps.lock().unwrap();
        ps.pump_all(context, engine);
        ps.reconcile_triggers(event_bus, &new_triggers);
    }
    if let Some(sched) = cron_scheduler {
        sched.reload(&new_triggers);
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
    // Clamp env overrides so a typo (e.g. `WORKERS=10000`) can't exhaust the OS.
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
        let (entries, total) = r1
            .replay(since_ms, kinds.as_deref(), limit)
            .map_err(internal_error)?;
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
    // Only the WINNING entry per plugin name (sorted slice, `resolve_by_name` returns the
    // last-by-dir): a losing duplicate with a command the winner lacks would otherwise leak into
    // the dispatch table.
    let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let winners: Vec<&LoadedPlugin> = plugins
        .iter()
        .rev()
        .filter(|p| seen_names.insert(p.manifest.plugin.name.as_str()))
        .collect();
    for plugin in winners.iter() {
        let plugin_name = plugin.manifest.plugin.name.clone();
        for cmd in &plugin.manifest.commands {
            // A dot would make a 4+ segment method that breaks `plugin.<name>.<cmd>` parsing.
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
        // `resolve_by_name` picks the sorted-last winner, matching `register_plugin_commands`.
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
        // `current` is per-window GUI state; the daemon reports null.
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

fn env_flag_enabled(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => false,
    }
}

/// Like `env_flag_enabled` for flags whose default is ON. `None` (unset) and unrecognised values
/// are enabled; only `0`/`false`/`no`/empty (case-insensitive, trimmed) disable, so a typo does
/// not silently turn the feature off.
///
/// Takes the value rather than the var name so tests need not mutate the process environment
/// (concurrent `set_var`/`remove_var` is unsound on glibc under parallel cargo tests).
fn env_flag_default_on(value: Option<&str>) -> bool {
    match value {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | ""
        ),
        None => true,
    }
}

/// `manifest.plugin.name` is the dispatch key for `plugin.<name>.<cmd>` and `_module.run`. The
/// list is sorted so two daemons register the same set in the same order (deterministic
/// last-write-wins). Warns about duplicates.
pub fn discover_and_sort_plugins() -> Arc<Vec<copad_core::plugin::LoadedPlugin>> {
    let plugins = copad_core::plugin::discover_sorted_plugins();
    // After sort, equal names are adjacent and ordered by dir. `register_blocking` is
    // last-write-wins, so the largest dir is active; `resolve_by_name` picks the same winner.
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
        .chain(BROWSER_RESERVED_METHODS.iter().copied())
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
            when: Some(WhenSpec {
                event_kind: kind.into(),
                payload_match: serde_json::Map::new(),
            }),
            cron: None,
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
        apply_reloaded_triggers(&engine, Some(&pump), &ctx, &bus, &cached, None, new.clone());
        assert_eq!(engine.count(), 2);
        assert_eq!(pump.lock().unwrap().trigger_subs_len(), 2);
        assert_eq!(cached.lock().unwrap().len(), 2);
    }

    #[test]
    fn apply_reloaded_triggers_without_pump_only_updates_engine() {
        let (bus, engine, _pump, ctx) = mk_pump_bundle();
        let cached = Arc::new(Mutex::new(Vec::<Trigger>::new()));
        apply_reloaded_triggers(
            &engine,
            None,
            &ctx,
            &bus,
            &cached,
            None,
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
                None,
                stop_clone,
                &path_clone,
            );
        });

        // The 2s WATCHER_TICK makes this a slow end-to-end check of the poll loop.
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
        // Blocking-silent: the subprocess runs on the action pool and no `<action>.completed`
        // event reaches the bus.
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
        assert_eq!(notifier.captured.lock().unwrap().len(), 1);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            matches!(
                completed_rx.recv_timeout(Duration::from_millis(50)),
                copad_core::event_bus::RecvOutcome::Timeout
            ),
            "silent action must not publish .completed"
        );
    }

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
        for v in ["1", "true", "yes", "on", "tru", "fasle", "garbage"] {
            assert!(env_flag_default_on(Some(v)), "value {v:?} should enable");
        }
    }
}
