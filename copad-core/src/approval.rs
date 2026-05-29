//! Phase 22.6 — Approval gate.
//!
//! An `Approval` is a request to perform a privileged action that
//! waits for explicit user grant/deny. Lifecycle:
//!
//! ```text
//!  Pending ─grant──→ Granted
//!     │
//!     ├─deny────→ Denied
//!     └─expire──→ Expired   (after `expires_at`; swept by background thread)
//! ```
//!
//! v1 ships the data model + CRUD; the `register_privileged_with_
//! approval` ActionRegistry hook that gates dispatch on a fresh
//! grant lands in 22.7 alongside the Brain dispatcher's per-role
//! action whitelist. Until then, callers explicitly request +
//! poll approval state before performing the privileged action.
//!
//! Persistence: `~/.local/state/copad/approvals/<id>.yaml`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

fn next_approval_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalState {
    Pending,
    Granted,
    Denied,
    Expired,
}

impl ApprovalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Granted | Self::Denied | Self::Expired)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Approval {
    pub id: String,
    pub action: String,
    pub params_preview: serde_json::Value,
    #[serde(default)]
    pub rationale: String,
    pub state: ApprovalState,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    #[serde(default)]
    pub mission_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub decided_at_ms: Option<i64>,
    #[serde(default)]
    pub decided_by: Option<String>,
    #[serde(default)]
    pub decision_note: Option<String>,
}

#[derive(Debug)]
pub struct ApprovalRegistry {
    root: PathBuf,
    inner: Mutex<HashMap<String, Approval>>,
}

impl ApprovalRegistry {
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
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }
            let raw = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let appr: Approval = match serde_yml::from_str(&raw) {
                Ok(a) => a,
                Err(_) => continue,
            };
            guard.insert(appr.id.clone(), appr);
        }
    }

    pub fn list(&self) -> Vec<Approval> {
        let mut v: Vec<Approval> = self.inner.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.created_at_ms));
        v
    }

    pub fn list_for(&self, state: Option<ApprovalState>, project: Option<&str>) -> Vec<Approval> {
        self.list()
            .into_iter()
            .filter(|a| state.is_none_or(|s| a.state == s))
            .filter(|a| project.is_none_or(|p| a.project.as_deref() == Some(p)))
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Approval> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn request(
        &self,
        action: &str,
        params_preview: serde_json::Value,
        rationale: &str,
        mission_id: Option<&str>,
        agent_id: Option<&str>,
        project: Option<&str>,
        ttl_secs: u64,
        now_ms: i64,
    ) -> Result<Approval, String> {
        let id = format!("approval-{}-{}", now_ms, next_approval_seq());
        let appr = Approval {
            id: id.clone(),
            action: action.to_string(),
            params_preview,
            rationale: rationale.to_string(),
            state: ApprovalState::Pending,
            created_at_ms: now_ms,
            expires_at_ms: now_ms + (ttl_secs as i64) * 1000,
            mission_id: mission_id.map(String::from),
            agent_id: agent_id.map(String::from),
            project: project.map(String::from),
            decided_at_ms: None,
            decided_by: None,
            decision_note: None,
        };
        self.persist(&appr)?;
        self.inner.lock().unwrap().insert(id, appr.clone());
        Ok(appr)
    }

    fn update<F>(&self, id: &str, mutator: F) -> Result<Approval, String>
    where
        F: FnOnce(&mut Approval),
    {
        // Lock held through `persist` (codex C4): dropping the lock
        // before fs IO opened a window where an older snapshot could
        // overwrite a newer one on disk under concurrent updates.
        // fs::rename is fast enough that holding the lock through it
        // is acceptable for single-user workstation use.
        let mut guard = self.inner.lock().unwrap();
        let appr = guard
            .get_mut(id)
            .ok_or_else(|| format!("approval not found: {id}"))?;
        mutator(appr);
        let snapshot = appr.clone();
        self.persist(&snapshot)?;
        Ok(snapshot)
    }

    /// Returns `(approval, transitioned)`. `transitioned=false` when the
    /// approval was already in a terminal state — the caller (daemon)
    /// uses this to suppress the `approval.granted` bus event so a
    /// post-deny grant call doesn't falsely tell a waiter to proceed
    /// (codex C1). Same shape for `deny` / `sweep_expired`.
    pub fn grant(
        &self,
        id: &str,
        by: &str,
        note: Option<&str>,
        now_ms: i64,
    ) -> Result<(Approval, bool), String> {
        let mut did_transition = false;
        let appr = self.update(id, |a| {
            if a.state == ApprovalState::Pending {
                a.state = ApprovalState::Granted;
                a.decided_at_ms = Some(now_ms);
                a.decided_by = Some(by.to_string());
                a.decision_note = note.map(String::from);
                did_transition = true;
            }
        })?;
        Ok((appr, did_transition))
    }

    pub fn deny(
        &self,
        id: &str,
        by: &str,
        reason: Option<&str>,
        now_ms: i64,
    ) -> Result<(Approval, bool), String> {
        let mut did_transition = false;
        let appr = self.update(id, |a| {
            if a.state == ApprovalState::Pending {
                a.state = ApprovalState::Denied;
                a.decided_at_ms = Some(now_ms);
                a.decided_by = Some(by.to_string());
                a.decision_note = reason.map(String::from);
                did_transition = true;
            }
        })?;
        Ok((appr, did_transition))
    }

    /// Sweep expired pending approvals. Returns only entries that
    /// **actually transitioned this call** (codex C2 fix) — concurrent
    /// grant/deny between the filter pass and the per-id update means
    /// the mutator no-ops, in which case we MUST NOT republish an
    /// `approval.expired` event.
    pub fn sweep_expired(&self, now_ms: i64) -> Vec<Approval> {
        let mut transitioned = Vec::new();
        let ids: Vec<String> = self
            .inner
            .lock()
            .unwrap()
            .values()
            .filter(|a| a.state == ApprovalState::Pending && now_ms >= a.expires_at_ms)
            .map(|a| a.id.clone())
            .collect();
        for id in ids {
            let mut did_transition = false;
            if let Ok(a) = self.update(&id, |a| {
                if a.state == ApprovalState::Pending {
                    a.state = ApprovalState::Expired;
                    a.decided_at_ms = Some(now_ms);
                    a.decided_by = Some("system".into());
                    a.decision_note = Some("expired".into());
                    did_transition = true;
                }
            }) && did_transition
            {
                transitioned.push(a);
            }
        }
        transitioned
    }

    fn persist(&self, appr: &Approval) -> Result<(), String> {
        let _ = fs::create_dir_all(&self.root);
        let final_path = self.root.join(format!("{}.yaml", appr.id));
        let tmp = self.root.join(format!(".{}.tmp", appr.id));
        let body = serde_yml::to_string(appr)
            .map_err(|e| format!("serialize approval {}: {e}", appr.id))?;
        fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &final_path)
            .map_err(|e| format!("rename {}: {e}", final_path.display()))?;
        Ok(())
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
            "copad-approval-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            label
        ));
        p
    }

    fn mk() -> ApprovalRegistry {
        ApprovalRegistry::new(unique_root("reg"))
    }

    #[test]
    fn request_persists_and_lists() {
        let r = mk();
        let a = r
            .request(
                "tabs.close",
                serde_json::json!({"id": "t1"}),
                "agent X wants this",
                Some("mission-1"),
                Some("architect"),
                Some("proj"),
                60,
                1_000,
            )
            .unwrap();
        assert_eq!(a.state, ApprovalState::Pending);
        assert!(a.id.starts_with("approval-"));
        assert_eq!(r.list().len(), 1);
        // Reload from disk.
        let r2 = ApprovalRegistry::new(r.root.clone());
        assert_eq!(r2.list().len(), 1);
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn grant_transitions_to_granted() {
        let r = mk();
        let a = r
            .request("x", serde_json::Value::Null, "", None, None, None, 60, 1)
            .unwrap();
        let (g, transitioned) = r.grant(&a.id, "user", Some("ok"), 100).unwrap();
        assert!(transitioned);
        assert_eq!(g.state, ApprovalState::Granted);
        assert_eq!(g.decided_by.as_deref(), Some("user"));
        assert_eq!(g.decided_at_ms, Some(100));
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn deny_transitions_to_denied() {
        let r = mk();
        let a = r
            .request("x", serde_json::Value::Null, "", None, None, None, 60, 1)
            .unwrap();
        let (d, transitioned) = r.deny(&a.id, "user", Some("no thanks"), 100).unwrap();
        assert!(transitioned);
        assert_eq!(d.state, ApprovalState::Denied);
        assert_eq!(d.decision_note.as_deref(), Some("no thanks"));
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn sweep_expired_transitions_pending_past_ttl() {
        let r = mk();
        let a = r
            .request("x", serde_json::Value::Null, "", None, None, None, 60, 0)
            .unwrap();
        // sweep at t=70_000 ms (TTL 60s = 60_000 ms past created_at=0).
        let trans = r.sweep_expired(70_000);
        assert_eq!(trans.len(), 1);
        assert_eq!(trans[0].id, a.id);
        assert_eq!(trans[0].state, ApprovalState::Expired);
        // Sweep again — no double-transition.
        let again = r.sweep_expired(80_000);
        assert!(again.is_empty());
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn grant_after_deny_is_noop() {
        let r = mk();
        let a = r
            .request("x", serde_json::Value::Null, "", None, None, None, 60, 1)
            .unwrap();
        let _ = r.deny(&a.id, "u", None, 50).unwrap();
        let (g, transitioned) = r.grant(&a.id, "u", None, 100).unwrap();
        // State stayed Denied AND we report no transition (codex C1 fix).
        assert_eq!(g.state, ApprovalState::Denied);
        assert!(
            !transitioned,
            "grant on terminal state must not report transition"
        );
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn list_for_filters_state_and_project() {
        let r = mk();
        let a1 = r
            .request(
                "x",
                serde_json::Value::Null,
                "",
                None,
                None,
                Some("p1"),
                60,
                1,
            )
            .unwrap();
        let _a2 = r
            .request(
                "y",
                serde_json::Value::Null,
                "",
                None,
                None,
                Some("p2"),
                60,
                2,
            )
            .unwrap();
        r.grant(&a1.id, "u", None, 10).unwrap();
        // Adding a codex C2 sanity check inline: a second grant on the
        // already-granted record must NOT report transition.
        let (_, trans) = r.grant(&a1.id, "u", None, 11).unwrap();
        assert!(!trans);
        let pending = r.list_for(Some(ApprovalState::Pending), None);
        assert_eq!(pending.len(), 1);
        let p1_only = r.list_for(None, Some("p1"));
        assert_eq!(p1_only.len(), 1);
        let _ = fs::remove_dir_all(&r.root);
    }
}
