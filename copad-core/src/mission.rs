//! Phase 22.5 — Mission substrate.
//!
//! A `Mission` is an orchestrated task with assigned agents, wake
//! conditions, and a budget. v1 ships persistence + CRUD; wake-
//! condition auto-firing (cron / event triggers) lands in 22.7 with
//! the Brain dispatcher — until then, the user explicitly drives
//! turn execution via `mission.turn.submit` calls.
//!
//! Persistence: `~/.local/state/copad/missions/<id>/manifest.yaml` +
//! `timeline.md`. The optional `workspace/` subdir is reserved for
//! mission-scratch files (e.g. shared artifacts between turns).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// Process-local sequence for mission id uniqueness inside one wall
/// millisecond.
fn next_mission_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MissionState {
    Pending,
    Active,
    Paused,
    Done,
    Aborted,
}

impl MissionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Aborted => "aborted",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Aborted)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AgentAssignment {
    pub agent_id: String,
    #[serde(default)]
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MissionBudget {
    /// Maximum number of agent turns allowed in this mission. 0 = uncapped.
    #[serde(default)]
    pub max_turns: u32,
    /// Soft cost cap in dollar-cents. Informational — driver tracks but
    /// does NOT auto-stop on overage in v1 (would need a per-turn cost
    /// estimate that doesn't exist yet).
    #[serde(default)]
    pub cost_cap_cents: u32,
}

/// Wake-condition variants. v1 parses but does NOT auto-fire them —
/// the wiring to TriggerEngine lives in 22.7 (where the same
/// machinery powers pipeline cron stages). The shape is fixed now so
/// future migration is just a registration-pass over existing missions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeCondition {
    /// Cron spec, e.g. `"*/15 * * * *"`. Fires `mission.wake` event.
    Time { cron: String },
    /// Subscribe to a bus event; fire when payload_match matches the
    /// payload subset (exact-key, exact-value match).
    Event {
        event_kind: String,
        #[serde(default)]
        payload_match: serde_json::Value,
    },
    /// Fire when an external HTTP webhook hits the daemon. Currently
    /// uninstalled — included so missions can declare hooks for the
    /// future webhook receiver.
    Webhook { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mission {
    pub id: String,
    pub title: String,
    pub objective: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub assigned_agents: Vec<AgentAssignment>,
    pub state: MissionState,
    #[serde(default)]
    pub urgency: u8,
    #[serde(default)]
    pub cadence: String,
    #[serde(default)]
    pub budget: MissionBudget,
    #[serde(default)]
    pub wake_conditions: Vec<WakeCondition>,
    pub created_at_ms: i64,
    #[serde(default)]
    pub last_turn_at_ms: i64,
    #[serde(default)]
    pub turn_count: u32,
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug)]
pub struct MissionRegistry {
    root: PathBuf,
    inner: Mutex<HashMap<String, Mission>>,
}

impl MissionRegistry {
    pub fn new(root: PathBuf) -> Self {
        let inner = Mutex::new(HashMap::new());
        let reg = Self { root, inner };
        reg.reload_from_disk();
        reg
    }

    pub fn reload_from_disk(&self) {
        let _ = fs::create_dir_all(&self.root);
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut guard = self.inner.lock().unwrap();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest = path.join("manifest.yaml");
            let raw = match fs::read_to_string(&manifest) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mission: Mission = match serde_yml::from_str(&raw) {
                Ok(m) => m,
                Err(_) => continue,
            };
            guard.insert(mission.id.clone(), mission);
        }
    }

    pub fn list(&self) -> Vec<Mission> {
        let mut v: Vec<Mission> = self.inner.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|m| std::cmp::Reverse(m.created_at_ms));
        v
    }

    pub fn list_for(&self, project: Option<&str>, state: Option<MissionState>) -> Vec<Mission> {
        self.list()
            .into_iter()
            .filter(|m| project.is_none_or(|p| m.project.as_deref() == Some(p)))
            .filter(|m| state.is_none_or(|s| m.state == s))
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Mission> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &self,
        title: &str,
        objective: &str,
        project: Option<&str>,
        assigned_agents: Vec<AgentAssignment>,
        budget: MissionBudget,
        wake_conditions: Vec<WakeCondition>,
        cadence: Option<&str>,
        urgency: u8,
        now_ms: i64,
    ) -> Result<Mission, String> {
        // Sequence ensures uniqueness inside one wall millisecond
        // (codex I1: `now_ms` is real millis now).
        let id = format!("mission-{}-{}", now_ms, next_mission_seq());
        let mission = Mission {
            id: id.clone(),
            title: title.to_string(),
            objective: objective.to_string(),
            project: project.map(String::from),
            assigned_agents,
            state: MissionState::Pending,
            urgency,
            cadence: cadence.unwrap_or("").to_string(),
            budget,
            wake_conditions,
            created_at_ms: now_ms,
            last_turn_at_ms: 0,
            turn_count: 0,
            paused: false,
        };
        self.persist(&mission)?;
        self.append_timeline(&id, "created", &format!("submitted by user at {now_ms}"))?;
        self.inner.lock().unwrap().insert(id, mission.clone());
        Ok(mission)
    }

    fn update<F>(&self, id: &str, mutator: F) -> Result<Mission, String>
    where
        F: FnOnce(&mut Mission),
    {
        // Lock held through `persist` — codex C4 closes the window where
        // an older snapshot could overwrite a newer one on disk.
        let mut guard = self.inner.lock().unwrap();
        let mission = guard
            .get_mut(id)
            .ok_or_else(|| format!("mission not found: {id}"))?;
        mutator(mission);
        let snapshot = mission.clone();
        self.persist(&snapshot)?;
        Ok(snapshot)
    }

    /// Returns `(mission, advanced)`. Pause/resume become no-ops on
    /// terminal missions (codex round-5 C1) — daemon handler suppresses
    /// the bus event on no-op.
    pub fn pause(&self, id: &str) -> Result<(Mission, bool), String> {
        let mut advanced = false;
        let m = self.update(id, |m| {
            if m.state.is_terminal() || m.paused {
                return;
            }
            m.paused = true;
            if m.state == MissionState::Active || m.state == MissionState::Pending {
                m.state = MissionState::Paused;
            }
            advanced = true;
        })?;
        Ok((m, advanced))
    }

    pub fn resume(&self, id: &str) -> Result<(Mission, bool), String> {
        let mut advanced = false;
        let m = self.update(id, |m| {
            if m.state == MissionState::Paused {
                m.paused = false;
                m.state = if m.last_turn_at_ms > 0 {
                    MissionState::Active
                } else {
                    MissionState::Pending
                };
                advanced = true;
            }
        })?;
        Ok((m, advanced))
    }

    /// Codex round-6 C1: append the timeline entry only when the
    /// mutation actually applied — terminal missions shouldn't see
    /// their `timeline.md` mutated.
    pub fn redirect_objective(
        &self,
        id: &str,
        new_objective: &str,
    ) -> Result<(Mission, bool), String> {
        let mut advanced = false;
        let m = self.update(id, |m| {
            if !m.state.is_terminal() {
                m.objective = new_objective.to_string();
                advanced = true;
            }
        })?;
        if advanced {
            let _ = self.append_timeline(id, "redirected", new_objective);
        }
        Ok((m, advanced))
    }

    /// Codex round-3 C3: refuses to mutate terminal missions. The prior
    /// implementation appended assignments to aborted/done missions
    /// because it didn't gate on `is_terminal()`.
    pub fn assign_agent(&self, id: &str, agent_id: &str, role: &str) -> Result<Mission, String> {
        let mut advanced = false;
        let after = self.update(id, |m| {
            if m.state.is_terminal() {
                return;
            }
            advanced = true;
            m.assigned_agents.push(AgentAssignment {
                agent_id: agent_id.to_string(),
                role: role.to_string(),
            });
        })?;
        if advanced {
            let _ = self.append_timeline(id, "agent_assigned", &format!("{agent_id} as {role}"));
        }
        Ok(after)
    }

    /// Returns `(mission, advanced)`. Codex round-5 C1: prior version
    /// appended the timeline entry BEFORE checking terminal state, then
    /// only mutated when not-terminal — so a completed mission's
    /// timeline got an "aborted" line and the daemon still emitted
    /// `mission.aborted` even though state stayed Done.
    pub fn abort(&self, id: &str) -> Result<(Mission, bool), String> {
        let mut advanced = false;
        let m = self.update(id, |m| {
            if !m.state.is_terminal() {
                m.state = MissionState::Aborted;
                advanced = true;
            }
        })?;
        if advanced {
            let _ = self.append_timeline(id, "aborted", "");
        }
        Ok((m, advanced))
    }

    /// Mark a turn started. **Codex round-2 C2**: refuses to advance a
    /// paused or terminal mission — the prior implementation bumped
    /// `last_turn_at_ms` + `turn_count` regardless, which then poisoned
    /// the `resume` heuristic (`last_turn_at_ms > 0 → Active`) so a
    /// paused-pending mission resumed as Active instead of Pending.
    /// Returns `(mission, advanced)`. `advanced=false` when paused /
    /// terminal — daemon handler uses this to suppress the
    /// `mission.turn_started` event so external listeners don't act on
    /// refused turns (codex round-3 C1).
    pub fn turn_started(&self, id: &str, now_ms: i64) -> Result<(Mission, bool), String> {
        let mut advanced = false;
        let after = self.update(id, |m| {
            if m.paused || m.state.is_terminal() || m.state == MissionState::Paused {
                return;
            }
            if m.state == MissionState::Pending {
                m.state = MissionState::Active;
            }
            m.last_turn_at_ms = now_ms;
            m.turn_count = m.turn_count.saturating_add(1);
            advanced = true;
        })?;
        if advanced {
            let _ = self.append_timeline(id, "turn_started", "");
        }
        Ok((after, advanced))
    }

    /// Returns `(mission, advanced)`. Same paused-blind protection as
    /// `turn_started` (codex round-3 C2): a paused or terminal mission
    /// can't be completed/advanced by `turn_completed`.
    pub fn turn_completed(
        &self,
        id: &str,
        decision: &str,
        detail: &str,
    ) -> Result<(Mission, bool), String> {
        let mut advanced = false;
        let after = self.update(id, |m| {
            if m.paused || m.state.is_terminal() || m.state == MissionState::Paused {
                return;
            }
            advanced = true;
            if decision == "complete" {
                m.state = MissionState::Done;
            }
        })?;
        if advanced {
            let _ = self.append_timeline(id, "turn_completed", &format!("{decision}: {detail}"));
        }
        Ok((after, advanced))
    }

    fn persist(&self, mission: &Mission) -> Result<(), String> {
        let dir = self.root.join(&mission.id);
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        let manifest = dir.join("manifest.yaml");
        let tmp = dir.join(".manifest.tmp");
        let body = serde_yml::to_string(mission)
            .map_err(|e| format!("serialize mission {}: {e}", mission.id))?;
        fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &manifest).map_err(|e| format!("rename {}: {e}", manifest.display()))?;
        Ok(())
    }

    fn append_timeline(&self, id: &str, kind: &str, detail: &str) -> Result<(), String> {
        let dir = self.root.join(id);
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        let path = dir.join("timeline.md");
        let line = format!("- [{kind}] {detail}\n");
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        file.write_all(line.as_bytes())
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(())
    }

    pub fn timeline_path(&self, id: &str) -> PathBuf {
        self.root.join(id).join("timeline.md")
    }

    pub fn read_timeline(&self, id: &str) -> Result<String, String> {
        let p = self.timeline_path(id);
        fs::read_to_string(&p).map_err(|e| format!("read timeline {}: {e}", p.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_root(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "copad-mission-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            label
        ));
        p
    }

    fn mk() -> MissionRegistry {
        MissionRegistry::new(unique_root("reg"))
    }

    #[test]
    fn submit_persists_and_lists() {
        let r = mk();
        let m = r
            .submit(
                "ship 22.5",
                "wire mission/agent",
                Some("p"),
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                3,
                1_000,
            )
            .unwrap();
        assert!(m.id.starts_with("mission-"));
        assert_eq!(m.state, MissionState::Pending);
        assert_eq!(r.list().len(), 1);
        // Reload.
        let r2 = MissionRegistry::new(r.root.clone());
        assert_eq!(r2.list().len(), 1);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn turn_started_promotes_to_active() {
        let r = mk();
        let m = r
            .submit(
                "t",
                "o",
                None,
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        let (m2, _) = r.turn_started(&m.id, 100).unwrap();
        assert_eq!(m2.state, MissionState::Active);
        assert_eq!(m2.turn_count, 1);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn pause_resume_round_trip() {
        let r = mk();
        let m = r
            .submit(
                "t",
                "o",
                None,
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        r.turn_started(&m.id, 10).unwrap();
        let (p, _) = r.pause(&m.id).unwrap();
        assert_eq!(p.state, MissionState::Paused);
        assert!(p.paused);
        let (r2, _) = r.resume(&m.id).unwrap();
        assert_eq!(r2.state, MissionState::Active);
        assert!(!r2.paused);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn pause_pending_mission_then_resume_returns_to_pending() {
        // Codex C3: pausing a pending mission previously left state=Pending
        // with paused=true, and resume only handled state=Paused → stranded.
        // Fix: pause transitions Pending → Paused; resume reads last_turn
        // to return to Pending (no turn yet) or Active (turn already ran).
        let r = mk();
        let m = r
            .submit(
                "t",
                "o",
                None,
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        assert_eq!(m.state, MissionState::Pending);
        let (p, _) = r.pause(&m.id).unwrap();
        assert_eq!(p.state, MissionState::Paused);
        assert!(p.paused);
        let (r2, _) = r.resume(&m.id).unwrap();
        // Never ticked → resume returns to Pending, not Active.
        assert_eq!(r2.state, MissionState::Pending);
        assert!(!r2.paused);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn turn_completed_refuses_paused_mission_round3_c2() {
        let r = mk();
        let m = r
            .submit(
                "t",
                "o",
                None,
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        r.turn_started(&m.id, 10).unwrap();
        r.pause(&m.id).unwrap();
        let (after, advanced) = r.turn_completed(&m.id, "complete", "x").unwrap();
        assert!(!advanced);
        assert_eq!(after.state, MissionState::Paused);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn assign_agent_refuses_terminal_mission_round3_c3() {
        let r = mk();
        let m = r
            .submit(
                "t",
                "o",
                None,
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        r.abort(&m.id).unwrap();
        let m2 = r.assign_agent(&m.id, "architect", "lead").unwrap();
        assert_eq!(m2.state, MissionState::Aborted);
        assert_eq!(m2.assigned_agents.len(), 0);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn turn_started_refuses_to_advance_paused_mission() {
        // Codex round-2 C2: prior turn_started bumped last_turn_at_ms +
        // turn_count regardless of paused, which poisoned the resume
        // heuristic (last_turn_at_ms > 0 → Active).
        let r = mk();
        let m = r
            .submit(
                "t",
                "o",
                None,
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        let (p, _) = r.pause(&m.id).unwrap();
        assert_eq!(p.state, MissionState::Paused);
        let (after_attempt, _) = r.turn_started(&m.id, 100).unwrap();
        // Nothing changed.
        assert_eq!(after_attempt.last_turn_at_ms, 0);
        assert_eq!(after_attempt.turn_count, 0);
        assert_eq!(after_attempt.state, MissionState::Paused);
        // Resume → Pending (not Active) because no valid turn ever
        // happened.
        let (resumed, _) = r.resume(&m.id).unwrap();
        assert_eq!(resumed.state, MissionState::Pending);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn abort_is_terminal() {
        let r = mk();
        let m = r
            .submit(
                "t",
                "o",
                None,
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        let (a, _) = r.abort(&m.id).unwrap();
        assert_eq!(a.state, MissionState::Aborted);
        // Abort again is a no-op (state stays terminal, no error).
        let (a2, advanced) = r.abort(&m.id).unwrap();
        assert_eq!(a2.state, MissionState::Aborted);
        assert!(!advanced, "abort on terminal must be no-op");
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn assign_agent_appends() {
        let r = mk();
        let m = r
            .submit(
                "t",
                "o",
                None,
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        let m2 = r.assign_agent(&m.id, "architect", "lead").unwrap();
        assert_eq!(m2.assigned_agents.len(), 1);
        assert_eq!(m2.assigned_agents[0].agent_id, "architect");
        assert_eq!(m2.assigned_agents[0].role, "lead");
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn redirect_objective_updates() {
        let r = mk();
        let m = r
            .submit(
                "t",
                "old",
                None,
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        let (m2, _) = r.redirect_objective(&m.id, "new objective").unwrap();
        assert_eq!(m2.objective, "new objective");
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn turn_completed_with_complete_marks_done() {
        let r = mk();
        let m = r
            .submit(
                "t",
                "o",
                None,
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        r.turn_started(&m.id, 10).unwrap();
        let (m2, _) = r.turn_completed(&m.id, "complete", "shipped").unwrap();
        assert_eq!(m2.state, MissionState::Done);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn list_for_filters_by_project_and_state() {
        let r = mk();
        let _ = r
            .submit(
                "a",
                "o",
                Some("p1"),
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                1,
            )
            .unwrap();
        let m2 = r
            .submit(
                "b",
                "o",
                Some("p2"),
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                2,
            )
            .unwrap();
        let _ = r
            .submit(
                "c",
                "o",
                Some("p1"),
                vec![],
                MissionBudget::default(),
                vec![],
                None,
                1,
                3,
            )
            .unwrap();
        r.pause(&m2.id).unwrap();
        let p1_only = r.list_for(Some("p1"), None);
        assert_eq!(p1_only.len(), 2);
        let pending = r.list_for(None, Some(MissionState::Pending));
        assert!(pending.iter().all(|m| m.state == MissionState::Pending));
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn wake_condition_yaml_round_trip() {
        let cron = WakeCondition::Time {
            cron: "*/5 * * * *".into(),
        };
        let evt = WakeCondition::Event {
            event_kind: "todo.created".into(),
            payload_match: serde_json::json!({"priority":"high"}),
        };
        let v = vec![cron.clone(), evt.clone()];
        let yaml = serde_yml::to_string(&v).unwrap();
        let parsed: Vec<WakeCondition> = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(parsed[0], cron);
        assert_eq!(parsed[1], evt);
    }
}
