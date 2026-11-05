//! Per-pane AI-agent status derived from `claude.*` bus events, shown by the GUI cockpit panel
//! (`docs/agent-cockpit.md`). One instance lives for the whole GUI process, fed by a single
//! event pump.
//!
//! Arrival-order, last-write-wins, best-effort: the hooks emit no causal sequence, but for local
//! panes `coctl event publish` fires synchronously in causal order, so arrival order is causal
//! order. SSH agents carry `panel_id == ""` and are skipped. `session` is display-only and never
//! gates transitions. Nothing is hydrated at startup; `reset()` clears stale overlays after a
//! daemon restart.

use std::collections::HashMap;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentState {
    #[default]
    Idle,
    Working,
    Awaiting,
    Done,
}

impl AgentState {
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Awaiting | Self::Done)
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Awaiting => 0,
            Self::Done => 1,
            Self::Working => 2,
            Self::Idle => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Awaiting => "awaiting",
            Self::Done => "done",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Awaiting => "needs input",
            Self::Done => "done",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PaneAgent {
    pub session: Option<String>,
    pub state: AgentState,
}

#[derive(Debug, Default)]
pub struct AgentCockpit {
    panes: HashMap<String, PaneAgent>,
}

impl AgentCockpit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one `claude.*` agent event; true iff a pane's state changed, so views rebuild only
    /// on real change. Non-agent kinds, empty `panel_id` and malformed payloads are ignored.
    pub fn observe(&mut self, kind: &str, payload: &Value) -> bool {
        let new_state = match kind {
            "claude.working" => AgentState::Working,
            "claude.awaiting_input" => AgentState::Awaiting,
            "claude.session_stopped" => AgentState::Done,
            _ => return false,
        };
        let panel_id = match payload.get("panel_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id,
            _ => return false, // SSH agents (panel_id == "") + malformed are skipped
        };
        let session = payload
            .get("session")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);

        let entry = self.panes.entry(panel_id.to_string()).or_default();
        // Session is display-visible, so a session-only change must also dirty the view.
        let changed = entry.state != new_state || entry.session != session;
        entry.state = new_state;
        entry.session = session;
        changed
    }

    /// Clears attention on a pane the user acted on (cockpit click, `panel.focused`):
    /// Awaiting/Done become Idle. True iff changed.
    pub fn acknowledge(&mut self, panel_id: &str) -> bool {
        if let Some(entry) = self.panes.get_mut(panel_id)
            && entry.state.needs_attention()
        {
            entry.state = AgentState::Idle;
            return true;
        }
        false
    }

    pub fn forget(&mut self, panel_id: &str) -> bool {
        self.panes.remove(panel_id).is_some()
    }

    /// Drops tracked panes `keep` rejects; true iff anything was evicted. `forget()` cannot cover
    /// a closed tab: `tab.closed` carries a single `panel_id` even when the tab held several
    /// split panes, so the GUI reconciles against its live pane set instead.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) -> bool {
        let before = self.panes.len();
        self.panes.retain(|id, _| keep(id));
        self.panes.len() != before
    }

    pub fn reset(&mut self) {
        for entry in self.panes.values_mut() {
            entry.state = AgentState::Idle;
        }
    }

    pub fn state(&self, panel_id: &str) -> AgentState {
        self.panes
            .get(panel_id)
            .map(|e| e.state)
            .unwrap_or_default()
    }

    pub fn session(&self, panel_id: &str) -> Option<&str> {
        self.panes.get(panel_id).and_then(|e| e.session.as_deref())
    }

    pub fn attention_count(&self) -> usize {
        self.panes
            .values()
            .filter(|e| e.state.needs_attention())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(panel: &str, session: &str) -> Value {
        json!({ "panel_id": panel, "session": session, "cwd": "/x" })
    }

    #[test]
    fn transitions_map_kinds_to_states() {
        let mut c = AgentCockpit::new();
        assert!(c.observe("claude.working", &ev("p", "s1")));
        assert_eq!(c.state("p"), AgentState::Working);
        assert!(c.observe("claude.awaiting_input", &ev("p", "s1")));
        assert_eq!(c.state("p"), AgentState::Awaiting);
        assert!(c.observe("claude.session_stopped", &ev("p", "s1")));
        assert_eq!(c.state("p"), AgentState::Done);
    }

    #[test]
    fn observe_reports_change_flag() {
        let mut c = AgentCockpit::new();
        assert!(c.observe("claude.working", &ev("p", "s1"))); // idle→working
        assert!(!c.observe("claude.working", &ev("p", "s1"))); // same state + session = no change
        assert!(c.observe("claude.working", &ev("p", "s2")));
    }

    #[test]
    fn empty_or_missing_panel_id_ignored() {
        let mut c = AgentCockpit::new();
        assert!(!c.observe("claude.working", &ev("", "s1"))); // SSH agent
        assert!(!c.observe("claude.working", &json!({ "session": "s1" }))); // missing
        assert_eq!(c.state(""), AgentState::Idle);
    }

    #[test]
    fn unknown_kind_ignored() {
        let mut c = AgentCockpit::new();
        assert!(!c.observe("claude.session_started", &ev("p", "s1"))); // SessionStart, not ours
        assert!(!c.observe("notify.show", &ev("p", "s1")));
        assert_eq!(c.state("p"), AgentState::Idle);
    }

    #[test]
    fn acknowledge_clears_attention_only() {
        let mut c = AgentCockpit::new();
        c.observe("claude.awaiting_input", &ev("p", "s1"));
        assert!(c.acknowledge("p")); // awaiting→idle
        assert_eq!(c.state("p"), AgentState::Idle);
        assert!(!c.acknowledge("p")); // already idle
        c.observe("claude.working", &ev("p", "s1"));
        assert!(!c.acknowledge("p")); // working is not attention → left as-is
        assert_eq!(c.state("p"), AgentState::Working);
    }

    #[test]
    fn forget_and_reset() {
        let mut c = AgentCockpit::new();
        c.observe("claude.session_stopped", &ev("a", "s1"));
        c.observe("claude.awaiting_input", &ev("b", "s2"));
        assert_eq!(c.attention_count(), 2);
        assert!(c.forget("a"));
        assert!(!c.forget("a")); // already gone
        assert_eq!(c.state("a"), AgentState::Idle);
        c.reset();
        assert_eq!(c.state("b"), AgentState::Idle);
        assert_eq!(c.attention_count(), 0);
    }

    #[test]
    fn label_matches_the_swift_port() {
        assert_eq!(AgentState::Idle.label(), "idle");
        assert_eq!(AgentState::Working.label(), "working");
        assert_eq!(AgentState::Awaiting.label(), "needs input");
        assert_eq!(AgentState::Done.label(), "done");
        assert_eq!(AgentState::Awaiting.as_str(), "awaiting");
    }

    #[test]
    fn retain_evicts_panes_the_predicate_rejects() {
        let mut c = AgentCockpit::new();
        c.observe("claude.awaiting_input", &ev("live", "s1"));
        c.observe("claude.session_stopped", &ev("dead", "s2"));
        assert_eq!(c.attention_count(), 2);

        assert!(c.retain(|id| id == "live"));
        assert_eq!(c.state("dead"), AgentState::Idle); // untracked reads as Idle
        assert_eq!(c.state("live"), AgentState::Awaiting);
        assert_eq!(c.attention_count(), 1); // the whole point: no phantom attention

        assert!(!c.retain(|id| id == "live")); // idempotent — nothing left to drop
    }

    #[test]
    fn session_recorded_for_display() {
        let mut c = AgentCockpit::new();
        c.observe("claude.working", &ev("p", "sess-123"));
        assert_eq!(c.session("p"), Some("sess-123"));
        c.observe("claude.awaiting_input", &ev("p", ""));
        assert_eq!(c.session("p"), None);
    }

    #[test]
    fn attention_rank_orders_correctly() {
        let mut ranks = [
            AgentState::Idle,
            AgentState::Working,
            AgentState::Done,
            AgentState::Awaiting,
        ];
        ranks.sort_by_key(|s| s.rank());
        assert_eq!(
            ranks,
            [
                AgentState::Awaiting,
                AgentState::Done,
                AgentState::Working,
                AgentState::Idle
            ]
        );
    }
}
