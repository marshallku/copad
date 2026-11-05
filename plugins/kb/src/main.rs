//! First-party KB service plugin: `kb.search` / `kb.read` / `kb.append` / `kb.ensure` over a
//! filesystem root (default `~/docs`, overridable via `COPAD_KB_ROOT`). Protocol contract:
//! `docs/kb-protocol.md`.
//!
//! - `id` is a logical path-like key, validated against `..`, leading slash and embedded nul.
//! - `kb.ensure` uses temp-file + `renameat2(RENAME_NOREPLACE)`: one creator, no torn reads.
//! - `kb.append` writes the whole payload in one `write(2)` on an `O_APPEND` fd; short writes are
//!   errors, not retried, so concurrent appends never interleave.
//! - `.raw/` is excluded from `kb.search` results (still writable by id).
//!
//! Unix-only: relies on `O_NOFOLLOW` plus a kernel atomic-create-or-fail primitive (Linux
//! `renameat2(RENAME_NOREPLACE)` / macOS `renamex_np(RENAME_EXCL)`) via `copad_core::fs_atomic`.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "copad-plugin-kb supports Linux and macOS. Other Unixes need a \
     backend-specific atomic-create primitive — extend copad_core::fs_atomic."
);

mod kb;

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use serde_json::{Value, json};

use kb::Kb;

const PROTOCOL_VERSION: u32 = 1;

fn main() {
    let kb = Kb::from_env();
    eprintln!("[kb] root = {}", kb.root().display());

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    // Single writer thread so replies never interleave bytes.
    let (tx, rx) = channel::<String>();
    let writer_tx = tx.clone();
    thread::spawn(move || {
        let mut out = stdout.lock();
        for line in rx.iter() {
            if writeln!(out, "{line}").is_err() {
                break;
            }
            if out.flush().is_err() {
                break;
            }
        }
    });

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
                eprintln!("[kb] parse error: {e}");
                continue;
            }
        };
        handle_frame(&kb, &value, &writer_tx);
    }
}

fn handle_frame(kb: &Kb, frame: &Value, tx: &Sender<String>) {
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
                    &format!("kb plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": ["kb.search", "kb.read", "kb.append", "kb.ensure", "kb.list"],
                    "subscribes": [],
                }),
            );
        }
        "initialized" => {}
        "action.invoke" => {
            let action_name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let action_params = params.get("params").cloned().unwrap_or(Value::Null);
            match kb.invoke(&action_name, &action_params) {
                Ok(result) => send_response(tx, id, result),
                Err((code, msg)) => send_error(tx, id, &code, &msg),
            }
        }
        "event.dispatch" => {
            // KB doesn't subscribe to anything; ignore.
        }
        "shutdown" => {
            std::process::exit(0);
        }
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                tx,
                id,
                "unknown_method",
                &format!("kb plugin: unknown method {other}"),
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
        "error": { "code": code, "message": message },
    });
    let _ = tx.send(frame.to_string());
}
