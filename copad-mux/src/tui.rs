//! The multi-pane TUI: a ratatui front-end over the authoritative `State` (layout
//! authority) plus one `PaneTerm` (real shell PTY) per terminal. It renders every
//! pane in its derived rect with dividers, highlights the focused pane, and routes
//! keys to the focused pane. tmux-style prefix `Ctrl-b` then: `%` split right,
//! `"` split down, `o`/arrows focus, `x` close, `q` quit.
//!
//! Work-unit 3: multi-pane splits in one workspace/tab. The sidebar, popup,
//! server/client split, and CLI come in later units.

use std::collections::HashMap;
use std::io::{self, Stdout};
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

use crate::model::{ClientId, Dir, PaneId, PaneRect, Rect, Role, TerminalId, WorkspaceId};
use crate::state::{Command, Event, Origin, State};
use crate::term::{CellColor, PaneTerm};

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

/// The multi-pane application: the authoritative layout `State` + a live shell per
/// terminal. The local TUI is the single controller client (`ClientId(0)`).
struct App {
    state: State,
    ws: WorkspaceId,
    client: ClientId,
    panes: HashMap<TerminalId, PaneTerm>,
    cols: u16,
    rows: u16,
}

impl App {
    fn new(cols: u16, rows: u16) -> io::Result<Self> {
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
        let Some(pt) = PaneTerm::spawn(cols.max(1), rows.max(1), None, None) else {
            return Err(io::Error::other("failed to spawn shell PTY"));
        };
        panes.insert(term0, pt);
        let app = Self {
            state,
            ws,
            client,
            panes,
            cols,
            rows,
        };
        app.sync_sizes();
        Ok(app)
    }

    // --- reads off the authoritative state ---

    /// Placed rects for the active tab, tiling `cols`×`rows`.
    fn layout(&self, cols: u16, rows: u16) -> Vec<PaneRect> {
        let mut out = Vec::new();
        if let Some(w) = self.state.workspace(&self.ws)
            && let Some(tab) = w.tab(&w.active_tab)
        {
            tab.layout.derive_layout(0, 0, cols, rows, &mut out);
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
        for rect in self.layout(self.cols, self.rows) {
            if let Some(pt) = self.panes.get(&rect.terminal) {
                pt.resize(rect.cols, rect.rows);
            }
        }
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
            match PaneTerm::spawn(self.cols.max(1), self.rows.max(1), None, None) {
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
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let events = match self.state.apply(Command::ClosePane {
            origin: Origin::Client(self.client),
            workspace: self.ws.clone(),
            pane,
            if_rev: None,
        }) {
            Ok(e) => e,
            Err(_) => return, // e.g. last pane — leave it; shell-exit path handles quit
        };
        for e in &events {
            if let Event::PaneClosed { terminal, .. } = e {
                self.panes.remove(terminal);
            }
        }
        self.sync_sizes();
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
        let rects = self.layout(self.cols, self.rows);
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
        let _ = self.state.apply(Command::Resize {
            client: self.client,
            cols,
            rows,
        });
        self.sync_sizes();
    }

    /// Close panes whose shell has exited. Returns true if the app is now empty.
    fn reap_exited(&mut self) -> bool {
        let exited: Vec<TerminalId> = self
            .panes
            .iter()
            .filter(|(_, p)| p.has_exited())
            .map(|(id, _)| id.clone())
            .collect();
        for term in exited {
            match self.pane_of_terminal(&term) {
                Some(pane) => {
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
                        // The last pane can't be closed in the layout — its shell
                        // exiting means "quit": drop it so the app becomes empty.
                        Err(_) => {
                            self.panes.remove(&term);
                        }
                    }
                }
                None => {
                    self.panes.remove(&term);
                }
            }
        }
        self.sync_sizes();
        self.is_empty()
    }

    // --- render ---

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let rects = self.layout(area.width, area.height);
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
        let buf = frame.buffer_mut();
        for yy in 0..area.height {
            for xx in 0..area.width {
                if covered[yy as usize][xx as usize] {
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

        if let Some(p) = cursor_pos {
            frame.set_cursor_position(p);
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

    let size = terminal.size()?;
    let mut app = App::new(size.width, size.height)?;

    let mut prefix = false;
    'main: loop {
        if app.reap_exited() {
            break;
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
                    if prefix {
                        prefix = false;
                        match k.code {
                            KeyCode::Char('%') => app.split(Dir::Right),
                            KeyCode::Char('"') => app.split(Dir::Down),
                            KeyCode::Char('o') => app.focus_next(),
                            KeyCode::Char('x') => app.close_focused(),
                            KeyCode::Char('q') => break 'main,
                            KeyCode::Left => app.focus_dir(FocusDir::Left),
                            KeyCode::Right => app.focus_dir(FocusDir::Right),
                            KeyCode::Up => app.focus_dir(FocusDir::Up),
                            KeyCode::Down => app.focus_dir(FocusDir::Down),
                            _ => {}
                        }
                    } else {
                        // Ctrl-b enters prefix mode. Depending on the terminal /
                        // keyboard protocol, crossterm delivers it as `Char('b')`
                        // with CONTROL, or as the raw control byte `Char('\u{2}')`.
                        let is_prefix_key = matches!(k.code, KeyCode::Char('\u{2}'))
                            || (k.code == KeyCode::Char('b')
                                && k.modifiers.contains(KeyModifiers::CONTROL));
                        if is_prefix_key {
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

    Ok(())
}

fn to_color(c: CellColor) -> Color {
    match c {
        CellColor::Default => Color::Reset,
        CellColor::Indexed(i) => Color::Indexed(i),
        CellColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
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
