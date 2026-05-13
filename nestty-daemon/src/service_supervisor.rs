//! Service plugin supervisor.
//!
//! A service plugin is a long-running subprocess that speaks newline-JSON
//! over stdio. This module owns the lifecycle (spawn, init handshake,
//! restart on crash), wires bidirectional RPC, forwards bus events the
//! service `subscribes` to, and registers `provides` actions through the
//! shared `ActionRegistry`.
//!
//! Conflict resolution. Before any process is spawned, every enabled
//! plugin's manifest is walked in lexical order of `[plugin].name`. The
//! first plugin to claim an action name wins; later plugins quietly skip
//! that one entry while keeping their other registrations. This makes the
//! global ownership table stable across runs and independent of spawn
//! ordering / filesystem mtime.
//!
//! Init validation. The runtime `initialize` reply is checked against the
//! manifest with the same asymmetric rule applied to BOTH `provides` and
//! `subscribes`: the served set is always the runtime∩manifest intersection.
//! Runtime ⊂ manifest is degraded mode — manifest-only actions stay
//! registered and return `service_degraded` on invoke. Runtime ⊃ manifest
//! warns and drops the extras. The pre-spawn analysis must stay accurate
//! or the conflict resolution above is invalid.
//!
//! Threading. Each running service owns three OS threads: a writer that
//! drains the outgoing channel into child stdin, a reader that parses
//! child stdout and dispatches, and a stderr-tail thread that logs. One
//! additional forwarder thread per `subscribes` pattern bridges the bus
//! into the outgoing channel. All blocking happens off the GTK main
//! thread.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use nestty_core::action_registry::{ActionRegistry, ActionResult, internal_error};
use nestty_core::event_bus::{Event as BusEvent, EventBus, pattern_matches};
use nestty_core::plugin::{Activation, LoadedPlugin, PluginServiceDef, RestartPolicy};
use nestty_core::protocol::{Request, Response, ResponseError};

/// Announced to plugins in the `initialize` request. Plugins decide whether
/// to accept; rejecting (e.g. with `protocol_mismatch`) makes init fail and
/// the supervisor kills the child. Bump on backwards-incompatible RPC changes.
pub const PROTOCOL_VERSION: u32 = 1;

const DEFAULT_INIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Single global action-reply timeout. Sized to accommodate long-running
/// LLM completions (10-90s); per-action overrides are a future addition.
const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_PENDING_BUFFER: usize = 64;
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(60);

#[derive(Debug)]
#[allow(dead_code)] // fields are surfaced via Debug output for diagnostics
enum ServiceState {
    /// Service is not running. Spawning is the responsibility of whichever
    /// activation rule fires next (`onStartup`, `onAction:`, `onEvent:`).
    Stopped,
    /// Process spawned and `initialize` request sent; awaiting reply.
    Starting { started_at: Instant },
    /// Init complete. The negotiated runtime capability set is the
    /// intersection of manifest + reply (subset rule).
    Running { service_version: String },
    /// Init timed out, the binary couldn't be spawned, or the protocol
    /// version was rejected. Stays here unless an activation re-arms it.
    Failed,
}

enum OutgoingFrame {
    Request(Request),
    Notification { method: String, params: Value },
    Response(Response),
}

struct PendingInvocation {
    action_name: String,
    params: Value,
    reply: Sender<ActionResult>,
    deadline: Instant,
}

struct BackoffState {
    consecutive_failures: u32,
}

impl BackoffState {
    fn new() -> Self {
        Self {
            consecutive_failures: 0,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let exp = self.consecutive_failures.min(6);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let secs = BACKOFF_BASE.as_secs() * (1u64 << exp);
        Duration::from_secs(secs).min(BACKOFF_CAP)
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

struct ServiceHandle {
    plugin_name: String,
    service_name: String,
    plugin_dir: PathBuf,
    spec: PluginServiceDef,
    /// Post-conflict-resolution manifest set. Runtime init reply checked
    /// against this (subset OK, superset rejected); see module preamble.
    approved_provides: Vec<String>,
    approved_subscribes: Vec<String>,
    state: Mutex<ServiceState>,
    /// Set when a process is alive. Sender goes to writer thread.
    outgoing: Mutex<Option<Sender<OutgoingFrame>>>,
    /// Pending requests nestty has sent to the service, awaiting reply.
    /// Keyed by request id.
    pending_responses: Mutex<HashMap<String, Sender<Response>>>,
    /// Action invocations buffered while state is `Starting`. Drained in
    /// FIFO order on the transition to `Running`.
    pending_invocations: Mutex<VecDeque<PendingInvocation>>,
    /// Negotiated runtime action set (`None` until first successful init).
    /// `invoke_remote` gates dispatch on this so manifest-approved actions
    /// the runtime omitted return `service_degraded` instead of vanishing.
    /// Cleared on exit so restart re-establishes via a fresh handshake.
    runtime_provides: Mutex<Option<HashSet<String>>>,
    /// Set while the child is alive. The init-timeout path SIGKILLs via
    /// this since stdin-EOF cooperation can't be assumed.
    child_pid: Mutex<Option<u32>>,
    next_id: AtomicU64,
    backoff: Mutex<BackoffState>,
    /// Per-init subscribe-forwarder bookkeeping. Without cooperative
    /// teardown via `handle_exit`, restarts would leak one forwarder
    /// thread + bus subscription per `subscribes` pattern.
    forwarder_stop: Arc<AtomicBool>,
    forwarder_handles: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl ServiceHandle {
    fn fq_name(&self) -> String {
        format!("{}::{}", self.plugin_name, self.service_name)
    }

    fn next_request_id(&self) -> String {
        format!(
            "svc-{}-{}",
            self.plugin_name,
            self.next_id.fetch_add(1, Ordering::SeqCst)
        )
    }

    fn send(&self, frame: OutgoingFrame) -> Result<(), ResponseError> {
        let guard = self.outgoing.lock().unwrap();
        match guard.as_ref() {
            Some(tx) => tx.send(frame).map_err(|_| service_unavailable(self)),
            None => Err(service_unavailable(self)),
        }
    }
}

fn service_unavailable(svc: &ServiceHandle) -> ResponseError {
    ResponseError {
        code: "service_unavailable".into(),
        message: format!("service {} is not running", svc.fq_name()),
    }
}

#[derive(Debug)]
pub struct ProvideConflict {
    pub action: String,
    pub winner: String,
    pub losers: Vec<String>,
}

/// `(plugin_name, service_name) → approved action names`.
pub type ApprovedProvides = HashMap<(String, String), Vec<String>>;

/// Resolve action-name conflicts deterministically: walks plugins in lexical
/// order of `[plugin].name` BEFORE any process is spawned, so spawn order,
/// mtimes, and filesystem layout don't influence which plugin wins.
pub fn resolve_provides(plugins: &[LoadedPlugin]) -> (ApprovedProvides, Vec<ProvideConflict>) {
    // Sort by `[plugin].name` first, then by `dir` path as a stable
    // tiebreaker. Without the secondary key, two plugins with the
    // same manifest name (which the loader currently doesn't reject —
    // see roadmap caveat) would resolve via filesystem enumeration
    // order, which differs across runs and machines. The directory
    // path is stable per install. Duplicate names are warned about
    // separately so the user notices and renames one of them.
    let mut ordered: Vec<&LoadedPlugin> = plugins.iter().collect();
    ordered.sort_by(|a, b| {
        a.manifest
            .plugin
            .name
            .cmp(&b.manifest.plugin.name)
            .then_with(|| a.dir.cmp(&b.dir))
    });
    // Warn about duplicate names so the user can fix the manifest.
    let mut seen: HashMap<&str, &Path> = HashMap::new();
    for plugin in &ordered {
        let name = plugin.manifest.plugin.name.as_str();
        if let Some(prev_dir) = seen.get(name) {
            eprintln!(
                "[nestty] duplicate plugin name {:?} at {:?} and {:?} — using directory path as tiebreaker; consider renaming one",
                name, prev_dir, plugin.dir
            );
        } else {
            seen.insert(name, plugin.dir.as_path());
        }
    }

    let mut owner: HashMap<String, String> = HashMap::new();
    let mut approved: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut conflicts: HashMap<String, ProvideConflict> = HashMap::new();

    for plugin in &ordered {
        let pname = &plugin.manifest.plugin.name;
        for svc in &plugin.manifest.services {
            let mut take = Vec::new();
            for action in &svc.provides {
                match owner.get(action) {
                    None => {
                        owner.insert(action.clone(), pname.clone());
                        take.push(action.clone());
                    }
                    Some(winner) if winner == pname => {
                        // Same plugin declared the action in multiple
                        // services. Treat as already-taken; the first
                        // service to declare it wins within the plugin.
                        // (Service order within a manifest is the user's
                        // choice — we don't try to be smarter than that.)
                    }
                    Some(winner) => {
                        let entry =
                            conflicts
                                .entry(action.clone())
                                .or_insert_with(|| ProvideConflict {
                                    action: action.clone(),
                                    winner: winner.clone(),
                                    losers: Vec::new(),
                                });
                        if !entry.losers.contains(pname) {
                            entry.losers.push(pname.clone());
                        }
                    }
                }
            }
            approved.insert((pname.clone(), svc.name.clone()), take);
        }
    }

    let mut conflict_list: Vec<ProvideConflict> = conflicts.into_values().collect();
    conflict_list.sort_by(|a, b| a.action.cmp(&b.action));
    (approved, conflict_list)
}

pub struct ServiceSupervisor {
    bus: Arc<EventBus>,
    registry: Arc<ActionRegistry>,
    services: Mutex<Vec<Arc<ServiceHandle>>>,
    nestty_version: String,
    init_timeout: Duration,
    action_timeout: Duration,
    /// `onEvent:` activations need a bus-listener that spawns the service
    /// on first matching event. `onAction:` doesn't — the registered handler
    /// triggers spawn via `invoke_remote`. Boot-immutable.
    on_event_rules: Vec<(String, Arc<ServiceHandle>)>,
    /// Set by `shutdown_all` to suppress restarts and new activations,
    /// preventing respawn during window-destroy teardown.
    shutting_down: AtomicBool,
}

impl ServiceSupervisor {
    pub fn new(
        bus: Arc<EventBus>,
        registry: Arc<ActionRegistry>,
        plugins: &[LoadedPlugin],
        nestty_version: impl Into<String>,
        extra_reserved: &[&str],
    ) -> Arc<Self> {
        // Snapshot the registry AND any additional reserved names BEFORE
        // walking the manifests. Two sources are needed because action
        // dispatch is currently bimodal:
        //
        //   1. Built-ins migrated into `ActionRegistry` (e.g.
        //      `system.ping`, `system.log`, `context.snapshot`) —
        //      `ActionRegistry::register` would overwrite them silently.
        //   2. Legacy match-arm commands in `socket::dispatch`
        //      (`tab.*`, `terminal.*`, `webview.*`, …) — these aren't
        //      in the registry, but the dispatcher checks the registry
        //      first, so a plugin that claimed `tab.new` would shadow
        //      the legacy handler.
        //
        // `extra_reserved` carries the legacy list (see
        // `socket::LEGACY_DISPATCH_METHODS`) so the supervisor protects
        // both. As more commands migrate into the registry, the
        // legacy list shrinks and this dual-source pattern fades.
        let mut reserved: HashSet<String> = registry.names().into_iter().collect();
        for name in extra_reserved {
            reserved.insert((*name).to_string());
        }
        // Existing `[[commands]]` plugins surface as `plugin.<name>.<cmd>`
        // through a wildcard match arm in `socket::dispatch`. Those
        // names aren't in the registry either, but registry-first
        // dispatch would let a service plugin claim them and shadow the
        // shell-command handler. Reserve every discovered command's
        // socket name so the `[[services]]` rollout stays additive
        // (docs/service-plugins.md D7).
        for plugin in plugins {
            for cmd in &plugin.manifest.commands {
                reserved.insert(format!(
                    "plugin.{}.{}",
                    plugin.manifest.plugin.name, cmd.name
                ));
            }
        }

        let (approved_map, conflicts) = resolve_provides(plugins);
        for c in &conflicts {
            eprintln!(
                "[nestty] service conflict: action {:?} taken by {:?}; skipped for: {:?}",
                c.action, c.winner, c.losers
            );
        }

        let mut services = Vec::new();
        let mut on_event_rules = Vec::new();

        for plugin in plugins {
            let pname = &plugin.manifest.plugin.name;
            for svc in &plugin.manifest.services {
                let raw_approved = approved_map
                    .get(&(pname.clone(), svc.name.clone()))
                    .cloned()
                    .unwrap_or_default();
                // Strip out any actions reserved by built-ins. A plugin
                // that lists a reserved name in `provides` keeps its
                // other declarations; the conflicting one is dropped
                // with a clear warning so the user can rename if it
                // was intentional.
                let approved_provides: Vec<String> = raw_approved
                    .into_iter()
                    .filter(|action| {
                        if reserved.contains(action) {
                            eprintln!(
                                "[nestty] service {}::{} declared {:?} but the name is reserved by a built-in; skipping",
                                pname, svc.name, action
                            );
                            false
                        } else {
                            true
                        }
                    })
                    .collect();
                let approved_subscribes = svc.subscribes.clone();

                let handle = Arc::new(ServiceHandle {
                    plugin_name: pname.clone(),
                    service_name: svc.name.clone(),
                    plugin_dir: plugin.dir.clone(),
                    spec: svc.clone(),
                    approved_provides: approved_provides.clone(),
                    approved_subscribes,
                    state: Mutex::new(ServiceState::Stopped),
                    outgoing: Mutex::new(None),
                    pending_responses: Mutex::new(HashMap::new()),
                    pending_invocations: Mutex::new(VecDeque::new()),
                    runtime_provides: Mutex::new(None),
                    child_pid: Mutex::new(None),
                    next_id: AtomicU64::new(0),
                    backoff: Mutex::new(BackoffState::new()),
                    forwarder_stop: Arc::new(AtomicBool::new(false)),
                    forwarder_handles: Mutex::new(Vec::new()),
                });

                if let Activation::OnEvent(glob) = &svc.activation {
                    on_event_rules.push((glob.clone(), handle.clone()));
                }

                services.push(handle);
            }
        }

        let supervisor = Arc::new(Self {
            bus,
            registry,
            services: Mutex::new(services),
            nestty_version: nestty_version.into(),
            init_timeout: DEFAULT_INIT_TIMEOUT,
            action_timeout: DEFAULT_ACTION_TIMEOUT,
            on_event_rules,
            shutting_down: AtomicBool::new(false),
        });

        // Register approved actions in the global registry. Each handler
        // captures an Arc<ServiceHandle> + Arc<Self> and routes through
        // the supervisor regardless of the service's current state. The
        // action name is captured per-registration so the remote call
        // carries the correct method even though every handler shares
        // the same closure shape.
        let services_snapshot = supervisor.services.lock().unwrap().clone();
        for handle in &services_snapshot {
            for action_name in handle.approved_provides.clone() {
                let svc = handle.clone();
                let sup = supervisor.clone();
                let captured_name = action_name.clone();
                // `register_blocking`: invoke_remote can park the
                // calling thread for up to the action timeout
                // (DEFAULT_ACTION_TIMEOUT — currently 120s for LLM
                // completions; see the constant at the top of this
                // file) waiting on a stdio reply from the plugin
                // subprocess. Marking the entry blocking lets
                // `ActionRegistry::try_dispatch` route this onto a
                // worker thread so the GTK main loop and trigger
                // pump don't stall while the plugin computes.
                supervisor
                    .registry
                    .register_blocking(action_name, move |params| {
                        sup.invoke_remote(&svc, &captured_name, params)
                    });
            }
        }

        // Wire `onEvent:` activations as PURE SPAWN TRIGGERS, matching
        // the documented contract that `event.dispatch` is driven by
        // `subscribes` (not by activation). Each onEvent thread blocks
        // on bus matches and triggers `start_service` if the service is
        // currently `Stopped` or `Failed`; the event itself is NOT
        // forwarded via this path.
        //
        // Known caveat (tracked in roadmap): the very first event that
        // activates the service is dropped from delivery unless the
        // user also declares the same glob in `subscribes`. The
        // post-init `subscribes` forwarders subscribe AFTER init
        // completes, so any event that arrived during init is gone by
        // the time they exist. Authors who need both activation AND
        // delivery should put the glob in both lists for Phase 9.1; a
        // future iteration will pre-subscribe `subscribes` at
        // supervisor::new so the buffer survives across init.
        //
        // `Failed` is included alongside `Stopped` so init failures
        // don't permanently inert an event-activated service, per the
        // lifecycle contract (`docs/service-plugins.md`): activation
        // re-arms `Failed`.
        for (glob, handle) in &supervisor.on_event_rules {
            let rx = supervisor.bus.subscribe_unbounded(glob.clone());
            let sup = supervisor.clone();
            let svc = handle.clone();
            thread::spawn(move || {
                while let Some(_ev) = rx.recv() {
                    if !matches!(
                        *svc.state.lock().unwrap(),
                        ServiceState::Stopped | ServiceState::Failed
                    ) {
                        continue;
                    }
                    sup.spawn_service_async(svc.clone());
                }
            });
        }

        // Eager-start services with `onStartup` activation.
        for handle in &services_snapshot {
            if matches!(handle.spec.activation, Activation::OnStartup) {
                supervisor.spawn_service_async(handle.clone());
            }
        }

        supervisor
    }

    fn spawn_service_async(self: &Arc<Self>, handle: Arc<ServiceHandle>) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let sup = self.clone();
        thread::spawn(move || {
            if let Err(e) = sup.start_service(&handle) {
                eprintln!(
                    "[nestty] service {} failed to start: {}",
                    handle.fq_name(),
                    e.message
                );
            }
        });
    }

    fn start_service(self: &Arc<Self>, handle: &Arc<ServiceHandle>) -> Result<(), ResponseError> {
        // Atomically claim the right to spawn. Any other thread (a
        // concurrent activation or restart) sees `Starting` and bails.
        {
            let mut state = handle.state.lock().unwrap();
            match *state {
                ServiceState::Running { .. } | ServiceState::Starting { .. } => return Ok(()),
                _ => {}
            }
            *state = ServiceState::Starting {
                started_at: Instant::now(),
            };
        }

        let result = self.start_service_inner(handle);
        if let Err(ref err) = result {
            *handle.state.lock().unwrap() = ServiceState::Failed;
            // Closing the outgoing channel triggers the writer thread
            // to drop child stdin, which gives the child EOF and lets
            // the wait thread observe a clean exit. Without this, a
            // child that ignores its stdin sits around forever.
            *handle.outgoing.lock().unwrap() = None;
            // Drain any pending invocations with the error so callers
            // unblock instead of timing out.
            let mut buf = handle.pending_invocations.lock().unwrap();
            while let Some(p) = buf.pop_front() {
                let _ = p.reply.send(Err(err.clone()));
            }
        }
        result
    }

    fn start_service_inner(
        self: &Arc<Self>,
        handle: &Arc<ServiceHandle>,
    ) -> Result<(), ResponseError> {
        let exec_path = resolve_exec(&handle.plugin_dir, &handle.spec.exec);
        let mut cmd = Command::new(&exec_path);
        cmd.args(&handle.spec.args)
            .current_dir(&handle.plugin_dir)
            .env("NESTTY_PLUGIN_NAME", &handle.plugin_name)
            .env(
                "NESTTY_PLUGIN_DIR",
                handle.plugin_dir.to_string_lossy().as_ref(),
            )
            .env("NESTTY_SERVICE_NAME", &handle.service_name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Phase 9.5 originally armed `PR_SET_PDEATHSIG` here so a
        // nestty crash (SIGKILL / segfault / unrecoverable panic
        // before `connect_destroy → shutdown_all` can fire) would
        // also reap every supervised plugin. Removed at Phase
        // 18.x because of a Linux quirk that's well-documented
        // but easy to miss: the kernel signal fires when the
        // **THREAD** that called fork() exits, not when the
        // parent process exits. `spawn_service_async` runs each
        // spawn on a fresh worker thread that returns as soon as
        // `start_service` finishes — so every onStartup plugin
        // received SIGTERM moments after init succeeded, looped
        // through the supervisor's restart=on-crash policy, and
        // crash-looped indefinitely.
        //
        // Acceptable trade-off without it: on nestty SIGKILL /
        // segfault, plugin children become init-reparented orphans
        // until `shutdown_all` fires (normal exit) or the user
        // notices stray nestty-plugin-* processes. For our threat
        // model (single-user desktop, infrequent crashes), this is
        // strictly better than the working-but-broken pdeathsig
        // race.
        //
        // To re-introduce crash-safe child reaping cleanly: spawn
        // every plugin from a dedicated long-lived "spawner"
        // thread that never exits, OR use a `pidfd_open` /
        // self-pipe pattern instead of pdeathsig. Tracked as
        // a follow-up on the supervisor.
        let mut child = cmd.spawn().map_err(|e| ResponseError {
            code: "spawn_failed".into(),
            message: format!(
                "failed to spawn {} ({}): {}",
                handle.fq_name(),
                exec_path.display(),
                e
            ),
        })?;

        let pid = child.id();
        *handle.child_pid.lock().unwrap() = Some(pid);

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| internal_error("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| internal_error("no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| internal_error("no stderr"))?;

        let (out_tx, out_rx) = channel::<OutgoingFrame>();
        *handle.outgoing.lock().unwrap() = Some(out_tx.clone());

        // Writer thread.
        spawn_writer(handle.fq_name(), stdin, out_rx);

        // Reader thread: dispatches frames to the supervisor.
        spawn_reader(self.clone(), handle.clone(), stdout);

        // Stderr tail.
        spawn_stderr_tail(handle.fq_name(), stderr);

        // Process-wait thread: notifies supervisor on exit so it can
        // restart per policy.
        let sup_for_wait = self.clone();
        let handle_for_wait = handle.clone();
        thread::spawn(move || {
            let status = child.wait();
            sup_for_wait.handle_exit(handle_for_wait, status);
        });

        // Send `initialize` request and block here for the reply (we're
        // already off the GTK main thread — `start_service` is always
        // called from a worker via `spawn_service_async`).
        let req_id = handle.next_request_id();
        let req = Request::new(
            req_id.clone(),
            "initialize",
            json!({
                "nestty_version": self.nestty_version,
                "protocol_version": PROTOCOL_VERSION,
            }),
        );
        let (reply_tx, reply_rx) = channel::<Response>();
        handle
            .pending_responses
            .lock()
            .unwrap()
            .insert(req_id.clone(), reply_tx);
        handle.send(OutgoingFrame::Request(req))?;

        let response = match reply_rx.recv_timeout(self.init_timeout) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                handle.pending_responses.lock().unwrap().remove(&req_id);
                // Best-effort SIGKILL so a plugin that ignores its stdin
                // doesn't accumulate as an orphaned process across
                // restart attempts. The wait thread will pick up the
                // exit and run `handle_exit` cleanly afterwards.
                kill_child(pid);
                // `service_unavailable` matches the documented protocol
                // error code (docs/service-plugins.md "During Starting,
                // requests for the service are buffered… If the service
                // doesn't initialize within a timeout, pending invokes
                // return ResponseError { code: 'service_unavailable' }").
                return Err(ResponseError {
                    code: "service_unavailable".into(),
                    message: format!(
                        "service {} did not reply to initialize within {:?}",
                        handle.fq_name(),
                        self.init_timeout
                    ),
                });
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ResponseError {
                    code: "service_unavailable".into(),
                    message: format!("service {} closed before init reply", handle.fq_name()),
                });
            }
        };

        if !response.ok {
            kill_child(pid);
            return Err(response.error.unwrap_or_else(|| ResponseError {
                code: "init_failed".into(),
                message: "service rejected initialize".into(),
            }));
        }
        let result = response.result.ok_or_else(|| {
            kill_child(pid);
            ResponseError {
                code: "init_failed".into(),
                message: "init response missing result".into(),
            }
        })?;

        let service_version = result
            .get("service_version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let runtime_provides = string_array(&result, "provides");
        let runtime_subscribes = string_array(&result, "subscribes");

        // Asymmetric validation, applied identically to both fields.
        let manifest_provides: HashSet<&str> = handle
            .approved_provides
            .iter()
            .map(String::as_str)
            .collect();
        let manifest_subscribes: HashSet<&str> = handle
            .approved_subscribes
            .iter()
            .map(String::as_str)
            .collect();

        let mut accepted_provides = Vec::new();
        for entry in &runtime_provides {
            if manifest_provides.contains(entry.as_str()) {
                accepted_provides.push(entry.clone());
            } else {
                eprintln!(
                    "[nestty] service {} runtime declared unauthorized provide {:?}; ignoring",
                    handle.fq_name(),
                    entry
                );
            }
        }
        let mut accepted_subscribes = Vec::new();
        for entry in &runtime_subscribes {
            if manifest_subscribes.contains(entry.as_str()) {
                accepted_subscribes.push(entry.clone());
            } else {
                eprintln!(
                    "[nestty] service {} runtime declared unauthorized subscribe {:?}; ignoring",
                    handle.fq_name(),
                    entry
                );
            }
        }

        // Send `initialized` notification.
        handle.send(OutgoingFrame::Notification {
            method: "initialized".into(),
            params: json!({}),
        })?;

        // Atomic Starting→Running transition. We hold the state lock
        // for the entire transition and re-check that we're still
        // `Starting` because the wait thread may have observed an exit
        // mid-init and already flipped the state to `Stopped`. Setting
        // `runtime_provides` and `state` under the same critical
        // section (and against the same lock order `handle_exit` uses)
        // means a concurrent exit can't half-clear the next instance's
        // state. Subset rule: invocations for manifest-approved actions
        // the runtime didn't claim now return `service_degraded` from
        // `invoke_remote` instead of being silently sent to a service
        // that won't handle them.
        {
            let mut state_g = handle.state.lock().unwrap();
            if !matches!(*state_g, ServiceState::Starting { .. }) {
                // Wait thread saw the child exit while we were still
                // in init. Roll back: don't transition to Running.
                return Err(ResponseError {
                    code: "service_unavailable".into(),
                    message: format!("service {} exited during init", handle.fq_name()),
                });
            }
            *handle.runtime_provides.lock().unwrap() =
                Some(accepted_provides.iter().cloned().collect());
            *state_g = ServiceState::Running { service_version };
        }
        handle.backoff.lock().unwrap().reset();

        eprintln!(
            "[nestty] service {} ready (provides={:?}, subscribes={:?})",
            handle.fq_name(),
            accepted_provides,
            accepted_subscribes
        );

        // Forwarder threads bridge bus events into the outgoing channel
        // for kinds the runtime actually accepted. They exit on:
        //   1. The shared `forwarder_stop` flag flipping true
        //      (handle_exit's cooperative teardown).
        //   2. The bus dropping the subscriber (RecvOutcome::Disconnected),
        //      e.g. on a global supervisor shutdown.
        //   3. The writer channel rejecting a send (out_rx dropped).
        // Reset the stop flag for this fresh instance and clear any
        // stale JoinHandles left over from a previous run that
        // crashed before handle_exit could join them.
        handle
            .forwarder_stop
            .store(false, std::sync::atomic::Ordering::SeqCst);
        handle.forwarder_handles.lock().unwrap().clear();
        for pattern in accepted_subscribes {
            let rx = self.bus.subscribe_unbounded(pattern.clone());
            let writer = out_tx.clone();
            let svc_label = handle.fq_name();
            let stop = handle.forwarder_stop.clone();
            let join = thread::spawn(move || {
                use nestty_core::event_bus::RecvOutcome;
                loop {
                    if stop.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                        RecvOutcome::Event(ev) => {
                            let frame = OutgoingFrame::Notification {
                                method: "event.dispatch".into(),
                                params: json!({
                                    "kind": ev.kind,
                                    "source": ev.source,
                                    "timestamp_ms": ev.timestamp_ms,
                                    "payload": ev.payload,
                                }),
                            };
                            if writer.send(frame).is_err() {
                                log::debug!(
                                    "forwarder for {svc_label} pattern {pattern:?} exiting (writer closed)"
                                );
                                break;
                            }
                        }
                        RecvOutcome::Timeout => continue,
                        RecvOutcome::Disconnected => {
                            log::debug!(
                                "forwarder for {svc_label} pattern {pattern:?} exiting (bus disconnected)"
                            );
                            break;
                        }
                    }
                }
            });
            handle.forwarder_handles.lock().unwrap().push(join);
        }

        // Drain buffered invocations now that the service is Running.
        let mut buf = handle.pending_invocations.lock().unwrap();
        while let Some(p) = buf.pop_front() {
            if Instant::now() >= p.deadline {
                let _ = p.reply.send(Err(ResponseError {
                    code: "service_unavailable".into(),
                    message: "queued action timed out before service was ready".into(),
                }));
                continue;
            }
            self.dispatch_invocation(handle.clone(), p);
        }

        Ok(())
    }

    fn handle_exit(
        self: Arc<Self>,
        handle: Arc<ServiceHandle>,
        status: std::io::Result<std::process::ExitStatus>,
    ) {
        // Hold state lock for the entire cleanup so a concurrent
        // `start_service` can't observe a partly-cleaned instance and
        // race a replacement. Lock order is state → outgoing → child_pid
        // → runtime_provides; `start_service_inner` follows the same
        // order at the Running transition, so no deadlock.
        let prev_state = {
            let mut state_g = handle.state.lock().unwrap();
            let prev = std::mem::replace(&mut *state_g, ServiceState::Stopped);
            *handle.outgoing.lock().unwrap() = None;
            *handle.child_pid.lock().unwrap() = None;
            // Clear the negotiated set so a restart re-establishes through
            // a fresh handshake; until then `invoke_remote` returns to
            // the pre-init buffering behavior for manifest-approved
            // actions.
            *handle.runtime_provides.lock().unwrap() = None;
            prev
        };

        // Tear down per-instance forwarder threads BEFORE we drop the
        // state lock so the JoinHandles can't be observed by the next
        // `start_service` call. Without this, every successful init
        // accumulates one forwarder thread + one bus subscription per
        // `subscribes` pattern, which over a crash-loop scenario
        // grows unbounded. Cooperative shutdown via the shared
        // `forwarder_stop` flag — each forwarder polls every 200ms
        // (worst-case shutdown latency).
        handle
            .forwarder_stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let joins: Vec<thread::JoinHandle<()>> =
            std::mem::take(&mut *handle.forwarder_handles.lock().unwrap());
        for j in joins {
            // Wait briefly for graceful exit; if the forwarder is
            // somehow stuck we'd rather leak it (next start will
            // overwrite the JoinHandles list anyway) than block
            // handle_exit forever.
            let _ = j.join();
        }

        // Fail any pending responses.
        let pending: Vec<Sender<Response>> = handle
            .pending_responses
            .lock()
            .unwrap()
            .drain()
            .map(|(_, s)| s)
            .collect();
        for tx in pending {
            let _ = tx.send(Response::error(
                String::new(),
                "service_unavailable",
                "service exited before reply",
            ));
        }

        let exited_cleanly = matches!(&status, Ok(s) if s.success());
        let was_running = matches!(prev_state, ServiceState::Running { .. });

        // Once shutdown_all has been called, ALL services should stay
        // dead — even ones with `restart=always`, even ones that
        // crashed in response to the `shutdown` notification. Without
        // this gate a fast-restarting service can outlive the GUI.
        let should_restart = if self.shutting_down.load(Ordering::SeqCst) {
            false
        } else {
            match handle.spec.restart {
                RestartPolicy::Never => false,
                RestartPolicy::Always => was_running,
                RestartPolicy::OnCrash => was_running && !exited_cleanly,
            }
        };

        eprintln!(
            "[nestty] service {} exited (status={:?}, was_running={}, restart={})",
            handle.fq_name(),
            status.as_ref().ok().and_then(|s| s.code()),
            was_running,
            should_restart
        );

        if !should_restart {
            return;
        }

        let delay = handle.backoff.lock().unwrap().next_delay();
        let sup = self.clone();
        let h = handle.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            if let Err(e) = sup.start_service(&h) {
                eprintln!(
                    "[nestty] service {} restart failed: {}",
                    h.fq_name(),
                    e.message
                );
            }
        });
    }

    /// Action-registry entry point: blocks the caller until the service
    /// replies or `self.action_timeout` elapses.
    fn invoke_remote(
        self: &Arc<Self>,
        handle: &Arc<ServiceHandle>,
        action_name: &str,
        params: Value,
    ) -> ActionResult {
        // Subset enforcement. Once init has negotiated the runtime
        // action set, manifest-approved actions the runtime didn't
        // claim must NOT be dispatched (the documented "degraded mode"
        // contract). Pre-init the set is None and we still buffer +
        // spawn — at that point the manifest is the best information
        // we have.
        //
        // We snapshot `runtime_provides` into a local before touching
        // any other lock so this method never holds two locks at once.
        // That keeps `invoke_remote` lock-order-safe with respect to
        // `start_service_inner` and `handle_exit`, both of which take
        // state → runtime_provides nested.
        let provides_snapshot: Option<HashSet<String>> =
            handle.runtime_provides.lock().unwrap().clone();
        if let Some(set) = provides_snapshot.as_ref()
            && !set.contains(action_name)
        {
            return Err(ResponseError {
                code: "service_degraded".into(),
                message: format!(
                    "service {} does not currently provide {action_name} (manifest: yes; runtime: no)",
                    handle.fq_name()
                ),
            });
        }

        let (reply_tx, reply_rx) = channel::<ActionResult>();
        let deadline = Instant::now() + self.action_timeout;
        let pending = PendingInvocation {
            action_name: action_name.to_string(),
            params,
            reply: reply_tx,
            deadline,
        };

        // Decide on routing under a single state lock to avoid TOCTOU.
        //
        //   Running   → dispatch immediately.
        //   Starting  → buffer; the worker that's already starting
        //               drains the buffer after init.
        //   Stopped/  → only an `onAction:<glob>` activation that
        //   Failed     matches `action_name` may trigger spawn. For
        //               `onStartup`/`onEvent` services in this state,
        //               the activation rule did not fire — actions
        //               must not resurrect them — and `restart=never`
        //               services are intentionally inert. We return
        //               `service_unavailable` per the protocol.
        let need_spawn = {
            let state = handle.state.lock().unwrap();
            match &*state {
                ServiceState::Running { .. } => {
                    drop(state);
                    self.dispatch_invocation(handle.clone(), pending);
                    false
                }
                ServiceState::Starting { .. } => {
                    let mut buf = handle.pending_invocations.lock().unwrap();
                    if buf.len() >= MAX_PENDING_BUFFER {
                        return Err(ResponseError {
                            code: "buffer_full".into(),
                            message: format!("service {} startup buffer full", handle.fq_name()),
                        });
                    }
                    buf.push_back(pending);
                    false
                }
                ServiceState::Stopped | ServiceState::Failed => {
                    match &handle.spec.activation {
                        Activation::OnAction(glob) if pattern_matches(glob, action_name) => {
                            // Action matches the activation glob —
                            // permitted to spawn. Buffer the invocation
                            // so the worker drains it after init.
                            let mut buf = handle.pending_invocations.lock().unwrap();
                            if buf.len() >= MAX_PENDING_BUFFER {
                                return Err(ResponseError {
                                    code: "buffer_full".into(),
                                    message: format!(
                                        "service {} startup buffer full",
                                        handle.fq_name()
                                    ),
                                });
                            }
                            buf.push_back(pending);
                            true
                        }
                        _ => {
                            // onStartup / onEvent / mismatched onAction —
                            // actions cannot trigger spawn. Return the
                            // documented protocol error rather than
                            // silently incurring a side effect.
                            return Err(ResponseError {
                                code: "service_unavailable".into(),
                                message: format!(
                                    "service {} is not running and \
                                     {action_name} cannot trigger its activation \
                                     ({:?})",
                                    handle.fq_name(),
                                    handle.spec.activation
                                ),
                            });
                        }
                    }
                }
            }
        };

        if need_spawn {
            self.spawn_service_async(handle.clone());
        }

        match reply_rx.recv_timeout(self.action_timeout) {
            Ok(r) => r,
            Err(_) => Err(ResponseError {
                code: "action_timeout".into(),
                message: format!(
                    "no response from {} within {:?}",
                    handle.fq_name(),
                    self.action_timeout
                ),
            }),
        }
    }

    /// Send the buffered invocation to the running service. The reader
    /// thread will fulfill `pending.reply` when the response arrives.
    fn dispatch_invocation(
        self: &Arc<Self>,
        handle: Arc<ServiceHandle>,
        pending: PendingInvocation,
    ) {
        let req_id = handle.next_request_id();
        // Send-side of the response channel; reader thread routes here.
        let (resp_tx, resp_rx) = channel::<Response>();
        handle
            .pending_responses
            .lock()
            .unwrap()
            .insert(req_id.clone(), resp_tx);

        let invoke_request = Request::new(
            req_id.clone(),
            "action.invoke",
            json!({
                "name": pending.action_name,
                "params": pending.params,
            }),
        );
        if let Err(e) = handle.send(OutgoingFrame::Request(invoke_request)) {
            handle.pending_responses.lock().unwrap().remove(&req_id);
            let _ = pending.reply.send(Err(e));
            return;
        }

        // Wait for the response on a worker thread so this method can
        // return promptly. The `reply` sender unblocks the original
        // `invoke_remote` caller. On timeout we ALSO drop the entry
        // from `pending_responses` so a permanently-hung service
        // doesn't accumulate one stale sender per timed-out call.
        let reply = pending.reply;
        let timeout = self.action_timeout;
        let svc_label = handle.fq_name();
        let cleanup_handle = handle.clone();
        let cleanup_id = req_id.clone();
        thread::spawn(move || {
            let outcome = match resp_rx.recv_timeout(timeout) {
                Ok(resp) => {
                    if resp.ok {
                        Ok(resp.result.unwrap_or(Value::Null))
                    } else {
                        Err(resp.error.unwrap_or_else(|| ResponseError {
                            code: "service_error".into(),
                            message: format!("{svc_label} returned error without detail"),
                        }))
                    }
                }
                Err(_) => {
                    cleanup_handle
                        .pending_responses
                        .lock()
                        .unwrap()
                        .remove(&cleanup_id);
                    Err(ResponseError {
                        code: "action_timeout".into(),
                        message: format!("no response from {svc_label} within {timeout:?}"),
                    })
                }
            };
            let _ = reply.send(outcome);
        });
    }

    /// Called by the reader thread when the service produces a frame.
    fn handle_inbound(self: &Arc<Self>, handle: &Arc<ServiceHandle>, frame: InboundFrame) {
        match frame {
            InboundFrame::Response(resp) => {
                if let Some(tx) = handle.pending_responses.lock().unwrap().remove(&resp.id) {
                    let _ = tx.send(resp);
                } else {
                    log::warn!(
                        "service {} returned response for unknown id {:?}",
                        handle.fq_name(),
                        resp.id
                    );
                }
            }
            InboundFrame::Request(req) => self.handle_service_request(handle, req),
            InboundFrame::Notification { method, params } => {
                self.handle_service_notification(handle, method, params)
            }
        }
    }

    fn handle_service_request(self: &Arc<Self>, handle: &Arc<ServiceHandle>, req: Request) {
        // Run on a worker thread so the reader stays free. A registered
        // action handler may itself call back into a service action,
        // which would block here on a response that the same reader is
        // responsible for delivering — classic deadlock if we kept
        // executing inline.
        let sup = self.clone();
        let handle = handle.clone();
        thread::spawn(move || {
            let resp = match req.method.as_str() {
                "action.invoke" => {
                    let name = req
                        .params
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let params = req.params.get("params").cloned().unwrap_or(Value::Null);
                    match name {
                        Some(n) => match sup.registry.invoke(&n, params) {
                            Ok(v) => Response::success(req.id.clone(), v),
                            Err(e) => Response {
                                id: req.id.clone(),
                                ok: false,
                                result: None,
                                error: Some(e),
                            },
                        },
                        None => Response::error(
                            req.id.clone(),
                            "invalid_params",
                            "missing 'name' on action.invoke",
                        ),
                    }
                }
                other => Response::error(
                    req.id.clone(),
                    "unknown_method",
                    &format!("unknown service→nestty request method: {other}"),
                ),
            };
            if let Err(e) = handle.send(OutgoingFrame::Response(resp)) {
                log::warn!(
                    "could not return action.invoke response to {}: {}",
                    handle.fq_name(),
                    e.message
                );
            }
        });
    }

    fn handle_service_notification(
        self: &Arc<Self>,
        handle: &Arc<ServiceHandle>,
        method: String,
        params: Value,
    ) {
        match method.as_str() {
            "event.publish" => {
                let kind = match params.get("kind").and_then(Value::as_str) {
                    Some(k) => k.to_string(),
                    None => {
                        log::warn!("service {} event.publish missing 'kind'", handle.fq_name());
                        return;
                    }
                };
                let payload = params.get("payload").cloned().unwrap_or(Value::Null);
                let source = format!("plugin:{}", handle.plugin_name);
                self.bus.publish(BusEvent::new(kind, source, payload));
            }
            "log" => {
                let level = params
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("info");
                let msg = params.get("message").and_then(Value::as_str).unwrap_or("");
                eprintln!("[plugin:{}] {level}: {msg}", handle.plugin_name);
            }
            other => {
                log::warn!(
                    "service {} sent unknown notification {:?}",
                    handle.fq_name(),
                    other
                );
            }
        }
    }

    #[allow(dead_code)]
    pub fn service_count(&self) -> usize {
        self.services.lock().unwrap().len()
    }

    /// Idempotent three-stage shutdown:
    /// 1. Set `shutting_down` (must precede exits, else `restart=always`
    ///    plugins exiting on the `shutdown` notification could respawn).
    /// 2. Notify every running service via `shutdown`.
    /// 3. After 200ms grace, SIGKILL anything still alive.
    ///
    /// Subscribe-forwarder threads hold cloned `outgoing` senders; the
    /// SIGKILL chain (writer → forwarder) terminates them naturally.
    pub fn shutdown_all(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);

        let services = self.services.lock().unwrap().clone();
        for handle in &services {
            if !matches!(*handle.state.lock().unwrap(), ServiceState::Running { .. }) {
                continue;
            }
            let _ = handle.send(OutgoingFrame::Notification {
                method: "shutdown".into(),
                params: json!({}),
            });
            *handle.state.lock().unwrap() = ServiceState::Stopped;
        }

        thread::sleep(Duration::from_millis(200));

        for handle in &services {
            if let Some(pid) = *handle.child_pid.lock().unwrap() {
                kill_child(pid);
            }
        }
    }
}

enum InboundFrame {
    Request(Request),
    Response(Response),
    Notification { method: String, params: Value },
}

fn parse_inbound(raw: &str) -> Option<InboundFrame> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if value.get("ok").is_some() {
        let resp: Response = serde_json::from_value(value).ok()?;
        return Some(InboundFrame::Response(resp));
    }
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let method = value
        .get("method")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match id {
        Some(id_str) if !id_str.is_empty() => Some(InboundFrame::Request(Request {
            id: id_str,
            method,
            params,
        })),
        _ => Some(InboundFrame::Notification { method, params }),
    }
}

fn spawn_writer(label: String, mut stdin: ChildStdin, rx: Receiver<OutgoingFrame>) {
    thread::spawn(move || {
        for frame in rx.iter() {
            let line = match frame {
                OutgoingFrame::Request(req) => serde_json::to_string(&req),
                OutgoingFrame::Response(resp) => serde_json::to_string(&resp),
                OutgoingFrame::Notification { method, params } => {
                    serde_json::to_string(&serde_json::json!({
                        "method": method,
                        "params": params,
                    }))
                }
            };
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    log::warn!("writer for {label} serialize error: {e}");
                    continue;
                }
            };
            if writeln!(stdin, "{line}").is_err() || stdin.flush().is_err() {
                log::debug!("writer for {label} closed");
                return;
            }
        }
    });
}

fn spawn_reader(
    sup: Arc<ServiceSupervisor>,
    handle: Arc<ServiceHandle>,
    stdout: std::process::ChildStdout,
) {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => return,
            };
            if line.is_empty() {
                continue;
            }
            let frame = match parse_inbound(&line) {
                Some(f) => f,
                None => {
                    log::warn!("service {} sent unparseable line: {line}", handle.fq_name());
                    continue;
                }
            };
            sup.handle_inbound(&handle, frame);
        }
    });
}

fn spawn_stderr_tail(label: String, stderr: std::process::ChildStderr) {
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("[plugin:{label}:stderr] {line}");
        }
    });
}

/// Best-effort SIGKILL by raw PID — `Child` has been moved into the wait
/// thread, so we can't `kill()` it directly. Return value is ignored;
/// "process already exited" is the most common failure and is the goal anyway,
/// but PID reuse means a successful syscall is also not a strict guarantee
/// the right process was hit.
fn kill_child(pid: u32) {
    // Safety: `libc::kill` accepts a PID + signal. The PID came from
    // `Child::id()` and was ours when first recorded; PID reuse after
    // wait+respawn could in theory aim at an unrelated process, but
    // SIGKILL on PID 0 / negative is the only foot-gun the syscall has,
    // and we never pass those values. Return ignored (see outer doc).
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

fn resolve_exec(plugin_dir: &Path, exec: &str) -> PathBuf {
    let path = PathBuf::from(exec);
    if path.is_absolute() {
        return path;
    }
    let candidate = plugin_dir.join(&path);
    if candidate.exists() {
        return candidate;
    }
    // Fall back to PATH lookup by passing through; Command::new will use
    // the bare name if it can't find it in plugin_dir.
    path
}

fn string_array(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestty_core::plugin::{
        Activation, PluginManifest, PluginMeta, PluginServiceDef, RestartPolicy,
    };

    fn mk_plugin(name: &str, services: Vec<PluginServiceDef>) -> LoadedPlugin {
        LoadedPlugin {
            manifest: PluginManifest {
                plugin: PluginMeta {
                    name: name.into(),
                    title: name.into(),
                    version: "0.0.0".into(),
                    description: None,
                },
                panels: vec![],
                commands: vec![],
                modules: vec![],
                services,
            },
            dir: PathBuf::from("/tmp"),
        }
    }

    fn mk_service(name: &str, provides: &[&str]) -> PluginServiceDef {
        PluginServiceDef {
            name: name.into(),
            exec: "noop".into(),
            args: vec![],
            activation: Activation::OnStartup,
            restart: RestartPolicy::Never,
            provides: provides.iter().map(|s| s.to_string()).collect(),
            subscribes: vec![],
        }
    }

    #[test]
    fn provide_conflict_resolves_lexically() {
        let a = mk_plugin("alpha", vec![mk_service("main", &["kb.search"])]);
        let b = mk_plugin("bravo", vec![mk_service("main", &["kb.search", "kb.read"])]);
        let (approved, conflicts) = resolve_provides(&[b.clone(), a.clone()]);
        // alpha sorts first → wins kb.search; bravo keeps kb.read.
        assert_eq!(
            approved.get(&("alpha".into(), "main".into())),
            Some(&vec!["kb.search".into()])
        );
        assert_eq!(
            approved.get(&("bravo".into(), "main".into())),
            Some(&vec!["kb.read".into()])
        );
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].action, "kb.search");
        assert_eq!(conflicts[0].winner, "alpha");
        assert_eq!(conflicts[0].losers, vec!["bravo".to_string()]);
    }

    #[test]
    fn provide_no_conflict_returns_empty_conflicts() {
        let a = mk_plugin("alpha", vec![mk_service("main", &["a.x"])]);
        let b = mk_plugin("bravo", vec![mk_service("main", &["b.y"])]);
        let (approved, conflicts) = resolve_provides(&[a, b]);
        assert!(conflicts.is_empty());
        assert_eq!(approved.len(), 2);
    }

    #[test]
    fn provide_three_way_conflict_collects_all_losers() {
        let a = mk_plugin("alpha", vec![mk_service("main", &["x"])]);
        let b = mk_plugin("bravo", vec![mk_service("main", &["x"])]);
        let c = mk_plugin("charlie", vec![mk_service("main", &["x"])]);
        let (_, conflicts) = resolve_provides(&[c, a, b]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].winner, "alpha");
        assert_eq!(
            conflicts[0].losers,
            vec!["bravo".to_string(), "charlie".to_string()]
        );
    }

    #[test]
    fn parse_inbound_recognizes_response() {
        let line = r#"{"id":"x","ok":true,"result":42}"#;
        let frame = parse_inbound(line).expect("response");
        assert!(matches!(frame, InboundFrame::Response(_)));
    }

    #[test]
    fn parse_inbound_recognizes_request() {
        let line = r#"{"id":"x","method":"action.invoke","params":{}}"#;
        let frame = parse_inbound(line).expect("request");
        assert!(matches!(frame, InboundFrame::Request(_)));
    }

    #[test]
    fn parse_inbound_recognizes_notification() {
        let line = r#"{"method":"event.publish","params":{"kind":"x"}}"#;
        let frame = parse_inbound(line).expect("notification");
        match frame {
            InboundFrame::Notification { method, .. } => assert_eq!(method, "event.publish"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_inbound_treats_empty_id_as_notification() {
        let line = r#"{"id":"","method":"x"}"#;
        let frame = parse_inbound(line).expect("notification");
        assert!(matches!(frame, InboundFrame::Notification { .. }));
    }

    #[test]
    fn supervisor_skips_provides_that_collide_with_built_ins() {
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(ActionRegistry::new());
        // Pre-register a built-in that the plugin tries to override.
        registry.register("system.ping", |_| Ok(json!({"status": "core"})));

        let plugin = mk_plugin(
            "rogue",
            vec![mk_service("main", &["system.ping", "rogue.do"])],
        );
        let _sup = ServiceSupervisor::new(bus, registry.clone(), &[plugin], "test", &[]);
        // Built-in is untouched; plugin's other action still registers.
        assert_eq!(
            registry.invoke("system.ping", json!({})).unwrap(),
            json!({"status": "core"})
        );
        assert!(registry.has("rogue.do"));
    }

    #[test]
    fn supervisor_skips_provides_that_collide_with_existing_plugin_command() {
        use nestty_core::plugin::PluginCommandDef;
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(ActionRegistry::new());

        // An existing `[[commands]]` plugin claiming `plugin.hello.greet`.
        let cmds_plugin = LoadedPlugin {
            manifest: PluginManifest {
                plugin: PluginMeta {
                    name: "hello".into(),
                    title: "Hello".into(),
                    version: "0.1.0".into(),
                    description: None,
                },
                panels: vec![],
                commands: vec![PluginCommandDef {
                    name: "greet".into(),
                    exec: "true".into(),
                    description: None,
                }],
                modules: vec![],
                services: vec![],
            },
            dir: PathBuf::from("/tmp"),
        };
        // A rogue service plugin trying to shadow `plugin.hello.greet`.
        let rogue = mk_plugin(
            "rogue",
            vec![mk_service("main", &["plugin.hello.greet", "rogue.do"])],
        );

        let _sup =
            ServiceSupervisor::new(bus, registry.clone(), &[cmds_plugin, rogue], "test", &[]);
        // The shell-command name stays unclaimed in the registry so
        // `socket::dispatch` falls through to `handle_plugin_command`.
        assert!(!registry.has("plugin.hello.greet"));
        // Non-conflicting plugin action still registers.
        assert!(registry.has("rogue.do"));
    }

    #[test]
    fn supervisor_skips_provides_that_collide_with_extra_reserved() {
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(ActionRegistry::new());
        let plugin = mk_plugin("rogue", vec![mk_service("main", &["tab.new", "rogue.do"])]);
        // `tab.new` lives in `socket::dispatch`'s match arm, not in the
        // registry, but must still be reserved so a plugin can't shadow
        // it via the registry-first dispatch lookup.
        let _sup = ServiceSupervisor::new(bus, registry.clone(), &[plugin], "test", &["tab.new"]);
        assert!(!registry.has("tab.new"));
        assert!(registry.has("rogue.do"));
    }

    #[test]
    fn supervisor_skips_system_spawn_provides_via_trigger_only_reserved() {
        // Trust-boundary regression guard for WR-2: `system.spawn` is
        // a trigger-only intercept (LiveTriggerSink). It MUST stay
        // unreachable through the registry, otherwise `nestctl call
        // system.spawn` from any process holding NESTTY_SOCKET would
        // get arbitrary process exec. A malicious or buggy plugin
        // declaring `provides = ["system.spawn"]` would defeat that
        // unless the supervisor reserves the name. The `extra_reserved`
        // arg is what window.rs threads `TRIGGER_ONLY_RESERVED_METHODS`
        // through.
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(ActionRegistry::new());
        let plugin = mk_plugin(
            "rogue",
            vec![mk_service("main", &["system.spawn", "rogue.do"])],
        );
        let _sup =
            ServiceSupervisor::new(bus, registry.clone(), &[plugin], "test", &["system.spawn"]);
        assert!(
            !registry.has("system.spawn"),
            "system.spawn MUST stay trigger-only — supervisor should drop the conflicting provide"
        );
        assert!(registry.has("rogue.do"));
    }

    #[test]
    fn invoke_remote_gates_onaction_glob_when_stopped() {
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(ActionRegistry::new());
        let plugin = mk_plugin(
            "kb",
            vec![PluginServiceDef {
                name: "main".into(),
                exec: "/does/not/exist".into(),
                args: vec![],
                activation: Activation::OnAction("kb.search".into()),
                restart: RestartPolicy::Never,
                provides: vec!["kb.search".into(), "kb.read".into()],
                subscribes: vec![],
            }],
        );
        let sup = ServiceSupervisor::new(bus, registry, &[plugin], "test", &[]);
        let svc = sup.services.lock().unwrap()[0].clone();
        // onAction services don't auto-spawn at boot; state should be
        // Stopped, but be defensive against future changes.
        wait_for_stopped_or_failed(&svc);
        // kb.read is approved by manifest but does NOT match the
        // activation glob — must refuse to spawn.
        let err = sup
            .invoke_remote(&svc, "kb.read", json!({}))
            .expect_err("kb.read should be gated by onAction:kb.search");
        assert_eq!(err.code, "service_unavailable");
    }

    fn wait_for_stopped_or_failed(svc: &Arc<ServiceHandle>) {
        // The boot-time spawn worker for `onStartup` services briefly
        // flips state to `Starting` before discovering the exec is
        // bogus and settling at `Failed`. Poll instead of racing.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(
                *svc.state.lock().unwrap(),
                ServiceState::Stopped | ServiceState::Failed
            ) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn invoke_remote_refuses_to_spawn_onstartup_service_from_action() {
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(ActionRegistry::new());
        let plugin = mk_plugin(
            "calendar",
            vec![PluginServiceDef {
                name: "main".into(),
                exec: "/does/not/exist".into(),
                args: vec![],
                activation: Activation::OnStartup,
                restart: RestartPolicy::Never,
                provides: vec!["calendar.next".into()],
                subscribes: vec![],
            }],
        );
        let sup = ServiceSupervisor::new(bus, registry, &[plugin], "test", &[]);
        let svc = sup.services.lock().unwrap()[0].clone();
        wait_for_stopped_or_failed(&svc);
        let err = sup
            .invoke_remote(&svc, "calendar.next", json!({}))
            .expect_err("onStartup service shouldn't be revived by an action");
        assert_eq!(err.code, "service_unavailable");
    }

    #[test]
    fn invoke_remote_refuses_to_spawn_onevent_service_from_action() {
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(ActionRegistry::new());
        let plugin = mk_plugin(
            "slack",
            vec![PluginServiceDef {
                name: "main".into(),
                exec: "/does/not/exist".into(),
                args: vec![],
                activation: Activation::OnEvent("slack.*".into()),
                restart: RestartPolicy::Never,
                provides: vec!["slack.draft_reply".into()],
                subscribes: vec![],
            }],
        );
        let sup = ServiceSupervisor::new(bus, registry, &[plugin], "test", &[]);
        let svc = sup.services.lock().unwrap()[0].clone();
        // onEvent never auto-spawns at boot, but be defensive.
        wait_for_stopped_or_failed(&svc);
        let err = sup
            .invoke_remote(&svc, "slack.draft_reply", json!({}))
            .expect_err("onEvent service shouldn't be revived by an action");
        assert_eq!(err.code, "service_unavailable");
    }

    #[test]
    fn invoke_remote_rejects_action_outside_runtime_set() {
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(ActionRegistry::new());
        let plugin = mk_plugin(
            "kb",
            vec![PluginServiceDef {
                name: "main".into(),
                exec: "/does/not/exist".into(),
                args: vec![],
                activation: Activation::OnAction("kb.*".into()),
                restart: RestartPolicy::Never,
                provides: vec!["kb.search".into(), "kb.read".into()],
                subscribes: vec![],
            }],
        );
        let sup = ServiceSupervisor::new(bus, registry.clone(), &[plugin], "test", &[]);
        // Both manifest-approved actions are registered.
        assert!(registry.has("kb.search"));
        assert!(registry.has("kb.read"));

        // Simulate a successful init that announced ONLY kb.read at
        // runtime — the degraded-mode case the doc describes.
        let svc = sup.services.lock().unwrap()[0].clone();
        *svc.runtime_provides.lock().unwrap() = Some(["kb.read".to_string()].into_iter().collect());

        let err = sup
            .invoke_remote(&svc, "kb.search", json!({}))
            .expect_err("kb.search should be gated post-init");
        assert_eq!(err.code, "service_degraded");
    }

    #[test]
    fn backoff_grows_then_caps() {
        let mut b = BackoffState::new();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
        assert_eq!(b.next_delay(), Duration::from_secs(2));
        assert_eq!(b.next_delay(), Duration::from_secs(4));
        assert_eq!(b.next_delay(), Duration::from_secs(8));
        for _ in 0..20 {
            let d = b.next_delay();
            assert!(d <= BACKOFF_CAP);
        }
    }
}
