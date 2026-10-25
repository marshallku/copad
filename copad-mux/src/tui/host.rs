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
