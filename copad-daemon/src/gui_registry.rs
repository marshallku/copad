//! Registered GUI clients and the GUI-owned method routing table.
//!
//! See `docs/gui-daemon-protocol.md` § `gui.register` schema + Routing rules.

use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::mpsc::sync_channel;
use std::sync::mpsc::{Sender, SyncSender, channel};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use copad_core::event_bus::{EventBus, RecvOutcome};
use copad_core::protocol::{Event as WireEvent, Invoke, Response, ResponseError};
use serde_json::{Value, json};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_MAX_MISSES: u32 = 2;

/// Allowed keys for the per-GUI env a client publishes via `gui.register`'s `gui_env`,
/// forwarded into `system.spawn` children so triggers reach the compositor / Wayland session.
/// Kept small: unfiltered, a malicious GUI could leak `LD_PRELOAD`, `PATH` or secrets into
/// children.
pub const GUI_ENV_ALLOWED_KEYS: &[&str] = &[
    "HYPRLAND_INSTANCE_SIGNATURE",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Applied at the daemon trust boundary on every `gui.register`: GUI-side filtering alone is
/// not enough (the client could be a mock).
pub fn filter_gui_env(raw: &Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(obj) = raw.as_object() else {
        return out;
    };
    for k in GUI_ENV_ALLOWED_KEYS {
        if let Some(v) = obj.get(*k).and_then(|v| v.as_str()) {
            out.insert((*k).to_string(), v.to_string());
        }
    }
    out
}

#[derive(Clone, Copy)]
struct HeartbeatConfig {
    interval: Duration,
    timeout: Duration,
    max_misses: u32,
}

impl HeartbeatConfig {
    const PROD: Self = Self {
        interval: HEARTBEAT_INTERVAL,
        timeout: HEARTBEAT_TIMEOUT,
        max_misses: HEARTBEAT_MAX_MISSES,
    };
}

/// Maps a GUI-owned method to its capability. `None` = daemon-owned.
pub fn method_capability(method: &str) -> Option<&'static str> {
    match method {
        "tab.new" | "tab.close" | "tab.list" | "tab.info" | "tab.rename" | "tab.switch"
        | "tabs.toggle_bar" | "claude.start" => Some("tab"),
        // Focus movement rides the pane-geometry capability: `split` is already in the GUI's
        // advertised CAPABILITIES, so no handshake change. `panel.focus` is the same act by id.
        "split.horizontal" | "split.vertical" | "pane.focus_next" | "pane.focus_prev"
        | "panel.focus" => Some("split"),
        "terminal.read" | "terminal.state" | "terminal.exec" | "terminal.feed"
        | "terminal.history" | "terminal.context" => Some("terminal"),
        m if m.starts_with("webview.") => Some("webview"),
        // `browser.*` rides `webview`: the namespaces split by naming, not boundary (that is
        // `TabMode`, checked in `copad_core::browser::authorize`). A separate capability would
        // need both GUIs to advertise it, and an un-updated GUI would answer `unknown_method`
        // for a method it implements.
        m if m.starts_with("browser.") => Some("webview"),
        m if m.starts_with("background.") => Some("background"),
        "statusbar.show" | "statusbar.hide" | "statusbar.toggle" => Some("statusbar"),
        // Both open a GUI-owned panel; the cockpit belongs to `agent.ui`.
        "agent.approve" | "cockpit.open" => Some("agent.ui"),
        "plugin.open" => Some("plugin.open"),
        "session.list" | "session.info" => Some("session"),
        // workflow.run dispatches `claude.start` in-process (needs TabManager and the GTK
        // window), so project.* and workflow.list/get are GUI-routed too.
        m if m.starts_with("project.") => Some("project"),
        m if m.starts_with("workflow.") => Some("workflow"),
        _ => None,
    }
}

/// Slow methods (WebView ops especially) need more than the default 5s. Matches the supervisor's
/// 120s action_timeout for anything that can trigger a plugin RPC or heavy WebView call.
pub fn method_invoke_timeout(method: &str) -> Duration {
    if method.starts_with("webview.") || method.starts_with("browser.") || method == "claude.start"
    {
        Duration::from_secs(120)
    } else {
        Duration::from_secs(5)
    }
}

pub struct GuiClient {
    pub client_id: String,
    pub capabilities: HashSet<String>,
    pub want_primary: bool,
    /// Filtered via [`filter_gui_env`] at registration; the daemon never trusts raw GUI input.
    pub gui_env: HashMap<String, String>,
    writer_tx: SyncSender<String>,
    /// `None` after unregister, so a stale `Arc<GuiClient>` cannot insert a pending entry nobody
    /// will resolve.
    pending: Mutex<Option<HashMap<String, Sender<Response>>>>,
    /// Lets `fail_all_pending` shutdown(Both) the daemon side, so both
    /// readers EOF and the GUI's reconnect_loop fires.
    shutdown_handle: Mutex<Option<UnixStream>>,
    forwarder_stop: Arc<AtomicBool>,
}

impl GuiClient {
    pub fn invoke(&self, method: &str, params: Value, timeout: Duration) -> Response {
        let invoke_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = channel::<Response>();
        {
            let mut guard = self.pending.lock().unwrap();
            match guard.as_mut() {
                Some(map) => {
                    map.insert(invoke_id.clone(), tx);
                }
                None => {
                    return Response::error(
                        String::new(),
                        "gui_disconnected",
                        "GUI disconnected before invoke",
                    );
                }
            }
        }
        let line = match serde_json::to_string(&Invoke::new(&invoke_id, method, params)) {
            Ok(s) => s,
            Err(e) => {
                self.remove_pending(&invoke_id);
                return Response::error(
                    String::new(),
                    "internal_error",
                    &format!("serialize invoke: {e}"),
                );
            }
        };
        // Heartbeat shares this path; blocking on a full buffer would
        // prevent the miss-count that tears down a wedged GUI.
        if self.writer_tx.try_send(line).is_err() {
            self.remove_pending(&invoke_id);
            return Response::error(
                String::new(),
                "gui_disconnected",
                "GUI writer unreachable (channel full or closed)",
            );
        }
        match rx.recv_timeout(timeout) {
            Ok(resp) => resp,
            Err(_) => {
                self.remove_pending(&invoke_id);
                Response::error(
                    String::new(),
                    "gui_timeout",
                    &format!("no GUI response within {:?}", timeout),
                )
            }
        }
    }

    fn remove_pending(&self, invoke_id: &str) {
        if let Some(map) = self.pending.lock().unwrap().as_mut() {
            map.remove(invoke_id);
        }
    }

    pub fn resolve(&self, response: Response) {
        let tx = self
            .pending
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|m| m.remove(&response.id));
        if let Some(tx) = tx {
            let _ = tx.send(response);
        }
    }

    /// Drains pending Invokes with `err`, marks the client disconnected, and tears down the socket.
    pub fn fail_all_pending(&self, err: ResponseError) {
        // Stop the forwarder BEFORE shutdown, or it can push lines into writer_tx that nothing
        // will read.
        self.forwarder_stop.store(true, Ordering::SeqCst);
        let drained = self.pending.lock().unwrap().take();
        if let Some(map) = drained {
            for (id, tx) in map {
                let _ = tx.send(Response {
                    id,
                    ok: false,
                    result: None,
                    error: Some(err.clone()),
                });
            }
        }
        if let Some(stream) = self.shutdown_handle.lock().unwrap().take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[derive(Default)]
pub struct GuiRegistry {
    clients: Mutex<HashMap<String, Arc<GuiClient>>>,
    /// Registration order, newest last. Primary promotion picks the most recent
    /// `want_primary=true` entry.
    order: Mutex<Vec<String>>,
    primary: Mutex<Option<String>>,
}

impl GuiRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns `(client_id, is_primary)`. Lock order: clients -> order -> primary (same as
    /// `unregister`/`route`; reversing deadlocks).
    pub fn register(
        self: &Arc<Self>,
        capabilities: HashSet<String>,
        want_primary: bool,
        gui_env: HashMap<String, String>,
        writer_tx: SyncSender<String>,
        shutdown_handle: Option<UnixStream>,
    ) -> (String, bool) {
        let client_id = uuid::Uuid::new_v4().to_string();
        let caps_summary = {
            let mut v: Vec<&str> = capabilities.iter().map(String::as_str).collect();
            v.sort();
            v.join(",")
        };
        let client = Arc::new(GuiClient {
            client_id: client_id.clone(),
            capabilities,
            want_primary,
            gui_env,
            writer_tx,
            pending: Mutex::new(Some(HashMap::new())),
            shutdown_handle: Mutex::new(shutdown_handle),
            forwarder_stop: Arc::new(AtomicBool::new(false)),
        });
        let weak_client = Arc::downgrade(&client);
        let mut clients = self.clients.lock().unwrap();
        let mut order = self.order.lock().unwrap();
        let mut primary = self.primary.lock().unwrap();
        clients.insert(client_id.clone(), client);
        order.push(client_id.clone());
        let is_primary = if primary.is_none() && want_primary {
            *primary = Some(client_id.clone());
            true
        } else {
            false
        };
        drop(primary);
        drop(order);
        drop(clients);

        let weak_reg = Arc::downgrade(self);
        let cid = client_id.clone();
        let _ = thread::Builder::new()
            .name(format!(
                "copad-heartbeat-{}",
                &client_id[..8.min(client_id.len())]
            ))
            .spawn(move || heartbeat_loop(weak_client, weak_reg, cid, HeartbeatConfig::PROD));

        log::info!(
            "gui registered: client_id={client_id} primary={is_primary} caps={caps_summary}"
        );
        (client_id, is_primary)
    }

    pub fn unregister(&self, client_id: &str) {
        // Lock order: clients -> order -> primary, as in `register`.
        let mut clients = self.clients.lock().unwrap();
        let mut order = self.order.lock().unwrap();
        let mut primary = self.primary.lock().unwrap();
        let removed = clients.remove(client_id);
        order.retain(|id| id != client_id);
        if primary.as_deref() == Some(client_id) {
            *primary = order
                .iter()
                .rev()
                .find(|id| clients.get(*id).map(|c| c.want_primary).unwrap_or(false))
                .cloned();
        }
        drop(primary);
        drop(order);
        drop(clients);
        if let Some(client) = removed {
            log::info!("gui unregistered: client_id={client_id}");
            client.fail_all_pending(ResponseError {
                code: "gui_disconnected".into(),
                message: "GUI client unregistered".into(),
            });
        }
    }

    /// Lock order: clients -> primary, as in `register`.
    pub fn route(
        &self,
        method: &str,
        target: Option<&str>,
    ) -> Result<Arc<GuiClient>, &'static str> {
        let Some(cap) = method_capability(method) else {
            return Err("not_gui_owned");
        };
        let clients = self.clients.lock().unwrap();
        let primary = self.primary.lock().unwrap().clone();
        let candidate = match target {
            Some(target_id) => clients.get(target_id).cloned().ok_or("unknown_client")?,
            None => {
                let primary_id = primary.ok_or("no_gui")?;
                clients.get(&primary_id).cloned().ok_or("no_gui")?
            }
        };
        if candidate.capabilities.contains(cap) {
            Ok(candidate)
        } else {
            Err("no_gui")
        }
    }

    pub fn get(&self, client_id: &str) -> Option<Arc<GuiClient>> {
        self.clients.lock().unwrap().get(client_id).cloned()
    }

    /// `None` when no primary is registered (headless). Lock order `clients -> primary`, as in
    /// `route()`.
    pub fn primary_gui_env(&self) -> Option<HashMap<String, String>> {
        let clients = self.clients.lock().unwrap();
        let primary = self.primary.lock().unwrap().clone();
        let primary_id = primary?;
        clients.get(&primary_id).map(|c| c.gui_env.clone())
    }

    /// Drains the daemon `EventBus` into the GUI's writer channel as wire `Event` lines.
    ///
    /// MUST be called only after the `gui.register` ack is queued on `writer_tx`: an earlier
    /// event would reach the GUI's `await_register_ack` as its first line, read as a `Response`.
    pub fn start_event_forwarder(&self, client_id: &str, bus: Arc<EventBus>) {
        let Some(client) = self.get(client_id) else {
            return;
        };
        let weak_client = Arc::downgrade(&client);
        let stop = client.forwarder_stop.clone();
        let cid = client_id.to_string();
        let rx = bus.subscribe("*");
        let _ = thread::Builder::new()
            .name(format!(
                "copad-event-forwarder-{}",
                &cid[..8.min(cid.len())]
            ))
            .spawn(move || forwarder_loop(weak_client, rx, stop));
    }
}

fn forwarder_loop(
    weak_client: Weak<GuiClient>,
    rx: copad_core::event_bus::EventReceiver,
    stop: Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            RecvOutcome::Event(ev) => {
                // Echo prevention: an event with `bridge_id` was lifted off the GUI's
                // `_bus.publish` ingest; re-bridging it to the origin would loop.
                if ev.bridge_id.is_some() {
                    continue;
                }
                let Some(client) = weak_client.upgrade() else {
                    return;
                };
                // `source` and `origin` MUST round-trip: preflight promotion gates on `source`,
                // `[security] accept_external` on `origin`. Dropping origin would downgrade a
                // daemon-side `External` tag to `Internal` on the GUI bus.
                let wire = WireEvent::new(ev.kind, ev.payload)
                    .with_source(ev.source)
                    .with_origin(ev.origin);
                let line = match serde_json::to_string(&wire) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("event forwarder serialize: {e}");
                        continue;
                    }
                };
                use std::sync::mpsc::TrySendError;
                match client.writer_tx.try_send(line) {
                    Ok(_) => {}
                    // Drop on Full to keep the stop-flag check alive; the heartbeat on the same
                    // channel tears down a wedged GUI.
                    Err(TrySendError::Full(_)) => {
                        log::debug!("event forwarder: writer_tx full, dropping event");
                    }
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            RecvOutcome::Timeout => continue,
            RecvOutcome::Disconnected => return,
        }
    }
}

fn heartbeat_loop(
    weak_client: Weak<GuiClient>,
    weak_registry: Weak<GuiRegistry>,
    client_id: String,
    config: HeartbeatConfig,
) {
    let mut misses: u32 = 0;
    loop {
        thread::sleep(config.interval);
        let Some(client) = weak_client.upgrade() else {
            return;
        };
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let resp = client.invoke("_ping", json!({ "ts": ts }), config.timeout);
        drop(client);
        if resp.ok {
            misses = 0;
            continue;
        }
        misses += 1;
        if misses >= config.max_misses {
            if let Some(reg) = weak_registry.upgrade() {
                eprintln!(
                    "[copad] heartbeat: {misses} consecutive misses on {client_id}, unregistering"
                );
                reg.unregister(&client_id);
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mk_caps(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn method_capability_maps_known_legacy_methods() {
        assert_eq!(method_capability("tab.list"), Some("tab"));
        assert_eq!(method_capability("webview.click"), Some("webview"));
        assert_eq!(method_capability("terminal.exec"), Some("terminal"));
        assert_eq!(method_capability("claude.start"), Some("tab"));
        assert_eq!(method_capability("cockpit.open"), Some("agent.ui"));
        assert_eq!(method_capability("tab.switch"), Some("tab"));
        assert_eq!(method_capability("pane.focus_next"), Some("split"));
        assert_eq!(method_capability("pane.focus_prev"), Some("split"));
        // Without a capability the daemon treats it as daemon-owned and answers `unknown_method`
        // for a method both GUIs implement, and nothing reserves the name against a plugin.
        assert_eq!(method_capability("panel.focus"), Some("split"));
        assert_eq!(method_capability("webview.state"), Some("webview"));
        assert_eq!(method_capability("system.ping"), None);
        assert_eq!(method_capability("kb.search"), None);
    }

    #[test]
    fn the_browser_namespace_routes_to_the_gui_like_webview_does() {
        // `browser.secret.*` lives in the GUI behind the same authorize gate as `webview.*`;
        // without a capability the daemon would answer `unknown_method` for it.
        assert_eq!(method_capability("browser.secret.list"), Some("webview"));
        assert_eq!(method_capability("browser.secret.fill"), Some("webview"));
        assert_eq!(method_capability("browser.secret.save"), Some("webview"));
        assert_eq!(method_capability("browser.secret.delete"), Some("webview"));
    }

    #[test]
    fn browser_methods_get_the_long_webview_timeout() {
        assert!(method_invoke_timeout("browser.secret.fill") > Duration::from_secs(5));
        assert_eq!(
            method_invoke_timeout("browser.secret.fill"),
            method_invoke_timeout("webview.execute_js")
        );
    }

    #[test]
    fn plugin_dot_name_dot_cmd_is_daemon_owned() {
        assert_eq!(method_capability("plugin.echo.greet"), None);
        assert_eq!(method_capability("plugin.todo.add"), None);
        // Single-dot `plugin.open` and `plugin.list` are unaffected.
        assert_eq!(method_capability("plugin.open"), Some("plugin.open"));
    }

    #[test]
    fn filter_gui_env_keeps_allowed_strips_disallowed() {
        let raw = json!({
            "HYPRLAND_INSTANCE_SIGNATURE": "sig-1",
            "DISPLAY": ":0",
            "PATH": "/tmp/evil",
            "LD_PRELOAD": "/tmp/bad.so",
            "OPENAI_API_KEY": "sk-secret",
            "XDG_RUNTIME_DIR": "/run/user/1000",
        });
        let filtered = filter_gui_env(&raw);
        assert_eq!(
            filtered
                .get("HYPRLAND_INSTANCE_SIGNATURE")
                .map(String::as_str),
            Some("sig-1")
        );
        assert_eq!(filtered.get("DISPLAY").map(String::as_str), Some(":0"));
        assert_eq!(
            filtered.get("XDG_RUNTIME_DIR").map(String::as_str),
            Some("/run/user/1000")
        );
        assert!(!filtered.contains_key("PATH"), "PATH must be filtered out");
        assert!(
            !filtered.contains_key("LD_PRELOAD"),
            "LD_PRELOAD must be filtered out"
        );
        assert!(
            !filtered.contains_key("OPENAI_API_KEY"),
            "secrets must be filtered out"
        );
    }

    #[test]
    fn filter_gui_env_rejects_non_string_values() {
        let raw = json!({"DISPLAY": 42, "WAYLAND_DISPLAY": "wayland-0"});
        let filtered = filter_gui_env(&raw);
        assert!(
            !filtered.contains_key("DISPLAY"),
            "non-string values rejected"
        );
        assert_eq!(
            filtered.get("WAYLAND_DISPLAY").map(String::as_str),
            Some("wayland-0")
        );
    }

    #[test]
    fn primary_gui_env_returns_filtered_set_for_primary() {
        let reg = GuiRegistry::new();
        let (tx, _rx) = sync_channel::<String>(64);
        let mut env = HashMap::new();
        env.insert(
            "HYPRLAND_INSTANCE_SIGNATURE".to_string(),
            "sig-x".to_string(),
        );
        let (_, _) = reg.register(mk_caps(&["tab"]), true, env, tx, None);
        let got = reg.primary_gui_env().expect("primary env present");
        assert_eq!(
            got.get("HYPRLAND_INSTANCE_SIGNATURE").map(String::as_str),
            Some("sig-x")
        );
    }

    #[test]
    fn primary_gui_env_returns_none_with_no_primary() {
        let reg = GuiRegistry::new();
        assert!(reg.primary_gui_env().is_none());
    }

    #[test]
    fn first_want_primary_becomes_primary() {
        let reg = GuiRegistry::new();
        let (tx, _rx) = sync_channel::<String>(64);
        let (_, is_primary) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), tx, None);
        assert!(is_primary);
    }

    #[test]
    fn want_primary_false_stays_secondary() {
        let reg = GuiRegistry::new();
        let (tx, _rx) = sync_channel::<String>(64);
        let (_, is_primary) = reg.register(mk_caps(&["tab"]), false, HashMap::new(), tx, None);
        assert!(!is_primary);
        let (tx2, _rx2) = sync_channel::<String>(64);
        let (_, is_primary2) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), tx2, None);
        assert!(is_primary2);
    }

    #[test]
    fn second_register_with_want_primary_stays_secondary() {
        let reg = GuiRegistry::new();
        let (tx1, _rx1) = sync_channel::<String>(64);
        let (_, p1) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), tx1, None);
        let (tx2, _rx2) = sync_channel::<String>(64);
        let (_, p2) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), tx2, None);
        assert!(p1);
        assert!(!p2);
    }

    #[test]
    fn route_returns_primary_for_matching_capability() {
        let reg = GuiRegistry::new();
        let (tx, _rx) = sync_channel::<String>(64);
        let (cid, _) = reg.register(mk_caps(&["tab", "split"]), true, HashMap::new(), tx, None);
        let client = reg.route("tab.list", None).expect("routed");
        assert_eq!(client.client_id, cid);
    }

    #[test]
    fn route_no_gui_when_no_primary() {
        let reg = GuiRegistry::new();
        assert_eq!(reg.route("tab.list", None).err(), Some("no_gui"));
    }

    #[test]
    fn route_no_gui_when_capability_missing() {
        let reg = GuiRegistry::new();
        let (tx, _rx) = sync_channel::<String>(64);
        reg.register(mk_caps(&["split"]), true, HashMap::new(), tx, None); // no "tab" cap
        assert_eq!(reg.route("tab.list", None).err(), Some("no_gui"));
    }

    #[test]
    fn route_target_client_id_picks_specific() {
        let reg = GuiRegistry::new();
        let (tx_primary, _rx_p) = sync_channel::<String>(64);
        let (_, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), tx_primary, None);
        let (tx_secondary, _rx_s) = sync_channel::<String>(64);
        let (secondary_id, _) =
            reg.register(mk_caps(&["tab"]), false, HashMap::new(), tx_secondary, None);
        let client = reg.route("tab.list", Some(&secondary_id)).expect("routed");
        assert_eq!(client.client_id, secondary_id);
    }

    #[test]
    fn route_target_unknown_returns_unknown_client() {
        let reg = GuiRegistry::new();
        assert_eq!(
            reg.route("tab.list", Some("nope")).err(),
            Some("unknown_client")
        );
    }

    #[test]
    fn route_non_gui_owned_method_returns_not_gui_owned() {
        let reg = GuiRegistry::new();
        let (tx, _rx) = sync_channel::<String>(64);
        reg.register(mk_caps(&["tab"]), true, HashMap::new(), tx, None);
        assert_eq!(reg.route("system.ping", None).err(), Some("not_gui_owned"));
    }

    #[test]
    fn unregister_primary_promotes_most_recent_want_primary() {
        let reg = GuiRegistry::new();
        let (tx1, _rx1) = sync_channel::<String>(64);
        let (id1, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), tx1, None);
        let (tx2, _rx2) = sync_channel::<String>(64);
        let (id2, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), tx2, None);
        let (tx3, _rx3) = sync_channel::<String>(64);
        let (id3, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), tx3, None);
        // Dropping id1 promotes the most recent (id3), not id2.
        reg.unregister(&id1);
        let routed = reg.route("tab.list", None).expect("primary transferred");
        assert_eq!(routed.client_id, id3);
        reg.unregister(&id3);
        let routed = reg
            .route("tab.list", None)
            .expect("primary transferred again");
        assert_eq!(routed.client_id, id2);
    }

    #[test]
    fn invoke_timeout_returns_gui_timeout_error() {
        let reg = GuiRegistry::new();
        let (writer_tx, _writer_rx) = sync_channel(64);
        let (_, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), writer_tx, None);
        let client = reg.route("tab.list", None).unwrap();
        let resp = client.invoke("tab.list", json!({}), Duration::from_millis(50));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "gui_timeout");
    }

    #[test]
    fn heartbeat_unregisters_after_consecutive_misses() {
        // Tight cadence; the dropped writer_rx makes every invoke fail, counting as a miss.
        let reg = GuiRegistry::new();
        let (writer_tx, writer_rx) = sync_channel::<String>(64);
        drop(writer_rx);
        let (cid, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), writer_tx, None);
        let client = reg.get(&cid).unwrap();
        let weak_client = Arc::downgrade(&client);
        drop(client);
        let weak_reg = Arc::downgrade(&reg);
        let cid_thread = cid.clone();
        thread::spawn(move || {
            heartbeat_loop(
                weak_client,
                weak_reg,
                cid_thread,
                HeartbeatConfig {
                    interval: Duration::from_millis(30),
                    timeout: Duration::from_millis(30),
                    max_misses: 2,
                },
            );
        });
        let start = std::time::Instant::now();
        while reg.get(&cid).is_some() && start.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            reg.get(&cid).is_none(),
            "heartbeat should have unregistered the client within 2s"
        );
    }

    #[test]
    fn invoke_after_unregister_returns_disconnect_fast() {
        // route() hands out an Arc<GuiClient>, unregister fires, then the caller invokes: must
        // surface gui_disconnected, not wait out the timeout.
        let reg = GuiRegistry::new();
        let (writer_tx, _writer_rx) = sync_channel::<String>(64);
        let (cid, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), writer_tx, None);
        let client = reg.get(&cid).unwrap();
        reg.unregister(&cid);
        let start = std::time::Instant::now();
        let resp = client.invoke("tab.list", json!({}), Duration::from_secs(5));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "gui_disconnected");
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "invoke after disconnect must return immediately, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn unregister_fails_pending_invokes_with_disconnect() {
        let reg = GuiRegistry::new();
        let (writer_tx, _writer_rx) = sync_channel(64);
        let (cid, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), writer_tx, None);
        let client = reg.get(&cid).unwrap();
        let client_clone = client.clone();
        let handle = std::thread::spawn(move || {
            client_clone.invoke("tab.list", json!({}), Duration::from_secs(5))
        });
        std::thread::sleep(Duration::from_millis(30));
        reg.unregister(&cid);
        let resp = handle.join().unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "gui_disconnected");
    }

    #[test]
    fn event_forwarder_writes_bus_events_to_writer() {
        let reg = GuiRegistry::new();
        let bus = Arc::new(EventBus::new());
        let (writer_tx, writer_rx) = sync_channel::<String>(64);
        let (cid, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), writer_tx, None);
        reg.start_event_forwarder(&cid, bus.clone());
        thread::sleep(Duration::from_millis(50));
        bus.publish(copad_core::event_bus::Event::new(
            "todo.create.completed",
            "test",
            json!({ "id": "t-1" }),
        ));
        let line = writer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("forwarder should have written within 1s");
        let parsed: WireEvent = serde_json::from_str(&line).expect("valid Event wire shape");
        assert_eq!(parsed.event_type, "todo.create.completed");
        assert_eq!(parsed.data["id"], json!("t-1"));
    }

    #[test]
    fn invoke_fast_fails_when_writer_buffer_full() {
        let reg = GuiRegistry::new();
        let (writer_tx, writer_rx) = sync_channel::<String>(1);
        drop(writer_rx);
        let (cid, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), writer_tx, None);
        let client = reg.get(&cid).unwrap();
        let start = std::time::Instant::now();
        let resp = client.invoke("tab.list", json!({}), Duration::from_secs(5));
        assert!(!resp.ok);
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "must NOT block on full/closed buffer, took {:?}",
            start.elapsed()
        );
        assert_eq!(resp.error.unwrap().code, "gui_disconnected");
    }

    #[test]
    fn event_forwarder_skips_bridged_events() {
        // Events with `bridge_id` came from the GUI's `_bus.publish`; echoing them back would loop.
        let reg = GuiRegistry::new();
        let bus = Arc::new(EventBus::new());
        let (writer_tx, writer_rx) = sync_channel::<String>(64);
        let (cid, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), writer_tx, None);
        reg.start_event_forwarder(&cid, bus.clone());
        thread::sleep(Duration::from_millis(50));
        bus.publish_bridged(
            copad_core::event_bus::Event::new("bridged.kind", "src", json!({})),
            42,
        );
        bus.publish(copad_core::event_bus::Event::new(
            "native.kind",
            "src",
            json!({}),
        ));
        let line = writer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("forwarder should deliver the native event");
        let parsed: WireEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.event_type, "native.kind");
        match writer_rx.recv_timeout(Duration::from_millis(200)) {
            Err(_) => {}
            Ok(extra) => {
                let parsed: WireEvent = serde_json::from_str(&extra).unwrap();
                panic!("forwarder leaked bridged event: {}", parsed.event_type);
            }
        }
    }

    #[test]
    fn event_forwarder_stops_on_unregister() {
        // After unregister no more events flow (`forwarder_stop` is flipped by fail_all_pending).
        let reg = GuiRegistry::new();
        let bus = Arc::new(EventBus::new());
        let (writer_tx, writer_rx) = sync_channel::<String>(64);
        let (cid, _) = reg.register(mk_caps(&["tab"]), true, HashMap::new(), writer_tx, None);
        reg.start_event_forwarder(&cid, bus.clone());
        thread::sleep(Duration::from_millis(50));
        bus.publish(copad_core::event_bus::Event::new(
            "before.unreg",
            "test",
            json!({}),
        ));
        writer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first event delivered");
        reg.unregister(&cid);
        thread::sleep(Duration::from_millis(300));
        bus.publish(copad_core::event_bus::Event::new(
            "after.unreg",
            "test",
            json!({}),
        ));
        if let Ok(line) = writer_rx.recv_timeout(Duration::from_millis(300)) {
            let parsed: WireEvent = serde_json::from_str(&line).unwrap();
            assert_ne!(
                parsed.event_type, "after.unreg",
                "forwarder must NOT deliver events post-unregister"
            );
        }
    }
}
