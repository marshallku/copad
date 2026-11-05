//! First-party bookmark service plugin for copad.
//!
//! Captures URLs into `~/docs/bookmarks/YYYY-MM/<urlhash8>-<slug>.md`, one frontmatter-headed
//! Markdown file per bookmark. The filesystem is the source of truth with no on-disk index:
//! `bookmark.list` walks the tree (as the kb/todo plugins do), so vim/git edits are safe.
//!
//! Actions: `bookmark.add` (canonicalize URL, dedup by urlhash8, write a queued stub, no fetch),
//! `bookmark.list` (optional filters), `bookmark.show` and `bookmark.delete` (by id-prefix or url).
//!
//! Unix-only. Path-safety mirrors `copad-plugin-kb` (canonicalize root, re-validate every
//! resolved path); the atomic create-or-fail rename goes through `copad_core::fs_atomic`.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "copad-plugin-bookmark supports Linux and macOS. Other Unixes need a \
     backend-specific atomic-create primitive — extend copad_core::fs_atomic."
);

mod bookmark;
mod canonical;
mod frontmatter;
mod store;

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use serde_json::{Value, json};

use bookmark::Bookmark;

const PROTOCOL_VERSION: u32 = 1;

const PROVIDED_ACTIONS: &[&str] = &[
    "bookmark.add",
    "bookmark.list",
    "bookmark.show",
    "bookmark.delete",
];

fn main() {
    let bookmark = match Bookmark::from_env() {
        Ok(bm) => bm,
        Err(e) => {
            eprintln!("[bookmark] init failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[bookmark] root = {}", bookmark.root().display());

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    // Single writer thread so replies and notifications never interleave bytes, as in kb/todo.
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
                eprintln!("[bookmark] parse error: {e}");
                continue;
            }
        };
        handle_frame(&bookmark, &value, &writer_tx);
    }
}

fn handle_frame(bookmark: &Bookmark, frame: &Value, tx: &Sender<String>) {
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
                    &format!("bookmark plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": PROVIDED_ACTIONS,
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
            match bookmark.invoke(&action_name, &action_params) {
                Ok(result) => send_response(tx, id, result),
                Err((code, msg)) => send_error(tx, id, code, &msg),
            }
        }
        "event.dispatch" => {}
        "shutdown" => std::process::exit(0),
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                tx,
                id,
                "unknown_method",
                &format!("bookmark plugin: unknown method {other}"),
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
