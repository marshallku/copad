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

mod daemon_client;

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
    default_subscribe_patterns: Arc<Vec<String>>,
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

    let state = AppState {
        daemon: DaemonClient::new(socket_path),
        token: Arc::new(token.to_string()),
        default_subscribe_patterns: Arc::new(vec![
            "presence.*".to_string(),
            "claude.*".to_string(),
            "discord.send_message.*".to_string(),
            "notify.show.*".to_string(),
        ]),
    };

    let auth_state = state.clone();
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
        .route("/ws/events", get(handle_ws_events))
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: Next| {
                let token = auth_state.token.clone();
                async move {
                    let path = req.uri().path();
                    // SPA root + health are unauthenticated so the
                    // dashboard HTML loads before JS injects the token
                    // into the Authorization header on subsequent
                    // /api/* / /ws/* calls.
                    let public = path == "/" || path == "/healthz";
                    let upgrades = path.starts_with("/ws/");
                    if public {
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
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("[web-bridge] listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
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
struct AppError(daemon_client::DaemonError);

impl From<daemon_client::DaemonError> for AppError {
    fn from(e: daemon_client::DaemonError) -> Self {
        AppError(e)
    }
}

impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        let (status, code, message) = match &self.0 {
            daemon_client::DaemonError::Io(e) => {
                (StatusCode::BAD_GATEWAY, "io".to_string(), e.to_string())
            }
            daemon_client::DaemonError::Serde(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "serde".to_string(),
                e.to_string(),
            ),
            daemon_client::DaemonError::Closed => (
                StatusCode::BAD_GATEWAY,
                "closed".to_string(),
                "daemon closed connection".to_string(),
            ),
            daemon_client::DaemonError::Daemon { code, message } => {
                // no_gui is the most common expected daemon error
                // (the UI surfaces it as a banner, not a 5xx), so
                // map it to 503 to make that distinguishable.
                let status = if code == "no_gui" {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::BAD_GATEWAY
                };
                (status, code.clone(), message.clone())
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
