//! Background dispatcher thread. NOT an action handler — service-plugin
//! action calls are bounded by the supervisor's ~120s timeout (codex
//! C5), so the long-running drive loop lives here and the `pilot.*`
//! RPCs only enqueue/signal.
//!
//! Drives ONE goal at a time (decision #51): the first non-terminal goal
//! in the queue is taken from `Queued` → spawn a per-goal `csd` session
//! (name = goal id) → send the instruction plus a per-goal completion
//! sentinel → poll `csd state` until `idle_done` carries the sentinel
//! (done), a gate appears (pause the queue), or `csd` errors (fail). A
//! gate blocks the whole queue until `pilot.answer` / `pilot.approve`
//! resolves it — sequential by design.
//!
//! Crash idempotency (codex C4): `Status` is persisted before each
//! irreversible `csd` step, so [`recover`] can resume a goal caught
//! mid-spawn or mid-send on restart without blindly re-sending.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::csd::{Csd, CsdState};
use crate::queue::{Goal, Status, Store, detect_completion};

pub fn spawn(
    store: Arc<Mutex<Store>>,
    csd: Csd,
    tx: Sender<String>,
    initialized: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let poll = Duration::from_millis(
        std::env::var("COPAD_PILOT_POLL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3000),
    );
    let max_reprompts: u32 = std::env::var("COPAD_PILOT_MAX_REPROMPTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    thread::spawn(move || {
        // Hold off on driving (and publishing) until the host handshake
        // completes — same gate the echo plugin uses for heartbeats.
        while !initialized.load(Ordering::SeqCst) {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }

        recover(&store, &csd, &tx);

        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let next = store.lock().unwrap().claim_next();
            match next {
                None => {}
                Some(goal) => match goal.status {
                    Status::Queued => start_goal(&store, &csd, &tx, &goal),
                    Status::Running => poll_goal(&store, &csd, &tx, &goal, max_reprompts),
                    // AwaitingGate pauses the queue; recover() already
                    // resolved Spawning/Sending. Anything else is terminal
                    // and claim_next wouldn't have returned it.
                    _ => {}
                },
            }
            thread::sleep(poll);
        }
    });
}

/// Resume goals caught mid-step by a restart (codex C4).
fn recover(store: &Arc<Mutex<Store>>, csd: &Csd, tx: &Sender<String>) {
    for goal in store.lock().unwrap().snapshot() {
        if matches!(
            goal.status,
            Status::Spawning | Status::Sending | Status::AwaitingGate
        ) {
            eprintln!("[pilot] recovering goal {} from {:?}", goal.id, goal.status);
        }
        match goal.status {
            // Spawn was never confirmed sent-to — kill any orphan session
            // and requeue for a clean start. No instruction landed yet, so
            // requeue can't duplicate work.
            Status::Spawning => {
                let _ = csd.kill(&goal.id);
                store
                    .lock()
                    .unwrap()
                    .update_if(&goal.id, &[Status::Spawning], |g| {
                        g.status = Status::Queued;
                        g.csd_session = None;
                        g.session_id = None;
                        g.nonce = None;
                    });
            }
            // Session exists; the send may or may not have landed. Ask csd:
            // a turn present means it landed (→ poll); only `spawning` (no
            // turn at all) re-sends.
            Status::Sending => recover_sending(store, csd, tx, &goal),
            // A gate-resume (`pilot.answer` / `pilot.approve`) sends to
            // csd and only then persists `Running`. A crash in that window
            // reloads here as `AwaitingGate`, which the loop never drives —
            // reconcile against live state so it can't deadlock: advanced
            // past the gate → resume polling; still/newly gated → refresh
            // the recorded gate so a retry matches reality.
            Status::AwaitingGate => reconcile_gate(store, csd, tx, &goal),
            _ => {}
        }
    }
}

fn reconcile_gate(store: &Arc<Mutex<Store>>, csd: &Csd, tx: &Sender<String>, goal: &Goal) {
    let id = &goal.id;
    match csd.state(id) {
        // Transient read failure — leave it gated. The `pilot.answer` /
        // `pilot.approve` path re-checks live state itself, so the user
        // can still resolve it; we just don't auto-advance here.
        Err(e) => eprintln!("[pilot] gate recovery: csd state failed for {id}: {e}"),
        Ok(CsdState::Dead) => fail(store, tx, id, "csd session died while gated"),
        Ok(CsdState::AwaitingAnswer { question }) => {
            refresh_gate(store, id, make_gate("answer", Some(question), None, None));
        }
        Ok(CsdState::PlanReady { plan_file, plan }) => {
            refresh_gate(store, id, make_gate("plan", plan, None, plan_file));
        }
        Ok(CsdState::Blocked {
            gate,
            prompt,
            options,
        }) => {
            refresh_gate(store, id, make_gate(&gate, prompt, options, None));
        }
        // No longer gated — the resume landed (or the session advanced).
        // Hand back to the poll loop, which re-evaluates IdleDone etc.
        Ok(CsdState::Spawning)
        | Ok(CsdState::Working)
        | Ok(CsdState::Unknown)
        | Ok(CsdState::IdleDone { .. }) => {
            store
                .lock()
                .unwrap()
                .update_if(id, &[Status::AwaitingGate], |g| {
                    g.status = Status::Running;
                    g.gate = None;
                });
        }
    }
}

fn refresh_gate(store: &Arc<Mutex<Store>>, id: &str, gate: crate::queue::Gate) {
    store
        .lock()
        .unwrap()
        .update_if(id, &[Status::AwaitingGate], |g| g.gate = Some(gate));
}

fn recover_sending(store: &Arc<Mutex<Store>>, csd: &Csd, tx: &Sender<String>, goal: &Goal) {
    let Some(nonce) = goal.nonce.clone() else {
        fail(store, tx, &goal.id, "missing nonce on recovery");
        return;
    };
    match csd.state(&goal.id) {
        Ok(CsdState::Spawning) => {
            let prompt = build_prompt(&goal.instruction, &goal.id, &nonce);
            match csd.send(&goal.id, &prompt) {
                Ok(()) => {
                    store
                        .lock()
                        .unwrap()
                        .update_if(&goal.id, &[Status::Sending], |g| g.status = Status::Running);
                }
                Err(e) => fail(store, tx, &goal.id, &e.to_string()),
            }
        }
        Ok(CsdState::Dead) | Err(_) => {
            let _ = csd.kill(&goal.id);
            store
                .lock()
                .unwrap()
                .update_if(&goal.id, &[Status::Sending], |g| {
                    g.status = Status::Queued;
                    g.csd_session = None;
                    g.session_id = None;
                    g.nonce = None;
                });
        }
        Ok(_) => {
            store
                .lock()
                .unwrap()
                .update_if(&goal.id, &[Status::Sending], |g| g.status = Status::Running);
        }
    }
}

fn start_goal(store: &Arc<Mutex<Store>>, csd: &Csd, tx: &Sender<String>, goal: &Goal) {
    let nonce = crate::queue::gen_nonce();
    let id = goal.id.clone();

    // Persist Spawning BEFORE the spawn (C4). If a concurrent cancel
    // already moved the goal off Queued, the precondition fails and we
    // abort without touching csd.
    let claimed = store
        .lock()
        .unwrap()
        .update_if(&id, &[Status::Queued], |g| {
            g.status = Status::Spawning;
            g.csd_session = Some(id.clone());
            g.nonce = Some(nonce.clone());
        });
    if !claimed {
        return;
    }

    let info = match csd.spawn(&goal.cwd, &id, &goal.posture) {
        Ok(info) => info,
        Err(e) => {
            fail(store, tx, &id, &e.to_string());
            return;
        }
    };
    if let Some(warning) = &info.marker_warning {
        warn_marker_drift(tx, &id, warning);
    }

    // A cancel could have landed during the spawn — if the goal is no
    // longer Spawning, the session is now an orphan; kill it.
    let ok = store
        .lock()
        .unwrap()
        .update_if(&id, &[Status::Spawning], |g| {
            g.session_id = info.session_id.clone();
            g.jsonl_path = info.jsonl_path.clone();
        });
    if !ok {
        let _ = csd.kill(&id);
        return;
    }

    // Persist Sending BEFORE the send (C4).
    if !store
        .lock()
        .unwrap()
        .update_if(&id, &[Status::Spawning], |g| g.status = Status::Sending)
    {
        let _ = csd.kill(&id);
        return;
    }

    let prompt = build_prompt(&goal.instruction, &id, &nonce);
    match csd.send(&id, &prompt) {
        Ok(()) => {
            let promoted = store
                .lock()
                .unwrap()
                .update_if(&id, &[Status::Sending], |g| g.status = Status::Running);
            if promoted {
                publish(
                    tx,
                    "pilot.goal_started",
                    json!({ "id": id, "cwd": goal.cwd }),
                );
            } else {
                let _ = csd.kill(&id);
            }
        }
        Err(e) => fail(store, tx, &id, &e.to_string()),
    }
}

fn poll_goal(
    store: &Arc<Mutex<Store>>,
    csd: &Csd,
    tx: &Sender<String>,
    goal: &Goal,
    max_reprompts: u32,
) {
    let id = goal.id.clone();
    let nonce = goal.nonce.clone().unwrap_or_default();

    let state = match csd.state(&id) {
        Ok(s) => s,
        Err(e) => {
            fail(store, tx, &id, &e.to_string());
            return;
        }
    };

    match state {
        CsdState::Spawning | CsdState::Working | CsdState::Unknown => {}
        CsdState::IdleDone { text } => {
            if !nonce.is_empty() && detect_completion(&text, &id, &nonce) {
                // Finalize only if the Done write reached disk (codex). On a
                // rolled-back persist the session stays alive and the next
                // poll retries — don't kill or announce completion against a
                // cursor a restart can't see.
                if store
                    .lock()
                    .unwrap()
                    .update_if(&id, &[Status::Running], |g| g.status = Status::Done)
                {
                    let _ = csd.kill(&id);
                    publish(tx, "pilot.goal_completed", json!({ "id": id }));
                }
            } else if goal.reprompts < max_reprompts {
                // Persist the bumped counter BEFORE re-prompting; skip the
                // send if it didn't persist so a crash can't lose the count
                // and re-prompt unboundedly. Accepted limitation: a crash in
                // the window between this persist and the send below can cost
                // one nudge (the goal may stall one re-prompt early) — the
                // re-prompt budget is a soft heuristic, not a durability
                // invariant, so we don't carry a dedicated cursor for it.
                if store
                    .lock()
                    .unwrap()
                    .update_if(&id, &[Status::Running], |g| g.reprompts += 1)
                {
                    let cont = continue_prompt(&id, &nonce);
                    if let Err(e) = csd.send(&id, &cont) {
                        fail(store, tx, &id, &e.to_string());
                    }
                }
            } else if store
                .lock()
                .unwrap()
                .update_if(&id, &[Status::Running], |g| g.status = Status::Stalled)
            {
                publish(tx, "pilot.goal_stalled", json!({ "id": id }));
            }
        }
        CsdState::AwaitingAnswer { question } => {
            set_gate(store, tx, &id, "answer", Some(question), None, None);
        }
        CsdState::PlanReady { plan_file, plan } => {
            set_gate(store, tx, &id, "plan", plan, None, plan_file);
        }
        CsdState::Blocked {
            gate,
            prompt,
            options,
        } => {
            set_gate(store, tx, &id, &gate, prompt, options, None);
        }
        CsdState::Dead => fail(store, tx, &id, "csd session died unexpectedly"),
    }
}

fn set_gate(
    store: &Arc<Mutex<Store>>,
    tx: &Sender<String>,
    id: &str,
    kind: &str,
    prompt: Option<String>,
    options: Option<String>,
    plan_file: Option<String>,
) {
    let gate = make_gate(kind, prompt, options, plan_file);
    let gate_json = serde_json::to_value(&gate).unwrap_or(Value::Null);
    let applied = store
        .lock()
        .unwrap()
        .update_if(id, &[Status::Running], |g| {
            g.status = Status::AwaitingGate;
            g.gate = Some(gate);
        });
    if applied {
        publish(
            tx,
            "pilot.goal_blocked",
            json!({ "id": id, "gate": gate_json }),
        );
    }
}

fn fail(store: &Arc<Mutex<Store>>, tx: &Sender<String>, id: &str, message: &str) {
    // `AwaitingGate` is failable too: a gated session that died during
    // downtime is surfaced by `reconcile_gate` → `fail`, and without it
    // here the goal would stay gated forever and block the queue.
    let applied = store.lock().unwrap().update_if(
        id,
        &[
            Status::Spawning,
            Status::Sending,
            Status::Running,
            Status::AwaitingGate,
        ],
        |g| {
            g.status = Status::Failed;
            g.error = Some(message.to_string());
            g.gate = None;
        },
    );
    if applied {
        eprintln!("[pilot] goal {id} failed: {message}");
        publish(
            tx,
            "pilot.goal_failed",
            json!({ "id": id, "error": message }),
        );
    }
}

fn make_gate(
    kind: &str,
    prompt: Option<String>,
    options: Option<String>,
    plan_file: Option<String>,
) -> crate::queue::Gate {
    crate::queue::Gate {
        kind: kind.to_string(),
        prompt,
        options: options.and_then(|o| serde_json::from_str(&o).ok()),
        plan_file,
    }
}

fn build_prompt(instruction: &str, id: &str, nonce: &str) -> String {
    format!(
        "{instruction}\n\nWhen you have fully and successfully completed everything above, \
         write this exact line by itself as the very last line of your reply, then stop:\n\
         DONE:{id}:{nonce}"
    )
}

fn continue_prompt(id: &str, nonce: &str) -> String {
    format!(
        "Continue until the goal is fully done. When finished, write this exact line by itself \
         as the very last line of your reply, then stop:\nDONE:{id}:{nonce}"
    )
}

/// Surface csd's marker version guard as a `pilot.marker_warning` event,
/// once per distinct warning text per process — an unattended fleet on a
/// drifted claude release would otherwise repeat it on every goal.
fn warn_marker_drift(tx: &Sender<String>, id: &str, warning: &str) {
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    if seen.lock().unwrap().insert(warning.to_string()) {
        eprintln!("[pilot] goal {id}: {warning}");
        publish(
            tx,
            "pilot.marker_warning",
            json!({ "id": id, "warning": warning }),
        );
    }
}

fn publish(tx: &Sender<String>, kind: &str, payload: Value) {
    let frame = json!({
        "method": "event.publish",
        "params": { "kind": kind, "payload": payload },
    });
    let _ = tx.send(frame.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_embeds_sentinel_on_its_own_final_line() {
        let p = build_prompt("do X", "g-1", "abc");
        let last = p.lines().last().unwrap();
        assert_eq!(last, "DONE:g-1:abc");
        assert!(p.starts_with("do X"));
    }

    #[test]
    fn continue_prompt_carries_same_sentinel() {
        let p = continue_prompt("g-1", "abc");
        assert_eq!(p.lines().last().unwrap(), "DONE:g-1:abc");
    }
}
