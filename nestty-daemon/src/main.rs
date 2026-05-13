//! `nesttyd` binary entry.
//!
//! Step 2b: scaffolds + supervisor wiring. The daemon
//! now hosts an `ActionRegistry` populated with daemon-side built-ins
//! (`system.ping`, `system.log`) and, when `NESTTYD_HOST_PLUGINS=1` is set
//! in the environment, also spawns a `ServiceSupervisor` that activates
//! every discovered plugin manifest with `onStartup` activation.
//!
//! The env flag is a **transitional gate**: nestty-linux's GUI window also
//! constructs a supervisor today, so unconditionally hosting plugins in
//! `nesttyd` while a GUI is running would spawn each plugin twice (every
//! Discord/Slack/Calendar gateway runs in stereo, etc.). When migration
//! step 4–5 lands and the GUI becomes a socket client, the supervisor
//! moves here permanently and the flag goes away.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use nestty_core::action_registry::{ActionRegistry, internal_error};
use nestty_core::paths;
use nestty_daemon::service_supervisor::ServiceSupervisor;
use nestty_daemon::socket::{
    self, DaemonState, LEGACY_DISPATCH_METHODS, SocketPrep, new_event_bus,
};
use nestty_daemon::trigger_sink::TRIGGER_ONLY_RESERVED_METHODS;
use serde_json::json;

const ENV_HOST_PLUGINS: &str = "NESTTYD_HOST_PLUGINS";

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let socket_path: PathBuf = paths::socket_path();
    log::info!("nesttyd starting; socket={}", socket_path.display());

    match socket::prepare_socket_path(&socket_path) {
        SocketPrep::Fresh => log::debug!("socket path fresh"),
        SocketPrep::StaleCleared => log::info!("removed stale socket file"),
        SocketPrep::InUse => {
            log::error!(
                "socket {} already bound by another nesttyd; refusing to start",
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
                "path {} exists but is not a Unix socket; refusing to unlink (set NESTTY_SOCKET to a fresh path)",
                socket_path.display()
            );
            return ExitCode::from(3);
        }
    }

    // Shared host state: event bus + action registry. Cheap, no plugin
    // children spawn yet.
    let event_bus = new_event_bus();
    let actions = Arc::new(ActionRegistry::with_completion_bus(event_bus.clone()));
    register_builtins(&actions);

    // Bind the listener BEFORE activating plugins. `ServiceSupervisor::new`
    // eagerly spawns every `onStartup` service, so if we bound after and
    // hit a bind error, those children would be orphaned without ever
    // having had a daemon to talk to. Bind-first means any startup
    // failure aborts before we incur the supervised-child cost.
    let listener = match socket::bind_listener(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            log::error!("bind({}): {e}", socket_path.display());
            return ExitCode::from(1);
        }
    };

    // Plugin host — gated. See module docstring above for rationale.
    // Held in a binding scoped to `main` so RAII drops it on every exit
    // path, and we explicitly call `shutdown_all` *before* the bind socket
    // unlinks so the supervisor signals graceful exit to each child.
    //
    // The gate is strict: only `1`, `true`, or `yes` (case-insensitive)
    // count as "enable plugin hosting". Unset / empty / `0` / `false` /
    // anything else → disabled. Lax `is_ok()` would treat `=0` as enabled,
    // which contradicts the documented contract.
    let supervisor_guard: Option<Arc<ServiceSupervisor>> = if env_flag_enabled(ENV_HOST_PLUGINS) {
        Some(activate_supervisor(&actions, &event_bus))
    } else {
        log::info!(
            "plugin host disabled (set {ENV_HOST_PLUGINS}=1 to activate plugins from this daemon)"
        );
        None
    };

    let state = DaemonState::new(actions);

    log::info!("nesttyd listening on {}", socket_path.display());
    socket::run_accept_loop(listener, state);

    // Graceful shutdown of plugin children, if we started any. `Arc::drop`
    // does NOT call shutdown_all — only an explicit call does — so we
    // must invoke it before exit on the normal accept-loop-return path
    // AND any future error-exit paths we add.
    if let Some(sup) = supervisor_guard.as_ref() {
        log::info!("shutting down supervised plugins");
        sup.shutdown_all();
    }

    socket::cleanup_socket(&socket_path);
    log::info!("nesttyd shut down");
    ExitCode::SUCCESS
}

/// Daemon-side built-in actions. Currently mirrors the subset of
/// nestty-linux's `window.rs` builtins that don't touch GUI state. Plugin-
/// reachable from triggers and from `nestctl call`.
fn register_builtins(actions: &Arc<ActionRegistry>) {
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
    // Identification surface — useful for client-side feature negotiation
    // and for confirming a socket lands at nesttyd vs the legacy GUI socket.
    actions.register_silent("daemon.info", |_| {
        serde_json::to_value(serde_json::json!({
            "daemon": "nesttyd",
            "version": env!("CARGO_PKG_VERSION"),
            "host_plugins": env_flag_enabled(ENV_HOST_PLUGINS),
        }))
        .map_err(|e| internal_error(format!("daemon.info serialization failed: {e}")))
    });
}

/// Strict boolean env flag parser. Accepts `1`, `true`, `yes` (case-
/// insensitive) as enabled. Everything else — including `0`, `false`,
/// empty string, and the var being unset — disables.
fn env_flag_enabled(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes")
        }
        Err(_) => false,
    }
}

fn activate_supervisor(
    actions: &Arc<ActionRegistry>,
    event_bus: &Arc<nestty_core::event_bus::EventBus>,
) -> Arc<ServiceSupervisor> {
    let plugins = nestty_core::plugin::discover_plugins();
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
    let reserved: Vec<&str> = LEGACY_DISPATCH_METHODS
        .iter()
        .copied()
        .chain(TRIGGER_ONLY_RESERVED_METHODS.iter().copied())
        .collect();
    ServiceSupervisor::new(
        event_bus.clone(),
        actions.clone(),
        &plugins,
        env!("CARGO_PKG_VERSION"),
        &reserved,
    )
}
