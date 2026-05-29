use crate::event_bus::Event;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    #[default]
    Active,
    Away,
}

impl Presence {
    pub fn as_str(self) -> &'static str {
        match self {
            Presence::Active => "active",
            Presence::Away => "away",
        }
    }
}

/// Per-pane context payload published by the local shell's precmd hook via
/// `coctl event publish pane.context_changed '<json>'`. Trust note: events
/// stamped `Origin::External` by the daemon socket (see decision #46) — any
/// same-UID process can publish, so callers downstream of `ContextService`
/// must treat this as best-effort display data.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneContext {
    #[serde(default)]
    pub panel_id: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub git_remote: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub tmux_session: String,
    #[serde(default)]
    pub pane_cmd: String,
    #[serde(default)]
    pub timestamp_ms: i64,
    #[serde(default)]
    pub v: u32,
}

/// Phase 22.3 — active nvim document signal published from the shell's
/// preexec hook (when `nvim <path>` is invoked) via
/// `coctl event publish doc.opened '<json>'`. `path` is relative to the
/// KB root (`COPAD_KB_ROOT` / `COPAD_DOCS_ROOT` / `~/docs`), so it can be
/// passed directly to `kb.read { id }`. Same Origin::External trust note
/// as PaneContext.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveDoc {
    #[serde(default)]
    pub panel_id: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub timestamp_ms: i64,
    #[serde(default)]
    pub v: u32,
}

/// "What the user is currently doing" — v1 carries only the two fields
/// with confirmed event-stream sources. See `docs/workflow-runtime.md`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Context {
    pub active_panel: Option<String>,
    pub active_cwd: Option<PathBuf>,
    #[serde(default)]
    pub presence: Presence,
    #[serde(default)]
    pub pane_context: Option<PaneContext>,
    #[serde(default)]
    pub active_doc: Option<ActiveDoc>,
}

struct Inner {
    active_panel: Option<String>,
    cwd_by_panel: HashMap<String, PathBuf>,
    pane_context_by_panel: HashMap<String, PaneContext>,
    active_doc_by_panel: HashMap<String, ActiveDoc>,
    presence: Presence,
}

/// Caller drains an `EventBus` subscription into `apply_event`.
pub struct ContextService {
    inner: RwLock<Inner>,
}

impl ContextService {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                active_panel: None,
                cwd_by_panel: HashMap::new(),
                pane_context_by_panel: HashMap::new(),
                active_doc_by_panel: HashMap::new(),
                presence: Presence::default(),
            }),
        }
    }

    pub fn snapshot(&self) -> Context {
        let inner = self.inner.read().unwrap();
        let active_cwd = inner
            .active_panel
            .as_ref()
            .and_then(|p| inner.cwd_by_panel.get(p).cloned());
        let pane_context = inner
            .active_panel
            .as_ref()
            .and_then(|p| inner.pane_context_by_panel.get(p).cloned());
        let active_doc = inner
            .active_panel
            .as_ref()
            .and_then(|p| inner.active_doc_by_panel.get(p).cloned());
        Context {
            active_panel: inner.active_panel.clone(),
            active_cwd,
            presence: inner.presence,
            pane_context,
            active_doc,
        }
    }

    pub fn active_panel(&self) -> Option<String> {
        self.inner.read().unwrap().active_panel.clone()
    }

    pub fn active_cwd(&self) -> Option<PathBuf> {
        let inner = self.inner.read().unwrap();
        inner
            .active_panel
            .as_ref()
            .and_then(|p| inner.cwd_by_panel.get(p).cloned())
    }

    pub fn pane_context(&self, panel_id: &str) -> Option<PaneContext> {
        self.inner
            .read()
            .unwrap()
            .pane_context_by_panel
            .get(panel_id)
            .cloned()
    }

    pub fn active_pane_context(&self) -> Option<PaneContext> {
        let inner = self.inner.read().unwrap();
        inner
            .active_panel
            .as_ref()
            .and_then(|p| inner.pane_context_by_panel.get(p).cloned())
    }

    pub fn doc_for_panel(&self, panel_id: &str) -> Option<ActiveDoc> {
        self.inner
            .read()
            .unwrap()
            .active_doc_by_panel
            .get(panel_id)
            .cloned()
    }

    pub fn active_doc(&self) -> Option<ActiveDoc> {
        let inner = self.inner.read().unwrap();
        inner
            .active_panel
            .as_ref()
            .and_then(|p| inner.active_doc_by_panel.get(p).cloned())
    }

    pub fn presence(&self) -> Presence {
        self.inner.read().unwrap().presence
    }

    /// Returns the previous presence so callers can decide whether to
    /// broadcast a `presence.changed` event (avoid emitting on no-op).
    pub fn set_presence(&self, presence: Presence) -> Presence {
        let mut inner = self.inner.write().unwrap();
        let prev = inner.presence;
        inner.presence = presence;
        prev
    }

    pub fn apply_event(&self, event: &Event) {
        match event.kind.as_str() {
            "panel.focused" => {
                if let Some(panel_id) = panel_id_of(event) {
                    self.inner.write().unwrap().active_panel = Some(panel_id);
                }
            }
            // `panel.exited` is the only cross-platform-reliable panel-death
            // signal that carries `panel_id` — both copad-linux/tabs.rs and
            // copad-macos/TerminalViewController emit it on shell exit. We
            // intentionally do NOT consume `tab.closed`: its payload is
            // contracted as `{index}` (see docs/architecture.md), and the
            // Linux superset that includes `panel_id` is incidental.
            "panel.exited" => {
                if let Some(panel_id) = panel_id_of(event) {
                    let mut inner = self.inner.write().unwrap();
                    inner.cwd_by_panel.remove(&panel_id);
                    inner.pane_context_by_panel.remove(&panel_id);
                    inner.active_doc_by_panel.remove(&panel_id);
                    if inner.active_panel.as_deref() == Some(panel_id.as_str()) {
                        inner.active_panel = None;
                    }
                }
            }
            "terminal.cwd_changed" => {
                if let (Some(panel_id), Some(cwd)) = (
                    panel_id_of(event),
                    event.payload.get("cwd").and_then(|v| v.as_str()),
                ) {
                    self.inner
                        .write()
                        .unwrap()
                        .cwd_by_panel
                        .insert(panel_id, PathBuf::from(cwd));
                }
            }
            "pane.context_changed" => {
                match serde_json::from_value::<PaneContext>(event.payload.clone()) {
                    Ok(ctx) if !ctx.panel_id.is_empty() => {
                        let panel_id = ctx.panel_id.clone();
                        self.inner
                            .write()
                            .unwrap()
                            .pane_context_by_panel
                            .insert(panel_id, ctx);
                    }
                    Ok(_) => {
                        log::debug!("context: dropped pane.context_changed with empty panel_id");
                    }
                    Err(e) => {
                        log::debug!(
                            "context: dropped pane.context_changed (deserialize failed): {e}"
                        );
                    }
                }
            }
            "doc.opened" => match serde_json::from_value::<ActiveDoc>(event.payload.clone()) {
                Ok(doc) if !doc.panel_id.is_empty() && !doc.path.is_empty() => {
                    let panel_id = doc.panel_id.clone();
                    self.inner
                        .write()
                        .unwrap()
                        .active_doc_by_panel
                        .insert(panel_id, doc);
                }
                Ok(_) => {
                    log::debug!("context: dropped doc.opened with empty panel_id or path");
                }
                Err(e) => {
                    log::debug!("context: dropped doc.opened (deserialize failed): {e}");
                }
            },
            _ => {}
        }
    }
}

impl Default for ContextService {
    fn default() -> Self {
        Self::new()
    }
}

fn panel_id_of(event: &Event) -> Option<String> {
    event
        .payload
        .get("panel_id")
        .and_then(|v| v.as_str())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn evt(kind: &str, payload: serde_json::Value) -> Event {
        Event::new(kind, "test", payload)
    }

    #[test]
    fn empty_initial_state() {
        let ctx = ContextService::new();
        let snap = ctx.snapshot();
        assert!(snap.active_panel.is_none());
        assert!(snap.active_cwd.is_none());
        assert_eq!(snap.presence, Presence::Active);
    }

    #[test]
    fn presence_default_is_active() {
        let ctx = ContextService::new();
        assert_eq!(ctx.presence(), Presence::Active);
        assert_eq!(ctx.snapshot().presence, Presence::Active);
    }

    #[test]
    fn set_presence_round_trip_and_returns_previous() {
        let ctx = ContextService::new();
        let prev = ctx.set_presence(Presence::Away);
        assert_eq!(prev, Presence::Active);
        assert_eq!(ctx.presence(), Presence::Away);
        assert_eq!(ctx.snapshot().presence, Presence::Away);
        let prev = ctx.set_presence(Presence::Active);
        assert_eq!(prev, Presence::Away);
        assert_eq!(ctx.presence(), Presence::Active);
    }

    #[test]
    fn presence_orthogonal_to_panel_state() {
        let ctx = ContextService::new();
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        ctx.set_presence(Presence::Away);
        let snap = ctx.snapshot();
        assert_eq!(snap.active_panel.as_deref(), Some("p1"));
        assert_eq!(snap.presence, Presence::Away);
        ctx.apply_event(&evt("panel.exited", json!({"panel_id": "p1"})));
        assert_eq!(ctx.presence(), Presence::Away);
    }

    #[test]
    fn presence_serde_lowercase_strings() {
        assert_eq!(
            serde_json::to_string(&Presence::Active).unwrap(),
            "\"active\""
        );
        assert_eq!(serde_json::to_string(&Presence::Away).unwrap(), "\"away\"");
        let active: Presence = serde_json::from_str("\"active\"").unwrap();
        let away: Presence = serde_json::from_str("\"away\"").unwrap();
        assert_eq!(active, Presence::Active);
        assert_eq!(away, Presence::Away);
    }

    #[test]
    fn panel_focused_sets_active_panel() {
        let ctx = ContextService::new();
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "abc"})));
        assert_eq!(ctx.active_panel().unwrap(), "abc");
    }

    #[test]
    fn cwd_recorded_per_panel_and_snapshot_picks_active() {
        let ctx = ContextService::new();
        ctx.apply_event(&evt(
            "terminal.cwd_changed",
            json!({"panel_id": "p1", "cwd": "/x/y"}),
        ));
        // No active panel yet → active_cwd is None even though we cached cwd
        assert!(ctx.active_cwd().is_none());
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        assert_eq!(ctx.active_cwd().unwrap(), PathBuf::from("/x/y"));
    }

    #[test]
    fn focus_switch_uses_other_panels_cached_cwd() {
        let ctx = ContextService::new();
        ctx.apply_event(&evt(
            "terminal.cwd_changed",
            json!({"panel_id": "p1", "cwd": "/a"}),
        ));
        ctx.apply_event(&evt(
            "terminal.cwd_changed",
            json!({"panel_id": "p2", "cwd": "/b"}),
        ));
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        assert_eq!(ctx.active_cwd().unwrap(), PathBuf::from("/a"));
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p2"})));
        assert_eq!(ctx.active_cwd().unwrap(), PathBuf::from("/b"));
    }

    #[test]
    fn panel_exited_clears_active_and_cwd_entry() {
        let ctx = ContextService::new();
        ctx.apply_event(&evt(
            "terminal.cwd_changed",
            json!({"panel_id": "p1", "cwd": "/x"}),
        ));
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        ctx.apply_event(&evt("panel.exited", json!({"panel_id": "p1"})));
        assert!(ctx.active_panel().is_none());
        assert!(ctx.active_cwd().is_none());
    }

    #[test]
    fn panel_exited_for_background_panel_keeps_active_unchanged() {
        let ctx = ContextService::new();
        ctx.apply_event(&evt(
            "terminal.cwd_changed",
            json!({"panel_id": "p1", "cwd": "/a"}),
        ));
        ctx.apply_event(&evt(
            "terminal.cwd_changed",
            json!({"panel_id": "p2", "cwd": "/b"}),
        ));
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        ctx.apply_event(&evt("panel.exited", json!({"panel_id": "p2"})));
        assert_eq!(ctx.active_panel().unwrap(), "p1");
        assert_eq!(ctx.active_cwd().unwrap(), PathBuf::from("/a"));
    }

    #[test]
    fn tab_closed_alone_does_not_clean_up() {
        // `tab.closed` is contracted as `{index}` (see docs/architecture.md)
        // and is intentionally NOT a cleanup trigger here. Cleanup happens
        // when the shell process exits and emits `panel.exited`. This test
        // pins that semantic so a future "let's also handle tab.closed"
        // change forces a re-discussion of the cross-platform contract.
        let ctx = ContextService::new();
        ctx.apply_event(&evt(
            "terminal.cwd_changed",
            json!({"panel_id": "p1", "cwd": "/x"}),
        ));
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        ctx.apply_event(&evt("tab.closed", json!({"panel_id": "p1", "tab": 0})));
        // tab.closed alone is a no-op:
        assert_eq!(ctx.active_panel().unwrap(), "p1");
        assert_eq!(ctx.active_cwd().unwrap(), PathBuf::from("/x"));
        // Cleanup only happens on panel.exited:
        ctx.apply_event(&evt("panel.exited", json!({"panel_id": "p1"})));
        assert!(ctx.active_panel().is_none());
        assert!(ctx.active_cwd().is_none());
    }

    #[test]
    fn unrelated_event_kinds_ignored() {
        let ctx = ContextService::new();
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        ctx.apply_event(&evt(
            "terminal.output",
            json!({"panel_id": "p1", "text": "hi"}),
        ));
        ctx.apply_event(&evt(
            "webview.navigated",
            json!({"panel_id": "p1", "url": "https://x"}),
        ));
        ctx.apply_event(&evt("calendar.event_imminent", json!({"id": "e1"})));
        assert_eq!(ctx.active_panel().unwrap(), "p1");
    }

    #[test]
    fn malformed_payload_does_not_panic() {
        let ctx = ContextService::new();
        ctx.apply_event(&evt("panel.focused", json!({})));
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": 42})));
        ctx.apply_event(&evt("terminal.cwd_changed", json!({"panel_id": "p1"})));
        ctx.apply_event(&evt(
            "terminal.cwd_changed",
            json!({"panel_id": "p1", "cwd": null}),
        ));
        assert!(ctx.active_panel().is_none());
        assert!(ctx.active_cwd().is_none());
    }

    #[test]
    fn concurrent_reads_during_writes_do_not_deadlock() {
        let ctx = Arc::new(ContextService::new());
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p0"})));
        let writer = {
            let c = ctx.clone();
            std::thread::spawn(move || {
                for i in 0..500 {
                    c.apply_event(&evt(
                        "terminal.cwd_changed",
                        json!({"panel_id": "p0", "cwd": format!("/x{i}")}),
                    ));
                }
            })
        };
        let reader = {
            let c = ctx.clone();
            std::thread::spawn(move || {
                for _ in 0..500 {
                    let _ = c.snapshot();
                }
            })
        };
        writer.join().unwrap();
        reader.join().unwrap();
        let cwd = ctx.active_cwd().unwrap();
        assert!(cwd.to_string_lossy().starts_with("/x"));
    }

    fn sample_pane_context(panel_id: &str) -> PaneContext {
        PaneContext {
            panel_id: panel_id.into(),
            host: "arch".into(),
            cwd: "/home/marshall/dev/copad".into(),
            git_remote: "marshallku/copad".into(),
            branch: "master".into(),
            tmux_session: "".into(),
            pane_cmd: "zsh".into(),
            timestamp_ms: 1_748_419_200_000,
            v: 1,
        }
    }

    #[test]
    fn pane_context_changed_records_payload_per_panel() {
        let ctx = ContextService::new();
        let pc = sample_pane_context("p1");
        ctx.apply_event(&evt(
            "pane.context_changed",
            serde_json::to_value(&pc).unwrap(),
        ));
        assert_eq!(ctx.pane_context("p1").unwrap(), pc);
        // Other panels unaffected
        assert!(ctx.pane_context("p2").is_none());
    }

    #[test]
    fn pane_context_replaces_on_second_event() {
        let ctx = ContextService::new();
        let mut pc = sample_pane_context("p1");
        ctx.apply_event(&evt(
            "pane.context_changed",
            serde_json::to_value(&pc).unwrap(),
        ));
        pc.cwd = "/tmp".into();
        pc.git_remote = "".into();
        pc.timestamp_ms += 1_000;
        ctx.apply_event(&evt(
            "pane.context_changed",
            serde_json::to_value(&pc).unwrap(),
        ));
        let stored = ctx.pane_context("p1").unwrap();
        assert_eq!(stored.cwd, "/tmp");
        assert_eq!(stored.git_remote, "");
    }

    #[test]
    fn active_pane_context_resolves_via_active_panel() {
        let ctx = ContextService::new();
        let pc = sample_pane_context("p1");
        ctx.apply_event(&evt(
            "pane.context_changed",
            serde_json::to_value(&pc).unwrap(),
        ));
        // No active panel yet
        assert!(ctx.active_pane_context().is_none());
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        assert_eq!(ctx.active_pane_context().unwrap(), pc);
        // Snapshot mirrors the active query
        assert_eq!(ctx.snapshot().pane_context.unwrap(), pc);
    }

    #[test]
    fn panel_exited_drops_pane_context_entry() {
        let ctx = ContextService::new();
        ctx.apply_event(&evt(
            "pane.context_changed",
            serde_json::to_value(sample_pane_context("p1")).unwrap(),
        ));
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        ctx.apply_event(&evt("panel.exited", json!({"panel_id": "p1"})));
        assert!(ctx.pane_context("p1").is_none());
        assert!(ctx.active_pane_context().is_none());
    }

    #[test]
    fn pane_context_empty_panel_id_is_dropped() {
        let ctx = ContextService::new();
        let mut pc = sample_pane_context("");
        pc.cwd = "/somewhere".into();
        ctx.apply_event(&evt(
            "pane.context_changed",
            serde_json::to_value(&pc).unwrap(),
        ));
        // Nothing recorded
        assert!(ctx.pane_context("").is_none());
        assert!(ctx.active_pane_context().is_none());
    }

    #[test]
    fn pane_context_forward_compat_extra_fields_ignored() {
        let ctx = ContextService::new();
        let payload = json!({
            "panel_id": "p1",
            "cwd": "/x",
            "git_remote": "owner/repo",
            "timestamp_ms": 1_000,
            "v": 1,
            "future_field": "ignored",
            "another": 42
        });
        ctx.apply_event(&evt("pane.context_changed", payload));
        let stored = ctx.pane_context("p1").unwrap();
        assert_eq!(stored.cwd, "/x");
        assert_eq!(stored.git_remote, "owner/repo");
    }

    fn sample_active_doc(panel_id: &str, path: &str) -> ActiveDoc {
        ActiveDoc {
            panel_id: panel_id.into(),
            path: path.into(),
            timestamp_ms: 1_748_500_000_000,
            v: 1,
        }
    }

    #[test]
    fn doc_opened_records_payload_per_panel() {
        let ctx = ContextService::new();
        let doc = sample_active_doc("p1", "topics/copad.md");
        ctx.apply_event(&evt("doc.opened", serde_json::to_value(&doc).unwrap()));
        assert_eq!(ctx.doc_for_panel("p1").unwrap(), doc);
        assert!(ctx.doc_for_panel("p2").is_none());
    }

    #[test]
    fn doc_opened_replaces_on_second_event() {
        let ctx = ContextService::new();
        let mut doc = sample_active_doc("p1", "topics/copad.md");
        ctx.apply_event(&evt("doc.opened", serde_json::to_value(&doc).unwrap()));
        doc.path = "topics/copad-revised.md".into();
        doc.timestamp_ms += 1_000;
        ctx.apply_event(&evt("doc.opened", serde_json::to_value(&doc).unwrap()));
        let stored = ctx.doc_for_panel("p1").unwrap();
        assert_eq!(stored.path, "topics/copad-revised.md");
    }

    #[test]
    fn active_doc_resolves_via_active_panel() {
        let ctx = ContextService::new();
        let doc = sample_active_doc("p1", "topics/copad.md");
        ctx.apply_event(&evt("doc.opened", serde_json::to_value(&doc).unwrap()));
        assert!(ctx.active_doc().is_none());
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        assert_eq!(ctx.active_doc().unwrap(), doc);
        assert_eq!(ctx.snapshot().active_doc.unwrap(), doc);
    }

    #[test]
    fn panel_exited_drops_active_doc_entry() {
        let ctx = ContextService::new();
        ctx.apply_event(&evt(
            "doc.opened",
            serde_json::to_value(sample_active_doc("p1", "topics/copad.md")).unwrap(),
        ));
        ctx.apply_event(&evt("panel.focused", json!({"panel_id": "p1"})));
        ctx.apply_event(&evt("panel.exited", json!({"panel_id": "p1"})));
        assert!(ctx.doc_for_panel("p1").is_none());
        assert!(ctx.active_doc().is_none());
    }

    #[test]
    fn doc_opened_empty_panel_id_or_path_dropped() {
        let ctx = ContextService::new();
        let mut doc = sample_active_doc("", "topics/copad.md");
        ctx.apply_event(&evt("doc.opened", serde_json::to_value(&doc).unwrap()));
        assert!(ctx.doc_for_panel("").is_none());
        doc = sample_active_doc("p1", "");
        ctx.apply_event(&evt("doc.opened", serde_json::to_value(&doc).unwrap()));
        assert!(ctx.doc_for_panel("p1").is_none());
    }

    #[test]
    fn pane_context_missing_fields_default_empty() {
        let ctx = ContextService::new();
        let payload = json!({"panel_id": "p1"});
        ctx.apply_event(&evt("pane.context_changed", payload));
        let stored = ctx.pane_context("p1").unwrap();
        assert_eq!(stored.cwd, "");
        assert_eq!(stored.git_remote, "");
        assert_eq!(stored.timestamp_ms, 0);
        assert_eq!(stored.v, 0);
    }
}
