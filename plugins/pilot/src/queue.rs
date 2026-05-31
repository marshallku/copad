//! Persistent goal queue — the single source of truth for the pilot
//! plugin (decision #51). One JSON file under
//! `$XDG_STATE_HOME/copad/pilot/queue.json` (override with
//! `COPAD_PILOT_QUEUE_FILE`), rewritten atomically (temp + rename) on
//! every mutation so a crash mid-write can't corrupt the queue.
//!
//! `Status` doubles as the dispatcher's crash-recovery cursor: it's
//! persisted *before* each irreversible `csd` step (`Spawning` before
//! spawn, `Sending` before send) and advanced *after* it lands, so a
//! restart can tell how far a goal got and resume without duplicating
//! a natural-language send into the same session.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Accepted, not yet dispatched.
    Queued,
    /// Persisted before `csd spawn` — session may or may not exist yet.
    Spawning,
    /// Persisted before `csd send` — instruction may or may not have landed.
    Sending,
    /// Instruction confirmed sent; dispatcher is polling for completion.
    Running,
    /// Paused on a `csd` gate (question / plan / trust / permission);
    /// blocks the queue until `pilot.answer` / `pilot.approve` resolves it.
    AwaitingGate,
    Done,
    /// `idle_done` without the completion sentinel after the re-prompt budget.
    Stalled,
    /// A `csd` invocation failed (missing binary, tmux failure, timeout, dead session).
    Failed,
    Cancelled,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Status::Done | Status::Stalled | Status::Failed | Status::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gate {
    /// `answer` (clarifying question) | `plan` | `trust` | `permission`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub cwd: String,
    pub instruction: String,
    pub posture: String,
    pub status: Status,
    /// tmux session name driven via `csd` (equals `id` once spawned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csd_session: Option<String>,
    /// `csd`-assigned transcript UUID (lets the cockpit locate the JSONL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Absolute path to the session transcript (`csd` reports it on spawn;
    /// the cockpit reads it to render live progress — Phase 24.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jsonl_path: Option<String>,
    /// Per-goal completion sentinel; only this exact value, on the last
    /// line of the final assistant turn, counts as done (codex C3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<Gate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub reprompts: u32,
    pub created: u64,
    pub updated: u64,
}

impl Goal {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct QueueFile {
    #[serde(default)]
    goals: Vec<Goal>,
}

pub struct Store {
    path: PathBuf,
    goals: Vec<Goal>,
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        let goals = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<QueueFile>(&raw) {
                Ok(f) => f.goals,
                Err(e) => {
                    eprintln!(
                        "[pilot] queue file {} is corrupt ({e}); starting empty",
                        path.display()
                    );
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        Self { path, goals }
    }

    fn persist(&self) -> std::io::Result<()> {
        let file = QueueFile {
            goals: self.goals.clone(),
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Enqueue a goal. Returns `Err` (and rolls back the in-memory push)
    /// if it couldn't be durably persisted — a goal that isn't on disk
    /// would be silently lost on restart, so the caller must surface it.
    pub fn add(
        &mut self,
        cwd: String,
        instruction: String,
        posture: String,
    ) -> std::io::Result<Goal> {
        let now = now_secs();
        let goal = Goal {
            id: gen_id(),
            cwd,
            instruction,
            posture,
            status: Status::Queued,
            csd_session: None,
            session_id: None,
            jsonl_path: None,
            nonce: None,
            gate: None,
            error: None,
            reprompts: 0,
            created: now,
            updated: now,
        };
        self.goals.push(goal.clone());
        if let Err(e) = self.persist() {
            self.goals.pop();
            return Err(e);
        }
        Ok(goal)
    }

    pub fn snapshot(&self) -> Vec<Goal> {
        self.goals.clone()
    }

    pub fn get(&self, id: &str) -> Option<&Goal> {
        self.goals.iter().find(|g| g.id == id)
    }

    /// The single goal the dispatcher should act on next: the first
    /// non-terminal goal in insertion order (one-at-a-time semantics).
    pub fn claim_next(&self) -> Option<Goal> {
        self.goals.iter().find(|g| !g.status.is_terminal()).cloned()
    }

    /// Apply `f` to the goal, stamp `updated`, and persist — but only
    /// when the goal's current status is in `allowed` (empty = any).
    /// Returns `false` if the id is gone or the precondition fails.
    /// Callers run slow `csd` calls OUTSIDE the lock and write the result
    /// back through this; the precondition guards against a
    /// `pilot.cancel` that landed mid-call (a cancelled goal won't be
    /// resurrected to `Running`).
    pub fn update_if<F: FnOnce(&mut Goal)>(&mut self, id: &str, allowed: &[Status], f: F) -> bool {
        let Some(idx) = self.goals.iter().position(|g| g.id == id) else {
            return false;
        };
        if !allowed.is_empty() && !allowed.contains(&self.goals[idx].status) {
            return false;
        }
        // The dispatcher writes the crash cursor (`Spawning` / `Sending`)
        // through here BEFORE each irreversible `csd` step, so a persist
        // that didn't reach disk must report failure — otherwise the
        // dispatcher would proceed on a cursor that a restart can't see.
        // Roll the mutation back so memory and disk stay equal.
        let backup = self.goals[idx].clone();
        f(&mut self.goals[idx]);
        self.goals[idx].updated = now_secs();
        match self.persist() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[pilot] persist failed; rolling back goal {id}: {e}");
                self.goals[idx] = backup;
                false
            }
        }
    }

    /// Mark a non-terminal goal `Cancelled`. Returns the prior session
    /// name (if any) so the caller can `csd kill` it outside the lock.
    /// `Err` carries a code: `not_found`, `terminal`, or `io_error`
    /// (persist failed — rolled back).
    pub fn cancel(&mut self, id: &str) -> Result<Option<String>, &'static str> {
        let Some(idx) = self.goals.iter().position(|g| g.id == id) else {
            return Err("not_found");
        };
        if self.goals[idx].status.is_terminal() {
            return Err("terminal");
        }
        let backup = self.goals[idx].clone();
        let session = self.goals[idx].csd_session.clone();
        self.goals[idx].status = Status::Cancelled;
        self.goals[idx].gate = None;
        self.goals[idx].updated = now_secs();
        match self.persist() {
            Ok(()) => Ok(session),
            Err(e) => {
                eprintln!("[pilot] persist failed; rolling back cancel of {id}: {e}");
                self.goals[idx] = backup;
                Err("io_error")
            }
        }
    }
}

/// Detect the per-goal completion sentinel. Only the LAST non-empty
/// line of the final assistant turn may be the exact `DONE:<id>:<nonce>`
/// — a quoted mention mid-message ("I'll print DONE:… when done")
/// won't match, and the user prompt that carries the instruction is a
/// user turn, never `idle_done.text` (codex C3).
pub fn detect_completion(text: &str, id: &str, nonce: &str) -> bool {
    let sentinel = format!("DONE:{id}:{nonce}");
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim() == sentinel)
        .unwrap_or(false)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn gen_id() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // `[A-Za-z0-9._-]` only — satisfies `csd --name`.
    format!("g-{:x}-{n:x}", nanos)
}

pub fn gen_nonce() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{n:x}", nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store(dir: &tempfile::TempDir) -> Store {
        Store::load(dir.path().join("queue.json"))
    }

    #[test]
    fn add_then_snapshot_persists_and_reloads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let id = {
            let mut s = Store::load(path.clone());
            let g = s
                .add("/tmp/work".into(), "do a thing".into(), "trust".into())
                .unwrap();
            assert_eq!(g.status, Status::Queued);
            g.id
        };
        // Fresh load sees the persisted goal.
        let reloaded = Store::load(path);
        let snap = reloaded.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, id);
        assert_eq!(snap[0].instruction, "do a thing");
    }

    #[test]
    fn claim_next_skips_terminal_and_is_fifo() {
        let dir = tempdir().unwrap();
        let mut s = store(&dir);
        let a = s.add("/a".into(), "first".into(), "trust".into()).unwrap();
        let _b = s.add("/b".into(), "second".into(), "trust".into()).unwrap();
        // First claim is the oldest.
        assert_eq!(s.claim_next().unwrap().id, a.id);
        // Mark it done — claim advances to the next.
        s.update_if(&a.id, &[], |g| g.status = Status::Done);
        let next = s.claim_next().unwrap();
        assert_eq!(next.instruction, "second");
    }

    #[test]
    fn cancel_returns_session_and_blocks_double_cancel() {
        let dir = tempdir().unwrap();
        let mut s = store(&dir);
        let g = s.add("/a".into(), "x".into(), "trust".into()).unwrap();
        s.update_if(&g.id, &[], |g| {
            g.status = Status::Running;
            g.csd_session = Some(g.id.clone());
        });
        let session = s.cancel(&g.id).unwrap();
        assert_eq!(session.as_deref(), Some(g.id.as_str()));
        assert_eq!(s.get(&g.id).unwrap().status, Status::Cancelled);
        // Already terminal — second cancel is rejected.
        assert_eq!(s.cancel(&g.id), Err("terminal"));
    }

    #[test]
    fn detect_completion_matches_only_exact_final_line() {
        let id = "g-1";
        let nonce = "abc";
        assert!(detect_completion("all done\nDONE:g-1:abc", id, nonce));
        assert!(detect_completion("DONE:g-1:abc\n\n", id, nonce));
        // Mentioned mid-message but not the last line → no match.
        assert!(!detect_completion(
            "I will print DONE:g-1:abc when finished.\nWorking on it now.",
            id,
            nonce
        ));
        // Wrong nonce → no match.
        assert!(!detect_completion("DONE:g-1:zzz", id, nonce));
    }

    #[test]
    fn add_reports_persist_failure_and_rolls_back() {
        // Point the queue path under a regular file so `create_dir_all`
        // on the parent fails — a persist error must surface and leave
        // the queue empty (no in-memory ghost).
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let mut s = Store::load(blocker.join("queue.json"));
        let err = s.add("/a".into(), "x".into(), "trust".into());
        assert!(err.is_err());
        assert!(s.snapshot().is_empty());
    }

    #[test]
    fn ids_and_nonces_are_unique() {
        let a = gen_id();
        let b = gen_id();
        assert_ne!(a, b);
        assert!(a.starts_with("g-"));
        assert_ne!(gen_nonce(), gen_nonce());
    }
}
