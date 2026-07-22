//! The multi-pane TUI: a ratatui front-end over the authoritative `State` (layout
//! authority) plus one `PaneTerm` (real shell PTY) per terminal. It renders every
//! pane in its derived rect with dividers, highlights the focused pane, and routes
//! keys to the focused pane. tmux-style prefix `Ctrl-b` then: `%` split right,
//! `"` split down, `o`/arrows focus, `x` close, `q` quit.
//!
//! Work-unit 3: multi-pane splits in one workspace/tab. The sidebar, popup,
//! server/client split, and CLI come in later units.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Stdout, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::time::Duration;

use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event as CEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};

use crate::control;
use crate::model::{ClientId, Dir, PaneId, PaneRect, Rect, Role, TabId, TerminalId, WorkspaceId};
use crate::procinfo;
use crate::state::{Command, Event, MuxError, Origin, State};
use crate::term::{CellColor, PaneTerm};

/// A control request handed from a socket connection thread to the main loop
/// (which is the single writer of `State`), with a channel for the reply.
struct ControlMsg {
    req: control::Req,
    reply: mpsc::Sender<control::Resp>,
}

/// Read ndjson requests off one control connection, forward each to the main loop
/// over `tx`, and write back the reply line. Never touches `State` directly.
fn handle_conn(stream: UnixStream, tx: mpsc::Sender<ControlMsg>) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return, // EOF or error → connection done
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<control::Req>(trimmed) {
            Ok(req) => {
                let (rtx, rrx) = mpsc::channel();
                if tx.send(ControlMsg { req, reply: rtx }).is_err() {
                    return; // main loop gone
                }
                rrx.recv()
                    .unwrap_or_else(|_| control::Resp::err("mux shutting down"))
            }
            Err(e) => control::Resp::err(format!("bad request: {e}")),
        };
        let out = serde_json::to_string(&resp).unwrap_or_else(|_| "{\"ok\":false}".to_string());
        if writeln!(writer, "{out}").is_err() {
            return;
        }
        let _ = writer.flush();
    }
}

/// Restores the host terminal (raw mode off + leave alt screen) on drop — so a
/// panic mid-render never leaves the user's terminal wedged.
struct TermGuard;

impl TermGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // Construct the guard immediately so that if EnterAlternateScreen below
        // fails, its Drop still disables raw mode (no wedged terminal).
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(guard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Direction for focus navigation with the arrow keys.
#[derive(Clone, Copy)]
enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// Sidebar width in columns (a 1-col border is added to its right).
const SIDEBAR_W: u16 = 24;
/// Below this terminal width the sidebar is suppressed even when toggled on —
/// the popup switcher is the fallback on narrow / mobile widths (size-adaptive).
const SIDEBAR_MIN_COLS: u16 = 80;

/// The multi-pane application: the authoritative layout `State` + a live shell per
/// terminal. The local TUI is the single controller client (`ClientId(0)`).
struct App {
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
    /// Env vars injected into every pane's shell (e.g. `COPAD_MUX_SOCK`), so a
    /// shell inside a pane can control its own mux via `copad-mux ctl`.
    sock_env: Vec<(String, String)>,
    /// Per-terminal foreground-process label (agent / shell / command), refreshed
    /// on a throttled cadence (`refresh_labels`), read by the sidebar/popup/CLI.
    labels: HashMap<TerminalId, procinfo::Label>,
    /// When the labels were last refreshed (throttle `ps` to ~2 Hz).
    last_labels: std::time::Instant,
}

impl App {
    fn new(cols: u16, rows: u16, sock_env: Vec<(String, String)>) -> io::Result<Self> {
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
            sidebar: false,
            popup: None,
            sock_env,
            labels: HashMap::new(),
            last_labels: std::time::Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(std::time::Instant::now),
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

    /// The tab bar occupies the top row only when there is more than one tab
    /// (a single tab needs no chooser, so it keeps the full height).
    fn tabbar_visible(&self) -> bool {
        self.tab_count() > 1
    }

    /// Top y-offset of the pane grid (the tab bar reserves row 0 when visible).
    fn content_y(&self) -> u16 {
        if self.tabbar_visible() { 1 } else { 0 }
    }

    /// Height available to the pane grid, below the tab bar.
    fn content_rows(&self) -> u16 {
        self.rows.saturating_sub(self.content_y())
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

    fn is_empty(&self) -> bool {
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

    /// Locate a terminal's `(tab, pane)` across ALL tabs (not just the active one),
    /// so a background tab's exited shell can be reaped correctly.
    fn find_tab_pane_of_terminal(&self, term: &TerminalId) -> Option<(TabId, PaneId)> {
        let w = self.state.workspace(&self.ws)?;
        for t in &w.tabs {
            for p in t.layout.panes() {
                if t.layout.terminal_of(&p) == Some(term) {
                    return Some((t.id.clone(), p));
                }
            }
        }
        None
    }

    // --- control API (spec §3): applied on the main loop (single writer) ---

    /// Handle one control request, mutating state as the sole writer. Returns the
    /// wire response. Never blocks.
    fn handle_control(&mut self, req: &control::Req) -> control::Resp {
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
                        let kind = match self.pane_label_at(index).map(|l| l.kind) {
                            Some(procinfo::Kind::Agent) => "agent",
                            Some(procinfo::Kind::Shell) => "shell",
                            _ => "other",
                        };
                        PaneInfo {
                            index,
                            id: p.to_string(),
                            focused: focused.as_ref() == Some(p),
                            cols,
                            rows,
                            label: self.pane_label(index),
                            kind: kind.to_string(),
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
        }
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
            pt.input(bytes);
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
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
    fn reconcile_popup(&mut self) {
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
    fn reap_exited(&mut self) -> bool {
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
            let Some((tab_id, pane)) = self.find_tab_pane_of_terminal(&term) else {
                // No longer in any tab (already collapsed) — just drop the runtime.
                self.panes.remove(&term);
                continue;
            };
            let tab_is_single = self
                .state
                .workspace(&self.ws)
                .and_then(|w| w.tab(&tab_id))
                .map(|t| t.layout.is_single_leaf())
                .unwrap_or(true);
            if !tab_is_single {
                // The tab has other panes — collapse just this one.
                match self.state.apply(Command::ClosePane {
                    origin: Origin::Client(self.client),
                    workspace: self.ws.clone(),
                    pane,
                    if_rev: None,
                }) {
                    Ok(evs) => {
                        for e in &evs {
                            if let Event::PaneClosed { terminal, .. } = e {
                                self.panes.remove(terminal);
                            }
                        }
                    }
                    Err(_) => {
                        self.panes.remove(&term);
                    }
                }
            } else if self.tab_count() > 1 {
                // Last pane of this tab, but other tabs survive — close the tab.
                match self.state.apply(Command::CloseTab {
                    origin: Origin::Client(self.client),
                    workspace: self.ws.clone(),
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
                    }
                    Err(_) => {
                        self.panes.remove(&term);
                    }
                }
            } else {
                // Last pane of the last tab — its exit means "quit": drop it so the
                // app becomes empty and the main loop terminates.
                self.panes.remove(&term);
            }
        }
        // Reaping may have closed a tab (hiding the tab bar) → re-derive viewport.
        self.reflow();
        self.is_empty()
    }

    // --- render ---

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
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
            let buf = frame.buffer_mut();
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
        let content_y = self.content_y();
        let buf = frame.buffer_mut();
        for yy in 0..area.height {
            for xx in 0..area.width {
                // The tab bar owns row 0 and the sidebar owns the left strip; don't
                // paint dividers over either.
                if yy < content_y || xx < content_x || covered[yy as usize][xx as usize] {
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
            self.render_sidebar(frame);
        }

        // 4) the tab bar across the top row (over the sidebar's top too).
        if self.tabbar_visible() {
            self.render_tabbar(frame);
        }

        // 5) the Ctrl-f switcher popup, over everything.
        if self.popup.is_some() {
            self.render_popup(frame);
        }

        // The shell cursor shows only when no popup is capturing input.
        if self.popup.is_none()
            && let Some(p) = cursor_pos
        {
            frame.set_cursor_position(p);
        }
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

    /// Left panel: a header + one row per pane, focus-marked.
    fn render_sidebar(&self, frame: &mut Frame) {
        let area = frame.area();
        let h = area.height;
        let order = self.pane_order();
        let focused = self.focused_pane();
        let panel_bg = Color::Rgb(28, 30, 38);
        let buf = frame.buffer_mut();

        // Fill the strip + the border column.
        for y in 0..h {
            for x in 0..SIDEBAR_W {
                if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                    bc.set_symbol(" ");
                    bc.set_style(Style::default().bg(panel_bg));
                }
            }
            if let Some(bc) = buf.cell_mut(Position::new(SIDEBAR_W, y)) {
                bc.set_symbol("│");
                bc.set_style(Style::default().fg(Color::DarkGray).bg(panel_bg));
            }
        }

        let put = |buf: &mut ratatui::buffer::Buffer, y: u16, s: &str, style: Style| {
            for (i, ch) in s.chars().take(SIDEBAR_W as usize).enumerate() {
                if let Some(bc) = buf.cell_mut(Position::new(i as u16, y)) {
                    let mut sb = [0u8; 4];
                    bc.set_symbol(ch.encode_utf8(&mut sb));
                    bc.set_style(style);
                }
            }
        };

        // Leave row 0 for the tab bar when it is showing.
        let cy = self.content_y();
        put(
            buf,
            cy,
            &format!(" PANES ({})", order.len()),
            Style::default()
                .fg(Color::Cyan)
                .bg(panel_bg)
                .add_modifier(Modifier::BOLD),
        );
        for (i, pane) in order.iter().enumerate() {
            let y = cy + 1 + i as u16;
            if y >= h {
                break;
            }
            let is_focus = focused.as_ref() == Some(pane);
            let marker = if is_focus { "▸" } else { " " };
            let kind = self.pane_label_at(i).map(|l| l.kind);
            // A filled dot flags an AI agent; shells/others get a blank.
            let dot = if kind == Some(procinfo::Kind::Agent) {
                "●"
            } else {
                " "
            };
            let row = format!(" {marker}{i} {dot}{}", self.pane_label(i));
            let style = if is_focus {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(48, 52, 66))
                    .add_modifier(Modifier::BOLD)
            } else {
                match kind {
                    Some(procinfo::Kind::Agent) => Style::default()
                        .fg(Color::Yellow)
                        .bg(panel_bg)
                        .add_modifier(Modifier::BOLD),
                    Some(procinfo::Kind::Shell) => {
                        Style::default().fg(Color::DarkGray).bg(panel_bg)
                    }
                    _ => Style::default().fg(Color::Gray).bg(panel_bg),
                }
            };
            put(buf, y, &row, style);
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

    /// Top-row tab bar: one 1-based chip per tab, active highlighted, with a `●`
    /// flag on any tab that hosts an agent.
    fn render_tabbar(&self, frame: &mut Frame) {
        let area = frame.area();
        let w = area.width;
        let ids = self.tab_ids();
        let active = self.active_tab_id();
        let bg = Color::Rgb(20, 22, 30);
        let buf = frame.buffer_mut();

        // Clear row 0 (also lifts any wide-char skip flag left by a pane cell).
        for x in 0..w {
            if let Some(bc) = buf.cell_mut(Position::new(x, 0)) {
                bc.set_symbol(" ");
                bc.set_skip(false);
                bc.set_style(Style::default().bg(bg));
            }
        }

        let mut x = 0u16;
        let put = |buf: &mut ratatui::buffer::Buffer, s: &str, st: Style, x: &mut u16| {
            for ch in s.chars() {
                if *x >= w {
                    break;
                }
                if let Some(bc) = buf.cell_mut(Position::new(*x, 0)) {
                    let mut sb = [0u8; 4];
                    bc.set_symbol(ch.encode_utf8(&mut sb));
                    bc.set_skip(false);
                    bc.set_style(st);
                }
                *x += 1;
            }
        };

        for (i, id) in ids.iter().enumerate() {
            let is_active = active.as_ref() == Some(id);
            let dot = if self.tab_has_agent(id) { "●" } else { "" };
            let label = format!(" {dot}{} ", i + 1);
            let style = if is_active {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if !dot.is_empty() {
                Style::default().fg(Color::Yellow).bg(bg)
            } else {
                Style::default().fg(Color::Gray).bg(bg)
            };
            put(buf, &label, style, &mut x);
            // one-column gap between chips
            put(buf, " ", Style::default().bg(bg), &mut x);
        }
    }

    /// Centered `Ctrl-f` switcher over the panes: a bordered box listing panes with
    /// a selection cursor.
    fn render_popup(&self, frame: &mut Frame) {
        let Some(sel) = self.popup else { return };
        let area = frame.area();
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
        let buf = frame.buffer_mut();

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

/// Run the multi-pane TUI to completion (quit with `Ctrl-b q` or when every shell
/// has exited).
pub fn run() -> io::Result<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    let _guard = TermGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    // Control socket (spec §3): bind, and let an accept thread forward requests to
    // this loop over a channel — the socket thread NEVER writes `State`. Env-inject
    // the socket path so a shell inside a pane can `copad-mux ctl` its own mux.
    let sock_path = control::socket_path();
    let (ctl_tx, ctl_rx) = mpsc::channel::<ControlMsg>();
    let mut sock_env = Vec::new();
    let mut we_bound_socket = false;
    // Only clear the socket if it is STALE: probe-connect first so a second
    // instance never unlinks a still-live server's socket (and its cleanup never
    // deletes another instance's socket).
    if UnixStream::connect(&sock_path).is_ok() {
        eprintln!(
            "copad-mux: control socket {} is already in use by another instance; \
             ctl disabled here (set COPAD_MUX_SOCK for a separate socket)",
            sock_path.display()
        );
    } else {
        let _ = std::fs::remove_file(&sock_path); // clear a stale (dead) socket
        match UnixListener::bind(&sock_path) {
            Ok(listener) => {
                we_bound_socket = true;
                sock_env.push((
                    "COPAD_MUX_SOCK".to_string(),
                    sock_path.to_string_lossy().to_string(),
                ));
                let tx = ctl_tx.clone();
                std::thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        let tx = tx.clone();
                        std::thread::spawn(move || handle_conn(stream, tx));
                    }
                });
            }
            Err(e) => {
                // Non-fatal: run the TUI without the control API rather than
                // refusing to start (e.g. path permission issue).
                eprintln!("copad-mux: control socket unavailable ({e}); ctl disabled");
            }
        }
    }

    let size = terminal.size()?;
    let mut app = App::new(size.width, size.height, sock_env)?;

    let mut prefix = false;
    'main: loop {
        if app.reap_exited() {
            break;
        }
        // A pane may have exited while the popup was open — keep its selection
        // index valid (or close it) so it never points past the pane list.
        app.reconcile_popup();

        // Throttled foreground-process/agent label refresh (one `ps` sweep, ~2 Hz).
        if app.last_labels.elapsed() >= Duration::from_millis(500) {
            app.refresh_labels();
        }

        // Apply any pending control requests as the single writer.
        while let Ok(msg) = ctl_rx.try_recv() {
            let resp = app.handle_control(&msg.req);
            let _ = msg.reply.send(resp);
        }

        terminal.draw(|frame| app.render(frame))?;

        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        // Drain the whole pending input burst before the next render, so a prefix
        // and its command (`Ctrl-b` then `%`) are always processed together and no
        // event is deferred across frames (rapid/pasted input stays correct).
        loop {
            match event::read()? {
                CEvent::Key(k) if k.kind != KeyEventKind::Release => {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    if app.popup.is_some() {
                        // The switcher captures all input while open.
                        match k.code {
                            KeyCode::Char('j') | KeyCode::Down => app.popup_move(1),
                            KeyCode::Char('k') | KeyCode::Up => app.popup_move(-1),
                            KeyCode::Enter => app.popup_select(),
                            KeyCode::Esc | KeyCode::Char('\u{6}') => app.popup = None,
                            KeyCode::Char('f') if ctrl => app.popup = None,
                            _ => {}
                        }
                    } else if prefix {
                        prefix = false;
                        match k.code {
                            KeyCode::Char('%') => app.split(Dir::Right),
                            KeyCode::Char('"') => app.split(Dir::Down),
                            KeyCode::Char('o') => app.focus_next(),
                            KeyCode::Char('x') => app.close_focused(),
                            KeyCode::Char('s') => app.toggle_sidebar(),
                            // tabs: `c` new, `n`/`p` next/prev, `&` close, `1`-`9` jump
                            KeyCode::Char('c') => app.new_tab(),
                            KeyCode::Char('n') => app.cycle_tab(1),
                            KeyCode::Char('p') => app.cycle_tab(-1),
                            KeyCode::Char('&') => app.close_active_tab(),
                            KeyCode::Char(d @ '1'..='9') => {
                                app.select_tab_index(d as usize - '1' as usize)
                            }
                            KeyCode::Char('q') => break 'main,
                            KeyCode::Left => app.focus_dir(FocusDir::Left),
                            KeyCode::Right => app.focus_dir(FocusDir::Right),
                            KeyCode::Up => app.focus_dir(FocusDir::Up),
                            KeyCode::Down => app.focus_dir(FocusDir::Down),
                            _ => {}
                        }
                    } else if let Some(dir) = ctrl_shift_nav(k.code, k.modifiers) {
                        // Direct (prefix-less) pane navigation: hold `Ctrl+Shift` and
                        // press an arrow to jump focus to the neighbouring pane. Faster
                        // than the `Ctrl-b <arrow>` prefix form (which stays as a
                        // fallback for terminals that don't deliver modified arrows —
                        // e.g. tmux without `xterm-keys on`).
                        app.focus_dir(dir);
                    } else {
                        // `Ctrl-f` opens the switcher popup (matches the owner's tmux
                        // binding). `Ctrl-b` enters prefix mode. Both may arrive as a
                        // raw control byte (`\u{6}`/`\u{2}`) or `Char(_)`+CONTROL.
                        let is_popup_key = matches!(k.code, KeyCode::Char('\u{6}'))
                            || (k.code == KeyCode::Char('f') && ctrl);
                        let is_prefix_key = matches!(k.code, KeyCode::Char('\u{2}'))
                            || (k.code == KeyCode::Char('b') && ctrl);
                        if is_popup_key {
                            app.open_popup();
                        } else if is_prefix_key {
                            prefix = true;
                        } else if let Some(bytes) = key_to_bytes(k.code, k.modifiers) {
                            app.input_focused(&bytes);
                        }
                    }
                }
                CEvent::Resize(w, h) => app.resize(w, h),
                _ => {}
            }
            if !event::poll(Duration::from_millis(0))? {
                break;
            }
        }
    }

    // Best-effort: remove the control socket ONLY if we bound it, so we never
    // delete another live instance's socket.
    if we_bound_socket {
        let _ = std::fs::remove_file(&sock_path);
    }
    Ok(())
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
        KeyCode::Left => Some(FocusDir::Left),
        KeyCode::Right => Some(FocusDir::Right),
        KeyCode::Up => Some(FocusDir::Up),
        KeyCode::Down => Some(FocusDir::Down),
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
