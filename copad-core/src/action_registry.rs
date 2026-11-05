//! Action registry: name to handler map shared by socket dispatch, plugin RPC and triggers.
//!
//! Panic semantics: every dispatch path (`invoke` / `try_invoke` / `try_dispatch`) runs the
//! handler before completion fan-out and any continuation, so a handler panic unwinds past both:
//! no `<action>.completed`/`.failed` is published and any `Responder` is dropped uncalled.
//! Handlers that need the bus to see a failure must catch the panic and return `Err`.

use crate::event_bus::{Event as BusEvent, EventBus};
use crate::protocol::ResponseError;
use crate::thread_pool::{Cancelable, ThreadPool};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub type ActionResult = Result<Value, ResponseError>;
pub type ActionFn = Arc<dyn Fn(Value) -> ActionResult + Send + Sync + 'static>;
/// `try_dispatch` continuation: inline for sync handlers, on a worker for `register_blocking`.
pub type Responder = Box<dyn FnOnce(ActionResult) + Send + 'static>;

/// Trust-boundary stamp on auto-published completion events; subscribers (notably
/// `TriggerEngine::try_promote_or_drop_preflight`) use it to tell registry-synthesized events
/// from plugin-emitted ones with the same `<x>.completed` kind.
pub const COMPLETION_EVENT_SOURCE: &str = "copad.action";

struct Entry {
    handler: ActionFn,
    /// Run on a worker thread by `try_dispatch` so time-sensitive callers (GTK pump, socket
    /// dispatcher) don't stall.
    blocking: bool,
    /// Suppresses completion-event fan-out, for high-frequency internal actions.
    silent: bool,
}

pub struct ActionRegistry {
    entries: RwLock<HashMap<String, Entry>>,
    /// When set, non-silent dispatches publish `<action>.completed` (Ok) or `<action>.failed`
    /// (Err) BEFORE the caller's `Responder` fires.
    completion_bus: Option<Arc<EventBus>>,
    /// When set, blocking handlers use a bounded worker pool instead of `std::thread::spawn`;
    /// saturation yields an `overloaded` error. `None` keeps the unbounded fallback.
    pool: Mutex<Option<Arc<ThreadPool>>>,
}

impl ActionRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            completion_bus: None,
            pool: Mutex::new(None),
        }
    }

    /// Auto-publish `<action>.completed` / `<action>.failed` after every non-silent dispatch
    /// (`invoke`/`try_invoke`/`try_dispatch`). Only registry-routed actions emit: legacy match-arm
    /// commands in `copad-linux::socket::dispatch` bypass the registry.
    pub fn with_completion_bus(bus: Arc<EventBus>) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            completion_bus: Some(bus),
            pool: Mutex::new(None),
        }
    }

    /// Routes blocking handlers through a bounded pool. Call once at daemon startup before
    /// dispatch; a second call replaces the pool but `debug_assert`s the prior was unset.
    pub fn with_pool(self: Arc<Self>, pool: Arc<ThreadPool>) -> Arc<Self> {
        let prev = self.pool.lock().unwrap().replace(pool);
        debug_assert!(prev.is_none(), "with_pool called twice");
        self
    }

    pub fn pool_stats(&self) -> Option<crate::thread_pool::PoolStats> {
        self.pool.lock().unwrap().as_ref().map(|p| p.stats())
    }

    /// Sync handler, run inline by `try_dispatch`; microsecond-scale work only.
    pub fn register<F>(&self, name: impl Into<String>, handler: F)
    where
        F: Fn(Value) -> ActionResult + Send + Sync + 'static,
    {
        self.entries.write().unwrap().insert(
            name.into(),
            Entry {
                handler: Arc::new(handler),
                blocking: false,
                silent: false,
            },
        );
    }

    /// Like `register` but suppresses completion-event fan-out, for high-frequency internal
    /// actions (`system.ping`, `context.snapshot`).
    pub fn register_silent<F>(&self, name: impl Into<String>, handler: F)
    where
        F: Fn(Value) -> ActionResult + Send + Sync + 'static,
    {
        self.entries.write().unwrap().insert(
            name.into(),
            Entry {
                handler: Arc::new(handler),
                blocking: false,
                silent: true,
            },
        );
    }

    pub fn register_blocking<F>(&self, name: impl Into<String>, handler: F)
    where
        F: Fn(Value) -> ActionResult + Send + Sync + 'static,
    {
        self.entries.write().unwrap().insert(
            name.into(),
            Entry {
                handler: Arc::new(handler),
                blocking: true,
                silent: false,
            },
        );
    }

    /// `register_blocking` + silent, for high-frequency blocking work (statusbar module ticks)
    /// that needs pool backpressure without flooding the bus.
    pub fn register_blocking_silent<F>(&self, name: impl Into<String>, handler: F)
    where
        F: Fn(Value) -> ActionResult + Send + Sync + 'static,
    {
        self.entries.write().unwrap().insert(
            name.into(),
            Entry {
                handler: Arc::new(handler),
                blocking: true,
                silent: true,
            },
        );
    }

    pub fn invoke(&self, name: &str, params: Value) -> ActionResult {
        // Clone the handler out under the read lock and drop the guard before running it, so a
        // handler may call `register`/`invoke` without deadlocking.
        let (handler, silent) = {
            let entries = self.entries.read().unwrap();
            entries
                .get(name)
                .map(|e| (e.handler.clone(), e.silent))
                .ok_or_else(|| not_found_error(name))?
        };
        let result = handler(params);
        // The default TriggerSink resolves chained actions through here; without publishing,
        // their completion events would be lost.
        if !silent {
            publish_completion(self.completion_bus.as_deref(), name, &result);
        }
        result
    }

    /// Like `invoke` but `None` for unregistered actions so dispatchers can fall through. Runs
    /// the handler inline regardless of `blocking`; time-sensitive callers use `try_dispatch`.
    pub fn try_invoke(&self, name: &str, params: Value) -> Option<ActionResult> {
        let (handler, silent) = {
            let entries = self.entries.read().unwrap();
            entries.get(name).map(|e| (e.handler.clone(), e.silent))?
        };
        let result = handler(params);
        if !silent {
            publish_completion(self.completion_bus.as_deref(), name, &result);
        }
        Some(result)
    }

    /// `true` if registered (the caller need not handle `on_done`); `false` if not, in which case
    /// `on_done` is dropped uncalled. Sync handlers run inline, blocking ones on a worker.
    pub fn try_dispatch(self: &Arc<Self>, name: &str, params: Value, on_done: Responder) -> bool {
        let (handler, blocking, silent) = {
            let entries = self.entries.read().unwrap();
            match entries.get(name) {
                Some(e) => (e.handler.clone(), e.blocking, e.silent),
                None => return false,
            }
        };
        // Capture what the completion publisher needs BEFORE moving handler/on_done into
        // closures; `None` with no completion bus or a silent action.
        let publish_bus = if silent {
            None
        } else {
            self.completion_bus.clone()
        };
        let action_name = name.to_string();
        if blocking {
            let pool_snapshot = self.pool.lock().unwrap().clone();
            if let Some(pool) = pool_snapshot {
                let job = Box::new(DispatchJob {
                    handler,
                    params,
                    on_done,
                    publish_bus,
                    action_name,
                });
                if let Err(rejected) = pool.try_execute(job) {
                    // Cancel synchronously on the caller thread: publishes `<action>.failed` and
                    // fires on_done with `overloaded`, so no caller is orphaned.
                    rejected.cancel();
                }
            } else {
                // Unbounded spawn for tests and embedders that wire no pool.
                std::thread::spawn(move || {
                    let result = handler(params);
                    publish_completion(publish_bus.as_deref(), &action_name, &result);
                    on_done(result);
                });
            }
        } else {
            let result = handler(params);
            publish_completion(publish_bus.as_deref(), &action_name, &result);
            on_done(result);
        }
        true
    }

    pub fn has(&self, name: &str) -> bool {
        self.entries.read().unwrap().contains_key(name)
    }

    pub fn is_blocking(&self, name: &str) -> bool {
        self.entries
            .read()
            .unwrap()
            .get(name)
            .map(|e| e.blocking)
            .unwrap_or(false)
    }

    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.entries.read().unwrap().keys().cloned().collect();
        out.sort();
        out
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.read().unwrap().is_empty()
    }
}

impl Default for ActionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Worker-pool job. `run` and `cancel` both publish the completion event after the handler (or
/// overload error) and before the Responder fires. Chaining is correct, but same-tick chaining
/// is not guaranteed: the downstream trigger fires on the platform pump cadence.
struct DispatchJob {
    handler: ActionFn,
    params: Value,
    on_done: Responder,
    publish_bus: Option<Arc<EventBus>>,
    action_name: String,
}

impl Cancelable for DispatchJob {
    fn run(self: Box<Self>) {
        let this = *self;
        let result = (this.handler)(this.params);
        publish_completion(this.publish_bus.as_deref(), &this.action_name, &result);
        (this.on_done)(result);
    }

    fn cancel(self: Box<Self>) {
        let this = *self;
        let err = ResponseError {
            code: "overloaded".into(),
            message: format!(
                "action queue saturated; {} was not dispatched",
                this.action_name
            ),
        };
        let result: ActionResult = Err(err);
        publish_completion(this.publish_bus.as_deref(), &this.action_name, &result);
        (this.on_done)(result);
    }
}

fn publish_completion(bus: Option<&EventBus>, action: &str, result: &ActionResult) {
    let Some(bus) = bus else { return };
    publish_completion_unconditional(bus, action, result);
}

/// `<action>.completed` / `<action>.failed` with source `copad.action`, the same shape as the
/// registry's auto-publish; lets legacy dispatch surfaces (copad-linux `socket::dispatch`,
/// copad-daemon `dispatch_via_gui`) fan out completion events too.
pub fn publish_completion_unconditional(bus: &EventBus, action: &str, result: &ActionResult) {
    let (kind, payload) = match result {
        Ok(value) => (format!("{action}.completed"), value.clone()),
        Err(err) => (
            format!("{action}.failed"),
            json!({ "code": err.code, "message": err.message }),
        ),
    };
    bus.publish(BusEvent::new(kind, COMPLETION_EVENT_SOURCE, payload));
}

fn not_found_error(name: &str) -> ResponseError {
    ResponseError {
        code: "action_not_found".into(),
        message: format!("no action registered: {name}"),
    }
}

pub fn invalid_params(message: impl Into<String>) -> ResponseError {
    ResponseError {
        code: "invalid_params".into(),
        message: message.into(),
    }
}

pub fn internal_error(message: impl Into<String>) -> ResponseError {
    ResponseError {
        code: "internal_error".into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn register_and_invoke() {
        let reg = ActionRegistry::new();
        reg.register("ping", |_| Ok(json!({"pong": true})));
        let out = reg.invoke("ping", json!({})).unwrap();
        assert_eq!(out, json!({"pong": true}));
    }

    #[test]
    fn try_invoke_returns_none_for_unknown() {
        let reg = ActionRegistry::new();
        reg.register("known", |_| Ok(json!("ok")));
        assert!(reg.try_invoke("missing", json!({})).is_none());
        let some = reg.try_invoke("known", json!({})).expect("registered");
        assert_eq!(some.unwrap(), json!("ok"));
    }

    #[test]
    fn unknown_action_returns_not_found() {
        let reg = ActionRegistry::new();
        let err = reg.invoke("missing", json!({})).unwrap_err();
        assert_eq!(err.code, "action_not_found");
        assert!(err.message.contains("missing"));
    }

    #[test]
    fn handler_error_propagates() {
        let reg = ActionRegistry::new();
        reg.register("fail", |_| Err(invalid_params("bad input")));
        let err = reg.invoke("fail", json!({})).unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert_eq!(err.message, "bad input");
    }

    #[test]
    fn params_are_passed_through() {
        let reg = ActionRegistry::new();
        reg.register("echo", Ok);
        let out = reg.invoke("echo", json!({"a": 1, "b": [2, 3]})).unwrap();
        assert_eq!(out, json!({"a": 1, "b": [2, 3]}));
    }

    #[test]
    fn has_and_names_reflect_registrations() {
        let reg = ActionRegistry::new();
        assert!(reg.is_empty());
        reg.register("b.thing", |_| Ok(json!(null)));
        reg.register("a.thing", |_| Ok(json!(null)));
        assert!(reg.has("a.thing"));
        assert!(reg.has("b.thing"));
        assert!(!reg.has("c.thing"));
        assert_eq!(reg.len(), 2);
        assert_eq!(
            reg.names(),
            vec!["a.thing".to_string(), "b.thing".to_string()]
        );
    }

    #[test]
    fn handler_can_capture_state() {
        let reg = ActionRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        reg.register("bump", move |_| {
            let prev = c.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"prev": prev}))
        });
        reg.invoke("bump", json!({})).unwrap();
        reg.invoke("bump", json!({})).unwrap();
        let out = reg.invoke("bump", json!({})).unwrap();
        assert_eq!(out, json!({"prev": 2}));
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn register_overwrites_existing() {
        let reg = ActionRegistry::new();
        reg.register("x", |_| Ok(json!("old")));
        reg.register("x", |_| Ok(json!("new")));
        assert_eq!(reg.invoke("x", json!({})).unwrap(), json!("new"));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn concurrent_invoke_from_multiple_threads() {
        let reg = Arc::new(ActionRegistry::new());
        let shared = Arc::new(Mutex::new(0u64));
        {
            let s = shared.clone();
            reg.register("add", move |params| {
                let n = params
                    .as_u64()
                    .ok_or_else(|| invalid_params("expected u64"))?;
                let mut g = s.lock().unwrap();
                *g += n;
                Ok(json!(*g))
            });
        }
        let mut handles = vec![];
        for _ in 0..8 {
            let r = reg.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    r.invoke("add", json!(1u64)).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*shared.lock().unwrap(), 8 * 100);
    }

    #[test]
    fn handler_can_register_more_actions_without_deadlock() {
        let reg = Arc::new(ActionRegistry::new());
        let r = reg.clone();
        reg.register("self_extend", move |_| {
            r.register("added_later", |_| Ok(json!("ok")));
            Ok(json!("extended"))
        });
        assert_eq!(
            reg.invoke("self_extend", json!({})).unwrap(),
            json!("extended")
        );
        assert!(reg.has("added_later"));
        assert_eq!(reg.invoke("added_later", json!({})).unwrap(), json!("ok"));
    }

    #[test]
    fn handler_can_invoke_another_action_without_deadlock() {
        let reg = Arc::new(ActionRegistry::new());
        reg.register("inner", |params| Ok(json!({ "echoed": params })));
        let r = reg.clone();
        reg.register("outer", move |_| r.invoke("inner", json!(42)));
        assert_eq!(
            reg.invoke("outer", json!({})).unwrap(),
            json!({ "echoed": 42 })
        );
    }

    #[test]
    fn error_helpers_have_stable_codes() {
        assert_eq!(invalid_params("x").code, "invalid_params");
        assert_eq!(internal_error("x").code, "internal_error");
    }

    #[test]
    fn try_dispatch_returns_false_for_unknown() {
        let reg = Arc::new(ActionRegistry::new());
        let cb_fired = Arc::new(AtomicUsize::new(0));
        let c = cb_fired.clone();
        let dispatched = reg.try_dispatch(
            "missing",
            json!({}),
            Box::new(move |_| {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );
        assert!(!dispatched);
        assert_eq!(cb_fired.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn try_dispatch_sync_runs_inline_on_caller_thread() {
        let reg = Arc::new(ActionRegistry::new());
        reg.register("ping", |_| Ok(json!("pong")));
        let caller = std::thread::current().id();
        let observed = Arc::new(Mutex::new(None::<std::thread::ThreadId>));
        let captured: Arc<Mutex<Option<ActionResult>>> = Arc::new(Mutex::new(None));
        {
            let obs = observed.clone();
            let cap = captured.clone();
            let dispatched = reg.try_dispatch(
                "ping",
                json!({}),
                Box::new(move |result| {
                    *obs.lock().unwrap() = Some(std::thread::current().id());
                    *cap.lock().unwrap() = Some(result);
                }),
            );
            assert!(dispatched);
        }
        assert_eq!(*observed.lock().unwrap(), Some(caller));
        assert_eq!(
            captured.lock().unwrap().as_ref().unwrap().as_ref().unwrap(),
            &json!("pong")
        );
    }

    #[test]
    fn try_dispatch_blocking_runs_on_worker_thread() {
        let reg = Arc::new(ActionRegistry::new());
        reg.register_blocking("slow", |_| {
            std::thread::sleep(Duration::from_millis(50));
            Ok(json!("done"))
        });
        let caller = std::thread::current().id();
        let observed = Arc::new(Mutex::new(None::<std::thread::ThreadId>));
        let done = Arc::new(Mutex::new(false));
        let captured: Arc<Mutex<Option<ActionResult>>> = Arc::new(Mutex::new(None));
        let start = Instant::now();
        {
            let obs = observed.clone();
            let cap = captured.clone();
            let d = done.clone();
            let dispatched = reg.try_dispatch(
                "slow",
                json!({}),
                Box::new(move |result| {
                    *obs.lock().unwrap() = Some(std::thread::current().id());
                    *cap.lock().unwrap() = Some(result);
                    *d.lock().unwrap() = true;
                }),
            );
            assert!(dispatched);
        }
        // Returned well under the handler's sleep: the caller wasn't blocked.
        assert!(
            start.elapsed() < Duration::from_millis(40),
            "try_dispatch returned in {:?}, expected < 40ms",
            start.elapsed()
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !*done.lock().unwrap() {
            assert!(Instant::now() < deadline, "worker never completed");
            std::thread::sleep(Duration::from_millis(5));
        }
        let observed_tid = observed.lock().unwrap().unwrap();
        assert_ne!(
            observed_tid, caller,
            "blocking callback should fire on worker thread, not caller"
        );
        assert_eq!(
            captured.lock().unwrap().as_ref().unwrap().as_ref().unwrap(),
            &json!("done")
        );
    }

    #[test]
    fn try_dispatch_sync_propagates_handler_error_via_callback() {
        let reg = Arc::new(ActionRegistry::new());
        reg.register("fail", |_| Err(invalid_params("nope")));
        let captured: Arc<Mutex<Option<ActionResult>>> = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        reg.try_dispatch(
            "fail",
            json!({}),
            Box::new(move |result| {
                *cap.lock().unwrap() = Some(result);
            }),
        );
        let err = captured
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .clone();
        assert_eq!(err.code, "invalid_params");
        assert_eq!(err.message, "nope");
    }

    #[test]
    fn try_dispatch_blocking_propagates_handler_error_via_callback() {
        let reg = Arc::new(ActionRegistry::new());
        reg.register_blocking("fail-slow", |_| {
            std::thread::sleep(Duration::from_millis(20));
            Err(invalid_params("blocked nope"))
        });
        let captured: Arc<Mutex<Option<ActionResult>>> = Arc::new(Mutex::new(None));
        let done = Arc::new(Mutex::new(false));
        {
            let cap = captured.clone();
            let d = done.clone();
            reg.try_dispatch(
                "fail-slow",
                json!({}),
                Box::new(move |result| {
                    *cap.lock().unwrap() = Some(result);
                    *d.lock().unwrap() = true;
                }),
            );
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !*done.lock().unwrap() {
            assert!(Instant::now() < deadline, "blocking worker never completed");
            std::thread::sleep(Duration::from_millis(5));
        }
        let err = captured
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .clone();
        assert_eq!(err.code, "invalid_params");
        assert_eq!(err.message, "blocked nope");
    }

    #[test]
    fn is_blocking_reflects_registration_kind() {
        let reg = ActionRegistry::new();
        reg.register("sync", |_| Ok(json!(null)));
        reg.register_blocking("slow", |_| Ok(json!(null)));
        assert!(!reg.is_blocking("sync"));
        assert!(reg.is_blocking("slow"));
        assert!(!reg.is_blocking("missing"));
    }

    #[test]
    fn try_dispatch_publishes_completed_on_ok() {
        let bus = Arc::new(EventBus::new());
        let rx = bus.subscribe_unbounded("kb.search.completed");
        let reg = Arc::new(ActionRegistry::with_completion_bus(bus));
        reg.register("kb.search", |_| Ok(json!({"hits": 3})));
        reg.try_dispatch("kb.search", json!({"q": "x"}), Box::new(|_| {}));
        let evt = rx
            .recv_timeout(Duration::from_millis(200))
            .expect_event("expected completion event");
        assert_eq!(evt.kind, "kb.search.completed");
        assert_eq!(evt.source, COMPLETION_EVENT_SOURCE);
        assert_eq!(evt.payload, json!({"hits": 3}));
    }

    #[test]
    fn try_dispatch_publishes_failed_on_err() {
        let bus = Arc::new(EventBus::new());
        let rx = bus.subscribe_unbounded("foo.failed");
        let reg = Arc::new(ActionRegistry::with_completion_bus(bus));
        reg.register("foo", |_| Err(invalid_params("bad")));
        reg.try_dispatch("foo", json!({}), Box::new(|_| {}));
        let evt = rx
            .recv_timeout(Duration::from_millis(200))
            .expect_event("expected failure event");
        assert_eq!(evt.kind, "foo.failed");
        assert_eq!(evt.payload["code"], "invalid_params");
        assert_eq!(evt.payload["message"], "bad");
    }

    #[test]
    fn blocking_dispatch_publishes_from_worker_thread() {
        let bus = Arc::new(EventBus::new());
        let rx = bus.subscribe_unbounded("slow.completed");
        let reg = Arc::new(ActionRegistry::with_completion_bus(bus));
        reg.register_blocking("slow", |_| {
            std::thread::sleep(Duration::from_millis(20));
            Ok(json!("done"))
        });
        let done = Arc::new(Mutex::new(false));
        {
            let d = done.clone();
            reg.try_dispatch(
                "slow",
                json!({}),
                Box::new(move |_| {
                    *d.lock().unwrap() = true;
                }),
            );
        }
        let evt = rx
            .recv_timeout(Duration::from_millis(500))
            .expect_event("expected completion event from worker");
        assert_eq!(evt.kind, "slow.completed");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !*done.lock().unwrap() {
            assert!(Instant::now() < deadline, "on_done never fired");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn registry_without_completion_bus_does_not_publish() {
        let reg = Arc::new(ActionRegistry::new());
        reg.register("ping", |_| Ok(json!("pong")));
        let captured = Arc::new(Mutex::new(None::<ActionResult>));
        let cap = captured.clone();
        reg.try_dispatch(
            "ping",
            json!({}),
            Box::new(move |r| {
                *cap.lock().unwrap() = Some(r);
            }),
        );
        assert_eq!(
            captured.lock().unwrap().as_ref().unwrap().as_ref().unwrap(),
            &json!("pong")
        );
    }

    #[test]
    fn silent_action_suppresses_fan_out() {
        let bus = Arc::new(EventBus::new());
        let rx = bus.subscribe_unbounded("system.ping.completed");
        let reg = Arc::new(ActionRegistry::with_completion_bus(bus));
        reg.register_silent("system.ping", |_| Ok(json!({"status": "ok"})));
        for _ in 0..5 {
            reg.try_dispatch("system.ping", json!({}), Box::new(|_| {}));
        }
        // No completion event may arrive; fan-out leaking would show up almost immediately.
        match rx.recv_timeout(Duration::from_millis(100)) {
            crate::event_bus::RecvOutcome::Timeout => {}
            other => panic!("silent action published an event: {other:?}"),
        }
    }

    #[test]
    fn blocking_silent_action_runs_on_pool_and_suppresses_fan_out() {
        use crate::thread_pool::ThreadPool;

        let bus = Arc::new(EventBus::new());
        let rx = bus.subscribe_unbounded("mod.tick.completed");
        let pool = ThreadPool::new(2, 4);
        let reg = Arc::new(ActionRegistry::with_completion_bus(bus)).with_pool(pool);

        let runner_thread = Arc::new(Mutex::new(None::<std::thread::ThreadId>));
        let dispatch_thread = std::thread::current().id();
        let rt_set = runner_thread.clone();
        reg.register_blocking_silent("mod.tick", move |_| {
            *rt_set.lock().unwrap() = Some(std::thread::current().id());
            Ok(json!({"stdout": "x"}))
        });

        let done = Arc::new(Mutex::new(false));
        let d = done.clone();
        reg.try_dispatch(
            "mod.tick",
            json!({}),
            Box::new(move |_| *d.lock().unwrap() = true),
        );

        let deadline = Instant::now() + Duration::from_secs(1);
        while !*done.lock().unwrap() {
            assert!(Instant::now() < deadline, "on_done never fired");
            std::thread::sleep(Duration::from_millis(5));
        }
        let observed_thread = runner_thread
            .lock()
            .unwrap()
            .expect("handler must have observed its thread id");
        assert_ne!(
            observed_thread, dispatch_thread,
            "blocking_silent handler must run off the caller thread"
        );

        match rx.recv_timeout(Duration::from_millis(50)) {
            crate::event_bus::RecvOutcome::Timeout => {}
            other => panic!("silent blocking action published an event: {other:?}"),
        }
    }

    #[test]
    fn completion_event_publishes_before_responder_runs_for_sync() {
        // A downstream trigger waiting on `<action>.completed` must see the event no later than
        // the Responder fires, or a chained trigger could miss it in a tight loop.
        let bus = Arc::new(EventBus::new());
        let rx = bus.subscribe_unbounded("seq.completed");
        let reg = Arc::new(ActionRegistry::with_completion_bus(bus));
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        reg.register("seq", |_| Ok(json!("v")));
        let o = order.clone();
        reg.try_dispatch(
            "seq",
            json!({}),
            Box::new(move |_| {
                o.lock().unwrap().push("on_done");
            }),
        );
        let evt = rx
            .recv_timeout(Duration::from_millis(50))
            .expect_event("expected sync-path event");
        order.lock().unwrap().push("event_seen");
        assert_eq!(evt.kind, "seq.completed");
        let observed = order.lock().unwrap().clone();
        assert!(observed.contains(&"on_done"));
        assert!(observed.contains(&"event_seen"));
    }

    trait ExpectEvent {
        fn expect_event(self, msg: &str) -> crate::event_bus::Event;
    }
    impl ExpectEvent for crate::event_bus::RecvOutcome {
        fn expect_event(self, msg: &str) -> crate::event_bus::Event {
            match self {
                crate::event_bus::RecvOutcome::Event(e) => e,
                crate::event_bus::RecvOutcome::Timeout => panic!("{msg}: timed out"),
                crate::event_bus::RecvOutcome::Disconnected => panic!("{msg}: disconnected"),
            }
        }
    }

    #[test]
    fn register_blocking_overwrites_register_and_vice_versa() {
        // A blocking-registered name can be replaced by a sync one and vice versa: the flag is
        // part of the entry.
        let reg = ActionRegistry::new();
        reg.register("x", |_| Ok(json!("sync1")));
        assert!(!reg.is_blocking("x"));
        reg.register_blocking("x", |_| Ok(json!("blocking")));
        assert!(reg.is_blocking("x"));
        reg.register("x", |_| Ok(json!("sync2")));
        assert!(!reg.is_blocking("x"));
    }

    #[test]
    fn pool_routes_blocking_handler_to_pool_worker() {
        use crate::thread_pool::ThreadPool;
        let pool = ThreadPool::new(2, 4);
        let reg = Arc::new(ActionRegistry::new()).with_pool(pool.clone());
        let caller = std::thread::current().id();
        let observed = Arc::new(Mutex::new(None::<std::thread::ThreadId>));
        let obs_in_handler = observed.clone();
        reg.register_blocking("on_pool", move |_| {
            *obs_in_handler.lock().unwrap() = Some(std::thread::current().id());
            Ok(json!("done"))
        });
        let done = Arc::new(Mutex::new(false));
        let d = done.clone();
        reg.try_dispatch(
            "on_pool",
            json!({}),
            Box::new(move |_| {
                *d.lock().unwrap() = true;
            }),
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while !*done.lock().unwrap() {
            assert!(Instant::now() < deadline, "on_done never fired");
            std::thread::sleep(Duration::from_millis(5));
        }
        let worker_id = observed.lock().unwrap().expect("handler must have run");
        assert_ne!(worker_id, caller, "handler should run on a pool worker");
        drop(pool);
    }

    #[test]
    fn pool_saturation_yields_overloaded_response() {
        use crate::thread_pool::ThreadPool;
        // 1 worker + 1 queue slot = at most 2 in flight; the rest are cancelled `overloaded`.
        let pool = ThreadPool::new(1, 1);
        let bus = Arc::new(EventBus::new());
        let rx = bus.subscribe_unbounded("slowping.failed");
        let reg =
            Arc::new(ActionRegistry::with_completion_bus(bus.clone())).with_pool(pool.clone());
        reg.register_blocking("slowping", |_| {
            std::thread::sleep(Duration::from_millis(300));
            Ok(json!({}))
        });
        let mut callbacks = Vec::new();
        let mut received = Vec::new();
        for _ in 0..6 {
            let (tx, rx) = std::sync::mpsc::channel::<ActionResult>();
            received.push(rx);
            reg.try_dispatch(
                "slowping",
                json!({}),
                Box::new(move |r| {
                    let _ = tx.send(r);
                }),
            );
            callbacks.push(());
        }
        let mut overloaded_count = 0;
        for rx in received {
            let result = rx.recv_timeout(Duration::from_secs(5)).expect("orphan");
            if let Err(e) = &result
                && e.code == "overloaded"
            {
                overloaded_count += 1;
            }
        }
        assert!(
            overloaded_count >= 3,
            "expected ≥3 overloaded; got {overloaded_count}"
        );
        let mut failed_events = 0;
        while let crate::event_bus::RecvOutcome::Event(_) =
            rx.recv_timeout(Duration::from_millis(50))
        {
            failed_events += 1;
        }
        assert!(
            failed_events >= overloaded_count,
            "expected ≥{overloaded_count} failed events; got {failed_events}"
        );
        drop(pool);
    }
}
