//! First-party bookmark service plugin for copad.
//!
//! Captures URLs into `~/docs/bookmarks/YYYY-MM/<urlhash8>-<slug>.md`,
//! one file per bookmark, frontmatter-headed Markdown. Filesystem is
//! the source of truth — there is NO on-disk index. `bookmark.list`
//! re-derives by walking the tree, mirroring the kb/todo plugin
//! pattern. Vim/git-edit safe.
//!
//! BM-1 surface (this crate, today):
//! - `bookmark.add`     — canonicalize URL, dedup by urlhash8, write
//!   queued stub (no fetch yet — BM-2 adds the worker).
//! - `bookmark.list`    — walk tree, optional filters.
//! - `bookmark.show`    — read by id-prefix or url.
//! - `bookmark.delete`  — unlink by id-prefix or url.
//!
//! Out of scope for BM-1 (see roadmap):
//! - BM-2: async fetch + readability extraction + status transitions.
//! - BM-3: keyword-based linker writing `linked_kb` frontmatter.
//! - BM-4: HTML panel.
//! - BM-5: harness `/bookmark` slash skill + offline inbox drain.
//!
//! Unix-only (Linux + macOS); path-safety primitives mirror
//! `copad-plugin-kb` (canonicalize root + re-validate every resolved
//! path). The atomic-create-or-fail rename routes through
//! `copad_core::fs_atomic` so the per-OS syscall lives in one place.

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

    // Single writer thread so action replies and (future) notifications
    // never interleave bytes — same shape as kb/todo.
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
        "event.dispatch" => {
            // bookmark plugin doesn't subscribe to anything in BM-1.
        }
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
