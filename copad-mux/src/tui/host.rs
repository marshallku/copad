//! The L0 topic values (decisions #166): what a host UI needs to draw comux's chrome
//! natively, computed off the same state the TUI chrome renders from. Each builder returns a
//! COMPLETE topic value; the server serializes it and publishes only when the string changes,
//! so nothing here may carry a value that moves on its own (no "for N seconds" — a status
//! carries the moment it began instead).

use super::*;
use crate::control::Resp;
use crate::term::Snapshot;
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
    /// Remove entry `id` (with `same_pane`, every entry from its pane), or all of them.
    /// Returns how many went; an unknown id removes nothing.
    pub(super) fn notif_dismiss(&mut self, id: Option<u64>, same_pane: bool) -> usize {
        let before = self.notifications.len();
        match id {
            None => self.notifications.clear(),
            Some(id) => {
                let pane = self
                    .notifications
                    .iter()
                    .find(|n| n.id == id)
                    .map(|n| n.terminal.clone());
                self.notifications
                    .retain(|n| n.id != id && !(same_pane && pane.as_ref() == Some(&n.terminal)));
            }
        }
        self.reconcile_center();
        before - self.notifications.len()
    }

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

/// Largest preview a host may ask for: a big screen's pane, never a scrollback dump.
const PREVIEW_MAX_ROWS: u16 = 200;
const PREVIEW_MAX_COLS: u16 = 400;

impl App {
    /// `pane-preview` (decisions #180): what the TUI switcher previews, for a host to draw.
    pub(super) fn pane_preview(&self, target: &str, rows: Option<u16>, cols: Option<u16>) -> Resp {
        let Some(term) = self.resolve_pane_target(target) else {
            return Resp::err_code("not_found", format!("unknown pane '{target}'"));
        };
        let Some(pt) = self.panes.get(&term) else {
            return Resp::err("pane has no live terminal");
        };
        let snap = pt.snapshot();
        let rows = rows.unwrap_or(snap.rows).clamp(1, PREVIEW_MAX_ROWS);
        let cols = cols.unwrap_or(snap.cols).clamp(1, PREVIEW_MAX_COLS);
        Resp {
            preview: Some(l0::Preview {
                pane: pt.pane_token().unwrap_or_default().to_string(),
                header: self.preview_header(&term),
                cols,
                lines: preview_lines(&snap, rows as usize, cols as usize),
            }),
            ..Resp::ok()
        }
    }
}

fn run_color(c: CellColor) -> Option<l0::RunColor> {
    match c {
        CellColor::Default => None,
        CellColor::Indexed(i) => Some(l0::RunColor::Indexed(i)),
        CellColor::Rgb(r, g, b) => Some(l0::RunColor::Rgb(r, g, b)),
    }
}

/// `snap` as styled runs: bottom-anchored on its last row with content (or the cursor) like
/// the TUI preview, at most `rows` × `cols`. Equal-styled cells merge, wide-glyph spacers are
/// skipped, a wide glyph whose second column would fall past `cols` is dropped, and trailing
/// unstyled blanks are trimmed.
pub(super) fn preview_lines(snap: &Snapshot, rows: usize, cols: usize) -> Vec<Vec<l0::Run>> {
    let last_used = snap
        .cells
        .iter()
        .rposition(|row| row.iter().any(|c| !c.spacer && c.sym.trim() != ""))
        .unwrap_or(0)
        .max(snap.cursor.1 as usize);
    let end = (last_used + 1).min(snap.cells.len());
    let start = end.saturating_sub(rows);
    snap.cells[start..end]
        .iter()
        .map(|row| {
            let mut runs: Vec<l0::Run> = Vec::new();
            for (cx, cell) in row.iter().enumerate() {
                if cx >= cols {
                    break;
                }
                if cell.spacer {
                    continue;
                }
                if cell.sym.width() >= 2 && cx + 1 >= cols {
                    break;
                }
                let style = l0::Run {
                    t: String::new(),
                    fg: run_color(cell.fg),
                    bg: run_color(cell.bg),
                    bold: cell.bold,
                    dim: cell.attrs.contains(Modifier::DIM),
                    italic: cell.attrs.contains(Modifier::ITALIC),
                    underline: cell.attrs.contains(Modifier::UNDERLINED),
                    reverse: cell.reverse,
                };
                let sym = if cell.sym.is_empty() { " " } else { &cell.sym };
                match runs.last_mut() {
                    Some(r)
                        if l0::Run {
                            t: String::new(),
                            ..r.clone()
                        } == style =>
                    {
                        r.t.push_str(sym)
                    }
                    _ => runs.push(l0::Run {
                        t: sym.to_string(),
                        ..style
                    }),
                }
            }
            // Trailing blanks only show when something paints them.
            while let Some(r) = runs.last_mut() {
                if r.bg.is_some() || r.reverse || r.underline {
                    break;
                }
                let kept = r.t.trim_end_matches(' ').len();
                r.t.truncate(kept);
                if !r.t.is_empty() {
                    break;
                }
                runs.pop();
            }
            runs
        })
        .collect()
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config as AlacConfig, Term};
    use alacritty_terminal::vte::ansi::Processor;

    fn snap(cols: u16, rows: u16, bytes: &[u8]) -> Snapshot {
        let mut t = Term::new(
            AlacConfig::default(),
            &TermSize::new(cols as usize, rows as usize),
            VoidListener,
        );
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut t, bytes);
        crate::term::snapshot_grid(&t)
    }

    fn text(line: &[l0::Run]) -> String {
        line.iter().map(|r| r.t.as_str()).collect()
    }

    #[test]
    fn preview_runs_keep_colours_and_merge_equal_cells() {
        let s = snap(20, 4, b"\x1b[31mab\x1b[1;38;2;1;2;3mc\x1b[0m d");
        let lines = preview_lines(&s, 4, 20);
        assert_eq!(lines.len(), 1, "bottom-anchored on the last used row");
        let l = &lines[0];
        assert_eq!(l[0].t, "ab");
        assert_eq!(l[0].fg, Some(l0::RunColor::Indexed(1)));
        assert_eq!(l[1].t, "c");
        assert!(l[1].bold);
        assert_eq!(l[1].fg, Some(l0::RunColor::Rgb(1, 2, 3)));
        assert_eq!(text(l), "abc d", "trailing blanks trimmed");
        let json = serde_json::to_string(&l[1]).unwrap();
        assert!(json.contains("\"#010203\""), "{json}");
        let back: l0::Run = serde_json::from_str(&json).unwrap();
        assert_eq!(back, l[1]);
    }

    #[test]
    fn preview_takes_the_bottom_rows_and_crops_wide_glyphs() {
        let s = snap(10, 6, "one\r\ntwo\r\nthree\r\nab漢".as_bytes());
        let lines = preview_lines(&s, 2, 10);
        assert_eq!(
            lines.iter().map(|l| text(l)).collect::<Vec<_>>(),
            ["three", "ab漢"],
            "the newest rows, not the top of the screen"
        );
        // The wide glyph's second column would fall past the crop: it is dropped whole.
        let lines = preview_lines(&s, 1, 3);
        assert_eq!(text(&lines[0]), "ab");
        // A painted background is content: its blanks stay.
        let s = snap(10, 2, b"x\x1b[44m   \x1b[0m");
        assert_eq!(text(&preview_lines(&s, 1, 10)[0]), "x   ");
    }
}
