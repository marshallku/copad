//! Pilot — autonomous goal-queue orchestration plugin (Phase 24, decision #51).
//!
//! Owns a persistent goal queue (the single source of truth) and drives
//! one interactive agent session at a time by shelling out to `csd`
//! (claude/codex session driver, `~/dev/csd`) — the same consume-don't-
//! reimplement pattern copad uses for `tmx`. The long drive loop lives in
//! a background thread ([`dispatcher`]); the `pilot.*` actions are quick
//! RPCs that enqueue or resolve gates.
//!
//! Actions:
//! - `pilot.add {cwd, instruction, posture?}` — enqueue a goal.
//! - `pilot.list` — the full queue (cockpit reads this; events are ephemeral).
//! - `pilot.cancel {id}` — cancel a goal and kill its session.
//! - `pilot.answer {id, text}` — answer a clarifying-question gate.
//! - `pilot.approve {id, option?}` — approve a plan / permission / trust gate.
//!
//! Activation `onStartup`: the dispatcher must be alive (and resume
//! crash-interrupted goals) whenever copad runs. Env knobs:
//! `COPAD_PILOT_QUEUE_FILE`, `COPAD_PILOT_CSD_BIN`,
//! `COPAD_PILOT_CSD_TIMEOUT_SECS`, `COPAD_PILOT_POLL_MS`,
//! `COPAD_PILOT_MAX_REPROMPTS`.

mod csd;
mod dispatcher;
mod queue;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use serde_json::{Value, json};

use csd::{Csd, CsdState};
use queue::{Status, Store};

const PROTOCOL_VERSION: u32 = 1;

const VALID_POSTURES: [&str; 5] = ["trust", "auto-accept", "bypass", "yolo", "default"];

fn main() {
    let queue_path = resolve_queue_path();
    eprintln!("[pilot] queue = {}", queue_path.display());
    let store = Arc::new(Mutex::new(Store::load(queue_path)));
    let csd = Csd::from_env();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
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
    let stop = Arc::new(AtomicBool::new(false));

    dispatcher::spawn(
        store.clone(),
        csd.clone(),
        tx.clone(),
        initialized.clone(),
        stop.clone(),
    );

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
                eprintln!("[pilot] parse error: {e}");
                continue;
            }
        };
        handle_frame(&frame, &writer_tx, &initialized, &store, &csd);
    }
}

fn handle_frame(
    frame: &Value,
    tx: &Sender<String>,
    initialized: &AtomicBool,
    store: &Arc<Mutex<Store>>,
    csd: &Csd,
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
                    &format!("pilot plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": [
                        "pilot.add",
                        "pilot.list",
                        "pilot.status",
                        "pilot.cancel",
                        "pilot.answer",
                        "pilot.approve",
                    ],
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
            let result = handle_action(&name, &action_params, store, csd, tx);
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
                &format!("pilot plugin: unknown method {other}"),
            );
        }
        _ => {}
    }
}

fn handle_action(
    name: &str,
    params: &Value,
    store: &Arc<Mutex<Store>>,
    csd: &Csd,
    tx: &Sender<String>,
) -> Result<Value, (String, String)> {
    match name {
        "pilot.add" => action_add(params, store, tx),
        "pilot.list" => action_list(store),
        "pilot.status" => action_status(store, csd),
        "pilot.cancel" => action_cancel(params, store, csd, tx),
        "pilot.answer" => action_answer(params, store, csd),
        "pilot.approve" => action_approve(params, store, csd),
        other => Err((
            "action_not_found".into(),
            format!("pilot plugin does not handle {other}"),
        )),
    }
}

fn action_add(
    params: &Value,
    store: &Arc<Mutex<Store>>,
    tx: &Sender<String>,
) -> Result<Value, (String, String)> {
    let cwd = required_string(params, "cwd")?;
    let instruction = required_string(params, "instruction")?;
    let posture = optional_string(params, "posture")?.unwrap_or_else(|| "trust".to_string());
    if !VALID_POSTURES.contains(&posture.as_str()) {
        return Err((
            "invalid_params".into(),
            format!("posture must be one of {VALID_POSTURES:?}, got {posture:?}"),
        ));
    }
    let dir = PathBuf::from(&cwd);
    if !dir.is_dir() {
        return Err((
            "invalid_params".into(),
            format!("cwd {cwd:?} is not an existing directory"),
        ));
    }
    let goal = store
        .lock()
        .unwrap()
        .add(cwd, instruction, posture)
        .map_err(|e| {
            (
                "io_error".to_string(),
                format!("goal not enqueued — could not persist the queue: {e}"),
            )
        })?;
    // Observable enqueue (Phase 24.4): lets the cockpit and chained
    // triggers react to a goal being added — including goals added BY a
    // trigger (`event_kind → pilot.add`), which is how event-driven
    // enqueue works (the TriggerEngine interpolates `{event.*}` into the
    // params; pilot just exposes the action). See triggers.example.toml.
    publish_event(
        tx,
        "pilot.goal_added",
        json!({ "id": goal.id, "cwd": goal.cwd, "instruction": goal.instruction }),
    );
    Ok(goal.to_json())
}

fn action_list(store: &Arc<Mutex<Store>>) -> Result<Value, (String, String)> {
    let goals: Vec<Value> = store
        .lock()
        .unwrap()
        .snapshot()
        .iter()
        .map(|g| g.to_json())
        .collect();
    Ok(json!({ "goals": goals }))
}

/// Cockpit read (Phase 24.3). The full queue + status counts + the active
/// goal and its current gate, plus a best-effort LIVE `csd state` for the
/// one active goal (sub-poll freshness + working/tools detail the persisted
/// status doesn't carry). Events are ephemeral, so a browser opened later
/// renders the live gate from this read. Cross-session observation
/// (`csd ps --json` + `tmx agents --json`) is aggregated by `web-bridge`
/// (Phase 24.6), not reimplemented here.
fn action_status(store: &Arc<Mutex<Store>>, csd: &Csd) -> Result<Value, (String, String)> {
    let goals = store.lock().unwrap().snapshot();

    // The dispatcher drives exactly one goal at a time: the first
    // non-terminal goal in insertion order.
    let active = goals.iter().find(|g| !g.status.is_terminal()).cloned();
    let active_id = active.as_ref().map(|g| g.id.clone());

    // One live `csd state` query, for the active goal only (bounded cost).
    let live = active
        .as_ref()
        .and_then(|g| g.csd_session.as_deref())
        .and_then(|session| csd.state_json(session).ok());

    let gate = active
        .as_ref()
        .filter(|g| g.status == Status::AwaitingGate)
        .and_then(|g| g.gate.as_ref())
        .map(|gate| serde_json::to_value(gate).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);

    let mut counts = serde_json::Map::new();
    for g in &goals {
        let key = serde_json::to_value(g.status)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "unknown".into());
        let n = counts.get(&key).and_then(Value::as_u64).unwrap_or(0);
        counts.insert(key, json!(n + 1));
    }

    let goal_jsons: Vec<Value> = goals
        .iter()
        .map(|g| {
            let mut v = g.to_json();
            if active_id.as_deref() == Some(g.id.as_str())
                && let Some(obj) = v.as_object_mut()
            {
                obj.insert("live".into(), live.clone().unwrap_or(Value::Null));
            }
            v
        })
        .collect();

    Ok(json!({
        "goals": goal_jsons,
        "active": active_id,
        "gate": gate,
        "counts": Value::Object(counts),
    }))
}

fn action_cancel(
    params: &Value,
    store: &Arc<Mutex<Store>>,
    csd: &Csd,
    tx: &Sender<String>,
) -> Result<Value, (String, String)> {
    let id = required_string(params, "id")?;
    // Distinguish "no such id" from "already terminal" for a useful error.
    let exists = store.lock().unwrap().get(&id).is_some();
    if !exists {
        return Err(("not_found".into(), format!("no goal with id {id:?}")));
    }
    let session = match store.lock().unwrap().cancel(&id) {
        Ok(s) => s,
        Err("not_found") => return Err(("not_found".into(), format!("no goal with id {id:?}"))),
        Err("io_error") => {
            return Err((
                "io_error".into(),
                format!("goal {id:?} not cancelled — could not persist the queue"),
            ));
        }
        Err(_) => {
            return Err((
                "invalid_state".into(),
                format!("goal {id:?} is already in a terminal state"),
            ));
        }
    };
    // Surface (don't swallow) the kill outcome: if `csd kill` fails the
    // interactive agent may linger, so the cockpit/caller can see it and
    // intervene. We still mark the goal cancelled — the queue shouldn't be
    // held hostage by a flaky kill — but a failed kill is reported, not lost.
    let session_kill = match &session {
        Some(name) => match csd.kill(name) {
            Ok(()) => json!("ok"),
            Err(e) => {
                eprintln!("[pilot] cancel: csd kill {name} failed (agent may linger): {e}");
                json!(format!("failed: {e}"))
            }
        },
        None => json!("no_session"),
    };
    publish_event(tx, "pilot.goal_cancelled", json!({ "id": id }));
    Ok(json!({ "id": id, "status": "cancelled", "session_kill": session_kill }))
}

fn action_answer(
    params: &Value,
    store: &Arc<Mutex<Store>>,
    csd: &Csd,
) -> Result<Value, (String, String)> {
    let id = required_string(params, "id")?;
    let text = required_string(params, "text")?;
    let (session, gate) = require_open_gate(store, &id, &["answer"])?;
    // Re-check live state and confirm it's the SAME question we recorded
    // (codex C6/C1) — a session that advanced to a different question
    // since the gate was recorded must reject the stale answer rather
    // than send it into the wrong turn.
    match csd.state(&session).map_err(csd_err)? {
        CsdState::AwaitingAnswer { question }
            if gate.prompt.as_deref().map(str::trim) == Some(question.trim()) => {}
        other => return Err(stale_gate(&id, &format!("{other:?}"))),
    }
    csd.send(&session, &text).map_err(csd_err)?;
    if !resume_after_gate(store, &id) {
        return Err((
            "io_error".into(),
            format!(
                "answer sent to {id:?} but the resume could not be persisted; it will recover on restart"
            ),
        ));
    }
    Ok(json!({ "id": id, "status": "running" }))
}

fn action_approve(
    params: &Value,
    store: &Arc<Mutex<Store>>,
    csd: &Csd,
) -> Result<Value, (String, String)> {
    let id = required_string(params, "id")?;
    let option = optional_u32(params, "option")?.unwrap_or(1);
    let (session, recorded) = require_open_gate(store, &id, &["plan", "trust", "permission"])?;
    // Reject a stale gate (codex C1/C2): the LIVE gate must match the
    // recorded kind AND its identifying content (plan file / prompt), so
    // an approval queued for one gate can't land on a different one the
    // session has since advanced to.
    match csd.state(&session).map_err(csd_err)? {
        CsdState::PlanReady { plan_file, plan } if recorded.kind == "plan" => {
            // Match BOTH the plan file path AND the plan text (stored in
            // the gate's `prompt`) so a reused/`None` file path can't let a
            // stale approval land on a different plan.
            if recorded.plan_file != plan_file {
                return Err(stale_gate(
                    &id,
                    "the live plan file differs from the recorded one",
                ));
            }
            if recorded.prompt.as_deref().map(str::trim) != plan.as_deref().map(str::trim) {
                return Err(stale_gate(
                    &id,
                    "the live plan differs from the recorded one",
                ));
            }
        }
        CsdState::Blocked { gate, prompt, .. }
            if recorded.kind == gate
                && recorded.prompt.as_deref().map(str::trim)
                    == prompt.as_deref().map(str::trim) => {}
        other => return Err(stale_gate(&id, &format!("{other:?}"))),
    }
    csd.approve(&session, option).map_err(csd_err)?;
    if !resume_after_gate(store, &id) {
        return Err((
            "io_error".into(),
            format!(
                "approval sent to {id:?} but the resume could not be persisted; it will recover on restart"
            ),
        ));
    }
    Ok(json!({ "id": id, "status": "running", "option": option }))
}

/// Validate that `id` is paused on a gate of one of `kinds`; return its
/// `(session, recorded gate)` so the caller can match it against the live
/// `csd state`.
fn require_open_gate(
    store: &Arc<Mutex<Store>>,
    id: &str,
    kinds: &[&str],
) -> Result<(String, queue::Gate), (String, String)> {
    let guard = store.lock().unwrap();
    let goal = guard
        .get(id)
        .ok_or_else(|| ("not_found".to_string(), format!("no goal with id {id:?}")))?;
    if goal.status != Status::AwaitingGate {
        return Err((
            "invalid_state".into(),
            format!(
                "goal {id:?} is not waiting on a gate (status {:?})",
                goal.status
            ),
        ));
    }
    let gate = goal.gate.as_ref().ok_or_else(|| {
        (
            "invalid_state".to_string(),
            format!("goal {id:?} is AwaitingGate but has no gate recorded"),
        )
    })?;
    if !kinds.contains(&gate.kind.as_str()) {
        return Err((
            "invalid_state".into(),
            format!(
                "goal {id:?} gate is {:?}, expected one of {kinds:?}",
                gate.kind
            ),
        ));
    }
    let session = goal.csd_session.clone().ok_or_else(|| {
        (
            "invalid_state".to_string(),
            format!("goal {id:?} has no session"),
        )
    })?;
    Ok((session, gate.clone()))
}

/// Clear the gate and resume polling. Returns `false` if the transition
/// couldn't be persisted (rolled back) — the caller must surface that
/// rather than report success, since the `csd send`/`approve` already
/// landed and only a restart's `recover()` will reconcile it.
fn resume_after_gate(store: &Arc<Mutex<Store>>, id: &str) -> bool {
    store
        .lock()
        .unwrap()
        .update_if(id, &[Status::AwaitingGate], |g| {
            g.status = Status::Running;
            g.gate = None;
        })
}

fn stale_gate(id: &str, observed: &str) -> (String, String) {
    (
        "stale_gate".into(),
        format!("goal {id:?} gate is no longer open (csd now reports {observed})"),
    )
}

fn csd_err(e: csd::CsdError) -> (String, String) {
    ("csd_error".into(), e.to_string())
}

fn resolve_queue_path() -> PathBuf {
    if let Ok(p) = std::env::var("COPAD_PILOT_QUEUE_FILE") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("copad").join("pilot").join("queue.json")
}

fn required_string(params: &Value, key: &str) -> Result<String, (String, String)> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            (
                "invalid_params".into(),
                format!("missing or empty required field {key:?}"),
            )
        })
}

fn optional_string(params: &Value, key: &str) -> Result<Option<String>, (String, String)> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err((
            "invalid_params".into(),
            format!("{key:?} must be a string, got {other}"),
        )),
    }
}

fn optional_u32(params: &Value, key: &str) -> Result<Option<u32>, (String, String)> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| {
                (
                    "invalid_params".into(),
                    format!("{key:?} must be a non-negative integer, got {v}"),
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
        "error": { "code": code, "message": message },
    });
    let _ = tx.send(frame.to_string());
}

fn publish_event(tx: &Sender<String>, kind: &str, payload: Value) {
    let frame = json!({
        "method": "event.publish",
        "params": { "kind": kind, "payload": payload },
    });
    let _ = tx.send(frame.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture() -> (tempfile::TempDir, Arc<Mutex<Store>>) {
        let dir = tempdir().unwrap();
        let store = Arc::new(Mutex::new(Store::load(dir.path().join("queue.json"))));
        (dir, store)
    }

    // rx dropped immediately — publish_event sends are best-effort.
    fn null_tx() -> Sender<String> {
        channel::<String>().0
    }

    #[test]
    fn add_requires_existing_cwd() {
        let (dir, store) = fixture();
        let err = action_add(
            &json!({ "cwd": "/no/such/dir/xyz", "instruction": "do it" }),
            &store,
            &null_tx(),
        )
        .unwrap_err();
        assert_eq!(err.0, "invalid_params");

        // A real dir succeeds and lands as Queued.
        let ok = action_add(
            &json!({ "cwd": dir.path().to_str().unwrap(), "instruction": "do it" }),
            &store,
            &null_tx(),
        )
        .unwrap();
        assert_eq!(ok["status"], "queued");
        assert_eq!(ok["posture"], "trust");
    }

    #[test]
    fn add_rejects_unknown_posture() {
        let (dir, store) = fixture();
        let err = action_add(
            &json!({ "cwd": dir.path().to_str().unwrap(), "instruction": "x", "posture": "wild" }),
            &store,
            &null_tx(),
        )
        .unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn list_reflects_added_goals() {
        let (dir, store) = fixture();
        action_add(
            &json!({ "cwd": dir.path().to_str().unwrap(), "instruction": "a" }),
            &store,
            &null_tx(),
        )
        .unwrap();
        let listed = action_list(&store).unwrap();
        assert_eq!(listed["goals"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn add_publishes_goal_added_event() {
        // Phase 24.4: an enqueue (incl. a trigger-driven `event_kind →
        // pilot.add`) is observable on the bus so the cockpit / chained
        // triggers can react.
        let (dir, store) = fixture();
        let (tx, rx) = channel::<String>();
        let ok = action_add(
            &json!({ "cwd": dir.path().to_str().unwrap(), "instruction": "triage CI" }),
            &store,
            &tx,
        )
        .unwrap();
        let frame: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(frame["method"], "event.publish");
        assert_eq!(frame["params"]["kind"], "pilot.goal_added");
        assert_eq!(frame["params"]["payload"]["id"], ok["id"]);
        assert_eq!(frame["params"]["payload"]["instruction"], "triage CI");
    }

    #[test]
    fn status_reports_counts_and_active() {
        let (dir, store) = fixture();
        let csd = Csd::from_env();
        let a = action_add(
            &json!({ "cwd": dir.path().to_str().unwrap(), "instruction": "a" }),
            &store,
            &null_tx(),
        )
        .unwrap();
        action_add(
            &json!({ "cwd": dir.path().to_str().unwrap(), "instruction": "b" }),
            &store,
            &null_tx(),
        )
        .unwrap();
        // First goal done; the second (Queued, no session → no csd shell-out) is active.
        store
            .lock()
            .unwrap()
            .update_if(a["id"].as_str().unwrap(), &[], |g| g.status = Status::Done);
        let st = action_status(&store, &csd).unwrap();
        assert_eq!(st["goals"].as_array().unwrap().len(), 2);
        assert_eq!(st["counts"]["done"], 1);
        assert_eq!(st["counts"]["queued"], 1);
        assert!(st["active"].is_string());
        assert_ne!(st["active"].as_str().unwrap(), a["id"].as_str().unwrap());
        assert_eq!(st["gate"], Value::Null);
    }

    #[test]
    fn status_surfaces_the_active_gate() {
        let (dir, store) = fixture();
        let csd = Csd::from_env();
        let added = action_add(
            &json!({ "cwd": dir.path().to_str().unwrap(), "instruction": "a" }),
            &store,
            &null_tx(),
        )
        .unwrap();
        let id = added["id"].as_str().unwrap().to_string();
        // AwaitingGate but no session → action_status skips the live query.
        store.lock().unwrap().update_if(&id, &[], |g| {
            g.status = Status::AwaitingGate;
            g.gate = Some(queue::Gate {
                kind: "answer".into(),
                prompt: Some("which file?".into()),
                options: None,
                plan_file: None,
            });
        });
        let st = action_status(&store, &csd).unwrap();
        assert_eq!(st["active"].as_str().unwrap(), id);
        assert_eq!(st["gate"]["kind"], "answer");
        assert_eq!(st["gate"]["prompt"], "which file?");
        assert_eq!(st["counts"]["awaiting_gate"], 1);
    }

    #[test]
    fn cancel_unknown_id_is_not_found() {
        let (_dir, store) = fixture();
        let csd = Csd::from_env();
        let (tx, _rx) = channel::<String>();
        let err = action_cancel(&json!({ "id": "nope" }), &store, &csd, &tx).unwrap_err();
        assert_eq!(err.0, "not_found");
    }

    #[test]
    fn answer_rejects_goal_not_on_gate() {
        let (dir, store) = fixture();
        let csd = Csd::from_env();
        let added = action_add(
            &json!({ "cwd": dir.path().to_str().unwrap(), "instruction": "a" }),
            &store,
            &null_tx(),
        )
        .unwrap();
        let id = added["id"].as_str().unwrap();
        // Queued, not AwaitingGate → invalid_state before any csd call.
        let err = action_answer(&json!({ "id": id, "text": "hi" }), &store, &csd).unwrap_err();
        assert_eq!(err.0, "invalid_state");
    }

    #[test]
    fn require_open_gate_checks_kind() {
        let (dir, store) = fixture();
        let added = action_add(
            &json!({ "cwd": dir.path().to_str().unwrap(), "instruction": "a" }),
            &store,
            &null_tx(),
        )
        .unwrap();
        let id = added["id"].as_str().unwrap().to_string();
        store.lock().unwrap().update_if(&id, &[], |g| {
            g.status = Status::AwaitingGate;
            g.csd_session = Some(id.clone());
            g.gate = Some(queue::Gate {
                kind: "answer".into(),
                prompt: Some("?".into()),
                options: None,
                plan_file: None,
            });
        });
        // approve expects plan/trust/permission, gate is "answer".
        let err = require_open_gate(&store, &id, &["plan", "trust", "permission"]).unwrap_err();
        assert_eq!(err.0, "invalid_state");
        // answer accepts it.
        let (session, gate) = require_open_gate(&store, &id, &["answer"]).unwrap();
        assert_eq!(session, id);
        assert_eq!(gate.kind, "answer");
    }
}
