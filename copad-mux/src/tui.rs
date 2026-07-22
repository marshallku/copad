//! The mux [`App`]: the authoritative `State` (layout authority) + one `PaneTerm`
//! (real shell PTY) per terminal, plus the composition logic. It renders the whole
//! workspace into a `Buffer` ([`App::render_to`]) and interprets keys
//! ([`App::feed_key`]) — tmux-style prefix `Ctrl-b` then `%`/`"` split, `o`/arrows
//! focus, `x` close, `c`/`n`/`p`/`&` tabs, `d` detach; plus prefix-less
//! `Ctrl+Shift+arrow` nav and the `Ctrl-f` switcher.
//!
//! `App` is transport-agnostic: it owns no terminal I/O and no socket. The headless
//! [`crate::server`] drives it (render → ship cell diffs; forwarded keys → feed_key)
//! and the thin [`crate::client`] renders + forwards input, so the same App serves
//! local and detached/reattached sessions.

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agentstate;
use crate::control;
use crate::gitinfo;
use crate::model::{ClientId, Dir, PaneId, PaneRect, Rect, Role, TabId, TerminalId, WorkspaceId};
use crate::procinfo;
use crate::proto::MouseKind;
use crate::state::{Command, Event, MuxError, Origin, State};
use crate::term::{CellColor, PaneTerm};

/// Direction for focus navigation with the arrow keys.
#[derive(Clone, Copy)]
enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// What feeding one key to the app implies for the caller (the server loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Keep the session going.
    Continue,
    /// The user pressed the detach chord (`Ctrl-b d` / `Ctrl-b q`): the client should
    /// leave, but the server + shells keep running.
    Detach,
}

/// Lines scrolled per mouse-wheel notch.
const SCROLL_STEP: i32 = 3;
/// Height of the always-on bottom status bar (tmux-style).
const STATUS_H: u16 = 1;
/// Sidebar width in columns (a 1-col border is added to its right).
const SIDEBAR_W: u16 = 24;

// Catppuccin Mocha — the owner's tmux status palette (`~/dotfiles/tmux/.tmux.conf`).
const CAT_BASE: Color = Color::Rgb(0x1e, 0x1e, 0x2e);
const CAT_TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
const CAT_MAUVE: Color = Color::Rgb(0xcb, 0xa6, 0xf7);
const CAT_SURFACE0: Color = Color::Rgb(0x31, 0x32, 0x44);
const CAT_BLUE: Color = Color::Rgb(0x89, 0xb4, 0xfa);
const CAT_YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
const CAT_SUBTEXT: Color = Color::Rgb(0xba, 0xc2, 0xde);
const CAT_OVERLAY: Color = Color::Rgb(0x6c, 0x70, 0x86);
const CAT_GREEN: Color = Color::Rgb(0xa6, 0xe3, 0xa1);
const CAT_PEACH: Color = Color::Rgb(0xfa, 0xb3, 0x87);
/// Below this terminal width the sidebar is suppressed even when toggled on —
/// the popup switcher is the fallback on narrow / mobile widths (size-adaptive).
const SIDEBAR_MIN_COLS: u16 = 80;

/// The multi-pane application: the authoritative layout `State` + a live shell per
/// terminal. The local TUI is the single controller client (`ClientId(0)`).
pub struct App {
    state: State,
    ws: WorkspaceId,
    client: ClientId,
    panes: HashMap<TerminalId, PaneTerm>,
    cols: u16,
    rows: u16,
    /// Neovim-style left panel toggle (`Ctrl-b s`). Honored only when the
    /// terminal is at least `SIDEBAR_MIN_COLS` wide.
    sidebar: bool,
    /// When `Some(sel)`, the `Ctrl-f` popup switcher is open with row `sel`
    /// selected; keys drive the popup instead of the shells.
    popup: Option<usize>,
    /// Keyboard scrollback mode (`Ctrl-b [`): `Some(term)` binds the mode to the
    /// terminal it was entered on, so a focus change (click / another client) can't
    /// strand that pane scrolled-up. Keys scroll it; exit bottoms it. Shared state.
    scroll_pane: Option<TerminalId>,
    /// Env vars injected into every pane's shell (e.g. `COPAD_MUX_SOCK`), so a
    /// shell inside a pane can control its own mux via `copad-mux ctl`.
    sock_env: Vec<(String, String)>,
    /// Per-terminal foreground-process label (agent / shell / command), refreshed
    /// on a throttled cadence (`refresh_labels`), read by the sidebar/popup/CLI.
    labels: HashMap<TerminalId, procinfo::Label>,
    /// When the labels were last refreshed (throttle `ps` to ~2 Hz).
    last_labels: std::time::Instant,
    /// Per-agent rolled-up status (working/ready/blocked/idle), refreshed at the
    /// label cadence from Claude's session file + a screen-text fallback (`agentstate`).
    agent_statuses: HashMap<TerminalId, agentstate::AgentStatus>,
    /// Per-session git branch (its focused pane's cwd) for the `spaces` subtitle,
    /// refreshed at the label cadence.
    branches: HashMap<WorkspaceId, String>,
    /// Monotonic counter for minting unique session (workspace) ids (`local` is the
    /// first, so new sessions start at `s1`).
    next_session: u64,
}

impl App {
    pub fn new(cols: u16, rows: u16, sock_env: Vec<(String, String)>) -> io::Result<Self> {
        let mut state = State::new();
        let ws = WorkspaceId::new("local");
        let (_tab, _pane, term0) = state.create_workspace(ws.clone(), None, Rect { cols, rows });
        let client = ClientId(0);
        let _ = state.apply(Command::Attach {
            client,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols,
            rows,
        });
        let mut panes = HashMap::new();
        let Some(pt) = PaneTerm::spawn_with_env(cols.max(1), rows.max(1), None, None, &sock_env)
        else {
            return Err(io::Error::other("failed to spawn shell PTY"));
        };
        panes.insert(term0, pt);
        let mut app = Self {
            state,
            ws,
            client,
            panes,
            cols,
            rows,
            // On by default (herdr-style always-on spaces + agents panel); still
            // size-adaptive (hidden below SIDEBAR_MIN_COLS) and toggleable via Ctrl-b s.
            sidebar: true,
            popup: None,
            scroll_pane: None,
            sock_env,
            labels: HashMap::new(),
            last_labels: std::time::Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(std::time::Instant::now),
            agent_statuses: HashMap::new(),
            branches: HashMap::new(),
            next_session: 1,
        };
        app.reflow();
        Ok(app)
    }

    // --- reads off the authoritative state ---

    /// Is the sidebar actually drawn? Toggled on AND wide enough (size-adaptive:
    /// narrow / mobile widths fall back to the popup).
    fn sidebar_visible(&self) -> bool {
        self.sidebar && self.cols >= SIDEBAR_MIN_COLS
    }

    /// Left x-offset of the pane grid (the sidebar reserves `SIDEBAR_W` + 1 border).
    fn content_x(&self) -> u16 {
        if self.sidebar_visible() {
            SIDEBAR_W + 1
        } else {
            0
        }
    }

    /// Width available to the pane grid, right of the sidebar.
    fn content_cols(&self) -> u16 {
        if self.sidebar_visible() {
            self.cols.saturating_sub(SIDEBAR_W + 1)
        } else {
            self.cols
        }
    }

    // --- tabs ---

    fn tab_count(&self) -> usize {
        self.state
            .workspace(&self.ws)
            .map(|w| w.tabs.len())
            .unwrap_or(0)
    }

    fn tab_ids(&self) -> Vec<TabId> {
        self.state
            .workspace(&self.ws)
            .map(|w| w.tabs.iter().map(|t| t.id.clone()).collect())
            .unwrap_or_default()
    }

    fn active_tab_id(&self) -> Option<TabId> {
        self.state.workspace(&self.ws).map(|w| w.active_tab.clone())
    }

    // --- sessions (workspaces) ---

    fn session_ids(&self) -> Vec<WorkspaceId> {
        self.state.workspace_ids()
    }

    fn session_count(&self) -> usize {
        self.state.workspace_count()
    }

    fn active_session_index(&self) -> usize {
        self.session_ids()
            .iter()
            .position(|w| w == &self.ws)
            .unwrap_or(0)
    }

    /// Does session `wid` host any classified AI agent (across all its tabs)?
    fn session_has_agent(&self, wid: &WorkspaceId) -> bool {
        let Some(w) = self.state.workspace(wid) else {
            return false;
        };
        w.tabs.iter().any(|t| {
            t.layout.panes().iter().any(|p| {
                t.layout
                    .terminal_of(p)
                    .and_then(|tid| self.labels.get(tid))
                    .map(|l| l.kind == procinfo::Kind::Agent)
                    .unwrap_or(false)
            })
        })
    }

    /// Top y-offset of the pane grid. Tabs live in the bottom status bar now, so
    /// there is no top bar — the grid starts at row 0.
    fn content_y(&self) -> u16 {
        0
    }

    /// Height available to the pane grid, above the always-on bottom status bar.
    fn content_rows(&self) -> u16 {
        self.rows.saturating_sub(STATUS_H)
    }

    /// Placed rects for the active tab, tiling the content area (right of the
    /// sidebar when it is visible). Rects are already offset by `content_x`.
    fn layout(&self) -> Vec<PaneRect> {
        let mut out = Vec::new();
        if let Some(w) = self.state.workspace(&self.ws)
            && let Some(tab) = w.tab(&w.active_tab)
        {
            tab.layout.derive_layout(
                self.content_x(),
                self.content_y(),
                self.content_cols(),
                self.content_rows(),
                &mut out,
            );
        }
        out
    }

    fn focused_pane(&self) -> Option<PaneId> {
        let w = self.state.workspace(&self.ws)?;
        let tab = w.tab(&w.active_tab)?;
        Some(tab.focused.clone())
    }

    fn focused_terminal(&self) -> Option<TerminalId> {
        let w = self.state.workspace(&self.ws)?;
        let tab = w.tab(&w.active_tab)?;
        tab.layout.terminal_of(&tab.focused).cloned()
    }

    fn pane_order(&self) -> Vec<PaneId> {
        self.state
            .workspace(&self.ws)
            .and_then(|w| w.tab(&w.active_tab))
            .map(|t| t.layout.panes())
            .unwrap_or_default()
    }

    fn pane_of_terminal(&self, term: &TerminalId) -> Option<PaneId> {
        let w = self.state.workspace(&self.ws)?;
        let tab = w.tab(&w.active_tab)?;
        tab.layout
            .panes()
            .into_iter()
            .find(|p| tab.layout.terminal_of(p) == Some(term))
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Resize every hosted PTY to match its on-screen rect, so shell output wraps
    /// at the right width (the layout is the source of truth for geometry).
    fn sync_sizes(&self) {
        for rect in self.layout() {
            if let Some(pt) = self.panes.get(&rect.terminal) {
                pt.resize(rect.cols, rect.rows);
            }
        }
    }

    /// Apply any `Resized` geometry carried in `events` directly to the matching
    /// PaneTerms. Needed for BACKGROUND sessions/tabs: `sync_sizes`/`reflow` only
    /// touch the ACTIVE session's layout, but a reap in a background session
    /// recomputes ITS surviving terminals (G3) — those PTYs must still be SIGWINCH'd.
    fn apply_resized_events(&self, events: &[Event]) {
        for e in events {
            if let Event::Resized { terminals, .. } = e {
                for tg in terminals {
                    if let Some(pt) = self.panes.get(&tg.id) {
                        pt.resize(tg.cols, tg.rows);
                    }
                }
            }
        }
    }

    /// Push the CONTENT-area size (terminal minus sidebar + tab-bar chrome) to the
    /// authoritative `State` as the workspace viewport, then resize every PTY.
    /// Keeping the viewport equal to the real pane area means the authoritative
    /// geometry — and the `Resized` events a remote client will rely on — matches
    /// the actual PTY sizes (G2), even as chrome toggles. Call this on any change
    /// that alters the chrome footprint (terminal resize, sidebar toggle, tab
    /// add/remove); a pure in-tab layout change (split/close/focus) can use
    /// `sync_sizes` since the viewport is unchanged.
    fn reflow(&mut self) {
        let cols = self.content_cols().max(1);
        let rows = self.content_rows().max(1);
        let _ = self.state.apply(Command::Resize {
            client: self.client,
            cols,
            rows,
        });
        self.sync_sizes();
    }

    // --- mutations (through the authoritative actor) ---

    fn split(&mut self, dir: Dir) {
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let events = match self.state.apply(Command::SplitPane {
            origin: Origin::Client(self.client),
            workspace: self.ws.clone(),
            pane,
            dir,
            if_rev: None,
        }) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut new_term = None;
        let mut new_pane = None;
        for e in &events {
            if let Event::PaneSplit {
                new_terminal,
                new_pane: np,
                ..
            } = e
            {
                new_term = Some(new_terminal.clone());
                new_pane = Some(np.clone());
            }
        }
        if let (Some(nt), Some(np)) = (new_term, new_pane) {
            match PaneTerm::spawn_with_env(
                self.cols.max(1),
                self.rows.max(1),
                None,
                None,
                &self.sock_env,
            ) {
                Some(pt) => {
                    self.panes.insert(nt, pt);
                    // tmux-style: focus the freshly created pane.
                    let _ = self.state.apply(Command::FocusPane {
                        client: self.client,
                        pane: np,
                    });
                }
                None => {
                    // PTY spawn failed — roll the split back so authoritative state
                    // never holds a pane without a matching PaneTerm (which would
                    // render blank + silently drop input). Closing `np` collapses the
                    // branch and refocuses the original pane.
                    let _ = self.state.apply(Command::ClosePane {
                        origin: Origin::Client(self.client),
                        workspace: self.ws.clone(),
                        pane: np,
                        if_rev: None,
                    });
                }
            }
        }
        self.sync_sizes();
    }

    /// Grow/shrink the focused pane along an axis (`Right` = width, `Down` = height)
    /// by nudging its split divider.
    fn resize_focused(&mut self, axis: Dir, grow: bool) {
        let Some(pane) = self.focused_pane() else {
            return;
        };
        if self
            .state
            .apply(Command::ResizePane {
                origin: Origin::Client(self.client),
                workspace: self.ws.clone(),
                pane,
                axis,
                grow,
            })
            .is_ok()
        {
            self.sync_sizes();
        }
    }

    fn close_focused(&mut self) {
        if let Some(pane) = self.focused_pane() {
            // The `x` key ignores a rejected close (e.g. the last pane stays;
            // the shell-exit path handles quitting).
            let _ = self.close_pane(pane);
        }
    }

    /// Close a specific pane (used by the `x` key on the focused pane and by the
    /// control API's `close <index>`).
    fn close_pane(&mut self, pane: PaneId) -> Result<(), MuxError> {
        // Propagate the actor's verdict (e.g. `CannotCloseLastPane`) so the
        // control API reports a real failure instead of a false `ok`.
        let events = self.state.apply(Command::ClosePane {
            origin: Origin::Client(self.client),
            workspace: self.ws.clone(),
            pane,
            if_rev: None,
        })?;
        for e in &events {
            if let Event::PaneClosed { terminal, .. } = e {
                self.panes.remove(terminal);
            }
        }
        self.sync_sizes();
        Ok(())
    }

    /// Create a new tab (a fresh single-pane layout) and switch to it, spawning its
    /// shell. Rolls the tab back if the PTY can't spawn (never leave a blank tab).
    fn new_tab(&mut self) {
        let events = match self.state.apply(Command::NewTab {
            origin: Origin::Client(self.client),
            workspace: self.ws.clone(),
        }) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut created: Option<(TabId, TerminalId)> = None;
        for e in &events {
            if let Event::TabCreated { tab, terminal, .. } = e {
                created = Some((tab.clone(), terminal.clone()));
            }
        }
        if let Some((tab_id, term)) = created {
            match PaneTerm::spawn_with_env(
                self.cols.max(1),
                self.rows.max(1),
                None,
                None,
                &self.sock_env,
            ) {
                Some(pt) => {
                    self.panes.insert(term, pt);
                }
                None => {
                    // Spawn failed — close the just-created tab so state never holds a
                    // tab whose pane has no live terminal (blank render + dropped input).
                    let _ = self.state.apply(Command::CloseTab {
                        origin: Origin::Client(self.client),
                        workspace: self.ws.clone(),
                        tab: tab_id,
                    });
                }
            }
        }
        // A new tab may make the tab bar appear (1→2 tabs), shrinking the content
        // height → re-derive the viewport, not just PTY sizes.
        self.reflow();
    }

    /// Switch the active tab by `delta` (wrapping): `+1` next, `-1` previous.
    fn cycle_tab(&mut self, delta: i32) {
        let ids = self.tab_ids();
        if ids.len() < 2 {
            return;
        }
        let Some(active) = self.active_tab_id() else {
            return;
        };
        let cur = ids.iter().position(|t| t == &active).unwrap_or(0);
        let next = ids[(cur as i32 + delta).rem_euclid(ids.len() as i32) as usize].clone();
        let _ = self.state.apply(Command::SelectTab {
            origin: Origin::Client(self.client),
            workspace: self.ws.clone(),
            tab: next,
        });
        self.sync_sizes();
    }

    /// Switch to the tab at 0-based `index` (the tab bar shows 1-based labels, so
    /// `Ctrl-b 1` → index 0). Out-of-range is ignored.
    fn select_tab_index(&mut self, index: usize) {
        if let Some(tab) = self.tab_ids().get(index).cloned() {
            let _ = self.state.apply(Command::SelectTab {
                origin: Origin::Client(self.client),
                workspace: self.ws.clone(),
                tab,
            });
            self.sync_sizes();
        }
    }

    /// Close the active tab (and reap its shells). Rejected when it is the last tab.
    fn close_active_tab(&mut self) {
        let Some(active) = self.active_tab_id() else {
            return;
        };
        let events = match self.state.apply(Command::CloseTab {
            origin: Origin::Client(self.client),
            workspace: self.ws.clone(),
            tab: active,
        }) {
            Ok(e) => e,
            Err(_) => return, // e.g. last tab — keep it
        };
        for e in &events {
            if let Event::TabClosed { terminals, .. } = e {
                for t in terminals {
                    self.panes.remove(t);
                }
            }
        }
        // Closing a tab may hide the tab bar (2→1 tabs), growing the content
        // height → re-derive the viewport.
        self.reflow();
    }

    /// Make `wid` the active session, re-attaching the local client as its controller
    /// (takeover releases the previous session's lease → it freezes, G3; shells keep
    /// running in the background). Reflows the new session to the current size.
    fn switch_session(&mut self, wid: WorkspaceId) {
        if self.ws == wid || self.state.workspace(&wid).is_none() {
            return;
        }
        self.ws = wid.clone();
        let cols = self.content_cols().max(1);
        let rows = self.content_rows().max(1);
        let _ = self.state.apply(Command::Attach {
            client: self.client,
            workspace: wid,
            role: Role::Controller,
            takeover: true,
            cols,
            rows,
        });
        self.sync_sizes();
    }

    /// Create a new session (a fresh single-pane workspace) and switch to it. Rolls
    /// the session back if the PTY can't spawn (never leave a blank session).
    fn new_session(&mut self) {
        let n = self.next_session;
        self.next_session += 1;
        let id = WorkspaceId::new(format!("s{n}"));
        let cols = self.content_cols().max(1);
        let rows = self.content_rows().max(1);
        let (_tab, _pane, term0) =
            self.state
                .create_workspace(id.clone(), None, Rect { cols, rows });
        match PaneTerm::spawn_with_env(
            self.cols.max(1),
            self.rows.max(1),
            None,
            None,
            &self.sock_env,
        ) {
            Some(pt) => {
                self.panes.insert(term0, pt);
                self.switch_session(id);
            }
            None => {
                // Spawn failed — drop the just-created session so state never holds a
                // session whose pane has no live terminal.
                if let Some(terms) = self.state.remove_workspace(&id) {
                    for t in &terms {
                        self.panes.remove(t);
                    }
                }
            }
        }
    }

    /// Switch to the next/previous session (wrapping): `+1` next, `-1` previous.
    fn cycle_session(&mut self, delta: i32) {
        let ids = self.session_ids();
        if ids.len() < 2 {
            return;
        }
        let cur = self.active_session_index();
        let next = ids[(cur as i32 + delta).rem_euclid(ids.len() as i32) as usize].clone();
        self.switch_session(next);
    }

    /// Switch to the session at 0-based `index` (as listed by `list-sessions`).
    fn select_session_index(&mut self, index: usize) {
        if let Some(id) = self.session_ids().get(index).cloned() {
            self.switch_session(id);
        }
    }

    /// Locate a terminal's `(workspace, tab, pane)` across ALL sessions and tabs (not
    /// just the active one), so a background session's/tab's exited shell is reaped.
    fn locate_terminal(&self, term: &TerminalId) -> Option<(WorkspaceId, TabId, PaneId)> {
        for wid in self.state.workspace_ids() {
            let Some(w) = self.state.workspace(&wid) else {
                continue;
            };
            for t in &w.tabs {
                for p in t.layout.panes() {
                    if t.layout.terminal_of(&p) == Some(term) {
                        return Some((wid.clone(), t.id.clone(), p));
                    }
                }
            }
        }
        None
    }

    // --- control API (spec §3): applied on the main loop (single writer) ---

    /// Handle one control request, mutating state as the sole writer. Returns the
    /// wire response. Never blocks.
    pub fn handle_control(&mut self, req: &control::Req) -> control::Resp {
        use control::{PaneInfo, Req, Resp};
        match req {
            Req::List => {
                let order = self.pane_order();
                let focused = self.focused_pane();
                let rects = self.layout();
                let panes = order
                    .iter()
                    .enumerate()
                    .map(|(index, p)| {
                        let term = self
                            .state
                            .workspace(&self.ws)
                            .and_then(|w| w.tab(&w.active_tab))
                            .and_then(|t| t.layout.terminal_of(p).cloned());
                        let (cols, rows) = term
                            .as_ref()
                            .and_then(|tid| rects.iter().find(|r| &r.terminal == tid))
                            .map(|r| (r.cols, r.rows))
                            .unwrap_or((0, 0));
                        let is_agent = self.pane_label_at(index).map(|l| l.kind)
                            == Some(procinfo::Kind::Agent);
                        let kind = if is_agent {
                            "agent"
                        } else if self.pane_label_at(index).map(|l| l.kind)
                            == Some(procinfo::Kind::Shell)
                        {
                            "shell"
                        } else {
                            "other"
                        };
                        let status = match (is_agent, term.as_ref()) {
                            (true, Some(tid)) => self.agent_status(tid).to_string(),
                            _ => String::new(),
                        };
                        PaneInfo {
                            index,
                            id: p.to_string(),
                            focused: focused.as_ref() == Some(p),
                            cols,
                            rows,
                            label: self.pane_label(index),
                            kind: kind.to_string(),
                            status,
                        }
                    })
                    .collect();
                let fi = focused
                    .and_then(|f| order.iter().position(|p| p == &f))
                    .unwrap_or(0);
                Resp::list(panes, fi)
            }
            Req::Split { dir } => {
                let d = match dir.as_str() {
                    "down" => Dir::Down,
                    "right" => Dir::Right,
                    other => return Resp::err(format!("bad dir '{other}' (right|down)")),
                };
                let before = self.pane_order().len();
                self.split(d);
                if self.pane_order().len() > before {
                    Resp::ok()
                } else {
                    Resp::err("split failed (could not spawn a shell)")
                }
            }
            Req::ResizePane { index, dir } => {
                let (axis, grow) = match dir.as_str() {
                    "right" => (Dir::Right, true),
                    "left" => (Dir::Right, false),
                    "down" => (Dir::Down, true),
                    "up" => (Dir::Down, false),
                    other => return Resp::err(format!("bad dir '{other}' (left|right|up|down)")),
                };
                match self.pane_order().get(*index).cloned() {
                    Some(pane) => {
                        let _ = self.state.apply(Command::ResizePane {
                            origin: Origin::Client(self.client),
                            workspace: self.ws.clone(),
                            pane,
                            axis,
                            grow,
                        });
                        self.sync_sizes();
                        Resp::ok()
                    }
                    None => Resp::err(format!("no pane at index {index}")),
                }
            }
            Req::Focus { index } => match self.pane_order().get(*index).cloned() {
                Some(pane) => {
                    let _ = self.state.apply(Command::FocusPane {
                        client: self.client,
                        pane,
                    });
                    Resp::ok()
                }
                None => Resp::err(format!("no pane at index {index}")),
            },
            Req::Close { index } => match self.pane_order().get(*index).cloned() {
                Some(pane) => match self.close_pane(pane) {
                    Ok(()) => Resp::ok(),
                    Err(e) => Resp::err(e.to_string()),
                },
                None => Resp::err(format!("no pane at index {index}")),
            },
            Req::SendKeys { index, text } => {
                let order = self.pane_order();
                let Some(pane) = order.get(*index) else {
                    return Resp::err(format!("no pane at index {index}"));
                };
                let term = self
                    .state
                    .workspace(&self.ws)
                    .and_then(|w| w.tab(&w.active_tab))
                    .and_then(|t| t.layout.terminal_of(pane).cloned());
                match term.and_then(|tid| self.panes.get(&tid)) {
                    Some(pt) => {
                        pt.input(text.as_bytes());
                        Resp::ok()
                    }
                    None => Resp::err("pane has no live terminal"),
                }
            }
            Req::ListTabs => {
                let Some(w) = self.state.workspace(&self.ws) else {
                    return Resp::err("no workspace");
                };
                let active_id = w.active_tab.clone();
                let mut active_idx = 0;
                let infos = w
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let panes = t.layout.panes();
                        let agents = panes
                            .iter()
                            .filter(|p| {
                                t.layout
                                    .terminal_of(p)
                                    .and_then(|tid| self.labels.get(tid))
                                    .map(|l| l.kind == procinfo::Kind::Agent)
                                    .unwrap_or(false)
                            })
                            .count();
                        let active = t.id == active_id;
                        if active {
                            active_idx = i;
                        }
                        control::TabInfo {
                            index: i,
                            id: t.id.to_string(),
                            active,
                            panes: panes.len(),
                            agents,
                        }
                    })
                    .collect();
                Resp::tab_list(infos, active_idx)
            }
            Req::NewTab => {
                let before = self.tab_count();
                self.new_tab();
                if self.tab_count() > before {
                    Resp::ok()
                } else {
                    Resp::err("new-tab failed (could not spawn a shell)")
                }
            }
            Req::SelectTab { index } => {
                if self.tab_ids().get(*index).is_some() {
                    self.select_tab_index(*index);
                    Resp::ok()
                } else {
                    Resp::err(format!("no tab at index {index}"))
                }
            }
            Req::ListSessions => {
                let ids = self.session_ids();
                let active_idx = self.active_session_index();
                let infos = ids
                    .iter()
                    .enumerate()
                    .map(|(i, wid)| {
                        let (tabs, panes, agents) = self
                            .state
                            .workspace(wid)
                            .map(|w| {
                                let mut panes = 0usize;
                                let mut agents = 0usize;
                                for t in &w.tabs {
                                    for p in t.layout.panes() {
                                        panes += 1;
                                        let is_agent = t
                                            .layout
                                            .terminal_of(&p)
                                            .and_then(|tid| self.labels.get(tid))
                                            .map(|l| l.kind == procinfo::Kind::Agent)
                                            .unwrap_or(false);
                                        if is_agent {
                                            agents += 1;
                                        }
                                    }
                                }
                                (w.tabs.len(), panes, agents)
                            })
                            .unwrap_or((0, 0, 0));
                        control::SessionInfo {
                            index: i,
                            id: wid.to_string(),
                            active: i == active_idx,
                            tabs,
                            panes,
                            agents,
                        }
                    })
                    .collect();
                Resp::session_list(infos, active_idx)
            }
            Req::NewSession => {
                let before = self.session_count();
                self.new_session();
                if self.session_count() > before {
                    Resp::ok()
                } else {
                    Resp::err("new-session failed (could not spawn a shell)")
                }
            }
            Req::SelectSession { index } => {
                if self.session_ids().get(*index).is_some() {
                    self.select_session_index(*index);
                    Resp::ok()
                } else {
                    Resp::err(format!("no session at index {index}"))
                }
            }
            // Intercepted by the server before it reaches here; answered ok for a
            // (hypothetical) direct call so the match stays exhaustive.
            Req::KillServer => Resp::ok(),
        }
    }

    /// Route one key event through the mux (popup → prefix → direct-nav → shell).
    /// Returns [`KeyAction::Detach`] on the detach chord. The `Ctrl-b` prefix state is
    /// PER-CLIENT (`prefix`, owned by the caller) so a chord can't span connections
    /// when several clients share input; the popup is shared workspace state.
    pub fn feed_key(&mut self, k: KeyEvent, prefix: &mut bool) -> KeyAction {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

        // The switcher captures all input while open. Clear THIS client's pending
        // prefix too: the popup is shared state, so a `Ctrl-b` armed on one client
        // must not survive another client's popup interaction and fire a stray mux
        // command (close/detach) after the popup closes.
        if self.popup.is_some() {
            *prefix = false;
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => self.popup_move(1),
                KeyCode::Char('k') | KeyCode::Up => self.popup_move(-1),
                KeyCode::Enter => self.popup_select(),
                KeyCode::Esc | KeyCode::Char('\u{6}') => self.popup = None,
                KeyCode::Char('f') if ctrl => self.popup = None,
                _ => {}
            }
            return KeyAction::Continue;
        }

        // Scrollback mode (`Ctrl-b [`): keys drive the BOUND pane's history (bound at
        // entry, so a focus change can't strand it). If that pane is gone, drop out.
        if let Some(term) = self.scroll_pane.clone() {
            if !self.panes.contains_key(&term) {
                self.scroll_pane = None;
            } else {
                let page = (self.term_rows(&term) / 2).max(1) as i32;
                match k.code {
                    KeyCode::Char('k') | KeyCode::Up => self.scroll_term(&term, 1),
                    KeyCode::Char('j') | KeyCode::Down => self.scroll_term(&term, -1),
                    KeyCode::PageUp => self.scroll_term(&term, page),
                    KeyCode::PageDown => self.scroll_term(&term, -page),
                    KeyCode::Char('u') if ctrl => self.scroll_term(&term, page),
                    KeyCode::Char('d') if ctrl => self.scroll_term(&term, -page),
                    KeyCode::Char('g') => self.scroll_term(&term, 1_000_000), // clamps to top
                    // `G`/`q`/`Esc` return the bound pane to the live bottom AND exit.
                    KeyCode::Char('G') | KeyCode::Char('q') | KeyCode::Esc => {
                        self.scroll_pane = None;
                        self.scroll_term_to_bottom(&term);
                    }
                    _ => {}
                }
                *prefix = false;
                return KeyAction::Continue;
            }
        }

        if *prefix {
            *prefix = false;
            match k.code {
                KeyCode::Char('%') => self.split(Dir::Right),
                KeyCode::Char('"') => self.split(Dir::Down),
                // enter scrollback, bound to the currently focused pane
                KeyCode::Char('[') => self.scroll_pane = self.focused_terminal(),
                KeyCode::Char('o') => self.focus_next(),
                KeyCode::Char('x') => self.close_focused(),
                KeyCode::Char('s') => self.toggle_sidebar(),
                // tabs: `c` new, `n`/`p` next/prev, `&` close, `1`-`9` jump
                KeyCode::Char('c') => self.new_tab(),
                KeyCode::Char('n') => self.cycle_tab(1),
                KeyCode::Char('p') => self.cycle_tab(-1),
                KeyCode::Char('&') => self.close_active_tab(),
                KeyCode::Char(d @ '1'..='9') => self.select_tab_index(d as usize - '1' as usize),
                // sessions (workspaces): `C` new, `)`/`(` next/prev (tmux-style)
                KeyCode::Char('C') => self.new_session(),
                KeyCode::Char(')') => self.cycle_session(1),
                KeyCode::Char('(') => self.cycle_session(-1),
                // Detach — leave the server + shells running. Both `d` and `q`
                // detach; NO key kills the server (that would drop every shell).
                KeyCode::Char('d') | KeyCode::Char('q') => return KeyAction::Detach,
                // directional pane focus — vim `hjkl` (preferred) or arrows (fallback)
                KeyCode::Char('h') | KeyCode::Left => self.focus_dir(FocusDir::Left),
                KeyCode::Char('j') | KeyCode::Down => self.focus_dir(FocusDir::Down),
                KeyCode::Char('k') | KeyCode::Up => self.focus_dir(FocusDir::Up),
                KeyCode::Char('l') | KeyCode::Right => self.focus_dir(FocusDir::Right),
                // pane resize — vim-uppercase `HJKL`: L/H widen/narrow, J/K taller/shorter
                KeyCode::Char('L') => self.resize_focused(Dir::Right, true),
                KeyCode::Char('H') => self.resize_focused(Dir::Right, false),
                KeyCode::Char('J') => self.resize_focused(Dir::Down, true),
                KeyCode::Char('K') => self.resize_focused(Dir::Down, false),
                _ => {}
            }
            return KeyAction::Continue;
        }

        // Direct (prefix-less) pane navigation: Ctrl+Shift+arrow.
        if let Some(dir) = ctrl_shift_nav(k.code, k.modifiers) {
            self.focus_dir(dir);
            return KeyAction::Continue;
        }

        // `Ctrl-f` opens the switcher; `Ctrl-b` enters prefix mode; both may arrive
        // as a raw control byte or `Char(_)`+CONTROL. Everything else is shell input.
        let is_popup_key =
            matches!(k.code, KeyCode::Char('\u{6}')) || (k.code == KeyCode::Char('f') && ctrl);
        let is_prefix_key =
            matches!(k.code, KeyCode::Char('\u{2}')) || (k.code == KeyCode::Char('b') && ctrl);
        if is_popup_key {
            self.open_popup();
        } else if is_prefix_key {
            *prefix = true;
        } else if let Some(bytes) = key_to_bytes(k.code, k.modifiers) {
            self.input_focused(&bytes);
        }
        KeyAction::Continue
    }

    fn focus_next(&mut self) {
        let order = self.pane_order();
        if order.len() < 2 {
            return;
        }
        let Some(cur) = self.focused_pane() else {
            return;
        };
        let idx = order.iter().position(|p| p == &cur).unwrap_or(0);
        let next = order[(idx + 1) % order.len()].clone();
        let _ = self.state.apply(Command::FocusPane {
            client: self.client,
            pane: next,
        });
    }

    /// Focus the nearest pane in a direction, by rect-center heuristic.
    fn focus_dir(&mut self, dir: FocusDir) {
        let rects = self.layout();
        let Some(cur_term) = self.focused_terminal() else {
            return;
        };
        let Some(cur) = rects.iter().find(|r| r.terminal == cur_term) else {
            return;
        };
        let ccx = cur.x as i32 + cur.cols as i32 / 2;
        let ccy = cur.y as i32 + cur.rows as i32 / 2;
        let mut best: Option<(&PaneRect, i32)> = None;
        for r in &rects {
            if r.terminal == cur_term {
                continue;
            }
            let cx = r.x as i32 + r.cols as i32 / 2;
            let cy = r.y as i32 + r.rows as i32 / 2;
            let (dx, dy) = (cx - ccx, cy - ccy);
            let ok = match dir {
                FocusDir::Left => dx < 0 && dx.abs() >= dy.abs(),
                FocusDir::Right => dx > 0 && dx.abs() >= dy.abs(),
                FocusDir::Up => dy < 0 && dy.abs() >= dx.abs(),
                FocusDir::Down => dy > 0 && dy.abs() >= dx.abs(),
            };
            if !ok {
                continue;
            }
            let dist = dx * dx + dy * dy;
            if best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((r, dist));
            }
        }
        if let Some((r, _)) = best
            && let Some(pane) = self.pane_of_terminal(&r.terminal)
        {
            let _ = self.state.apply(Command::FocusPane {
                client: self.client,
                pane,
            });
        }
    }

    fn input_focused(&self, bytes: &[u8]) {
        if let Some(term) = self.focused_terminal()
            && let Some(pt) = self.panes.get(&term)
        {
            pt.scroll_to_bottom(); // typing returns to the live bottom
            pt.input(bytes);
        }
    }

    // --- scrollback ---

    /// Handle a forwarded mouse action at frame cell `(x, y)`: the wheel scrolls the
    /// pane UNDER the cursor (falling back to the focused pane); a left click focuses
    /// the pane under the cursor.
    pub fn mouse_at(&mut self, x: u16, y: u16, kind: MouseKind) {
        let target = self
            .layout()
            .into_iter()
            .find(|r| x >= r.x && x < r.x + r.cols && y >= r.y && y < r.y + r.rows);
        match kind {
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                let lines = if matches!(kind, MouseKind::ScrollUp) {
                    SCROLL_STEP
                } else {
                    -SCROLL_STEP
                };
                let term = target
                    .map(|r| r.terminal)
                    .or_else(|| self.focused_terminal());
                if let Some(t) = term
                    && let Some(pt) = self.panes.get(&t)
                {
                    pt.scroll(lines);
                }
            }
            MouseKind::Click => {
                if let Some(r) = target
                    && let Some(pane) = self.pane_of_terminal(&r.terminal)
                {
                    let _ = self.state.apply(Command::FocusPane {
                        client: self.client,
                        pane,
                    });
                }
            }
        }
    }

    /// Scroll a specific terminal by `lines` (positive = up / older).
    fn scroll_term(&self, term: &TerminalId, lines: i32) {
        if let Some(pt) = self.panes.get(term) {
            pt.scroll(lines);
        }
    }

    fn scroll_term_to_bottom(&self, term: &TerminalId) {
        if let Some(pt) = self.panes.get(term) {
            pt.scroll_to_bottom();
        }
    }

    /// On-screen rows of a terminal (the page size for page scrolling).
    fn term_rows(&self, term: &TerminalId) -> u16 {
        self.layout()
            .into_iter()
            .find(|r| &r.terminal == term)
            .map(|r| r.rows)
            .unwrap_or(24)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        // reflow derives the content-area viewport (minus chrome) and pushes it.
        self.reflow();
    }

    // --- sidebar + popup ---

    /// Toggle the neovim-style left panel (honored only when wide enough). The
    /// pane content area shrinks/grows, so resize the PTYs to match.
    fn toggle_sidebar(&mut self) {
        self.sidebar = !self.sidebar;
        // The content width changes → re-derive the viewport, not just PTY sizes.
        self.reflow();
    }

    /// Open the `Ctrl-f` switcher, preselecting the focused pane's row.
    fn open_popup(&mut self) {
        let order = self.pane_order();
        let sel = self
            .focused_pane()
            .and_then(|f| order.iter().position(|p| p == &f))
            .unwrap_or(0);
        self.popup = Some(sel);
    }

    /// Keep the popup selection within the current pane list (or close the popup
    /// if no panes remain). Called each frame so an async pane exit can't leave a
    /// stale selection index.
    pub fn reconcile_popup(&mut self) {
        if let Some(sel) = self.popup {
            let n = self.pane_order().len();
            if n == 0 {
                self.popup = None;
            } else if sel >= n {
                self.popup = Some(n - 1);
            }
        }
    }

    /// Move the popup selection by `delta`, wrapping.
    fn popup_move(&mut self, delta: i32) {
        let n = self.pane_order().len() as i32;
        if n == 0 {
            self.popup = None;
            return;
        }
        if let Some(sel) = self.popup {
            self.popup = Some((sel as i32 + delta).rem_euclid(n) as usize);
        }
    }

    /// Focus the selected pane and close the popup.
    fn popup_select(&mut self) {
        if let Some(sel) = self.popup.take()
            && let Some(pane) = self.pane_order().get(sel).cloned()
        {
            let _ = self.state.apply(Command::FocusPane {
                client: self.client,
                pane,
            });
        }
    }

    /// Close panes whose shell has exited (in ANY tab). Returns true if the app is
    /// now empty (last pane of the last tab exited → quit). A tab whose last pane
    /// exits is closed whole; other tabs keep running in the background.
    pub fn reap_exited(&mut self) -> bool {
        let exited: Vec<TerminalId> = self
            .panes
            .iter()
            .filter(|(_, p)| p.has_exited())
            .map(|(id, _)| id.clone())
            .collect();
        // Fast path: nothing exited this tick — don't reflow (which would bump the
        // workspace rev + resize every PTY) on every idle main-loop iteration.
        if exited.is_empty() {
            return self.is_empty();
        }
        for term in exited {
            let Some((wid, tab_id, pane)) = self.locate_terminal(&term) else {
                // No longer in any session/tab (already collapsed) — drop the runtime.
                self.panes.remove(&term);
                continue;
            };
            // Read the tab/session shape of the OWNING session (may be a background
            // one), using Api-origin mutations since the client controls only the
            // active session.
            let (tab_is_single, ws_tab_count) = self
                .state
                .workspace(&wid)
                .map(|w| {
                    let single = w
                        .tab(&tab_id)
                        .map(|t| t.layout.is_single_leaf())
                        .unwrap_or(true);
                    (single, w.tabs.len())
                })
                .unwrap_or((true, 0));

            if !tab_is_single {
                // The tab has other panes — collapse just this one.
                match self.state.apply(Command::ClosePane {
                    origin: Origin::Api,
                    workspace: wid,
                    pane,
                    if_rev: None,
                }) {
                    Ok(evs) => {
                        for e in &evs {
                            if let Event::PaneClosed { terminal, .. } = e {
                                self.panes.remove(terminal);
                            }
                        }
                        // Reflow the OWNING session's survivors (works for background
                        // sessions, which `reflow()` below would otherwise miss).
                        self.apply_resized_events(&evs);
                    }
                    Err(_) => {
                        self.panes.remove(&term);
                    }
                }
            } else if ws_tab_count > 1 {
                // Last pane of this tab, but other tabs survive — close the tab.
                match self.state.apply(Command::CloseTab {
                    origin: Origin::Api,
                    workspace: wid,
                    tab: tab_id,
                }) {
                    Ok(evs) => {
                        for e in &evs {
                            if let Event::TabClosed { terminals, .. } = e {
                                for t in terminals {
                                    self.panes.remove(t);
                                }
                            }
                        }
                        // Reflow the owning session's now-active tab (background-safe).
                        self.apply_resized_events(&evs);
                    }
                    Err(_) => {
                        self.panes.remove(&term);
                    }
                }
            } else if self.session_count() > 1 {
                // Last pane of the last tab of a NON-last session — drop the whole
                // session; if it was the active one, switch to another.
                let was_active = self.ws == wid;
                match self.state.remove_workspace(&wid) {
                    Some(terms) => {
                        for t in &terms {
                            self.panes.remove(t);
                        }
                        if was_active
                            && let Some(next) = self.state.workspace_ids().into_iter().next()
                        {
                            self.switch_session(next);
                        }
                    }
                    None => {
                        self.panes.remove(&term);
                    }
                }
            } else {
                // Last pane / last tab / only session — its exit means "quit": drop it
                // so the app becomes empty and the main loop terminates.
                self.panes.remove(&term);
            }
        }
        // Reaping may have closed a tab or session (changing chrome) → re-derive.
        self.reflow();
        self.is_empty()
    }

    // --- render ---

    /// Render the whole composite (panes + dividers + sidebar + tab bar + popup)
    /// into `buf`, returning the desired cursor position (or `None` when a popup
    /// captures input). Rendering targets a plain `Buffer` — not a `Frame` — so the
    /// same code path serves both the local terminal and the (headless) server that
    /// ships cell diffs to a remote client.
    pub fn render_to(&self, buf: &mut Buffer) -> Option<Position> {
        let area = buf.area;
        let rects = self.layout();
        let focused_term = self.focused_terminal();
        let mut cursor_pos: Option<Position> = None;

        // 1) pane contents (inactive panes dimmed so the focused one stands out).
        for rect in &rects {
            let Some(pt) = self.panes.get(&rect.terminal) else {
                continue;
            };
            let snap = pt.snapshot();
            let is_focused = Some(&rect.terminal) == focused_term.as_ref();
            for (ry, row) in snap.cells.iter().enumerate() {
                if ry as u16 >= rect.rows {
                    break;
                }
                let y = rect.y + ry as u16;
                if y >= area.height {
                    break;
                }
                for (rx, cell) in row.iter().enumerate() {
                    if rx as u16 >= rect.cols {
                        break;
                    }
                    let x = rect.x + rx as u16;
                    if x >= area.width {
                        break;
                    }
                    if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                        if cell.spacer {
                            bc.set_skip(true);
                            continue;
                        }
                        let mut style =
                            Style::default().fg(to_color(cell.fg)).bg(to_color(cell.bg));
                        if cell.bold {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        if cell.reverse {
                            style = style.add_modifier(Modifier::REVERSED);
                        }
                        if !is_focused {
                            style = style.add_modifier(Modifier::DIM);
                        }
                        bc.set_symbol(&cell.sym);
                        bc.set_style(style);
                    }
                }
            }
            if is_focused {
                let (cx, cy) = snap.cursor;
                if cx < rect.cols && cy < rect.rows {
                    let (ax, ay) = (rect.x + cx, rect.y + cy);
                    if ax < area.width && ay < area.height {
                        cursor_pos = Some(Position::new(ax, ay));
                    }
                }
            }
        }

        // 2) dividers in the 1-cell gaps between panes; accent the focused pane's.
        let mut covered = vec![vec![false; area.width as usize]; area.height as usize];
        let mut focus_rect: Option<PaneRect> = None;
        for rect in &rects {
            if Some(&rect.terminal) == focused_term.as_ref() {
                focus_rect = Some(rect.clone());
            }
            for yy in rect.y..(rect.y + rect.rows).min(area.height) {
                for xx in rect.x..(rect.x + rect.cols).min(area.width) {
                    covered[yy as usize][xx as usize] = true;
                }
            }
        }
        let content_x = self.content_x();
        let content_rows = self.content_rows();
        for yy in 0..area.height {
            for xx in 0..area.width {
                // The sidebar owns the left strip and the status bar owns the bottom
                // row; don't paint dividers over either.
                if yy >= content_rows || xx < content_x || covered[yy as usize][xx as usize] {
                    continue;
                }
                let left = xx > 0 && covered[yy as usize][(xx - 1) as usize];
                let right = (xx + 1) < area.width && covered[yy as usize][(xx + 1) as usize];
                let glyph = if left || right { "│" } else { "─" };
                let accent = focus_rect
                    .as_ref()
                    .map(|fr| divider_touches(fr, xx, yy))
                    .unwrap_or(false);
                let color = if accent { Color::Cyan } else { Color::DarkGray };
                if let Some(bc) = buf.cell_mut(Position::new(xx, yy)) {
                    bc.set_symbol(glyph);
                    bc.set_style(Style::default().fg(color));
                }
            }
        }

        // 3) the neovim-style left panel, over the reserved strip.
        if self.sidebar_visible() {
            self.render_sidebar(buf);
        }

        // 4) the always-on bottom status bar (session · tabs · scroll/agents/clock/host).
        self.render_status_bar(buf);

        // 5) the Ctrl-f switcher popup, over everything.
        if self.popup.is_some() {
            self.render_popup(buf);
        }

        // The shell cursor shows only when nothing modal is capturing input.
        if self.popup.is_none() && self.scroll_pane.is_none() {
            cursor_pos
        } else {
            None
        }
    }

    /// The current viewport size (terminal size the app renders at).
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Refresh foreground-process labels at most ~2 Hz (throttled internally), so
    /// the server loop can call it every tick without hammering `ps`.
    pub fn maybe_refresh_labels(&mut self) {
        if self.last_labels.elapsed() >= Duration::from_millis(500) {
            self.refresh_labels();
            self.refresh_agent_statuses();
            self.refresh_branches();
        }
    }

    /// Recompute each session's git branch from its focused pane's shell cwd (for the
    /// `spaces` subtitle). Rebuilt each pass so a `cd` out of a repo clears it.
    fn refresh_branches(&mut self) {
        let mut next = HashMap::new();
        for wid in self.state.workspace_ids() {
            let branch = self
                .state
                .workspace(&wid)
                .and_then(|w| w.tab(&w.active_tab))
                .and_then(|t| t.layout.terminal_of(&t.focused).cloned())
                .and_then(|tid| self.panes.get(&tid))
                .and_then(|pane| pane.pid())
                .and_then(procinfo::process_cwd)
                .and_then(|cwd| gitinfo::branch(&cwd));
            if let Some(branch) = branch {
                next.insert(wid, branch);
            }
        }
        self.branches = next;
    }

    /// Recompute each agent pane's status (working/ready/blocked/idle) from Claude's
    /// session file + a screen-text fallback (`agentstate`). Agent panes only; the map
    /// is rebuilt each pass so closed/reclassified panes drop out.
    fn refresh_agent_statuses(&mut self) {
        let mut next = HashMap::new();
        for (tid, pane) in &self.panes {
            let Some(label) = self.labels.get(tid) else {
                continue;
            };
            if label.kind != procinfo::Kind::Agent {
                continue;
            }
            let status = agentstate::resolve(Some(label.pid), &pane.snapshot());
            next.insert(tid.clone(), status);
        }
        self.agent_statuses = next;
    }

    /// An agent pane's rolled-up status label for the sidebar (`idle` if unknown).
    fn agent_status(&self, tid: &TerminalId) -> &'static str {
        self.agent_statuses
            .get(tid)
            .map(|s| s.label())
            .unwrap_or("idle")
    }

    /// A one-line label for a pane row (index + short cwd basename if available).
    /// Refresh every pane's foreground-process label from a single `ps` sweep
    /// (throttled by the caller to ~2 Hz — never per frame).
    fn refresh_labels(&mut self) {
        let tree = procinfo::ProcTree::snapshot();
        let mut next = HashMap::new();
        for (tid, pane) in &self.panes {
            // The terminal's foreground process GROUP is the real foreground
            // command (resolve a live member — the pgid leader may have exited in
            // a pipeline); fall back to descending from the shell pid.
            let label = pane
                .foreground_pgid()
                .and_then(|pg| tree.command_of_pgroup(pg))
                .or_else(|| pane.pid().and_then(|p| tree.foreground(p)));
            if let Some(label) = label {
                next.insert(tid.clone(), label);
            }
        }
        self.labels = next;
        self.last_labels = std::time::Instant::now();
    }

    /// The terminal id backing the pane at list index `idx`.
    fn terminal_at(&self, idx: usize) -> Option<TerminalId> {
        let pane = self.pane_order().get(idx)?.clone();
        let w = self.state.workspace(&self.ws)?;
        let tab = w.tab(&w.active_tab)?;
        tab.layout.terminal_of(&pane).cloned()
    }

    /// The classified label for the pane at list index `idx`, if known.
    fn pane_label_at(&self, idx: usize) -> Option<&procinfo::Label> {
        let term = self.terminal_at(idx)?;
        self.labels.get(&term)
    }

    /// Display text for the pane at list index `idx`: its foreground command
    /// (agent / shell / program), or a fallback before the first label sweep.
    fn pane_label(&self, idx: usize) -> String {
        self.pane_label_at(idx)
            .map(|l| l.text.clone())
            .unwrap_or_else(|| format!("pane {idx}"))
    }

    /// The foreground command of a session's focused pane (its "what's happening"
    /// subtitle), e.g. `claude` / `zsh` / `nvim`.
    fn session_focus_label(&self, wid: &WorkspaceId) -> String {
        let Some(w) = self.state.workspace(wid) else {
            return String::new();
        };
        let Some(t) = w.tab(&w.active_tab) else {
            return String::new();
        };
        t.layout
            .terminal_of(&t.focused)
            .and_then(|tid| self.labels.get(tid))
            .map(|l| l.text.clone())
            .unwrap_or_default()
    }

    /// Every agent pane across ALL sessions: `(space, tool, status)`.
    fn agent_rows(&self) -> Vec<(String, String, &'static str)> {
        let mut out = Vec::new();
        for wid in self.state.workspace_ids() {
            let Some(w) = self.state.workspace(&wid) else {
                continue;
            };
            let space = w.name.clone().unwrap_or_else(|| wid.to_string());
            for t in &w.tabs {
                for p in t.layout.panes() {
                    if let Some(tid) = t.layout.terminal_of(&p)
                        && let Some(label) = self.labels.get(tid)
                        && label.kind == procinfo::Kind::Agent
                    {
                        out.push((space.clone(), label.text.clone(), self.agent_status(tid)));
                    }
                }
            }
        }
        out
    }

    /// The herdr-style left panel: `spaces` (sessions, top half) + `agents` (agent
    /// panes with status·tool, bottom half). Always on when wide enough.
    fn render_sidebar(&self, buf: &mut Buffer) {
        let h = self.content_rows(); // above the bottom status bar
        let panel_bg = CAT_BASE;

        // Fill the strip + the right border column (down to the status bar).
        for y in 0..h {
            for x in 0..SIDEBAR_W {
                if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                    bc.set_symbol(" ");
                    bc.set_skip(false);
                    bc.set_style(Style::default().bg(panel_bg));
                }
            }
            if let Some(bc) = buf.cell_mut(Position::new(SIDEBAR_W, y)) {
                bc.set_symbol("│");
                bc.set_skip(false);
                bc.set_style(Style::default().fg(CAT_SURFACE0).bg(panel_bg));
            }
        }

        // Display-width-aware writer that ADVANCES `x` (so a colored dot and the name
        // after it don't overwrite each other). Clipped to the strip.
        let put = |buf: &mut Buffer, x: &mut u16, y: u16, s: &str, style: Style| {
            for ch in s.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                if cw == 0 {
                    continue;
                }
                if *x + cw > SIDEBAR_W {
                    break;
                }
                if let Some(bc) = buf.cell_mut(Position::new(*x, y)) {
                    let mut sb = [0u8; 4];
                    bc.set_symbol(ch.encode_utf8(&mut sb));
                    bc.set_skip(false);
                    bc.set_style(style);
                }
                if cw == 2
                    && let Some(bc) = buf.cell_mut(Position::new(*x + 1, y))
                {
                    bc.set_symbol(" ");
                    bc.set_skip(true);
                    bc.set_style(style);
                }
                *x += cw;
            }
        };
        let header = Style::default()
            .fg(CAT_OVERLAY)
            .bg(panel_bg)
            .add_modifier(Modifier::BOLD);
        let sub_style = Style::default().fg(CAT_OVERLAY).bg(panel_bg);

        // ---- spaces (top half) ----
        let mid = (h / 2).max(2);
        put(buf, &mut 0, 0, " spaces", header);
        let sids = self.session_ids();
        let active_si = self.active_session_index();
        let mut y = 1u16;
        for (i, sid) in sids.iter().enumerate() {
            if y + 1 >= mid {
                break;
            }
            let is_active = i == active_si;
            let name = self
                .state
                .workspace(sid)
                .and_then(|w| w.name.clone())
                .unwrap_or_else(|| sid.to_string());
            let dot_color = if is_active {
                CAT_MAUVE
            } else if self.session_has_agent(sid) {
                CAT_PEACH
            } else {
                CAT_OVERLAY
            };
            let name_style = Style::default()
                .fg(if is_active { CAT_TEXT } else { CAT_SUBTEXT })
                .bg(panel_bg)
                .add_modifier(if is_active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });
            let mut x = 1u16;
            put(
                buf,
                &mut x,
                y,
                "●",
                Style::default().fg(dot_color).bg(panel_bg),
            );
            put(buf, &mut x, y, " ", name_style);
            put(buf, &mut x, y, &name, name_style);
            // subtitle: git branch (herdr-style), falling back to the focused command
            // when the cwd isn't a git repo.
            let sub = self
                .branches
                .get(sid)
                .cloned()
                .unwrap_or_else(|| self.session_focus_label(sid));
            let mut sx = 4u16;
            put(buf, &mut sx, y + 1, &sub, sub_style);
            y += 2;
        }

        // ---- agents (bottom half) ----
        let mut y = mid;
        put(buf, &mut 0, y, " agents", header);
        y += 1;
        for (space, tool, status) in self.agent_rows() {
            if y + 1 >= h {
                break;
            }
            // tmx-style glyph + colour per status (all width-1 geometric shapes).
            let (dot, scolor) = match status {
                "working" => ("●", CAT_GREEN),
                "ready" => ("◐", CAT_BLUE),
                "blocked" => ("▲", CAT_YELLOW),
                _ => ("○", CAT_OVERLAY), // idle
            };
            let mut x = 1u16;
            put(
                buf,
                &mut x,
                y,
                dot,
                Style::default().fg(scolor).bg(panel_bg),
            );
            let name_style = Style::default()
                .fg(CAT_TEXT)
                .bg(panel_bg)
                .add_modifier(Modifier::BOLD);
            put(buf, &mut x, y, " ", name_style);
            put(buf, &mut x, y, &space, name_style);
            let mut sx = 4u16;
            put(
                buf,
                &mut sx,
                y + 1,
                &format!("{status} · {tool}"),
                sub_style,
            );
            y += 2;
        }
    }

    /// Does any pane in `tab_id` currently run a classified AI agent?
    fn tab_has_agent(&self, tab_id: &TabId) -> bool {
        let Some(w) = self.state.workspace(&self.ws) else {
            return false;
        };
        let Some(t) = w.tab(tab_id) else {
            return false;
        };
        t.layout.panes().iter().any(|p| {
            t.layout
                .terminal_of(p)
                .and_then(|tid| self.labels.get(tid))
                .map(|l| l.kind == procinfo::Kind::Agent)
                .unwrap_or(false)
        })
    }

    /// Number of agent panes across the ACTIVE session (all its tabs).
    fn active_session_agent_count(&self) -> usize {
        let Some(w) = self.state.workspace(&self.ws) else {
            return 0;
        };
        let mut n = 0;
        for t in &w.tabs {
            for p in t.layout.panes() {
                let is_agent = t
                    .layout
                    .terminal_of(&p)
                    .and_then(|tid| self.labels.get(tid))
                    .map(|l| l.kind == procinfo::Kind::Agent)
                    .unwrap_or(false);
                if is_agent {
                    n += 1;
                }
            }
        }
        n
    }

    /// The always-on bottom status bar (Catppuccin Mocha, matching the owner's tmux):
    /// LEFT = session pill + tab chips (active highlighted, agent `●`); RIGHT =
    /// scroll flag · agent count · clock · host.
    fn render_status_bar(&self, buf: &mut Buffer) {
        let area = buf.area;
        let w = area.width;
        let y = area.height.saturating_sub(1);

        // Fill the row (also lifts any wide-char skip flag left by a pane cell).
        for x in 0..w {
            if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                bc.set_symbol(" ");
                bc.set_skip(false);
                bc.set_style(Style::default().bg(CAT_BASE));
            }
        }

        // Display-width-aware: advance by each glyph's terminal width and mark the
        // continuation cell of a wide (CJK/emoji) glyph, so a wide session name
        // can't corrupt the chips to its right.
        let put = |buf: &mut Buffer, x: &mut u16, s: &str, st: Style| {
            for ch in s.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                if cw == 0 {
                    continue;
                }
                if *x + cw > w {
                    break;
                }
                if let Some(bc) = buf.cell_mut(Position::new(*x, y)) {
                    let mut sb = [0u8; 4];
                    bc.set_symbol(ch.encode_utf8(&mut sb));
                    bc.set_skip(false);
                    bc.set_style(st);
                }
                if cw == 2
                    && let Some(bc) = buf.cell_mut(Position::new(*x + 1, y))
                {
                    bc.set_symbol(" ");
                    bc.set_skip(true);
                    bc.set_style(st);
                }
                *x += cw;
            }
        };

        // LEFT: session pill + tab chips.
        let mut x = 0u16;
        let sname = self
            .state
            .workspace(&self.ws)
            .and_then(|w| w.name.clone())
            .unwrap_or_else(|| self.ws.to_string());
        put(
            buf,
            &mut x,
            &format!(" {sname} "),
            Style::default()
                .fg(CAT_BASE)
                .bg(CAT_MAUVE)
                .add_modifier(Modifier::BOLD),
        );
        put(buf, &mut x, " ", Style::default().bg(CAT_BASE));
        let ids = self.tab_ids();
        let active = self.active_tab_id();
        for (i, id) in ids.iter().enumerate() {
            let is_active = active.as_ref() == Some(id);
            let agent = self.tab_has_agent(id);
            let label = format!(" {}{} ", if agent { "●" } else { "" }, i + 1);
            let st = if is_active {
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_MAUVE)
                    .add_modifier(Modifier::BOLD)
            } else if agent {
                Style::default().fg(CAT_YELLOW).bg(CAT_BASE)
            } else {
                Style::default().fg(CAT_OVERLAY).bg(CAT_BASE)
            };
            put(buf, &mut x, &label, st);
        }

        // RIGHT: scroll · agents · clock · host, right-aligned.
        let mut segs: Vec<(String, Style)> = Vec::new();
        if self.scroll_pane.is_some() {
            segs.push((
                " SCROLL ".to_string(),
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_YELLOW)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let agents = self.active_session_agent_count();
        if agents > 0 {
            segs.push((
                format!(" ●{agents} "),
                Style::default()
                    .fg(CAT_GREEN)
                    .bg(CAT_SURFACE0)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        segs.push((
            format!(" {} ", local_hhmm()),
            Style::default().fg(CAT_SUBTEXT).bg(CAT_SURFACE0),
        ));
        segs.push((
            format!(" {} ", hostname()),
            Style::default()
                .fg(CAT_BASE)
                .bg(CAT_BLUE)
                .add_modifier(Modifier::BOLD),
        ));
        let total: u16 = segs.iter().map(|(s, _)| s.width() as u16).sum();
        // Don't overwrite the left cluster on a narrow bar.
        let mut rx = w.saturating_sub(total).max(x);
        for (s, st) in &segs {
            put(buf, &mut rx, s, *st);
        }
    }

    /// Centered `Ctrl-f` switcher over the panes: a bordered box listing panes with
    /// a selection cursor.
    fn render_popup(&self, buf: &mut Buffer) {
        let Some(sel) = self.popup else { return };
        let area = buf.area;
        let order = self.pane_order();

        // Clamp bounds so `min <= max` even on tiny/narrow terminals (the popup
        // is the narrow/mobile fallback, so it must never panic there).
        let maxw = area.width.saturating_sub(2).max(1);
        let w = (area.width / 2).clamp(24.min(maxw), maxw);
        let maxh = area.height.saturating_sub(2).max(1);
        let h = ((area.height as u32 * 3 / 5) as u16).clamp(5.min(maxh), maxh);
        let x0 = (area.width.saturating_sub(w)) / 2;
        let y0 = (area.height.saturating_sub(h)) / 2;
        let bg = Color::Rgb(20, 22, 30);
        let border = Style::default().fg(Color::Cyan).bg(bg);

        // Box background + border.
        for y in y0..(y0 + h).min(area.height) {
            for x in x0..(x0 + w).min(area.width) {
                let sym = if y == y0 && x == x0 {
                    "┌"
                } else if y == y0 && x == x0 + w - 1 {
                    "┐"
                } else if y == y0 + h - 1 && x == x0 {
                    "└"
                } else if y == y0 + h - 1 && x == x0 + w - 1 {
                    "┘"
                } else if y == y0 || y == y0 + h - 1 {
                    "─"
                } else if x == x0 || x == x0 + w - 1 {
                    "│"
                } else {
                    " "
                };
                if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                    bc.set_symbol(sym);
                    bc.set_skip(false);
                    let edge = y == y0 || y == y0 + h - 1 || x == x0 || x == x0 + w - 1;
                    bc.set_style(if edge {
                        border
                    } else {
                        Style::default().bg(bg)
                    });
                }
            }
        }

        let put =
            |buf: &mut ratatui::buffer::Buffer, y: u16, x: u16, maxw: u16, s: &str, st: Style| {
                for (i, ch) in s.chars().take(maxw as usize).enumerate() {
                    if let Some(bc) = buf.cell_mut(Position::new(x + i as u16, y)) {
                        let mut sb = [0u8; 4];
                        bc.set_symbol(ch.encode_utf8(&mut sb));
                        // Clear any wide-char skip flag inherited from the pane cell
                        // underneath, or buffer-diffing omits the overlay cell.
                        bc.set_skip(false);
                        bc.set_style(st);
                    }
                }
            };

        // Title in the top border.
        put(
            buf,
            y0,
            x0 + 2,
            w.saturating_sub(4),
            " switch pane ",
            Style::default()
                .fg(Color::Cyan)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        );

        let inner_x = x0 + 2;
        let inner_w = w.saturating_sub(4);
        for (i, _pane) in order.iter().enumerate() {
            let y = y0 + 1 + i as u16;
            if y >= y0 + h - 1 {
                break;
            }
            let selected = i == sel;
            let kind = self.pane_label_at(i).map(|l| l.kind);
            let dot = if kind == Some(procinfo::Kind::Agent) {
                "●"
            } else {
                " "
            };
            let row = format!(
                "{} {i}  {dot}{}",
                if selected { "▸" } else { " " },
                self.pane_label(i)
            );
            let st = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if kind == Some(procinfo::Kind::Agent) {
                Style::default()
                    .fg(Color::Yellow)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray).bg(bg)
            };
            // pad the selected row across the inner width so the highlight is a full bar
            let padded = format!("{row:<width$}", width = inner_w as usize);
            put(buf, y, inner_x, inner_w, &padded, st);
        }
    }
}

/// Does the gap cell `(x, y)` border the focused rect (so its divider is accented)?
fn divider_touches(fr: &PaneRect, x: u16, y: u16) -> bool {
    let in_y = y >= fr.y && y < fr.y + fr.rows;
    let in_x = x >= fr.x && x < fr.x + fr.cols;
    let vert = in_y && (x + 1 == fr.x || x == fr.x + fr.cols);
    let horiz = in_x && (y + 1 == fr.y || y == fr.y + fr.rows);
    vert || horiz
}

/// Local `HH:MM` for the status-bar clock (via libc `localtime_r`, no chrono dep).
fn local_hhmm() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `secs` is a valid time_t; `tm` is a zeroed, correctly-sized out-param.
    let r = unsafe { libc::localtime_r(&secs as *const libc::time_t, &mut tm) };
    if r.is_null() {
        return "--:--".to_string();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// The short hostname (before the first dot) for the status bar.
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: writing at most `buf.len()` bytes into our own buffer.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if r != 0 {
        return "host".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let full = String::from_utf8_lossy(&buf[..end]);
    full.split('.').next().unwrap_or("host").to_string()
}

fn to_color(c: CellColor) -> Color {
    match c {
        CellColor::Default => Color::Reset,
        CellColor::Indexed(i) => Color::Indexed(i),
        CellColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Map a `Ctrl+Shift+<arrow>` chord to a focus direction, or `None` for anything
/// else. Requires BOTH modifiers so plain arrows still reach the shell and
/// `Ctrl+<arrow>` stays free for shell word-motion. Standard xterm CSI modifier
/// encoding (`ESC[1;6<dir>`) delivers this without kitty keyboard enhancement, so
/// it works over ssh / inside tmux (with `xterm-keys on`).
fn ctrl_shift_nav(code: KeyCode, mods: KeyModifiers) -> Option<FocusDir> {
    if !(mods.contains(KeyModifiers::CONTROL) && mods.contains(KeyModifiers::SHIFT)) {
        return None;
    }
    match code {
        // vim `hjkl` (preferred) or arrows (fallback). Match BOTH cases: holding Shift
        // usually reports the letter uppercased (`H`/`J`/…). This chord is best-effort
        // (some terminals can't distinguish Ctrl+Shift+letter without the kitty
        // keyboard protocol) — the always-reliable hjkl path is `Ctrl-b h/j/k/l`.
        // Plain hjkl still reaches the shell — only the chord navigates.
        KeyCode::Char('h' | 'H') | KeyCode::Left => Some(FocusDir::Left),
        KeyCode::Char('j' | 'J') | KeyCode::Down => Some(FocusDir::Down),
        KeyCode::Char('k' | 'K') | KeyCode::Up => Some(FocusDir::Up),
        KeyCode::Char('l' | 'L') | KeyCode::Right => Some(FocusDir::Right),
        _ => None,
    }
}

/// Translate a key event into the bytes a PTY expects.
fn key_to_bytes(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let esc = |mut v: Vec<u8>| {
        if alt {
            v.insert(0, 0x1b);
        }
        v
    };

    let bytes = match code {
        KeyCode::Char(ch) => {
            let utf8 = |ch: char| {
                let mut s = [0u8; 4];
                ch.encode_utf8(&mut s).as_bytes().to_vec()
            };
            if ctrl {
                // Only the ASCII control combinations map to control bytes;
                // Ctrl with digits / non-ASCII passes the char through unchanged.
                match ch {
                    ' ' | '@' => vec![0], // Ctrl-Space / Ctrl-@ → NUL
                    'a'..='z' | 'A'..='Z' => vec![(ch.to_ascii_lowercase() as u8) & 0x1f],
                    '[' | '\\' | ']' | '^' | '_' => vec![(ch as u8) & 0x1f],
                    _ => utf8(ch),
                }
            } else {
                utf8(ch)
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        _ => return None,
    };
    Some(esc(bytes))
}
