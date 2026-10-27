//! Overlays handed to a host UI instead of being drawn (decisions #170, #172). A key, click
//! or host request opens comux's own overlay; when the client's host draws overlays, the TUI
//! state is moved here under a fresh id and the host gets a generic spec (picker / prompt /
//! confirm / menu). The host's answer runs exactly the code a key would have.
//!
//! The directory browser (`Ctrl-b C`) and the resume picker (`Ctrl-b R`) fill from off-loop
//! threads. Each host-drawn one owns a PRIVATE result slot — two clients browsing at once can
//! never take each other's listing — and is sent at once as `loading`, then re-sent under the
//! same id when its data lands ([`App::poll_delegated`]).

use super::*;
use copad_mux_proto as l0;
use std::sync::atomic::Ordering;

/// Host-drawn directory listings still running before a new one is refused. A listing on a
/// dead mount never returns, so without a cap every open/cancel would strand another thread.
const MAX_HOST_LISTINGS: usize = 4;

pub(super) enum Delegated {
    Prompt(PromptKind),
    Confirm(ConfirmAction),
    /// The last spec sent: an open switcher / center is re-sent under its id whenever what it
    /// would show changes, as the TUI's redraws every frame.
    Switcher(l0::OverlaySpec),
    Center(l0::OverlaySpec),
    Browser(BrowserOv),
    Resume(ResumeOv),
    Menu(Vec<MenuAction>),
}

/// A host-drawn `Ctrl-b C` directory browser.
pub(super) struct BrowserOv {
    dir: PathBuf,
    slot: dirbrowse::Slot,
    /// The rows the host was sent (`None` while listing). Row ids are positions in it, so
    /// a non-UTF-8 name stays navigable and an id from another snapshot can't alias.
    rows: Option<Vec<dirbrowse::Row>>,
    /// How many of `rows` are the visible ones; the rest are hidden (dot) directories, sent
    /// as their own tab because the host filters only what it was sent (the TUI reveals them
    /// when the filter starts with `.`).
    visible: usize,
    note: String,
}

/// A host-drawn `Ctrl-b R` resume picker.
pub(super) struct ResumeOv {
    st: ResumeState,
    shared: agentsessions::Shared,
    rows: Option<Vec<ResumeRow>>,
}

/// Which delegable overlays are open — taken before a key is fed, compared after, so only an
/// overlay THAT key opened is delegated (one another client opened stays its TUI).
#[derive(Clone, Copy, Default)]
pub struct OverlayMarks([bool; 7]);

fn prompt_title(kind: &PromptKind) -> &'static str {
    match kind {
        PromptKind::NewSessionIn(_) => "New session name",
        PromptKind::NewWorktree => "New worktree branch",
        PromptKind::RenameSession(_) => "Rename session",
        PromptKind::RenameTab(..) => "Rename tab",
        PromptKind::RenamePane(_) => "Name pane",
    }
}

fn picker(title: String, rows: Vec<l0::PickerRow>) -> l0::OverlaySpec {
    l0::OverlaySpec::Picker {
        title,
        tabs: vec![l0::PickerTab {
            name: String::new(),
            rows,
        }],
        tab: 0,
        actions: Vec::new(),
        selected: None,
        loading: false,
        note: String::new(),
        keys: Vec::new(),
        no_filter: false,
    }
}

fn row(id: String, label: String, detail: String) -> l0::PickerRow {
    l0::PickerRow::new(id, label, detail)
}

fn action(id: &str, label: &str, key: &str, stays: bool) -> l0::PickerAction {
    l0::PickerAction {
        id: id.into(),
        label: label.into(),
        key: key.into(),
        stays,
    }
}

fn key(key: &str, choice: &str, stays: bool) -> l0::PickerKey {
    l0::PickerKey {
        key: key.into(),
        choice: choice.into(),
        stays,
    }
}

/// `<prefix>:<i>` → `i`.
fn index_of(choice: &str, prefix: &str) -> Option<usize> {
    choice.strip_prefix(prefix)?.parse().ok()
}

impl App {
    pub fn overlay_marks(&self) -> OverlayMarks {
        OverlayMarks([
            self.prompt.is_some(),
            self.confirm.is_some(),
            self.popup.is_some(),
            self.center.is_some(),
            self.browser.is_some(),
            self.resume.is_some(),
            self.menu.is_some(),
        ])
    }

    fn delegate(&mut self, d: Delegated, spec: l0::OverlaySpec) -> (u64, l0::OverlaySpec) {
        self.overlay_seq += 1;
        self.delegated.insert(self.overlay_seq, d);
        (self.overlay_seq, spec)
    }

    /// Turn every overlay opened since `before` into a host overlay: the TUI state is
    /// cleared (so the shared frame never shows it) and remembered under a fresh id.
    pub fn delegate_new_overlays(&mut self, before: OverlayMarks) -> Vec<(u64, l0::OverlaySpec)> {
        let mut out = Vec::new();
        if !before.0[0]
            && let Some(p) = self.prompt.take()
        {
            let spec = l0::OverlaySpec::Prompt {
                title: prompt_title(&p.kind).into(),
                initial: p.buf.clone(),
                placeholder: String::new(),
            };
            out.push(self.delegate(Delegated::Prompt(p.kind), spec));
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
            let spec = l0::OverlaySpec::Confirm { message };
            out.push(self.delegate(Delegated::Confirm(c.action), spec));
        }
        if !before.0[2] && self.popup.take().is_some() {
            let spec = self.switcher_spec();
            out.push(self.delegate(Delegated::Switcher(spec.clone()), spec));
        }
        if !before.0[3] && self.center.take().is_some() {
            let spec = self.center_spec();
            out.push(self.delegate(Delegated::Center(spec.clone()), spec));
        }
        if !before.0[4]
            && let Some(b) = self.browser.take()
        {
            out.push(self.delegate_browser(b.dir));
        }
        if !before.0[5]
            && let Some(st) = self.resume.take()
        {
            out.push(self.delegate_resume(st));
        }
        if !before.0[6]
            && let Some(m) = self.menu.take()
        {
            let items = m
                .items
                .iter()
                .enumerate()
                .map(|(i, (label, action))| l0::MenuItem {
                    id: format!("m:{i}"),
                    label: (*label).to_string(),
                    destructive: matches!(
                        action,
                        MenuAction::CloseTab(..) | MenuAction::KillSession(_)
                    ),
                })
                .collect();
            let actions = m.items.into_iter().map(|(_, a)| a).collect();
            let spec = l0::OverlaySpec::Menu {
                title: m.title,
                items,
            };
            out.push(self.delegate(Delegated::Menu(actions), spec));
        }
        out
    }

    /// A host-drawn browser at `dir`, listing on its own thread (sent as `loading`).
    fn delegate_browser(&mut self, dir: PathBuf) -> (u64, l0::OverlaySpec) {
        let slot = dirbrowse::Slot::default();
        let mut ov = BrowserOv {
            dir: dir.clone(),
            slot: slot.clone(),
            rows: None,
            visible: 0,
            note: String::new(),
        };
        let running = self.host_listings.clone();
        if running.fetch_add(1, Ordering::SeqCst) >= MAX_HOST_LISTINGS {
            running.fetch_sub(1, Ordering::SeqCst);
            ov.rows = Some(vec![dirbrowse::Row::Up]);
            ov.visible = 1;
            ov.note = "too many directory listings are still running (a dead mount?)".into();
        } else {
            std::thread::spawn(move || {
                let listing = dirbrowse::list_dirs(&dir);
                if let Ok(mut g) = slot.lock() {
                    *g = Some(dirbrowse::ListResult {
                        req: 0,
                        dir,
                        listing,
                    });
                }
                running.fetch_sub(1, Ordering::SeqCst);
            });
        }
        let spec = browser_spec(&ov);
        self.delegate(Delegated::Browser(ov), spec)
    }

    /// A host-drawn resume picker, scanning on its own thread (sent as `loading`).
    fn delegate_resume(&mut self, st: ResumeState) -> (u64, l0::OverlaySpec) {
        let shared = agentsessions::idle();
        let mut ov = ResumeOv {
            st,
            shared: shared.clone(),
            rows: None,
        };
        match std::env::var_os("HOME").map(PathBuf::from) {
            Some(home) => agentsessions::spawn_scan(shared, 1, home, ov.st.all),
            None => ov.rows = Some(Vec::new()),
        }
        let spec = resume_spec(&ov, &self.resume_live_tokens(&ov));
        self.delegate(Delegated::Resume(ov), spec)
    }

    /// Adopt the listings/scans that landed for host-drawn overlays. Returns each overlay to
    /// RE-SEND (same id, now with its rows). Called from the server loop.
    pub fn poll_delegated(&mut self) -> Vec<(u64, l0::OverlaySpec)> {
        let home = home_dir();
        let mut out = Vec::new();
        // A live switcher / center: re-sent when what it shows changed (an agent finished, a
        // notification arrived). Same id, so the host keeps its filter and selection.
        let live: Vec<u64> = self
            .delegated
            .iter()
            .filter(|(_, d)| matches!(d, Delegated::Switcher(_) | Delegated::Center(_)))
            .map(|(id, _)| *id)
            .collect();
        for id in live {
            let (now, last) = match self.delegated.get(&id) {
                Some(Delegated::Switcher(last)) => (self.switcher_spec(), last),
                Some(Delegated::Center(last)) => (self.center_spec(), last),
                _ => continue,
            };
            if now == *last {
                continue;
            }
            out.push((id, now.clone()));
            if let Some(Delegated::Switcher(last) | Delegated::Center(last)) =
                self.delegated.get_mut(&id)
            {
                *last = now;
            }
        }
        let mut ready = Vec::new();
        for (id, d) in &self.delegated {
            let landed = match d {
                Delegated::Browser(b) => {
                    b.rows.is_none() && b.slot.lock().is_ok_and(|g| g.is_some())
                }
                Delegated::Resume(r) => {
                    r.rows.is_none() && r.shared.lock().is_ok_and(|g| g.is_some())
                }
                _ => false,
            };
            if landed {
                ready.push(*id);
            }
        }
        for id in ready {
            let Some(d) = self.delegated.remove(&id) else {
                continue;
            };
            let d = match d {
                Delegated::Browser(mut b) => {
                    let res = b.slot.lock().ok().and_then(|mut g| g.take());
                    let empty = dirbrowse::Listing::default();
                    let (listing, note) = match res.map(|r| r.listing) {
                        Some(Ok(l)) => (l, String::new()),
                        Some(Err(e)) => (empty, e),
                        None => (empty, String::new()),
                    };
                    let mut rows = dirbrowse::rows(&b.dir, &listing, "", &home);
                    b.visible = rows.len();
                    rows.extend(
                        listing
                            .names
                            .iter()
                            .filter(|n| n.to_string_lossy().starts_with('.'))
                            .cloned()
                            .map(dirbrowse::Row::Dir),
                    );
                    b.rows = Some(rows);
                    b.note = if listing.truncated && note.is_empty() {
                        "too many directories — only the first ones are listed".into()
                    } else {
                        note
                    };
                    out.push((id, browser_spec(&b)));
                    Delegated::Browser(b)
                }
                Delegated::Resume(mut r) => {
                    let entries = r
                        .shared
                        .lock()
                        .ok()
                        .and_then(|mut g| g.take())
                        .map(|s| s.entries)
                        .unwrap_or_default();
                    r.rows = Some(self.resume_rows_in(&r.st, &entries));
                    let tokens = self.resume_live_tokens(&r);
                    out.push((id, resume_spec(&r, &tokens)));
                    Delegated::Resume(r)
                }
                other => other,
            };
            self.delegated.insert(id, d);
        }
        out
    }

    /// The pane token of each resume row whose conversation is live in a pane.
    fn resume_live_tokens(&self, r: &ResumeOv) -> Vec<Option<String>> {
        r.rows
            .iter()
            .flatten()
            .map(|x| {
                let t = x.live.as_ref()?;
                self.panes.get(t)?.pane_token().map(str::to_string)
            })
            .collect()
    }

    /// The `$COPAD_MUX_PANE` token a host previews for `target`.
    fn preview_token(&self, target: &PopupTarget) -> String {
        self.preview_terminal(target)
            .and_then(|t| self.panes.get(&t)?.pane_token().map(str::to_string))
            .unwrap_or_default()
    }

    pub(super) fn switcher_spec(&self) -> l0::OverlaySpec {
        let row = |r: &PopupRow| {
            let (id, current, session) = match &r.target {
                PopupTarget::Session(w) => (format!("s:{w}"), *w == self.ws, w.to_string()),
                PopupTarget::Agent(t) => (
                    format!("a:{t}"),
                    self.focused_terminal().as_ref() == Some(t),
                    String::new(),
                ),
            };
            let badge = match &r.target {
                PopupTarget::Agent(t) => self.agent_status(t).0.to_string(),
                PopupTarget::Session(_) => String::new(),
            };
            l0::PickerRow {
                badge,
                current,
                session,
                pane: self.preview_token(&r.target),
                // Only a session is renamed from here (the TUI's ^R).
                no_actions: matches!(r.target, PopupTarget::Agent(_)),
                ..l0::PickerRow::new(id, r.text.clone(), String::new())
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
            actions: vec![action("rename", "Rename", "C-r", false)],
            loading: false,
            note: String::new(),
            // Ctrl-f again closes it, as in the TUI.
            keys: vec![key("C-f", "", false)],
            no_filter: false,
        }
    }

    pub(super) fn center_spec(&self) -> l0::OverlaySpec {
        let rows = self
            .notifications
            .iter()
            .map(|n| l0::PickerRow {
                badge: n.kind.into(),
                current: !n.read,
                pane: self
                    .panes
                    .get(&n.terminal)
                    .and_then(|p| p.pane_token().map(str::to_string))
                    .unwrap_or_default(),
                ..l0::PickerRow::new(
                    format!("n:{}", n.id),
                    format!("{} · {}/{}  {}", n.tool, n.space, n.tab, n.when),
                    n.body.clone(),
                )
            })
            .collect();
        let mut spec = picker("Notifications".into(), rows);
        if let l0::OverlaySpec::Picker {
            tabs,
            actions,
            keys,
            no_filter,
            ..
        } = &mut spec
        {
            tabs[0].name = "notifications".into();
            // The TUI center's keys: d dismisses one, D clears all, and it stays open.
            actions.push(action("dismiss", "Dismiss", "d", true));
            keys.push(key("D", "clear-all", true));
            *no_filter = true;
        }
        spec
    }

    /// A host answered overlay `id`: run what the TUI would have run. `choice`/`text` both
    /// absent = cancelled. Returns whether anything visible changed, and the overlays the
    /// answer opened IN THE HOST (a browser descending into a directory, the resume picker
    /// flipping to headless runs) — the caller hands those to the same host. Overlays the
    /// answer opened in the TUI (a menu item's rename prompt) are found by the caller's
    /// [`OverlayMarks`] diff instead.
    pub fn overlay_result(
        &mut self,
        id: u64,
        choice: Option<String>,
        text: Option<String>,
    ) -> (bool, Vec<(u64, l0::OverlaySpec)>) {
        let Some(d) = self.delegated.remove(&id) else {
            return (false, Vec::new());
        };
        let mut next = Vec::new();
        let changed = match d {
            Delegated::Prompt(kind) => match text {
                Some(buf) => {
                    self.commit_prompt(Prompt { kind, buf });
                    true
                }
                None => false,
            },
            Delegated::Confirm(action) => {
                let yes = choice.as_deref() == Some("yes");
                if yes {
                    self.perform_confirm(action);
                }
                yes
            }
            Delegated::Switcher(_) => match choice {
                Some(c) => {
                    self.switcher_choice(&c);
                    true
                }
                None => false,
            },
            Delegated::Center(_) => match choice {
                Some(c) => {
                    let stays = c == "clear-all" || c.starts_with("dismiss:");
                    let changed = self.center_choice(&c);
                    // Dismissing keeps the center up (the TUI's `d` / `D`): the same id again,
                    // so the host replaces it in place.
                    if stays {
                        let spec = self.center_spec();
                        self.delegated.insert(id, Delegated::Center(spec.clone()));
                        next.push((id, spec));
                    }
                    changed
                }
                None => false,
            },
            Delegated::Browser(b) => {
                // `open:<row>` is `→`: descend into a directory, never create (TUI parity).
                let (open_only, choice) = match choice.as_deref().map(|c| c.strip_prefix("open:")) {
                    Some(Some(rest)) => (true, Some(rest.to_string())),
                    _ => (false, choice),
                };
                let picked = choice
                    .as_deref()
                    .and_then(|c| match c {
                        "here" => Some(dirbrowse::Row::Here),
                        "up" => Some(dirbrowse::Row::Up),
                        _ => index_of(c, "d:").and_then(|i| b.rows.as_ref()?.get(i).cloned()),
                    })
                    // Only a row this overlay offered (an unreadable directory has no `here`).
                    .filter(|r| b.rows.as_ref().is_some_and(|rows| rows.contains(r)));
                match picked {
                    Some(dirbrowse::Row::Here) if open_only => {
                        // Nothing to descend into: the same listing again (the host dropped it).
                        let spec = browser_spec(&b);
                        self.delegated.insert(id, Delegated::Browser(b));
                        next.push((id, spec));
                        false
                    }
                    Some(dirbrowse::Row::Here) => {
                        self.prompt = Some(Prompt {
                            buf: dirbrowse::default_name(&b.dir),
                            kind: PromptKind::NewSessionIn(b.dir),
                        });
                        true
                    }
                    Some(dirbrowse::Row::Up) => {
                        if let Some(parent) = b.dir.parent() {
                            next.push(self.delegate_browser(parent.to_path_buf()));
                        }
                        false
                    }
                    Some(dirbrowse::Row::Dir(name)) => {
                        next.push(self.delegate_browser(b.dir.join(name)));
                        false
                    }
                    Some(dirbrowse::Row::Path(p)) => {
                        next.push(self.delegate_browser(p));
                        false
                    }
                    None => false,
                }
            }
            Delegated::Resume(r) => match choice.as_deref() {
                Some("toggle-all") => {
                    let mut st = r.st;
                    st.all = !st.all;
                    next.push(self.delegate_resume(st));
                    false
                }
                // The TUI's ^r: scan again.
                Some("rescan") => {
                    next.push(self.delegate_resume(r.st));
                    false
                }
                Some(c) => {
                    let (fork, c) = match c.strip_prefix("fork:") {
                        Some(rest) => (true, rest),
                        None => (false, c),
                    };
                    let row = index_of(c, "c:").and_then(|i| r.rows.as_ref()?.get(i).cloned());
                    match row {
                        Some(row) => {
                            match (fork, row.live.clone()) {
                                (false, Some(term)) => self.jump_to_terminal(&term),
                                _ => self.open_resumed(row, fork),
                            }
                            true
                        }
                        None => false,
                    }
                }
                None => false,
            },
            Delegated::Menu(actions) => {
                let action = choice
                    .as_deref()
                    .and_then(|c| index_of(c, "m:"))
                    .and_then(|i| actions.get(i).cloned());
                match action {
                    Some(a) => {
                        self.run_menu_action(a);
                        true
                    }
                    None => false,
                }
            }
        };
        (changed, next)
    }

    fn switcher_choice(&mut self, c: &str) {
        // ^R on a session row: switch to it and open the rename prompt there (the TUI's
        // `popup_rename`); the prompt is delegated by the caller's marks diff.
        if let Some(w) = c.strip_prefix("rename:s:") {
            let Some(ws) = self
                .state
                .workspace_ids()
                .into_iter()
                .find(|x| x.to_string() == w)
            else {
                return self.flash("that session is gone".into(), false);
            };
            self.switch_session(ws);
            self.open_rename_prompt();
            return;
        }
        if let Some(w) = c.strip_prefix("s:") {
            let Some(ws) = self
                .state
                .workspace_ids()
                .into_iter()
                .find(|x| x.to_string() == w)
            else {
                return self.flash("that session is gone".into(), false);
            };
            self.switch_session(ws);
        } else if let Some(t) = c.strip_prefix("a:") {
            let Some(term) = self.panes.keys().find(|x| x.to_string() == t).cloned() else {
                return self.flash("that agent's pane is gone".into(), false);
            };
            self.jump_to_terminal(&term);
        }
    }

    fn center_choice(&mut self, c: &str) -> bool {
        if c == "clear-all" {
            let had = !self.notifications.is_empty();
            self.notifications.clear();
            return had;
        }
        let (dismiss, nid) = match c.strip_prefix("dismiss:") {
            Some(rest) => (true, rest),
            None => (false, c),
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
            self.consume_notifications(&term);
            if self.panes.contains_key(&term) {
                self.jump_to_terminal(&term);
            }
        }
        true
    }

    /// A host asked to open one of comux's overlays (`SubMsg::Open`, decisions #172). It runs
    /// the same opener the key or menu item would; the caller then delegates whatever opened
    /// to that host. Refused while the same kind of overlay is open in the TUI (another
    /// client's), which the opener would otherwise replace.
    pub fn open_for_host(&mut self, what: &str, target: Option<&str>) -> Result<(), String> {
        let busy = |open: bool| {
            if open {
                Err(format!("another client has a {what} overlay open"))
            } else {
                Ok(())
            }
        };
        match what {
            "new-session" => {
                busy(self.browser.is_some())?;
                self.open_new_session_prompt();
            }
            "resume" => {
                busy(self.resume.is_some())?;
                self.open_resume();
            }
            "switcher" => {
                busy(self.popup.is_some())?;
                self.open_popup();
            }
            "notifications" => {
                busy(self.center.is_some())?;
                self.open_center();
            }
            "new-worktree" => {
                busy(self.prompt.is_some())?;
                self.open_new_worktree_prompt();
            }
            "rename-session" => {
                busy(self.prompt.is_some())?;
                let ws = self.target_session(target)?;
                self.open_rename_prompt_for(ws);
            }
            "kill-session" => {
                busy(self.confirm.is_some())?;
                let ws = self.target_session(target)?;
                if self.state.workspace_count() <= 1 {
                    return Err("the last session can't be killed".into());
                }
                self.open_kill_session_confirm_for(ws);
            }
            "rename-tab" => {
                busy(self.prompt.is_some())?;
                let (ws, tab) = self.target_tab(target)?;
                self.open_rename_tab_prompt_for(ws, tab);
            }
            "close-tab" => {
                busy(self.confirm.is_some())?;
                let (ws, tab) = self.target_tab(target)?;
                self.request_close_tab(ws, tab);
            }
            _ => return Err(format!("unknown overlay {what:?}")),
        }
        Ok(())
    }

    /// `<session id>`, or the active session when absent.
    fn target_session(&self, target: Option<&str>) -> Result<WorkspaceId, String> {
        let Some(t) = target else {
            return Ok(self.ws.clone());
        };
        self.state
            .workspace_ids()
            .into_iter()
            .find(|w| w.to_string() == t)
            .ok_or_else(|| format!("no session {t:?}"))
    }

    /// `<session id>/<tab id>`, or the active session's active tab when absent.
    fn target_tab(&self, target: Option<&str>) -> Result<(WorkspaceId, TabId), String> {
        let (ws, tab) = match target {
            None => (self.ws.clone(), None),
            Some(t) => {
                let (w, tab) = t.split_once('/').ok_or_else(|| format!("bad tab {t:?}"))?;
                (self.target_session(Some(w))?, Some(tab))
            }
        };
        let w = self.state.workspace(&ws).ok_or("that session is gone")?;
        let found = match tab {
            Some(tab) => w.tabs.iter().find(|x| x.id.to_string() == tab),
            None => w.tabs.iter().find(|x| x.id == w.active_tab),
        };
        let tab = found.ok_or("that tab is gone")?.id.clone();
        Ok((ws, tab))
    }

    /// The host that held these overlays is gone: they are simply dropped (the TUI never
    /// showed them, so there is nothing to close).
    pub fn forget_overlays(&mut self, ids: &[u64]) {
        for id in ids {
            self.delegated.remove(id);
        }
    }
}

fn browser_spec(b: &BrowserOv) -> l0::OverlaySpec {
    let rows: Vec<l0::PickerRow> = b
        .rows
        .iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, r)| {
            Some(match r {
                dirbrowse::Row::Here => l0::PickerRow {
                    no_actions: true,
                    ..row(
                        "here".into(),
                        "Create session here".into(),
                        home_short(&b.dir),
                    )
                },
                dirbrowse::Row::Up => l0::PickerRow {
                    no_actions: true,
                    ..row("up".into(), "..".into(), String::new())
                },
                dirbrowse::Row::Dir(n) => row(
                    format!("d:{i}"),
                    n.to_string_lossy().into_owned(),
                    String::new(),
                ),
                dirbrowse::Row::Path(_) => return None,
            })
        })
        .collect();
    let split = b.visible.min(rows.len());
    let has_up = rows.iter().any(|r| r.id == "up");
    let (shown, hidden) = rows.split_at(split);
    let mut spec = picker(home_short(&b.dir), shown.to_vec());
    if let l0::OverlaySpec::Picker {
        tabs,
        loading,
        note,
        actions,
        keys,
        ..
    } = &mut spec
    {
        // The TUI browser's arrows: ← goes up, → descends without creating.
        actions.push(action("open", "Open", "Right", false));
        if has_up {
            keys.push(key("Left", "up", false));
        }
        tabs[0].name = "directories".into();
        if !hidden.is_empty() {
            tabs.push(l0::PickerTab {
                name: "hidden".into(),
                rows: hidden.to_vec(),
            });
        }
        *loading = b.rows.is_none();
        note.clone_from(&b.note);
    }
    spec
}

fn resume_spec(r: &ResumeOv, live_tokens: &[Option<String>]) -> l0::OverlaySpec {
    let toggle = if r.st.all {
        "Hide headless runs"
    } else {
        "Show headless runs"
    };
    let mut rows = vec![l0::PickerRow {
        no_actions: true,
        ..row("toggle-all".into(), toggle.into(), String::new())
    }];
    for (i, x) in r.rows.iter().flatten().enumerate() {
        let path = x.cwd.as_deref().map(home_short).unwrap_or_default();
        let label = if x.title.is_empty() {
            "(no prompt)".to_string()
        } else {
            x.title.clone()
        };
        rows.push(l0::PickerRow {
            badge: if x.live.is_some() {
                "live".into()
            } else {
                String::new()
            },
            pane: live_tokens.get(i).cloned().flatten().unwrap_or_default(),
            ..row(
                format!("c:{i}"),
                label,
                format!("{} · {path} · {}", x.tool.bin(), x.age),
            )
        });
    }
    let mut spec = picker("Resume conversation".into(), rows);
    if let l0::OverlaySpec::Picker {
        tabs,
        actions,
        loading,
        selected,
        keys,
        ..
    } = &mut spec
    {
        tabs[0].name = "conversations".into();
        keys.push(key("C-a", "toggle-all", false));
        keys.push(key("C-r", "rescan", false));
        // Enter picks the newest conversation, not the headless toggle above it.
        *selected = r
            .rows
            .as_ref()
            .filter(|x| !x.is_empty())
            .map(|_| "c:0".to_string());
        *loading = r.rows.is_none();
        actions.push(action("fork", "Fork", "C-f", false));
    }
    spec
}
