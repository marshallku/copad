//! First-party copad plugin: HTTP+WebSocket broker over copadd's
//! socket surface. Slice 3.0 of the remote-harness effort.
//!
//! Lifecycle: this binary IS a copad service plugin. The supervisor
//! spawns it with stdio piped and expects an `initialize` reply, the
//! same handshake `plugins/discord/src/main.rs` runs. Once that
//! handshake completes, the stdio side stays idle and the real work
//! happens in a tokio runtime that owns an axum HTTP+WS listener.
//!
//! Daemon access: the supervisor injects `COPAD_SOCKET` into the
//! child env (see `copad-daemon/src/service_supervisor.rs` —
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
//! from `COPAD_WEB_BRIDGE_TOKEN` env and must be ≥32 chars; if
//! missing/short the plugin exits before binding.

mod agents;
mod cockpit;
mod comux;
mod daemon_client;
mod font;
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
    let token = match validate_token_env(std::env::var("COPAD_WEB_BRIDGE_TOKEN").ok().as_deref()) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("[web-bridge] {msg}");
            return ExitCode::from(2);
        }
    };

    let bind_addr =
        std::env::var("COPAD_WEB_BRIDGE_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());

    let socket_path = match std::env::var("COPAD_SOCKET") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!(
                "[web-bridge] COPAD_SOCKET env is not set. The supervisor should \
                 inject this; without it the bridge cannot reach the daemon. \
                 Falling back to /tmp/copad.sock for debugging."
            );
            "/tmp/copad.sock".to_string()
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
            "COPAD_WEB_BRIDGE_TOKEN is too short ({} chars; need ≥{TOKEN_MIN_LEN}). Refusing to start.",
            short.len()
        )),
        None => Err(format!(
            "COPAD_WEB_BRIDGE_TOKEN is not set. \
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
    /// The terminal font, resolved + validated once at startup. `None` when the
    /// configured family isn't installed (or the override failed validation) — the
    /// endpoint then 404s and the PWA keeps its fallback stack, which is exactly
    /// today's behaviour, so a missing font degrades rather than breaks.
    font: Option<Arc<font::Font>>,
    /// A font carrying the icon glyphs the terminal font lacks.
    ///
    /// The desktop renders devicons and powerline separators because the configured family does
    /// not have them and the SYSTEM falls back to a Nerd Font. A phone has no such chain — it
    /// gets exactly the files this server sends — so serving only the terminal font rendered
    /// every one of those codepoints as tofu.
    symbol_font: Option<Arc<font::Font>>,
    default_subscribe_patterns: Arc<Vec<String>>,
    /// In-process 2 s TTL cache for `/api/tmux/panes`. Multiple
    /// dashboard tabs hitting refresh shouldn't fan out as N tmux
    /// shell-outs per click; once the snapshot is built we serve it
    /// from memory until the TTL expires.
    tmux_cache: Arc<tokio::sync::Mutex<Option<TmuxCacheEntry>>>,
    /// In-process TTL cache for `/api/board`, and — unlike `tmux_cache` — the plugin's
    /// single-flight guard for comux reads. The board handler holds this lock ACROSS the
    /// fetch, so however many phones are polling there is at most one `comux` child alive at
    /// a time — the handler's two reads are awaited sequentially for the same reason. That
    /// bound is the point: comux's client reads its socket with no deadline, so a wedged
    /// server turns every concurrent poll into another stuck child.
    board_cache: Arc<tokio::sync::Mutex<Option<TmuxCacheEntry>>>,
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
/// for IPv4 or `::1` for IPv6).
/// Returns false for unparseable bind strings (fail-closed).
///
/// This used to gate the Tailscale identity-header auth path, which no longer exists. It now
/// drives a startup warning: on a loopback bind the only way to this service is from this
/// machine or through a proxy the operator put there, while on a routable one the bearer token
/// is the single thing between the LAN and a terminal.
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
            "[web-bridge] COPAD_WEB_BRIDGE_VAPID_PRIVATE/PUBLIC not set — push notifications disabled"
        );
    }
    // Resolve the terminal font once, at startup, from the SAME config the desktop
    // terminal uses — so the phone renders in the user's font rather than a guess.
    // Failure is non-fatal: the PWA just keeps its fallback stack (today's behaviour).
    // The glyphs the terminal font does not have. Family is configurable; the default is the
    // Nerd Font the desktop already falls back to. Its own override var, deliberately not
    // `COPAD_WEB_BRIDGE_FONT` — that pins one exact file, and sharing it would serve the same
    // glyph-incomplete font from both endpoints.
    let symbol_font = (|| {
        let family = std::env::var("COPAD_WEB_BRIDGE_SYMBOL_FAMILY")
            .unwrap_or_else(|_| "JetBrainsMonoNerdFont".to_string());
        if family.is_empty() {
            return None;
        }
        let path = font::resolve_with_override(&family, "COPAD_WEB_BRIDGE_SYMBOL_FONT")?;
        match font::load(&path) {
            Ok(f) => {
                eprintln!(
                    "[web-bridge] symbol font: {} ({} KB, {})",
                    f.path.display(),
                    f.bytes.len() / 1024,
                    f.content_type
                );
                Some(Arc::new(f))
            }
            Err(e) => {
                eprintln!("[web-bridge] symbol font rejected: {e}");
                None
            }
        }
    })();
    if symbol_font.is_none() {
        eprintln!(
            "[web-bridge] no symbol font resolved — icon glyphs will render as tofu on the \
             phone. Set COPAD_WEB_BRIDGE_SYMBOL_FAMILY to a Nerd Font you have installed."
        );
    }

    let font = (|| {
        let family = copad_core::config::CopadConfig::load()
            .map(|c| c.terminal.font_family)
            .unwrap_or_else(|_| String::new());
        if family.is_empty() {
            return None;
        }
        let path = font::resolve(&family)?;
        match font::load(&path) {
            Ok(f) => {
                eprintln!(
                    "[web-bridge] terminal font: {} ({} KB, {})",
                    f.path.display(),
                    f.bytes.len() / 1024,
                    f.content_type
                );
                Some(Arc::new(f))
            }
            Err(e) => {
                eprintln!("[web-bridge] terminal font rejected: {e}");
                None
            }
        }
    })();
    if font.is_none() {
        eprintln!(
            "[web-bridge] no terminal font served — phone glyphs will fall back \
             (set COPAD_WEB_BRIDGE_FONT to an absolute font path to override)"
        );
    }

    let state = AppState {
        daemon: DaemonClient::new(socket_path),
        token: Arc::new(token.to_string()),
        font,
        symbol_font,
        tmux_cache: Arc::new(tokio::sync::Mutex::new(None)),
        board_cache: Arc::new(tokio::sync::Mutex::new(None)),
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
        // Phase 24.6 — orchestration cockpit (pilot goal queue + csd + tmx).
        .route("/api/pilot/status", get(handle_pilot_status))
        .route("/api/pilot/goals", post(handle_pilot_add))
        .route("/api/pilot/goals/:id/answer", post(handle_pilot_answer))
        .route("/api/pilot/goals/:id/approve", post(handle_pilot_approve))
        .route("/api/pilot/goals/:id/cancel", post(handle_pilot_cancel))
        .route("/api/board", get(handle_board))
        .route("/api/board/attach-preflight", get(handle_board_preflight))
        .route("/api/board/jump", post(handle_board_jump))
        .route("/ws/board/attach", get(handle_ws_board_attach))
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
        .route("/app.js", get(handle_app_js))
        .route("/app.css", get(handle_app_css))
        .route("/font/terminal", get(handle_font))
        .route("/font/symbols", get(handle_symbol_font))
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
                        || path == "/icon.svg"
                        // The shell's script + stylesheet. A `<script src>` / `<link>` fetch
                        // sends no Authorization header, so authing these would 401 and leave
                        // a blank page; neither carries a secret.
                        || path == "/app.js"
                        || path == "/app.css"
                        // A CSS @font-face fetch sends no Authorization header, so an
                        // authed font URL would 401 and leave the glyphs broken — the
                        // very bug it exists to fix. Public is therefore forced, which
                        // is why font::load() validates magic + size + regular-file
                        // before a byte is served.
                        || path == "/font/terminal"
                        // Same reason: a CSS @font-face fetch carries no Authorization header.
                        || path == "/font/symbols";
                    let upgrades = path.starts_with("/ws/");
                    if public {
                        return next.run(req).await;
                    }
                    // There is deliberately NO identity-header auth path here.
                    //
                    // `tailscale serve` attaches `Tailscale-User-Login` to every request it
                    // forwards, and trusting it let a browser on any admitted device reach this
                    // service with no token. WebSocket upgrades are not subject to CORS and send
                    // no preflight, so ANY page that device visited could have opened
                    // `/ws/board/attach` — a terminal that accepts raw keystrokes — and had
                    // serve supply the identity for it. The bearer path has no equivalent hole:
                    // a cross-site page cannot produce `Sec-WebSocket-Protocol: bearer.<token>`
                    // without the token. Closing the gap would have meant an origin policy on
                    // top of an allowlist, two competing auth decisions, to save typing a token
                    // once per session. See docs/mobile-access.md.
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
        let pilot_state = state.clone();
        tokio::spawn(async move { pilot_push_loop(pilot_state).await });
    }

    let listener = tokio::net::TcpListener::bind(bind).await?;
    if !bind_is_loopback(bind) {
        eprintln!(
            "[web-bridge] WARNING: bound to {bind}, which is not loopback — the bearer token is \
             the only thing protecting a terminal from anyone who can reach this address. The \
             supported setup is a loopback bind behind `tailscale serve`."
        );
    }
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
            let url = if entry.tmux_target.is_empty() {
                "/".to_string()
            } else {
                format!("/#attention/{}", urlenc(&entry.tmux_target))
            };
            let title = if entry.title.is_empty() {
                "copad".to_string()
            } else {
                entry.title.clone()
            };
            let body = if entry.body.is_empty() {
                entry.kind.clone()
            } else {
                entry.body.clone()
            };
            let tag = format!("copad-{}", entry.kind);
            let payload = push::PushPayload {
                title: &title,
                body: &body,
                tag: &tag,
                kind: &entry.kind,
                url: &url,
            };
            fan_out(
                &cfg,
                &subs_snapshot,
                Some(&entry.kind),
                &payload,
                &mut pruned_ids,
            )
            .await;
        }
        prune_subscriptions(&state, &pruned_ids).await;
    }
}

/// Send one payload to every subscription, optionally gated by the
/// subscriber's kind filter. `filter_kind = None` force-delivers to all
/// subscriptions regardless of their filter — used for pilot gate
/// notifications, the highest-signal event copad emits (a blocked
/// autonomous loop waiting on a human). Terminal endpoints (410 Gone /
/// 404) are appended to `pruned` for the caller to remove in one pass.
async fn fan_out(
    cfg: &push::PushConfig,
    subs: &[push::Subscription],
    filter_kind: Option<&str>,
    payload: &push::PushPayload<'_>,
    pruned: &mut Vec<String>,
) {
    for sub in subs {
        if let Some(kind) = filter_kind
            && !sub.matches_kind(kind)
        {
            continue;
        }
        if let Err(e) = push::send_to(cfg, sub, payload).await {
            if push::is_terminal_error(&e) {
                pruned.push(sub.id.clone());
            } else {
                eprintln!("[web-bridge] push send error (kept): {e:?}");
            }
        }
    }
}

/// Drop subscriptions whose endpoints returned a terminal error and
/// persist the trimmed list. No-op on an empty prune set. `retain`-by-id
/// is idempotent, so two loops pruning the same id concurrently is safe.
async fn prune_subscriptions(state: &AppState, pruned_ids: &[String]) {
    if pruned_ids.is_empty() {
        return;
    }
    let mut subs = state.push_subs.lock().await;
    subs.retain(|s| !pruned_ids.contains(&s.id));
    if let Err(e) = push::save_subscriptions(&subs) {
        eprintln!("[web-bridge] push save_subscriptions failed: {e}");
    }
}

/// Poll task: surfaces pilot goals that hit a gate (`awaiting_gate` —
/// waiting on a human answer/approval) as Web Push, so a goal that
/// blocks at 2am pings the phone instead of silently stalling the queue.
///
/// Polls `pilot.status` rather than subscribing to the `pilot.goal_blocked`
/// bus event: events are ephemeral, so a consumer that's down when the
/// gate opens loses it; a poll re-reads the live queue and is self-healing
/// across a web-bridge restart. No-op when VAPID is unconfigured or the
/// pilot plugin isn't running (the RPC just errors and we retry).
async fn pilot_push_loop(state: AppState) {
    let cfg = match state.push_config.as_ref().as_ref() {
        Some(c) => c.clone(),
        None => return,
    };
    // No boot pre-seed (unlike push_loop): the pilot queue is sequential,
    // so at most one gate is open at any time. Re-surfacing that single
    // open gate after a web-bridge restart is a desired reminder, not the
    // stale-backlog dump push_loop guards against. The in-memory `seen`
    // set still prevents re-pushing the same gate on every 5s tick.
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let status = match state.daemon.rpc("pilot.status", json!({})).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[web-bridge] pilot_push_loop: pilot.status failed: {e}");
                continue;
            }
        };
        let pushes = pilot_gate_pushes(&status, &mut seen);
        if pushes.is_empty() {
            continue;
        }
        let subs_snapshot = state.push_subs.lock().await.clone();
        if subs_snapshot.is_empty() {
            continue;
        }
        let mut pruned_ids: Vec<String> = Vec::new();
        for p in &pushes {
            let payload = push::PushPayload {
                title: &p.title,
                body: &p.body,
                tag: &p.tag,
                kind: "pilot-blocked",
                url: &p.url,
            };
            fan_out(&cfg, &subs_snapshot, None, &payload, &mut pruned_ids).await;
        }
        prune_subscriptions(&state, &pruned_ids).await;
    }
}

/// One Web Push to emit for a freshly-blocked pilot goal. Owned (not
/// borrowed) so `pilot_gate_pushes` stays a pure, unit-testable mapping
/// from a `pilot.status` value to the notifications it implies.
struct GatePush {
    title: String,
    body: String,
    tag: String,
    url: String,
}

/// Scan a `pilot.status` response for goals at a gate, returning one
/// `GatePush` per gate not already in `seen`. The dedup fingerprint
/// covers the goal id plus the *entire* gate object (kind + prompt +
/// options + plan_file), so a goal that re-blocks with a different
/// question or option set notifies again, while a still-open gate seen
/// on the previous tick stays quiet. Marks every gate seen (even with no
/// subscribers) so dedup state advances regardless of delivery.
fn pilot_gate_pushes(status: &Value, seen: &mut std::collections::HashSet<u64>) -> Vec<GatePush> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut out = Vec::new();
    let Some(goals) = status.get("goals").and_then(|g| g.as_array()) else {
        return out;
    };
    for g in goals {
        if g.get("status").and_then(|s| s.as_str()) != Some("awaiting_gate") {
            continue;
        }
        let Some(gate) = g.get("gate").filter(|v| !v.is_null()) else {
            continue;
        };
        let id = g.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let mut h = DefaultHasher::new();
        id.hash(&mut h);
        gate.to_string().hash(&mut h);
        if !seen.insert(h.finish()) {
            continue;
        }
        let gkind = gate.get("kind").and_then(|v| v.as_str()).unwrap_or("gate");
        let prompt = gate
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(truncate_prompt)
            .unwrap_or_else(|| match gkind {
                "plan" => "plan ready for approval".to_string(),
                "answer" => "a question is waiting".to_string(),
                _ => "input needed".to_string(),
            });
        out.push(GatePush {
            title: "Pilot needs you".to_string(),
            body: format!("{gkind} gate · {prompt}"),
            tag: format!("copad-pilot-{id}"),
            url: "/".to_string(),
        });
    }
    out
}

/// Clamp a gate prompt to a notification-sized string on a char boundary
/// (push bodies render at most a couple of lines on a phone anyway).
fn truncate_prompt(s: &str) -> String {
    const MAX: usize = 120;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX).collect();
    out.push('…');
    out
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

/// Serve one of the three compile-time files that make up the PWA shell.
///
/// **Caching.** `no-cache` + a strong content ETag, the same shape `/font/terminal` uses and
/// for the same reason: the page, its script and its stylesheet are separate requests at
/// stable URLs with no build hash (the HTML is `include_str!`'d and cannot carry one), so
/// anything with a lifetime could pin a stale `app.js` against a new `index.html` — a mismatch
/// whose only symptom is a broken page. `no-cache` stores the body but always revalidates, and
/// the ETag is what makes that revalidation cost a 304 instead of re-sending ~70 KB to a phone
/// on cellular every single load. Without a validator `no-cache` is just "download it again".
///
/// Not `private`: these bytes are identical for every viewer and carry nothing user-specific.
///
/// The ETag is content-derived, so it changes exactly when the asset does — there is no build
/// step here to supply a version, and mtime/size would not survive `include_str!` at all.
fn shell_asset(
    headers: &axum::http::HeaderMap,
    content_type: &'static str,
    body: &'static str,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    use sha2::Digest;
    let etag = format!("\"{:x}\"", sha2::Sha256::digest(body.as_bytes()));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        body,
    )
        .into_response()
}

async fn handle_index(headers: axum::http::HeaderMap) -> axum::response::Response {
    shell_asset(
        &headers,
        "text/html; charset=utf-8",
        include_str!("../static/index.html"),
    )
}

/// `GET /app.js` — the PWA's script, loaded as a module.
///
/// Public, like `/sw.js` and `/icon.svg`: a `<script src>` sends no Authorization header, so an
/// authed URL would 401 and leave a blank page — the same reason `/font/terminal` is public.
/// It carries no secrets; the bearer token is typed by the user and lives in `sessionStorage`.
/// The MIME type is load-bearing for a module script: a browser refuses to execute one served
/// as anything but JavaScript, and it does so silently apart from a console line.
async fn handle_app_js(headers: axum::http::HeaderMap) -> axum::response::Response {
    shell_asset(
        &headers,
        "text/javascript; charset=utf-8",
        include_str!("../static/app.js"),
    )
}

/// `GET /app.css` — the PWA's stylesheet. Public for the same reason as `/app.js`.
async fn handle_app_css(headers: axum::http::HeaderMap) -> axum::response::Response {
    shell_asset(
        &headers,
        "text/css; charset=utf-8",
        include_str!("../static/app.css"),
    )
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
/// `GET /api/whoami` — how this request authenticated.
///
/// Always `bearer`, because that is now the only way to get here: the route is behind the
/// auth middleware, so reaching this function already proves a valid token. It must NOT infer
/// a mode from `Tailscale-User-Login`; reporting an identity the middleware did not act on is
/// how a client ends up believing it is authenticated by a header that authenticates nothing.
async fn handle_whoami() -> axum::Json<Value> {
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

/// `GET /font/terminal` — the workstation's terminal font, for the PWA's @font-face.
///
/// Public (see the allowlist). `Content-Type` is sniffed from the file's magic rather
/// than assumed from a path, so the declaration always matches the bytes.
///
/// Caching is `private, no-cache` + a strong content-hash ETag, NOT
/// `immutable`/`max-age`: the URL is stable (the HTML is include_str!'d and can't carry
/// a build hash), so a long max-age would pin a stale font for its whole lifetime with
/// the ETag never consulted. `no-cache` still stores the body — it just revalidates —
/// so the 2.4 MB body transfers once and later loads cost a tiny 304. `private` keeps
/// shared proxies from storing the user's font bytes.
async fn handle_font(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    serve_font(state.font.clone(), "terminal", &headers)
}

/// `GET /font/symbols` — the icon glyphs the terminal font lacks. Public, same as
/// `/font/terminal` and for the same reason.
async fn handle_symbol_font(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    serve_font(state.symbol_font.clone(), "symbol", &headers)
}

fn serve_font(
    font: Option<Arc<font::Font>>,
    what: &str,
    headers: &axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    let Some(font) = font else {
        return (StatusCode::NOT_FOUND, format!("no {what} font resolved\n")).into_response();
    };

    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == font.etag))
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, font.etag.clone()),
                (header::CACHE_CONTROL, "private, no-cache".to_string()),
            ],
        )
            .into_response();
    }

    (
        [
            (header::CONTENT_TYPE, font.content_type.to_string()),
            (header::ETAG, font.etag.clone()),
            (header::CACHE_CONTROL, "private, no-cache".to_string()),
        ],
        font.bytes.clone(),
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
            title: "copad",
            body: "test push from web-bridge",
            tag: "copad-test",
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
    // daemon expects `id` (resolve_terminal at copad-linux/src/socket.rs:1225);
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

// --- Phase 24.6: orchestration cockpit (pilot goal queue) ---

/// `GET /api/pilot/status` — the goal queue (pilot.status, via daemon RPC)
/// aggregated with the live `csd ps` driven sessions and the `tmx agents`
/// observation snapshot. The two side-cars are best-effort (see
/// `cockpit::aggregate`); a `no_gui`-style daemon error on pilot.status
/// itself still surfaces as a 5xx so the UI shows "pilot not running".
async fn handle_pilot_status(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<axum::Json<Value>, AppError> {
    let pilot = state.daemon.rpc("pilot.status", json!({})).await?;
    let csd = tokio::task::spawn_blocking(cockpit::read_csd_ps)
        .await
        .map_err(|e| AppError::custom("internal", &format!("csd ps task: {e}")))?;
    let tmx = tokio::task::spawn_blocking(|| {
        agents::read_snapshot().and_then(|s| serde_json::to_value(s).map_err(|e| e.to_string()))
    })
    .await
    .map_err(|e| AppError::custom("internal", &format!("tmx agents task: {e}")))?;
    let mut out = cockpit::aggregate(pilot, csd, tmx);
    // Configured projects power the cockpit's cwd picker (so a goal can be
    // targeted at a project without hand-typing the path). Best-effort.
    let projects = state
        .daemon
        .rpc("project.list", json!({}))
        .await
        .ok()
        .and_then(|v| v.get("projects").cloned())
        .unwrap_or_else(|| Value::Array(vec![]));
    out["projects"] = projects;
    Ok(axum::Json(out))
}

#[derive(Deserialize)]
struct PilotAddBody {
    cwd: String,
    instruction: String,
    posture: Option<String>,
}

/// `POST /api/pilot/goals` — enqueue a goal from the cockpit.
async fn handle_pilot_add(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Json(body): axum::Json<PilotAddBody>,
) -> Result<axum::Json<Value>, AppError> {
    let mut params = json!({ "cwd": body.cwd, "instruction": body.instruction });
    if let Some(posture) = body.posture {
        params["posture"] = json!(posture);
    }
    Ok(axum::Json(state.daemon.rpc("pilot.add", params).await?))
}

#[derive(Deserialize)]
struct PilotAnswerBody {
    text: String,
}

/// `POST /api/pilot/goals/:id/answer` — answer a clarifying-question gate.
async fn handle_pilot_answer(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<PilotAnswerBody>,
) -> Result<axum::Json<Value>, AppError> {
    let v = state
        .daemon
        .rpc("pilot.answer", json!({ "id": id, "text": body.text }))
        .await?;
    Ok(axum::Json(v))
}

#[derive(Deserialize)]
struct PilotApproveBody {
    option: Option<u32>,
}

/// `POST /api/pilot/goals/:id/approve` — approve a plan / permission / trust gate.
async fn handle_pilot_approve(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<PilotApproveBody>,
) -> Result<axum::Json<Value>, AppError> {
    let mut params = json!({ "id": id });
    if let Some(option) = body.option {
        params["option"] = json!(option);
    }
    Ok(axum::Json(state.daemon.rpc("pilot.approve", params).await?))
}

/// `POST /api/pilot/goals/:id/cancel` — cancel a goal and kill its session.
async fn handle_pilot_cancel(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::Json<Value>, AppError> {
    let v = state
        .daemon
        .rpc("pilot.cancel", json!({ "id": id }))
        .await?;
    Ok(axum::Json(v))
}

/// `GET /api/board` — the mobile board's fleet read: every comux session and every agent
/// pane across all of them.
///
/// This is the endpoint that exists because `/api/tmux/panes` answers `{"panes":[]}` on a
/// machine running 10 comux sessions and 27 agents.
///
/// **Partial failure is representable on purpose.** `sessions` and `agents` are two separate
/// `comux` invocations, so one can fail while the other succeeds. A collection is `null` when
/// it could not be READ and `[]` when it was read and was empty — collapsing those would
/// render "we could not look" as "your fleet is gone". Whatever succeeded is still returned,
/// and every failure is named in `errors`.
///
/// **The counts can disagree.** The two calls are sampled at slightly different instants, so
/// a session's `agents` count need not match how many rows `agents` carries for it during a
/// change. `fetched_at_ms` is when the composition finished; it says nothing about how fresh
/// comux's own cached classification is, which is why `status_is_inferred` is always set.
async fn handle_board(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<axum::Json<Value>, AppError> {
    const TTL: std::time::Duration = std::time::Duration::from_secs(2);
    // Held across the fetch — see `AppState::board_cache`.
    let mut cache = state.board_cache.lock().await;
    if let Some(entry) = cache.as_ref()
        && entry.at.elapsed() < TTL
    {
        return Ok(axum::Json(entry.value.clone()));
    }

    // SEQUENTIAL, not `join!`. Running the two reads concurrently would put two children in
    // flight, and against a wedged comux server both sit there until their deadlines — which
    // would make the single-flight guard above a half-truth. Both results are still kept, so
    // partial failure is unaffected; the only cost is two serial socket round trips.
    let sessions = comux::list_sessions().await;
    let agents = comux::list_agents().await;
    let mut errors = Vec::new();
    let mut take = |what: &str, r: Result<Value, comux::ComuxError>| match r {
        Ok(v) => v,
        Err(e) => {
            errors.push(json!({ "what": what, "code": e.code(), "message": e.to_string() }));
            Value::Null
        }
    };
    let sessions = take("sessions", sessions.map(|v| json!(v)));
    let agents = take("agents", agents.map(|v| json!(v)));

    let snapshot = json!({
        "fetched_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "sessions": sessions,
        "agents": agents,
        // A flag, not a nicety: comux infers `status` from screen text for everything that
        // is not Claude, `idle` doubles as "unresolved", and `for_secs` is time holding that
        // inferred label — not task duration. A UI that prints these as fact is lying.
        "status_is_inferred": true,
        "errors": errors,
    });
    *cache = Some(TmuxCacheEntry {
        at: std::time::Instant::now(),
        value: snapshot.clone(),
    });
    Ok(axum::Json(snapshot))
}

/// `GET /api/board/attach-preflight` — can the phone open a terminal right now?
///
/// This exists because a WebSocket upgrade cannot report WHY it failed: a browser that gets a
/// 503 on `/ws/board/attach` sees only a closed socket, and the UI can say nothing better than
/// "disconnected". So the client asks over plain HTTP first and gets an answer it can show.
async fn handle_board_preflight(
    axum::extract::State(_state): axum::extract::State<AppState>,
) -> axum::Json<Value> {
    match comux::server_info().await {
        Ok(info) if info.running => axum::Json(json!({ "ok": true, "socket": info.socket })),
        Ok(info) => axum::Json(json!({
            "ok": false,
            "code": "no_server",
            "message": format!("no comux server is running at {}", info.socket),
        })),
        Err(e) => axum::Json(json!({ "ok": false, "code": e.code(), "message": e.to_string() })),
    }
}

#[derive(Deserialize)]
struct BoardJump {
    token: String,
}

/// `POST /api/board/jump` — move the mux's focus to one pane, addressed by its token.
///
/// This is what makes a board row a destination rather than a link to "whatever comux happens to
/// be showing". It MOVES THE DESKTOP'S VIEW as well, because comux composes one frame for every
/// attached client — and that is the intended behaviour, not a leak: the person tapping the phone
/// is the person sitting at the desk. The alternative (a per-client view) is the per-pane semantic
/// grid, decisions #65/#66.
///
/// A refusal is reported rather than smoothed over. Pane tokens are qualified by server
/// incarnation, so every token the phone is holding goes stale on `comux server restart`; showing
/// the focused session instead of saying so would reproduce exactly the bug this endpoint fixes.
async fn handle_board_jump(
    axum::extract::State(_state): axum::extract::State<AppState>,
    axum::Json(body): axum::Json<BoardJump>,
) -> Result<axum::Json<Value>, AppError> {
    if !comux::is_pane_token(&body.token) {
        return Err(AppError::custom("bad_token", "not a pane token"));
    }
    match comux::jump(&body.token).await {
        Ok(()) => Ok(axum::Json(json!({ "ok": true, "token": body.token }))),
        // A pane that is not there is the CLIENT's problem — a token it has been holding since
        // before the last `comux server restart` — not a server fault, and logging it as a 500
        // would bury the one failure this endpoint expects to see.
        Err(e) if e.to_string().contains("unknown pane") => {
            Err(AppError::custom("unknown_pane", &e.to_string()))
        }
        Err(e) => Err(AppError::custom(e.code(), &e.to_string())),
    }
}

/// `WS /ws/board/attach` — a real terminal on the phone: the comux CLIENT itself, run inside a
/// PTY and pumped to xterm.js.
///
/// Running the actual client rather than reimplementing its rendering means the phone sees
/// exactly what the desktop sees — same sidebar, status bar, keybindings — and there is no
/// second renderer to keep in sync.
///
/// Two things are load-bearing:
///
/// * **`--no-spawn`.** Bare `comux` is connect-or-spawn, so with no server running this would
///   birth one parented to copadd, whose environment is scrubbed of the volatile session vars
///   and whose kernel session is a daemon's. That server would then own every pane the user
///   opens afterwards. The flag makes comux refuse instead.
/// * **An explicit socket.** The path comes from `comux doctor` and is passed back in as
///   `COPAD_MUX_SOCK`, so the preflight and the client cannot resolve different servers.
///
/// Known limitation: comux adopts the ATTACHING client's session vars for panes that client
/// creates. copadd's environment has those scrubbed, so a pane created FROM THE PHONE gets no
/// `DISPLAY`/`DBUS_SESSION_BUS_ADDRESS` injected. The desktop client re-applies its own on its
/// next action, so nothing it creates is affected.
async fn handle_ws_board_attach(
    axum::extract::State(state): axum::extract::State<AppState>,
    ws: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let info = match comux::server_info().await {
        Ok(i) if i.running => i,
        Ok(i) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("no comux server is running at {}\n", i.socket),
            )
                .into_response();
        }
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("comux: {e}\n")).into_response();
        }
    };
    let proto = format!("bearer.{}", state.token);
    ws.protocols([proto])
        .on_upgrade(move |socket| async move {
            let mut cmd = portable_pty::CommandBuilder::new("comux");
            cmd.args(["attach", "--no-spawn"]);
            // NOT cleared: portable-pty seeds the builder from this process's environment,
            // which is where PATH/LANG/TMPDIR come from. Only the socket is pinned.
            cmd.env("COPAD_MUX_SOCK", &info.socket);
            if std::env::var("LANG").is_err() && std::env::var("LC_ALL").is_err() {
                cmd.env("LANG", "C.UTF-8");
            }
            if std::env::var("TERM").is_err() {
                cmd.env("TERM", "xterm-256color");
            }
            if let Err(e) = run_pty_attach(socket, cmd).await {
                eprintln!("[web-bridge] board attach ended: {e}");
            }
        })
        .into_response()
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
    // codex_jobs ride along in the snapshot since tmx 1.1 (alive-filtered
    // there) — no local read of `~/.claude/state/codex-companion/`.
    let tmx_snap = agents::read_snapshot().unwrap_or_else(|e| {
        eprintln!("[web-bridge] tmx snapshot unavailable, degrading: {e}");
        agents::TmxSnapshot::default()
    });
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
    // daemon expects `id` (resolve_terminal at copad-linux/src/socket.rs:1225);
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
    let proto = format!("bearer.{}", state.token);
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
    let proto = format!("bearer.{}", state.token);
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
    use portable_pty::CommandBuilder;

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
    // Fallback locale if the parent env had none — guarantees UTF-8.
    if std::env::var("LANG").is_err() && std::env::var("LC_ALL").is_err() {
        cmd.env("LANG", "C.UTF-8");
    }
    if std::env::var("TERM").is_err() {
        cmd.env("TERM", "xterm-256color");
    }
    run_pty_attach(socket, cmd).await
}

/// Own one PTY-backed attach WebSocket: spawn `cmd` on a PTY, pump it both ways, and tear the
/// child down when either side goes.
///
/// Extracted from the tmux attach so the comux one can reuse it — the pump never cared which
/// program it was carrying, only that it speaks a terminal.
///
/// Protocol: PTY output goes out as WS Binary; WS Binary comes back in as keystrokes; a WS Text
/// frame is JSON control, currently only `{type:"resize",rows,cols}`.
async fn run_pty_attach(
    socket: axum::extract::ws::WebSocket,
    cmd: portable_pty::CommandBuilder,
) -> Result<(), String> {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    use portable_pty::{PtySize, native_pty_system};

    /// How long one WebSocket send may take before the attach is considered dead.
    ///
    /// A phone that sleeps or loses signal mid-frame leaves the send pending forever. Without
    /// a bound the whole attach parks: the PTY child stays alive and, because comux sizes its
    /// composed frame to the SMALLEST attached client, the user's desktop terminal stays
    /// clamped to that phone's grid until the TCP stack eventually gives up. Dropping the
    /// attach on a stalled send is what releases both.
    const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn attach child: {e}"))?;
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
                        // Bounded: an unbounded send against a slept phone parks the whole
                        // attach and keeps the desktop clamped to that phone's grid.
                        match tokio::time::timeout(SEND_TIMEOUT, sink.send(Message::Binary(bytes))).await {
                            Err(_) => break,          // stalled peer — drop the attach
                            Ok(Err(_)) => break,      // socket closed
                            Ok(Ok(())) => {}
                        }
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
    // Tear down: kill the child (for tmux this detaches the client and leaves the session;
    // for comux the same — the server and its panes outlive any client).
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
    let proto = format!("bearer.{}", state.token);
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
                } else if code == "bad_token" {
                    StatusCode::BAD_REQUEST
                } else if code == "unknown_pane" {
                    // Gone, not missing: the pane existed and the id the client holds is from a
                    // previous server incarnation.
                    StatusCode::GONE
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

    fn status_with(goals: Value) -> Value {
        json!({ "goals": goals })
    }

    #[test]
    fn pilot_gate_pushes_emits_one_per_open_gate() {
        let mut seen = std::collections::HashSet::new();
        let st = status_with(json!([
            { "id": "g-abc", "status": "awaiting_gate",
              "gate": { "kind": "answer", "prompt": "Which DB?" } },
            { "id": "g-run", "status": "running" },
            { "id": "g-plan", "status": "awaiting_gate",
              "gate": { "kind": "plan", "prompt": null } },
        ]));
        let pushes = pilot_gate_pushes(&st, &mut seen);
        assert_eq!(pushes.len(), 2);
        assert_eq!(pushes[0].tag, "copad-pilot-g-abc");
        assert_eq!(pushes[0].body, "answer gate · Which DB?");
        // null prompt → kind-specific fallback copy.
        assert_eq!(pushes[1].body, "plan gate · plan ready for approval");
    }

    #[test]
    fn pilot_gate_pushes_dedups_unchanged_gate_across_ticks() {
        let mut seen = std::collections::HashSet::new();
        let st = status_with(json!([
            { "id": "g-1", "status": "awaiting_gate",
              "gate": { "kind": "answer", "prompt": "Q1" } },
        ]));
        assert_eq!(pilot_gate_pushes(&st, &mut seen).len(), 1);
        // Same gate on the next poll → no re-push.
        assert_eq!(pilot_gate_pushes(&st, &mut seen).len(), 0);
    }

    #[test]
    fn pilot_gate_pushes_renotifies_on_changed_gate() {
        let mut seen = std::collections::HashSet::new();
        let g1 = status_with(json!([
            { "id": "g-1", "status": "awaiting_gate",
              "gate": { "kind": "answer", "prompt": "Q1" } },
        ]));
        let g2 = status_with(json!([
            { "id": "g-1", "status": "awaiting_gate",
              "gate": { "kind": "answer", "prompt": "Q2" } },
        ]));
        assert_eq!(pilot_gate_pushes(&g1, &mut seen).len(), 1);
        // Same goal, different prompt → a new push.
        assert_eq!(pilot_gate_pushes(&g2, &mut seen).len(), 1);
    }

    #[test]
    fn pilot_gate_pushes_renotifies_on_changed_options() {
        let mut seen = std::collections::HashSet::new();
        let g1 = status_with(json!([
            { "id": "g-1", "status": "awaiting_gate",
              "gate": { "kind": "approve", "prompt": "ok?", "options": ["a", "b"] } },
        ]));
        let g2 = status_with(json!([
            { "id": "g-1", "status": "awaiting_gate",
              "gate": { "kind": "approve", "prompt": "ok?", "options": ["a", "b", "c"] } },
        ]));
        assert_eq!(pilot_gate_pushes(&g1, &mut seen).len(), 1);
        // Same kind + prompt but different options → still a new push.
        assert_eq!(pilot_gate_pushes(&g2, &mut seen).len(), 1);
    }

    #[test]
    fn pilot_gate_pushes_ignores_non_gated_and_empty() {
        let mut seen = std::collections::HashSet::new();
        let none = status_with(json!([
            { "id": "g-1", "status": "running" },
            { "id": "g-2", "status": "done" },
        ]));
        assert_eq!(pilot_gate_pushes(&none, &mut seen).len(), 0);
        // Missing/!array goals → no panic, no pushes.
        assert_eq!(pilot_gate_pushes(&json!({}), &mut seen).len(), 0);
    }

    #[test]
    fn truncate_prompt_clamps_on_char_boundary() {
        let short = "hi";
        assert_eq!(truncate_prompt(short), "hi");
        let long: String = "가".repeat(200); // multi-byte chars
        let got = truncate_prompt(&long);
        assert_eq!(got.chars().count(), 121); // 120 + ellipsis
        assert!(got.ends_with('…'));
    }
}
