//! Agent cockpit state — per-pane AI-agent status derived from `claude.*` bus
//! events, rendered by the desktop GUI cockpit panel (`tmx agents`-style
//! attention dashboard).
//!
//! This is the data half of the agent-lifecycle feature's Slice 2 (Slice 1 was
//! the notification-only toast). It lives at **app-lifetime** in each GUI
//! process (one instance, updated by a single event pump); the cockpit panel is
//! a view/observer over it. See `docs/agent-cockpit.md`.
//!
//! Design constraints (from the plan's codex review):
//! - **Arrival-order, last-write-wins, best-effort.** Perfect stale-event
//!   rejection would need a causal sequence the hooks don't emit. For LOCAL
//!   panes, `coctl event publish` fires synchronously from each hook in causal
//!   order over the local socket, so arrival order == causal order. SSH agents
//!   carry `panel_id == ""` and are skipped. `session` is recorded for display
//!   only; it does not gate transitions.
//! - **No hydration.** The model captures events from process startup; a manual
//!   `reset()` clears stale overlays after a daemon restart.

use std::collections::HashMap;

use serde_json::Value;

/// Per-pane agent state. `Idle` is the default (no agent / acknowledged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentState {
    #[default]
    Idle,
    Working,
    Awaiting,
    Done,
}

impl AgentState {
    /// True when the agent is waiting on the user — these sort to the top.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Awaiting | Self::Done)
    }

    /// Attention-first sort rank (lower = higher in the cockpit list).
    pub fn rank(self) -> u8 {
        match self {
            Self::Awaiting => 0,
            Self::Done => 1,
            Self::Working => 2,
            Self::Idle => 3,
        }
    }

    /// Stable lowercase tag for the wire / UI / tests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Awaiting => "awaiting",
            Self::Done => "done",
        }
    }
}

/// One pane's tracked agent. `session` is display-only (see module docs).
#[derive(Debug, Clone, Default)]
pub struct PaneAgent {
    pub session: Option<String>,
    pub state: AgentState,
}

/// The app-lifetime agent-status map, keyed by `panel_id`.
#[derive(Debug, Default)]
pub struct AgentCockpit {
    panes: HashMap<String, PaneAgent>,
}

impl AgentCockpit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one `claude.*` agent event. Returns `true` iff a pane's state
    /// actually changed (so observer views rebuild only on real change).
    /// Ignores non-agent kinds, empty/missing `panel_id`, and malformed payloads.
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
        // Empty session normalizes to None (display-only, never gates).
        let session = payload
            .get("session")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);

        let entry = self.panes.entry(panel_id.to_string()).or_default();
        // Session is display-visible, so a session-only change must also mark the
        // view dirty (else the row keeps showing the previous session).
        let changed = entry.state != new_state || entry.session != session;
        entry.state = new_state;
        entry.session = session;
        changed
    }

    /// Clear attention on a pane the user acted on (cockpit click, or a
    /// `panel.focused` event). Awaiting/Done → Idle. Returns `true` iff changed.
    pub fn acknowledge(&mut self, panel_id: &str) -> bool {
        if let Some(entry) = self.panes.get_mut(panel_id)
            && entry.state.needs_attention()
        {
            entry.state = AgentState::Idle;
            return true;
        }
        false
    }

    /// Evict a pane's tracked state (on `panel.exited`). Returns `true` iff present.
    pub fn forget(&mut self, panel_id: &str) -> bool {
        self.panes.remove(panel_id).is_some()
    }

    /// Reset every pane to Idle — the manual "Reset" that drops stale overlays
    /// after a daemon restart. Real states re-arrive on the next events.
    pub fn reset(&mut self) {
        for entry in self.panes.values_mut() {
            entry.state = AgentState::Idle;
        }
    }

    /// Current state for a pane (default `Idle` if untracked).
    pub fn state(&self, panel_id: &str) -> AgentState {
        self.panes.get(panel_id).map(|e| e.state).unwrap_or_default()
    }

    /// Current display session for a pane, if any.
    pub fn session(&self, panel_id: &str) -> Option<&str> {
        self.panes.get(panel_id).and_then(|e| e.session.as_deref())
    }

    /// How many tracked panes currently need attention (for a badge/count).
    pub fn attention_count(&self) -> usize {
        self.panes.values().filter(|e| e.state.needs_attention()).count()
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
        // A session-only change (new turn, still working) must mark dirty so the
        // displayed session updates.
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
    fn session_recorded_for_display() {
        let mut c = AgentCockpit::new();
        c.observe("claude.working", &ev("p", "sess-123"));
        assert_eq!(c.session("p"), Some("sess-123"));
        // empty session normalizes to None
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
            [AgentState::Awaiting, AgentState::Done, AgentState::Working, AgentState::Idle]
        );
    }
}
