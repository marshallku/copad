use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;

use copad_core::action_registry::{ActionRegistry, ActionResult, internal_error, invalid_params};
use copad_core::protocol::{Request, Response};
use copad_core::trigger::TriggerSink;
use serde_json::{Value, json};

use crate::socket::SocketCommand;

/// Trigger-only methods. Reserved by `ServiceSupervisor` so a plugin manifest can't register
/// them: the unix socket is reachable from any user process, and spawning from it would break
/// the trust boundary.
pub const TRIGGER_ONLY_RESERVED_METHODS: &[&str] = &["system.spawn"];

pub struct LiveTriggerSink {
    registry: Arc<ActionRegistry>,
    dispatch_tx: mpsc::Sender<SocketCommand>,
    reply_tx: mpsc::Sender<Response>,
}

impl LiveTriggerSink {
    pub fn new(registry: Arc<ActionRegistry>, dispatch_tx: mpsc::Sender<SocketCommand>) -> Self {
        let (reply_tx, reply_rx) = mpsc::channel::<Response>();
        // Logs fallthrough replies with ok=false. Lives until every `reply_tx` clone drops.
        std::thread::spawn(move || {
            while let Ok(resp) = reply_rx.recv() {
                if resp.ok {
                    continue;
                }
                let (code, msg) = resp
                    .error
                    .map(|e| (e.code, e.message))
                    .unwrap_or_else(|| ("unknown".into(), String::new()));
                eprintln!(
                    "[copad] trigger fallthrough id={} failed: {}: {}",
                    resp.id, code, msg
                );
            }
        });
        Self {
            registry,
            dispatch_tx,
            reply_tx,
        }
    }
}

impl LiveTriggerSink {
    /// Reachable ONLY from `[[triggers]]` config; `coctl call system.spawn` returns
    /// `unknown_method`.
    ///
    /// argv is NOT wrapped in a shell, so `{event.*}`/`{context.*}` values land as literal argv
    /// elements and cannot be re-parsed (`; rm -rf ~` in a payload stays inert). A user who opts
    /// into `["sh", "-c", ...]` owns auditing every spliced value.
    fn handle_system_spawn(params: Value) -> ActionResult {
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
        let mut child = Command::new(&program)
            .args(&argv_strs[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| {
                internal_error(format!("system.spawn: failed to exec {program:?}: {e}"))
            })?;

        let pid = child.id();
        eprintln!("[copad] trigger system.spawn pid={pid} argv={argv_strs:?}");

        // Reap on a worker so the child doesn't become a zombie; log non-zero exits.
        let argv_log = argv_strs;
        std::thread::spawn(move || match child.wait() {
            Ok(status) if !status.success() => {
                eprintln!(
                    "[copad] trigger system.spawn pid={pid} argv={argv_log:?} exited {status}"
                );
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[copad] trigger system.spawn pid={pid} wait failed: {e}");
            }
        });

        Ok(json!({ "queued": true, "pid": pid }))
    }
}

impl TriggerSink for LiveTriggerSink {
    fn dispatch_action(&self, action: &str, params: Value) -> ActionResult {
        // Intercepted before the registry check so the unix socket can't reach it.
        if action == "system.spawn" {
            return Self::handle_system_spawn(params);
        }
        if self.registry.has(action) {
            // Blocking actions return `Ok(queued)`; sync ones must keep returning their real
            // result, since TriggerEngine counts `fired` only on `Ok` and logs `Err`.
            if self.registry.is_blocking(action) {
                // The callback fires after `Ok(queued)` went to the engine, which never sees the
                // error, so log it here.
                let action_owned = action.to_string();
                self.registry.try_dispatch(
                    action,
                    params,
                    Box::new(move |result| {
                        if let Err(err) = result {
                            eprintln!(
                                "[copad] trigger registry id={} (blocking) failed: {}: {}",
                                action_owned, err.code, err.message
                            );
                        }
                    }),
                );
                return Ok(json!({ "queued": true }));
            }
            // Sync path: propagate the ActionResult so the engine counts `fired` only on Ok.
            return self
                .registry
                .try_invoke(action, params)
                .expect("registry.has() just returned true");
        }
        let cmd = SocketCommand {
            request: Request::new(
                format!("trg-{}", uuid::Uuid::new_v4()),
                action.to_string(),
                params,
            ),
            reply: self.reply_tx.clone(),
            silent_completion: false,
        };
        self.dispatch_tx
            .send(cmd)
            .map_err(|e| internal_error(format!("trigger redispatch failed: {e}")))?;
        Ok(json!({ "queued": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copad_core::action_registry::invalid_params;

    fn mk_sink_with_registry() -> (
        Arc<ActionRegistry>,
        LiveTriggerSink,
        mpsc::Receiver<SocketCommand>,
    ) {
        let registry = Arc::new(ActionRegistry::new());
        let (tx, rx) = mpsc::channel::<SocketCommand>();
        let sink = LiveTriggerSink::new(registry.clone(), tx);
        (registry, sink, rx)
    }

    #[test]
    fn sync_registry_action_returns_actual_result_not_queued() {
        let (registry, sink, _rx) = mk_sink_with_registry();
        registry.register("sync.ok", |_| Ok(json!("real-value")));
        let r = sink.dispatch_action("sync.ok", json!({})).unwrap();
        assert_eq!(r, json!("real-value"));
    }

    #[test]
    fn sync_registry_action_propagates_err_so_engine_logs_it() {
        let (registry, sink, _rx) = mk_sink_with_registry();
        registry.register("sync.fail", |_| Err(invalid_params("bad")));
        let err = sink.dispatch_action("sync.fail", json!({})).unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert_eq!(err.message, "bad");
    }

    #[test]
    fn blocking_registry_action_returns_queued_immediately() {
        let (registry, sink, _rx) = mk_sink_with_registry();
        registry.register_blocking("slow.ok", |_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok(json!("eventual"))
        });
        let start = std::time::Instant::now();
        let r = sink.dispatch_action("slow.ok", json!({})).unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_millis(40),
            "dispatch_action returned in {:?}, expected <40ms",
            start.elapsed()
        );
        assert_eq!(r, json!({"queued": true}));
    }

    #[test]
    fn unknown_action_falls_through_to_socket_dispatch() {
        let (_registry, sink, rx) = mk_sink_with_registry();
        let r = sink
            .dispatch_action("legacy.thing", json!({"x": 1}))
            .unwrap();
        assert_eq!(r, json!({"queued": true}));
        let cmd = rx.try_recv().expect("expected one queued legacy command");
        assert_eq!(cmd.request.method, "legacy.thing");
        assert_eq!(cmd.request.params, json!({"x": 1}));
    }

    #[test]
    fn system_spawn_rejects_missing_argv() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink.dispatch_action("system.spawn", json!({})).unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(err.message.contains("argv"));
    }

    #[test]
    fn system_spawn_rejects_non_array_argv() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink
            .dispatch_action("system.spawn", json!({ "argv": "true" }))
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
    }

    #[test]
    fn system_spawn_rejects_empty_argv() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink
            .dispatch_action("system.spawn", json!({ "argv": [] }))
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(err.message.contains("must not be empty"));
    }

    #[test]
    fn system_spawn_rejects_empty_program_name() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink
            .dispatch_action("system.spawn", json!({ "argv": ["", "--flag"] }))
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(err.message.contains("program name"));
    }

    #[test]
    fn system_spawn_rejects_non_string_argv_element() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink
            .dispatch_action("system.spawn", json!({ "argv": ["true", 123] }))
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(err.message.contains("strings"));
    }

    #[test]
    fn system_spawn_executes_program_and_reports_pid() {
        let (_r, sink, rx) = mk_sink_with_registry();
        // Asserts the spawn path returns Ok with a pid and does NOT fall through to
        // socket::dispatch. macOS has no `/bin/true`, so probe both paths.
        let true_path = if std::path::Path::new("/bin/true").exists() {
            "/bin/true"
        } else {
            "/usr/bin/true"
        };
        let r = sink
            .dispatch_action("system.spawn", json!({ "argv": [true_path] }))
            .unwrap();
        assert_eq!(r["queued"], json!(true));
        assert!(r["pid"].as_u64().unwrap() > 0);
        assert!(
            rx.try_recv().is_err(),
            "system.spawn must not fall through to socket dispatch"
        );
    }

    #[test]
    fn system_spawn_reports_exec_failure() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink
            .dispatch_action(
                "system.spawn",
                json!({ "argv": ["/nonexistent/binary/that/does/not/exist"] }),
            )
            .unwrap_err();
        assert_eq!(err.code, "internal_error");
        assert!(err.message.contains("failed to exec"));
    }

    #[test]
    fn system_spawn_is_not_registered_in_action_registry() {
        // Trust boundary: `system.spawn` must not be in the registry, or `coctl call system.spawn`
        // (registry-first via socket::dispatch) could spawn from any process holding COPAD_SOCKET.
        let (registry, _sink, _rx) = mk_sink_with_registry();
        assert!(
            !registry.has("system.spawn"),
            "system.spawn must remain trigger-only — not in ActionRegistry"
        );
    }
}
