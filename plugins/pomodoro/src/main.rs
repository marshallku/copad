//! Pomodoro service plugin for copad.
//!
//! A standalone focus timer that renders in the status bar. It is
//! DELIBERATELY DECOUPLED from the todo plugin: it never imports or
//! subscribes to todo. The "start a pomodoro when I start a todo" flow is
//! wired entirely through copad's `[[triggers]]` engine — `todo.start`
//! emits `todo.start_requested`, and a user trigger maps that to
//! `pomodoro.start` with `label = "{event.title}"` (see
//! `triggers.example.toml`). Any event can drive the timer that way; the
//! plugin only ever sees "start with this label."
//!
//! State model lives in `state.rs` (pure, clock-injected). This file owns
//! the wire protocol, the single mutex + condvar, and the one scheduler
//! thread that reads the real clock, fires phase-completion toasts (via a
//! best-effort `action.invoke` → `notify.show`), and emits `pomodoro.state`
//! events on every transition.

mod persist;
mod state;

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use state::{Durations, PomodoroState};

const PROTOCOL_VERSION: u32 = 1;

const PROVIDES: &[&str] = &[
    "pomodoro.start",
    "pomodoro.pause",
    "pomodoro.resume",
    "pomodoro.toggle",
    "pomodoro.reset",
    "pomodoro.status",
    "pomodoro.set_durations",
];

/// Never wait longer than this on the condvar, so a backwards/forwards wall
/// clock jump can strand a pending expiry for at most this long.
const MAX_WAIT_MS: u64 = 3_600_000;

type Shared = Arc<(Mutex<PomodoroState>, Condvar)>;

fn main() {
    let stdout = std::io::stdout();
    let (tx, rx) = channel::<String>();

    // Single writer so the scheduler thread and the request handler can't
    // interleave bytes mid-line on stdout.
    thread::spawn(move || {
        let mut out = stdout.lock();
        for line in rx.iter() {
            use std::io::Write;
            if writeln!(out, "{line}").is_err() || out.flush().is_err() {
                break;
            }
        }
    });

    let durations = persist::load_durations();
    let shared: Shared = Arc::new((Mutex::new(PomodoroState::new(durations)), Condvar::new()));
    let initialized = Arc::new(AtomicBool::new(false));
    let notify_seq = Arc::new(AtomicU64::new(0));

    // Scheduler: the only clock reader. Parks on the condvar until an RPC
    // mutates state or the active phase is due.
    {
        let shared = shared.clone();
        let tx = tx.clone();
        let initialized = initialized.clone();
        let notify_seq = notify_seq.clone();
        thread::Builder::new()
            .name("pomodoro-scheduler".into())
            .spawn(move || run_scheduler(shared, tx, initialized, notify_seq))
            .expect("spawn pomodoro scheduler");
    }

    let reader = BufReader::new(std::io::stdin().lock());
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
                eprintln!("[pomodoro] parse error: {e}");
                continue;
            }
        };
        handle_frame(&value, &tx, &shared, &initialized, &notify_seq);
    }
}

fn handle_frame(
    value: &Value,
    tx: &Sender<String>,
    shared: &Shared,
    initialized: &AtomicBool,
    notify_seq: &AtomicU64,
) {
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    let params = value.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let proto = params.get("protocol_version").and_then(Value::as_u64);
            if proto != Some(PROTOCOL_VERSION as u64) {
                send_error(
                    tx,
                    id,
                    "protocol_mismatch",
                    &format!("pomodoro plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": PROVIDES,
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
            match dispatch_action(name, &action_params, tx, shared, initialized, notify_seq) {
                Ok(result) => send_response(tx, id, result),
                Err((code, message)) => send_error(tx, id, &code, &message),
            }
        }
        "event.dispatch" => {
            // We subscribe to nothing (decoupled by design); ignore.
        }
        "shutdown" => {
            std::process::exit(0);
        }
        // A Response frame from our own notify.show action.invoke has no
        // `method`; ignore it. Only unknown *requests* (method + id) error.
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                tx,
                id,
                "unknown_method",
                &format!("pomodoro plugin: unknown method {other}"),
            );
        }
        _ => {}
    }
}

/// Apply a `pomodoro.*` action. Mutating actions emit a `pomodoro.state`
/// event and wake the scheduler; all actions reply with the current state
/// snapshot. Returns `Err((code, message))` on bad params — with no partial
/// mutation.
fn dispatch_action(
    name: &str,
    params: &Value,
    tx: &Sender<String>,
    shared: &Shared,
    initialized: &AtomicBool,
    notify_seq: &AtomicU64,
) -> Result<Value, (String, String)> {
    let (lock, cvar) = &**shared;

    // `status` is read-only; everything else mutates then notifies. In every
    // case sample `now` AFTER acquiring the lock: if we read the clock first
    // and then block while the scheduler transitions Work→Break under the
    // lock, we'd apply a pre-transition timestamp to the post-transition
    // phase (e.g. `pause` freezing the new break with more than its length).
    if name == "pomodoro.status" {
        let guard = lock.lock().unwrap();
        let now = now_ms();
        return Ok(guard.event_payload(now));
    }

    let mut guard = lock.lock().unwrap();
    let now = now_ms();
    match name {
        "pomodoro.start" => {
            let minutes = match params.get("minutes") {
                None | Some(Value::Null) => None,
                Some(v) => Some(parse_minutes(v)?),
            };
            let label = params.get("label").and_then(Value::as_str);
            guard
                .start(now, minutes, label)
                .map_err(|e| ("invalid_params".to_string(), e))?;
        }
        "pomodoro.pause" => {
            guard.pause(now);
        }
        "pomodoro.resume" => {
            guard.resume(now);
        }
        "pomodoro.toggle" => {
            guard.toggle(now);
        }
        "pomodoro.reset" => {
            guard.reset();
        }
        "pomodoro.set_durations" => {
            let cur = guard.durations();
            let next = Durations::validated(
                merge_min(params, "work_min", cur.work_min)?,
                merge_min(params, "break_min", cur.break_min)?,
                merge_min(params, "long_break_min", cur.long_break_min)?,
                merge_min(params, "rounds_before_long", cur.rounds_before_long)?,
            )
            .map_err(|e| ("invalid_params".to_string(), e))?;
            // Persist FIRST, apply only on success: returning ok while the
            // write failed would leave runtime disagreeing with what a
            // restart loads (AC#4). Done under the lock so a concurrent set
            // can't interleave read-cur / save / apply; the write is a tiny
            // atomic temp+rename on a rare (config-change) path.
            persist::save_durations(&next).map_err(|e| {
                (
                    "io_error".to_string(),
                    format!("failed to persist durations: {e}"),
                )
            })?;
            guard.set_durations(next);
        }
        other => {
            return Err((
                "action_not_found".to_string(),
                format!("pomodoro plugin does not handle {other}"),
            ));
        }
    }

    // Emit while still holding the lock so state events are serialized in
    // the same order as the mutations that produced them — otherwise a
    // concurrent scheduler transition could publish a stale payload after
    // this newer one. `emit_state` is just an mpsc push (the writer thread
    // never touches this lock), so holding it here can't deadlock or block.
    let payload = guard.event_payload(now);
    emit_state(tx, initialized, &payload);
    drop(guard);
    cvar.notify_all();
    let _ = notify_seq; // toasts are the scheduler's job
    Ok(payload)
}

/// The scheduler thread. Owns wall-clock reads; fires toasts and state
/// events on phase completion. One sleeper, so there is no orphaned-timer
/// cancellation problem — it re-derives everything from locked state on
/// every wake.
fn run_scheduler(
    shared: Shared,
    tx: Sender<String>,
    initialized: Arc<AtomicBool>,
    notify_seq: Arc<AtomicU64>,
) {
    let (lock, cvar) = &*shared;
    loop {
        let mut guard = lock.lock().unwrap();
        let now = now_ms();
        if let Some(trans) = guard.tick_if_due(now) {
            // Publish the STATE event under the lock so it's ordered with the
            // RPC path's emits (a concurrent start/reset can't slip a newer
            // payload ahead of this one). The toast, by contrast, is
            // best-effort and order-independent, so it goes AFTER the unlock
            // — keeping the "never invoke notify while holding the lock" rule
            // the docs promise. (Both are non-blocking mpsc pushes regardless;
            // the actual notify.show runs in the daemon and we never await it.)
            let payload = guard.event_payload(now);
            emit_state(&tx, &initialized, &payload);
            drop(guard);
            fire_notify(&tx, &notify_seq, &trans.toast_title, &trans.toast_body);
            continue;
        }
        match guard.next_wake_ms() {
            Some(deadline) => {
                let now2 = now_ms();
                if deadline <= now2 {
                    drop(guard);
                    continue;
                }
                let wait = (deadline - now2).clamp(1, MAX_WAIT_MS);
                let (_g, _timeout) = cvar
                    .wait_timeout(guard, Duration::from_millis(wait))
                    .unwrap();
            }
            None => {
                let _g = cvar.wait(guard).unwrap();
            }
        }
    }
}

fn emit_state(tx: &Sender<String>, initialized: &AtomicBool, payload: &Value) {
    if !initialized.load(Ordering::SeqCst) {
        return;
    }
    let frame = json!({
        "method": "event.publish",
        "params": { "kind": "pomodoro.state", "payload": payload }
    });
    let _ = tx.send(frame.to_string());
}

fn fire_notify(tx: &Sender<String>, seq: &AtomicU64, title: &str, body: &str) {
    let id = format!("pomodoro-notify-{}", seq.fetch_add(1, Ordering::Relaxed));
    let frame = json!({
        "id": id,
        "method": "action.invoke",
        "params": {
            "name": "notify.show",
            "params": { "title": title, "body": body, "level": "info" }
        }
    });
    let _ = tx.send(frame.to_string());
}

fn parse_minutes(v: &Value) -> Result<u32, (String, String)> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            (
                "invalid_params".to_string(),
                "minutes must be a non-negative integer".to_string(),
            )
        })
}

/// Read an optional integer override for a duration field, falling back to
/// the current value when absent/null.
fn merge_min(params: &Value, field: &str, current: u32) -> Result<u32, (String, String)> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(current),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| {
                (
                    "invalid_params".to_string(),
                    format!("{field} must be a non-negative integer"),
                )
            }),
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
