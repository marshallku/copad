//! Socket transport for copadd: bind well-known path, accept connections,
//! route requests through `ActionRegistry`.

use std::fs;
use std::io::{BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use copad_core::action_registry::{ActionRegistry, COMPLETION_EVENT_SOURCE};
use copad_core::event_bus::{
    Event as BusEvent, EventBus as CoreEventBus, RecvOutcome, next_bridge_id, pattern_matches,
};
use copad_core::plugin::LoadedPlugin;
use copad_core::protocol::{Event as WireEvent, PROTOCOL_VERSION, Request, Response};
use serde_json::Value;

use crate::gui_registry::{GuiRegistry, method_capability, method_invoke_timeout};

pub type EventBus = Arc<CoreEventBus>;

/// Legacy match-arm methods whose `<method>.completed` publication would
/// dwarf real workflow events on the bus. These are read-only / agent-
/// polled paths; their "completion" is the response itself. Mirror of
/// `register_silent` for the legacy dispatch surface. copad-linux's
/// socket::dispatch wrapper + the daemon-side `dispatch_via_gui` publish
/// helper both consult this list before emitting.
pub const LEGACY_SILENT_METHODS: &[&str] = &[
    "terminal.read",
    "terminal.state",
    "terminal.history",
    "terminal.context",
    "tab.list",
    "tab.info",
    "session.list",
    "session.info",
    // Read-only and pollable — without this, a client polling it over the
    // daemon emits a `.completed` event per call, which triggers can see.
    // (`webview.page_info` is missing here for the same reason; pre-existing,
    // left alone rather than widened into this change.)
    "webview.state",
];

pub fn is_legacy_silent(method: &str) -> bool {
    LEGACY_SILENT_METHODS.contains(&method)
}

impl SocketCommand {
    /// Publish `<method>.completed` / `<method>.failed` on `bus` (when
    /// the command is not marked silent and the method is not in
    /// [`LEGACY_SILENT_METHODS`]) and forward `resp` to the original
    /// caller. Convenience wrapper used by every copad-linux
    /// `socket::dispatch` legacy match arm so chained triggers see
    /// completion events uniformly across registry and legacy actions.
    pub fn reply_with_completion(&self, bus: &EventBus, resp: Response) {
        publish_legacy_completion(bus, &self.request.method, self.silent_completion, &resp);
        let _ = self.reply.send(resp);
    }
}

/// Free-function variant of [`SocketCommand::reply_with_completion`]'s
/// publish step. Used by callback-deferred handlers (webview JS exec,
/// agent.approve dialog) that move `cmd.reply` into a closure and can no
/// longer reach the SocketCommand method.
pub fn publish_legacy_completion(bus: &EventBus, method: &str, silent: bool, resp: &Response) {
    if silent || is_legacy_silent(method) {
        return;
    }
    let result = response_to_action_result(resp);
    copad_core::action_registry::publish_completion_unconditional(bus, method, &result);
}

fn response_to_action_result(resp: &Response) -> copad_core::action_registry::ActionResult {
    if resp.ok {
        Ok(resp.result.clone().unwrap_or(serde_json::Value::Null))
    } else {
        Err(resp
            .error
            .clone()
            .unwrap_or_else(|| copad_core::protocol::ResponseError {
                code: "unknown".into(),
                message: String::new(),
            }))
    }
}

/// GUI-owned methods that `ServiceSupervisor::new` reserves against plugin
/// `provides[]` claims so a plugin can't shadow a GUI handler. copad-linux
/// re-exports for its own dispatch match. Remove an entry when its method
/// migrates into `ActionRegistry`.
pub const LEGACY_DISPATCH_METHODS: &[&str] = &[
    "background.set",
    "background.clear",
    "background.next",
    "background.delete_current",
    "background.toggle",
    "background.set_tint",
    "tab.new",
    "tab.close",
    "tab.list",
    "tab.info",
    "tab.rename",
    "tab.switch",
    "tabs.toggle_bar",
    "split.horizontal",
    "split.vertical",
    "pane.focus_next",
    "pane.focus_prev",
    "session.list",
    "session.info",
    "webview.open",
    "webview.navigate",
    "webview.back",
    "webview.forward",
    "webview.reload",
    "webview.execute_js",
    "webview.get_content",
    "webview.screenshot",
    "webview.query",
    "webview.query_all",
    "webview.get_styles",
    "webview.click",
    "webview.fill",
    "webview.scroll",
    "webview.page_info",
    "webview.state",
    "webview.devtools",
    "terminal.read",
    "terminal.state",
    "terminal.exec",
    "terminal.feed",
    "terminal.history",
    "terminal.context",
    "agent.approve",
    "cockpit.open",
    "claude.start",
    "theme.list",
    "plugin.list",
    "plugin.open",
    "statusbar.show",
    "statusbar.hide",
    "statusbar.toggle",
];

pub fn new_event_bus() -> EventBus {
    Arc::new(CoreEventBus::new())
}

/// Socket request + reply channel, shared between the GUI's GTK main-loop
/// pump and the daemon's stdio plugin dispatch.
pub struct SocketCommand {
    pub request: Request,
    pub reply: mpsc::Sender<Response>,
    /// Set true when this SocketCommand was constructed by
    /// `gui_client::handle_invoke` (i.e., a daemon-Invoke proxy reaching
    /// the GUI's local dispatch). The dispatch wrapper skips local
    /// completion publishing in that case — the daemon already publishes
    /// on its own bus after `client.invoke` returns, and the daemon→GUI
    /// event forwarder bridges that event back so GUI-side triggers see
    /// it exactly once.
    pub silent_completion: bool,
}

const RUNTIME_DIR_MODE: u32 = 0o700;
const SOCKET_FILE_MODE: u32 = 0o600;

#[derive(Debug, PartialEq, Eq)]
pub enum SocketPrep {
    Fresh,
    StaleCleared,
    /// A live `copadd` is already listening — caller must not bind.
    InUse,
    /// Path exists but is not a Unix socket — refuse to unlink. Caller
    /// likely pointed `COPAD_SOCKET` at a regular file by mistake.
    NotSocket,
    /// Filesystem error while preparing.
    Error(String),
}

/// Idempotent. Creates parent dir (0700 atomically if it's our runtime
/// dir), verifies ownership, then probes the path. Only `ConnectionRefused`
/// on connect proves stale-ness; other errors → `Error` (refuse to unlink
/// a possibly-live socket). Non-socket inodes are never unlinked.
pub fn prepare_socket_path(path: &Path) -> SocketPrep {
    if let Some(parent) = path.parent() {
        let owns_parent = parent == copad_core::paths::runtime_dir();
        if owns_parent {
            // Atomic create with mode 0700 closes the umask window where
            // another local user could pre-create the predictable
            // `/tmp/copad-{uid}/` and slip a listener in.
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            builder.mode(RUNTIME_DIR_MODE);
            if let Err(e) = builder.create(parent) {
                return SocketPrep::Error(format!(
                    "create_dir_all({}, mode=0700): {e}",
                    parent.display()
                ));
            }

            // Atomic create no-ops on existing dirs — verify ownership in
            // case an attacker created it first.
            use std::os::unix::fs::MetadataExt;
            match fs::metadata(parent) {
                Ok(meta) => {
                    let current_uid = unsafe { libc::getuid() };
                    if meta.uid() != current_uid {
                        return SocketPrep::Error(format!(
                            "runtime dir {} not owned by uid {current_uid} (got uid={}); refusing to use — investigate before retrying",
                            parent.display(),
                            meta.uid()
                        ));
                    }
                }
                Err(e) => {
                    return SocketPrep::Error(format!("stat({}): {e}", parent.display()));
                }
            }

            // Repair perms on a dir created by an older copadd that
            // didn't use the atomic-mode path.
            if let Err(e) =
                fs::set_permissions(parent, fs::Permissions::from_mode(RUNTIME_DIR_MODE))
            {
                return SocketPrep::Error(format!("chmod({}, 0700): {e}", parent.display()));
            }
        } else if let Err(e) = fs::create_dir_all(parent) {
            return SocketPrep::Error(format!("create_dir_all({}): {e}", parent.display()));
        }
    }

    if !path.exists() {
        return SocketPrep::Fresh;
    }

    match is_unix_socket(path) {
        Ok(false) => return SocketPrep::NotSocket,
        Err(e) => return SocketPrep::Error(format!("stat({}): {e}", path.display())),
        Ok(true) => {}
    }

    match UnixStream::connect(path) {
        Ok(_) => SocketPrep::InUse,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            match std::fs::remove_file(path) {
                Ok(()) => SocketPrep::StaleCleared,
                Err(e) => SocketPrep::Error(format!("unlink({}): {e}", path.display())),
            }
        }
        Err(e) => SocketPrep::Error(format!(
            "connect probe failed for {}: {e} ({:?})",
            path.display(),
            e.kind()
        )),
    }
}

fn is_unix_socket(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::FileTypeExt;
    let meta = std::fs::symlink_metadata(path)?;
    Ok(meta.file_type().is_socket())
}

/// Caller MUST have called `prepare_socket_path` first and not received
/// `InUse`/`Error`. Socket file chmod'd to 0600 — fs perms gate `connect()`
/// on Unix sockets, so this enforces owner-only access even if the parent
/// dir mode is lax.
pub fn bind_listener(path: &Path) -> std::io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_FILE_MODE)) {
        let _ = fs::remove_file(path);
        return Err(std::io::Error::other(format!(
            "chmod socket 0600 failed: {e}"
        )));
    }
    Ok(listener)
}

pub struct DaemonState {
    pub actions: Arc<ActionRegistry>,
    pub gui: Arc<GuiRegistry>,
    /// Forwarded to each registered GUI via `start_event_forwarder`.
    pub event_bus: EventBus,
    /// Discovered once at startup, sorted by `manifest.plugin.name`
    /// then `dir` as tiebreaker. Will back `plugin.<name>.<cmd>` and
    /// `_module.run` dispatch in the next commit; currently consumed
    /// by `plugin.list` only.
    pub plugins: Arc<Vec<LoadedPlugin>>,
    /// Path the daemon actually bound. Will be passed as
    /// `COPAD_SOCKET` to plugin shell children in the next commit;
    /// currently unused by dispatch.
    pub socket_path: PathBuf,
    /// Daemon-side dispatch authority signal. Set from
    /// `COPADD_HOST_TRIGGERS` at startup; advertised in
    /// `gui.register` ack + `daemon.info`. When `true`, the daemon's
    /// pump thread drives trigger dispatch; the GUI is expected
    /// (Stage C) to empty its local engine in response.
    pub host_triggers: bool,
}

impl DaemonState {
    pub fn new(
        actions: Arc<ActionRegistry>,
        gui: Arc<GuiRegistry>,
        event_bus: EventBus,
        plugins: Arc<Vec<LoadedPlugin>>,
        socket_path: PathBuf,
        host_triggers: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            actions,
            gui,
            event_bus,
            plugins,
            socket_path,
            host_triggers,
        })
    }

    /// Test-only constructor with empty plugins + placeholder socket path
    /// and a fresh `GuiRegistry`. Production callers should pass real
    /// values and share the registry with the trigger sink.
    #[cfg(test)]
    pub fn new_for_test(actions: Arc<ActionRegistry>, event_bus: EventBus) -> Arc<Self> {
        Self::new(
            actions,
            GuiRegistry::new(),
            event_bus,
            Arc::new(Vec::new()),
            PathBuf::from("/tmp/copadd-test-placeholder.sock"),
            false,
        )
    }
}

/// Returns on listener fatal error (caller runs `cleanup_socket`). We
/// don't swallow accept errors — `accept(2)` on a Unix socket fails for
/// fd exhaustion / bad listener fd, which we can't recover from.
pub fn run_accept_loop(listener: UnixListener, state: Arc<DaemonState>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let s = state.clone();
                thread::spawn(move || handle_connection(stream, s));
            }
            Err(e) => {
                log::error!(
                    "copadd accept error: {e}; shutting down accept loop so caller can run cleanup"
                );
                break;
            }
        }
    }
}

/// Extract the peer process's pid from a Unix socket via
/// `SO_PEERCRED`. Used by `events.publish` to stamp the event's
/// `source = "client.<pid>"` so trigger configs can correlate
/// publishes to their originating script. Linux-only; non-Linux
/// builds return `None` and the handler falls back to
/// `client.unknown`.
#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> Option<u32> {
    use std::mem::MaybeUninit;
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut cred: MaybeUninit<libc::ucred> = MaybeUninit::uninit();
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr() as *mut _,
            &mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(unsafe { cred.assume_init() }.pid as u32)
}

#[cfg(not(target_os = "linux"))]
fn peer_pid(_stream: &UnixStream) -> Option<u32> {
    // Other platforms use `LOCAL_PEERCRED` / `getpeereid` — out of
    // scope for v1; callers fall back to `client.unknown`.
    None
}

fn handle_connection(stream: UnixStream, state: Arc<DaemonState>) {
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("copadd try_clone failed: {e}");
            return;
        }
    };
    let peer = peer_pid(&stream);
    // Third fd handed to GuiClient so unregister (heartbeat or other)
    // can shutdown(Both) from outside this reader thread.
    let shutdown_stream = stream.try_clone().ok();

    // Bounded so the event forwarder can't accumulate unbounded memory
    // behind a wedged socket writer. Caller (heartbeat / invoke) treats
    // Full as disconnect; see `GuiClient::invoke`.
    let (writer_tx, writer_rx) = mpsc::sync_channel::<String>(512);
    thread::spawn(move || {
        let mut writer = write_stream;
        while let Ok(line) = writer_rx.recv() {
            if writeln!(writer, "{line}").is_err() {
                return;
            }
        }
    });

    let mut reader = BufReader::new(stream);
    let mut registered_client_id: Option<String> = None;
    let mut shutdown_stream = shutdown_stream;
    let mut frame_buf: Vec<u8> = Vec::with_capacity(8192);

    loop {
        let line = match read_line_capped(&mut reader, &mut frame_buf) {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                log::warn!("copadd rejecting oversized frame, closing: {e}");
                send_line(
                    &writer_tx,
                    &Response::error(String::new(), "frame_too_large", &e.to_string()),
                );
                // Connection-fatal: read_line_capped fail-fasts on cap
                // overflow without consuming the offending bytes, so
                // continuing would re-trigger on the same data. Drop
                // the connection instead.
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                log::warn!("copadd rejecting non-utf8 frame, closing: {e}");
                send_line(
                    &writer_tx,
                    &Response::error(String::new(), "invalid_utf8", &e.to_string()),
                );
                break;
            }
            Err(e) => {
                log::debug!("copadd connection read err: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        match parse_wire(&line) {
            WireMessage::Request(req) => {
                if req.method == "gui.register" {
                    if registered_client_id.is_some() {
                        send_line(
                            &writer_tx,
                            &Response::error(
                                req.id.clone(),
                                "already_registered",
                                "gui.register called twice on the same connection",
                            ),
                        );
                        continue;
                    }
                    let (resp, new_client_id) = handle_gui_register(
                        &req,
                        &state,
                        writer_tx.clone(),
                        shutdown_stream.take(),
                    );
                    send_line(&writer_tx, &resp);
                    if let Some(cid) = new_client_id {
                        // MUST come after the ack is queued — see
                        // `start_event_forwarder` doc.
                        state
                            .gui
                            .start_event_forwarder(&cid, state.event_bus.clone());
                        registered_client_id = Some(cid);
                    }
                    continue;
                }
                if req.method == "_bus.publish" {
                    // Wire-only bridge method — not in ActionRegistry so
                    // a generic socket client can't reach it via the
                    // registry path (and so the bus doesn't fan out a
                    // `_bus.publish.completed` event for every forwarded
                    // GUI event). Auth: registered-GUI convention under
                    // the 0600 socket trust model.
                    let resp = handle_bus_publish(&req, &state, registered_client_id.as_deref());
                    send_line(&writer_tx, &resp);
                    continue;
                }
                if req.method == "events.publish" {
                    // Public external surface — no registered-GUI gate.
                    // 0600 socket reachability IS the auth. `source` and
                    // `timestamp_ms` are daemon-controlled to prevent
                    // spoofing of action-registry completion events.
                    let resp = handle_events_publish(&req, &state, peer);
                    send_line(&writer_tx, &resp);
                    continue;
                }
                if req.method == "event.subscribe" {
                    // Registered GUIs already receive events via
                    // start_event_forwarder. Allowing a second pump
                    // here would duplicate every event.
                    if registered_client_id.is_some() {
                        send_line(
                            &writer_tx,
                            &Response::error(
                                req.id.clone(),
                                "invalid_request",
                                "registered GUI connections receive events automatically; use gui.subscribe / gui.unsubscribe to narrow",
                            ),
                        );
                        continue;
                    }
                    let patterns = match parse_subscribe_patterns(&req) {
                        Ok(p) => p,
                        Err(resp) => {
                            send_line(&writer_tx, &resp);
                            continue;
                        }
                    };
                    // Subscribe BEFORE the ack so an event published on
                    // another connection between the client receiving
                    // the ack and the daemon entering the recv loop is
                    // never dropped on the floor.
                    let rx = state.event_bus.subscribe_unbounded("*");
                    send_line(
                        &writer_tx,
                        &Response::success(
                            req.id.clone(),
                            serde_json::json!({ "status": "subscribed" }),
                        ),
                    );
                    run_event_subscribe(rx, &writer_tx, patterns);
                    return;
                }
                let resp = dispatch(&req, &state);
                send_line(&writer_tx, &resp);
            }
            WireMessage::Response(resp) => {
                // From a registered GUI replying to one of our Invokes.
                if let Some(ref cid) = registered_client_id
                    && let Some(client) = state.gui.get(cid)
                {
                    client.resolve(resp);
                }
            }
            WireMessage::Unknown(reason) => {
                send_line(
                    &writer_tx,
                    &Response::error(String::new(), "invalid_request", &reason),
                );
            }
        }
    }

    if let Some(cid) = registered_client_id {
        state.gui.unregister(&cid);
    }
}

enum WireMessage {
    Request(Request),
    Response(Response),
    Unknown(String),
}

fn parse_wire(line: &str) -> WireMessage {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return WireMessage::Unknown(format!("malformed JSON: {line:.200}"));
    };
    if value.get("ok").is_some() {
        match serde_json::from_value::<Response>(value) {
            Ok(r) => WireMessage::Response(r),
            Err(e) => WireMessage::Unknown(format!("malformed Response: {e}")),
        }
    } else if value.get("method").is_some() {
        match serde_json::from_value::<Request>(value) {
            Ok(r) => WireMessage::Request(r),
            Err(e) => WireMessage::Unknown(format!("malformed Request: {e}")),
        }
    } else {
        WireMessage::Unknown("missing discriminator (no `ok` or `method`)".into())
    }
}

fn send_line(tx: &mpsc::SyncSender<String>, response: &Response) {
    match serde_json::to_string(response) {
        Ok(s) => {
            let _ = tx.send(s);
        }
        Err(e) => log::warn!("copadd response serialize error: {e}"),
    }
}

/// `_bus.publish` ingest. Validates the GUI is registered on the
/// current connection (the only auth surface the 0600-socket trust
/// model provides), then publishes onto the daemon bus with a fresh
/// `bridge_id` so the symmetric daemon→GUI forwarder skips the echo.
///
/// Provenance rejections (`COMPLETION_EVENT_SOURCE`, reserved
/// `.completed`/`.failed` kinds) close the door on a malicious GUI
/// spoofing action-registry completions, which the trigger engine's
/// `try_promote_or_drop_preflight` gate trusts.
fn handle_bus_publish(
    req: &Request,
    state: &Arc<DaemonState>,
    registered_client_id: Option<&str>,
) -> Response {
    if registered_client_id.is_none() {
        return Response::error(
            req.id.clone(),
            "unauthorized",
            "_bus.publish requires gui.register on the same connection",
        );
    }
    let params = &req.params;
    let kind = match params.get("kind").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                "_bus.publish requires non-empty `kind` string",
            );
        }
    };
    if kind.ends_with(".completed") || kind.ends_with(".failed") {
        return Response::error(
            req.id.clone(),
            "invalid_params",
            "_bus.publish refuses reserved `.completed`/`.failed` kinds",
        );
    }
    let source = match params.get("source").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                "_bus.publish requires `source` string",
            );
        }
    };
    if source == COMPLETION_EVENT_SOURCE {
        return Response::error(
            req.id.clone(),
            "invalid_params",
            "_bus.publish refuses reserved `copad.action` source",
        );
    }
    // Required field. Defaulting silently to 0 would corrupt
    // `{event.timestamp_ms}` interpolation on the daemon side for any
    // trigger that reads it.
    let timestamp_ms = match params.get("timestamp_ms").and_then(|v| v.as_u64()) {
        Some(n) => n,
        None => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                "_bus.publish requires `timestamp_ms` u64",
            );
        }
    };
    let payload = params.get("payload").cloned().unwrap_or(Value::Null);
    let bridge_id = next_bridge_id();
    let mut event = BusEvent::new(kind, source, payload);
    event.timestamp_ms = timestamp_ms;
    state.event_bus.publish_bridged(event, bridge_id);
    Response::success(req.id.clone(), serde_json::json!({ "queued": true }))
}

/// `events.publish` public socket method. Anyone reaching the
/// 0600 socket can call it (same trust band as `coctl call`).
/// Daemon-controls `source` (peer-pid stamped) and `timestamp_ms`
/// (via `BusEvent::new`) so the caller cannot spoof an
/// action-registry completion event — the trigger engine's
/// preflight-promotion gate reads top-level `source` and trusts
/// only `copad.action`.
fn handle_events_publish(req: &Request, state: &Arc<DaemonState>, peer: Option<u32>) -> Response {
    let kind = match req.params.get("kind").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                "events.publish requires non-empty `kind` string",
            );
        }
    };
    if kind.ends_with(".completed") || kind.ends_with(".failed") {
        return Response::error(
            req.id.clone(),
            "invalid_params",
            "events.publish refuses reserved `.completed`/`.failed` kinds",
        );
    }
    let payload = req.params.get("payload").cloned().unwrap_or(Value::Null);
    let source = match peer {
        Some(pid) => format!("client.{pid}"),
        None => "client.unknown".into(),
    };
    // `events.publish` is the trust-boundary chokepoint for socket-driven
    // event injection — any same-UID process holding the daemon socket
    // (including SSH-forwarded harness hooks) can reach here. Tag the
    // event `External` so `TriggerEngine` fan-out skips it for any
    // trigger that didn't opt in via `[security] accept_external = true`.
    state.event_bus.publish(
        copad_core::event_bus::Event::new(kind, source, payload)
            .with_origin(copad_core::event_bus::Origin::External),
    );
    Response::success(req.id.clone(), serde_json::json!({ "queued": true }))
}

fn parse_subscribe_patterns(req: &Request) -> Result<Vec<String>, Response> {
    let Some(value) = req.params.get("patterns") else {
        return Ok(vec!["*".to_string()]);
    };
    let arr = match value.as_array() {
        Some(a) => a,
        None => {
            return Err(Response::error(
                req.id.clone(),
                "invalid_params",
                "event.subscribe `patterns` must be an array of strings",
            ));
        }
    };
    if arr.is_empty() {
        return Ok(vec!["*".to_string()]);
    }
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        match item.as_str() {
            Some(s) if !s.is_empty() => out.push(s.to_string()),
            _ => {
                return Err(Response::error(
                    req.id.clone(),
                    "invalid_params",
                    "event.subscribe `patterns` entries must be non-empty strings",
                ));
            }
        }
    }
    Ok(out)
}

const SUBSCRIBE_KEEPALIVE: Duration = Duration::from_secs(15);

/// Maximum bytes accepted for a single newline-delimited frame on the
/// daemon socket. `BufRead::lines` / `read_until` would otherwise buffer
/// without bound — a malicious or stuck client that streams bytes
/// without `\n` could exhaust daemon memory. 1 MiB is well above any
/// legitimate JSON frame (the largest action payloads we ship —
/// webview content reads, screenshot base64 — stay below ~256 KiB);
/// callers should chunk anything bigger over multiple actions.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Reads one newline-delimited frame, refusing to grow the buffer past
/// [`MAX_FRAME_BYTES`]. Returns `Ok(None)` on clean EOF, `Ok(Some(s))`
/// on a full line, `Err(InvalidData)` AS SOON AS the running total
/// would exceed the cap — no resync attempt, because a peer that never
/// sends `\n` would otherwise pin the helper indefinitely. Callers
/// must treat `InvalidData` as a connection-fatal signal (send a
/// `frame_too_large` reply, then drop the connection). Reused by
/// copad-linux's per-connection socket reader.
pub fn read_line_capped(
    reader: &mut std::io::BufReader<UnixStream>,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<String>> {
    use std::io::BufRead;
    buf.clear();
    let mut total = 0usize;
    loop {
        let avail = match reader.fill_buf() {
            Ok([]) => {
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let (chunk, found_newline) = match avail.iter().position(|b| *b == b'\n') {
            Some(idx) => (&avail[..=idx], true),
            None => (avail, false),
        };
        if total + chunk.len() > MAX_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("frame exceeds {MAX_FRAME_BYTES} bytes"),
            ));
        }
        let take = chunk.len();
        buf.extend_from_slice(chunk);
        total += take;
        reader.consume(take);
        if found_newline {
            break;
        }
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    match String::from_utf8(std::mem::take(buf)) {
        Ok(s) => Ok(Some(s)),
        // `InvalidInput` distinguishes "decode failed" from `InvalidData`'s
        // "frame too large" so callers can pick precise wire error codes.
        Err(e) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid utf-8: {e}"),
        )),
    }
}

/// Runs until the socket dies or the bus is dropped. `bus.subscribe_unbounded("*")`
/// honors the lossless contract for external `event.subscribe`; pattern filtering
/// happens here via [`pattern_matches`] OR'd across `patterns`. The 15s
/// keep-alive probe is the disconnect detector during quiet bus periods:
/// `writer_tx.send(String::new())` makes the writer thread write a `\n`, which
/// fails with EPIPE once the client has closed the socket and unwinds this
/// loop. coctl's subscribe reader already skips empty lines.
fn run_event_subscribe(
    rx: copad_core::event_bus::EventReceiver,
    writer_tx: &mpsc::SyncSender<String>,
    patterns: Vec<String>,
) {
    loop {
        match rx.recv_timeout(SUBSCRIBE_KEEPALIVE) {
            RecvOutcome::Event(ev) => {
                if !patterns.iter().any(|p| pattern_matches(p, &ev.kind)) {
                    continue;
                }
                // `origin` round-trips alongside `source` so subscribers
                // (e.g. coctl wrappers, future GUI bridges) can see the
                // trust-boundary tag set at `events.publish` ingest.
                let wire = WireEvent::new(ev.kind, ev.payload)
                    .with_source(ev.source)
                    .with_origin(ev.origin);
                let line = match serde_json::to_string(&wire) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("event.subscribe serialize error: {e}");
                        continue;
                    }
                };
                if writer_tx.send(line).is_err() {
                    return;
                }
            }
            RecvOutcome::Timeout => {
                if writer_tx.send(String::new()).is_err() {
                    return;
                }
            }
            RecvOutcome::Disconnected => return,
        }
    }
}

fn handle_gui_register(
    req: &Request,
    state: &Arc<DaemonState>,
    writer_tx: mpsc::SyncSender<String>,
    shutdown_handle: Option<UnixStream>,
) -> (Response, Option<String>) {
    let caps = req
        .params
        .get("capabilities")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();
    let want_primary = req
        .params
        .get("want_primary")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let client_version = req
        .params
        .get("protocol_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(PROTOCOL_VERSION as u64);
    if client_version != PROTOCOL_VERSION as u64 {
        return (
            Response::error(
                req.id.clone(),
                "incompatible",
                &format!(
                    "daemon protocol_version={PROTOCOL_VERSION}, gui sent protocol_version={client_version}"
                ),
            ),
            None,
        );
    }

    // Daemon-side env filter at the trust boundary: any registered
    // client could be a mock, so the GUI-side curation list is not
    // load-bearing. Only keys in `GUI_ENV_ALLOWED_KEYS` survive.
    let gui_env = req
        .params
        .get("gui_env")
        .map(crate::gui_registry::filter_gui_env)
        .unwrap_or_default();
    let (client_id, is_primary) =
        state
            .gui
            .register(caps, want_primary, gui_env, writer_tx, shutdown_handle);
    let resp = Response::success(
        req.id.clone(),
        serde_json::json!({
            "client_id": client_id,
            "primary": is_primary,
            "daemon_version": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
            "host_triggers": state.host_triggers,
        }),
    );
    (resp, Some(client_id))
}

/// `state.actions` first, then GUI-owned proxy via `gui_registry`, else
/// `unknown_method`.
pub fn dispatch(req: &Request, state: &Arc<DaemonState>) -> Response {
    if state.actions.has(&req.method) {
        return dispatch_via_registry(req, &state.actions);
    }

    if method_capability(&req.method).is_some() {
        let resp = dispatch_via_gui(req, &state.gui);
        // Daemon vouches that the GUI-owned action returned this
        // response; publish `<method>.completed`/`.failed` on the
        // daemon bus so daemon-side chained triggers fire. The
        // GUI suppresses local publish for daemon-Invoke origins
        // (`silent_completion=true` set by `handle_invoke`), and the
        // daemon→GUI forwarder bridges this event back so GUI-side
        // triggers also see it exactly once. Failure paths (no_gui,
        // unknown_client, invoke timeout) emit `<method>.failed`
        // identically.
        publish_legacy_completion(&state.event_bus, &req.method, false, &resp);
        return resp;
    }

    Response::error(
        req.id.clone(),
        "unknown_method",
        &format!("copadd has no action named {}", req.method),
    )
}

fn dispatch_via_gui(req: &Request, gui: &Arc<GuiRegistry>) -> Response {
    match gui.route(&req.method, req.target_client_id.as_deref()) {
        Ok(client) => {
            let timeout = method_invoke_timeout(&req.method);
            let resp = client.invoke(&req.method, req.params.clone(), timeout);
            // The GUI Response carries the invoke_id; rewrite to the
            // original caller's request id.
            Response {
                id: req.id.clone(),
                ok: resp.ok,
                result: resp.result,
                error: resp.error,
            }
        }
        Err("no_gui") => Response::error(
            req.id.clone(),
            "no_gui",
            &format!(
                "no GUI registered with capability for `{}`; start copad or pass --target_client_id",
                req.method
            ),
        ),
        Err("unknown_client") => Response::error(
            req.id.clone(),
            "unknown_client",
            "target_client_id does not match any registered GUI",
        ),
        Err(other) => Response::error(req.id.clone(), other, "GUI routing failed"),
    }
}

/// Bridges callback-based `try_dispatch` to a sync return so the
/// per-connection thread can block on plugin replies.
fn dispatch_via_registry(req: &Request, actions: &Arc<ActionRegistry>) -> Response {
    let (tx, rx) = std::sync::mpsc::channel();
    let req_id = req.id.clone();
    actions.try_dispatch(
        &req.method,
        req.params.clone(),
        Box::new(move |result| {
            let resp = match result {
                Ok(v) => Response::success(req_id, v),
                Err(err) => Response {
                    id: req_id,
                    ok: false,
                    result: None,
                    error: Some(err),
                },
            };
            let _ = tx.send(resp);
        }),
    );
    match rx.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(resp) => resp,
        Err(_) => Response::error(
            req.id.clone(),
            "action_timeout",
            "copadd action did not complete within 120s",
        ),
    }
}

pub fn cleanup_socket(path: &PathBuf) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("copadd socket cleanup failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufRead;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;

    fn tmp_socket() -> PathBuf {
        let dir = tempfile_dir();
        dir.join("test-sock")
    }

    fn tempfile_dir() -> PathBuf {
        // Avoid pulling tempfile crate as a dep for one test helper.
        let pid = std::process::id();
        let nano = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("copadd-test-{pid}-{nano}"));
        std::fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    fn mk_state_with_ping() -> Arc<DaemonState> {
        let actions = Arc::new(ActionRegistry::new());
        actions.register_silent("system.ping", |_| Ok(json!({"status": "ok"})));
        DaemonState::new_for_test(actions, new_event_bus())
    }

    #[test]
    fn dispatch_system_ping_returns_ok() {
        let state = mk_state_with_ping();
        let req = Request::new("abc", "system.ping", json!({}));
        let resp = dispatch(&req, &state);
        assert!(resp.ok);
        assert_eq!(resp.id, "abc");
        let body = resp.result.expect("result");
        assert_eq!(body["status"], json!("ok"));
    }

    #[test]
    fn dispatch_unknown_method_returns_error() {
        let state = mk_state_with_ping();
        let req = Request::new("xyz", "nothing.here", json!({}));
        let resp = dispatch(&req, &state);
        assert!(!resp.ok);
        let err = resp.error.expect("error");
        assert_eq!(err.code, "unknown_method");
    }

    #[test]
    fn dispatch_gui_owned_method_without_gui_returns_no_gui() {
        let state = mk_state_with_ping();
        let req = Request::new("xyz", "tab.new", json!({}));
        let resp = dispatch(&req, &state);
        assert!(!resp.ok);
        let err = resp.error.expect("error");
        assert_eq!(err.code, "no_gui");
    }

    #[test]
    fn dispatch_routes_to_registered_action() {
        let actions = Arc::new(ActionRegistry::new());
        actions.register("greet", |_| Ok(json!({"hi": true})));
        let state = DaemonState::new_for_test(actions, new_event_bus());
        let req = Request::new("g-1", "greet", json!({}));
        let resp = dispatch(&req, &state);
        assert!(resp.ok);
        assert_eq!(resp.result.unwrap()["hi"], json!(true));
    }

    #[test]
    fn prepare_socket_path_fresh() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let res = prepare_socket_path(&path);
        assert_eq!(res, SocketPrep::Fresh);
    }

    #[test]
    fn prepare_socket_path_clears_stale_socket_inode() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        {
            let _listener = UnixListener::bind(&path).expect("bind");
        } // dropped → listener closed, but socket inode persists
        assert!(path.exists());
        let res = prepare_socket_path(&path);
        assert_eq!(res, SocketPrep::StaleCleared);
        assert!(!path.exists(), "stale socket inode should be unlinked");
    }

    #[test]
    fn bind_listener_sets_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let _listener = bind_listener(&path).expect("bind");
        let meta = std::fs::metadata(&path).expect("metadata");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket file must be owner-only");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prepare_socket_path_leaves_foreign_parent_perms_alone() {
        // tempfile_dir() is not runtime_dir(); chmod must NOT fire.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile_dir();
        let path = dir.join("test-foreign-parent-sock");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("loosen dir");
        let _ = std::fs::remove_file(&path);
        let res = prepare_socket_path(&path);
        assert_eq!(res, SocketPrep::Fresh);
        let mode = std::fs::metadata(&dir)
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "foreign parent dir perms must NOT be modified");
    }

    #[test]
    fn prepare_socket_path_refuses_regular_file() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "very important user data").expect("write file");
        let res = prepare_socket_path(&path);
        assert_eq!(res, SocketPrep::NotSocket);
        assert!(path.exists(), "regular file must NOT be unlinked");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "very important user data"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prepare_socket_path_detects_live_listener() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        let res = prepare_socket_path(&path);
        assert_eq!(res, SocketPrep::InUse);
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn end_to_end_ping_roundtrip() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();

        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));

        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut stream = UnixStream::connect(&path_clone).expect("connect");
        let req = Request::new("rt-1", "system.ping", json!({}));
        let line = serde_json::to_string(&req).unwrap() + "\n";
        stream.write_all(line.as_bytes()).expect("write");

        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        assert!(resp.ok);
        assert_eq!(resp.id, "rt-1");

        drop(stream);
        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn bus_publish_without_register_returns_unauthorized() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let stream = UnixStream::connect(&path_clone).expect("connect");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let req = Request::new(
            "p-1",
            "_bus.publish",
            json!({"kind": "panel.focused", "source": "mock", "timestamp_ms": 0, "payload": {}}),
        );
        writer
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .expect("write");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        assert!(!resp.ok);
        assert_eq!(resp.error.expect("err").code, "unauthorized");
        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn bus_publish_rejects_reserved_source() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let stream = UnixStream::connect(&path_clone).expect("connect");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        // Register first so we pass the auth gate; the reserved-source
        // rejection should still bite.
        let reg = Request::new(
            "reg",
            "gui.register",
            json!({
                "window_id": "test",
                "capabilities": ["tab"],
                "want_primary": true,
                "protocol_version": 1
            }),
        );
        writer
            .write_all((serde_json::to_string(&reg).unwrap() + "\n").as_bytes())
            .expect("write reg");
        let mut ack = String::new();
        reader.read_line(&mut ack).expect("read ack");
        // Now attempt the spoof.
        let req = Request::new(
            "p-2",
            "_bus.publish",
            json!({
                "kind": "panel.focused",
                "source": COMPLETION_EVENT_SOURCE,
                "timestamp_ms": 0,
                "payload": {}
            }),
        );
        writer
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .expect("write");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        assert!(!resp.ok);
        let err = resp.error.expect("err");
        assert_eq!(err.code, "invalid_params");
        assert!(
            err.message.contains("copad.action"),
            "rejection should name the source: {}",
            err.message
        );
        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn bus_publish_rejects_reserved_kind_suffix() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let stream = UnixStream::connect(&path_clone).expect("connect");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let reg = Request::new(
            "reg",
            "gui.register",
            json!({
                "window_id": "test",
                "capabilities": ["tab"],
                "want_primary": true,
                "protocol_version": 1
            }),
        );
        writer
            .write_all((serde_json::to_string(&reg).unwrap() + "\n").as_bytes())
            .expect("write reg");
        let mut ack = String::new();
        reader.read_line(&mut ack).expect("read ack");
        let req = Request::new(
            "p-3",
            "_bus.publish",
            json!({
                "kind": "foo.completed",
                "source": "mock",
                "timestamp_ms": 0,
                "payload": {}
            }),
        );
        writer
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .expect("write");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        assert!(!resp.ok);
        assert_eq!(resp.error.expect("err").code, "invalid_params");
        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn bus_publish_success_lands_on_bus_with_bridge_id() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        // Subscribe BEFORE running the accept loop so the event handler
        // sees us as soon as it publishes.
        let rx = state.event_bus.subscribe_unbounded("panel.focused");
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let stream = UnixStream::connect(&path_clone).expect("connect");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let reg = Request::new(
            "reg",
            "gui.register",
            json!({
                "window_id": "test",
                "capabilities": ["tab"],
                "want_primary": true,
                "protocol_version": 1
            }),
        );
        writer
            .write_all((serde_json::to_string(&reg).unwrap() + "\n").as_bytes())
            .expect("write reg");
        let mut ack = String::new();
        reader.read_line(&mut ack).expect("read ack");
        let req = Request::new(
            "p-ok",
            "_bus.publish",
            json!({
                "kind": "panel.focused",
                "source": "mock",
                "timestamp_ms": 42,
                "payload": {"panel_id": "p1"}
            }),
        );
        writer
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .expect("write");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        assert!(resp.ok, "expected success: {resp:?}");
        // The published event landed on the bus with bridge_id set.
        let ev = match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            copad_core::event_bus::RecvOutcome::Event(e) => e,
            other => panic!("expected event, got {other:?}"),
        };
        assert_eq!(ev.kind, "panel.focused");
        assert_eq!(ev.source, "mock");
        assert_eq!(ev.timestamp_ms, 42);
        assert!(ev.bridge_id.is_some());
        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn events_publish_works_without_register_and_assigns_source() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        let rx = state.event_bus.subscribe_unbounded("e2e.public");
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let stream = UnixStream::connect(&path_clone).expect("connect");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let req = Request::new(
            "ep-1",
            "events.publish",
            json!({"kind": "e2e.public", "payload": {"hi": true}}),
        );
        writer
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .expect("write");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        assert!(
            resp.ok,
            "events.publish should succeed without register: {resp:?}"
        );
        // Event landed on the bus with daemon-assigned source.
        let ev = match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            copad_core::event_bus::RecvOutcome::Event(e) => e,
            other => panic!("expected event, got {other:?}"),
        };
        assert_eq!(ev.kind, "e2e.public");
        assert!(
            ev.source.starts_with("client."),
            "source should be daemon-assigned client.<pid>, got {}",
            ev.source
        );
        assert_eq!(ev.payload, json!({"hi": true}));
        assert!(ev.timestamp_ms > 0, "daemon-assigned timestamp present");
        assert_eq!(
            ev.origin,
            copad_core::event_bus::Origin::External,
            "events.publish is the trust-boundary chokepoint; events injected here MUST be tagged External"
        );
        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn events_publish_rejects_empty_kind() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let stream = UnixStream::connect(&path_clone).expect("connect");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let req = Request::new(
            "ep-empty",
            "events.publish",
            json!({"kind": "", "payload": {}}),
        );
        writer
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .expect("write");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        assert!(!resp.ok);
        assert_eq!(resp.error.expect("err").code, "invalid_params");
        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn events_publish_rejects_reserved_kind_suffix() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let stream = UnixStream::connect(&path_clone).expect("connect");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let req = Request::new(
            "ep-suffix",
            "events.publish",
            json!({"kind": "foo.completed", "payload": {}}),
        );
        writer
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .expect("write");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        assert!(!resp.ok);
        assert_eq!(resp.error.expect("err").code, "invalid_params");
        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn events_publish_omits_payload_defaults_to_null() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        let rx = state.event_bus.subscribe_unbounded("e2e.npayload");
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let stream = UnixStream::connect(&path_clone).expect("connect");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let req = Request::new("ep-np", "events.publish", json!({"kind": "e2e.npayload"}));
        writer
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .expect("write");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        assert!(resp.ok);
        let ev = match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            copad_core::event_bus::RecvOutcome::Event(e) => e,
            other => panic!("expected event, got {other:?}"),
        };
        assert_eq!(ev.payload, Value::Null);
        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn gui_register_ack_advertises_host_triggers_true_when_enabled() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let actions = Arc::new(ActionRegistry::new());
        let state = DaemonState::new(
            actions,
            GuiRegistry::new(),
            new_event_bus(),
            Arc::new(Vec::new()),
            path.clone(),
            true,
        );
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let gui_stream = UnixStream::connect(&path_clone).expect("connect");
        let mut gui_write = gui_stream.try_clone().expect("clone");
        let mut gui_read = BufReader::new(gui_stream);
        let reg_req = Request::new(
            "reg-ht",
            "gui.register",
            json!({
                "window_id": "test-ht",
                "capabilities": ["tab"],
                "want_primary": true,
                "protocol_version": 1
            }),
        );
        gui_write
            .write_all((serde_json::to_string(&reg_req).unwrap() + "\n").as_bytes())
            .expect("write");
        let mut line = String::new();
        gui_read.read_line(&mut line).expect("read");
        let resp: Response = serde_json::from_str(line.trim()).expect("parse");
        let result = resp.result.expect("result");
        assert_eq!(result["host_triggers"], json!(true));
        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn end_to_end_gui_register_and_invoke_roundtrip() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        // 1) GUI connects and registers.
        let gui_stream = UnixStream::connect(&path_clone).expect("connect gui");
        let mut gui_write = gui_stream.try_clone().expect("clone gui");
        let mut gui_read = BufReader::new(gui_stream);
        let reg_req = Request::new(
            "reg-1",
            "gui.register",
            json!({
                "window_id": "test-win",
                "capabilities": ["tab"],
                "want_primary": true,
                "protocol_version": 1
            }),
        );
        gui_write
            .write_all((serde_json::to_string(&reg_req).unwrap() + "\n").as_bytes())
            .expect("write register");
        let mut line = String::new();
        gui_read.read_line(&mut line).expect("read register reply");
        let reg_resp: Response = serde_json::from_str(line.trim()).expect("parse register reply");
        assert!(reg_resp.ok, "register failed: {reg_resp:?}");
        let result = reg_resp.result.expect("register has result");
        assert_eq!(result["primary"], json!(true));
        assert!(result["client_id"].is_string());
        // host_triggers from the ack must reflect daemon state (false here
        // — `new_for_test` defaults to false).
        assert_eq!(result["host_triggers"], json!(false));

        // 2) Separate client invokes tab.list. Daemon must route to our GUI.
        let client_stream = UnixStream::connect(&path_clone).expect("connect client");
        let mut client_write = client_stream.try_clone().expect("clone client");
        let mut client_read = BufReader::new(client_stream);
        let tab_req = Request::new("tab-1", "tab.list", json!({}));
        client_write
            .write_all((serde_json::to_string(&tab_req).unwrap() + "\n").as_bytes())
            .expect("write tab.list");

        // 3) GUI side reads the Invoke and replies.
        let mut invoke_line = String::new();
        gui_read.read_line(&mut invoke_line).expect("read invoke");
        let invoke: copad_core::protocol::Invoke =
            serde_json::from_str(invoke_line.trim()).expect("parse invoke");
        assert_eq!(invoke.invoke, "tab.list");
        let gui_resp = Response::success(invoke.id.clone(), json!({"count": 2, "current": 0}));
        gui_write
            .write_all((serde_json::to_string(&gui_resp).unwrap() + "\n").as_bytes())
            .expect("write gui response");

        // 4) Client receives the forwarded response with its own request id.
        let mut client_line = String::new();
        client_read
            .read_line(&mut client_line)
            .expect("read client reply");
        let client_resp: Response =
            serde_json::from_str(client_line.trim()).expect("parse client reply");
        assert!(client_resp.ok);
        assert_eq!(client_resp.id, "tab-1");
        let r = client_resp.result.expect("result");
        assert_eq!(r["count"], json!(2));
        assert_eq!(r["current"], json!(0));

        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn parse_subscribe_patterns_missing_defaults_to_star() {
        let req = Request::new("a", "event.subscribe", json!({}));
        assert_eq!(parse_subscribe_patterns(&req).unwrap(), vec!["*"]);
    }

    #[test]
    fn parse_subscribe_patterns_empty_array_defaults_to_star() {
        let req = Request::new("a", "event.subscribe", json!({ "patterns": [] }));
        assert_eq!(parse_subscribe_patterns(&req).unwrap(), vec!["*"]);
    }

    #[test]
    fn parse_subscribe_patterns_accepts_string_list() {
        let req = Request::new(
            "a",
            "event.subscribe",
            json!({ "patterns": ["claude.*", "system.heartbeat"] }),
        );
        let got = parse_subscribe_patterns(&req).unwrap();
        assert_eq!(got, vec!["claude.*", "system.heartbeat"]);
    }

    #[test]
    fn parse_subscribe_patterns_rejects_non_array() {
        let req = Request::new("a", "event.subscribe", json!({ "patterns": "claude.*" }));
        let err = parse_subscribe_patterns(&req).unwrap_err();
        assert_eq!(err.error.unwrap().code, "invalid_params");
    }

    #[test]
    fn parse_subscribe_patterns_rejects_empty_string_entry() {
        let req = Request::new(
            "a",
            "event.subscribe",
            json!({ "patterns": ["claude.*", ""] }),
        );
        let err = parse_subscribe_patterns(&req).unwrap_err();
        assert_eq!(err.error.unwrap().code, "invalid_params");
    }

    #[test]
    fn parse_subscribe_patterns_rejects_non_string_entry() {
        let req = Request::new(
            "a",
            "event.subscribe",
            json!({ "patterns": ["claude.*", 42] }),
        );
        let err = parse_subscribe_patterns(&req).unwrap_err();
        assert_eq!(err.error.unwrap().code, "invalid_params");
    }

    #[test]
    fn event_subscribe_streams_filtered_events_over_socket() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        let bus_handle = state.event_bus.clone();
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let stream = UnixStream::connect(&path_clone).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok();
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);

        let req = Request::new(
            "sub-1",
            "event.subscribe",
            json!({ "patterns": ["unit.test.*"] }),
        );
        writer
            .write_all((serde_json::to_string(&req).unwrap() + "\n").as_bytes())
            .expect("write");

        let mut ack = String::new();
        reader.read_line(&mut ack).expect("ack");
        let ack_resp: Response = serde_json::from_str(ack.trim()).expect("parse ack");
        assert!(ack_resp.ok);
        assert_eq!(ack_resp.id, "sub-1");

        // Publish a non-matching event followed by a matching one. The
        // first must not show up; the second must.
        bus_handle.publish(BusEvent::new("other.kind", "test", json!({ "skip": true })));
        bus_handle.publish(BusEvent::new(
            "unit.test.fire",
            "test",
            json!({ "hello": "world" }),
        ));

        let mut delivered: Option<WireEvent> = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let ev: WireEvent = serde_json::from_str(trimmed).expect("parse event");
            delivered = Some(ev);
            break;
        }
        let ev = delivered.expect("matching event delivered");
        assert_eq!(ev.event_type, "unit.test.fire");
        assert_eq!(ev.data["hello"], json!("world"));
        assert_eq!(ev.source.as_deref(), Some("test"));

        let _ = std::fs::remove_file(&path_clone);
    }

    #[test]
    fn event_subscribe_rejected_on_registered_connection() {
        let path = tmp_socket();
        let _ = std::fs::remove_file(&path);
        let listener = bind_listener(&path).expect("bind");
        let state = mk_state_with_ping();
        let path_clone = path.clone();
        let _server = thread::spawn(move || run_accept_loop(listener, state));
        std::thread::sleep(std::time::Duration::from_millis(50));

        let stream = UnixStream::connect(&path_clone).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok();
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);

        let reg = Request::new(
            "reg-1",
            "gui.register",
            json!({
                "capabilities": ["test.*"],
                "want_primary": true,
                "protocol_version": PROTOCOL_VERSION,
            }),
        );
        writer
            .write_all((serde_json::to_string(&reg).unwrap() + "\n").as_bytes())
            .expect("write reg");
        let mut ack = String::new();
        reader.read_line(&mut ack).expect("reg ack");

        let sub = Request::new("sub-1", "event.subscribe", json!({}));
        writer
            .write_all((serde_json::to_string(&sub).unwrap() + "\n").as_bytes())
            .expect("write sub");
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).expect("read response");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let resp: Response = serde_json::from_str(trimmed).expect("parse response");
            assert!(!resp.ok);
            assert_eq!(resp.id, "sub-1");
            assert_eq!(resp.error.unwrap().code, "invalid_request");
            break;
        }

        let _ = std::fs::remove_file(&path_clone);
    }

    fn capped_reader_for(input: &[u8]) -> BufReader<UnixStream> {
        let (mut a, b) = UnixStream::pair().expect("pair");
        // Linux unix-socket buffers default to ~208 KiB; oversized-frame
        // test inputs run past 1 MiB, so writing must happen on a thread
        // alongside the reader to avoid kernel-buffer deadlock.
        let payload = input.to_vec();
        std::thread::spawn(move || {
            let _ = a.write_all(&payload);
            // a dropped on thread exit → reader sees EOF after data
        });
        BufReader::new(b)
    }

    #[test]
    fn read_line_capped_returns_full_line() {
        let mut r = capped_reader_for(b"hello world\nnext\n");
        let mut buf = Vec::new();
        assert_eq!(
            read_line_capped(&mut r, &mut buf).unwrap(),
            Some("hello world".to_string())
        );
        assert_eq!(
            read_line_capped(&mut r, &mut buf).unwrap(),
            Some("next".to_string())
        );
        assert_eq!(read_line_capped(&mut r, &mut buf).unwrap(), None);
    }

    #[test]
    fn read_line_capped_rejects_oversized_frame_fail_fast() {
        // Stream past MAX_FRAME_BYTES with no newline. Helper must
        // fail-fast (NOT block draining toward a newline that never
        // arrives). Caller is responsible for closing the connection.
        let oversized: Vec<u8> = std::iter::repeat_n(b'x', MAX_FRAME_BYTES + 32).collect();
        let mut r = capped_reader_for(&oversized);
        let mut buf = Vec::new();
        let err = read_line_capped(&mut r, &mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_line_capped_rejects_overflow_even_when_newline_eventually_arrives() {
        // Even if the peer DOES send a newline past the cap, fail-fast —
        // we don't read past the cap to confirm.
        let mut input: Vec<u8> = std::iter::repeat_n(b'y', MAX_FRAME_BYTES + 1).collect();
        input.push(b'\n');
        let mut r = capped_reader_for(&input);
        let mut buf = Vec::new();
        let err = read_line_capped(&mut r, &mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_line_capped_handles_no_trailing_newline_at_eof() {
        let mut r = capped_reader_for(b"final line no newline");
        let mut buf = Vec::new();
        assert_eq!(
            read_line_capped(&mut r, &mut buf).unwrap(),
            Some("final line no newline".to_string())
        );
        assert_eq!(read_line_capped(&mut r, &mut buf).unwrap(), None);
    }
}
