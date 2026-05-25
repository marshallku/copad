//! First-party nestty plugin: HTTP+WebSocket broker over nesttyd's
//! socket surface. Slice 3.0 of the remote-harness effort.
//!
//! Lifecycle: this binary IS a nestty service plugin. The supervisor
//! spawns it with stdio piped and expects an `initialize` reply, the
//! same handshake `plugins/discord/src/main.rs` runs. Once that
//! handshake completes, the stdio side stays idle and the real work
//! happens in a tokio runtime that owns an axum HTTP+WS listener.
//!
//! Daemon access: the supervisor injects `NESTTY_SOCKET` into the
//! child env (see `nestty-daemon/src/service_supervisor.rs` —
//! `start_service_inner` sets it alongside the plugin metadata env
//! vars). The plugin opens raw daemon-socket connections (NOT the
//! service-plugin RPC channel) so requests flow through the daemon's
//! normal `dispatch()` path → `GuiRegistry` routing, reaching
//! GUI-owned methods like `terminal.read` / `terminal.feed` /
//! `session.list`.
//!
//! Auth: Bearer token in `Authorization` header, OR
//! `Sec-WebSocket-Protocol: bearer.<token>` for WS upgrades. The
//! middleware never accepts a query-string token. The token comes
//! from `NESTTY_WEB_BRIDGE_TOKEN` env and must be ≥32 chars; if
//! missing/short the plugin exits before binding.

mod agents;
mod daemon_client;
mod push;
mod tmux;

use std::io::{BufRead, BufReader, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};

use daemon_client::DaemonClient;

const PROTOCOL_VERSION: u32 = 1;
const DEFAULT_BIND: &str = "127.0.0.1:7575";
const TOKEN_MIN_LEN: usize = 32;
const DEFAULT_RECENT_LINES: u64 = 5;
const MAX_RECENT_LINES: u64 = 200;

type StdoutHandle = Arc<Mutex<std::io::Stdout>>;

fn main() -> ExitCode {
    // 1. Token validation must happen BEFORE the supervisor handshake
    //    so a misconfigured deploy fails fast and visibly. If we wait
    //    until after `initialize`, the plugin appears healthy in the
    //    supervisor's eyes but the HTTP listener is dead — much harder
    //    to diagnose.
    let token = match validate_token_env(std::env::var("NESTTY_WEB_BRIDGE_TOKEN").ok().as_deref()) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("[web-bridge] {msg}");
            return ExitCode::from(2);
        }
    };

    let bind_addr =
        std::env::var("NESTTY_WEB_BRIDGE_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());

    let socket_path = match std::env::var("NESTTY_SOCKET") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!(
                "[web-bridge] NESTTY_SOCKET env is not set. The supervisor should \
                 inject this; without it the bridge cannot reach the daemon. \
                 Falling back to /tmp/nestty.sock for debugging."
            );
            "/tmp/nestty.sock".to_string()
        }
    };

    eprintln!(
        "[web-bridge] starting; bind={bind_addr} socket={socket_path} token_len={}",
        token.len()
    );

    // 2. Spawn the HTTP listener on a tokio runtime in a dedicated
    //    OS thread. Keep the main thread free for stdio framing so
    //    the supervisor's `initialize` / `shutdown` messages stay
    //    responsive even when axum is mid-handling a long WS stream.
    let token_for_server = token.clone();
    let socket_for_server = socket_path.clone();
    let bind_for_server = bind_addr.clone();
    std::thread::Builder::new()
        .name("web-bridge-http".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[web-bridge] tokio runtime build failed: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(e) =
                    run_server(&bind_for_server, &token_for_server, &socket_for_server).await
                {
                    eprintln!("[web-bridge] server exited: {e}");
                }
            });
        })
        .expect("spawn web-bridge-http thread");

    // 3. Stdio RPC handshake loop. Same shape as discord plugin
    //    (`plugins/discord/src/main.rs` `run_rpc`). For Slice 3.0 we
    //    only need `initialize` and `shutdown`; the plugin provides
    //    no actions, so there are no `action.invoke` frames to
    //    route. Anything else is ignored with a warning.
    let stdin = std::io::stdin();
    let stdout: StdoutHandle = Arc::new(Mutex::new(std::io::stdout()));
    let reader = BufReader::new(stdin.lock());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        let frame: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[web-bridge] stdio parse error: {e}");
                continue;
            }
        };
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let id = frame.get("id").and_then(Value::as_str).unwrap_or("");
        match method {
            "initialize" => {
                let proto = frame
                    .get("params")
                    .and_then(|p| p.get("protocol_version"))
                    .and_then(Value::as_u64);
                if proto != Some(PROTOCOL_VERSION as u64) {
                    emit_error(
                        &stdout,
                        id,
                        "protocol_mismatch",
                        &format!(
                            "web-bridge plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"
                        ),
                    );
                    continue;
                }
                emit_response(
                    &stdout,
                    id,
                    &json!({
                        "service_version": env!("CARGO_PKG_VERSION"),
                        "provides": [],
                        "subscribes": [],
                    }),
                );
            }
            "initialized" => {
                // Supervisor's two-phase handshake: we already spun up
                // the HTTP listener on initialize, so the post-init
                // notification is informational. Don't need to gate
                // anything on it (discord does, to delay its Gateway
                // WS until daemon is fully ready; web-bridge has no
                // such ordering concern).
            }
            "shutdown" => {
                emit_response(&stdout, id, &json!({ "ok": true }));
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("[web-bridge] unexpected stdio method: {other:?}");
            }
        }
    }
    ExitCode::SUCCESS
}

/// Validate the env-supplied bearer token. Returns `Ok(token)` on
/// pass, `Err(message)` on fail — caller prints the message and
/// exits non-zero. Factored out of `main` so it's unit-testable.
fn validate_token_env(raw: Option<&str>) -> Result<String, String> {
    match raw {
        Some(t) if t.len() >= TOKEN_MIN_LEN => Ok(t.to_string()),
        Some(short) => Err(format!(
            "NESTTY_WEB_BRIDGE_TOKEN is too short ({} chars; need ≥{TOKEN_MIN_LEN}). Refusing to start.",
            short.len()
        )),
        None => Err(format!(
            "NESTTY_WEB_BRIDGE_TOKEN is not set. \
             See plugins/web-bridge/plugin.toml for setup. \
             Token must be ≥{TOKEN_MIN_LEN} chars. Refusing to start."
        )),
    }
}

fn emit_response(stdout: &StdoutHandle, id: &str, result: &Value) {
    // Wire shape mirrors `plugins/discord/src/main.rs::send_response`:
    // supervisor's frame parser requires `ok: true` alongside `result`,
    // not just `{id, result}`. Without `ok` the supervisor logs
    // "sent unparseable line" and the plugin times out on initialize.
    let frame = json!({ "id": id, "ok": true, "result": result });
    write_frame(stdout, &frame);
}

fn emit_error(stdout: &StdoutHandle, id: &str, code: &str, message: &str) {
    let frame = json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message },
    });
    write_frame(stdout, &frame);
}

fn write_frame(stdout: &StdoutHandle, frame: &Value) {
    let line = frame.to_string();
    let mut guard = stdout.lock().expect("stdout poisoned");
    let _ = guard.write_all(line.as_bytes());
    let _ = guard.write_all(b"\n");
    let _ = guard.flush();
}

#[derive(Clone)]
struct AppState {
    daemon: DaemonClient,
    token: Arc<String>,
    /// True only when the HTTP listener is bound to a loopback
    /// address. Header-based Tailscale auth (`Tailscale-User-Login`)
    /// is unsafe on non-loopback binds because any reachable caller
    /// could forge the header — so we disable that path entirely
    /// when bound to a routable interface and require bearer auth.
    allow_tailscale_header: bool,
    default_subscribe_patterns: Arc<Vec<String>>,
    /// In-process 2 s TTL cache for `/api/tmux/panes`. Multiple
    /// dashboard tabs hitting refresh shouldn't fan out as N tmux
    /// shell-outs per click; once the snapshot is built we serve it
    /// from memory until the TTL expires.
    tmux_cache: Arc<tokio::sync::Mutex<Option<TmuxCacheEntry>>>,
    /// VAPID config from env. `None` disables the push endpoints (501).
    push_config: Arc<Option<push::PushConfig>>,
    /// Loaded subscription list — serialised through this Mutex on
    /// every read/write so the file-on-disk and the in-memory mirror
    /// stay consistent. Pruning on 410 Gone happens through the same
    /// guard from the push-trigger task.
    push_subs: Arc<tokio::sync::Mutex<Vec<push::Subscription>>>,
}

#[derive(Clone)]
struct TmuxCacheEntry {
    at: std::time::Instant,
    value: Value,
}

/// True when `bind` resolves to a loopback address (`127.0.0.0/8`
/// for IPv4 or `::1` for IPv6). Header-based Tailscale auth is only
/// safe under that invariant — see `AppState::allow_tailscale_header`.
/// Returns false for unparseable bind strings (fail-closed).
fn bind_is_loopback(bind: &str) -> bool {
    use std::net::SocketAddr;
    let parsed: Option<SocketAddr> = bind.parse().ok();
    match parsed {
        Some(SocketAddr::V4(a)) => a.ip().is_loopback(),
        Some(SocketAddr::V6(a)) => a.ip().is_loopback(),
        None => false,
    }
}

async fn run_server(
    bind: &str,
    token: &str,
    socket_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use axum::{
        Router,
        http::StatusCode,
        middleware::Next,
        response::IntoResponse,
        routing::{get, post},
    };

    let push_config = push::PushConfig::from_env();
    if push_config.is_none() {
        eprintln!(
            "[web-bridge] NESTTY_WEB_BRIDGE_VAPID_PRIVATE/PUBLIC not set — push notifications disabled"
        );
    }
    let allow_tailscale_header = bind_is_loopback(bind);
    if !allow_tailscale_header {
        eprintln!(
            "[web-bridge] bind={bind} is not loopback; disabling Tailscale-User-Login header auth (bearer token required)"
        );
    }
    let state = AppState {
        daemon: DaemonClient::new(socket_path),
        token: Arc::new(token.to_string()),
        allow_tailscale_header,
        tmux_cache: Arc::new(tokio::sync::Mutex::new(None)),
        push_config: Arc::new(push_config),
        push_subs: Arc::new(tokio::sync::Mutex::new(push::load_subscriptions())),
        default_subscribe_patterns: Arc::new(vec![
            "presence.*".to_string(),
            "claude.*".to_string(),
            "discord.send_message.*".to_string(),
            "notify.show.*".to_string(),
        ]),
    };

    let auth_state = state.clone();
    let allow_ts_header = state.allow_tailscale_header;
    let app = Router::new()
        .route("/", get(handle_index))
        .route("/healthz", get(handle_healthz))
        .route(
            "/api/presence",
            get(handle_presence_get).post(handle_presence_set),
        )
        .route("/api/panes", get(handle_panes_list))
        .route("/api/panes/:id/recent", get(handle_pane_recent))
        .route("/api/panes/:id/input", post(handle_pane_input))
        .route("/api/events", get(handle_events_history))
        .route("/api/tmux/panes", get(handle_tmux_panes))
        .route("/api/tmux/send", post(handle_tmux_send))
        .route("/ws/tmux/overview", get(handle_ws_tmux_overview))
        .route("/ws/tmux/attach/:pane_id", get(handle_ws_tmux_attach))
        .route("/ws/events", get(handle_ws_events))
        .route("/api/whoami", get(handle_whoami))
        .route("/api/push/vapid-public", get(handle_push_vapid_public))
        .route("/api/push/subscribe", post(handle_push_subscribe))
        .route(
            "/api/push/subscribe/:id",
            axum::routing::delete(handle_push_unsubscribe),
        )
        .route("/api/push/test", post(handle_push_test))
        .route("/manifest.webmanifest", get(handle_manifest))
        .route("/sw.js", get(handle_service_worker))
        .route("/icon.svg", get(handle_icon))
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: Next| {
                let token = auth_state.token.clone();
                async move {
                    let path = req.uri().path();
                    // SPA root + health are unauthenticated so the
                    // dashboard HTML loads before JS injects the token
                    // into the Authorization header on subsequent
                    // /api/* / /ws/* calls. PWA manifest + service
                    // worker + icon are unauthenticated for the same
                    // reason — the browser fetches them on install
                    // without an Authorization header. None of these
                    // expose data (manifest is metadata, sw.js owns
                    // only post-permission push notifications).
                    let public = path == "/"
                        || path == "/healthz"
                        || path == "/manifest.webmanifest"
                        || path == "/sw.js"
                        || path == "/icon.svg";
                    let upgrades = path.starts_with("/ws/");
                    if public {
                        return next.run(req).await;
                    }
                    // Tailscale serve injects `Tailscale-User-Login`
                    // (and `-User-Name`) on every request that came
                    // through it. Our plugin only binds 127.0.0.1, so
                    // any external attacker would need to be in the
                    // tailnet AND going through serve to reach us —
                    // trust the header. Anyone with raw localhost
                    // access already has the bearer token in env / on
                    // disk, so per-process header forgery is a
                    // theoretical not a real attack here.
                    let ts_authed = allow_ts_header
                        && req
                            .headers()
                            .get("tailscale-user-login")
                            .and_then(|h| h.to_str().ok())
                            .map(|s| !s.is_empty())
                            .unwrap_or(false);
                    if ts_authed {
                        return next.run(req).await;
                    }
                    if upgrades {
                        // WS auth via Sec-WebSocket-Protocol: bearer.<token>
                        if !ws_subprotocol_ok(req.headers(), &token) {
                            return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
                        }
                    } else if !bearer_ok(req.headers(), &token) {
                        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
                    }
                    next.run(req).await
                }
            },
        ))
        .with_state(state.clone());

    // Push trigger task. Polls the attention queue every 5 s. Anything
    // strictly newer than the last seen ts gets fanned out to every
    // subscription whose kinds filter accepts the event. 410 Gone /
    // 404 Not Found endpoints are pruned in-place. On startup we
    // anchor at the latest ts in the queue so we don't push the user's
    // entire backlog at boot. No-op when VAPID isn't configured.
    if state.push_config.is_some() {
        let push_state = state.clone();
        tokio::spawn(async move { push_loop(push_state).await });
    }

    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("[web-bridge] listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn push_loop(state: AppState) {
    let cfg = match state.push_config.as_ref().as_ref() {
        Some(c) => c.clone(),
        None => return,
    };
    // Per-entry fingerprint dedup instead of just `ts > last_seen`.
    // The bash hooks write second-granularity timestamps so two events
    // appended in the same second would collide on `ts` alone — we
    // need to know each entry by its content, not just its time. Set
    // capacity grows then GCs to the latest N when overflowing.
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut seen_order: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
    const SEEN_CAP: usize = 1024;
    // Pre-seed with the existing backlog so boot doesn't dump it.
    if let Ok(boot) = agents::read_snapshot() {
        for e in &boot.attention {
            let fp = attention_fingerprint(e);
            if seen.insert(fp) {
                seen_order.push_back(fp);
            }
        }
    }
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        // Pull a fresh tmx snapshot for its attention array. tmx
        // applies a 60-min cutoff; we just diff against `seen`.
        let snap = match agents::read_snapshot() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[web-bridge] push_loop: tmx snapshot failed: {e}");
                continue;
            }
        };
        let mut new_entries: Vec<agents::Attention> = snap
            .attention
            .into_iter()
            .filter(|e| !seen.contains(&attention_fingerprint(e)))
            .collect();
        if new_entries.is_empty() {
            continue;
        }
        // Walk oldest first so notifications arrive in chronological
        // order on the device.
        new_entries.sort_by_key(|e| e.ts);

        let subs_snapshot = state.push_subs.lock().await.clone();
        // Mark as seen unconditionally so a no-subscriber tick still
        // advances the dedup state.
        for e in &new_entries {
            let fp = attention_fingerprint(e);
            if seen.insert(fp) {
                seen_order.push_back(fp);
            }
        }
        while seen.len() > SEEN_CAP {
            if let Some(old) = seen_order.pop_front() {
                seen.remove(&old);
            } else {
                break;
            }
        }
        if subs_snapshot.is_empty() {
            continue;
        }
        let mut pruned_ids: Vec<String> = Vec::new();
        for entry in &new_entries {
            for sub in &subs_snapshot {
                if !sub.matches_kind(&entry.kind) {
                    continue;
                }
                let url = if entry.tmux_target.is_empty() {
                    "/".to_string()
                } else {
                    format!("/#attention/{}", urlenc(&entry.tmux_target))
                };
                let title = if entry.title.is_empty() {
                    "nestty".to_string()
                } else {
                    entry.title.clone()
                };
                let body = if entry.body.is_empty() {
                    entry.kind.clone()
                } else {
                    entry.body.clone()
                };
                let tag = format!("nestty-{}", entry.kind);
                let payload = push::PushPayload {
                    title: &title,
                    body: &body,
                    tag: &tag,
                    kind: &entry.kind,
                    url: &url,
                };
                if let Err(e) = push::send_to(&cfg, sub, &payload).await {
                    if push::is_terminal_error(&e) {
                        pruned_ids.push(sub.id.clone());
                    } else {
                        eprintln!("[web-bridge] push send error (kept): {e:?}");
                    }
                }
            }
        }
        if !pruned_ids.is_empty() {
            let mut subs = state.push_subs.lock().await;
            subs.retain(|s| !pruned_ids.contains(&s.id));
            if let Err(e) = push::save_subscriptions(&subs) {
                eprintln!("[web-bridge] push save_subscriptions failed: {e}");
            }
        }
    }
}

/// Fingerprint an attention entry across (ts, kind, title, body,
/// session_id). Stable hasher choice doesn't matter — we never persist
/// the value across restarts, just dedup within one push_loop lifetime.
fn attention_fingerprint(e: &agents::Attention) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    e.ts.hash(&mut h);
    e.kind.hash(&mut h);
    e.title.hash(&mut h);
    e.body.hash(&mut h);
    e.session_id.hash(&mut h);
    h.finish()
}

/// Minimal URL-encoder for the path-segment subset we emit. Avoids
/// pulling in `urlencoding` for one call site.
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let is_safe = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b':');
        if is_safe {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

async fn handle_index() -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse;
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../static/index.html"),
    )
        .into_response()
}

async fn handle_healthz() -> &'static str {
    "ok\n"
}

/// `GET /api/whoami` — surfaces which auth path the caller is on.
/// SPA hits this once on load (without an Authorization header) to
/// decide whether to skip the token-input page entirely. Tailscale
/// serve injects `Tailscale-User-Login` on every proxied request;
/// bearer-token requests show that fallback. Anything else gets 401
/// from the middleware before reaching here.
async fn handle_whoami(headers: axum::http::HeaderMap) -> axum::Json<Value> {
    let login = headers
        .get("tailscale-user-login")
        .and_then(|h| h.to_str().ok())
        .filter(|s| !s.is_empty());
    let name = headers
        .get("tailscale-user-name")
        .and_then(|h| h.to_str().ok())
        .filter(|s| !s.is_empty());
    if let Some(login) = login {
        return axum::Json(json!({
            "auth": "tailscale",
            "login": login,
            "name": name,
        }));
    }
    axum::Json(json!({ "auth": "bearer" }))
}

async fn handle_manifest() -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse;
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        include_str!("../static/manifest.webmanifest"),
    )
        .into_response()
}

async fn handle_service_worker() -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse;
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../static/sw.js"),
    )
        .into_response()
}

async fn handle_icon() -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse;
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        include_str!("../static/icon.svg"),
    )
        .into_response()
}

async fn handle_push_vapid_public(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<axum::Json<Value>, AppError> {
    let cfg = state
        .push_config
        .as_ref()
        .as_ref()
        .ok_or_else(|| AppError::custom("push_disabled", "VAPID env not configured"))?;
    Ok(axum::Json(
        json!({ "public_key": cfg.vapid_public_b64.clone() }),
    ))
}

#[derive(Deserialize)]
struct PushSubscribeBody {
    endpoint: String,
    keys: PushKeys,
    #[serde(default)]
    kinds: Vec<String>,
}

#[derive(Deserialize)]
struct PushKeys {
    p256dh: String,
    auth: String,
}

async fn handle_push_subscribe(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Json(body): axum::Json<PushSubscribeBody>,
) -> Result<axum::Json<Value>, AppError> {
    if state.push_config.as_ref().is_none() {
        return Err(AppError::custom(
            "push_disabled",
            "VAPID env not configured",
        ));
    }
    let id = push::subscription_id_for(&body.endpoint);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let new_sub = push::Subscription {
        id: id.clone(),
        endpoint: body.endpoint,
        p256dh: body.keys.p256dh,
        auth: body.keys.auth,
        kinds: body.kinds,
        created_at_ms: now_ms,
    };
    {
        let mut subs = state.push_subs.lock().await;
        subs.retain(|s| s.id != id);
        subs.push(new_sub);
        push::save_subscriptions(&subs)
            .map_err(|e| AppError::custom("persist", &format!("save subs: {e}")))?;
    }
    Ok(axum::Json(json!({ "id": id, "ok": true })))
}

async fn handle_push_unsubscribe(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::Json<Value>, AppError> {
    if state.push_config.as_ref().is_none() {
        return Err(AppError::custom(
            "push_disabled",
            "VAPID env not configured",
        ));
    }
    let removed = {
        let mut subs = state.push_subs.lock().await;
        let before = subs.len();
        subs.retain(|s| s.id != id);
        let after = subs.len();
        if before != after {
            push::save_subscriptions(&subs)
                .map_err(|e| AppError::custom("persist", &format!("save subs: {e}")))?;
        }
        before != after
    };
    Ok(axum::Json(json!({ "removed": removed })))
}

async fn handle_push_test(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<axum::Json<Value>, AppError> {
    let cfg = state
        .push_config
        .as_ref()
        .as_ref()
        .ok_or_else(|| AppError::custom("push_disabled", "VAPID env not configured"))?;
    let subs_snapshot = state.push_subs.lock().await.clone();
    let mut sent = 0usize;
    let mut pruned_ids: Vec<String> = Vec::new();
    for sub in subs_snapshot.into_iter() {
        let payload = push::PushPayload {
            title: "nestty",
            body: "test push from web-bridge",
            tag: "nestty-test",
            kind: "test",
            url: "/",
        };
        match push::send_to(cfg, &sub, &payload).await {
            Ok(_) => {
                sent += 1;
            }
            Err(e) if push::is_terminal_error(&e) => {
                pruned_ids.push(sub.id.clone());
            }
            Err(e) => {
                eprintln!("[web-bridge] push send error (kept): {e:?}");
            }
        }
    }
    // Retain-by-id instead of full replace so a concurrent subscribe
    // landed during the send fan-out isn't clobbered. Save only when
    // we actually pruned to avoid a no-op disk write per test.
    let pruned = pruned_ids.len();
    if pruned > 0 {
        let mut subs = state.push_subs.lock().await;
        subs.retain(|s| !pruned_ids.contains(&s.id));
        push::save_subscriptions(&subs)
            .map_err(|e| AppError::custom("persist", &format!("save subs: {e}")))?;
    }
    Ok(axum::Json(json!({ "sent": sent, "pruned": pruned })))
}

async fn handle_presence_get(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<axum::Json<Value>, AppError> {
    let v = state.daemon.rpc("presence.get", json!({})).await?;
    Ok(axum::Json(json!({ "state": v })))
}

#[derive(Deserialize)]
struct PresenceSet {
    state: String,
}

async fn handle_presence_set(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Json(body): axum::Json<PresenceSet>,
) -> Result<axum::Json<Value>, AppError> {
    let v = state
        .daemon
        .rpc("presence.set", json!({ "state": body.state }))
        .await?;
    Ok(axum::Json(v))
}

async fn handle_events_history(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<EventsQuery>,
) -> Result<axum::Json<Value>, AppError> {
    let mut params = serde_json::Map::new();
    if let Some(since) = q.since_ms {
        params.insert("since_ms".into(), json!(since));
    }
    if let Some(kind) = q.kind {
        params.insert("kind".into(), json!(kind));
    }
    let v = state
        .daemon
        .rpc("event.history", Value::Object(params))
        .await?;
    Ok(axum::Json(v))
}

#[derive(Deserialize)]
struct EventsQuery {
    since_ms: Option<u64>,
    kind: Option<String>,
}

async fn handle_panes_list(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<axum::Json<Value>, AppError> {
    // Slice 3.0 takes the simpler path: surface session.list
    // verbatim. cwd + recent_lines per-panel enrichment is a
    // separate /api/panes/:id/recent call. The dashboard renders
    // the bare list first (fast) and lazily fills per-pane detail
    // when the user expands one — avoids the N+1 fan-out on every
    // refresh.
    let v = state.daemon.rpc("session.list", json!({})).await?;
    Ok(axum::Json(v))
}

#[derive(Deserialize)]
struct RecentQuery {
    lines: Option<u64>,
}

async fn handle_pane_recent(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<RecentQuery>,
) -> Result<axum::Json<Value>, AppError> {
    let lines = q
        .lines
        .unwrap_or(DEFAULT_RECENT_LINES)
        .clamp(1, MAX_RECENT_LINES);
    // daemon expects `id` (resolve_terminal at nestty-linux/src/socket.rs:1225);
    // sending `panel_id` silently falls through to the active terminal.
    let raw = state
        .daemon
        .rpc("terminal.history", json!({ "id": id, "lines": lines }))
        .await?;
    // daemon returns `{text, lines_requested, rows, cols}`; UI consumes
    // `{lines: [...]}`. Split text into trimmed-trailing-empty lines.
    let text = raw.get("text").and_then(Value::as_str).unwrap_or("");
    let mut split_lines: Vec<&str> = text.split('\n').collect();
    while split_lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        split_lines.pop();
    }
    Ok(axum::Json(json!({
        "lines": split_lines,
        "rows": raw.get("rows"),
        "cols": raw.get("cols"),
    })))
}

#[derive(Deserialize)]
struct InputBody {
    text: String,
}

/// Composite tmux overview: `tmux list-panes -a` + per-pane
/// `capture-pane` for the last 5 lines. Cached for 2 s in-plugin.
/// Empty array (NOT error) when no tmux server is running — the SPA
/// renders a "no tmux sessions yet" empty state.
async fn handle_tmux_panes(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<axum::Json<Value>, AppError> {
    const TTL: std::time::Duration = std::time::Duration::from_secs(2);
    {
        let cache = state.tmux_cache.lock().await;
        if let Some(entry) = cache.as_ref()
            && entry.at.elapsed() < TTL
        {
            return Ok(axum::Json(entry.value.clone()));
        }
    }
    let snapshot = tokio::task::spawn_blocking(build_tmux_snapshot)
        .await
        .map_err(|e| AppError::custom("internal", &format!("tmux snapshot task: {e}")))??;
    {
        let mut cache = state.tmux_cache.lock().await;
        *cache = Some(TmuxCacheEntry {
            at: std::time::Instant::now(),
            value: snapshot.clone(),
        });
    }
    Ok(axum::Json(snapshot))
}

#[derive(Deserialize)]
struct TmuxSendBody {
    target: String,
    text: String,
}

async fn handle_tmux_send(
    axum::Json(body): axum::Json<TmuxSendBody>,
) -> Result<axum::Json<Value>, AppError> {
    let target = body.target;
    let text = body.text;
    tokio::task::spawn_blocking(move || tmux::send_text(&target, &text))
        .await
        .map_err(|e| AppError::custom("internal", &format!("tmux send task: {e}")))?
        .map_err(|msg| AppError::custom("tmux_error", &msg))?;
    Ok(axum::Json(json!({ "ok": true })))
}

/// Sync helper used inside `spawn_blocking`. Builds the overview JSON
/// `{panes, attention, codex_jobs}` by joining our `tmux list-panes`
/// rows against `tmx agents --json`. tmx owns the agent classification,
/// process-tree walk, attention queue read, and codex-job scan; we own
/// pane preview (`capture-pane`) and the join by `pane_pid`.
///
/// Failure modes degrade rather than abort:
///   * tmx not installed / fails → panes still render, `agent` is null
///     on every card, attention + codex_jobs are empty.
///   * Per-pane capture-pane failure → that card's `last_lines: []`.
fn build_tmux_snapshot() -> Result<Value, AppError> {
    let panes = tmux::list_panes().map_err(|msg| AppError::custom("tmux_error", &msg))?;
    let mut tmx_snap = agents::read_snapshot().unwrap_or_else(|e| {
        eprintln!("[web-bridge] tmx snapshot unavailable, degrading: {e}");
        agents::TmxSnapshot::default()
    });
    // tmx 1.x doesn't surface codex-companion jobs in its snapshot
    // yet; populate them locally from `~/.claude/state/codex-companion/`.
    tmx_snap.codex_jobs = agents::read_codex_jobs();
    let rows: Vec<Value> = panes
        .into_iter()
        .map(|p| {
            let last = tmux::capture_pane(&p.pane_id, 5)
                .ok()
                .map(|raw| {
                    let mut lines: Vec<String> = raw.split('\n').map(|s| s.to_string()).collect();
                    while lines.last().map(|s| s.is_empty()).unwrap_or(false) {
                        lines.pop();
                    }
                    lines
                })
                .unwrap_or_default();
            // Join on pane_pid — that's the one identifier tmx + our
            // list_panes both surface. Falls back to None when either
            // side is missing the pid (old tmx without pane_pid in its
            // JSON, or a non-numeric pane_pid from the tmux format).
            let agent = p.pane_pid.and_then(|pid| tmx_snap.agent_for_pane_pid(pid));
            let agent_json = agent.map(|a| {
                json!({
                    "kind": a.kind,
                    "status": a.status,
                    "repo_name": a.repo_name,
                    "extra": a.extra,
                    "flags": {
                        "has_intent": a.flags.has_intent,
                        "blocked": a.flags.blocked,
                        "reviewed_fresh": a.flags.reviewed_fresh,
                    },
                })
            });
            json!({
                "session": p.session,
                "window_id": p.window_id,
                "window_index": p.window_index,
                "window_name": p.window_name,
                "pane_id": p.pane_id,
                "pane_active": p.pane_active,
                "cwd": p.cwd,
                "last_lines": last,
                "agent": agent_json,
            })
        })
        .collect();
    // Attention + codex_jobs come straight from tmx — same window /
    // zombie-filter / classification it uses for its TUI.
    let mut attention = tmx_snap.attention;
    attention.truncate(20);
    Ok(json!({
        "panes": rows,
        "attention": attention,
        "codex_jobs": tmx_snap.codex_jobs,
    }))
}

async fn handle_pane_input(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<InputBody>,
) -> Result<axum::Json<Value>, AppError> {
    // daemon expects `id` (resolve_terminal at nestty-linux/src/socket.rs:1225);
    // sending `panel_id` silently routes to the active terminal — a
    // remote command would land on the wrong pane.
    let v = state
        .daemon
        .rpc("terminal.feed", json!({ "id": id, "text": body.text }))
        .await?;
    Ok(axum::Json(v))
}

/// `WS /ws/tmux/overview` — push a full tmux pane snapshot every 5 s
/// as a single JSON Text frame. No diff protocol; SPA re-renders the
/// card grid from each snapshot. Lifecycle: WS close → polling task
/// returns on next tick (channel closed); we don't need StopOnDrop
/// because the task itself drives the loop (vs blocking on a daemon
/// socket).
async fn handle_ws_tmux_overview(
    axum::extract::State(state): axum::extract::State<AppState>,
    ws: axum::extract::WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    let proto = format!("bearer.{}", &state.token);
    ws.protocols([proto]).on_upgrade(move |socket| async move {
        use axum::extract::ws::Message;
        use futures_util::{SinkExt, StreamExt};
        let (mut sink, mut stream) = futures_split(socket);
        // Initial snapshot immediately on connect so the UI doesn't
        // wait 5 s for the first paint.
        loop {
            let snapshot = match tokio::task::spawn_blocking(build_tmux_snapshot).await {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    eprintln!("[web-bridge] tmux overview snapshot failed: {e:?}");
                    Value::Array(vec![])
                }
                Err(e) => {
                    eprintln!("[web-bridge] tmux overview task join failed: {e}");
                    Value::Array(vec![])
                }
            };
            let payload = serde_json::to_string(&snapshot).unwrap_or_else(|_| "[]".into());
            if sink.send(Message::Text(payload)).await.is_err() {
                break;
            }
            // Sleep 5 s, but bail early if the client sends a close
            // frame so reconnects don't pile up dead overview tasks.
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                msg = stream.next() => {
                    match msg {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                        Some(Ok(_)) => { /* ignore client-pushed frames */ }
                    }
                }
            }
        }
    })
}

/// `WS /ws/tmux/attach/:pane_id` — bidirectional xterm.js attach.
/// Validate pane id → spawn `tmux attach-session -t <session>` inside
/// a portable_pty PTY pair → forward PTY bytes as WS Binary frames
/// and WS Binary frames back into PTY stdin. Text/JSON frames carry
/// `{type:"resize",rows,cols}` control messages.
async fn handle_ws_tmux_attach(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(pane_id): axum::extract::Path<String>,
    ws: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let panes = match tokio::task::spawn_blocking(tmux::list_panes).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            return (StatusCode::BAD_GATEWAY, format!("tmux list-panes: {e}\n")).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task join: {e}\n"),
            )
                .into_response();
        }
    };
    let Some(pane) = tmux::find_pane(&panes, &pane_id).cloned() else {
        return (StatusCode::NOT_FOUND, format!("pane {pane_id} not found\n")).into_response();
    };
    let proto = format!("bearer.{}", &state.token);
    ws.protocols([proto])
        .on_upgrade(move |socket| async move {
            if let Err(e) = run_attach(socket, pane).await {
                eprintln!("[web-bridge] attach session ended: {e}");
            }
        })
        .into_response()
}

/// Owns the lifecycle of one attach WS: PTY spawn + bidirectional
/// pump + child kill on close. Errors bubble up so the wrapping
/// `on_upgrade` future can log them with the pane context.
async fn run_attach(
    socket: axum::extract::ws::WebSocket,
    pane: tmux::TmuxPane,
) -> Result<(), String> {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;

    // `tmux attach-session -t <session>` then `select-pane` via a
    // chained command. tmux supports `\;` as a command separator, but
    // doing two shell-outs in sequence inside the PTY isn't possible
    // (the PTY runs ONE command). Instead use `attach-session -t` +
    // pre-position the active pane via a separate `tmux select-pane`
    // shell-out BEFORE spawning the PTY. The session's notion of
    // "active pane" survives the new attach (multi-client tmux model).
    // Position BOTH the active window and the active pane in the
    // target session BEFORE attach. select-pane alone does not
    // reliably promote the containing window to active across all
    // tmux versions, so a multi-window session can land the new
    // attach client on the wrong window. Do them in order: window
    // first, then pane.
    let win_status = std::process::Command::new("tmux")
        .args(["select-window", "-t", &pane.window_id])
        .status()
        .map_err(|e| format!("spawn tmux select-window: {e}"))?;
    if !win_status.success() {
        return Err(format!(
            "tmux select-window {} failed: {win_status}",
            pane.window_id
        ));
    }
    let pane_status = std::process::Command::new("tmux")
        .args(["select-pane", "-t", &pane.pane_id])
        .status()
        .map_err(|e| format!("spawn tmux select-pane: {e}"))?;
    if !pane_status.success() {
        return Err(format!(
            "tmux select-pane {} failed: {pane_status}",
            pane.pane_id
        ));
    }

    let mut cmd = CommandBuilder::new("tmux");
    // `-u` forces UTF-8 even when the spawned tmux client can't detect
    // it from locale. Without this, any non-ASCII byte renders as `?`
    // in panes whose programs (vim, less, fzf) trust tmux's notion of
    // UTF-8 support over their own locale.
    cmd.args(["-u", "attach-session", "-t", pane.session.as_str()]);
    // CommandBuilder starts with an EMPTY env. Forward the variables
    // that tmux + its child shells need for UTF-8 + path resolution +
    // sensible behaviour. Without LANG/LC_CTYPE the child shell falls
    // back to C locale and mangles every multibyte character; without
    // PATH it can't find non-builtin binaries.
    for var in [
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "PATH",
        "HOME",
        "USER",
        "SHELL",
        "COLORTERM",
    ] {
        if let Ok(v) = std::env::var(var) {
            cmd.env(var, v);
        }
    }
    // Fallback locale if the parent env had none — guarantees UTF-8.
    if std::env::var("LANG").is_err() && std::env::var("LC_ALL").is_err() {
        cmd.env("LANG", "C.UTF-8");
    }
    if let Ok(term) = std::env::var("TERM") {
        cmd.env("TERM", term);
    } else {
        cmd.env("TERM", "xterm-256color");
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn tmux attach: {e}"))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("clone PTY reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("take PTY writer: {e}"))?;
    let writer = std::sync::Arc::new(std::sync::Mutex::new(writer));

    // MasterPty is `Send + !Sync`, so holding it across an await would
    // make the upgrade future non-Send (axum::on_upgrade requires
    // Send). Hand the master to a dedicated blocking task that owns
    // it; main loop sends resize requests via a channel.
    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::channel::<(u16, u16)>(8);
    let master_box = pair.master;
    let resize_task = tokio::task::spawn_blocking(move || {
        while let Some((rows, cols)) = resize_rx.blocking_recv() {
            let _ = master_box.resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    });

    // PTY → WS Binary. Read on a blocking task, push chunks through
    // a bounded mpsc<Vec<u8>>. Buffer 256 entries × ≤16 KiB = ~4 MiB
    // worst case. On overflow we close the WS rather than drop bytes
    // (corrupts xterm state).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let read_task = tokio::task::spawn_blocking(move || {
        // Box<dyn Read + Send> returns from portable-pty; we need the
        // Read trait method `read()` in scope. BufRead's use up top
        // doesn't bring it in — explicit import here is for clarity
        // even though older rustc allowed the call without it.
        use std::io::Read;
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if tx.blocking_send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let (mut sink, mut stream) = futures_split(socket);
    loop {
        tokio::select! {
            chunk = rx.recv() => {
                match chunk {
                    Some(bytes) => {
                        if sink.send(Message::Binary(bytes)).await.is_err() { break; }
                    }
                    None => break, // reader task ended (PTY closed)
                }
            }
            msg = stream.next() => {
                match msg {
                    None => break,
                    Some(Err(_)) => break,
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Binary(bytes))) => {
                        // raw keystrokes / mouse → PTY stdin
                        let writer_arc = writer.clone();
                        let send = tokio::task::spawn_blocking(move || {
                            let mut g = writer_arc.lock().unwrap_or_else(|p| p.into_inner());
                            g.write_all(&bytes).and_then(|_| g.flush())
                        }).await;
                        if matches!(send, Err(_) | Ok(Err(_))) { break; }
                    }
                    Some(Ok(Message::Text(t))) => {
                        // JSON control frame — only {type:"resize",rows,cols} for now.
                        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&t)
                            && obj.get("type").and_then(Value::as_str) == Some("resize")
                            && let (Some(rows), Some(cols)) = (
                                obj.get("rows").and_then(Value::as_u64),
                                obj.get("cols").and_then(Value::as_u64),
                            )
                        {
                            let _ = resize_tx.try_send((rows as u16, cols as u16));
                        }
                    }
                    Some(Ok(_)) => { /* Ping/Pong handled by axum, others ignored */ }
                }
            }
        }
    }
    // Tear down: kill the tmux attach child (detaches the client from
    // the session — multi-attach model preserves the session itself).
    let _ = child.kill();
    let _ = child.wait();
    drop(resize_tx); // signals resize task to exit
    read_task.abort();
    resize_task.abort();
    Ok(())
}

async fn handle_ws_events(
    axum::extract::State(state): axum::extract::State<AppState>,
    ws: axum::extract::WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    // Accept the bearer subprotocol so the upgrade handshake echoes
    // it back per RFC6455 — browsers won't connect without it.
    // RFC6455: the server MUST echo back one of the subprotocols the
    // client offered, else most browsers close the connection
    // immediately. We're authenticating via the subprotocol itself
    // (`Sec-WebSocket-Protocol: bearer.<token>`), so echo that exact
    // string. axum filters out non-matches automatically — wrong
    // tokens are already rejected by the auth middleware upstream,
    // so by the time we're here we trust the token in state.
    let proto = format!("bearer.{}", &state.token);
    ws.protocols([proto]).on_upgrade(move |socket| async move {
        let (mut sink, mut stream) = futures_split(socket);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(64);
        let daemon = state.daemon.clone();
        let patterns = (*state.default_subscribe_patterns).clone();
        let subscribe_task = tokio::spawn(async move {
            if let Err(e) = daemon.subscribe(patterns, tx).await {
                eprintln!("[web-bridge] WS subscribe stream ended: {e}");
            }
        });
        use axum::extract::ws::Message;
        use futures_util::{SinkExt, StreamExt};
        loop {
            tokio::select! {
                event = rx.recv() => {
                    let Some(event) = event else { break };
                    let payload = match serde_json::to_string(&event) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if sink.send(Message::Text(payload)).await.is_err() {
                        break;
                    }
                }
                // Drain client → server so close frames + pings are
                // observed. Without this branch a sleeping mobile
                // browser that drops the TCP connection won't surface
                // until the next failed sink.send — Codex C2.
                msg = stream.next() => {
                    match msg {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                        Some(Ok(_)) => { /* ignore client-pushed text/binary/ping */ }
                    }
                }
            }
        }
        subscribe_task.abort();
    })
}

fn futures_split(
    socket: axum::extract::ws::WebSocket,
) -> (
    futures_util::stream::SplitSink<axum::extract::ws::WebSocket, axum::extract::ws::Message>,
    futures_util::stream::SplitStream<axum::extract::ws::WebSocket>,
) {
    use futures_util::StreamExt;
    socket.split()
}

#[derive(Debug)]
enum AppError {
    Daemon(daemon_client::DaemonError),
    /// Anything outside the daemon path — tmux shell-outs, internal
    /// task join failures, etc. Carries `(code, message)` directly so
    /// `IntoResponse` can route to the right HTTP status.
    Custom {
        code: String,
        message: String,
    },
}

impl AppError {
    fn custom(code: &str, message: &str) -> Self {
        AppError::Custom {
            code: code.to_string(),
            message: message.to_string(),
        }
    }
}

impl From<daemon_client::DaemonError> for AppError {
    fn from(e: daemon_client::DaemonError) -> Self {
        AppError::Daemon(e)
    }
}

impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        let (status, code, message) = match self {
            AppError::Daemon(daemon_client::DaemonError::Io(e)) => {
                (StatusCode::BAD_GATEWAY, "io".to_string(), e.to_string())
            }
            AppError::Daemon(daemon_client::DaemonError::Serde(e)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "serde".to_string(),
                e.to_string(),
            ),
            AppError::Daemon(daemon_client::DaemonError::Closed) => (
                StatusCode::BAD_GATEWAY,
                "closed".to_string(),
                "daemon closed connection".to_string(),
            ),
            AppError::Daemon(daemon_client::DaemonError::Daemon { code, message }) => {
                // no_gui is the most common expected daemon error
                // (the UI surfaces it as a banner, not a 5xx), so
                // map it to 503 to make that distinguishable.
                let status = if code == "no_gui" {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::BAD_GATEWAY
                };
                (status, code, message)
            }
            AppError::Custom { code, message } => {
                // Map known codes to specific status. push_disabled =
                // 503 (Service Unavailable — feature exists but not
                // configured); tmux_error = 502 (upstream tmux
                // failed); anything else falls through to 500.
                let status = if code == "tmux_error" {
                    StatusCode::BAD_GATEWAY
                } else if code == "push_disabled" {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                (status, code, message)
            }
        };
        (
            status,
            axum::Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response()
    }
}

/// Bearer-token check. Accepts an `Authorization: Bearer <token>`
/// header. Query-string tokens (`?token=`) are NEVER accepted (leak
/// path via referrer / history / proxy logs). Constant-time compare
/// to avoid timing side channels.
fn bearer_ok(headers: &axum::http::HeaderMap, expected: &str) -> bool {
    let Some(value) = headers.get("authorization") else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.as_bytes(), expected.as_bytes())
}

/// WebSocket auth via `Sec-WebSocket-Protocol: bearer.<token>`.
/// Browsers send the subprotocol list comma-separated; we check that
/// at least one matches `bearer.<expected>`.
fn ws_subprotocol_ok(headers: &axum::http::HeaderMap, expected: &str) -> bool {
    let Some(value) = headers.get("sec-websocket-protocol") else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let expected_proto = format!("bearer.{expected}");
    for proto in value.split(',') {
        let proto = proto.trim();
        if constant_time_eq(proto.as_bytes(), expected_proto.as_bytes()) {
            return true;
        }
    }
    false
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn bearer_ok_accepts_matching_header() {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_static("Bearer secrettoken"),
        );
        assert!(bearer_ok(&h, "secrettoken"));
    }

    #[test]
    fn bearer_ok_rejects_missing_header() {
        let h = HeaderMap::new();
        assert!(!bearer_ok(&h, "anything"));
    }

    #[test]
    fn bearer_ok_rejects_wrong_token() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer wrong"));
        assert!(!bearer_ok(&h, "right"));
    }

    #[test]
    fn bearer_ok_rejects_non_bearer_scheme() {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert!(!bearer_ok(&h, "anything"));
    }

    #[test]
    fn constant_time_eq_matches_basic_cases() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn ws_subprotocol_ok_accepts_bearer_prefix() {
        let mut h = HeaderMap::new();
        h.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("bearer.tok123"),
        );
        assert!(ws_subprotocol_ok(&h, "tok123"));
    }

    #[test]
    fn ws_subprotocol_ok_rejects_naked_bearer() {
        let mut h = HeaderMap::new();
        h.insert("sec-websocket-protocol", HeaderValue::from_static("bearer"));
        assert!(!ws_subprotocol_ok(&h, "tok123"));
    }

    #[test]
    fn ws_subprotocol_ok_accepts_first_of_list() {
        let mut h = HeaderMap::new();
        h.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("bearer.tok123, chat.v1"),
        );
        assert!(ws_subprotocol_ok(&h, "tok123"));
    }

    #[test]
    fn ws_subprotocol_ok_rejects_unrelated_protocols() {
        let mut h = HeaderMap::new();
        h.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("chat.v1, json.v2"),
        );
        assert!(!ws_subprotocol_ok(&h, "tok123"));
    }

    #[test]
    fn validate_token_env_accepts_min_length() {
        let t = "x".repeat(TOKEN_MIN_LEN);
        let got = validate_token_env(Some(&t)).expect("≥min length must pass");
        assert_eq!(got, t);
    }

    #[test]
    fn validate_token_env_rejects_short() {
        let t = "x".repeat(TOKEN_MIN_LEN - 1);
        let err = validate_token_env(Some(&t)).expect_err("short must fail");
        assert!(err.contains("too short"));
    }

    #[test]
    fn validate_token_env_rejects_missing() {
        let err = validate_token_env(None).expect_err("missing must fail");
        assert!(err.contains("is not set"));
    }
}
