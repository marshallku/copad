//! Mock service plugin for copad.
//!
//! Useful as both a wire-protocol smoke test and a debug heartbeat.
//! Exposes a single action (`echo.ping`) that round-trips its params and
//! publishes a `system.heartbeat` event on a configurable interval
//! (`COPAD_ECHO_HEARTBEAT_SECS`, defaulting to 30s — short during E2E,
//! long enough not to spam logs in normal use).

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const PROTOCOL_VERSION: u32 = 1;

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    // All writes funnel through one channel so the heartbeat thread
    // and the request handler can't interleave bytes mid-line on
    // stdout.
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

    // Heartbeat publisher. Sleeps then sends — and gates on the
    // `initialized` notification flag so heartbeat events never leak
    // out before copad has finished the handshake.
    let initialized = Arc::new(AtomicBool::new(false));
    let interval_secs: u64 = std::env::var("COPAD_ECHO_HEARTBEAT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    if interval_secs > 0 {
        let hb_tx = tx.clone();
        let init_flag = initialized.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(interval_secs));
                if !init_flag.load(Ordering::SeqCst) {
                    continue;
                }
                let event = json!({
                    "method": "event.publish",
                    "params": {
                        "kind": "system.heartbeat",
                        "payload": {
                            "source": "copad-plugin-echo",
                            "timestamp_ms": now_millis(),
                        }
                    }
                });
                if hb_tx.send(event.to_string()).is_err() {
                    break;
                }
            }
        });
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
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[echo] parse error: {e}");
                continue;
            }
        };
        handle_frame(&value, &writer_tx, &initialized);
    }
}

fn handle_frame(value: &Value, tx: &Sender<String>, initialized: &AtomicBool) {
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    let params = value.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            // Verify the protocol version matches before claiming
            // capabilities. A mismatch surfaces clearly via the reply
            // rather than silent miswires later.
            let proto = params.get("protocol_version").and_then(Value::as_u64);
            if proto != Some(PROTOCOL_VERSION as u64) {
                send_error(
                    tx,
                    id,
                    "protocol_mismatch",
                    &format!("echo plugin only speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": ["echo.ping"],
                    "subscribes": [],
                }),
            );
        }
        "initialized" => {
            initialized.store(true, Ordering::SeqCst);
        }
        "action.invoke" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let action_params = params.get("params").cloned().unwrap_or(Value::Null);
            match name {
                "echo.ping" => {
                    // Optional `sleep_ms` lets E2E callers exercise the
                    // Phase 9.4 GTK-non-blocking guarantee — the plugin
                    // parks for the requested duration before replying,
                    // letting the test verify that concurrent dispatches
                    // on the host side stay live during the wait.
                    if let Some(ms) = action_params.get("sleep_ms").and_then(Value::as_u64) {
                        thread::sleep(Duration::from_millis(ms));
                    }
                    send_response(
                        tx,
                        id,
                        json!({ "echoed": action_params, "from": "copad-plugin-echo" }),
                    );
                }
                other => {
                    send_error(
                        tx,
                        id,
                        "action_not_found",
                        &format!("echo plugin does not handle {other}"),
                    );
                }
            }
        }
        "event.dispatch" => {
            // The echo plugin doesn't subscribe to anything, but the
            // protocol allows copad to forward events for future
            // configurations; ignore quietly.
        }
        "shutdown" => {
            std::process::exit(0);
        }
        // Unknown request: only reply if it had an id (otherwise
        // it was a notification — quietly ignored).
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                tx,
                id,
                "unknown_method",
                &format!("echo plugin: unknown method {other}"),
            );
        }
        _ => {}
    }
}

fn send_response(tx: &Sender<String>, id: &str, result: Value) {
    let frame = json!({ "id": id, "ok": true, "result": result });
    let _ = tx.send(frame.to_string());
}

fn send_error(tx: &Sender<String>, id: &str, code: &str, message: &str) {
    let frame = json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message }
    });
    let _ = tx.send(frame.to_string());
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
