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
use alacritty_terminal::term::{Config, Term};
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
}

impl MuxListener {
    fn new() -> Self {
        Self {
            sender: Arc::new(std::sync::Mutex::new(None)),
            child_exited: Arc::new(AtomicBool::new(false)),
        }
    }
    fn set_sender(&self, s: EventLoopSender) {
        *self.sender.lock().unwrap() = Some(s);
    }
}

impl EventListener for MuxListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(reply) => {
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
        })
    }

    /// The child shell's pid (fallback label when no foreground group is set).
    pub fn pid(&self) -> Option<u32> {
        self.child_pid
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
    }

    /// Has the child shell exited?
    pub fn has_exited(&self) -> bool {
        self.listener.child_exited.load(Ordering::Relaxed)
    }

    /// Scroll the viewport through scrollback: positive `lines` = UP (older),
    /// negative = DOWN (newer). `snapshot` then renders at the new offset.
    pub fn scroll(&self, lines: i32) {
        if lines != 0 {
            self.term.lock().scroll_display(Scroll::Delta(lines));
        }
    }

    /// Jump the viewport back to the live bottom (offset 0).
    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    /// How many lines the viewport is scrolled up from the live bottom (0 = live).
    pub fn scroll_offset(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    /// Snapshot the visible viewport for rendering. Keeps the term lock only for
    /// the copy so the reader thread isn't starved.
    pub fn snapshot(&self) -> Snapshot {
        let term = self.term.lock();
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
}

impl Drop for PaneTerm {
    fn drop(&mut self) {
        if let Some(fd) = self.fg_fd.take() {
            // SAFETY: our own dup'd fd, closed exactly once.
            unsafe { libc::close(fd) };
        }
        let _ = self.sender.send(Msg::Shutdown);
        if let Some(jh) = self.io_thread.take() {
            let _ = jh.join();
        }
    }
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
