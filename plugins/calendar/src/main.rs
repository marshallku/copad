//! First-party Google Calendar service plugin for copad.
//!
//! Run mode is selected by `argv[1]`:
//! - `auth`: interactive OAuth 2.0 device-code flow. Prints a user_code and verification URL to
//!   stderr, polls Google's token endpoint until approval, then writes the `TokenSet` to the
//!   store (keyring with plaintext fallback).
//! - no args: RPC mode over stdio (`docs/service-plugins.md`), providing `calendar.list_events` /
//!   `calendar.event_details` plus a poller that publishes `calendar.event_imminent` at the lead
//!   times in `COPAD_CALENDAR_LEAD_MINUTES`.
//!
//! With no stored token, `initialize` still completes (so `provides` resolution works), actions
//! return `not_authenticated` and the poller idles until `copad-plugin-calendar auth` supplies
//! tokens. Per-event customisation lives in the user's `[[triggers]]`; the plugin only emits events.
//!
//! Unix-only: `keyring`'s mock fallback on platforms without a native credential-store feature
//! would let `auth` succeed without persisting tokens. A Windows port needs `windows-native` in
//! the keyring features and this gate relaxed.

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
    if config.is_minimal() {
        eprintln!(
            "[calendar] auth requires COPAD_CALENDAR_CLIENT_ID and COPAD_CALENDAR_CLIENT_SECRET"
        );
        std::process::exit(1);
    }
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
            // Don't fail init: the user may fix the env vars while copad runs. Log loudly.
            eprintln!("[calendar] config error (actions will fail): {e}");
            // Minimal config to serve initialize; actions return not_authenticated and the error is
            // toasted once via plugin.config_error.
            Config::minimal_with_error(e)
        }
    };

    let store: Arc<dyn TokenStore> = Arc::from(store::open_store(&config));
    eprintln!("[calendar] token store: {}", store.kind());

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    // Single writer thread so init reply, action replies and event.publish never interleave.
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

    // The poller checks the store every tick, so it tolerates auth after startup.
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
            if let Some(err) = &config.fatal_error {
                emit_config_error(tx, err);
            }
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
        "event.dispatch" => {}
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
    // auth_status reports both the env-configured and stored-credentials state, so "no CLIENT_ID
    // set" is distinguishable from "not yet authenticated". Without `configured`, a bad env plus an
    // old valid token looks authenticated until the next refresh breaks.
    if name == "calendar.auth_status" {
        return Ok(json!({
            "configured": !config.is_minimal(),
            "authenticated": !config.is_minimal() && store.load().is_some(),
            "store_kind": store.kind(),
            "account": config.account_label.clone(),
        }));
    }
    // Refuse early when env is missing: a stale stored token would let `list_events` succeed once
    // and then fail on refresh with a confusing "client_secret missing" error.
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

/// Capped at one year: `0` or huge values would cause a tight refresh loop or wrap negative as `i64`.
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

/// Env vars/steps that fix a fatal config error — quoted verbatim in the toast the supervisor
/// raises, so a misconfigured plugin tells the user exactly what to set instead of failing silent.
const CONFIG_REMEDIATION: &str = "set COPAD_CALENDAR_CLIENT_ID and COPAD_CALENDAR_CLIENT_SECRET, \
     then run `copad-plugin-calendar auth`";

/// Publish a one-shot `plugin.config_error` event so the daemon can toast the misconfiguration.
/// Emitted only after `initialized`, since frames sent while the service is still `Starting` may
/// be dropped before the supervisor bridges this plugin's stdout onto the bus.
fn emit_config_error(tx: &Sender<String>, message: &str) {
    let frame = json!({
        "method": "event.publish",
        "params": {
            "kind": "plugin.config_error",
            "payload": { "service": "calendar", "message": message, "remediation": CONFIG_REMEDIATION },
        }
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
