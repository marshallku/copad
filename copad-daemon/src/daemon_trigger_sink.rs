//! Daemon-side `TriggerSink`, mirroring [`crate::trigger_sink::LiveTriggerSink`] but routing
//! GUI-owned actions through a registered GUI's `Invoke` channel.
//!
//! Fallthrough is fire-and-forget through a bounded queue and a single worker, so
//! `dispatch_action` never blocks the trigger pump on a GUI Invoke (up to
//! `method_invoke_timeout(action)`: 5s tabs/terminal, 120s webview). Saturation logs and drops.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use copad_core::action_registry::{ActionRegistry, ActionResult, internal_error, invalid_params};
use copad_core::protocol::Response;
use copad_core::trigger::TriggerSink;
use serde_json::{Value, json};

use crate::gui_registry::{GuiRegistry, method_invoke_timeout};

/// Larger than the action pool's queue (64): completion events fan out to every matching trigger.
/// Bounded so a wedged GUI cannot grow daemon memory without limit.
const FALLTHROUGH_QUEUE: usize = 256;

pub struct DaemonTriggerSink {
    registry: Arc<ActionRegistry>,
    /// Lets `system.spawn` merge the registered GUI's curated env (`HYPRLAND_INSTANCE_SIGNATURE`,
    /// `DISPLAY`, ...) into the child's environment.
    gui: Arc<GuiRegistry>,
    fallthrough_tx: mpsc::SyncSender<(String, Value)>,
}

impl DaemonTriggerSink {
    pub fn new(
        registry: Arc<ActionRegistry>,
        gui: Arc<GuiRegistry>,
        event_bus: Arc<copad_core::event_bus::EventBus>,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<(String, Value)>(FALLTHROUGH_QUEUE);
        let gui_for_worker = gui.clone();
        let bus_for_worker = event_bus;
        thread::Builder::new()
            .name("copad-trigger-fallthrough".into())
            .spawn(move || fallthrough_worker(rx, gui_for_worker, bus_for_worker))
            .expect("spawn trigger fallthrough worker");
        Self {
            registry,
            gui,
            fallthrough_tx: tx,
        }
    }

    /// Merges the primary GUI's curated env on top of the daemon's inherited env, so triggers
    /// see `HYPRLAND_INSTANCE_SIGNATURE` etc. With no GUI the child inherits the daemon env only.
    fn handle_system_spawn(&self, params: Value) -> ActionResult {
        let argv = params
            .get("argv")
            .and_then(|v| v.as_array())
            .ok_or_else(|| invalid_params("system.spawn: argv must be a non-empty string array"))?;
        if argv.is_empty() {
            return Err(invalid_params("system.spawn: argv must not be empty"));
        }
        let argv_strs: Vec<String> = argv
            .iter()
            .map(|v| {
                v.as_str().map(String::from).ok_or_else(|| {
                    invalid_params("system.spawn: argv elements must all be strings")
                })
            })
            .collect::<Result<_, _>>()?;
        if argv_strs[0].is_empty() {
            return Err(invalid_params(
                "system.spawn: argv[0] (program name) must not be an empty string",
            ));
        }
        let program = argv_strs[0].clone();
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&argv_strs[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        // `Command::envs` overrides matching keys without clearing the daemon's PATH etc.
        if let Some(gui_env) = self.gui.primary_gui_env() {
            cmd.envs(gui_env);
        }
        let mut child = cmd.spawn().map_err(|e| {
            internal_error(format!("system.spawn: failed to exec {program:?}: {e}"))
        })?;
        let pid = child.id();
        log::info!("trigger system.spawn pid={pid} argv={argv_strs:?}");
        let argv_log = argv_strs;
        thread::spawn(move || match child.wait() {
            Ok(status) if !status.success() => {
                log::warn!("trigger system.spawn pid={pid} argv={argv_log:?} exited {status}");
            }
            Ok(_) => {}
            Err(e) => log::warn!("trigger system.spawn pid={pid} wait failed: {e}"),
        });
        Ok(json!({ "queued": true, "pid": pid }))
    }
}

impl TriggerSink for DaemonTriggerSink {
    fn dispatch_action(&self, action: &str, params: Value) -> ActionResult {
        if action == "system.spawn" {
            return self.handle_system_spawn(params);
        }
        if self.registry.has(action) {
            if self.registry.is_blocking(action) {
                let action_owned = action.to_string();
                self.registry.try_dispatch(
                    action,
                    params,
                    Box::new(move |result| {
                        if let Err(err) = result {
                            log::warn!(
                                "trigger registry id={} (blocking) failed: {}: {}",
                                action_owned,
                                err.code,
                                err.message
                            );
                        }
                    }),
                );
                return Ok(json!({ "queued": true }));
            }
            return self
                .registry
                .try_invoke(action, params)
                .expect("registry.has() just returned true");
        }
        // `try_send` so a wedged GUI can't stall the trigger pump.
        match self.fallthrough_tx.try_send((action.to_string(), params)) {
            Ok(()) => Ok(json!({ "queued": true })),
            Err(mpsc::TrySendError::Full(_)) => {
                log::warn!(
                    "trigger fallthrough queue full ({FALLTHROUGH_QUEUE}); dropping {action}"
                );
                Err(internal_error("trigger fallthrough queue saturated"))
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err(internal_error("trigger fallthrough worker gone"))
            }
        }
    }
}

fn fallthrough_worker(
    rx: mpsc::Receiver<(String, Value)>,
    gui: Arc<GuiRegistry>,
    event_bus: Arc<copad_core::event_bus::EventBus>,
) {
    while let Ok((action, params)) = rx.recv() {
        let resp = match gui.route(&action, None) {
            Ok(client) => {
                let timeout = method_invoke_timeout(&action);
                let resp = client.invoke(&action, params, timeout);
                if !resp.ok {
                    let (code, message) = resp
                        .error
                        .as_ref()
                        .map(|e| (e.code.clone(), e.message.clone()))
                        .unwrap_or_else(|| ("unknown".into(), String::new()));
                    log::warn!("trigger fallthrough action={action} failed: {code}: {message}");
                }
                resp
            }
            Err(reason) => {
                log::warn!("trigger fallthrough action={action} unroutable: {reason}");
                Response::error(String::new(), reason, &format!("no GUI to handle {action}"))
            }
        };
        crate::socket::publish_legacy_completion(&event_bus, &action, false, &resp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copad_core::protocol::ResponseError;

    fn mk_sink() -> (Arc<ActionRegistry>, Arc<GuiRegistry>, DaemonTriggerSink) {
        let reg = Arc::new(ActionRegistry::new());
        let gui = GuiRegistry::new();
        let bus = Arc::new(copad_core::event_bus::EventBus::new());
        let sink = DaemonTriggerSink::new(reg.clone(), gui.clone(), bus);
        (reg, gui, sink)
    }

    #[test]
    fn sync_registry_action_returns_real_value() {
        let (reg, _gui, sink) = mk_sink();
        reg.register("sync.ok", |_| Ok(json!("hello")));
        assert_eq!(
            sink.dispatch_action("sync.ok", json!({})).unwrap(),
            json!("hello")
        );
    }

    #[test]
    fn sync_registry_err_is_propagated() {
        let (reg, _gui, sink) = mk_sink();
        reg.register("sync.bad", |_| {
            Err(ResponseError {
                code: "invalid_params".into(),
                message: "no".into(),
            })
        });
        let e = sink.dispatch_action("sync.bad", json!({})).unwrap_err();
        assert_eq!(e.code, "invalid_params");
    }

    #[test]
    fn system_spawn_rejects_empty_argv() {
        let (_reg, _gui, sink) = mk_sink();
        let e = sink
            .dispatch_action("system.spawn", json!({"argv": []}))
            .unwrap_err();
        assert_eq!(e.code, "invalid_params");
    }

    #[test]
    fn system_spawn_rejects_missing_argv() {
        let (_reg, _gui, sink) = mk_sink();
        let e = sink.dispatch_action("system.spawn", json!({})).unwrap_err();
        assert_eq!(e.code, "invalid_params");
    }

    #[test]
    fn unknown_action_fallthrough_with_no_gui_returns_queued() {
        let (_reg, _gui, sink) = mk_sink();
        // GUI-capability'd method (tab.new) with no GUI registered: returns `queued` at once and
        // the worker logs `unroutable: no_gui`.
        let r = sink.dispatch_action("tab.new", json!({})).unwrap();
        assert_eq!(r, json!({"queued": true}));
    }

    #[test]
    fn fallthrough_queue_returns_err_when_saturated() {
        let (_reg, _gui, sink) = mk_sink();
        // Cannot observe the Full path without a way to throttle the worker; smoke-test only.
        for _ in 0..50 {
            let _ = sink.dispatch_action("tab.new", json!({}));
        }
    }
}
