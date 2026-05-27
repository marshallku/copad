//! First-party Google Calendar service plugin for copad.
//!
//! Two run modes (selected by `argv[1]`):
//! - **`auth`** — interactive OAuth 2.0 device-code flow. Prints a
//!   user_code and verification URL to stderr, polls Google's token
//!   endpoint until the user approves, then writes the resulting
//!   `TokenSet` to the configured store (keyring with plaintext
//!   fallback). Exits 0 on success.
//! - **(no args)** — RPC mode. Speaks the copad service-plugin protocol
//!   over stdio, provides `calendar.list_events` / `calendar.event_details`,
//!   and runs a background poller that publishes `calendar.event_imminent`
//!   events at the lead times configured via `COPAD_CALENDAR_LEAD_MINUTES`.
//!
//! If RPC mode starts with no stored token the plugin still completes
//! `initialize` (so `provides` resolution works) but actions return
//! `not_authenticated` and the poller stays idle until tokens appear.
//! This lets the user run `copad-plugin-calendar auth` while copad is
//! already running — the poller picks up the new token on its next tick.
//!
//! See the protocol contract in `docs/service-plugins.md`. Per-event
//! customisation (different actions for different recurring events,
//! conditional execution by attendance status) lives in the user's
//! `[[triggers]]` config — calendar plugin is purely an event emitter.
//!
//! Unix-only. `keyring`'s mock fallback on platforms with no native
//! credential-store feature would let `auth` succeed without
//! actually persisting tokens — a silent failure mode we refuse to
//! ship. Linux + macOS is copad's full support matrix; if/when a
//! Windows port lands, add `windows-native` to the keyring features
//! and relax this gate.

#[cfg(not(unix))]
compile_error!(
    "copad-plugin-calendar is currently Unix-only. The keyring crate's mock fallback \
     would silently lose tokens on platforms without a native credential-store \
     feature; gate exists to make that failure compile-time instead of runtime."
);

mod config;
mod event;
mod gcal;
mod oauth;
mod poller;
mod store;

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use serde_json::{Value, json};

use config::Config;
use poller::Poller;
use store::TokenStore;

const PROTOCOL_VERSION: u32 = 1;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("auth") => run_auth(),
        Some(other) => {
            eprintln!("[calendar] unknown subcommand: {other}");
            eprintln!("usage: copad-plugin-calendar [auth]");
            std::process::exit(2);
        }
        None => run_rpc(),
    }
}

fn run_auth() {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[calendar] config error: {e}");
            std::process::exit(1);
        }
    };
    let store = store::open_store(&config);
    eprintln!("[calendar] token store: {}", store.kind());

    match oauth::run_device_code_flow(&config) {
        Ok(tokens) => {
            if let Err(e) = store.save(&tokens) {
                eprintln!("[calendar] failed to save tokens: {e}");
                std::process::exit(1);
            }
            eprintln!("[calendar] auth ok — tokens stored, you can now start copad");
        }
        Err(e) => {
            eprintln!("[calendar] auth failed: {e}");
            std::process::exit(1);
        }
    }
}

fn run_rpc() {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            // Don't fail init — the plugin can still echo errors back
            // for actions, and the user might fix the env vars while
            // copad is running. Log loudly so the cause is obvious.
            eprintln!("[calendar] config error (actions will fail): {e}");
            // Use a minimal config so we can at least serve initialize.
            // Subsequent actions will return not_authenticated.
            Config::minimal()
        }
    };

    let store: Arc<dyn TokenStore> = Arc::from(store::open_store(&config));
    eprintln!("[calendar] token store: {}", store.kind());

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    // Single writer thread funnels all outgoing JSON so init reply,
    // action replies, and event.publish notifications never interleave.
    let (tx, rx) = channel::<String>();
    let writer_tx = tx.clone();
    thread::spawn(move || {
        let mut out = stdout.lock();
        for line in rx.iter() {
            if writeln!(out, "{line}").is_err() || out.flush().is_err() {
                break;
            }
        }
    });

    let initialized = Arc::new(AtomicBool::new(false));

    // Poller runs in its own thread. It checks the store on every tick,
    // so it tolerates the auth-after-startup case.
    let poller = Arc::new(Poller::new(
        Arc::new(config.clone()),
        store.clone(),
        tx.clone(),
        initialized.clone(),
    ));
    {
        let p = poller.clone();
        thread::spawn(move || p.run());
    }

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
                eprintln!("[calendar] parse error: {e}");
                continue;
            }
        };
        handle_frame(&frame, &writer_tx, &initialized, &config, &store);
    }
}

fn handle_frame(
    frame: &Value,
    tx: &Sender<String>,
    initialized: &AtomicBool,
    config: &Config,
    store: &Arc<dyn TokenStore>,
) {
    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    let id = frame.get("id").and_then(Value::as_str).unwrap_or("");
    let params = frame.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let proto = params.get("protocol_version").and_then(Value::as_u64);
            if proto != Some(PROTOCOL_VERSION as u64) {
                send_error(
                    tx,
                    id,
                    "protocol_mismatch",
                    &format!("calendar plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": ["calendar.list_events", "calendar.event_details", "calendar.auth_status"],
                    "subscribes": [],
                }),
            );
        }
        "initialized" => {
            initialized.store(true, Ordering::SeqCst);
        }
        "action.invoke" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let action_params = params.get("params").cloned().unwrap_or(Value::Null);
            let result = handle_action(&name, &action_params, config, store);
            match result {
                Ok(v) => send_response(tx, id, v),
                Err((code, msg)) => send_error(tx, id, &code, &msg),
            }
        }
        "event.dispatch" => {
            // calendar plugin doesn't subscribe — quietly ignore.
        }
        "shutdown" => std::process::exit(0),
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                tx,
                id,
                "unknown_method",
                &format!("calendar plugin: unknown method {other}"),
            );
        }
        _ => {}
    }
}

fn handle_action(
    name: &str,
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
) -> Result<Value, (String, String)> {
    // auth_status surfaces BOTH the env-configured state and the
    // stored-credentials state so callers can distinguish "no
    // CLIENT_ID set" from "tokens not yet authenticated". A plugin
    // started with bad env can still hold a valid token from a
    // previous good run; without `configured`, that mixed state
    // looks like normal "authenticated" until the next refresh
    // breaks.
    if name == "calendar.auth_status" {
        return Ok(json!({
            "configured": !config.is_minimal(),
            "authenticated": !config.is_minimal() && store.load().is_some(),
            "store_kind": store.kind(),
            "account": config.account_label.clone(),
        }));
    }
    // For everything that needs to talk to Google we refuse early
    // when env is missing — without this guard, a stale stored token
    // would make `list_events` succeed once and then break on
    // refresh with a confusing "client_secret missing" error.
    if config.is_minimal() {
        return Err((
            "not_authenticated".to_string(),
            "COPAD_CALENDAR_CLIENT_ID / COPAD_CALENDAR_CLIENT_SECRET not set".to_string(),
        ));
    }
    match name {
        "calendar.list_events" => {
            let mut client = gcal::Client::new(config.clone(), store.clone())
                .map_err(|e| ("not_authenticated".to_string(), e))?;
            let lookahead_hours = parse_lookahead_param(params, config.lookahead_hours as u64)?;
            let now = chrono::Utc::now();
            let max = now + chrono::Duration::hours(lookahead_hours as i64);
            let events = client
                .list_events(now, max)
                .map_err(|e| ("io_error".to_string(), e))?;
            let arr: Vec<Value> = events.iter().map(event::to_json).collect();
            Ok(json!({ "events": arr }))
        }
        "calendar.event_details" => {
            let event_id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or(("invalid_params".to_string(), "missing 'id'".to_string()))?;
            let mut client = gcal::Client::new(config.clone(), store.clone())
                .map_err(|e| ("not_authenticated".to_string(), e))?;
            let evt = client
                .get_event(event_id)
                .map_err(|e| ("io_error".to_string(), e))?;
            match evt {
                Some(e) => Ok(event::to_json(&e)),
                None => Err(("not_found".to_string(), format!("no event {event_id}"))),
            }
        }
        other => Err((
            "action_not_found".to_string(),
            format!("calendar plugin does not handle {other}"),
        )),
    }
}

/// Capped at one year — `0` or huge values would cause a tight refresh
/// loop or wrap negative when cast to `i64` for `chrono::Duration`.
fn parse_lookahead_param(params: &Value, default: u64) -> Result<u64, (String, String)> {
    const MAX_HOURS: u64 = 24 * 365;
    let Some(v) = params.get("lookahead_hours") else {
        return Ok(default);
    };
    let n = v.as_u64().ok_or((
        "invalid_params".to_string(),
        "lookahead_hours must be a positive integer".to_string(),
    ))?;
    if n == 0 {
        return Err((
            "invalid_params".to_string(),
            "lookahead_hours must be > 0".to_string(),
        ));
    }
    if n > MAX_HOURS {
        return Err((
            "invalid_params".to_string(),
            format!("lookahead_hours must be <= {MAX_HOURS}"),
        ));
    }
    Ok(n)
}

fn send_response(tx: &Sender<String>, id: &str, result: Value) {
    let frame = json!({ "id": id, "ok": true, "result": result });
    let _ = tx.send(frame.to_string());
}

fn send_error(tx: &Sender<String>, id: &str, code: &str, message: &str) {
    let frame = json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message },
    });
    let _ = tx.send(frame.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use store::TokenStore;

    struct EmptyStore;
    impl TokenStore for EmptyStore {
        fn load(&self) -> Option<store::TokenSet> {
            None
        }
        fn save(&self, _: &store::TokenSet) -> Result<(), String> {
            Ok(())
        }
        fn clear(&self) -> Result<(), String> {
            Ok(())
        }
        fn kind(&self) -> &'static str {
            "test"
        }
    }

    fn minimal_arc() -> Arc<dyn TokenStore> {
        Arc::new(EmptyStore)
    }

    #[test]
    fn auth_status_reports_not_configured_when_env_missing() {
        let store = minimal_arc();
        let result = handle_action(
            "calendar.auth_status",
            &Value::Null,
            &Config::minimal(),
            &store,
        )
        .unwrap();
        assert_eq!(result["configured"], false);
        assert_eq!(result["authenticated"], false);
    }

    #[test]
    fn list_events_short_circuits_when_minimal_config() {
        let store = minimal_arc();
        let err = handle_action(
            "calendar.list_events",
            &Value::Null,
            &Config::minimal(),
            &store,
        )
        .unwrap_err();
        assert_eq!(err.0, "not_authenticated");
    }

    #[test]
    fn event_details_short_circuits_when_minimal_config() {
        let store = minimal_arc();
        let err = handle_action(
            "calendar.event_details",
            &json!({"id": "x"}),
            &Config::minimal(),
            &store,
        )
        .unwrap_err();
        assert_eq!(err.0, "not_authenticated");
    }

    #[test]
    fn parse_lookahead_rejects_zero_and_overlong() {
        let p = json!({ "lookahead_hours": 0 });
        assert!(parse_lookahead_param(&p, 24).is_err());
        let p = json!({ "lookahead_hours": 24 * 365 + 1 });
        assert!(parse_lookahead_param(&p, 24).is_err());
        let p = json!({ "lookahead_hours": "abc" });
        assert!(parse_lookahead_param(&p, 24).is_err());
    }

    #[test]
    fn parse_lookahead_accepts_valid() {
        let p = json!({ "lookahead_hours": 12 });
        assert_eq!(parse_lookahead_param(&p, 24).unwrap(), 12);
        let p = json!({});
        assert_eq!(parse_lookahead_param(&p, 24).unwrap(), 24);
        let p = json!({ "lookahead_hours": 24 * 365 });
        assert_eq!(parse_lookahead_param(&p, 24).unwrap(), 24 * 365);
    }
}
