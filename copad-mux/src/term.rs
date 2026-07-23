//! A single hosted terminal: a PTY + `alacritty_terminal` parser/grid, running
//! on alacritty's own `EventLoop` reader thread. This is the mux's terminal
//! runtime — the same proven engine `copad-term` uses (alacritty_terminal 0.26),
//! but a clean Rust surface (no C-ABI) so the server/TUI can host many of them.
//!
//! Work-unit 2 hosts exactly one; splits + the multi-pane server come later.

use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty::{self, Options as TtyOptions, Pty, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};

/// A cell color resolved to something a renderer can map directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    /// Use the surface's default fg/bg.
    Default,
    /// A 0–255 palette index (ANSI 16 + 256-color cube).
    Indexed(u8),
    /// A direct 24-bit color.
    Rgb(u8, u8, u8),
}

/// One rendered cell.
#[derive(Debug, Clone, PartialEq)]
pub struct CellSnap {
    /// The cell's grapheme: base char plus any zero-width combining marks.
    pub sym: String,
    /// True for the trailing half of a double-width character. The renderer
    /// leaves it blank/skipped so the wide grapheme in the preceding cell isn't
    /// overwritten (CJK, emoji).
    pub spacer: bool,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub reverse: bool,
}

/// A snapshot of the visible grid + cursor for one render tick.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    /// `rows` × `cols` cells, row-major, top-to-bottom of the viewport.
    pub cells: Vec<Vec<CellSnap>>,
    /// Cursor position in viewport coordinates `(col, row)`.
    pub cursor: (u16, u16),
}

/// Minimal `EventListener`: forwards `PtyWrite` replies (DSR/DA/OSC answers) so
/// prompts that query the terminal don't hang, and latches child-exit. Other
/// events (clipboard, color queries, title, bell) are dropped in this scaffold.
#[derive(Clone)]
struct MuxListener {
    sender: Arc<std::sync::Mutex<Option<EventLoopSender>>>,
    child_exited: Arc<AtomicBool>,
    /// Set whenever the terminal's visible state changed (alacritty `Wakeup`, or a
    /// query reply that writes back). The render loop reads+clears it via
    /// [`PaneTerm::take_dirty`] to skip composing frames when nothing changed.
    dirty: Arc<AtomicBool>,
}

impl MuxListener {
    fn new() -> Self {
        Self {
            sender: Arc::new(std::sync::Mutex::new(None)),
            child_exited: Arc::new(AtomicBool::new(false)),
            // Start dirty so the very first frame is composed.
            dirty: Arc::new(AtomicBool::new(true)),
        }
    }
    fn set_sender(&self, s: EventLoopSender) {
        *self.sender.lock().unwrap() = Some(s);
    }
}

impl EventListener for MuxListener {
    fn send_event(&self, event: Event) {
        match event {
            // The terminal processed output and wants a redraw — mark this pane dirty.
            Event::Wakeup => {
                self.dirty.store(true, Ordering::Relaxed);
            }
            Event::PtyWrite(reply) => {
                self.dirty.store(true, Ordering::Relaxed);
                if let Some(s) = self.sender.lock().unwrap().as_ref() {
                    let _ = s.send(Msg::Input(reply.into_bytes().into()));
                }
            }
            Event::ChildExit(_status) => {
                self.child_exited.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

/// A hosted terminal pane.
pub struct PaneTerm {
    term: Arc<FairMutex<Term<MuxListener>>>,
    sender: EventLoopSender,
    listener: MuxListener,
    io_thread: Option<JoinHandle<(EventLoop<Pty, MuxListener>, State)>>,
    /// The child shell's pid, captured at spawn. Used to find the pane's
    /// foreground process (agent/command label). `None` if unavailable.
    child_pid: Option<u32>,
    /// A dup of the PTY master fd, kept so we can query the terminal's foreground
    /// process group (`tcgetpgrp`) — the actual foreground process, not a guess.
    /// Closed on drop.
    fg_fd: Option<RawFd>,
    /// The directory the shell was spawned in. A stable fallback for liveness checks
    /// (e.g. worktree-removal safety) when the live `process_cwd` is momentarily
    /// unreadable — a pane never spawns "below" its initial cwd without our knowing.
    spawn_cwd: Option<PathBuf>,
}

impl PaneTerm {
    /// Spawn a shell in a PTY sized `cols`×`rows`. `shell` defaults to `$SHELL`
    /// (then the system default); `cwd` to the process cwd.
    pub fn spawn(
        cols: u16,
        rows: u16,
        shell: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Option<Self> {
        Self::spawn_with_env(cols, rows, shell, cwd, &[])
    }

    /// Like [`spawn`](Self::spawn) but injects extra environment variables into
    /// the child shell (e.g. `COPAD_MUX_SOCK` so a shell inside a pane can drive
    /// its own mux via `copad-mux ctl`).
    pub fn spawn_with_env(
        cols: u16,
        rows: u16,
        shell: Option<String>,
        cwd: Option<PathBuf>,
        env: &[(String, String)],
    ) -> Option<Self> {
        let cols = cols.max(1);
        let rows = rows.max(1);

        let mut opts = TtyOptions::default();
        if let Some(sh) = shell.or_else(|| std::env::var("SHELL").ok()) {
            // Login shell so PATH / profile are set up as in a normal terminal.
            opts.shell = Some(Shell::new(sh, vec!["-l".to_string()]));
        }
        let spawn_cwd = cwd.clone();
        if let Some(dir) = cwd {
            opts.working_directory = Some(dir);
        }
        for (k, v) in env {
            opts.env.insert(k.clone(), v.clone());
        }

        let window = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        };
        let pty = tty::new(&opts, window, 0).ok()?;
        // Capture the child pid + a dup of the master fd before the Pty is moved
        // into the EventLoop (the dup lets us query the foreground pgrp later).
        let child_pid = Some(pty.child().id());
        let fg_fd = {
            let raw = pty.file().as_raw_fd();
            // SAFETY: `raw` is a valid open fd for the duration of this call.
            let d = unsafe { libc::dup(raw) };
            (d >= 0).then_some(d)
        };

        let term_size = TermSize::new(cols as usize, rows as usize);
        let listener = MuxListener::new();
        // A generous scrollback so panes have history to scroll back through.
        let config = Config {
            scrolling_history: 10_000,
            ..Config::default()
        };
        let term = Term::new(config, &term_size, listener.clone());
        let term = Arc::new(FairMutex::new(term));

        let event_loop =
            EventLoop::new(Arc::clone(&term), listener.clone(), pty, false, false).ok()?;
        let sender = event_loop.channel();
        listener.set_sender(sender.clone());
        let io_thread = event_loop.spawn();

        Some(Self {
            term,
            sender,
            listener,
            io_thread: Some(io_thread),
            child_pid,
            fg_fd,
            spawn_cwd,
        })
    }

    /// The child shell's pid (fallback label when no foreground group is set).
    pub fn pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// The directory the shell was spawned in (liveness fallback when the live
    /// `process_cwd` can't be read).
    pub fn spawn_cwd(&self) -> Option<&PathBuf> {
        self.spawn_cwd.as_ref()
    }

    /// The pid of the terminal's foreground process GROUP leader — the process
    /// actually running in the foreground (`sleep`, `claude`, `nvim`, …), via
    /// `tcgetpgrp` on the PTY master. `None` if unavailable.
    pub fn foreground_pgid(&self) -> Option<u32> {
        let fd = self.fg_fd?;
        // SAFETY: `fd` is our own dup of the master, valid until drop.
        let pgid = unsafe { libc::tcgetpgrp(fd) };
        (pgid > 0).then_some(pgid as u32)
    }

    /// Feed input bytes to the child shell.
    pub fn input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let _ = self.sender.send(Msg::Input(bytes.to_vec().into()));
    }

    /// Resize the PTY (SIGWINCH to the child) + the Term grid.
    pub fn resize(&self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let window = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        };
        let _ = self.sender.send(Msg::Resize(window));
        self.term
            .lock()
            .resize(TermSize::new(cols as usize, rows as usize));
        self.mark_dirty(); // a reflow changes the visible grid but may not Wakeup
    }

    /// Has the child shell exited?
    pub fn has_exited(&self) -> bool {
        self.listener.child_exited.load(Ordering::Relaxed)
    }

    /// Read AND clear this pane's dirty flag (set by the io-thread on any screen change).
    /// The render loop ORs this across panes to decide whether a frame needs composing.
    pub fn take_dirty(&self) -> bool {
        self.listener.dirty.swap(false, Ordering::Relaxed)
    }

    /// Force this pane dirty (e.g. after a resize/scroll that changes what's visible but
    /// may not emit a `Wakeup`).
    pub fn mark_dirty(&self) {
        self.listener.dirty.store(true, Ordering::Relaxed);
    }

    /// Scroll the viewport through scrollback: positive `lines` = UP (older),
    /// negative = DOWN (newer). `snapshot` then renders at the new offset.
    pub fn scroll(&self, lines: i32) {
        if lines != 0 {
            self.term.lock().scroll_display(Scroll::Delta(lines));
            self.mark_dirty();
        }
    }

    /// Jump the viewport back to the live bottom (offset 0).
    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
        self.mark_dirty();
    }

    /// How many lines the viewport is scrolled up from the live bottom (0 = live).
    pub fn scroll_offset(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    /// Bytes to feed the child for ONE wheel notch, honoring the app's active input mode
    /// (tmux-style), or `None` if the app wants no wheel input — then the caller scrolls
    /// the pane's OWN scrollback instead. `col`/`row` are 1-based cell coords WITHIN the
    /// pane; `up` = wheel toward older content.
    ///
    /// - App has mouse reporting on (Claude Code, nvim `set mouse`): send an xterm wheel
    ///   button report (64 = up, 65 = down), SGR-encoded if the app negotiated SGR else the
    ///   legacy `ESC [ M` form.
    /// - Alt-screen app WITHOUT mouse reporting but WITH alternate-scroll (less, man, git
    ///   log): xterm turns the wheel into cursor-key presses so it pages as expected
    ///   (application-cursor-keys mode picks `ESC O A/B` vs `ESC [ A/B`).
    /// - Otherwise: `None` — the app isn't listening, so comux scrolls its scrollback.
    pub fn wheel_bytes(&self, up: bool, col: u16, row: u16) -> Option<Vec<u8>> {
        wheel_bytes_for_mode(*self.term.lock().mode(), up, col, row)
    }

    /// Snapshot the visible viewport for rendering. Keeps the term lock only for
    /// the copy so the reader thread isn't starved.
    pub fn snapshot(&self) -> Snapshot {
        snapshot_grid(&self.term.lock())
    }
}

/// Snapshot the visible viewport of any `Term` into renderer-ready [`Snapshot`]. Split
/// out of [`PaneTerm::snapshot`] so tests can drive a bare `Term` (via the VTE parser)
/// deterministically — no PTY, no shell, no timing.
fn snapshot_grid<L: EventListener>(term: &Term<L>) -> Snapshot {
    let cols = term.columns();
    let rows = term.screen_lines();
    let grid = term.grid();
    let display_offset = grid.display_offset() as i32;

    let mut cells = Vec::with_capacity(rows);
    for r in 0..rows as i32 {
        let line = Line(r - display_offset);
        let mut row = Vec::with_capacity(cols);
        for c in 0..cols {
            let cell = &grid[Point::new(line, Column(c))];
            let flags = cell.flags;
            let spacer =
                flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
            // Grapheme = base char + any zero-width combining marks.
            let mut sym = String::new();
            sym.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                sym.extend(zw.iter());
            }
            row.push(CellSnap {
                sym,
                spacer,
                fg: ansi_to_cell(cell.fg),
                bg: ansi_to_cell(cell.bg),
                bold: flags.contains(Flags::BOLD),
                reverse: flags.contains(Flags::INVERSE),
            });
        }
        cells.push(row);
    }

    let cursor_point = grid.cursor.point;
    let cursor_row = (cursor_point.line.0 + display_offset).clamp(0, rows as i32 - 1) as u16;
    let cursor_col = (cursor_point.column.0 as u16).min(cols.saturating_sub(1) as u16);

    Snapshot {
        cols: cols as u16,
        rows: rows as u16,
        cells,
        cursor: (cursor_col, cursor_row),
    }
}

impl Drop for PaneTerm {
    fn drop(&mut self) {
        if let Some(fd) = self.fg_fd.take() {
            // SAFETY: our own dup'd fd, closed exactly once.
            unsafe { libc::close(fd) };
        }
        let _ = self.sender.send(Msg::Shutdown);
        if let Some(jh) = self.io_thread.take() {
            // Bounded join: normally the io-thread exits immediately on `Shutdown`, but a
            // wedged PTY/shell must NOT hang teardown (that would strand a live server
            // holding its flock). Give it a short grace, then DETACH (drop the handle) and
            // let the OS reap it — never block forever.
            for _ in 0..50 {
                if jh.is_finished() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if jh.is_finished() {
                let _ = jh.join();
            }
        }
    }
}

/// The pure mode→wheel-bytes mapping behind [`PaneTerm::wheel_bytes`] (split out so the
/// encoding is unit-testable without a PTY). See that method for the tmux-style policy.
fn wheel_bytes_for_mode(mode: TermMode, up: bool, col: u16, row: u16) -> Option<Vec<u8>> {
    if mode.intersects(TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
    {
        let button: u16 = if up { 64 } else { 65 };
        if mode.contains(TermMode::SGR_MOUSE) {
            return Some(format!("\x1b[<{button};{col};{row}M").into_bytes());
        }
        if mode.contains(TermMode::UTF8_MOUSE) {
            // xterm 1005: `ESC [ M` then Cb,Cx,Cy each as a UTF-8 char = 32 + value. Unlike
            // legacy this expresses coords past 223 (up to the 2-byte UTF-8 ceiling, 0x7ff),
            // so wide panes report the right cell.
            let mut out = vec![0x1b, b'[', b'M'];
            for v in [button, col, row] {
                let cp = (v as u32 + 32).min(0x7ff);
                let mut b = [0u8; 4];
                out.extend_from_slice(
                    char::from_u32(cp)
                        .unwrap_or(' ')
                        .encode_utf8(&mut b)
                        .as_bytes(),
                );
            }
            return Some(out);
        }
        // Legacy `ESC [ M Cb Cx Cy`: each value offset by 32, clamped to the single-byte
        // range (coords past 223 can't be expressed and are pinned there, as in xterm).
        let enc = |v: u16| (v.saturating_add(32)).min(255) as u8;
        return Some(vec![0x1b, b'[', b'M', enc(button), enc(col), enc(row)]);
    }
    if mode.contains(TermMode::ALTERNATE_SCROLL) && mode.contains(TermMode::ALT_SCREEN) {
        let arrow: &[u8] = match (up, mode.contains(TermMode::APP_CURSOR)) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        return Some(arrow.to_vec());
    }
    None
}

/// Map an alacritty cell color to a renderer-friendly `CellColor`. Named ANSI
/// 0–15 become palette indices; the semantic Foreground/Background/Cursor/Dim*
/// names fall back to the surface default.
fn ansi_to_cell(color: AnsiColor) -> CellColor {
    match color {
        AnsiColor::Spec(rgb) => CellColor::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(i) => CellColor::Indexed(i),
        AnsiColor::Named(named) => match named {
            NamedColor::Black => CellColor::Indexed(0),
            NamedColor::Red => CellColor::Indexed(1),
            NamedColor::Green => CellColor::Indexed(2),
            NamedColor::Yellow => CellColor::Indexed(3),
            NamedColor::Blue => CellColor::Indexed(4),
            NamedColor::Magenta => CellColor::Indexed(5),
            NamedColor::Cyan => CellColor::Indexed(6),
            NamedColor::White => CellColor::Indexed(7),
            NamedColor::BrightBlack => CellColor::Indexed(8),
            NamedColor::BrightRed => CellColor::Indexed(9),
            NamedColor::BrightGreen => CellColor::Indexed(10),
            NamedColor::BrightYellow => CellColor::Indexed(11),
            NamedColor::BrightBlue => CellColor::Indexed(12),
            NamedColor::BrightMagenta => CellColor::Indexed(13),
            NamedColor::BrightCyan => CellColor::Indexed(14),
            NamedColor::BrightWhite => CellColor::Indexed(15),
            _ => CellColor::Default,
        },
    }
}

#[cfg(test)]
mod render_repro {
    //! Deterministic reproduction of the mux render pipeline WITHOUT a PTY: feed raw
    //! bytes straight into an alacritty `Term` via the VTE parser, snapshot it, then run
    //! the exact server→wire→client double-diff and render the client's result into a
    //! ratatui `TestBackend`. The client's screen must match a direct render of the
    //! server buffer — any divergence is a transport/compose bug (ghosts, blanks).
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;
    use ratatui::backend::{CrosstermBackend, TestBackend};
    use ratatui::buffer::Buffer;
    use ratatui::layout::{Position, Rect};
    use ratatui::style::{Modifier, Style};
    use ratatui::{Terminal, TerminalOptions, Viewport};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn term(cols: usize, rows: usize) -> (Term<VoidListener>, Processor) {
        let size = TermSize::new(cols, rows);
        (
            Term::new(Config::default(), &size, VoidListener),
            Processor::new(),
        )
    }

    fn feed(t: &mut Term<VoidListener>, p: &mut Processor, bytes: &[u8]) {
        p.advance(t, bytes);
    }

    /// Compose a snapshot into a ratatui `Buffer` the same way `App::render_to` composes a
    /// pane: real glyph + FULL style (fg/bg/bold/reverse, mirroring `tui::to_color`) for content
    /// cells, `skip` on wide-char spacers. Carrying the style is what lets the relay test emit
    /// real SGR and validate colour/attribute fidelity, not just glyphs.
    fn compose(snap: &Snapshot) -> Buffer {
        let to_color = |c: CellColor| match c {
            CellColor::Default => ratatui::style::Color::Reset,
            CellColor::Indexed(i) => ratatui::style::Color::Indexed(i),
            CellColor::Rgb(r, g, b) => ratatui::style::Color::Rgb(r, g, b),
        };
        let area = Rect::new(0, 0, snap.cols, snap.rows);
        let mut buf = Buffer::empty(area);
        for (y, row) in snap.cells.iter().enumerate() {
            for (x, cell) in row.iter().enumerate() {
                let Some(bc) = buf.cell_mut(Position::new(x as u16, y as u16)) else {
                    continue;
                };
                if cell.spacer {
                    bc.set_skip(true);
                    continue;
                }
                let mut style = Style::default().fg(to_color(cell.fg)).bg(to_color(cell.bg));
                if cell.bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.reverse {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                bc.set_symbol(&cell.sym);
                bc.set_style(style);
            }
        }
        buf
    }

    /// A style-aware comparable view of a snapshot for the relay test: content cells carry
    /// `(sym, fg, bg, bold, reverse)`; spacer cells collapse to a sentinel (their own colour is
    /// irrelevant — the wide glyph before them covers those columns and is never emitted).
    #[allow(clippy::type_complexity)]
    fn cells_norm(snap: &Snapshot) -> Vec<Vec<(String, CellColor, CellColor, bool, bool)>> {
        snap.cells
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| {
                        if c.spacer {
                            (
                                String::new(),
                                CellColor::Default,
                                CellColor::Default,
                                false,
                                false,
                            )
                        } else {
                            (c.sym.clone(), c.fg, c.bg, c.bold, c.reverse)
                        }
                    })
                    .collect()
            })
            .collect()
    }

    /// One tick of the server→client pipeline. `last` = the client's known baseline
    /// (advanced here); returns the client buffer AFTER applying the wire delta, exactly
    /// as `client::run_attached` does (`full` clears then applies; delta applies in place).
    fn roundtrip(server: &Buffer, last: &mut Buffer, client: &mut Buffer, full: bool) {
        if full {
            *last = Buffer::empty(server.area);
            *client = Buffer::empty(server.area);
        }
        let changed = last.diff(server);
        for (x, y, cell) in &changed {
            if let Some(bc) = client.cell_mut(Position::new(*x, *y)) {
                bc.set_symbol(cell.symbol());
                bc.set_style(cell.style());
                bc.set_skip(cell.skip);
            }
        }
        *last = server.clone();
    }

    /// Render a client buffer through a real ratatui `Terminal<TestBackend>` (its own
    /// diff + wide-char flush) and return the visible screen text, row by row.
    fn screen(term: &mut Terminal<TestBackend>, src: &Buffer) -> Vec<String> {
        term.draw(|frame| {
            let out = frame.buffer_mut();
            let area = *out.area();
            for y in 0..area.height {
                for x in 0..area.width {
                    if let (Some(s), Some(d)) = (
                        src.cell(Position::new(x, y)),
                        out.cell_mut(Position::new(x, y)),
                    ) {
                        *d = s.clone();
                    }
                }
            }
        })
        .unwrap();
        let b = term.backend().buffer();
        let (w, h) = (b.area.width, b.area.height);
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| b.cell(Position::new(x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn snap_text(snap: &Snapshot) -> Vec<String> {
        snap.cells
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| if c.spacer { "" } else { c.sym.as_str() })
                    .collect::<String>()
            })
            .collect()
    }

    /// The heart of it: after feeding a SEQUENCE of byte-batches (each = one server
    /// render tick), the client's on-screen text must equal the final snapshot's text.
    fn assert_pipeline(cols: usize, rows: usize, batches: &[&[u8]]) {
        let (mut t, mut p) = term(cols, rows);
        let backend = TestBackend::new(cols as u16, rows as u16);
        let mut cterm = Terminal::new(backend).unwrap();
        let mut last = Buffer::empty(Rect::new(0, 0, cols as u16, rows as u16));
        let mut client = Buffer::empty(Rect::new(0, 0, cols as u16, rows as u16));
        let mut first = true;
        let mut final_snap = None;
        for batch in batches {
            feed(&mut t, &mut p, batch);
            let snap = snapshot_grid(&t);
            let server = compose(&snap);
            roundtrip(&server, &mut last, &mut client, first);
            screen(&mut cterm, &client);
            first = false;
            final_snap = Some(snap);
        }
        let snap = final_snap.unwrap();
        let want = snap_text(&snap);
        let got = screen(&mut cterm, &client);
        assert_eq!(
            got, want,
            "\nclient screen diverged from server snapshot\n got: {got:?}\nwant: {want:?}"
        );
    }

    /// A `Write` sink shared with the caller so we can read back the exact escape-sequence
    /// bytes ratatui's `CrosstermBackend` emits (its `writer_mut` is feature-gated).
    #[derive(Clone)]
    struct SharedBytes(Rc<RefCell<Vec<u8>>>);
    impl std::io::Write for SharedBytes {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// END-TO-END FIDELITY: does comux faithfully relay one alacritty screen to another THROUGH
    /// the real emit path? Feed app bytes to a SOURCE `Term`; each tick compose → wire delta →
    /// client buffer → drive a real ratatui `Terminal<CrosstermBackend>` (its own diff + escape
    /// output) → replay the EMITTED BYTES into a spec-correct REFERENCE `Term`. If the reference
    /// screen matches the source, comux's emitted escape stream is correct — so any on-screen
    /// drift the owner sees is the OUTER emulator (copad) mis-rendering that stream, NOT comux.
    /// (`refresh_at` forces a full repaint before those ticks, exercising the clear+repaint path.)
    fn assert_relay_fidelity(cols: usize, rows: usize, batches: &[&[u8]], refresh_at: &[usize]) {
        let (mut src_t, mut src_p) = term(cols, rows);
        let (mut ref_t, mut ref_p) = term(cols, rows);
        let sink = SharedBytes(Rc::new(RefCell::new(Vec::new())));
        let area = Rect::new(0, 0, cols as u16, rows as u16);
        let mut cterm = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .unwrap();
        let mut client = Buffer::empty(area);
        for (i, batch) in batches.iter().enumerate() {
            feed(&mut src_t, &mut src_p, batch);
            let snap = snapshot_grid(&src_t);
            let server = compose(&snap);
            // First tick = full baseline; a `refresh_at` tick forces a clear+full repaint
            // (the self-heal / Ctrl-b r path); everything else is an incremental delta.
            let full = i == 0 || refresh_at.contains(&i);
            deliver(&server, &mut client, full);
            // The real client reconstructs wide-char spacers after applying each frame so its
            // buffer matches the server's (the wire omits them); model that here.
            crate::client::fix_wide_spacers(&mut client);
            if full {
                cterm.clear().unwrap();
            }
            let src_buf = client.clone();
            cterm
                .draw(|frame| {
                    let out = frame.buffer_mut();
                    for y in 0..area.height {
                        for x in 0..area.width {
                            if let (Some(s), Some(d)) = (
                                src_buf.cell(Position::new(x, y)),
                                out.cell_mut(Position::new(x, y)),
                            ) {
                                *d = s.clone();
                            }
                        }
                    }
                })
                .unwrap();
            let bytes = std::mem::take(&mut *sink.0.borrow_mut());
            feed(&mut ref_t, &mut ref_p, &bytes);
        }
        let src_snap = snapshot_grid(&src_t);
        let ref_snap = snapshot_grid(&ref_t);
        // Full-style comparison (glyph + fg/bg/bold/reverse), so a lost colour or attribute
        // fails too — not just a wrong glyph. The `snap_text` lines make the message readable.
        assert_eq!(
            cells_norm(&ref_snap),
            cells_norm(&src_snap),
            "\nRELAYED screen (via comux's emitted escapes) diverged from the SOURCE screen\n \
             got:  {:?}\n want: {:?}",
            snap_text(&ref_snap),
            snap_text(&src_snap),
        );
    }

    #[test]
    fn relay_fidelity_claude_code_like_session() {
        // A Claude-Code-ish full-screen session: alt-screen enter, a box with a wide-char
        // title, colored text, cursor jumps, PARTIAL interior redraws, a wide→narrow swap, a
        // mid-region clear, and a forced refresh — fed as many small server ticks so the
        // incremental-diff path is heavily exercised.
        assert_relay_fidelity(
            24,
            6,
            &[
                "\x1b[?1049h\x1b[2J\x1b[H".as_bytes(), // enter alt screen + clear
                "\x1b[H┌─ 세션 ─────────────┐".as_bytes(), // top border + wide title
                "\x1b[2;1H│ \x1b[32mready\x1b[0m            │".as_bytes(), // colored body
                "\x1b[3;1H│ 작업 중…            │".as_bytes(), // wide chars
                "\x1b[4;1H└────────────────────┘".as_bytes(), // bottom border
                "\x1b[2;4H\x1b[31mBUSY \x1b[0m".as_bytes(), // partial redraw over 'ready'
                "\x1b[3;4H가나다라마".as_bytes(),      // overwrite with more wide
                "\x1b[3;4Habcde".as_bytes(),           // wide→narrow at same origin
                "\x1b[2;2H\x1b[K│".as_bytes(),         // erase-to-EOL mid-line
                "\x1b[5;1H\x1b[38;5;39m▓▓▓▓▓▓\x1b[0m spinner".as_bytes(), // 256-color run
                "\x1b[5;1H\x1b[2Kdone".as_bytes(),     // clear line + replace
            ],
            &[8], // force a refresh (self-heal) before tick 8
        );
    }

    #[test]
    fn relay_fidelity_pure_delta_churn() {
        // Heavy PURE-INCREMENTAL path (no refresh): a scrolling/progress-style redraw that
        // rewrites the whole screen every tick with shifting wide+narrow content — the exact
        // churn where drift would accumulate. Fidelity must hold with deltas alone.
        let batches: Vec<Vec<u8>> = (0..20)
            .map(|i| {
                let mut s = String::from("\x1b[H");
                for row in 1..=5 {
                    let n = (i + row) % 7;
                    // Mix wide CJK, ASCII, and a moving colored marker per row.
                    s.push_str(&format!("\x1b[{row};1H\x1b[2K"));
                    for c in 0..6 {
                        if (c + n) % 3 == 0 {
                            s.push('가');
                        } else {
                            s.push_str(&format!("\x1b[3{}mX\x1b[0m", (c % 7) + 1));
                        }
                    }
                    s.push_str(&format!(" r{row}n{n}"));
                }
                s.into_bytes()
            })
            .collect();
        let refs: Vec<&[u8]> = batches.iter().map(|b| b.as_slice()).collect();
        assert_relay_fidelity(20, 6, &refs, &[]); // no refresh — deltas only
    }

    #[test]
    fn clear_after_full_screen_leaves_no_ghost() {
        // Fill the screen, then clear it — the classic ghost-after-clear case.
        assert_pipeline(
            10,
            3,
            &[b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123", b"\x1b[2J\x1b[H"],
        );
    }

    #[test]
    fn wide_chars_then_clear_leaves_no_ghost() {
        // CJK fills each row with wide glyph + spacer; clearing must wipe both halves.
        assert_pipeline(
            10,
            2,
            &[b"\xea\xb0\x80\xeb\x82\x98\xeb\x8b\xa4", b"\x1b[2J\x1b[H"],
        );
    }

    #[test]
    fn wide_char_replaced_by_narrow() {
        // A wide glyph overwritten by a narrow one at the same origin: the spacer half
        // must not linger as a ghost.
        assert_pipeline(6, 1, &[b"\x1b[H\xea\xb0\x80", b"\x1b[H", b"\x1b[Hxy"]);
    }

    /// Like [`assert_pipeline`] but models the real server's `sync_channel(1)` coalescing:
    /// the client only drains a frame every `drain_every` ticks. On a "drop" tick the frame
    /// is NOT delivered and `last` is NOT advanced (server semantics); the main loop keeps
    /// re-composing (pending) until the client drains, so the delta must catch up. After the
    /// last batch we flush all pending deliveries. Final screen must equal the final snapshot.
    fn assert_pipeline_backpressure(
        cols: usize,
        rows: usize,
        batches: &[&[u8]],
        drain_every: usize,
    ) {
        let (mut t, mut p) = term(cols, rows);
        let mut cterm = Terminal::new(TestBackend::new(cols as u16, rows as u16)).unwrap();
        let area = Rect::new(0, 0, cols as u16, rows as u16);
        let mut last = Buffer::empty(area); // server's view of the client baseline
        let mut client = Buffer::empty(area); // client's actual buffer
        let mut queued: Option<Buffer> = None; // the 1-slot channel (holds a composed frame)
        let mut first = true;
        let mut final_snap = None;
        let mut tick = 0usize;
        for batch in batches {
            feed(&mut t, &mut p, batch);
            let snap = snapshot_grid(&t);
            final_snap = Some(snap.clone());
            let server = compose(&snap);
            // Server tick: diff vs last; try to enqueue. Channel full (queued Some) => drop.
            let changed = last.diff(&server);
            // Enqueue if there's something to send AND the 1-slot channel is free; a full
            // channel drops the frame (last stays put — a real loop re-composes via pending).
            if (!changed.is_empty() || first) && queued.is_none() {
                queued = Some(server.clone());
                last = server.clone(); // advance only on successful enqueue
            }
            // Client drains on some ticks only.
            tick += 1;
            if tick.is_multiple_of(drain_every)
                && let Some(frame) = queued.take()
            {
                deliver(&frame, &mut client, first);
                screen(&mut cterm, &client);
                first = false;
            }
        }
        // Drain whatever is left, plus force a final catch-up compose (models `pending`).
        if let Some(frame) = queued.take() {
            deliver(&frame, &mut client, first);
            screen(&mut cterm, &client);
            first = false;
        }
        let snap = final_snap.unwrap();
        let server = compose(&snap);
        let changed = last.diff(&server);
        if !changed.is_empty() {
            deliver(&server, &mut client, first);
            screen(&mut cterm, &client);
        }
        let want = snap_text(&snap);
        let got = screen(&mut cterm, &client);
        assert_eq!(
            got, want,
            "\nbackpressure divergence\n got: {got:?}\nwant: {want:?}"
        );
    }

    /// Apply one wire frame to the client buffer (full = clear+apply, delta = apply in place).
    fn deliver(frame: &Buffer, client: &mut Buffer, full: bool) {
        if full {
            *client = Buffer::empty(frame.area);
        }
        // The wire is the diff the server computed vs ITS last; but here `frame` is the full
        // composed server buffer, so re-derive the changed set against an empty baseline for
        // full, or trust the caller advanced last. We instead just copy non-skip cells that
        // differ — equivalent to applying the delta the server would have sent.
        let base = if full {
            Buffer::empty(frame.area)
        } else {
            client.clone()
        };
        for (x, y, cell) in base.diff(frame) {
            if let Some(bc) = client.cell_mut(Position::new(x, y)) {
                bc.set_symbol(cell.symbol());
                bc.set_style(cell.style());
                bc.set_skip(cell.skip);
            }
        }
    }

    #[test]
    fn backpressure_coalescing_converges() {
        // Rapid full-screen churn with a client that drains 1-in-3 frames: the coalesced
        // deltas must still converge to the final screen (no lingering blanks/ghosts).
        assert_pipeline_backpressure(
            8,
            3,
            &[
                b"\x1b[HAAAAAAAA\x1b[2;1HBBBBBBBB\x1b[3;1HCCCCCCCC",
                b"\x1b[2J\x1b[Hx",
                b"\x1b[HDDDDDDDD\x1b[2;1HEEEEEEEE",
                b"\x1b[2J\x1b[H",
                b"\x1b[Hfinal!!!",
            ],
            3,
        );
    }

    #[test]
    fn wheel_sgr_mouse_mode() {
        // App negotiated SGR mouse reporting → SGR wheel button (64 up / 65 down) at coords.
        let m = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            wheel_bytes_for_mode(m, true, 5, 9).unwrap(),
            b"\x1b[<64;5;9M"
        );
        assert_eq!(
            wheel_bytes_for_mode(m, false, 5, 9).unwrap(),
            b"\x1b[<65;5;9M"
        );
    }

    #[test]
    fn wheel_legacy_mouse_mode() {
        // Mouse reporting without SGR → legacy ESC [ M Cb Cx Cy (each offset by 32).
        let m = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            wheel_bytes_for_mode(m, true, 1, 1).unwrap(),
            vec![0x1b, b'[', b'M', 96, 33, 33]
        );
    }

    #[test]
    fn wheel_utf8_mouse_mode_encodes_wide_coords() {
        // Mode 1005 (UTF8) without SGR: small coords are 1-byte; a coord past 223 becomes a
        // 2-byte UTF-8 char rather than clamping to a wrong cell.
        let m = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        // col=1,row=1 → Cb=96, Cx=33, Cy=33 (all 1-byte, same as legacy here).
        assert_eq!(
            wheel_bytes_for_mode(m, true, 1, 1).unwrap(),
            vec![0x1b, b'[', b'M', 96, 33, 33]
        );
        // col=300 → 300+32=332 = U+014C, a 2-byte UTF-8 sequence (0xC5 0x8C).
        let got = wheel_bytes_for_mode(m, true, 300, 1).unwrap();
        let mut want = vec![0x1b, b'[', b'M', 96u8];
        want.extend_from_slice('\u{14c}'.to_string().as_bytes());
        want.push(33);
        assert_eq!(got, want);
    }

    #[test]
    fn wheel_alternate_scroll_sends_arrows() {
        // Alt-screen pager (less/man) without mouse mode → cursor keys; app-cursor picks SS3.
        let base = TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN;
        assert_eq!(wheel_bytes_for_mode(base, true, 1, 1).unwrap(), b"\x1b[A");
        assert_eq!(wheel_bytes_for_mode(base, false, 1, 1).unwrap(), b"\x1b[B");
        let app = base | TermMode::APP_CURSOR;
        assert_eq!(wheel_bytes_for_mode(app, true, 1, 1).unwrap(), b"\x1bOA");
    }

    #[test]
    fn wheel_no_mouse_app_yields_none() {
        // A plain shell (no mouse, no alternate-scroll) → None → caller scrolls scrollback.
        assert_eq!(wheel_bytes_for_mode(TermMode::empty(), true, 1, 1), None);
        // Alternate-scroll but NOT on the alt screen (a normal prompt) also declines.
        assert_eq!(
            wheel_bytes_for_mode(TermMode::ALTERNATE_SCROLL, true, 1, 1),
            None
        );
    }

    #[test]
    fn box_drawing_partial_redraw() {
        // Mimic a TUI (Claude Code-like) drawing a box, then redrawing only its interior —
        // exercises partial deltas over previously-painted cells.
        assert_pipeline(
            8,
            3,
            &[
                "\x1b[H┌──────┐\x1b[2;1H│      │\x1b[3;1H└──────┘".as_bytes(),
                "\x1b[2;2Hhello".as_bytes(),
            ],
        );
    }
}
