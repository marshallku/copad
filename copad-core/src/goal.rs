//! Phase 22.4 — Goal driver.
//!
//! A `Goal` is a per-project long-running objective with a 1-minute
//! tick loop. Each tick dispatches `claude.start` with the goal's
//! `roadmap.md` + recent history tail; claude returns a JSON
//! `next_action` block which the driver parses to update the goal
//! state machine.
//!
//! State machine:
//! ```text
//!  Running ──pause──→ Paused ──resume──→ Running
//!     │                                    │
//!     ├─complete──→ Done                   │
//!     ├─ask_player──→ Blocked ──answer──→ ─┘
//!     ├─3 no-progress──→ Blocked           │
//!     └─cancel──→ Cancelled                │
//! ```
//!
//! Persistence: `~/.local/state/copad/goals/<id>/state.json` (atomic
//! rename on update) + `roadmap.md` (user-edited; driver reads).
//!
//! Concurrency: the registry holds an internal `Mutex<HashMap<id,
//! Goal>>` + a write-through disk persistence step. Single-process
//! single-writer (the daemon) — no flock.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Running,
    Paused,
    Blocked,
    Done,
    Cancelled,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            GoalStatus::Running => "running",
            GoalStatus::Paused => "paused",
            GoalStatus::Blocked => "blocked",
            GoalStatus::Done => "done",
            GoalStatus::Cancelled => "cancelled",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, GoalStatus::Done | GoalStatus::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TickRecord {
    pub timestamp_ms: i64,
    pub outcome: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub status: GoalStatus,
    pub project: String,
    pub project_path: String,
    pub created_at_ms: i64,
    #[serde(default)]
    pub last_tick_at_ms: i64,
    #[serde(default)]
    pub last_tick_result: Option<String>,
    #[serde(default)]
    pub blocked_question: Option<String>,
    #[serde(default)]
    pub blocked_answer: Option<String>,
    #[serde(default)]
    pub no_progress_count: u32,
    /// Set to the spawned tab/panel id while a tick is in flight; cleared
    /// when `tick.completed` is applied. `next_runnable()` skips goals
    /// with `in_flight_panel_id.is_some()` so tick-N+1 can't fire while
    /// tick-N is still running.
    #[serde(default)]
    pub in_flight_panel_id: Option<String>,
    #[serde(default)]
    pub history: Vec<TickRecord>,
}

/// What claude is expected to return in the fenced JSON block of its
/// tick reply. The driver normalizes unknown strings to `Error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    InvokeSpecialist,
    AskPlayer,
    RecordProgress,
    SelfSchedule,
    Complete,
    Error,
}

impl TickOutcome {
    pub fn parse(s: &str) -> Self {
        match s {
            "invoke_specialist" => Self::InvokeSpecialist,
            "ask_player" => Self::AskPlayer,
            "record_progress" => Self::RecordProgress,
            "self_schedule" => Self::SelfSchedule,
            "complete" => Self::Complete,
            _ => Self::Error,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvokeSpecialist => "invoke_specialist",
            Self::AskPlayer => "ask_player",
            Self::RecordProgress => "record_progress",
            Self::SelfSchedule => "self_schedule",
            Self::Complete => "complete",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TickResult {
    pub outcome: TickOutcome,
    pub detail: String,
}

/// Extract `next_action` + `detail` from claude's raw stdout. Looks
/// for a ```json...``` fence first; falls back to the last `{` to
/// last `}` substring. Parse failures map to `Error` + raw snippet —
/// the caller increments `no_progress_count` rather than crashing.
pub fn parse_tick_output(raw: &str) -> TickResult {
    if let Some(json) = extract_fenced_json(raw)
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json)
    {
        let action = parsed
            .get("next_action")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let detail = parsed
            .get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let outcome = TickOutcome::parse(action);
        if outcome != TickOutcome::Error {
            return TickResult { outcome, detail };
        }
    }
    let snippet: String = raw
        .chars()
        .rev()
        .take(200)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    TickResult {
        outcome: TickOutcome::Error,
        detail: format!("parse_error: {}", snippet.trim()),
    }
}

fn extract_fenced_json(raw: &str) -> Option<String> {
    // Match ```json\n...\n``` (case-insensitive on the language tag).
    let mut search = 0;
    while let Some(rel) = raw[search..].find("```") {
        let start = search + rel + 3;
        // Optional language tag — skip up to the next \n.
        let rest = &raw[start..];
        let nl = rest.find('\n')?;
        let lang = rest[..nl].trim();
        if lang.eq_ignore_ascii_case("json") || lang.is_empty() {
            let body_start = start + nl + 1;
            if body_start >= raw.len() {
                return None;
            }
            if let Some(end_rel) = raw[body_start..].find("```") {
                let body = &raw[body_start..body_start + end_rel];
                return Some(body.trim().to_string());
            }
        }
        search = start + nl + 1;
    }
    // Fallback: last `{` .. last `}` if it parses.
    let lb = raw.rfind('{')?;
    let rb = raw.rfind('}')?;
    if rb > lb {
        Some(raw[lb..=rb].to_string())
    } else {
        None
    }
}

#[derive(Debug)]
pub struct GoalRegistry {
    root: PathBuf,
    inner: Mutex<HashMap<String, Goal>>,
}

impl GoalRegistry {
    pub fn new(root: PathBuf) -> Self {
        let inner = Mutex::new(HashMap::new());
        let registry = Self { root, inner };
        registry.reload_from_disk();
        registry
    }

    /// Re-read every `<root>/<id>/state.json` and merge into the in-memory map.
    /// Existing in-memory entries for the same id are replaced. Missing entries
    /// are NOT removed (we never delete state.json in v1 — terminal goals stay
    /// on disk for audit).
    pub fn reload_from_disk(&self) {
        let _ = fs::create_dir_all(&self.root);
        let mut guard = self.inner.lock().unwrap();
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let state_path = path.join("state.json");
            let raw = match fs::read_to_string(&state_path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let goal: Goal = match serde_json::from_str(&raw) {
                Ok(g) => g,
                Err(_) => continue,
            };
            guard.insert(goal.id.clone(), goal);
        }
    }

    pub fn list(&self) -> Vec<Goal> {
        let guard = self.inner.lock().unwrap();
        let mut v: Vec<Goal> = guard.values().cloned().collect();
        v.sort_by_key(|g| std::cmp::Reverse(g.created_at_ms));
        v
    }

    pub fn list_for(&self, project: Option<&str>, status: Option<GoalStatus>) -> Vec<Goal> {
        self.list()
            .into_iter()
            .filter(|g| project.is_none_or(|p| g.project == p))
            .filter(|g| status.is_none_or(|s| g.status == s))
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Goal> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    /// Picks the oldest-last-ticked Running goal whose preconditions
    /// pass: no in-flight tick, no_progress_count < 3. Returns None when
    /// nothing's runnable.
    pub fn next_runnable(&self) -> Option<Goal> {
        let guard = self.inner.lock().unwrap();
        guard
            .values()
            .filter(|g| g.status == GoalStatus::Running)
            .filter(|g| g.no_progress_count < 3)
            .filter(|g| g.in_flight_panel_id.is_none())
            .min_by_key(|g| g.last_tick_at_ms)
            .cloned()
    }

    pub fn create(
        &self,
        title: &str,
        project: &str,
        project_path: &str,
        roadmap_template: Option<&str>,
        now_ms: i64,
    ) -> Result<Goal, String> {
        let id = format!("goal-{}", now_ms);
        let goal = Goal {
            id: id.clone(),
            title: title.to_string(),
            status: GoalStatus::Running,
            project: project.to_string(),
            project_path: project_path.to_string(),
            created_at_ms: now_ms,
            last_tick_at_ms: 0,
            last_tick_result: None,
            blocked_question: None,
            blocked_answer: None,
            no_progress_count: 0,
            in_flight_panel_id: None,
            history: Vec::new(),
        };
        let dir = self.root.join(&id);
        fs::create_dir_all(&dir).map_err(|e| format!("create goal dir: {e}"))?;
        let roadmap_path = dir.join("roadmap.md");
        if !roadmap_path.exists() {
            let default_body = default_roadmap(&goal);
            let body = roadmap_template.unwrap_or(default_body.as_str());
            fs::write(&roadmap_path, body).map_err(|e| format!("write roadmap: {e}"))?;
        }
        self.persist(&goal)?;
        self.inner.lock().unwrap().insert(id, goal.clone());
        Ok(goal)
    }

    /// Generic mutating updater. Reads the current goal, applies `mutator`,
    /// persists, and returns the new state. The mutator runs under the
    /// registry lock so concurrent updates serialize.
    fn update<F>(&self, id: &str, mutator: F) -> Result<Goal, String>
    where
        F: FnOnce(&mut Goal),
    {
        let mut guard = self.inner.lock().unwrap();
        let goal = guard
            .get_mut(id)
            .ok_or_else(|| format!("goal not found: {id}"))?;
        mutator(goal);
        let snapshot = goal.clone();
        // Persistence requires fs IO — drop lock first to avoid blocking
        // other readers if the FS is slow. Lock is re-acquired below.
        drop(guard);
        self.persist(&snapshot)?;
        Ok(snapshot)
    }

    pub fn mark_in_flight(&self, id: &str, panel_id: &str, now_ms: i64) -> Result<Goal, String> {
        self.update(id, |g| {
            g.in_flight_panel_id = Some(panel_id.to_string());
            g.last_tick_at_ms = now_ms;
        })
    }

    pub fn apply_tick_result(
        &self,
        id: &str,
        result: TickResult,
        now_ms: i64,
    ) -> Result<Goal, String> {
        self.update(id, |g| {
            // If the goal was cancelled/paused/done while the tick was
            // running, discard the result entirely (still clear in-flight
            // marker so the panel doesn't show stuck status).
            g.in_flight_panel_id = None;
            if g.status != GoalStatus::Running {
                return;
            }
            let outcome_str = result.outcome.as_str().to_string();
            let detail = result.detail.clone();
            g.history.push(TickRecord {
                timestamp_ms: now_ms,
                outcome: outcome_str.clone(),
                detail: detail.clone(),
            });
            g.last_tick_result = Some(format!("{outcome_str}: {detail}"));
            match result.outcome {
                TickOutcome::RecordProgress | TickOutcome::InvokeSpecialist => {
                    g.no_progress_count = 0;
                }
                TickOutcome::SelfSchedule => {
                    // Self-schedule is non-progress; bump but don't bias.
                }
                TickOutcome::AskPlayer => {
                    g.status = GoalStatus::Blocked;
                    g.blocked_question = Some(detail);
                    g.blocked_answer = None;
                }
                TickOutcome::Complete => {
                    g.status = GoalStatus::Done;
                }
                TickOutcome::Error => {
                    g.no_progress_count = g.no_progress_count.saturating_add(1);
                    if g.no_progress_count >= 3 {
                        g.status = GoalStatus::Blocked;
                        g.blocked_question =
                            Some("No progress for 3 ticks — what should change?".to_string());
                    }
                }
            }
        })
    }

    pub fn pause(&self, id: &str) -> Result<Goal, String> {
        self.update(id, |g| {
            if g.status == GoalStatus::Running || g.status == GoalStatus::Blocked {
                g.status = GoalStatus::Paused;
            }
        })
    }

    pub fn resume(&self, id: &str) -> Result<Goal, String> {
        self.update(id, |g| {
            if g.status == GoalStatus::Paused {
                g.status = GoalStatus::Running;
            }
        })
    }

    pub fn answer(&self, id: &str, answer: &str) -> Result<Goal, String> {
        self.update(id, |g| {
            if g.status == GoalStatus::Blocked {
                g.blocked_answer = Some(answer.to_string());
                g.status = GoalStatus::Running;
                g.no_progress_count = 0;
            }
        })
    }

    pub fn cancel(&self, id: &str) -> Result<Goal, String> {
        self.update(id, |g| {
            if !g.status.is_terminal() {
                g.status = GoalStatus::Cancelled;
            }
        })
    }

    pub fn roadmap_path(&self, id: &str) -> PathBuf {
        self.root.join(id).join("roadmap.md")
    }

    pub fn read_roadmap(&self, id: &str) -> Result<String, String> {
        let p = self.roadmap_path(id);
        fs::read_to_string(&p).map_err(|e| format!("read roadmap {}: {e}", p.display()))
    }

    /// Atomic write: temp file in the same dir → rename over state.json.
    fn persist(&self, goal: &Goal) -> Result<(), String> {
        let dir = self.root.join(&goal.id);
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        let state_path = dir.join("state.json");
        let tmp_path = dir.join(".state.tmp");
        let body = serde_json::to_string_pretty(goal)
            .map_err(|e| format!("serialize goal {}: {e}", goal.id))?;
        fs::write(&tmp_path, body).map_err(|e| format!("write {}: {e}", tmp_path.display()))?;
        fs::rename(&tmp_path, &state_path)
            .map_err(|e| format!("rename {}: {e}", state_path.display()))?;
        Ok(())
    }
}

fn default_roadmap(goal: &Goal) -> String {
    format!(
        "# {title}\n\n\
        Project: {project}\n\
        Created: {ts}\n\n\
        ## Plan\n\n\
        (Describe what success looks like, current state, and the next 1–3 steps.)\n\n\
        ## Notes\n\n",
        title = goal.title,
        project = goal.project,
        ts = goal.created_at_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_root(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        p.push(format!(
            "copad-goal-{}-{}-{}",
            std::process::id(),
            seq,
            label
        ));
        p
    }

    fn mk() -> GoalRegistry {
        GoalRegistry::new(unique_root("reg"))
    }

    #[test]
    fn empty_registry_has_no_runnable() {
        let r = mk();
        assert!(r.next_runnable().is_none());
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn create_persists_and_lists() {
        let r = mk();
        let g = r
            .create("ship", "owner/repo", "/tmp/repo", None, 1_000)
            .unwrap();
        assert!(g.id.starts_with("goal-"));
        assert_eq!(g.status, GoalStatus::Running);
        assert!(r.roadmap_path(&g.id).exists());
        assert_eq!(r.list().len(), 1);
        // Reload from disk in a new registry.
        let r2 = GoalRegistry::new(r.root.clone());
        assert_eq!(r2.list().len(), 1);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn next_runnable_picks_oldest_last_ticked() {
        let r = mk();
        let g1 = r.create("a", "p", "/tmp", None, 1).unwrap();
        let g2 = r.create("b", "p", "/tmp", None, 2).unwrap();
        // g2 was just ticked, g1 has never been ticked.
        r.update(&g2.id, |g| g.last_tick_at_ms = 99).unwrap();
        let chosen = r.next_runnable().unwrap();
        assert_eq!(chosen.id, g1.id);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn next_runnable_skips_in_flight() {
        let r = mk();
        let g = r.create("a", "p", "/tmp", None, 1).unwrap();
        r.mark_in_flight(&g.id, "panel-x", 10).unwrap();
        assert!(r.next_runnable().is_none());
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn next_runnable_skips_no_progress_3() {
        let r = mk();
        let g = r.create("a", "p", "/tmp", None, 1).unwrap();
        r.update(&g.id, |g| g.no_progress_count = 3).unwrap();
        assert!(r.next_runnable().is_none());
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn tick_record_progress_resets_counter() {
        let r = mk();
        let g = r.create("a", "p", "/tmp", None, 1).unwrap();
        r.update(&g.id, |g| g.no_progress_count = 2).unwrap();
        r.mark_in_flight(&g.id, "panel-x", 5).unwrap();
        let result = TickResult {
            outcome: TickOutcome::RecordProgress,
            detail: "did X".into(),
        };
        let updated = r.apply_tick_result(&g.id, result, 10).unwrap();
        assert_eq!(updated.no_progress_count, 0);
        assert_eq!(updated.history.len(), 1);
        assert!(updated.in_flight_panel_id.is_none());
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn tick_ask_player_blocks() {
        let r = mk();
        let g = r.create("a", "p", "/tmp", None, 1).unwrap();
        r.mark_in_flight(&g.id, "panel-x", 5).unwrap();
        let result = TickResult {
            outcome: TickOutcome::AskPlayer,
            detail: "What region?".into(),
        };
        let updated = r.apply_tick_result(&g.id, result, 10).unwrap();
        assert_eq!(updated.status, GoalStatus::Blocked);
        assert_eq!(updated.blocked_question.as_deref(), Some("What region?"));
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn tick_complete_marks_done() {
        let r = mk();
        let g = r.create("a", "p", "/tmp", None, 1).unwrap();
        let result = TickResult {
            outcome: TickOutcome::Complete,
            detail: "shipped".into(),
        };
        let updated = r.apply_tick_result(&g.id, result, 10).unwrap();
        assert_eq!(updated.status, GoalStatus::Done);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn tick_error_increments_counter_and_blocks_at_3() {
        let r = mk();
        let g = r.create("a", "p", "/tmp", None, 1).unwrap();
        for i in 1..=3 {
            let result = TickResult {
                outcome: TickOutcome::Error,
                detail: format!("err {i}"),
            };
            let updated = r.apply_tick_result(&g.id, result, i).unwrap();
            if i < 3 {
                assert_eq!(updated.status, GoalStatus::Running);
            } else {
                assert_eq!(updated.status, GoalStatus::Blocked);
            }
        }
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn cancelled_goal_discards_in_flight_tick() {
        let r = mk();
        let g = r.create("a", "p", "/tmp", None, 1).unwrap();
        r.mark_in_flight(&g.id, "panel-x", 5).unwrap();
        r.cancel(&g.id).unwrap();
        let result = TickResult {
            outcome: TickOutcome::RecordProgress,
            detail: "this should be discarded".into(),
        };
        let updated = r.apply_tick_result(&g.id, result, 10).unwrap();
        assert_eq!(updated.status, GoalStatus::Cancelled);
        assert!(updated.history.is_empty(), "no history append after cancel");
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn answer_unblocks_goal() {
        let r = mk();
        let g = r.create("a", "p", "/tmp", None, 1).unwrap();
        r.mark_in_flight(&g.id, "panel-x", 5).unwrap();
        let _ = r
            .apply_tick_result(
                &g.id,
                TickResult {
                    outcome: TickOutcome::AskPlayer,
                    detail: "?".into(),
                },
                10,
            )
            .unwrap();
        let updated = r.answer(&g.id, "yes").unwrap();
        assert_eq!(updated.status, GoalStatus::Running);
        assert_eq!(updated.blocked_answer.as_deref(), Some("yes"));
        assert_eq!(updated.no_progress_count, 0);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn pause_resume_round_trip() {
        let r = mk();
        let g = r.create("a", "p", "/tmp", None, 1).unwrap();
        let p = r.pause(&g.id).unwrap();
        assert_eq!(p.status, GoalStatus::Paused);
        let r2 = r.resume(&g.id).unwrap();
        assert_eq!(r2.status, GoalStatus::Running);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn parse_fenced_json_extracts_action() {
        let raw =
            "blah\n```json\n{\"next_action\": \"record_progress\", \"detail\": \"d\"}\n```\nbye";
        let r = parse_tick_output(raw);
        assert_eq!(r.outcome, TickOutcome::RecordProgress);
        assert_eq!(r.detail, "d");
    }

    #[test]
    fn parse_bare_json_object_fallback() {
        let raw =
            "no fence here, just text { \"next_action\": \"complete\", \"detail\": \"done\" }";
        let r = parse_tick_output(raw);
        assert_eq!(r.outcome, TickOutcome::Complete);
        assert_eq!(r.detail, "done");
    }

    #[test]
    fn parse_garbage_returns_error_outcome() {
        let r = parse_tick_output("just words, no json at all");
        assert_eq!(r.outcome, TickOutcome::Error);
        assert!(r.detail.starts_with("parse_error:"));
    }

    #[test]
    fn parse_unknown_action_treated_as_error() {
        let raw = "```json\n{\"next_action\": \"explode\", \"detail\": \"d\"}\n```";
        let r = parse_tick_output(raw);
        assert_eq!(r.outcome, TickOutcome::Error);
    }

    #[test]
    fn list_for_filters_by_project_and_status() {
        let r = mk();
        let _g1 = r.create("a", "p1", "/tmp", None, 1).unwrap();
        let g2 = r.create("b", "p2", "/tmp", None, 2).unwrap();
        let _g3 = r.create("c", "p1", "/tmp", None, 3).unwrap();
        r.pause(&g2.id).unwrap();
        let p1_only = r.list_for(Some("p1"), None);
        assert_eq!(p1_only.len(), 2);
        let paused = r.list_for(None, Some(GoalStatus::Paused));
        assert_eq!(paused.len(), 1);
        assert_eq!(paused[0].id, g2.id);
        let _ = fs::remove_dir_all(&r.root);
    }
}
