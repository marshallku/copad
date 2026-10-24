//! The L0 topic values (decisions #166): what a host UI needs to draw comux's chrome
//! natively, computed off the same state the TUI chrome renders from. Each builder returns a
//! COMPLETE topic value; the server serializes it and publishes only when the string changes,
//! so nothing here may carry a value that moves on its own (no "for N seconds" — a status
//! carries the moment it began instead).

use super::*;
use copad_mux_proto as l0;

/// A monotonic instant as UNIX seconds, for a host that renders its own "4m".
fn unix_of(at: std::time::Instant) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    now - at.elapsed().as_secs() as i64
}

impl App {
    /// `tree`: sessions → tabs → panes, in the sidebar's session order.
    pub fn host_tree(&self) -> l0::Tree {
        let sessions = self
            .session_ids()
            .into_iter()
            .enumerate()
            .filter_map(|(si, wid)| {
                let w = self.state.workspace(&wid)?;
                let tabs = w
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(ti, t)| {
                        let panes: Vec<l0::Pane> = t
                            .layout
                            .panes()
                            .iter()
                            .enumerate()
                            .filter_map(|(pi, p)| {
                                let tid = t.layout.terminal_of(p)?;
                                let label = self.labels.get(tid);
                                let kind = match label.map(|l| l.kind) {
                                    Some(procinfo::Kind::Agent) => "agent",
                                    Some(procinfo::Kind::Shell) => "shell",
                                    _ => "other",
                                };
                                Some(l0::Pane {
                                    token: self.token_of(tid).unwrap_or_default(),
                                    index: pi,
                                    focused: *p == t.focused,
                                    label: label.map(|l| l.text.clone()).unwrap_or_default(),
                                    kind: kind.into(),
                                    status: if kind == "agent" {
                                        self.agent_status(tid).0.into()
                                    } else {
                                        String::new()
                                    },
                                })
                            })
                            .collect();
                        let custom = t.name.clone().filter(|n| !n.trim().is_empty());
                        l0::Tab {
                            id: t.id.to_string(),
                            index: ti,
                            name: custom.clone().unwrap_or_default(),
                            title: custom
                                .or_else(|| self.tab_focus_label(&t.id))
                                .unwrap_or_else(|| format!("{}", ti + 1)),
                            active: t.id == w.active_tab,
                            bell: t
                                .layout
                                .panes()
                                .iter()
                                .filter_map(|p| t.layout.terminal_of(p))
                                .any(|tid| self.bell(tid)),
                            panes,
                        }
                    })
                    .collect();
                Some(l0::Session {
                    id: wid.to_string(),
                    index: si,
                    name: w.name.clone().unwrap_or_else(|| wid.to_string()),
                    active: wid == self.ws,
                    branch: self.branches.get(&wid).cloned().unwrap_or_default(),
                    dir: w
                        .dir
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default(),
                    tabs,
                })
            })
            .collect();
        l0::Tree {
            sessions,
            active: self.ws.to_string(),
        }
    }

    /// `agents`: every agent pane, in sidebar order.
    pub fn host_agents(&self) -> l0::Agents {
        let focused = self.focused_terminal();
        let agents = self
            .agent_rows()
            .into_iter()
            .map(|r| {
                let loc = self.tab_location(&r.term);
                l0::Agent {
                    token: self.token_of(&r.term).unwrap_or_default(),
                    space: r.space,
                    space_id: r.space_id.to_string(),
                    tab_id: loc.as_ref().map(|l| l.0.to_string()).unwrap_or_default(),
                    tab_index: loc.as_ref().map_or(0, |l| l.2),
                    title: r.title,
                    tool: r.tool,
                    status: r.status.to_string(),
                    since_unix: self
                        .agent_statuses
                        .get(&r.term)
                        .map_or(0, |s| unix_of(s.since)),
                    detail: r.detail,
                    focused: focused.as_ref() == Some(&r.term),
                }
            })
            .collect();
        l0::Agents { agents }
    }

    /// `notifications`: the `Ctrl-b a` center, newest first.
    pub fn host_notifications(&self) -> l0::Notifications {
        let items = self
            .notifications
            .iter()
            .map(|n| l0::Notification {
                id: n.id,
                when: n.when.clone(),
                kind: n.kind.into(),
                tool: n.tool.clone(),
                space: n.space.clone(),
                tab: n.tab.clone(),
                body: n.body.clone(),
                token: if self.panes.contains_key(&n.terminal) {
                    self.token_of(&n.terminal).unwrap_or_default()
                } else {
                    String::new()
                },
                read: n.read,
            })
            .collect();
        l0::Notifications {
            items,
            unread: self.unread_count(),
        }
    }

    /// `status`: everything the status bar shows besides the tabs.
    pub fn host_status(&self) -> l0::Status {
        let usage = self.usage_shown.as_ref().map(|u| {
            let mut windows = Vec::new();
            let mut push = |provider: &str, window: &str, pct, reset, stale| {
                if let Some(pct) = pct {
                    windows.push(l0::UsageWindow {
                        provider: provider.into(),
                        window: window.into(),
                        pct,
                        resets_at: reset,
                        stale,
                    });
                }
            };
            push(
                "claude",
                "5h",
                u.claude_5h,
                u.claude_5h_reset,
                u.claude_stale,
            );
            push(
                "claude",
                "wk",
                u.claude_wk,
                u.claude_wk_reset,
                u.claude_stale,
            );
            push("codex", "wk", u.codex_wk, u.codex_wk_reset, u.codex_stale);
            l0::Usage { windows }
        });
        l0::Status {
            prefix_armed: self.prefix_armed,
            prefix_label: self.prefix_label(),
            flash: self.message.as_ref().map(|m| l0::Flash {
                text: m.text.clone(),
                error: m.error,
            }),
            bells: self.bell_count(),
            attention: self.attention_count(),
            agents: self.agent_statuses.len(),
            unread: self.unread_count(),
            usage,
            update: self.version_shown.as_ref().map(|v| v.latest.clone()),
            host: hostname(),
        }
    }

    /// Hide (or restore) chrome a host now draws. Returns whether anything changed; the
    /// caller must then force a full frame to every client, because the content area moved.
    pub fn set_host_hidden(&mut self, sidebar: bool, status: bool) -> bool {
        if self.host_hidden == (sidebar, status) {
            return false;
        }
        self.host_hidden = (sidebar, status);
        // The content area changed size: same path as a sidebar toggle.
        self.reflow();
        true
    }

    /// Every outcome line flashed since the last call — each becomes a reliable `flash`
    /// event, so a message that expires (or is replaced) between two publications still
    /// reaches a host that hid the status bar.
    pub fn take_flashes(&mut self) -> Vec<l0::Flash> {
        std::mem::take(&mut self.flash_log)
    }

    /// `notif-read`: mark one notification (or all) read. Returns how many changed.
    pub(super) fn notif_read(&mut self, id: Option<u64>) -> usize {
        let mut n = 0;
        for x in self.notifications.iter_mut() {
            if !x.read && id.is_none_or(|i| i == x.id) {
                x.read = true;
                n += 1;
            }
        }
        n
    }
}

/// An overlay handed to a host UI instead of being drawn (decisions #170). The TUI state it
/// replaces is kept here, so the host's answer runs exactly the code a key would have.
pub(super) enum Delegated {
    Prompt(PromptKind),
    Confirm(ConfirmAction),
    Switcher,
    Center,
}

/// Which delegable overlays are open — taken before a key is fed, compared after, so only an
/// overlay THAT key opened is delegated (one another client opened stays its TUI).
#[derive(Clone, Copy, Default)]
pub struct OverlayMarks([bool; 4]);

fn prompt_title(kind: &PromptKind) -> &'static str {
    match kind {
        PromptKind::NewSessionIn(_) => "New session name",
        PromptKind::NewWorktree => "New worktree branch",
        PromptKind::RenameSession(_) => "Rename session",
        PromptKind::RenameTab(..) => "Rename tab",
        PromptKind::RenamePane(_) => "Name pane",
    }
}

impl App {
    pub fn overlay_marks(&self) -> OverlayMarks {
        OverlayMarks([
            self.prompt.is_some(),
            self.confirm.is_some(),
            self.popup.is_some(),
            self.center.is_some(),
        ])
    }

    /// Turn every overlay opened since `before` into a host overlay: the TUI state is
    /// cleared (so the shared frame never shows it) and remembered under a fresh id.
    pub fn delegate_new_overlays(&mut self, before: OverlayMarks) -> Vec<(u64, l0::OverlaySpec)> {
        let mut out = Vec::new();
        let mut next = |me: &mut Self, d: Delegated, spec: l0::OverlaySpec| {
            me.overlay_seq += 1;
            me.delegated.insert(me.overlay_seq, d);
            out.push((me.overlay_seq, spec));
        };
        if !before.0[0]
            && let Some(p) = self.prompt.take()
        {
            let spec = l0::OverlaySpec::Prompt {
                title: prompt_title(&p.kind).into(),
                initial: p.buf.clone(),
                placeholder: String::new(),
            };
            next(self, Delegated::Prompt(p.kind), spec);
        }
        if !before.0[1]
            && let Some(c) = self.confirm.take()
        {
            let message = c
                .message
                .split(" (y = ")
                .next()
                .unwrap_or(&c.message)
                .to_string();
            next(
                self,
                Delegated::Confirm(c.action),
                l0::OverlaySpec::Confirm { message },
            );
        }
        if !before.0[2] && self.popup.take().is_some() {
            let spec = self.switcher_spec();
            next(self, Delegated::Switcher, spec);
        }
        if !before.0[3] && self.center.take().is_some() {
            let spec = self.center_spec();
            next(self, Delegated::Center, spec);
        }
        out
    }

    fn switcher_spec(&self) -> l0::OverlaySpec {
        let row = |r: &PopupRow| {
            let (id, current) = match &r.target {
                PopupTarget::Session(w) => (format!("s:{w}"), *w == self.ws),
                PopupTarget::Agent(t) => (
                    format!("a:{t}"),
                    self.focused_terminal().as_ref() == Some(t),
                ),
            };
            let badge = match &r.target {
                PopupTarget::Agent(t) => self.agent_status(t).0.to_string(),
                PopupTarget::Session(_) => String::new(),
            };
            l0::PickerRow {
                id,
                label: r.text.clone(),
                detail: String::new(),
                badge,
                current,
            }
        };
        // The TUI's own default row (the previous session), so Ctrl-f Enter still toggles.
        let selected = self
            .popup_items(PopupTab::Sessions, "")
            .get(self.popup_default_sel(PopupTab::Sessions, ""))
            .map(|r| row(r).id);
        l0::OverlaySpec::Picker {
            selected,
            title: "Switch".into(),
            tabs: vec![
                l0::PickerTab {
                    name: "sessions".into(),
                    rows: self
                        .popup_items(PopupTab::Sessions, "")
                        .iter()
                        .map(row)
                        .collect(),
                },
                l0::PickerTab {
                    name: "agents".into(),
                    rows: self
                        .popup_items(PopupTab::Agents, "")
                        .iter()
                        .map(row)
                        .collect(),
                },
            ],
            tab: 0,
            actions: Vec::new(),
        }
    }

    fn center_spec(&self) -> l0::OverlaySpec {
        let rows = self
            .notifications
            .iter()
            .map(|n| l0::PickerRow {
                id: format!("n:{}", n.id),
                label: format!("{} · {}/{}  {}", n.tool, n.space, n.tab, n.when),
                detail: n.body.clone(),
                badge: n.kind.into(),
                current: !n.read,
            })
            .collect();
        l0::OverlaySpec::Picker {
            selected: None,
            title: "Notifications".into(),
            tabs: vec![l0::PickerTab {
                name: "notifications".into(),
                rows,
            }],
            tab: 0,
            actions: vec![l0::PickerAction {
                id: "dismiss".into(),
                label: "Dismiss".into(),
            }],
        }
    }

    /// A host answered overlay `id`: run what the TUI would have run. `choice`/`text` both
    /// absent = cancelled. Returns whether anything visible changed.
    pub fn overlay_result(
        &mut self,
        id: u64,
        choice: Option<String>,
        text: Option<String>,
    ) -> bool {
        let Some(d) = self.delegated.remove(&id) else {
            return false;
        };
        match d {
            Delegated::Prompt(kind) => {
                let Some(buf) = text else { return false };
                self.commit_prompt(Prompt { kind, buf });
            }
            Delegated::Confirm(action) => {
                if choice.as_deref() != Some("yes") {
                    return false;
                }
                self.perform_confirm(action);
            }
            Delegated::Switcher => {
                let Some(c) = choice else { return false };
                if let Some(w) = c.strip_prefix("s:") {
                    let Some(ws) = self
                        .state
                        .workspace_ids()
                        .into_iter()
                        .find(|x| x.to_string() == w)
                    else {
                        self.flash("that session is gone".into(), false);
                        return true;
                    };
                    self.switch_session(ws);
                } else if let Some(t) = c.strip_prefix("a:") {
                    let Some(term) = self.panes.keys().find(|x| x.to_string() == t).cloned() else {
                        self.flash("that agent's pane is gone".into(), false);
                        return true;
                    };
                    self.jump_to_terminal(&term);
                }
            }
            Delegated::Center => {
                let Some(c) = choice else { return false };
                let (dismiss, nid) = match c.strip_prefix("dismiss:") {
                    Some(rest) => (true, rest),
                    None => (false, c.as_str()),
                };
                let Some(n) = nid.strip_prefix("n:").and_then(|s| s.parse::<u64>().ok()) else {
                    return false;
                };
                let Some(pos) = self.notifications.iter().position(|x| x.id == n) else {
                    return false;
                };
                if dismiss {
                    self.notifications.remove(pos);
                } else {
                    let term = self.notifications[pos].terminal.clone();
                    self.mark_read(&term);
                    if self.panes.contains_key(&term) {
                        self.jump_to_terminal(&term);
                    }
                }
            }
        }
        true
    }

    /// The host that held these overlays is gone: they are simply dropped (the TUI never
    /// showed them, so there is nothing to close).
    pub fn forget_overlays(&mut self, ids: &[u64]) {
        for id in ids {
            self.delegated.remove(id);
        }
    }
}
