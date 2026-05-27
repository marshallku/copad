//! Discord Gateway WebSocket client.
//!
//! Flow per connection:
//! 1. `GET /gateway/bot` (or `/gateway`) returns the WSS URL. Discord
//!    handles its own load-balancing here. Resume connections instead
//!    use the `resume_gateway_url` from the prior session's READY.
//! 2. Connect via tungstenite. WS handshake is plain — auth happens in
//!    the IDENTIFY/RESUME payload.
//! 3. First server frame is HELLO (op 10) which carries
//!    `heartbeat_interval` (ms). We schedule heartbeat sends AND set
//!    the underlying TCP read timeout to a fraction of that interval
//!    so the single-threaded read loop wakes up to send heartbeats on
//!    schedule.
//! 4. Send IDENTIFY (op 2) on first connect, or RESUME (op 6) when we
//!    have a live `session_id` + `seq`. Server responds with READY
//!    (op 0 t=READY) capturing `user.id`, `session_id`, and
//!    `resume_gateway_url`, OR RESUMED (op 0 t=RESUMED) for a
//!    successful resume, OR INVALID_SESSION (op 9) which forces a
//!    full IDENTIFY after a small random delay.
//! 5. Steady state reads:
//!    - op 0 DISPATCH — update `seq`, parse `t`+`d`, hand to
//!      `events::from_dispatch`, deliver to `on_event`.
//!    - op 1 HEARTBEAT — server-prompted; reply immediately.
//!    - op 7 RECONNECT — close + reconnect with RESUME.
//!    - op 9 INVALID_SESSION — `d=true` resumable; `d=false` full
//!      reset (clear session_id+seq+resume_url, IDENTIFY fresh).
//!    - op 10 HELLO — only valid as the first frame; later HELLO is
//!      a protocol surprise and ignored.
//!    - op 11 HEARTBEAT_ACK — touch `last_ack` for zombie detection.
//! 6. Zombie detection: if we sent a heartbeat but didn't receive an
//!    ACK before the next send, the connection is dead — close and
//!    reconnect with RESUME. Discord recommends this exact heuristic.
//!
//! Threading: single thread per connection. Heartbeat scheduling is
//! interleaved with frame reads via a TCP read timeout — a cleaner
//! design than spawning a heartbeat thread that contends with the
//! reader for `WebSocket` access. The TCP timeout is set to roughly
//! `heartbeat_interval / 4` so we wake up at least four times per
//! interval to check for due heartbeats and the zombie deadline.
//!
//! Intents bitfield: see `GATEWAY_INTENTS` constant below for the
//! current value and per-bit purpose. Privileged MESSAGE_CONTENT
//! must be enabled on the application's Bot tab; we surface a clear
//! error if Discord rejects IDENTIFY due to it (close code 4014).

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
/// `GUILD_MESSAGES | GUILD_MESSAGE_REACTIONS | DIRECT_MESSAGES |
/// DIRECT_MESSAGE_REACTIONS | MESSAGE_CONTENT`. The privileged
/// `MESSAGE_CONTENT` bit needs Developer Portal → Bot tab opt-in;
/// without it `content` arrives empty for non-mentions and keyword
/// triggers can't match. Reaction bits are non-privileged but must
/// still be in the field or `MESSAGE_REACTION_ADD` never dispatches.
const GATEWAY_INTENTS: u64 = (1 << 9) | (1 << 10) | (1 << 12) | (1 << 13) | (1 << 15);
/// Recheck cadence when no credentials — fresh `auth` is picked up
/// without supervisor restart (slack-plugin parallel).
const NO_CREDS_RECHECK: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ResolvedCredentials {
    pub bot_token: String,
    pub source: &'static str, // "env" | "store"
}

/// Env wins (test overrides), store falls back. Discord uses a single
/// token so there's no cross-source-mixing pitfall (cf. Slack pair).
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
    /// Discord may issue a region-specific gateway URL; resuming on the
    /// wrong host bounces with INVALID_SESSION.
    resume_gateway_url: Option<String>,
}

impl GatewaySession {
    fn can_resume(&self) -> bool {
        self.session_id.is_some() && self.seq.is_some()
    }
    fn full_reset(&mut self) {
        // Keep `bot_user_id` across resets — it's the bot's identity,
        // not a session artifact. Re-IDENTIFY will overwrite it from
        // the next READY frame anyway, but holding it across a brief
        // INVALID_SESSION window means the events filter doesn't
        // accidentally treat self-messages as user messages between
        // disconnect and re-READY.
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
    // Decide URL. RESUME prefers the per-session gateway URL Discord
    // returned in READY; fall back to bootstrap for first connect or
    // when a server-side rotation invalidated the cached URL.
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

    // Step 1: receive HELLO
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

    // Step 2: send IDENTIFY or RESUME
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
    // last_ack starts at "now" — without an ACK reference point, the
    // first zombie check would trip immediately. The first scheduled
    // heartbeat won't fire for at least `jitter * interval` ms so we
    // have a real ACK before the next deadline.
    let mut last_ack = Instant::now();
    let mut heartbeat_in_flight = false;

    while !stop.load(Ordering::SeqCst) {
        // Heartbeat scheduling: send if due, then check zombie state.
        let now = Instant::now();
        if now >= next_heartbeat {
            if heartbeat_in_flight && last_ack < next_heartbeat - heartbeat_period {
                // Sent a heartbeat last cycle, never got an ACK before
                // the next deadline → zombied connection. Discord docs
                // recommend close + RESUME.
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
                // Discord gateway is JSON-encoded text — binary would be
                // ETF (Erlang Term Format), opt-in via encoding=etf. We
                // don't request it.
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                // tungstenite handles WS-level ping/pong automatically
                // when we drive read() regularly.
            }
            Ok(Message::Close(frame)) => {
                let reason = frame
                    .as_ref()
                    .map(|f| format!("close {} '{}'", u16::from(f.code), f.reason))
                    .unwrap_or_else(|| "close (no frame)".to_string());
                // Some Discord close codes are NOT recoverable via
                // RESUME — surface them with a hint so the user can
                // diagnose. 4004 auth failed, 4013 invalid intent,
                // 4014 disallowed (privileged) intent. After these we
                // also clear the session so the next connect goes
                // through fresh IDENTIFY (which will fail again until
                // the user fixes config, but at least logs are
                // useful).
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
                // Read timeout — loop falls through to heartbeat check.
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
        // Only DISPATCH (op 0) carries non-null `s`; other ops set it
        // to null which serde_json::Value::as_i64 returns None for.
        // Tracking the latest seq across all ops would corrupt RESUME
        // if Discord ever changes that contract.
        if op == 0 {
            session.seq = Some(s_val);
        }
    }
    match op {
        0 => {
            // DISPATCH
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
            // Server-requested HEARTBEAT — reply immediately. Don't
            // touch our normal scheduling; the next scheduled
            // heartbeat will fire on its own clock.
            send_heartbeat(ws, session.seq)?;
            *heartbeat_in_flight = true;
        }
        7 => {
            // RECONNECT — graceful reconnect with RESUME.
            eprintln!("[discord] op 7 RECONNECT received");
            return Ok(Some("server requested reconnect"));
        }
        9 => {
            // INVALID_SESSION. `d` is `true` when resumable. False
            // means the session is gone — clear and re-IDENTIFY.
            //
            // Discord docs require a randomized 1–5s delay before
            // reconnecting on INVALID_SESSION (otherwise a server-
            // side bug can spin clients fast enough to exhaust
            // session_start_limit, ~1000/day for most bots). The
            // outer reconnect loop's backoff doesn't cover this
            // because we return Ok (graceful) — Ok resets backoff to
            // initial and immediately retries. Insert the sleep
            // here, inside the Ok path, so a tight INVALID_SESSION
            // storm self-throttles.
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
            // Stray HELLO — Discord doesn't send these post-init in
            // normal operation. Log and continue rather than fail; a
            // mid-stream HELLO would be a server-side change we can
            // observe via the log.
            eprintln!("[discord] unexpected mid-stream HELLO ignored");
        }
        11 => {
            // HEARTBEAT_ACK
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
    // `d: null` is correct when we have no seq yet (first heartbeat
    // before any DISPATCH); serde_json maps None → Null automatically.
    let frame = json!({"op": 1, "d": seq});
    ws.send(Message::Text(frame.to_string()))
        .map_err(|e| format!("send heartbeat: {e}"))
}

/// Walks through the TLS wrapper. Without a read timeout the loop
/// would block until a frame arrives, starving heartbeat scheduling
/// on a quiet channel and tripping Discord's 4009 (session timeout).
fn set_read_timeout(
    ws: &mut tungstenite::WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    timeout: Duration,
) -> std::io::Result<()> {
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(timeout)),
        MaybeTlsStream::Rustls(s) => s.get_mut().set_read_timeout(Some(timeout)),
        // Other TLS variants (NativeTls) aren't compiled in via our
        // tungstenite features list — explicit catch-all keeps us
        // forward-compatible if that changes.
        _ => Ok(()),
    }
}

/// Bounded one-shot text-frame read for the initial HELLO. `None` on
/// timeout, `Some(Ok)` on a text frame, `Some(Err)` on IO/parse errors.
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

/// `resume_gateway_url` arrives without query params; the
/// `v=10&encoding=json` suffix pins the wire format (server defaults
/// have changed over time).
fn ensure_query(url: &str) -> String {
    if url.contains("v=") {
        url.to_string()
    } else if url.contains('?') {
        format!("{url}&v=10&encoding=json")
    } else {
        format!("{url}/?v=10&encoding=json")
    }
}

/// Authenticated `/gateway/bot` (vs public `/gateway`) so over-quota
/// IDENTIFYs surface at connect time via session-start-limit instead
/// of mid-loop.
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
            // Surfacing the warning loud — once `remaining` hits 0
            // Discord refuses IDENTIFY entirely until the daily
            // reset, so the plugin would loop on errors all day.
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
