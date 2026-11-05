//! Discord Gateway WebSocket client.
//!
//! Per connection: fetch the WSS URL (`GET /gateway/bot`; resumes use the prior READY's
//! `resume_gateway_url`), connect via tungstenite (auth is in IDENTIFY/RESUME, not the handshake),
//! read HELLO (op 10) for `heartbeat_interval`, then send IDENTIFY (op 2) or, with a live
//! `session_id` + `seq`, RESUME (op 6). READY / RESUMED / INVALID_SESSION (op 9, which forces a
//! full IDENTIFY after a small random delay) follow.
//!
//! Steady-state ops: 0 DISPATCH (update `seq`, hand `t`+`d` to `events::from_dispatch`), 1
//! HEARTBEAT (server-prompted, reply at once), 7 RECONNECT (close, RESUME), 9 INVALID_SESSION
//! (`d=true` resumable; `d=false` clears session_id+seq+resume_url), 10 HELLO (ignored after the
//! first frame), 11 HEARTBEAT_ACK (touches `last_ack`).
//!
//! Zombie detection: a heartbeat sent without an ACK before the next send means the connection is
//! dead; close and RESUME (Discord's recommended heuristic).
//!
//! Threading: one thread per connection. Heartbeats are interleaved with frame reads through a TCP
//! read timeout of roughly `heartbeat_interval / 4`, instead of a heartbeat thread contending with
//! the reader for `WebSocket` access.
//!
//! Intents: see `GATEWAY_INTENTS`. Privileged MESSAGE_CONTENT must be enabled on the application's
//! Bot tab; Discord rejects IDENTIFY otherwise (close code 4014) and we surface a clear error.

use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, http::Uri};

use crate::config::Config;
use crate::events::{DiscordEvent, from_dispatch};
use crate::store::TokenStore;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
/// `GUILD_MESSAGES | GUILD_MESSAGE_REACTIONS | DIRECT_MESSAGES | DIRECT_MESSAGE_REACTIONS |
/// MESSAGE_CONTENT`. The privileged `MESSAGE_CONTENT` bit needs Developer Portal -> Bot tab
/// opt-in; without it `content` arrives empty for non-mentions and keyword triggers can't match.
/// Reaction bits are non-privileged but must be present or `MESSAGE_REACTION_ADD` never
/// dispatches.
const GATEWAY_INTENTS: u64 = (1 << 9) | (1 << 10) | (1 << 12) | (1 << 13) | (1 << 15);
/// Recheck cadence when there are no credentials, so a fresh `auth` is picked up without a
/// supervisor restart.
const NO_CREDS_RECHECK: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ResolvedCredentials {
    pub bot_token: String,
    pub source: &'static str, // "env" | "store"
}

/// Env wins (test overrides), store falls back. Discord uses a single token, so there is no
/// cross-source mixing pitfall.
pub fn current_credentials(config: &Config, store: &dyn TokenStore) -> Option<ResolvedCredentials> {
    if let Some(t) = &config.bot_token_env
        && !t.is_empty()
    {
        return Some(ResolvedCredentials {
            bot_token: t.clone(),
            source: "env",
        });
    }
    let stored = store.load()?;
    if stored.bot_token.is_empty() {
        return None;
    }
    Some(ResolvedCredentials {
        bot_token: stored.bot_token,
        source: "store",
    })
}

#[derive(Default)]
struct GatewaySession {
    bot_user_id: Option<String>,
    session_id: Option<String>,
    seq: Option<i64>,
    /// Region-specific gateway URL; resuming on the wrong host bounces with INVALID_SESSION.
    resume_gateway_url: Option<String>,
}

impl GatewaySession {
    fn can_resume(&self) -> bool {
        self.session_id.is_some() && self.seq.is_some()
    }
    fn full_reset(&mut self) {
        // Keep `bot_user_id`: it is the bot's identity, not a session artifact, and holding it
        // across an INVALID_SESSION window keeps the events filter from treating self-messages as
        // user messages before the next READY.
        self.session_id = None;
        self.seq = None;
        self.resume_gateway_url = None;
    }
}

pub fn run_loop<F>(config: &Config, store: Arc<dyn TokenStore>, stop: &AtomicBool, mut on_event: F)
where
    F: FnMut(DiscordEvent) + Send,
{
    if let Some(err) = &config.fatal_error {
        eprintln!(
            "[discord] gateway loop NOT starting: {err}\n\
             [discord] check `coctl call discord.auth_status` for details"
        );
        return;
    }
    let mut backoff_secs = config.reconnect_initial.as_secs().max(1);
    let mut session = GatewaySession::default();
    let mut last_no_creds_log = Instant::now()
        .checked_sub(NO_CREDS_RECHECK)
        .unwrap_or_else(Instant::now);
    while !stop.load(Ordering::SeqCst) {
        let creds = current_credentials(config, &*store);
        let Some(creds) = creds else {
            if last_no_creds_log.elapsed() >= NO_CREDS_RECHECK {
                eprintln!(
                    "[discord] no credentials yet — waiting (run `copad-plugin-discord auth` \
                     or set COPAD_DISCORD_BOT_TOKEN)"
                );
                last_no_creds_log = Instant::now();
            }
            if interruptible_sleep(stop, NO_CREDS_RECHECK) {
                return;
            }
            continue;
        };
        eprintln!(
            "[discord] connecting (credentials_source={}, can_resume={})",
            creds.source,
            session.can_resume()
        );
        match connect_and_serve(&creds.bot_token, &mut session, stop, &mut on_event) {
            Ok(reason) => {
                eprintln!("[discord] gateway disconnected ({reason}); reconnecting");
                backoff_secs = config.reconnect_initial.as_secs().max(1);
            }
            Err(e) => {
                eprintln!("[discord] gateway error, reconnecting in {backoff_secs}s: {e}");
                if interruptible_sleep(stop, Duration::from_secs(backoff_secs)) {
                    return;
                }
                backoff_secs = (backoff_secs * 2).min(config.reconnect_max.as_secs().max(1));
            }
        }
    }
    eprintln!("[discord] gateway loop exited (stop signal)");
}

fn interruptible_sleep(stop: &AtomicBool, total: Duration) -> bool {
    let step = Duration::from_millis(250);
    let mut elapsed = Duration::ZERO;
    while elapsed < total {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        let remaining = total - elapsed;
        let chunk = if remaining < step { remaining } else { step };
        std::thread::sleep(chunk);
        elapsed += chunk;
    }
    false
}

fn connect_and_serve<F>(
    token: &str,
    session: &mut GatewaySession,
    stop: &AtomicBool,
    on_event: &mut F,
) -> Result<&'static str, String>
where
    F: FnMut(DiscordEvent),
{
    // RESUME prefers the per-session gateway URL from READY; first connect (or a server-side
    // rotation that invalidated the cached URL) uses the bootstrap URL.
    let wss_url = if session.can_resume()
        && let Some(url) = &session.resume_gateway_url
    {
        ensure_query(url)
    } else {
        bootstrap_url(token)?
    };
    let uri: Uri = wss_url
        .parse()
        .map_err(|e| format!("invalid gateway url {wss_url:?}: {e}"))?;
    let (mut ws, _resp) = tungstenite::connect(uri).map_err(|e| format!("ws connect: {e}"))?;
    eprintln!("[discord] gateway WS connected");

    let hello = read_text_with_timeout(&mut ws, Duration::from_secs(15))
        .ok_or_else(|| "no HELLO before timeout".to_string())??;
    let hello_v: Value = serde_json::from_str(&hello).map_err(|e| format!("HELLO parse: {e}"))?;
    if hello_v.get("op").and_then(Value::as_u64) != Some(10) {
        return Err(format!("expected HELLO op=10, got: {hello}"));
    }
    let heartbeat_interval_ms = hello_v
        .get("d")
        .and_then(|d| d.get("heartbeat_interval"))
        .and_then(Value::as_u64)
        .ok_or_else(|| "HELLO missing heartbeat_interval".to_string())?;
    let heartbeat_period = Duration::from_millis(heartbeat_interval_ms);
    set_read_timeout(&mut ws, heartbeat_period / 4)
        .map_err(|e| format!("set TCP read_timeout: {e}"))?;

    if session.can_resume() {
        let frame = json!({
            "op": 6,
            "d": {
                "token": token,
                "session_id": session.session_id,
                "seq": session.seq,
            }
        });
        ws.send(Message::Text(frame.to_string()))
            .map_err(|e| format!("send RESUME: {e}"))?;
        eprintln!(
            "[discord] sent RESUME (session_id={:?}, seq={:?})",
            session.session_id, session.seq
        );
    } else {
        let frame = json!({
            "op": 2,
            "d": {
                "token": token,
                "intents": GATEWAY_INTENTS,
                "properties": {
                    "os": std::env::consts::OS,
                    "browser": "copad-plugin-discord",
                    "device": "copad-plugin-discord",
                }
            }
        });
        ws.send(Message::Text(frame.to_string()))
            .map_err(|e| format!("send IDENTIFY: {e}"))?;
        eprintln!("[discord] sent IDENTIFY (intents={GATEWAY_INTENTS})");
    }

    // First heartbeat is jittered to avoid thundering-herd reconnects.
    let jitter = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 % 1024)
        .unwrap_or(512) as f64)
        / 1024.0;
    let mut next_heartbeat =
        Instant::now() + Duration::from_millis((jitter * heartbeat_interval_ms as f64) as u64);
    // last_ack starts at "now": without a reference point the first zombie check would trip
    // immediately. The first heartbeat is at least `jitter * interval` away, so a real ACK
    // arrives before the next deadline.
    let mut last_ack = Instant::now();
    let mut heartbeat_in_flight = false;

    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= next_heartbeat {
            if heartbeat_in_flight && last_ack < next_heartbeat - heartbeat_period {
                // Heartbeat sent last cycle, no ACK before the next deadline: zombied connection;
                // Discord recommends close + RESUME.
                return Err("zombied connection (no HEARTBEAT_ACK)".to_string());
            }
            send_heartbeat(&mut ws, session.seq)?;
            heartbeat_in_flight = true;
            next_heartbeat = Instant::now() + heartbeat_period;
        }

        match ws.read() {
            Ok(Message::Text(s)) => {
                if let Some(reason) = handle_text_frame(
                    &s,
                    session,
                    on_event,
                    &mut ws,
                    &mut last_ack,
                    &mut heartbeat_in_flight,
                )? {
                    return Ok(reason);
                }
            }
            Ok(Message::Binary(_)) => {
                // The gateway is JSON text; binary would be ETF, which we don't request.
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                // tungstenite answers WS-level ping/pong while read() is driven.
            }
            Ok(Message::Close(frame)) => {
                let reason = frame
                    .as_ref()
                    .map(|f| format!("close {} '{}'", u16::from(f.code), f.reason))
                    .unwrap_or_else(|| "close (no frame)".to_string());
                // Some close codes are NOT recoverable via RESUME (4004 auth failed, 4013 invalid
                // intent, 4014 disallowed privileged intent). Clear the session so the next
                // connect does a fresh IDENTIFY, and surface a hint.
                let close_code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(0);
                if matches!(close_code, 4004 | 4010 | 4011 | 4012 | 4013 | 4014) {
                    session.full_reset();
                    return Err(format!(
                        "fatal close {close_code} ({reason}) — fix bot config and restart"
                    ));
                }
                return Err(format!("peer close: {reason}"));
            }
            Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(tungstenite::Error::ConnectionClosed) => {
                return Err("connection closed unexpectedly".to_string());
            }
            Err(tungstenite::Error::AlreadyClosed) => {
                return Err("connection already closed".to_string());
            }
            Err(e) => return Err(format!("ws read: {e}")),
        }
    }
    Ok("stop signal")
}

fn handle_text_frame<F>(
    s: &str,
    session: &mut GatewaySession,
    on_event: &mut F,
    ws: &mut tungstenite::WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    last_ack: &mut Instant,
    heartbeat_in_flight: &mut bool,
) -> Result<Option<&'static str>, String>
where
    F: FnMut(DiscordEvent),
{
    let frame: Value = serde_json::from_str(s).map_err(|e| format!("bad frame: {e}"))?;
    let op = frame.get("op").and_then(Value::as_u64).unwrap_or(u64::MAX);
    if let Some(s_val) = frame.get("s").and_then(Value::as_i64) {
        // Only DISPATCH (op 0) carries a non-null `s`; tracking seq across other ops would
        // corrupt RESUME.
        if op == 0 {
            session.seq = Some(s_val);
        }
    }
    match op {
        0 => {
            let event_name = frame
                .get("t")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let data = frame.get("d").cloned().unwrap_or(Value::Null);
            match event_name.as_str() {
                "READY" => {
                    session.session_id = data
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    session.resume_gateway_url = data
                        .get("resume_gateway_url")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    session.bot_user_id = data
                        .get("user")
                        .and_then(|u| u.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    eprintln!(
                        "[discord] READY — bot_user_id={:?}, session_id={:?}",
                        session.bot_user_id, session.session_id
                    );
                }
                "RESUMED" => {
                    eprintln!("[discord] session RESUMED");
                }
                _ => {
                    for ev in from_dispatch(&event_name, &data, session.bot_user_id.as_deref()) {
                        on_event(ev);
                    }
                }
            }
        }
        1 => {
            // Server-requested HEARTBEAT: reply at once without touching the normal schedule.
            send_heartbeat(ws, session.seq)?;
            *heartbeat_in_flight = true;
        }
        7 => {
            eprintln!("[discord] op 7 RECONNECT received");
            return Ok(Some("server requested reconnect"));
        }
        9 => {
            // `d` true = resumable; false = session gone, re-IDENTIFY. Discord requires a random
            // 1-5s delay before reconnecting on INVALID_SESSION (a server-side bug can otherwise
            // exhaust session_start_limit, ~1000/day). The outer loop's backoff doesn't cover it
            // because we return Ok, which resets backoff and retries at once, so the sleep lives
            // here to throttle an INVALID_SESSION storm.
            let resumable = frame.get("d").and_then(Value::as_bool).unwrap_or(false);
            eprintln!("[discord] op 9 INVALID_SESSION (resumable={resumable})");
            if !resumable {
                session.full_reset();
            }
            let delay_ms = 1000
                + (SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64 % 4001)
                    .unwrap_or(2000));
            eprintln!("[discord] sleeping {delay_ms}ms before reconnect (per Discord docs)");
            std::thread::sleep(Duration::from_millis(delay_ms));
            return Ok(Some(if resumable {
                "invalid session (resumable)"
            } else {
                "invalid session (full reset)"
            }));
        }
        10 => {
            // Discord doesn't send HELLO after init; log and continue rather than fail.
            eprintln!("[discord] unexpected mid-stream HELLO ignored");
        }
        11 => {
            *last_ack = Instant::now();
            *heartbeat_in_flight = false;
        }
        other => {
            eprintln!("[discord] unknown op {other} ignored");
        }
    }
    Ok(None)
}

fn send_heartbeat(
    ws: &mut tungstenite::WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    seq: Option<i64>,
) -> Result<(), String> {
    // `d: null` is correct before the first DISPATCH (no seq yet); None maps to Null.
    let frame = json!({"op": 1, "d": seq});
    ws.send(Message::Text(frame.to_string()))
        .map_err(|e| format!("send heartbeat: {e}"))
}

/// Walks through the TLS wrapper. Without a read timeout the loop would block until a frame
/// arrives, starving heartbeats on a quiet channel and tripping Discord's 4009 (session timeout).
fn set_read_timeout(
    ws: &mut tungstenite::WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    timeout: Duration,
) -> std::io::Result<()> {
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(timeout)),
        MaybeTlsStream::Rustls(s) => s.get_mut().set_read_timeout(Some(timeout)),
        // NativeTls etc. aren't compiled in via our tungstenite features; the catch-all keeps this
        // forward-compatible.
        _ => Ok(()),
    }
}

/// Bounded one-shot text-frame read for the initial HELLO. `None` on timeout, `Some(Err)` on
/// IO/parse errors.
fn read_text_with_timeout(
    ws: &mut tungstenite::WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    total: Duration,
) -> Option<Result<String, String>> {
    if let Err(e) = set_read_timeout(ws, total) {
        return Some(Err(format!("set read timeout: {e}")));
    }
    match ws.read() {
        Ok(Message::Text(s)) => Some(Ok(s)),
        Ok(other) => Some(Err(format!("expected text frame, got {other:?}"))),
        Err(tungstenite::Error::Io(e))
            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
        {
            None
        }
        Err(e) => Some(Err(format!("ws read: {e}"))),
    }
}

/// `resume_gateway_url` arrives without query params; `v=10&encoding=json` pins the wire format.
fn ensure_query(url: &str) -> String {
    if url.contains("v=") {
        url.to_string()
    } else if url.contains('?') {
        format!("{url}&v=10&encoding=json")
    } else {
        format!("{url}/?v=10&encoding=json")
    }
}

/// Authenticated `/gateway/bot` (vs public `/gateway`) so over-quota IDENTIFYs surface at connect
/// time via session-start-limit instead of mid-loop.
fn bootstrap_url(token: &str) -> Result<String, String> {
    let resp = ureq::get(&format!("{DISCORD_API_BASE}/gateway/bot"))
        .set("Authorization", &format!("Bot {token}"))
        .set("User-Agent", "copad-plugin-discord (copad, 0.1)")
        .timeout(HTTP_TIMEOUT)
        .call()
        .map_err(|e| format!("/gateway/bot: {e}"))?;
    let status = resp.status();
    if !(200..300).contains(&status) {
        let body = resp.into_string().unwrap_or_default();
        return Err(format!("/gateway/bot HTTP {status}: {body}"));
    }
    let body: Value = resp
        .into_json()
        .map_err(|e| format!("/gateway/bot parse: {e}"))?;
    if let Some(limit) = body.get("session_start_limit") {
        let remaining = limit
            .get("remaining")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX);
        if remaining < 5 {
            // Surface loudly: at 0 Discord refuses IDENTIFY until the daily reset, so the plugin
            // would loop on errors all day.
            eprintln!(
                "[discord] WARNING session_start_limit.remaining={remaining} \
                 (resets in {}ms)",
                limit
                    .get("reset_after")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            );
        }
    }
    let host = body
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "/gateway/bot response missing 'url'".to_string())?;
    Ok(ensure_query(host))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TokenSet;

    struct StubStore(Option<TokenSet>);
    impl TokenStore for StubStore {
        fn load(&self) -> Option<TokenSet> {
            self.0.clone()
        }
        fn save(&self, _: &TokenSet) -> Result<(), String> {
            Ok(())
        }
        fn clear(&self) -> Result<(), String> {
            Ok(())
        }
        fn kind(&self) -> &'static str {
            "stub"
        }
    }

    fn cfg(token_env: Option<&str>) -> Config {
        Config {
            workspace_label: "test".into(),
            bot_token_env: token_env.map(str::to_string),
            plaintext_path: std::path::PathBuf::from("/tmp/unused"),
            require_secure_store: false,
            reconnect_initial: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            fatal_error: None,
        }
    }

    fn stored(token: &str) -> StubStore {
        StubStore(Some(TokenSet {
            bot_token: token.into(),
            user_id: Some("BOT_STORE".into()),
            username: Some("copad".into()),
        }))
    }

    #[test]
    fn env_token_wins_over_store() {
        let store = stored("store-token");
        let r = current_credentials(&cfg(Some("env-token")), &store).unwrap();
        assert_eq!(r.bot_token, "env-token");
        assert_eq!(r.source, "env");
    }

    #[test]
    fn falls_back_to_store_when_env_absent() {
        let store = stored("store-token");
        let r = current_credentials(&cfg(None), &store).unwrap();
        assert_eq!(r.bot_token, "store-token");
        assert_eq!(r.source, "store");
    }

    #[test]
    fn no_creds_returns_none() {
        assert!(current_credentials(&cfg(None), &StubStore(None)).is_none());
    }

    #[test]
    fn empty_stored_token_returns_none() {
        let store = StubStore(Some(TokenSet {
            bot_token: String::new(),
            user_id: None,
            username: None,
        }));
        assert!(current_credentials(&cfg(None), &store).is_none());
    }

    #[test]
    fn ensure_query_adds_version_when_missing() {
        assert_eq!(
            ensure_query("wss://gateway.discord.gg"),
            "wss://gateway.discord.gg/?v=10&encoding=json"
        );
    }

    #[test]
    fn ensure_query_appends_when_url_already_has_query() {
        assert_eq!(
            ensure_query("wss://gateway.discord.gg/?compress=zlib"),
            "wss://gateway.discord.gg/?compress=zlib&v=10&encoding=json"
        );
    }

    #[test]
    fn ensure_query_passes_through_when_versioned() {
        assert_eq!(
            ensure_query("wss://gateway.discord.gg/?v=10&encoding=json"),
            "wss://gateway.discord.gg/?v=10&encoding=json"
        );
    }
}
