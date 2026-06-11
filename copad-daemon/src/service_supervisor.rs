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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use copad_core::action_registry::{ActionRegistry, ActionResult, internal_error};
use copad_core::event_bus::{Event as BusEvent, EventBus, pattern_matches};
use copad_core::plugin::{Activation, LoadedPlugin, PluginServiceDef, RestartPolicy};
use copad_core::protocol::{Request, Response, ResponseError};

/// Announced to plugins in the `initialize` request. Plugins decide whether
/// to accept; rejecting (e.g. with `protocol_mismatch`) makes init fail and
/// the supervisor kills the child. Bump on backwards-incompatible RPC changes.
pub const PROTOCOL_VERSION: u32 = 1;

const DEFAULT_INIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Single global action-reply timeout. Sized to accommodate long-running
/// LLM completions (10-90s).
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
    /// Pending requests copad has sent to the service, awaiting reply.
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
    // Sort by `[plugin].name` then by `dir` as a stable tiebreaker.
    // The loader doesn't reject duplicate manifest names; without the
    // secondary key, ordering would depend on filesystem enumeration
    // (differs across runs/machines). Duplicates surface as separate
    // warnings so the user notices.
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
                "[copad] duplicate plugin name {:?} at {:?} and {:?} — using directory path as tiebreaker; consider renaming one",
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

type SpawnRequest = Arc<ServiceHandle>;

pub struct ServiceSupervisor {
    bus: Arc<EventBus>,
    registry: Arc<ActionRegistry>,
    services: Mutex<Vec<Arc<ServiceHandle>>>,
    copad_version: String,
    /// Bound daemon socket path. Injected into every service plugin
    /// child as `COPAD_SOCKET=<path>` so plugins that need raw
    /// daemon-socket access (the path that goes through `dispatch()`
    /// → `GuiRegistry` for GUI-owned methods like `terminal.read`)
    /// can connect without rediscovery. Standard service-plugin RPC
    /// (stdin/stdout) does not require this — it's purely for the
    /// long-running listener pattern (web-bridge, future ntfy bridge,
    /// prometheus exporter, …).
    socket_path: PathBuf,
    init_timeout: Duration,
    action_timeout: Duration,
    on_event_rules: Vec<(String, Arc<ServiceHandle>)>,
    shutting_down: AtomicBool,
    /// All spawns route here so every `fork()` happens on the same OS
    /// thread; that makes `PR_SET_PDEATHSIG(SIGTERM)` fire on real daemon
    /// death rather than per-spawn worker exit. `None` post-shutdown.
    spawner_tx: Mutex<Option<Sender<SpawnRequest>>>,
    spawner_thread: Mutex<Option<JoinHandle<()>>>,
    /// Bound on concurrent in-flight waiters for `dispatch_invocation`.
    /// Pre-Phase 9.5: each invocation `thread::spawn`'d an unbounded
    /// waiter. Now admission goes through a counter (atomic) — saturation
    /// returns `overloaded` immediately without sending the invoke to the
    /// service. NOT a queueing pool: every admitted waiter spawns a thread
    /// instantly, so the daemon never buffers a response behind a
    /// not-yet-picked-up job. Counter-bounded `thread::spawn` is the
    /// right primitive here because waiters spend their lifetime blocked
    /// on `recv_timeout` — pool worker reuse would just add scheduling
    /// latency without saving real work.
    waiter_active: Arc<AtomicUsize>,
    waiter_max: usize,
}

impl ServiceSupervisor {
    pub fn new(
        bus: Arc<EventBus>,
        registry: Arc<ActionRegistry>,
        plugins: &[LoadedPlugin],
        copad_version: impl Into<String>,
        extra_reserved: &[&str],
        socket_path: PathBuf,
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
                "[copad] service conflict: action {:?} taken by {:?}; skipped for: {:?}",
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
                                "[copad] service {}::{} declared {:?} but the name is reserved by a built-in; skipping",
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
            copad_version: copad_version.into(),
            socket_path,
            init_timeout: DEFAULT_INIT_TIMEOUT,
            action_timeout: DEFAULT_ACTION_TIMEOUT,
            on_event_rules,
            shutting_down: AtomicBool::new(false),
            spawner_tx: Mutex::new(None),
            spawner_thread: Mutex::new(None),
            waiter_active: Arc::new(AtomicUsize::new(0)),
            waiter_max: env_waiter_max("COPADD_WAITER_MAX", 64),
        });

        // Spawner must be alive before any onEvent / onStartup activation
        // below sends a SpawnRequest. `Weak<Self>` so the spawner doesn't
        // keep the supervisor alive past `shutdown_all`'s join.
        let (spawner_tx, spawner_rx) = channel::<SpawnRequest>();
        let sup_weak = Arc::downgrade(&supervisor);
        let spawner_thread = thread::Builder::new()
            .name("copad-spawner".into())
            .spawn(move || spawner_loop(sup_weak, spawner_rx))
            .expect("spawn copad-spawner thread");
        *supervisor.spawner_tx.lock().unwrap() = Some(spawner_tx);
        *supervisor.spawner_thread.lock().unwrap() = Some(spawner_thread);

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
        // The very first event that activates the service is dropped
        // from delivery: post-init `subscribes` forwarders subscribe
        // AFTER init completes, so an event that arrived during init is
        // gone by the time they exist. Workaround: declare the glob in
        // both `provides` and `subscribes`.
        //
        // `Failed` is included alongside `Stopped` so init failures
        // don't permanently inert an event-activated service (lifecycle
        // contract: activation re-arms `Failed`).
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
        let tx_guard = self.spawner_tx.lock().unwrap();
        if let Some(tx) = tx_guard.as_ref() {
            let _ = tx.send(handle);
        }
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
            .env("COPAD_PLUGIN_NAME", &handle.plugin_name)
            .env(
                "COPAD_PLUGIN_DIR",
                handle.plugin_dir.to_string_lossy().as_ref(),
            )
            .env("COPAD_SERVICE_NAME", &handle.service_name)
            .env("COPAD_SOCKET", self.socket_path.to_string_lossy().as_ref())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // pdeathsig only fires correctly because every fork happens on
        // the long-lived `copad-spawner` thread (set up in `new`).
        // macOS has no equivalent — `shutdown_all` is the only path
        // there, plus the orphan-sweep at next launch that uses the
        // pid file written below.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: pre_exec runs post-fork in the child. `prctl` and
            // `getppid` are async-signal-safe; no allocs / locks.
            unsafe {
                cmd.pre_exec(|| {
                    let rc = libc::prctl(
                        libc::PR_SET_PDEATHSIG,
                        libc::SIGTERM as libc::c_ulong,
                        0_u64,
                        0_u64,
                        0_u64,
                    );
                    if rc != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Closes fork→prctl TOCTOU: parent died in the
                    // window and we got reparented to init.
                    if libc::getppid() == 1 {
                        libc::_exit(129);
                    }
                    Ok(())
                });
            }
        }
        // macOS: no PDEATHSIG. Put the child in its own process group
        // (pgid == child pid) so `shutdown_all` can `killpg` the
        // entire group, reaching plugin grandchildren too. Combined
        // with the post-spawn pid-file write + `sweep_orphan_pids`
        // at startup, SIGKILL of the daemon stops leaking orphan
        // plugin processes between sessions.
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: `setpgid` is async-signal-safe and only mutates
            // the calling (post-fork, pre-exec) process's pgid.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
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
        // macOS-only: write the pid (== pgid post `setpgid(0,0)`) to
        // a per-service file under the state dir. `sweep_orphan_pids`
        // at next launch reads these to find and kill plugin process
        // groups left behind by a SIGKILL'd or panicked daemon. Linux
        // doesn't need this — PDEATHSIG already covers the parent-died
        // case. Best-effort: a write failure logs but doesn't abort
        // the spawn, since the only downside is a one-time orphan on
        // an out-of-band daemon death.
        #[cfg(target_os = "macos")]
        write_pid_file(&handle.plugin_name, &handle.service_name, pid);

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
                "copad_version": self.copad_version,
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
                // macOS: also SIGKILL the whole pgroup + remove pid
                // file so grandchildren of a misbehaving init don't
                // survive and the next-launch sweep stays clean.
                kill_service_group(pid, &handle.plugin_name, &handle.service_name);
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
            kill_service_group(pid, &handle.plugin_name, &handle.service_name);
            return Err(response.error.unwrap_or_else(|| ResponseError {
                code: "init_failed".into(),
                message: "service rejected initialize".into(),
            }));
        }
        let plugin_name = handle.plugin_name.clone();
        let service_name = handle.service_name.clone();
        let result = response.result.ok_or_else(|| {
            kill_service_group(pid, &plugin_name, &service_name);
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
                    "[copad] service {} runtime declared unauthorized provide {:?}; ignoring",
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
                    "[copad] service {} runtime declared unauthorized subscribe {:?}; ignoring",
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
            "[copad] service {} ready (provides={:?}, subscribes={:?})",
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
                use copad_core::event_bus::RecvOutcome;
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
            "[copad] service {} exited (status={:?}, was_running={}, restart={})",
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
        // Backoff sleep lives on this transient thread; the actual fork
        // must go through `spawn_service_async` so pdeathsig binds to
        // the long-lived spawner thread, not us.
        thread::spawn(move || {
            thread::sleep(delay);
            sup.spawn_service_async(h);
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
        // Counter admission: every admitted waiter spawns its own thread
        // immediately, so there's no buffered "queued but not running"
        // state where a service response could land into a parked
        // `resp_rx` faster than the waiter thread is up to receive it.
        // Pre-fix this was unbounded `thread::spawn` per invoke; the
        // counter caps growth without introducing the buffering hazard
        // that ThreadPool's queue would.
        let cur = self.waiter_active.fetch_add(1, Ordering::SeqCst);
        if cur >= self.waiter_max {
            self.waiter_active.fetch_sub(1, Ordering::SeqCst);
            let _ = pending.reply.send(Err(ResponseError {
                code: "overloaded".into(),
                message: format!(
                    "supervisor waiter capacity {} reached; {} invocation rejected",
                    self.waiter_max,
                    handle.fq_name(),
                ),
            }));
            return;
        }
        let _permit = WaiterPermit::new(self.waiter_active.clone());

        let req_id = handle.next_request_id();
        let (resp_tx, resp_rx) = channel::<Response>();
        handle
            .pending_responses
            .lock()
            .unwrap()
            .insert(req_id.clone(), resp_tx);

        // Send the invoke synchronously on the caller's thread. Admission
        // is reserved (permit held by `_permit`), so saturation cannot
        // race the send.
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
            return; // permit drops, slot released
        }

        // Spawn the waiter thread. Permit moves into the thread; counter
        // decrements on thread exit (success, timeout, or panic). Clone
        // `reply` so we can wake the caller from the caller's own thread
        // if `thread::Builder::spawn` itself fails (e.g. system thread
        // limit). Without the fallback, the service might still run the
        // action but the caller would wait the full `action_timeout`
        // before hearing back, possibly retrying and double-firing.
        let reply = pending.reply;
        let reply_fallback = reply.clone();
        let timeout = self.action_timeout;
        let svc_label = handle.fq_name();
        let cleanup_handle = handle.clone();
        let cleanup_id = req_id.clone();
        let permit = _permit;
        let spawn_handle = handle.clone();
        let spawn_id = req_id.clone();
        if let Err(e) = thread::Builder::new()
            .name("copad-supervisor-waiter".into())
            .spawn(move || {
                let _permit_drop = permit;
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
            })
        {
            // Closure was consumed (and so was the permit → slot
            // released). The pending_responses entry stays in case the
            // service replies anyway (reader logs an "unknown id" warn
            // on receipt). Wake the caller immediately rather than
            // letting it block on reply_rx for action_timeout.
            spawn_handle
                .pending_responses
                .lock()
                .unwrap()
                .remove(&spawn_id);
            let _ = reply_fallback.send(Err(ResponseError {
                code: "spawn_failed".into(),
                message: format!("supervisor failed to spawn waiter thread: {e}"),
            }));
        }
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
                    &format!("unknown service→copad request method: {other}"),
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

    /// Idempotent. Stages: set `shutting_down`, send JSON `shutdown`,
    /// 200ms grace, drain+join spawner (pdeathsig SIGTERM), 200ms
    /// SIGTERM-handler grace, SIGKILL sweep.
    ///
    /// Worst-case latency ~5.4s if the spawner is mid-`start_service`
    /// (init_timeout 5s blocks join); typical ~400ms.
    pub fn shutdown_all(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);

        // Cooperative shutdown must precede the spawner join: joining
        // triggers pdeathsig SIGTERM on every still-running plugin, and
        // we want them to receive the JSON `shutdown` first.

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

        // Drop sender + join spawner. join() can block up to
        // init_timeout (5s) if the spawner is mid-`start_service`;
        // see fn doc-comment for worst-case latency.
        {
            let mut tx_guard = self.spawner_tx.lock().unwrap();
            *tx_guard = None;
        }
        if let Some(handle) = self.spawner_thread.lock().unwrap().take()
            && let Err(e) = handle.join()
        {
            eprintln!("[copad] spawner thread join failed: {e:?}");
        }

        // SIGTERM-handler grace — joining the spawner just fired
        // pdeathsig SIGTERM on every Linux survivor. On macOS we
        // explicitly killpg(SIGTERM) here, then 200ms wait, then
        // SIGKILL the leader (or whole group on macOS) below.
        #[cfg(target_os = "macos")]
        {
            for handle in &services {
                if let Some(pid) = *handle.child_pid.lock().unwrap() {
                    killpg_signal(pid, libc::SIGTERM);
                }
            }
        }
        thread::sleep(Duration::from_millis(200));

        for handle in &services {
            if let Some(pid) = *handle.child_pid.lock().unwrap() {
                kill_service_group(pid, &handle.plugin_name, &handle.service_name);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn pid_files_dir() -> std::path::PathBuf {
    copad_core::paths::state_dir().join("plugin-pids")
}

#[cfg(target_os = "macos")]
fn pid_file_path(plugin: &str, service: &str) -> std::path::PathBuf {
    // `<plugin>.<service>.pid`. service is typically `main`; the
    // filename is intentionally flat (no subdirs) to keep the sweep
    // a single readdir.
    pid_files_dir().join(format!("{plugin}.{service}.pid"))
}

#[cfg(target_os = "macos")]
fn write_pid_file(plugin: &str, service: &str, pid: u32) {
    let dir = pid_files_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!(
            "[copad] pid-file dir create failed ({}): {e}",
            dir.display()
        );
        return;
    }
    let path = pid_file_path(plugin, service);
    if let Err(e) = std::fs::write(&path, pid.to_string()) {
        log::warn!("[copad] pid-file write failed ({}): {e}", path.display());
    }
}

#[cfg(target_os = "macos")]
fn remove_pid_file(plugin: &str, service: &str) {
    let path = pid_file_path(plugin, service);
    if let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("[copad] pid-file remove failed ({}): {e}", path.display());
    }
}

#[cfg(target_os = "macos")]
fn killpg_signal(pid: u32, sig: libc::c_int) {
    // SAFETY: `killpg` only requires a pgid and signal; we passed
    // `setpgid(0, 0)` post-fork so child pid == pgid. Signal goes to
    // every process in the group including plugin grandchildren.
    unsafe {
        libc::killpg(pid as libc::pid_t, sig);
    }
}

/// macOS-only: at supervisor startup, walk the pid-files dir and
/// kill any process group whose pid file references a still-living
/// pgid. A live orphan here is by definition NOT owned by us (we
/// haven't started spawning yet), so it must be a leftover from a
/// SIGKILL'd or panicked prior daemon run — safe to terminate.
/// Files are removed regardless of whether the pgid was alive.
#[cfg(target_os = "macos")]
pub fn sweep_orphan_pids() {
    sweep_orphan_pids_in(&pid_files_dir());
}

/// Inner implementation that takes an explicit dir — `sweep_orphan_pids`
/// is the production caller. Lifted out so tests can isolate via a
/// per-test temp dir without racing on `env::set_var("HOME", ...)`.
#[cfg(target_os = "macos")]
fn sweep_orphan_pids_in(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut swept = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("pid") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(pid) = content.trim().parse::<i32>() else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        // Defensive: a malformed file containing `0` (or a negative)
        // would otherwise turn `kill(-pid, 0)` into `kill(0, 0)`,
        // which targets the DAEMON'S OWN process group, and the
        // followup `killpg(0, SIGTERM)` would actually signal us.
        // Treat any non-positive pid as garbage, remove the file,
        // and move on.
        if pid <= 0 {
            log::warn!(
                "[copad] orphan pid file {} contains non-positive pid {pid}; removing",
                path.display()
            );
            let _ = std::fs::remove_file(&path);
            continue;
        }
        // `kill(-pgid, 0)` returns 0 if the group has any live
        // member, -1 otherwise. We negate `pid` to address the
        // pgid since `setpgid(0, 0)` made pid == pgid for the
        // plugin's leader.
        let alive = unsafe { libc::kill(-pid, 0) } == 0;
        if alive {
            log::warn!(
                "[copad] orphan plugin pgid {pid} from {} — sending SIGTERM then SIGKILL",
                path.file_name().and_then(|s| s.to_str()).unwrap_or("?")
            );
            unsafe {
                libc::killpg(pid, libc::SIGTERM);
            }
            // Short grace then force-kill — orphans don't get to
            // negotiate. 200ms matches `shutdown_all`'s rhythm.
            std::thread::sleep(Duration::from_millis(200));
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
            swept += 1;
        }
        let _ = std::fs::remove_file(&path);
    }
    if swept > 0 {
        log::info!("[copad] swept {swept} orphan plugin process group(s)");
    }
}

fn env_waiter_max(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n >= 1)
        .unwrap_or(default)
}

/// Drop guard for the `waiter_active` counter — released when the
/// waiter thread exits (regardless of success / timeout / panic).
struct WaiterPermit(Arc<AtomicUsize>);

impl WaiterPermit {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        Self(counter)
    }
}

impl Drop for WaiterPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Receives `SpawnRequest`s, brings services up via `start_service`.
/// Lives until `shutdown_all` drops the Sender. The post-`start_service`
/// `shutting_down` re-check kills any newborn child that shutdown raced
/// past — without it the pid sweep would miss it.
fn spawner_loop(sup: Weak<ServiceSupervisor>, rx: Receiver<SpawnRequest>) {
    while let Ok(handle) = rx.recv() {
        let Some(sup) = sup.upgrade() else {
            return;
        };
        if sup.shutting_down.load(Ordering::SeqCst) {
            continue;
        }
        if let Err(e) = sup.start_service(&handle) {
            eprintln!(
                "[copad] service {} failed to start: {}",
                handle.fq_name(),
                e.message
            );
        }
        if sup.shutting_down.load(Ordering::SeqCst)
            && let Some(pid) = *handle.child_pid.lock().unwrap()
        {
            eprintln!(
                "[copad] service {} spawned during shutdown; killing pid {pid}",
                handle.fq_name()
            );
            kill_service_group(pid, &handle.plugin_name, &handle.service_name);
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
            target_client_id: None,
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

/// Symmetric kill+cleanup for a single plugin service.
/// - macOS: `killpg(SIGKILL)` to catch grandchildren in the same
///   pgroup the spawn-time `setpgid(0,0)` established, then the
///   per-service pid file is removed so the next-launch
///   `sweep_orphan_pids` doesn't see a stale (dead-pgid) entry that
///   takes a startup tick to GC anyway.
/// - Linux: just `kill_child` — PDEATHSIG handles grandchildren
///   transitively at parent exit, and no pid file was ever written.
///
/// Used by every macOS init-failure and shutdown-race path so the
/// cleanup is consistent. The normal `shutdown_all` flow does the
/// SIGTERM grace + SIGKILL + remove_pid_file inline (mirrors this
/// helper but interleaves a 200ms wait between SIGTERM and SIGKILL).
fn kill_service_group(pid: u32, _plugin: &str, _service: &str) {
    #[cfg(target_os = "macos")]
    {
        killpg_signal(pid, libc::SIGKILL);
        remove_pid_file(_plugin, _service);
    }
    kill_child(pid);
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
    use copad_core::plugin::{
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
    fn spawner_thread_started_on_new_and_joined_on_shutdown() {
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(ActionRegistry::new());
        let sup = ServiceSupervisor::new(
            bus,
            registry,
            &[],
            "test",
            &[],
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
        );
        assert!(
            sup.spawner_tx.lock().unwrap().is_some(),
            "spawner sender stored on supervisor::new"
        );
        assert!(
            sup.spawner_thread.lock().unwrap().is_some(),
            "spawner thread handle stored on supervisor::new"
        );
        sup.shutdown_all();
        assert!(
            sup.spawner_tx.lock().unwrap().is_none(),
            "shutdown_all took the spawner sender"
        );
        assert!(
            sup.spawner_thread.lock().unwrap().is_none(),
            "shutdown_all joined and took the spawner handle"
        );
    }

    #[test]
    fn shutdown_all_is_idempotent() {
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(ActionRegistry::new());
        let sup = ServiceSupervisor::new(
            bus,
            registry,
            &[],
            "test",
            &[],
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
        );
        sup.shutdown_all();
        sup.shutdown_all(); // must not panic
        assert!(sup.spawner_tx.lock().unwrap().is_none());
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
        let _sup = ServiceSupervisor::new(
            bus,
            registry.clone(),
            &[plugin],
            "test",
            &[],
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
        );
        // Built-in is untouched; plugin's other action still registers.
        assert_eq!(
            registry.invoke("system.ping", json!({})).unwrap(),
            json!({"status": "core"})
        );
        assert!(registry.has("rogue.do"));
    }

    #[test]
    fn supervisor_skips_provides_that_collide_with_existing_plugin_command() {
        use copad_core::plugin::PluginCommandDef;
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

        let _sup = ServiceSupervisor::new(
            bus,
            registry.clone(),
            &[cmds_plugin, rogue],
            "test",
            &[],
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
        );
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
        let _sup = ServiceSupervisor::new(
            bus,
            registry.clone(),
            &[plugin],
            "test",
            &["tab.new"],
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
        );
        assert!(!registry.has("tab.new"));
        assert!(registry.has("rogue.do"));
    }

    #[test]
    fn supervisor_skips_system_spawn_provides_via_trigger_only_reserved() {
        // Trust-boundary regression guard for WR-2: `system.spawn` is
        // a trigger-only intercept (LiveTriggerSink). It MUST stay
        // unreachable through the registry, otherwise `coctl call
        // system.spawn` from any process holding COPAD_SOCKET would
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
        let _sup = ServiceSupervisor::new(
            bus,
            registry.clone(),
            &[plugin],
            "test",
            &["system.spawn"],
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
        );
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
        let sup = ServiceSupervisor::new(
            bus,
            registry,
            &[plugin],
            "test",
            &[],
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
        );
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
        let sup = ServiceSupervisor::new(
            bus,
            registry,
            &[plugin],
            "test",
            &[],
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
        );
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
        let sup = ServiceSupervisor::new(
            bus,
            registry,
            &[plugin],
            "test",
            &[],
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
        );
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
        let sup = ServiceSupervisor::new(
            bus,
            registry.clone(),
            &[plugin],
            "test",
            &[],
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
        );
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
    fn waiter_permit_decrements_on_drop() {
        let counter = Arc::new(AtomicUsize::new(5));
        {
            let _p = WaiterPermit::new(counter.clone());
            // Mimic the dispatch_invocation increment (caller bumped to 6
            // before constructing the permit; permit's role is to undo
            // exactly one increment on drop).
        }
        assert_eq!(counter.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn waiter_permit_decrements_once_even_under_panic() {
        let counter = Arc::new(AtomicUsize::new(1));
        let c2 = counter.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _p = WaiterPermit::new(c2);
            panic!("simulated worker panic");
        }));
        assert!(result.is_err());
        // Drop fired on unwind → decrement happened.
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn env_waiter_max_falls_back_to_default_on_invalid() {
        // Default path (env not set).
        let n = env_waiter_max("__COPADD_WAITER_MAX_NOT_SET_AAA", 64);
        assert_eq!(n, 64);
        // Filter rejects 0 — would otherwise allow zero concurrency.
        unsafe { std::env::set_var("__COPADD_WAITER_MAX_TEST_ZERO", "0") };
        let n = env_waiter_max("__COPADD_WAITER_MAX_TEST_ZERO", 64);
        assert_eq!(n, 64);
        // Valid override.
        unsafe { std::env::set_var("__COPADD_WAITER_MAX_TEST_OK", "12") };
        let n = env_waiter_max("__COPADD_WAITER_MAX_TEST_OK", 64);
        assert_eq!(n, 12);
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

    /// macOS-only: dead-pgid + malformed-content + non-.pid branches.
    /// Live-orphan kill path is covered by
    /// `sweep_kills_live_orphan_pgid` below.
    #[cfg(target_os = "macos")]
    #[test]
    fn sweep_removes_pid_file_for_dead_pgid_and_malformed() {
        // Use a temp HOME so the sweep walks an isolated dir.
        let tmp = std::env::temp_dir().join(format!(
            "copad-sweep-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let pids_dir = tmp.join("plugin-pids");
        std::fs::create_dir_all(&pids_dir).unwrap();
        // Dead pgid: 999999 — almost certainly not assigned, kill(0)
        // returns ESRCH, sweep removes file.
        let dead_path = pids_dir.join("ghost.main.pid");
        std::fs::write(&dead_path, "999999").unwrap();
        // Malformed: non-numeric content, sweep removes file silently.
        let bad_path = pids_dir.join("malformed.main.pid");
        std::fs::write(&bad_path, "not-a-pid").unwrap();
        // Non-pid extension: sweep ignores entirely (left behind).
        let leave_path = pids_dir.join("README.txt");
        std::fs::write(&leave_path, "doc").unwrap();

        super::sweep_orphan_pids_in(&pids_dir);

        assert!(!dead_path.exists(), "dead-pgid pid file should be removed");
        assert!(!bad_path.exists(), "malformed pid file should be removed");
        assert!(leave_path.exists(), "non-.pid file should be ignored");

        // C1 defense: pid file containing 0 or a negative pid must
        // not turn the sweep into a `killpg(0, SIGTERM)` self-signal.
        // The file is removed and the daemon process group is left
        // untouched.
        let zero_path = pids_dir.join("zero.main.pid");
        std::fs::write(&zero_path, "0").unwrap();
        let neg_path = pids_dir.join("neg.main.pid");
        std::fs::write(&neg_path, "-42").unwrap();
        super::sweep_orphan_pids_in(&pids_dir);
        assert!(
            !zero_path.exists(),
            "zero-pid file should be removed without signaling"
        );
        assert!(
            !neg_path.exists(),
            "negative-pid file should be removed without signaling"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// macOS-only: spawn a real `sleep` child in its own process
    /// group, write its pid to a pid file, run sweep, assert the
    /// child was killed and the file was removed. Covers the
    /// `kill(-pgid, 0) == 0` branch — the one that actually matters
    /// for orphan cleanup after a SIGKILL'd daemon.
    #[cfg(target_os = "macos")]
    #[test]
    fn sweep_kills_live_orphan_pgid() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        let tmp = std::env::temp_dir().join(format!(
            "copad-sweep-live-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let pids_dir = tmp.join("plugin-pids");
        std::fs::create_dir_all(&pids_dir).unwrap();

        // Spawn `sleep 60` in its own pgroup so the sweep's
        // `kill(-pgid, ...)` targets a real, isolated process.
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn sleep");
        let pid = child.id() as i32;

        // Wait a beat for the pgid to settle.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            unsafe { libc::kill(-pid, 0) },
            0,
            "pre-sweep: sleep's pgroup should be alive"
        );

        let pid_file = pids_dir.join("orphan-test.main.pid");
        std::fs::write(&pid_file, pid.to_string()).unwrap();

        super::sweep_orphan_pids_in(&pids_dir);

        // 200ms grace (per sweep impl) + small buffer for SIGKILL to
        // land + the child to be reaped.
        std::thread::sleep(Duration::from_millis(400));
        // After sweep + SIGKILL, the pgroup should be dead.
        let still_alive = unsafe { libc::kill(-pid, 0) } == 0;
        assert!(!still_alive, "post-sweep: orphan pgroup should be killed");
        assert!(!pid_file.exists(), "pid file should be removed after sweep");

        // Reap the zombie via Child::wait so the Child handle's
        // lifetime is clean (satisfies clippy::zombie_processes and
        // gives us a reaped status to inspect).
        let _ = child.wait();

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
