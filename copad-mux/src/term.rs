//! A single hosted terminal: a PTY + `alacritty_terminal` parser/grid, running
//! on alacritty's own `EventLoop` reader thread. This is the mux's terminal
//! runtime — the same proven engine `copad-term` uses (alacritty_terminal 0.26),
//! but a clean Rust surface (no C-ABI) so the server/TUI can host many of them.
//!
//! Work-unit 2 hosts exactly one; splits + the multi-pane server come later.

use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{ClipboardType, Config, Term, TermMode};
use alacritty_terminal::tty::{self, Options as TtyOptions, Pty, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, CursorStyle, NamedColor};
use ratatui::style::Modifier;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    /// The rest of the cell's SGR attributes — italic, dim, underline (any style), blink,
    /// hidden, strikethrough — as the ratatui modifiers the wire already carries. Bold and
    /// reverse keep their own fields above.
    pub attrs: Modifier,
}

/// A snapshot of the visible grid + cursor for one render tick.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    /// `rows` × `cols` cells, row-major, top-to-bottom of the viewport.
    pub cells: Vec<Vec<CellSnap>>,
    /// Per-row soft-wrap flag: `wrapped[r]` is true when viewport row `r` continues onto row
    /// `r+1` (no logical line break), so a drag-copy across the seam joins them without a `\n`.
    pub wrapped: Vec<bool>,
    /// Cursor position in viewport coordinates `(col, row)`.
    pub cursor: (u16, u16),
    /// Whether the program wants the cursor shown (`?25h`) and it is on screen (not
    /// scrolled into history). A hidden cursor still has a position: the client parks the
    /// real cursor there so an IME's composition window opens where the text goes.
    pub cursor_visible: bool,
    /// The program's cursor shape as a `DECSCUSR` parameter (`1`–`6`, vim's insert-mode bar
    /// is `6`), or `0` when it never set one — the outer terminal's own default.
    pub cursor_shape: u8,
    /// Copy mode's cursor and selection, read under the SAME lock as `cells` so what is
    /// highlighted is exactly what a yank would copy. `None` outside copy mode.
    pub copy: Option<CopyView>,
}

/// Copy mode as the renderer needs it, in VIEWPORT rows (a row may be off-screen: negative
/// or past the bottom when the selection extends beyond the view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyView {
    /// The vi cursor, when it is on screen.
    pub cursor: Option<(u16, u16)>,
    /// Selection `(start, end)` as `(row, col)`, inclusive, and whether it is a block.
    pub selection: Option<(CopyCell, CopyCell, bool)>,
    /// Every on-screen match of the active search, `(start, end)` inclusive (a match may wrap
    /// onto the next row). Highlighted so `n`/`N` has somewhere visible to go.
    pub matches: Vec<(CopyCell, CopyCell)>,
}

/// A `(viewport row, column)` copy-mode position; the row may be off-screen.
pub type CopyCell = (i32, u16);

impl CopyView {
    /// Whether viewport cell `(row, col)` is inside the selection.
    pub fn selected(&self, row: i32, col: u16) -> bool {
        let Some(((r0, c0), (r1, c1), block)) = self.selection else {
            return false;
        };
        if row < r0 || row > r1 {
            return false;
        }
        if block {
            return col >= c0.min(c1) && col <= c0.max(c1);
        }
        (row > r0 || col >= c0) && (row < r1 || col <= c1)
    }

    /// Whether viewport cell `(row, col)` is part of a search match.
    pub fn matched(&self, row: i32, col: u16) -> bool {
        self.matches.iter().any(|&((r0, c0), (r1, c1))| {
            row >= r0 && row <= r1 && (row > r0 || col >= c0) && (row < r1 || col <= c1)
        })
    }
}

/// How a copy-mode selection is shaped (`v`, `V`, `Ctrl-v`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelKind {
    Chars,
    Lines,
    Block,
}

/// The most matches one frame highlights; a pathological one-letter query on a screen of
/// that letter should cost a bounded walk under the pane lock.
const MAX_HIGHLIGHTS: usize = 512;

/// Largest text a copy-mode yank may produce, in bytes (the same cap as an OSC 52 write).
pub const MAX_YANK_BYTES: usize = 1 << 20;

/// What a copy-mode search found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOutcome {
    Found,
    /// Found, after wrapping past the top/bottom of the history.
    Wrapped,
    NoMatch,
}

/// Escape `s` for a literal regex match.
fn regex_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Largest OSC 52 payload a pane app may put on the clipboard, in bytes of decoded text.
/// A runaway/hostile program must not be able to pin unbounded memory in the server between
/// render ticks — anything past this is dropped whole (a truncated clipboard would be worse
/// than none: you'd paste a silently-cut command).
const MAX_CLIPBOARD_BYTES: usize = 1 << 20; // 1 MiB

/// Ticket dispenser stamping every captured clipboard write, so the drain can pick the
/// genuinely LATEST one across panes. Panes live in a `HashMap`, whose iteration order is
/// arbitrary — without a sequence, two panes writing inside one render tick would relay
/// whichever the traversal happened to reach last, which can be the older write.
/// Starts at 1 so that 0 is a usable "nothing relayed yet" sentinel for the drain's
/// high-water mark — with a 0-based counter the very FIRST write compares equal to it and
/// is dropped.
static CLIPBOARD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Take the next clipboard sequence number. Drag-selection copies draw from the SAME
/// dispenser as pane writes, so the two sources can be ordered against each other and
/// neither can silently overwrite a newer copy that has not been delivered yet.
pub fn next_clipboard_seq() -> u64 {
    CLIPBOARD_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Minimal `EventListener`: forwards `PtyWrite` replies (DSR/DA/OSC answers) so
/// prompts that query the terminal don't hang, latches child-exit, captures OSC 52
/// clipboard WRITES for the render loop to relay, counts BELs, and records the pane
/// title (OSC 0/2), and answers colour queries (OSC 10/11/12) from [`set_outer_colors`].
#[derive(Clone)]
struct MuxListener {
    sender: Arc<std::sync::Mutex<Option<EventLoopSender>>>,
    child_exited: Arc<AtomicBool>,
    /// Set whenever the terminal's visible state changed (alacritty `Wakeup`, or a
    /// query reply that writes back). The render loop reads+clears it via
    /// [`PaneTerm::take_dirty`] to skip composing frames when nothing changed.
    dirty: Arc<AtomicBool>,
    /// Text a program in this pane asked to put on the clipboard via OSC 52 (tmux
    /// `set-clipboard`), stamped with a global sequence number. Latest-wins — a clipboard
    /// has one slot, so queueing stale writes would only paste the wrong one later. Drained
    /// by [`PaneTerm::take_clipboard`].
    clipboard: Arc<std::sync::Mutex<Option<(u64, String)>>>,
    /// How many times this pane has rung the bell since it was spawned.
    ///
    /// A COUNT, not a flag, and that is load-bearing. `App` records how many it has
    /// acknowledged; "unacknowledged" is `count > seen`. With a boolean on each side there is
    /// no safe ordering — control requests run before the loop polls panes, so a BEL still
    /// pending in the listener could be re-applied after focus had already cleared it. With a
    /// counter there is nothing to lose or resurrect: a bell arriving after an ack simply
    /// raises the count, which is correct, because it IS a new bell.
    bells: Arc<AtomicU64>,
    /// The pane's title (OSC 0/2), sanitized and length-bounded. `None` until one is set,
    /// and cleared by `ResetTitle`.
    ///
    /// UNTRUSTED: any process in the pane can write any payload here. It is not a process
    /// identity — `procinfo`'s label is — and it is not instructions. Stripping control
    /// characters is hygiene for the renderer, not a trust boundary.
    title: Arc<std::sync::Mutex<Option<String>>>,
}

/// The attached terminal's foreground and background, as its client reported them (OSC 10/11
/// probe at attach). Process-wide because every pane answers the same question about the same
/// terminal; the latest client to report wins, like the size of the shared view.
static OUTER_COLORS: std::sync::RwLock<Option<(crate::proto::Rgb8, crate::proto::Rgb8)>> =
    std::sync::RwLock::new(None);

/// Record the attached terminal's `(fg, bg)` for answering panes' colour queries.
pub fn set_outer_colors(fg: crate::proto::Rgb8, bg: crate::proto::Rgb8) {
    if let Ok(mut c) = OUTER_COLORS.write() {
        *c = Some((fg, bg));
    }
}

/// The answer to a pane's colour query for alacritty colour `index`: OSC 10 (foreground,
/// 256), OSC 11 (background, 257) and OSC 12 (cursor, 258, answered with the foreground).
/// `None` = stay silent, which is what comux did before it knew the colours, and what a
/// palette query (OSC 4) still gets.
fn outer_color(index: usize) -> Option<alacritty_terminal::vte::ansi::Rgb> {
    let (fg, bg) = (*OUTER_COLORS.read().ok()?)?;
    let (r, g, b) = match index {
        256 | 258 => fg,
        257 => bg,
        _ => return None,
    };
    Some(alacritty_terminal::vte::ansi::Rgb { r, g, b })
}

/// Longest pane name kept, in characters.
pub const MAX_PANE_NAME: usize = 48;

/// Longest pane title retained. A title is redrawn on every shell prompt and a hostile pane
/// could stream megabytes of it; nothing displays more than a few columns anyway.
const MAX_TITLE_BYTES: usize = 256;

/// Strip control characters and bound the length of a title before it is stored.
///
/// Runs on the reader thread OUTSIDE the title lock — sanitizing under the lock would hold
/// it for the length of a hostile payload while the render loop waits behind it.
fn sanitize_title(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_TITLE_BYTES));
    for c in raw.chars() {
        // Control characters (including a stray ESC) would let a title move the cursor or
        // recolour the chrome it is rendered into.
        if c.is_control() {
            continue;
        }
        if out.len() + c.len_utf8() > MAX_TITLE_BYTES {
            break; // cut on a char boundary, never mid-scalar
        }
        out.push(c);
    }
    out
}

impl MuxListener {
    fn new() -> Self {
        Self {
            sender: Arc::new(std::sync::Mutex::new(None)),
            child_exited: Arc::new(AtomicBool::new(false)),
            // Start dirty so the very first frame is composed.
            dirty: Arc::new(AtomicBool::new(true)),
            clipboard: Arc::new(std::sync::Mutex::new(None)),
            bells: Arc::new(AtomicU64::new(0)),
            title: Arc::new(std::sync::Mutex::new(None)),
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
            // A program in the pane wrote the clipboard (`ESC ] 52 ; c ; <base64> BEL`).
            // Stash it for the render loop to relay to the attached clients, which re-emit
            // it as their OWN OSC 52 — so it lands on the clipboard of the machine you are
            // SITTING AT, not the server's. Deliberately does NOT mark the pane dirty:
            // nothing on screen changed.
            // An EMPTY payload is a legitimate request to CLEAR the clipboard, not a no-op,
            // so it is captured like any other write.
            Event::ClipboardStore(ClipboardType::Clipboard, text)
                if text.len() <= MAX_CLIPBOARD_BYTES =>
            {
                // Stamp INSIDE the critical section: taking the ticket first would let two
                // writers commit in the opposite order and leave the older text in the slot.
                let mut slot = self.clipboard.lock().unwrap();
                *slot = Some((next_clipboard_seq(), text));
            }
            // Over the cap. Dropped WHOLE rather than truncated — half a clipboard silently
            // replacing what you copied is worse than the write not landing at all.
            Event::ClipboardStore(ClipboardType::Clipboard, _) => {}
            // The X11 PRIMARY selection (OSC 52 selector `p`/`s`). comux has no notion of it
            // (and macOS has none at all), so it is dropped rather than silently aliased onto
            // the real clipboard — an app asking for PRIMARY does not expect a Cmd-V clobber.
            Event::ClipboardStore(ClipboardType::Selection, _) => {}
            // OSC 52 clipboard READ (`ESC ] 52 ; c ; ? BEL`). Never answered: replying would
            // hand the host's clipboard contents to any program running in a pane. Dropping
            // it is also what VTE and (by default) most terminals do.
            Event::ClipboardLoad(..) => {}
            // A BEL. Counted, never toasted: interactive shells ring for ambiguous
            // tab-completion, the end of history, the end of a pager — wiring that to a
            // desktop notification would bury the agent toasts (#102) in noise. Does not mark
            // the pane dirty on its own; the marker rides the next frame.
            Event::Bell => {
                self.bells.fetch_add(1, Ordering::Relaxed);
            }
            // OSC 0 / OSC 2. Sanitize BEFORE taking the lock (see `sanitize_title`).
            Event::Title(raw) => {
                let clean = sanitize_title(&raw);
                *self.title.lock().unwrap() = Some(clean);
            }
            Event::ResetTitle => {
                *self.title.lock().unwrap() = None;
            }
            // OSC 10/11/12 `?`: answered with the attached terminal's own colours when its
            // client reported them, so dark/light detection sees the real background.
            Event::ColorRequest(index, format) => {
                if let Some(rgb) = outer_color(index)
                    && let Some(s) = self.sender.lock().unwrap().as_ref()
                {
                    let _ = s.send(Msg::Input(format(rgb).into_bytes().into()));
                }
            }
            _ => {}
        }
    }
}

/// A hosted terminal pane.
/// The environment variable naming a pane's identity inside its own shell — the
/// `$TMUX_PANE` analogue. A hook running inside a pane reads it to tell comux WHICH
/// pane raised a notification (`comux notify --pane "$COPAD_MUX_PANE"`).
pub const PANE_TOKEN_ENV: &str = "COPAD_MUX_PANE";

/// Mint a fresh pane token, unique across this server's lifetime AND across restarts.
///
/// The cross-restart part is why this is not just a counter: `TerminalId`s (and any
/// naive counter) restart at zero with the server, so a toast left on screen from a
/// previous incarnation would resolve against a DIFFERENT pane and jump somewhere
/// wrong. Qualifying every token with a per-process nonce (pid + boot second) makes a
/// stale click resolve to nothing, which reports "unknown pane" instead.
pub fn next_pane_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", incarnation())
}

/// This server process's identity (pid + boot second, hex) — the nonce every pane token is
/// qualified by, and the `incarnation` the L0 host API and the render `Hello` announce, so a
/// host can tell a restarted server from the one whose ids it holds (decisions #166).
pub fn incarnation() -> &'static str {
    static NONCE: OnceLock<String> = OnceLock::new();
    NONCE.get_or_init(|| {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{:x}{:x}", std::process::id(), secs)
    })
}

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
    /// The pane identity handed to the shell as `COPAD_MUX_PANE` (see
    /// [`next_pane_token`]) — read back out of the spawn env rather than passed
    /// separately, so every spawn path that builds a pane env gets it for free.
    pane_token: Option<String>,
    /// The copy-mode search to highlight (the literal query), while one is active.
    copy_query: std::sync::Mutex<Option<String>>,
    /// A name the user gave the pane (`Ctrl-b T`, `comux rename-pane`); beats the program on
    /// its title. Saved with the layout.
    name: std::sync::Mutex<Option<String>>,
}

/// Turn a pane-spawn I/O error into a message worth showing a user.
///
/// Descriptor exhaustion gets its own wording because it is the one failure that is
/// both self-inflicted and fixable: the raw `EMFILE` text ("too many open files")
/// reads like a system problem, when the actual answer is that this server's soft
/// limit is too low for the number of panes it hosts. Pointing at `comux health`
/// turns a dead keypress into a diagnosis.
fn describe_spawn_failure(stage: &str, err: &std::io::Error) -> String {
    let budget = crate::fdlimit::snapshot().ok();
    // EMFILE = this process is out of descriptors; ENFILE = the system-wide table is.
    // Detected primarily from our OWN budget rather than the error: the PTY layer
    // formats the errno into a message string, so `raw_os_error` is routinely `None`
    // by the time the failure reaches us. A spawn that failed with less than a pane's
    // worth of descriptors left is exhaustion whatever the text says — and the errno
    // check still catches the case where the budget is unreadable.
    let no_room = budget
        .and_then(|b| b.panes_remaining())
        .is_some_and(|room| room < 2);
    let exhausted =
        no_room || matches!(err.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE));
    if exhausted {
        let detail = budget
            .map(|b| format!(" ({} of {} in use)", b.open.unwrap_or(0), b.soft))
            .unwrap_or_default();
        format!(
            "out of file descriptors{detail} — this server cannot host more panes; \
             `comux health` shows the budget, `comux server restart` from a shell re-raises it"
        )
    } else {
        format!("{stage}: {err}")
    }
}

impl PaneTerm {
    /// Spawn a shell in a PTY sized `cols`×`rows`. `shell` defaults to `$SHELL`
    /// (then the system default); `cwd` to the process cwd.
    pub fn spawn(
        cols: u16,
        rows: u16,
        shell: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<Self, String> {
        Self::spawn_with_env(cols, rows, shell, cwd, &[])
    }

    /// Like [`spawn`](Self::spawn) but injects extra environment variables into
    /// the child shell (e.g. `COPAD_MUX_SOCK` so a shell inside a pane can drive
    /// its own mux via `copad-mux ctl`).
    ///
    /// Returns the failure REASON rather than a bare `None`: the callers that create
    /// panes on a keypress (new tab / new session / split) roll the just-created
    /// tab or session back when the shell can't spawn, so without a reason the whole
    /// failure is invisible — the key simply appears to do nothing. Descriptor
    /// exhaustion in particular (see [`crate::fdlimit`]) is undiagnosable from the
    /// UI without it.
    pub fn spawn_with_env(
        cols: u16,
        rows: u16,
        shell: Option<String>,
        cwd: Option<PathBuf>,
        env: &[(String, String)],
    ) -> Result<Self, String> {
        // Login shell so PATH / profile are set up as in a normal terminal.
        let program = shell
            .or_else(|| std::env::var("SHELL").ok())
            .map(|sh| Shell::new(sh, vec!["-l".to_string()]));
        Self::spawn_program(cols, rows, program, cwd, env)
    }

    /// Spawn `argv` as the pane's process instead of a shell. The pane ends when it exits.
    pub fn spawn_argv(
        cols: u16,
        rows: u16,
        argv: &[String],
        cwd: Option<PathBuf>,
        env: &[(String, String)],
    ) -> Result<Self, String> {
        let (prog, args) = argv
            .split_first()
            .ok_or_else(|| "nothing to run".to_string())?;
        Self::spawn_program(
            cols,
            rows,
            Some(Shell::new(prog.clone(), args.to_vec())),
            cwd,
            env,
        )
    }

    fn spawn_program(
        cols: u16,
        rows: u16,
        program: Option<Shell>,
        cwd: Option<PathBuf>,
        env: &[(String, String)],
    ) -> Result<Self, String> {
        let cols = cols.max(1);
        let rows = rows.max(1);

        let mut opts = TtyOptions {
            shell: program,
            ..TtyOptions::default()
        };
        let spawn_cwd = cwd.clone();
        if let Some(dir) = cwd {
            opts.working_directory = Some(dir);
        }
        // A pane is comux's terminal, not the one the server happened to be born in: without
        // this a server started from Ghostty hands every pane `TERM=xterm-ghostty` (features
        // comux's emulator does not have, and a terminfo an SSH host may lack), and one started
        // by launchd hands it no TERM at all. tmux does the same with `default-terminal`. An
        // explicit `--env` below still wins.
        opts.env.insert("TERM".into(), "xterm-256color".into());
        opts.env.insert("COLORTERM".into(), "truecolor".into());
        let mut pane_token = None;
        for (k, v) in env {
            if k == PANE_TOKEN_ENV {
                pane_token = Some(v.clone());
            }
            opts.env.insert(k.clone(), v.clone());
        }

        let window = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        };
        let pty = tty::new(&opts, window, 0).map_err(|e| describe_spawn_failure("pty", &e))?;
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
        let config = pane_config();
        let term = Term::new(config, &term_size, listener.clone());
        let term = Arc::new(FairMutex::new(term));

        let event_loop = EventLoop::new(Arc::clone(&term), listener.clone(), pty, false, false)
            .map_err(|e| describe_spawn_failure("pty event loop", &e))?;
        let sender = event_loop.channel();
        listener.set_sender(sender.clone());
        let io_thread = event_loop.spawn();

        Ok(Self {
            term,
            sender,
            listener,
            io_thread: Some(io_thread),
            child_pid,
            fg_fd,
            spawn_cwd,
            pane_token,
            copy_query: std::sync::Mutex::new(None),
            name: std::sync::Mutex::new(None),
        })
    }

    /// The child shell's pid (fallback label when no foreground group is set).
    pub fn pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// This pane's `COPAD_MUX_PANE` identity, if its spawn env carried one. What a
    /// notification's click action and `comux notify --pane` address.
    pub fn pane_token(&self) -> Option<&str> {
        self.pane_token.as_deref()
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

    /// Take any text a program in this pane asked to put on the clipboard via OSC 52,
    /// clearing the slot. `None` when nothing was written since the last call; the `u64`
    /// orders writes across panes. An empty string is a real value — "clear the clipboard".
    pub fn take_clipboard(&self) -> Option<(u64, String)> {
        self.listener.clipboard.lock().unwrap().take()
    }

    /// Feed raw bytes straight into this pane's terminal, bypassing the PTY — the injection
    /// point for the render-fidelity harness, which needs to put *chosen* content on a pane's
    /// grid without depending on a shell to echo it. The caller owns the VTE `Processor` so
    /// escape sequences can be split across calls exactly as a real read would split them.
    #[cfg(test)]
    pub(crate) fn feed_test_bytes(
        &self,
        parser: &mut alacritty_terminal::vte::ansi::Processor,
        bytes: &[u8],
    ) {
        parser.advance(&mut *self.term.lock(), bytes);
        self.mark_dirty();
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

    /// Lines of scrollback above the screen — the ceiling for [`Self::scroll_offset`].
    pub fn history_len(&self) -> usize {
        self.term.lock().grid().history_size()
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

    /// Bytes for a plain left click at `col`/`row` (1-based, pane-relative), or `None` if
    /// the app isn't listening for the mouse (a shell) or the pane is scrolled back into
    /// comux's scrollback, where the click would land on a cell the app isn't showing.
    pub fn click_bytes(&self, col: u16, row: u16) -> Option<Vec<u8>> {
        let term = self.term.lock();
        if term.grid().display_offset() != 0 {
            return None;
        }
        click_bytes_for_mode(*term.mode(), col, row)
    }

    /// Whether the pane's program asked for bracketed paste (`?2004h`).
    pub fn bracketed_paste(&self) -> bool {
        self.term.lock().mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Tell the program its pane gained or lost focus (`ESC [ I` / `ESC [ O`), if it asked
    /// for focus reports (`?1004h`). Returns whether it did.
    pub fn report_focus(&self, gained: bool) -> bool {
        if !self.term.lock().mode().contains(TermMode::FOCUS_IN_OUT) {
            return false;
        }
        self.input(if gained { b"\x1b[I" } else { b"\x1b[O" });
        true
    }

    /// The input modes the pane's program asked for, which decide how a key is encoded.
    pub fn key_modes(&self) -> crate::keys::KeyModes {
        let term = self.term.lock();
        let mode = term.mode();
        crate::keys::KeyModes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            kitty_disambiguate: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
            kitty_all_keys: mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
        }
    }

    /// Whether the pane is currently on the ALTERNATE screen (`?1049h`) — i.e. a
    /// full-screen app (nvim, less, htop, …) is running. The render loop polls this to
    /// notice when such an app EXITS (alt → primary), so it can force a full repaint and
    /// wipe any incremental-diff residue the alt→primary grid swap left behind.
    pub fn in_alt_screen(&self) -> bool {
        self.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }

    /// Snapshot the visible viewport for rendering. Keeps the term lock only for
    /// the copy so the reader thread isn't starved.
    pub fn snapshot(&self) -> Snapshot {
        let query = self.copy_query.lock().ok().and_then(|q| q.clone());
        snapshot_with_search(&self.term.lock(), query.as_deref())
    }

    // --- copy mode (decisions #147): alacritty's own vi mode, every op under ONE lock ---

    /// Enter copy mode: the vi cursor starts on the terminal cursor (or the viewport's top
    /// when that is scrolled out of view). Returns whether the alternate screen is up, which
    /// the caller remembers — a swap clears the selection but NOT the vi cursor, so copy mode
    /// is cancelled when it changes.
    pub fn copy_enter(&self) -> bool {
        let mut t = self.term.lock();
        if !t.mode().contains(TermMode::VI) {
            t.toggle_vi_mode();
        }
        self.mark_dirty();
        t.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Re-apply the pane configuration with the current [`history_limit`]. Lowering it drops
    /// the oldest lines at once. The caller leaves copy mode FIRST (`App::exit_copy_mode`):
    /// the vi cursor could otherwise point at a row that no longer exists.
    pub fn apply_history_limit(&self, lines: usize) {
        self.term.lock().set_options(Config {
            scrolling_history: lines,
            ..pane_config()
        });
        self.mark_dirty();
    }

    /// Leave copy mode: no selection, vi mode off, back at the live bottom.
    pub fn copy_exit(&self) {
        let mut t = self.term.lock();
        t.selection = None;
        if t.mode().contains(TermMode::VI) {
            t.toggle_vi_mode();
        }
        t.scroll_display(Scroll::Bottom);
        drop(t);
        if let Ok(mut q) = self.copy_query.lock() {
            *q = None;
        }
        self.mark_dirty();
    }

    pub fn copy_motion(&self, motion: alacritty_terminal::vi_mode::ViMotion) {
        self.term.lock().vi_motion(motion);
        self.mark_dirty();
    }

    /// Scroll the view by `lines` (positive = into history) and carry the vi cursor with it,
    /// the way alacritty's own vi mode pages.
    pub fn copy_scroll(&self, lines: i32) {
        let mut t = self.term.lock();
        // Target first: `scroll_display` already clamps the vi cursor into the new view, and
        // moving that clamped cursor by `lines` again would page twice as far.
        let target = t.vi_mode_cursor.scroll(&t, lines);
        t.scroll_display(Scroll::Delta(lines));
        t.vi_goto_point(target.point);
        drop(t);
        self.mark_dirty();
    }

    /// The top of the history (`g`).
    pub fn copy_top(&self) {
        let mut t = self.term.lock();
        let top = Point::new(t.topmost_line(), Column(0));
        t.vi_goto_point(top);
        drop(t);
        self.mark_dirty();
    }

    /// Toggle a selection of `ty` at the vi cursor (`v` / `V`); the same key again clears it.
    /// Returns whether a selection is up afterwards — read under the same lock, so a swap the
    /// PTY thread processes right after cannot make the caller believe there never was one.
    pub fn copy_toggle_selection(&self, kind: SelKind) -> bool {
        use alacritty_terminal::index::Side;
        use alacritty_terminal::selection::{Selection, SelectionType};
        let ty = match kind {
            SelKind::Chars => SelectionType::Simple,
            SelKind::Lines => SelectionType::Lines,
            SelKind::Block => SelectionType::Block,
        };
        let mut t = self.term.lock();
        if t.selection.as_ref().is_some_and(|s| s.ty == ty) {
            t.selection = None;
        } else if let Some(sel) = t.selection.as_mut() {
            // Another shape is up: reshape it in place, keeping its anchor (vim's `v` → `V`
            // → `Ctrl-v`), rather than starting over at the cursor.
            sel.ty = ty;
            sel.include_all();
        } else {
            let at = t.vi_mode_cursor.point;
            let mut sel = Selection::new(ty, at, Side::Left);
            // A fresh selection's endpoints coincide, and alacritty never extends an EMPTY
            // selection along with the cursor — include the cursor cell so it can grow.
            sel.include_all();
            t.selection = Some(sel);
            t.vi_goto_point(at);
        }
        let up = t.selection.is_some();
        drop(t);
        self.mark_dirty();
        up
    }

    /// Move the cursor to the selection's OTHER end (tmux `other-end`, vim `o`), which
    /// becomes the end that moves. Returns whether there was a selection.
    pub fn copy_other_end(&self) -> bool {
        use alacritty_terminal::index::Side;
        use alacritty_terminal::selection::{Selection, SelectionType};
        let mut t = self.term.lock();
        let Some((ty, range)) = t
            .selection
            .as_ref()
            .and_then(|s| Some((s.ty, s.to_range(&*t)?)))
        else {
            return false;
        };
        let at = t.vi_mode_cursor.point;
        // The anchor is whichever end the cursor is not on; for lines and blocks, the opposite
        // row (and, for a block, the opposite column) of the cursor.
        let other_line = if at.line == range.start.line {
            range.end.line
        } else {
            range.start.line
        };
        let other = match ty {
            SelectionType::Lines => Point::new(other_line, at.column),
            SelectionType::Block => {
                let col = if at.column == range.start.column {
                    range.end.column
                } else {
                    range.start.column
                };
                Point::new(other_line, col)
            }
            _ if at == range.start => range.end,
            _ => range.start,
        };
        let mut sel = Selection::new(ty, at, Side::Left);
        sel.update(other, Side::Left);
        sel.include_all();
        t.selection = Some(sel);
        t.vi_goto_point(other);
        drop(t);
        self.mark_dirty();
        true
    }

    /// `f`/`F`/`t`/`T`: jump to the next `ch` on the cursor's (wrapped) line, forward or
    /// back, landing ON it or one cell short (`till`). Returns whether it was found.
    pub fn copy_jump(&self, ch: char, forward: bool, till: bool, repeat: bool) -> bool {
        use alacritty_terminal::index::Boundary;
        let mut t = self.term.lock();
        let at = t.vi_mode_cursor.point;
        let needle = ch.to_string();
        // alacritty's inline search starts AFTER the given point, so the cursor itself is
        // passed. Only a REPEATED `t` (`;`) starts one cell further: it is sitting just short
        // of the last match and would otherwise find it again and never move.
        let skip = till && repeat;
        let hit = if forward {
            let from = if skip {
                at.add(&*t, Boundary::None, 1)
            } else {
                at
            };
            t.inline_search_right(from, &needle).ok().map(|p| {
                if till {
                    p.sub(&*t, Boundary::None, 1)
                } else {
                    p
                }
            })
        } else {
            let from = if skip {
                at.sub(&*t, Boundary::None, 1)
            } else {
                at
            };
            t.inline_search_left(from, &needle).ok().map(|p| {
                if till {
                    p.add(&*t, Boundary::None, 1)
                } else {
                    p
                }
            })
        };
        let Some(p) = hit else {
            return false;
        };
        t.vi_goto_point(p);
        drop(t);
        self.mark_dirty();
        true
    }

    /// The word under the copy cursor (vim's `*`), if it is on one.
    pub fn copy_word_at_cursor(&self) -> Option<String> {
        let t = self.term.lock();
        let at = t.vi_mode_cursor.point;
        let start = t.semantic_search_left(at);
        let end = t.semantic_search_right(at);
        let word = t.bounds_to_string(start, end);
        let word = word.trim();
        (!word.is_empty() && !word.contains(char::is_whitespace)).then(|| word.to_string())
    }

    /// The pane's user-given name, if it has one.
    pub fn name(&self) -> Option<String> {
        self.name.lock().ok().and_then(|n| n.clone())
    }

    /// Name the pane (`None` or blank clears). Control characters are dropped and the name is
    /// bounded, like a title: it is drawn into the chrome.
    pub fn set_name(&self, name: Option<&str>) {
        let clean = name
            .map(|n| {
                sanitize_title(n.trim())
                    .chars()
                    .take(MAX_PANE_NAME)
                    .collect::<String>()
            })
            .filter(|n| !n.is_empty());
        if let Ok(mut slot) = self.name.lock() {
            *slot = clean;
        }
        self.mark_dirty();
    }

    /// Highlight `query` on screen while copy mode shows it (`None` clears).
    pub fn copy_set_query(&self, query: Option<String>) {
        if let Ok(mut q) = self.copy_query.lock() {
            *q = query;
        }
        self.mark_dirty();
    }

    /// Whether a selection is up.
    pub fn copy_has_selection(&self) -> bool {
        self.term.lock().selection.is_some()
    }

    /// Clear the selection, keeping copy mode. Returns whether there was one.
    pub fn copy_clear_selection(&self) -> bool {
        let had = self.term.lock().selection.take().is_some();
        self.mark_dirty();
        had
    }

    /// Search for `query` (a literal, case-insensitive) from the vi cursor, older (`up`) or
    /// newer, wrapping at the ends of the history, and move the cursor to the match start.
    pub fn copy_search(&self, query: &str, up: bool) -> SearchOutcome {
        use alacritty_terminal::index::{Boundary, Direction, Side};
        use alacritty_terminal::term::search::RegexSearch;
        if query.is_empty() {
            return SearchOutcome::NoMatch;
        }
        let Ok(mut regex) = RegexSearch::new(&format!("(?i){}", regex_literal(query))) else {
            return SearchOutcome::NoMatch;
        };
        let mut t = self.term.lock();
        let at = t.vi_mode_cursor.point;
        // EXCLUSIVE start: one cell past the cursor in the search direction, so `n` moves on
        // from the match it is sitting on rather than finding it again.
        let (origin, dir) = if up {
            (at.sub(&*t, Boundary::None, 1), Direction::Left)
        } else {
            (at.add(&*t, Boundary::None, 1), Direction::Right)
        };
        let Some(m) = t.search_next(&mut regex, origin, dir, Side::Left, None) else {
            return SearchOutcome::NoMatch;
        };
        let start = *m.start();
        let wrapped = if up { start > at } else { start < at };
        t.vi_goto_point(start);
        drop(t);
        self.mark_dirty();
        if wrapped {
            SearchOutcome::Wrapped
        } else {
            SearchOutcome::Found
        }
    }

    /// The selected text (the cursor's line when nothing is selected), via alacritty's own
    /// extractor — wrapped-row seams and combining marks handled as alacritty handles them.
    /// Refused BEFORE extraction when it would exceed [`MAX_YANK_BYTES`]: the extractor appends
    /// every stored scalar with no budget, under the lock the PTY reader also needs.
    ///
    /// `expect_selection`: the caller believes a selection is up. If it is gone — cleared by a
    /// screen swap the PTY thread processed since the caller last looked — this refuses under
    /// the SAME lock it would extract with, instead of falling back to the cursor's line.
    pub fn copy_yank(&self, expect_selection: bool) -> Result<String, &'static str> {
        use alacritty_terminal::index::Side;
        use alacritty_terminal::selection::{Selection, SelectionType};
        let mut t = self.term.lock();
        if expect_selection && t.selection.is_none() {
            return Err("the screen changed and cleared the selection — nothing copied");
        }
        if t.selection.is_none() {
            let at = t.vi_mode_cursor.point;
            let mut sel = Selection::new(SelectionType::Lines, at, Side::Left);
            sel.include_all();
            t.selection = Some(sel);
        }
        let range = t
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&t))
            .ok_or("nothing selected")?;
        let mut bytes = 0usize;
        let cols = t.columns();
        let grid = t.grid();
        for line in range.start.line.0..=range.end.line.0 {
            for c in 0..cols {
                let p = Point::new(Line(line), Column(c));
                if !range.contains(p) {
                    continue;
                }
                let cell = &grid[p];
                bytes += cell.c.len_utf8() + 1; // +1: a separator/newline it may add
                if let Some(zw) = cell.zerowidth() {
                    bytes += zw.iter().map(|z| z.len_utf8()).sum::<usize>();
                }
                if bytes > MAX_YANK_BYTES {
                    return Err("selection too large to copy (over 1 MiB)");
                }
            }
        }
        let text = t.selection_to_string().ok_or("nothing selected")?;
        if text.len() > MAX_YANK_BYTES {
            return Err("selection too large to copy (over 1 MiB)");
        }
        Ok(text)
    }

    /// How many times this pane has rung the bell since it was spawned. Monotonic; the
    /// caller compares it against how many it has acknowledged.
    pub fn bell_count(&self) -> u64 {
        self.listener.bells.load(Ordering::Relaxed)
    }

    /// The pane's title (OSC 0/2), if it has set one. UNTRUSTED program output — see
    /// [`MuxListener::title`].
    pub fn title(&self) -> Option<String> {
        self.listener.title.lock().unwrap().clone()
    }

    /// Capture this pane's text for read-back (`comux capture-pane`).
    ///
    /// `want_rows` counts PHYSICAL grid rows ending at the live screen bottom, reaching
    /// back into scrollback; `None` means the visible screen's worth. See [`capture_rows`]
    /// for why this does not reuse the render snapshot and why it ignores the scroll
    /// position.
    ///
    /// The lock is scoped to the harvest so the concatenation — which touches every
    /// retained byte — cannot starve the PTY reader thread.
    pub fn capture(&self, want_rows: Option<usize>) -> Capture {
        let harvest = {
            let term = self.term.lock();
            let rows = want_rows.unwrap_or_else(|| term.screen_lines());
            capture_rows(&term, rows)
        };
        assemble_capture(harvest)
    }
}

/// Snapshot the visible viewport of any `Term` into renderer-ready [`Snapshot`]. Split
/// out of [`PaneTerm::snapshot`] so tests can drive a bare `Term` (via the VTE parser)
/// deterministically — no PTY, no shell, no timing. `pub(crate)` so the render-fidelity
/// harness in `tui.rs` can read a REFERENCE emulator's screen too.
#[cfg(test)]
pub(crate) fn snapshot_grid<L: EventListener>(term: &Term<L>) -> Snapshot {
    snapshot_with_search(term, None)
}

/// [`snapshot_grid`], highlighting copy mode's `query` (a literal) on screen.
pub(crate) fn snapshot_with_search<L: EventListener>(
    term: &Term<L>,
    query: Option<&str>,
) -> Snapshot {
    let cols = term.columns();
    let rows = term.screen_lines();
    let grid = term.grid();
    let display_offset = grid.display_offset() as i32;

    let mut cells = Vec::with_capacity(rows);
    let mut wrapped = Vec::with_capacity(rows);
    for r in 0..rows as i32 {
        let line = Line(r - display_offset);
        let mut row = Vec::with_capacity(cols);
        // Does the cell we just emitted occupy TWO columns? This single carry is what makes the
        // spacer marking and the emitted grapheme widths consistent BY CONSTRUCTION — see below.
        let mut prev_spans_two = false;
        for c in 0..cols {
            let cell = &grid[Point::new(line, Column(c))];
            let flags = cell.flags;

            // THE INVARIANT: a cell is a render spacer exactly when the cell before it occupies
            // two columns. Nothing else. The client's `fix_wide_spacers` rebuilds spacer
            // structure with literally this predicate ("is the previous symbol two columns
            // wide?"), because the wire delta cannot carry a `skip` cell — `Buffer::diff` drops
            // them. Deriving it here the same way is what stops the two from ever disagreeing.
            //
            // Reading alacritty's flags directly instead is what caused two distinct residue
            // bugs, because a `*_SPACER` flag does NOT imply a covering wide glyph:
            //
            //  * `LEADING_WIDE_CHAR_SPACER` is the filler alacritty leaves in the LAST column
            //    when a wide glyph did not fit and wrapped to the next row. Nothing precedes it
            //    and nothing covers it — alacritty's own renderer paints it as a blank.
            //  * `WIDE_CHAR_SPACER` can also appear with NO `WIDE_CHAR` cell before it (a ZWJ
            //    sequence collapsing into the preceding cell will do it).
            //
            // In both cases the server composed a `skip` cell, the delta therefore never shipped
            // that position, and the client — correctly seeing no wide glyph before it — cleared
            // `skip` and emitted whatever stale glyph and background the cell still held. The
            // server's baseline had already recorded the position as delivered, so no later delta
            // could correct it: a coloured blank or a leftover glyph frozen into a pane's edge
            // until something happened to overwrite it. Both were found by
            // `tui::app_render_fidelity` and are guarded by `a_spacer_always_follows_a_wide_cell`.
            let spacer = prev_spans_two;

            // How many columns does THIS cell occupy? The authoritative signal is that alacritty
            // RESERVED the next column for it, not the `WIDE_CHAR` flag on its own (see above).
            let span = if !spacer
                && flags.contains(Flags::WIDE_CHAR)
                && c + 1 < cols
                && grid[Point::new(line, Column(c + 1))]
                    .flags
                    .contains(Flags::WIDE_CHAR_SPACER)
            {
                2
            } else {
                1
            };

            // Grapheme = base char + any zero-width combining marks.
            let mut sym = String::new();
            sym.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                sym.extend(zw.iter());
            }
            // ROOT FIX for wide-glyph ghosts: the whole render pipeline downstream (ratatui's
            // `Buffer::diff` omission, the client's `fix_wide_spacers`, and ratatui's cell emit)
            // measures width via `unicode-width`, but alacritty lays the grid out with its OWN
            // width table. When they disagree on a grapheme — a VS16 emoji like "❤\u{fe0f}" that
            // alacritty gives 1 column but unicode-width calls 2, or a ZWJ sequence — the client
            // desyncs from the server's grid and a cell the diff wrongly deems unchanged is never
            // re-sent → residue. Force the emitted grapheme's unicode-width to equal the number of
            // columns alacritty allotted it, so every stage agrees. Width-inflating marks (VS/ZWJ)
            // are dropped to the base scalar; ordinary zero-width combining marks (accents) don't
            // change width and are kept. Together with `span` above this is what the invariant
            // rests on: a cell with `span == 2` always emits a symbol `unicode-width` calls 2.
            if spacer {
                // Never painted (the glyph before it covers this column), so its symbol is
                // unobservable — but it must still MEASURE one column, or the client would read
                // it as a wide cell and mark the NEXT position a spacer the server didn't. The
                // invariant has to hold in both directions.
                sym = " ".to_string();
            } else if UnicodeWidthStr::width(sym.as_str()) != span {
                sym = if UnicodeWidthChar::width(cell.c).unwrap_or(0) == span {
                    cell.c.to_string()
                } else {
                    " ".repeat(span) // unrepresentable at this width — blank the leading cell
                };
            }
            // Concealed text (`SGR 8`, a typed password) is blanked HERE rather than sent with
            // a hidden attribute: ratatui's crossterm emit has no `Hidden`, so the attribute
            // alone would put the secret on screen. Same width, so the invariant above holds.
            if flags.contains(Flags::HIDDEN) && !spacer {
                sym = " ".repeat(span);
            }
            row.push(CellSnap {
                sym,
                spacer,
                fg: ansi_to_cell(cell.fg),
                bg: ansi_to_cell(cell.bg),
                bold: flags.contains(Flags::BOLD),
                reverse: flags.contains(Flags::INVERSE),
                attrs: cell_attrs(flags),
            });
            prev_spans_two = span == 2;
        }
        // alacritty marks the LAST cell of a soft-wrapped row with `WRAPLINE` (the row
        // continues onto the next). Used by drag-copy to avoid a spurious newline at the seam.
        let last_wrapped = cols > 0
            && grid[Point::new(line, Column(cols - 1))]
                .flags
                .contains(Flags::WRAPLINE);
        wrapped.push(last_wrapped);
        cells.push(row);
    }

    let cursor_point = grid.cursor.point;
    let cursor_row = (cursor_point.line.0 + display_offset).clamp(0, rows as i32 - 1) as u16;
    let cursor_col = (cursor_point.column.0 as u16).min(cols.saturating_sub(1) as u16);

    Snapshot {
        cols: cols as u16,
        rows: rows as u16,
        cells,
        wrapped,
        cursor: (cursor_col, cursor_row),
        cursor_visible: term.mode().contains(TermMode::SHOW_CURSOR) && display_offset == 0,
        cursor_shape: decscusr(term.cursor_style()),
        copy: copy_view(term, display_offset, query),
    }
}

/// `history_limit` from mux.toml. Process-wide because panes are spawned on many paths
/// (splits, restore off the state lock, `spawn`), all of which read it here.
static HISTORY_LIMIT: AtomicUsize = AtomicUsize::new(crate::config::DEFAULT_HISTORY_LIMIT);

pub fn history_limit() -> usize {
    HISTORY_LIMIT.load(Ordering::Relaxed)
}

pub fn set_history_limit(lines: usize) {
    HISTORY_LIMIT.store(lines, Ordering::Relaxed);
}

/// The emulator configuration every pane runs with.
fn pane_config() -> Config {
    Config {
        scrolling_history: history_limit(),
        // Pin the parser-level OSC 52 policy to copy-only. This is alacritty's own
        // default today; pinning it means a future default change can never hand a pane
        // program the PASTE direction (reading the host clipboard) behind our back.
        osc52: alacritty_terminal::term::Osc52::OnlyCopy,
        // Answer `CSI ? u` and track `CSI > flags u`, so a program can ask for the kitty
        // keyboard protocol — Claude Code and nvim then get Shift+Enter, Ctrl+Enter, … as
        // distinct keys. The encoding side is `crate::keys`, fed by `key_modes`.
        kitty_keyboard: true,
        // See `decscusr`: the "never set" marker.
        default_cursor_style: CursorStyle {
            shape: CursorShape::HollowBlock,
            blinking: false,
        },
        ..Config::default()
    }
}

/// alacritty's cell flags → the ratatui modifiers for everything but bold and reverse.
/// Every underline style (curly, double, dotted, dashed) becomes a plain underline: ratatui
/// has no styles to carry them, and an underline in the wrong style beats none at all.
fn cell_attrs(flags: Flags) -> Modifier {
    let mut m = Modifier::empty();
    if flags.contains(Flags::DIM) {
        m |= Modifier::DIM;
    }
    if flags.contains(Flags::ITALIC) {
        m |= Modifier::ITALIC;
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        m |= Modifier::UNDERLINED;
    }
    if flags.contains(Flags::STRIKEOUT) {
        m |= Modifier::CROSSED_OUT;
    }
    if flags.contains(Flags::HIDDEN) {
        m |= Modifier::HIDDEN;
    }
    m
}

/// The cursor style as a `DECSCUSR` parameter. A pane's default style is `HollowBlock`
/// (set in `spawn_with_env`), which no `DECSCUSR` can select, so it reliably means "the
/// program never chose one" → `0`, the outer terminal's own default — forcing a block
/// on someone whose terminal draws a beam would be a regression nobody asked for.
fn decscusr(style: CursorStyle) -> u8 {
    let blink = |on: u8, off: u8| if style.blinking { on } else { off };
    match style.shape {
        CursorShape::Block => blink(1, 2),
        CursorShape::Underline => blink(3, 4),
        CursorShape::Beam => blink(5, 6),
        CursorShape::HollowBlock | CursorShape::Hidden => 0,
    }
}

/// Copy mode's cursor + selection in viewport rows, or `None` when vi mode is off.
fn copy_view<L: EventListener>(
    term: &Term<L>,
    display_offset: i32,
    query: Option<&str>,
) -> Option<CopyView> {
    if !term.mode().contains(TermMode::VI) {
        return None;
    }
    let rows = term.screen_lines() as i32;
    let vp = term.vi_mode_cursor.point;
    let vrow = vp.line.0 + display_offset;
    let cursor = (0..rows)
        .contains(&vrow)
        .then_some((vp.column.0 as u16, vrow as u16));
    let selection = term
        .selection
        .as_ref()
        .and_then(|s| s.to_range(term))
        .map(|r| {
            (
                (r.start.line.0 + display_offset, r.start.column.0 as u16),
                (r.end.line.0 + display_offset, r.end.column.0 as u16),
                r.is_block,
            )
        });
    let matches = query
        .map(|q| visible_matches(term, display_offset, q))
        .unwrap_or_default();
    Some(CopyView {
        cursor,
        selection,
        matches,
    })
}

/// The on-screen matches of a literal, case-insensitive `query`, in viewport rows.
fn visible_matches<L: EventListener>(
    term: &Term<L>,
    display_offset: i32,
    query: &str,
) -> Vec<(CopyCell, CopyCell)> {
    use alacritty_terminal::index::Direction;
    use alacritty_terminal::term::search::{RegexIter, RegexSearch};
    let rows = term.screen_lines() as i32;
    let cols = term.columns();
    if query.is_empty() || rows == 0 || cols == 0 {
        return Vec::new();
    }
    let Ok(mut regex) = RegexSearch::new(&format!("(?i){}", regex_literal(query))) else {
        return Vec::new();
    };
    let start = Point::new(Line(-display_offset), Column(0));
    let end = Point::new(Line(rows - 1 - display_offset), Column(cols - 1));
    RegexIter::new(start, end, Direction::Right, term, &mut regex)
        .take(MAX_HIGHLIGHTS)
        .map(|m| {
            let (a, b) = (*m.start(), *m.end());
            (
                (a.line.0 + display_offset, a.column.0 as u16),
                (b.line.0 + display_offset, b.column.0 as u16),
            )
        })
        .collect()
}

/// Server-owned traversal budgets for [`capture_text`]. These bound the WORK, not merely
/// the output: the walk runs on the single-writer main loop while holding the pane's
/// `term` lock, so a caller must not be able to make it traverse an unbounded grid.
///
/// Why three of them, none redundant:
/// - `CAPTURE_MAX_ROWS` — the same order as the configured scrollback, so `--lines`
///   cannot ask for more history than exists to begin with.
/// - `CAPTURE_MAX_BYTES` — same order as [`MAX_CLIPBOARD_BYTES`]; caps the retained text.
/// - `CAPTURE_MAX_CELLS` — neither of the other two bounds a history of BLANK rows: those
///   trim to nothing, so the byte budget never trips while the walk still visits
///   `rows × cols` cells. Zero-width combining marks are charged against it too, because
///   alacritty stores them in an UNCAPPED `Vec<char>` per cell — without that, a single
///   cell could blow the byte budget on its own and be copied in full before anyone noticed.
pub const CAPTURE_MAX_ROWS: usize = 10_000;
pub const CAPTURE_MAX_BYTES: usize = 1 << 20; // 1 MiB of retained text
pub const CAPTURE_MAX_CELLS: usize = 2_000_000;

/// The text of a pane's grid, newest-last. See [`capture_text`].
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    /// The captured text. Rows are joined with `\n` except at a soft-wrap seam; there is
    /// no trailing newline (matching the drag-copy extractor).
    pub text: String,
    /// Physical grid rows actually included.
    pub rows: usize,
    /// Older output exists that this capture could NOT include — a budget cut the walk
    /// short, or the request was clamped to [`CAPTURE_MAX_ROWS`]. Deliberately NOT set
    /// when the caller simply asked for fewer rows than exist and got exactly that:
    /// "I hit a limit" and "you are missing output" are different claims, and only the
    /// second is worth waking a caller up about.
    pub truncated: bool,
}

/// Rows harvested bottom-up by [`capture_rows`], before assembly. `wanted` is how many
/// rows the caller could legitimately have received, so the truncation verdict can be
/// formed without re-reading the grid (and therefore without re-taking the lock).
struct CaptureRows {
    /// `(row text, does this row soft-wrap into the one below it)`, BOTTOM-UP.
    rows: Vec<(String, bool)>,
    wanted: usize,
}

/// Harvest up to `want_rows` physical grid rows ending at the **live screen bottom**,
/// walking backward into scrollback history.
///
/// Deliberately NOT built on [`snapshot_grid`]: that path coerces each grapheme's
/// unicode-width to the column span alacritty allotted it, which drops VS16/ZWJ marks and
/// blanks unrepresentable graphemes (`❤️` → `❤`). That is the right trade for rendering —
/// it is the root fix for wide-glyph ghosting — and it is silent corruption for read-back.
/// This walker emits the stored scalars verbatim.
///
/// Anchored at the LIVE bottom rather than the displayed viewport, so a human scrolling in
/// copy-mode cannot change what a scripted reader sees.
///
/// `want_rows` counts PHYSICAL grid rows, not logical lines — a soft-wrapped logical line
/// spans several rows. In the alternate screen there is no history, so at most the alt
/// screen itself is available.
fn capture_rows<L: EventListener>(term: &Term<L>, want_rows: usize) -> CaptureRows {
    let cols = term.columns();
    let screen = term.screen_lines();
    let grid = term.grid();
    // Never index past the top of history: `Line(-n)` for n > history_size panics.
    let available = screen + grid.history_size();
    let wanted = want_rows.min(available);
    let walk = wanted.min(CAPTURE_MAX_ROWS);

    let mut rows: Vec<(String, bool)> = Vec::new();
    // Bytes already committed, charging each row the `\n` it may need (a history of blank
    // rows still costs one separator byte each — that is the whole byte budget otherwise).
    let mut retained = 0usize;
    let mut cells = 0usize;
    'walk: for k in 0..walk {
        let line = Line(screen as i32 - 1 - k as i32);
        let mut text = String::new();
        for c in 0..cols {
            cells += 1;
            if cells > CAPTURE_MAX_CELLS {
                break 'walk; // discard this incomplete row rather than emit a partial one
            }
            let cell = &grid[Point::new(line, Column(c))];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue; // wide-glyph right half: the grapheme lives on the leading cell
            }
            if retained + text.len() + cell.c.len_utf8() + 1 > CAPTURE_MAX_BYTES {
                break 'walk;
            }
            text.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                for ch in zw {
                    cells += 1;
                    if cells > CAPTURE_MAX_CELLS {
                        break 'walk;
                    }
                    if retained + text.len() + ch.len_utf8() + 1 > CAPTURE_MAX_BYTES {
                        break 'walk;
                    }
                    text.push(*ch);
                }
            }
        }
        // alacritty marks the LAST cell of a soft-wrapped row `WRAPLINE`: the row continues
        // onto the next one, so the seam must not become a newline.
        let wraps = cols > 0
            && grid[Point::new(line, Column(cols - 1))]
                .flags
                .contains(Flags::WRAPLINE);
        retained += text.len() + 1;
        rows.push((text, wraps));
    }
    CaptureRows { rows, wanted }
}

/// Join harvested rows top-down. Split from [`capture_rows`] so the concatenation — which
/// touches every retained byte — runs with the pane's `term` lock RELEASED, keeping the
/// PTY reader thread unblocked.
fn assemble_capture(harvest: CaptureRows) -> Capture {
    let CaptureRows { mut rows, wanted } = harvest;
    rows.reverse();
    let mut text = String::new();
    for (i, (row, wraps)) in rows.iter().enumerate() {
        let last = i + 1 == rows.len();
        if *wraps && !last {
            // Soft-wrap seam: the same logical line continues, so keep the trailing spaces
            // (trimming would fuse "word " + "next" into "wordnext") and emit no newline.
            text.push_str(row);
        } else {
            // Trim ONLY the ASCII space a terminal pads empty cells with. `trim_end()`
            // would also strip Unicode whitespace the program actually WROTE — a trailing
            // U+00A0 is content, not padding — and read-back has to be lossless or the
            // caller acts on text that never existed.
            text.push_str(row.trim_end_matches(' '));
            if !last {
                text.push('\n');
            }
        }
    }
    Capture {
        rows: rows.len(),
        truncated: rows.len() < wanted,
        text,
    }
}

/// Capture pane text from a bare `Term` — the composition of [`capture_rows`] and
/// [`assemble_capture`]. Used by tests (no PTY, no shell, deterministic); production goes
/// through [`PaneTerm::capture`], which scopes the lock to the harvest.
#[cfg(test)]
fn capture_text<L: EventListener>(term: &Term<L>, want_rows: usize) -> Capture {
    assemble_capture(capture_rows(term, want_rows))
}

/// How long a closed pane's shell gets to honour its `SIGHUP` before it is killed.
const REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Make sure `pid` is dead before the reaper thread drops the `Pty` that will `waitpid` for it.
///
/// Detaching the wait keeps the loop alive, but on its own it would only trade a wedged
/// server for a leak: a shell that ignores `SIGHUP` outlives its pane forever, holding the
/// PTY's descriptors and the reaper thread with it. So escalate — `SIGHUP` first (the same
/// signal `Pty::drop` will send, so a well-behaved shell still takes its normal exit path),
/// then `SIGKILL` once the grace is up.
///
/// Two things here are deliberate and easy to "fix" wrongly:
///
/// * **It must run on the thread that is about to do the wait, and before it.** An
///   un-`wait`ed child keeps its pid reserved — it turns into a zombie, not a free slot —
///   so no signal from here can land on an unrelated process that recycled the number. An
///   escalation racing the wait from a second thread would have no such guarantee. (For the
///   same reason we never `waitpid` here ourselves: `Pty::drop` sends its OWN `SIGHUP`
///   afterwards, which would then be aimed at a freed pid.)
/// * **There is no liveness poll.** For exactly the reason above, `kill(pid, 0)` succeeds
///   just as well for the zombie of a shell that already obeyed the `SIGHUP`, so polling it
///   would return "alive" every time and buy nothing. We sleep the grace out and then send a
///   `SIGKILL` that a zombie simply discards.
fn reap_child(pid: Option<u32>) {
    // pid 1 / 0 are not ours to signal; treat an unknown pid as nothing to do.
    let Some(pid) = pid.filter(|p| *p > 1).map(|p| p as libc::pid_t) else {
        return;
    };
    // SAFETY: plain signal syscalls against a child pid this thread is about to reap, so
    // the pid cannot have been recycled.
    unsafe {
        libc::kill(pid, libc::SIGHUP);
        std::thread::sleep(REAP_GRACE);
        libc::kill(pid, libc::SIGKILL);
    }
}

impl Drop for PaneTerm {
    fn drop(&mut self) {
        if let Some(fd) = self.fg_fd.take() {
            // SAFETY: our own dup'd fd, closed exactly once.
            unsafe { libc::close(fd) };
        }
        let _ = self.sender.send(Msg::Shutdown);
        // Everything past this point happens on a DETACHED thread, and teardown never blocks
        // the caller. It can't: joining the io-thread hands back alacritty's `EventLoop`,
        // which owns the `Pty`, and `Pty::drop` SIGHUPs the shell and then `waitpid`s for it
        // with NO timeout. A shell that doesn't die on that SIGHUP — a `zsh -l` still sourcing
        // its rc files loses that race often enough to hit on an ordinary close — would block
        // whoever dropped the pane, and on the server that is the single-writer main loop: no
        // frames, no control requests, flock held, fixable only with an external SIGKILL.
        if let Some(jh) = self.io_thread.take() {
            let pid = self.child_pid;
            std::thread::spawn(move || {
                // Escalate BEFORE the join, not after: killing the shell EOFs the PTY, which
                // is also what frees an io-thread that never acted on `Shutdown` — otherwise
                // the join below could hang and nothing would ever reap the child.
                reap_child(pid);
                drop(jh.join());
            });
        }
    }
}

/// The pure mode→wheel-bytes mapping behind [`PaneTerm::wheel_bytes`] (split out so the
/// encoding is unit-testable without a PTY). See that method for the tmux-style policy.
fn wheel_bytes_for_mode(mode: TermMode, up: bool, col: u16, row: u16) -> Option<Vec<u8>> {
    if reports_mouse(mode) {
        let button: u16 = if up { 64 } else { 65 };
        return Some(encode_mouse(mode, button, col, row, false));
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

/// Whether the app turned on any xterm mouse reporting (1000/1002/1003).
fn reports_mouse(mode: TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
}

/// The pure mode→click-bytes mapping behind [`PaneTerm::click_bytes`]: a left press +
/// release at `(col, row)` (1-based, pane-relative) in the app's negotiated encoding, or
/// `None` when the app isn't reporting the mouse.
fn click_bytes_for_mode(mode: TermMode, col: u16, row: u16) -> Option<Vec<u8>> {
    if !reports_mouse(mode) {
        return None;
    }
    let mut out = encode_mouse(mode, 0, col, row, false);
    out.extend(encode_mouse(mode, 0, col, row, true));
    Some(out)
}

/// One xterm mouse report. SGR keeps the button on release and marks it with a final `m`;
/// the older encodings have no release marker and report button 3 instead.
fn encode_mouse(mode: TermMode, button: u16, col: u16, row: u16, release: bool) -> Vec<u8> {
    if mode.contains(TermMode::SGR_MOUSE) {
        let fin = if release { 'm' } else { 'M' };
        return format!("\x1b[<{button};{col};{row}{fin}").into_bytes();
    }
    let button = if release { 3 } else { button };
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
        return out;
    }
    // Legacy `ESC [ M Cb Cx Cy`: each value offset by 32, clamped to the single-byte
    // range (coords past 223 can't be expressed and are pinned there, as in xterm).
    let enc = |v: u16| (v.saturating_add(32)).min(255) as u8;
    vec![0x1b, b'[', b'M', enc(button), enc(col), enc(row)]
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
mod token_tests {
    use super::next_pane_token;

    /// Tokens must be unique within a run AND carry a per-incarnation prefix, so a token
    /// minted by a PREVIOUS server (a toast that outlived a restart) cannot resolve here.
    #[test]
    fn tokens_are_unique_and_incarnation_qualified() {
        let a = next_pane_token();
        let b = next_pane_token();
        assert_ne!(a, b);
        let (na, ia) = a.rsplit_once('-').expect("token is <nonce>-<counter>");
        let (nb, ib) = b.rsplit_once('-').expect("token is <nonce>-<counter>");
        assert_eq!(na, nb, "same server incarnation shares the nonce");
        assert!(!na.is_empty(), "nonce must not be empty");
        assert_ne!(ia, ib);
        assert!(ia.parse::<u64>().is_ok() && ib.parse::<u64>().is_ok());
    }
}

#[cfg(test)]
mod clipboard {
    //! OSC 52 capture policy on the pane listener (tmux `set-clipboard`). No PTY needed —
    //! the listener is fed the same `Event`s alacritty's parser would emit.
    use super::*;

    fn store(kind: ClipboardType, text: &str) -> Option<String> {
        let l = MuxListener::new();
        l.send_event(Event::ClipboardStore(kind, text.to_string()));
        let taken = l.clipboard.lock().unwrap().take();
        taken.map(|(_, t)| t)
    }

    #[test]
    fn captures_a_clipboard_write() {
        assert_eq!(
            store(ClipboardType::Clipboard, "yanked"),
            Some("yanked".into())
        );
    }

    #[test]
    fn captures_an_empty_write_as_a_clear() {
        // OSC 52 with no payload means "clear the clipboard" — a real request, not a no-op.
        assert_eq!(store(ClipboardType::Clipboard, ""), Some(String::new()));
    }

    #[test]
    fn sequences_start_above_the_not_yet_relayed_sentinel() {
        // Regression: with a 0-based dispenser the FIRST clipboard write in the server's life
        // compared equal to the drain's `last_clipboard_seq = 0` and was silently dropped —
        // the passthrough relayed nothing at all until a second write happened.
        assert!(next_clipboard_seq() > 0);
    }

    #[test]
    fn writes_are_sequenced_across_panes() {
        // The drain picks the max sequence, because pane iteration order is arbitrary.
        let (a, b) = (MuxListener::new(), MuxListener::new());
        a.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "old".into(),
        ));
        b.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "new".into(),
        ));
        let sa = a.clipboard.lock().unwrap().take().unwrap();
        let sb = b.clipboard.lock().unwrap().take().unwrap();
        assert!(sb.0 > sa.0, "later write must carry the higher sequence");
    }

    #[test]
    fn ignores_the_primary_selection() {
        // No PRIMARY on macOS and no notion of it in comux — must NOT alias onto Cmd-V.
        assert_eq!(store(ClipboardType::Selection, "yanked"), None);
    }

    #[test]
    fn drops_an_oversize_payload_whole() {
        let big = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        // Dropped, not truncated: pasting a silently-cut command is worse than pasting nothing.
        assert_eq!(store(ClipboardType::Clipboard, &big), None);
        assert!(store(ClipboardType::Clipboard, &"x".repeat(MAX_CLIPBOARD_BYTES)).is_some());
    }

    #[test]
    fn latest_write_wins() {
        let l = MuxListener::new();
        l.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "first".into(),
        ));
        l.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "second".into(),
        ));
        let taken = l.clipboard.lock().unwrap().take();
        assert_eq!(taken.map(|(_, t)| t), Some("second".into()));
    }

    #[test]
    fn a_clipboard_write_does_not_dirty_the_pane() {
        let l = MuxListener::new();
        l.dirty.store(false, Ordering::Relaxed);
        l.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "yanked".into(),
        ));
        // Nothing on screen changed — dirtying here would cost a frame per yank.
        assert!(!l.dirty.load(Ordering::Relaxed));
    }

    #[test]
    fn a_clipboard_read_is_never_answered() {
        // Replying would hand the host's clipboard to any program in a pane.
        let l = MuxListener::new();
        l.dirty.store(false, Ordering::Relaxed);
        l.send_event(Event::ClipboardLoad(
            ClipboardType::Clipboard,
            Arc::new(|s: &str| s.to_string()),
        ));
        assert!(l.clipboard.lock().unwrap().is_none());
        assert!(!l.dirty.load(Ordering::Relaxed));
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
            // Through the real wire type, NOT `set_style`: a `Style` read back off a cell has an
            // empty `sub_modifier`, so `set_style` only ever ADDS modifier bits and would make
            // BOLD/DIM/REVERSED stick forever — a harness bug that flatters the pipeline.
            let wire = crate::proto::WireCell::of(*x, *y, cell);
            if let Some(bc) = client.cell_mut(Position::new(wire.x, wire.y)) {
                wire.apply_to(bc);
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

    /// Rewrite an emitted byte stream so the reference terminal measures one glyph DIFFERENTLY
    /// than the emitter did: every `U+E0B0` (a Powerline separator — Private Use Area, one column
    /// by `unicode-width`) becomes `가`, which every terminal agrees is two.
    ///
    /// This models the one failure comux cannot see from the inside. A Nerd Font icon's width
    /// lives in the OUTER terminal's font, not in any table we can read, so the fidelity fuzz
    /// cannot catch it — its reference emulator shares alacritty's width table and therefore
    /// agrees with us by construction. Substituting in the byte stream makes the reference
    /// advance two columns where the emitter assumed one, which is exactly what the owner sees.
    fn widen_sentinel(bytes: &[u8]) -> Vec<u8> {
        String::from_utf8_lossy(bytes)
            .replace('\u{e0b0}', "가")
            .into_bytes()
    }

    /// Paint `cur` over `prev` through the PRODUCTION emitter and report what a width-disagreeing
    /// terminal ends up showing.
    fn paint_into_widening_terminal(
        t: &mut Term<VoidListener>,
        p: &mut Processor,
        prev: Option<&Buffer>,
        cur: &Buffer,
        cols: usize,
        rows: usize,
    ) {
        let sink = SharedBytes(Rc::new(RefCell::new(Vec::new())));
        let area = Rect::new(0, 0, cols as u16, rows as u16);
        let mut cterm = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .unwrap();
        crate::client::emit_view(&mut cterm, prev, cur, None).unwrap();
        let bytes = std::mem::take(&mut *sink.0.borrow_mut());
        feed(t, p, &widen_sentinel(&bytes));
    }

    #[test]
    fn a_glyph_the_terminal_measures_wider_does_not_skew_its_row_or_jump_on_repaint() {
        // THE reported symptom, reproduced: "ls나 git status를 하면 글자가 떴다가 확 오른쪽으로
        // 움직여". Output lands, then the self-heal repaint snaps it sideways.
        //
        // The cause is a glyph the outer terminal draws wider than `unicode-width` says. Before
        // the emitter re-anchored, a row went out as ONE run positioned entirely by the
        // terminal's own cursor advance, so that one glyph pushed everything after it right —
        // and the repaint, which did anchor, put it back. Two renderers, one visible jump.
        //
        // Asserted here: the cells after the wide-measured glyph land on their true columns, and
        // a full repaint over the result changes NOTHING. Convergence is the property; the glyph
        // itself is collateral (printing its neighbour overwrites its second half, which
        // alacritty answers by blanking the pair) and that is the bounded damage by design.
        let (cols, rows) = (8usize, 1usize);
        let (mut t, mut p) = term(cols, rows);
        let area = Rect::new(0, 0, cols as u16, rows as u16);

        let mut frame = Buffer::empty(area);
        frame.set_string(0, 0, "ab\u{e0b0}cdef", ratatui::style::Style::default());
        let mut cur = Buffer::empty(area);
        crate::client::blit_view(&frame, &mut cur);

        // One ordinary delta from a blank screen.
        let empty = Buffer::empty(area);
        paint_into_widening_terminal(&mut t, &mut p, Some(&empty), &cur, cols, rows);
        let after_delta = snap_text(&snapshot_grid(&t));

        // Everything after the mismeasured glyph is on its TRUE COLUMN — no skew.
        //
        // Asserted per column off the grid, never off `snap_text`: that collapses a wide glyph's
        // spacer to an empty string, so a row shifted one column right still reads `ab가cdef`
        // and the skew — the whole bug — hides inside the text.
        let cells = &snapshot_grid(&t).cells[0];
        let at = |x: usize| -> String {
            if cells[x].spacer {
                "<spacer>".to_string()
            } else {
                cells[x].sym.clone()
            }
        };
        let row: Vec<String> = (0..cols).map(at).collect();
        assert_eq!(at(0), "a", "row: {row:?}");
        assert_eq!(at(1), "b", "row: {row:?}");
        for (i, want) in ["c", "d", "e", "f"].iter().enumerate() {
            assert_eq!(
                at(3 + i),
                *want,
                "column {} must still hold {want:?}; anything else is the reported skew: {row:?}",
                3 + i
            );
        }

        // Now the self-heal repaint, over the same content. It must change NOTHING — that
        // equality is the absence of the jump.
        paint_into_widening_terminal(&mut t, &mut p, None, &cur, cols, rows);
        let after_repaint = snap_text(&snapshot_grid(&t));
        assert_eq!(
            after_repaint, after_delta,
            "a repaint moved the screen — that movement IS what the owner sees"
        );
    }

    /// The heart of it: after feeding a SEQUENCE of byte-batches (each = one server
    /// render tick), the client's on-screen text must equal the final snapshot's text."""
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

    /// Production-accurate relay (mirrors `server::push_frames` + `client::run_attached`).
    ///
    /// The difference from [`assert_relay_fidelity`]: that harness's `deliver` re-derives the
    /// wire delta from the client's ALREADY-normalized buffer (`client.clone().diff(frame)`),
    /// which silently models the server as diffing against the client's post-`fix_wide_spacers`
    /// buffer. Production does NOT: `push_frames` keeps `c.last = <raw composed buffer>` and diffs
    /// the next raw compose against THAT. This harness keeps that independent RAW baseline, so it
    /// exposes any ghost caused by the server's baseline disagreeing with the client's normalized
    /// buffer (the trailing-spacer width-oracle asymmetry).
    fn assert_relay_fidelity_prod(
        cols: usize,
        rows: usize,
        batches: &[&[u8]],
        refresh_at: &[usize],
    ) {
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
        let mut server_last = Buffer::empty(area); // the server's `c.last` (RAW composed buffer)
        let mut client = Buffer::empty(area);
        for (i, batch) in batches.iter().enumerate() {
            feed(&mut src_t, &mut src_p, batch);
            let snap = snapshot_grid(&src_t);
            let server = compose(&snap); // RAW composed, exactly like `App::render_to`
            let full = i == 0 || refresh_at.contains(&i);
            if full {
                server_last = Buffer::empty(area);
                client = Buffer::empty(area);
            }
            // `push_frames`: diff the raw compose against the raw baseline, ship changed cells.
            let changed = server_last.diff(&server);
            for (x, y, cell) in &changed {
                // See `roundtrip`: the wire type, never `set_style`.
                let wire = crate::proto::WireCell::of(*x, *y, cell);
                if let Some(bc) = client.cell_mut(Position::new(wire.x, wire.y)) {
                    wire.apply_to(bc);
                }
            }
            server_last = server.clone(); // advance the RAW baseline (production semantics)
            // `run_attached`: normalize spacers, then emit through a real ratatui terminal.
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
        assert_eq!(
            cells_norm(&ref_snap),
            cells_norm(&src_snap),
            "\nPROD-accurate relay diverged from the SOURCE screen\n got:  {:?}\n want: {:?}",
            snap_text(&ref_snap),
            snap_text(&src_snap),
        );
    }

    #[test]
    fn a_spacer_always_follows_a_wide_cell() {
        // THE invariant the whole wide-glyph path rests on, asserted directly: a cell is marked
        // a spacer exactly when the cell before it emits a two-column symbol. The client's
        // `fix_wide_spacers` rebuilds spacer structure from that same predicate and CANNOT check
        // it — the wire delta never carries a `skip` cell — so if the two ever disagree the
        // client paints a stale cell the server believes it delivered, which no later delta can
        // correct. Both directions matter: a spacer with no wide cell before it (alacritty's
        // line-end filler, and a `WIDE_CHAR_SPACER` orphaned by a ZWJ sequence), and a wide
        // symbol whose following cell is NOT marked.
        //
        // Widths 8..11 × three paddings put a wide glyph at both column parities and straddling
        // the right edge, which is where alacritty leaves the line-end filler.
        for cols in [8usize, 9, 10, 11] {
            for pad in 0..3 {
                let (mut t, mut p) = term(cols, 4);
                let mut input = "x".repeat(pad);
                input.push_str(&"가".repeat(cols));
                // A ZWJ sequence and a VS16 emoji too: both make alacritty's flags and
                // unicode-width disagree, which is how the orphaned spacer arises.
                input.push_str("👨\u{200d}👩\u{200d}👧❤\u{fe0f}가");
                feed(&mut t, &mut p, input.as_bytes());
                let snap = snapshot_grid(&t);
                for (y, row) in snap.cells.iter().enumerate() {
                    for (x, cell) in row.iter().enumerate() {
                        let covered = x > 0
                            && !row[x - 1].spacer
                            && UnicodeWidthStr::width(row[x - 1].sym.as_str()) == 2;
                        assert_eq!(
                            cell.spacer,
                            covered,
                            "cols={cols} pad={pad} cell ({x},{y}): spacer={} but the cell before \
                             it is {:?} (covered={covered}). Row: {:?}",
                            cell.spacer,
                            x.checked_sub(1)
                                .and_then(|i| row.get(i))
                                .map(|c| (&c.sym, c.spacer)),
                            row.iter().map(|c| (&c.sym, c.spacer)).collect::<Vec<_>>(),
                        );
                        // And every non-spacer symbol measures 1 or 2 columns — never 0, which
                        // would desync the client's left-to-right walk.
                        if !cell.spacer {
                            let w = UnicodeWidthStr::width(cell.sym.as_str());
                            assert!(
                                w == 1 || w == 2,
                                "cols={cols} pad={pad} ({x},{y}) width {w} for {:?}",
                                cell.sym
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_line_end_wide_char_filler_is_a_blank_not_a_spacer() {
        // The concrete case behind the invariant above: a wide glyph that does not fit in the
        // last column makes alacritty leave a `LEADING_WIDE_CHAR_SPACER` filler there and wrap
        // the glyph to the next row. Nothing covers that filler, so it must be composed as an
        // ordinary blank — marking it `skip` froze whatever the cell previously held into the
        // pane's last column, because a `skip` cell is never shipped.
        let (mut t, mut p) = term(5, 3);
        // Odd offset so the 5th column can only hold half of a wide glyph.
        feed(&mut t, &mut p, "ab가가".as_bytes());
        let snap = snapshot_grid(&t);
        let row0 = &snap.cells[0];
        assert_eq!((&row0[0].sym, row0[0].spacer), (&"a".to_string(), false));
        assert_eq!((&row0[1].sym, row0[1].spacer), (&"b".to_string(), false));
        assert_eq!((&row0[2].sym, row0[2].spacer), (&"가".to_string(), false));
        assert!(
            row0[3].spacer,
            "col 3 is the wide glyph's real trailing half"
        );
        assert!(
            !row0[4].spacer,
            "col 4 is the line-end filler — a blank, NOT a spacer: {:?}",
            row0.iter().map(|c| (&c.sym, c.spacer)).collect::<Vec<_>>()
        );
        // And the glyph that did not fit wrapped to the next row.
        assert_eq!(&snap.cells[1][0].sym, "가");
    }

    #[test]
    fn snapshot_coerces_grapheme_width_to_alacritty_span() {
        // The root fix: a grapheme whose unicode-width disagrees with alacritty's column span is
        // coerced so the two agree (VS16 ❤️ → ❤, width 1). CJK is untouched (both models say 2).
        let (mut t, mut p) = term(10, 1);
        feed(&mut t, &mut p, "❤\u{fe0f}가X".as_bytes());
        let row = &snapshot_grid(&t).cells[0];
        // ❤️ occupies 1 alacritty column → the VS16 is dropped so the sym is the width-1 base.
        assert_eq!(row[0].sym, "❤");
        assert!(!row[0].spacer);
        // 가 is genuinely wide (2 cols) → unchanged, with a trailing spacer at col 2.
        assert_eq!(row[1].sym, "가");
        assert!(row[2].spacer);
        // X sits at col 3 (right after 가's spacer), proving ❤️ consumed exactly ONE column.
        assert_eq!(row[3].sym, "X");
    }

    #[test]
    fn prod_relay_passes_on_normal_content() {
        // Sanity: the production-accurate harness (independent raw baseline) relays ordinary
        // wide-CJK + colored content faithfully — same script as the claude-code-like session.
        assert_relay_fidelity_prod(
            24,
            6,
            &[
                "\x1b[?1049h\x1b[2J\x1b[H".as_bytes(),
                "\x1b[H┌─ 세션 ─────────────┐".as_bytes(),
                "\x1b[2;1H│ \x1b[32mready\x1b[0m            │".as_bytes(),
                "\x1b[3;1H│ 작업 중…            │".as_bytes(),
                "\x1b[4;1H└────────────────────┘".as_bytes(),
                "\x1b[2;4H\x1b[31mBUSY \x1b[0m".as_bytes(),
                "\x1b[3;4H가나다라마".as_bytes(),
                "\x1b[3;4Habcde".as_bytes(),
                "\x1b[2;2H\x1b[K│".as_bytes(),
                "\x1b[5;1H\x1b[38;5;39m▓▓▓▓▓▓\x1b[0m spinner".as_bytes(),
                "\x1b[5;1H\x1b[2Kdone".as_bytes(),
            ],
            &[],
        );
    }

    #[test]
    fn prod_relay_wide_char_desync_is_fixed() {
        // A pure-delta emoji/CJK churn: mixes VS16 ❤️ (alacritty width 1, unicode-width 2) with
        // 가 (both 2) and X. This USED to diverge through the production-accurate baseline — a 가
        // that replaced an ❤️ was never re-sent, leaving a stale ❤️ ghost (the root of the user's
        // "sometimes ghost"). Now `snapshot_grid` coerces each grapheme's unicode-width to match
        // alacritty's column span (❤️ → ❤, width 1), so every pipeline stage agrees and the relay
        // stays faithful with deltas alone — no forced repaint needed. This test now PASSES.
        let batches: Vec<Vec<u8>> = (0..20)
            .map(|i| {
                let mut s = String::from("\x1b[H");
                for row in 1..=3 {
                    let n = (i + row) % 7;
                    s.push_str(&format!("\x1b[{row};1H\x1b[2K"));
                    for c in 0..5 {
                        if (c + n) % 3 == 0 {
                            s.push('❤');
                            s.push('\u{fe0f}');
                        } else if (c + n) % 3 == 1 {
                            s.push('가');
                        } else {
                            s.push('X');
                        }
                    }
                    s.push_str(&format!(" r{row}"));
                }
                s.into_bytes()
            })
            .collect();
        let refs: Vec<&[u8]> = batches.iter().map(|b| b.as_slice()).collect();
        assert_relay_fidelity_prod(20, 4, &refs, &[]);
    }

    #[test]
    fn full_frame_on_alt_exit_clears_wide_char_residue() {
        // The shipped fix's MECHANISM: a full repaint forced at the alt-screen EXIT wipes any
        // wide-char desync residue accumulated during the app's life, so the restored primary
        // (shell) screen is clean. Ticks 0-2 accumulate VS16/emoji desync on the alt screen;
        // tick 3 leaves the alt screen (`?1049l`) and `refresh_at: [3]` models the server forcing
        // `needs_full` on THAT exit transition (the production edge); tick 4 draws the ASCII shell
        // prompt. The restored primary screen (width-agreement content) must match the source
        // exactly — no residue. (Without the forced full — see `prod_relay_known_wide_char_desync`
        // — the stale alt-screen cells the incremental diff wrongly deems unchanged would linger.)
        assert_relay_fidelity_prod(
            20,
            4,
            &[
                "\x1b[?1049h\x1b[2J\x1b[H❤\u{fe0f}가X❤\u{fe0f}가 alt".as_bytes(), // 0: enter + emoji churn
                "\x1b[2;1H가❤\u{fe0f}X가❤\u{fe0f} row2".as_bytes(), // 1: more desync-prone content
                "\x1b[3;1H❤\u{fe0f}❤\u{fe0f}❤\u{fe0f} spin".as_bytes(), // 2: dense VS16
                "\x1b[?1049l".as_bytes(),                           // 3: EXIT alt screen
                "\x1b[H$ ls -la\r\ntotal 0".as_bytes(),             // 4: ASCII shell prompt
            ],
            &[3], // server forces a full repaint ON the alt-screen exit tick (the production edge)
        );
    }

    #[test]
    fn alt_screen_bit_tracks_enter_exit() {
        // The signal the fix polls: `?1049h` sets `ALT_SCREEN`, `?1049l` clears it. This is what
        // `PaneTerm::in_alt_screen` reads and the render loop watches for the true->false edge.
        let (mut t, mut p) = term(10, 3);
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        feed(&mut t, &mut p, b"\x1b[?1049h");
        assert!(t.mode().contains(TermMode::ALT_SCREEN));
        feed(&mut t, &mut p, b"\x1b[?1049l");
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
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
            // See `roundtrip`: the wire type, never `set_style`.
            let wire = crate::proto::WireCell::of(x, y, cell);
            if let Some(bc) = client.cell_mut(Position::new(wire.x, wire.y)) {
                wire.apply_to(bc);
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
    fn click_sgr_sends_press_and_release() {
        let m = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            click_bytes_for_mode(m, 7, 1).unwrap(),
            b"\x1b[<0;7;1M\x1b[<0;7;1m"
        );
    }

    #[test]
    fn click_legacy_releases_with_button_3() {
        let m = TermMode::MOUSE_DRAG;
        assert_eq!(
            click_bytes_for_mode(m, 1, 2).unwrap(),
            vec![0x1b, b'[', b'M', 32, 33, 34, 0x1b, b'[', b'M', 35, 33, 34]
        );
    }

    #[test]
    fn click_without_mouse_reporting_is_not_forwarded() {
        // A shell, or an alt-screen pager with only alternate-scroll: comux keeps the click.
        assert!(click_bytes_for_mode(TermMode::empty(), 1, 1).is_none());
        let pager = TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN;
        assert!(click_bytes_for_mode(pager, 1, 1).is_none());
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

#[cfg(test)]
mod capture_tests {
    //! Read-back (`comux capture-pane`) driven against a bare `Term` through the VTE
    //! parser — no PTY, no shell, deterministic.
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    fn term(cols: usize, rows: usize) -> (Term<VoidListener>, Processor) {
        let size = TermSize::new(cols, rows);
        (
            Term::new(Config::default(), &size, VoidListener),
            Processor::new(),
        )
    }

    fn feed(t: &mut Term<VoidListener>, p: &mut Processor, s: &str) {
        p.advance(t, s.as_bytes());
    }

    #[test]
    fn captures_the_visible_screen() {
        let (mut t, mut p) = term(20, 4);
        feed(&mut t, &mut p, "alpha\r\nbeta\r\ngamma");
        let cap = capture_text(&t, 4);
        assert_eq!(cap.text, "alpha\nbeta\ngamma\n");
        assert_eq!(cap.rows, 4);
        // Everything available was returned, so this is NOT truncation.
        assert!(!cap.truncated);
    }

    #[test]
    fn reaches_back_into_scrollback() {
        let (mut t, mut p) = term(20, 3);
        // 6 lines through a 3-row screen: the first three scroll into history.
        for i in 1..=6 {
            feed(&mut t, &mut p, &format!("line{i}\r\n"));
        }
        // The visible screen alone cannot see line1.
        assert!(!capture_text(&t, 3).text.contains("line1"));
        let deep = capture_text(&t, 9);
        assert!(
            deep.text.contains("line1") && deep.text.contains("line6"),
            "scrollback capture missed rows: {:?}",
            deep.text
        );
        // Order is oldest-first, newest-last.
        let first = deep.text.find("line1").unwrap();
        let last = deep.text.find("line6").unwrap();
        assert!(first < last, "capture is not in display order");
    }

    #[test]
    fn clamps_past_the_top_of_history() {
        // Indexing `Line(-n)` past `history_size` panics in alacritty, so an over-long
        // request must clamp rather than reach.
        let (mut t, mut p) = term(20, 3);
        feed(&mut t, &mut p, "only\r\n");
        let cap = capture_text(&t, 50_000);
        assert!(cap.text.contains("only"));
        // Nothing older exists, so asking for more is not "missing output".
        assert!(!cap.truncated);
    }

    #[test]
    fn zero_rows_is_empty_not_a_panic() {
        // The server refuses `--lines 0` at the boundary; the walker must still be safe,
        // because a clamp/index slip here is an out-of-bounds panic on the main loop.
        let (mut t, mut p) = term(20, 3);
        feed(&mut t, &mut p, "x");
        let cap = capture_text(&t, 0);
        assert_eq!(cap.text, "");
        assert_eq!(cap.rows, 0);
        assert!(!cap.truncated);
    }

    #[test]
    fn joins_a_softwrap_seam_without_a_newline() {
        // 6 columns: "abcdefgh" soft-wraps, and the seam is the SAME logical line.
        let (mut t, mut p) = term(6, 3);
        feed(&mut t, &mut p, "abcdefgh");
        let cap = capture_text(&t, 3);
        assert!(
            cap.text.starts_with("abcdefgh"),
            "soft-wrap seam became a line break: {:?}",
            cap.text
        );
    }

    #[test]
    fn a_hard_line_break_stays_a_newline() {
        // Capture counts rows UP from the live bottom, so a 3-row screen needs all 3 to
        // reach "one" — asking for 2 legitimately starts at "two".
        let (mut t, mut p) = term(20, 3);
        feed(&mut t, &mut p, "one\r\ntwo");
        assert!(capture_text(&t, 3).text.starts_with("one\ntwo"));
        assert!(capture_text(&t, 2).text.starts_with("two"));
    }

    #[test]
    fn a_wide_glyph_is_captured_once() {
        // The trailing spacer cell must be skipped, or every CJK glyph doubles.
        let (mut t, mut p) = term(10, 1);
        feed(&mut t, &mut p, "가나");
        assert_eq!(capture_text(&t, 1).text, "가나");
    }

    #[test]
    fn preserves_vs16_that_the_render_snapshot_drops() {
        // THE regression test for the reason this walker exists. `snapshot_grid` coerces a
        // grapheme's unicode-width to alacritty's column span, which turns ❤️ into ❤ (see
        // `snapshot_coerces_grapheme_width_to_alacritty_span`). That is right for rendering
        // and silent corruption for read-back, so capture must NOT go through it.
        let (mut t, mut p) = term(10, 1);
        feed(&mut t, &mut p, "❤\u{fe0f}");
        let captured = capture_text(&t, 1).text;
        assert_eq!(captured, "❤\u{fe0f}", "capture lost the VS16 selector");
        assert_ne!(
            captured,
            snapshot_grid(&t).cells[0][0].sym,
            "capture must not reproduce the render path's lossy coercion"
        );
    }

    #[test]
    fn keeps_unicode_whitespace_the_program_wrote() {
        // Read-back must be lossless, which makes the trailing-trim rule load-bearing. A
        // trailing U+00A0 is CONTENT, not padding: `trim_end()` (Unicode White_Space) would
        // delete it and still report `truncated: false` — a lie the caller cannot detect.
        // Only the ASCII space the grid pads empty cells with is safe to strip.
        let (mut t, mut p) = term(10, 1);
        feed(&mut t, &mut p, "x\u{00A0}");
        assert_eq!(capture_text(&t, 1).text, "x\u{00A0}");
    }

    #[test]
    fn trailing_whitespace_is_trimmed_per_row() {
        let (mut t, mut p) = term(20, 2);
        feed(&mut t, &mut p, "padded   \r\n");
        assert!(capture_text(&t, 2).text.starts_with("padded\n"));
    }

    #[test]
    fn row_budget_keeps_the_newest_and_flags_truncation() {
        // alacritty's default history is 10_000 rows, so a full screen plus full history
        // exceeds CAPTURE_MAX_ROWS and the clamp becomes real missing output.
        let (mut t, mut p) = term(20, 5);
        for i in 0..10_200 {
            feed(&mut t, &mut p, &format!("r{i}\r\n"));
        }
        let available = 5 + t.grid().history_size();
        assert!(
            available > CAPTURE_MAX_ROWS,
            "test needs more history than the row cap ({available})"
        );
        let cap = capture_text(&t, available);
        assert_eq!(cap.rows, CAPTURE_MAX_ROWS);
        assert!(cap.truncated, "a clamped request IS missing output");
        // The newest rows are the ones kept.
        assert!(cap.text.contains("r10199"));
    }

    #[test]
    fn byte_budget_keeps_the_newest_and_flags_truncation() {
        // Wide rows so the byte budget trips before the row budget does.
        let (mut t, mut p) = term(200, 5);
        let wide = "x".repeat(199);
        for i in 0..8_000 {
            feed(&mut t, &mut p, &format!("{i:06}{wide}\r\n"));
        }
        let cap = capture_text(&t, 20_000);
        assert!(cap.truncated, "1 MiB of text should have tripped the cap");
        assert!(
            cap.text.len() <= CAPTURE_MAX_BYTES,
            "retained {} bytes, cap is {}",
            cap.text.len(),
            CAPTURE_MAX_BYTES
        );
        // Newest suffix, cut on a row boundary — the last line must be the last written.
        let last = cap.text.lines().next_back().unwrap_or_default();
        assert!(
            last.starts_with("007999"),
            "byte budget dropped the NEWEST rows instead of the oldest: {last:.20}"
        );
    }

    #[test]
    fn is_unaffected_by_the_scroll_position() {
        // Capture anchors at the LIVE screen bottom, so a human scrolling in copy-mode
        // cannot change what a scripted reader sees.
        let (mut t, mut p) = term(20, 3);
        for i in 1..=9 {
            feed(&mut t, &mut p, &format!("row{i}\r\n"));
        }
        let live = capture_text(&t, 3).text;
        t.scroll_display(Scroll::Delta(4));
        assert!(
            t.grid().display_offset() > 0,
            "test did not actually scroll"
        );
        assert_eq!(capture_text(&t, 3).text, live);
    }

    #[test]
    fn alt_screen_has_no_history_to_reach_into() {
        // Documented contract: in the alternate screen a deep request returns at most the
        // alt screen itself — and must not index past it.
        let (mut t, mut p) = term(20, 3);
        for i in 1..=9 {
            feed(&mut t, &mut p, &format!("scroll{i}\r\n"));
        }
        feed(&mut t, &mut p, "\x1b[?1049h"); // enter alt screen
        feed(&mut t, &mut p, "alt-only");
        let cap = capture_text(&t, 500);
        assert!(cap.text.contains("alt-only"));
        assert!(
            !cap.text.contains("scroll1"),
            "alt-screen capture leaked the primary screen's scrollback: {:?}",
            cap.text
        );
    }
}

#[cfg(test)]
mod bell_title_tests {
    //! The two pane events comux used to throw away, driven through the VTE parser against a
    //! bare `Term` — no PTY, no shell.
    use super::*;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    fn term() -> (Term<MuxListener>, Processor, MuxListener) {
        let l = MuxListener::new();
        let size = TermSize::new(20, 4);
        (
            Term::new(Config::default(), &size, l.clone()),
            Processor::new(),
            l,
        )
    }

    fn feed(t: &mut Term<MuxListener>, p: &mut Processor, s: &str) {
        p.advance(t, s.as_bytes());
    }

    #[test]
    fn a_bel_is_counted_not_flagged() {
        // A COUNT is what removes the ordering hazard between the socket thread clearing a
        // flag and the render loop re-reading a still-pending one. Three bells must be three.
        let (mut t, mut p, l) = term();
        assert_eq!(l.bells.load(Ordering::Relaxed), 0);
        feed(&mut t, &mut p, "\x07\x07\x07");
        assert_eq!(l.bells.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_bell_after_an_ack_is_a_new_bell() {
        // The ack model: `unacknowledged = count > seen`. Acknowledging cannot swallow a
        // bell that arrives afterwards, which a boolean pair could.
        let (mut t, mut p, l) = term();
        feed(&mut t, &mut p, "\x07");
        let seen = l.bells.load(Ordering::Relaxed);
        assert!(seen > 0);
        assert!(l.bells.load(Ordering::Relaxed) <= seen, "nothing pending");
        feed(&mut t, &mut p, "\x07");
        assert!(
            l.bells.load(Ordering::Relaxed) > seen,
            "a bell after the ack must raise the count again"
        );
    }

    #[test]
    fn osc_0_and_osc_2_both_set_the_title() {
        for seq in ["\x1b]0;hello\x07", "\x1b]2;hello\x07"] {
            let (mut t, mut p, l) = term();
            feed(&mut t, &mut p, seq);
            assert_eq!(
                l.title.lock().unwrap().as_deref(),
                Some("hello"),
                "sequence {seq:?}"
            );
        }
    }

    #[test]
    fn control_characters_are_stripped_from_a_title() {
        // A title is rendered into the chrome. Left raw, it could move the cursor or recolour
        // the status bar from inside a pane.
        assert_eq!(sanitize_title("a\x1b[31mb\x07c\n"), "a[31mbc");
        assert!(!sanitize_title("x\x1by").contains('\x1b'));
    }

    #[test]
    fn a_title_is_bounded_and_cut_on_a_char_boundary() {
        let long = "가".repeat(1000);
        let out = sanitize_title(&long);
        assert!(out.len() <= MAX_TITLE_BYTES, "{} bytes", out.len());
        // Cutting mid-scalar would make the stored string invalid to slice later.
        assert!(out.chars().all(|c| c == '가'));
    }

    #[test]
    fn an_empty_title_is_stored_not_ignored() {
        // `printf '\033]0;\007'` is a deliberate "clear my title", not a no-op.
        let (mut t, mut p, l) = term();
        feed(&mut t, &mut p, "\x1b]0;x\x07");
        feed(&mut t, &mut p, "\x1b]0;\x07");
        assert_eq!(l.title.lock().unwrap().as_deref(), Some(""));
    }
}

/// Capability probe for the desktop-notification OSC sequences (`OSC 9`, `OSC 777`, `OSC 99`).
///
/// comux does NOT capture these, and this module exists to say so with evidence rather than in
/// a comment, and to notice the day that changes.
///
/// **Why not.** The bytes reach `vte::ansi::Processor`, which owns the OSC dispatch table. In
/// vte 0.15 that table handles `0`, `2`, `4`, `8`, `10`–`12`, `22`, `50`, `52`, `104`, `110`–
/// `112`; everything else — including all three notification codes — falls to its private
/// `unhandled()` and is logged at debug level and dropped. There is no `Handler` method for an
/// unrecognised OSC, so `Term` never learns about it and neither does our `EventListener`.
///
/// **What capturing them would cost.** Only two options, both structural:
/// 1. Fork or patch `vte` to add an unknown-OSC hook. Owning a fork of the parser under the
///    terminal we do not otherwise touch is a large, permanent maintenance surface.
/// 2. Run our own PTY read loop so the bytes can be tee'd into a second `vte::Parser` before
///    alacritty's sees them. `alacritty_terminal::tty::{EventedPty, EventedReadWrite}` are both
///    public, but `EventedReadWrite::reader()` hands back `&mut Self::Reader` — a wrapper would
///    have to OWN the inner reader, and `tty::Pty` exposes no way to take it. So this means
///    reimplementing the tty module (openpty, child spawn, poll registration, race-free exit),
///    which is exactly the "own read loop" the competitive analysis predicted.
///
/// Neither is worth it for a feature comux already covers by a better route: `comux notify
/// --pane` is a PUSH that carries the pane identity, and a pushed notification is jumpable
/// (#102, #115) where an OSC one would not be — an OSC 9 says only "something happened", and
/// the pane it came from would still have to be inferred.
///
/// These tests pin the current behaviour in both directions. If a future vte routes any of
/// these codes to a `Handler` method, the "nothing observable happens" assertions here are what
/// will notice.
#[cfg(test)]
mod osc_notification_probe {
    use super::*;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    /// Every desktop-notification OSC in the wild, as a program would emit it.
    const SEQUENCES: &[(&str, &str)] = &[
        // iTerm2 / ConEmu growl notification.
        ("OSC 9", "\x1b]9;build finished\x07"),
        // rxvt-unicode extension, also supported by wezterm/foot: title + body.
        ("OSC 777", "\x1b]777;notify;comux;build finished\x07"),
        // kitty's desktop notification protocol (chunked; this is a whole one-shot message).
        ("OSC 99", "\x1b]99;i=1:d=0;build finished\x1b\\"),
        // String-terminator forms, in case the bell terminator is what is being ignored.
        ("OSC 9 (ST)", "\x1b]9;build finished\x1b\\"),
        ("OSC 777 (ST)", "\x1b]777;notify;comux;build finished\x1b\\"),
    ];

    fn term() -> (Term<MuxListener>, Processor, MuxListener) {
        let l = MuxListener::new();
        let size = TermSize::new(40, 4);
        (
            Term::new(Config::default(), &size, l.clone()),
            Processor::new(),
            l,
        )
    }

    #[test]
    fn a_notification_osc_is_swallowed_whole() {
        for (name, seq) in SEQUENCES {
            let (mut t, mut p, l) = term();
            p.advance(&mut t, seq.as_bytes());

            // Nothing is drawn. This is the half that matters for `capture-pane` and the
            // status heuristics: the sequence must not leak into the grid as text either.
            let text: String = t
                .grid()
                .display_iter()
                .map(|c| c.c)
                .collect::<String>()
                .replace(' ', "");
            assert!(
                text.is_empty(),
                "{name} leaked into the grid as {text:?} — the parser did not consume it"
            );

            // And nothing reaches us. `set_title` is checked explicitly because OSC 9 differs
            // from OSC 0/2 by one character, and a parser that lumped them together would
            // silently rename the pane on every notification.
            assert_eq!(
                l.title.lock().unwrap().clone(),
                None,
                "{name} was mistaken for a title"
            );
            assert_eq!(
                l.bells.load(std::sync::atomic::Ordering::Relaxed),
                0,
                "{name} rang the bell"
            );
        }
    }

    #[test]
    fn the_bell_terminator_of_a_notification_is_not_a_bell() {
        // `\x07` ends an OSC string; it must be consumed as the terminator, not delivered as
        // BEL. If it were, every notification would light the `!` marker (#108) — a
        // plausible-looking wrong behaviour, since the marker WOULD appear.
        let (mut t, mut p, l) = term();
        let bells = || l.bells.load(std::sync::atomic::Ordering::Relaxed);
        p.advance(&mut t, b"\x1b]9;hello\x07");
        assert_eq!(bells(), 0);
        // A real BEL still counts, so the assertion above is not vacuous.
        p.advance(&mut t, b"\x07");
        assert_eq!(bells(), 1);
    }

    #[test]
    fn osc_0_still_sets_the_title_beside_them() {
        // Guards the probe itself: if the listener stopped observing titles, every assertion
        // above would pass for the wrong reason.
        let (mut t, mut p, l) = term();
        p.advance(&mut t, b"\x1b]0;real title\x07");
        assert_eq!(l.title.lock().unwrap().as_deref(), Some("real title"));
    }
}

/// The bytes a PASTE of `text` sends, the way a terminal pastes: wrapped in `ESC[200~` /
/// `ESC[201~` when the program enabled bracketed paste, so a multi-line paste is taken as
/// text instead of being executed line by line.
///
/// Any paste delimiter already inside `text` is removed first — in both the 7-bit (`ESC [`)
/// and the 8-bit C1 (`U+009B`) CSI spellings, repeatedly, since removing one can splice two
/// halves into another — or the text could close the bracket early and run what follows.
/// Line breaks become `\r`, which is what Enter sends.
///
/// Without bracketed paste there is nothing to wrap in, and every line break in the text
/// still submits a line: that is what a paste into such a program does too.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    const MARKERS: [&str; 4] = ["\x1b[200~", "\x1b[201~", "\u{9b}200~", "\u{9b}201~"];
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    // One pass, as a stack: after each char, drop a marker the output now ENDS with. Removing
    // one exposes what came before it, and the chars still to come can complete another with
    // it — exactly the splice a repeated `replace` would need a pass per layer for (quadratic
    // on a crafted 1 MiB paste, and this runs on the server's single-writer loop).
    let mut clean = String::with_capacity(normalized.len());
    for ch in normalized.chars() {
        clean.push(ch);
        if ch == '~'
            && let Some(m) = MARKERS.iter().find(|m| clean.ends_with(*m))
        {
            clean.truncate(clean.len() - m.len());
        }
    }
    if !bracketed {
        return clean.into_bytes();
    }
    let mut out = Vec::with_capacity(clean.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(clean.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

#[cfg(test)]
mod paste_tests {
    use super::paste_bytes;

    #[test]
    fn a_bracketed_paste_is_wrapped_and_lines_become_cr() {
        assert_eq!(paste_bytes("a\nb\r\nc", true), b"\x1b[200~a\rb\rc\x1b[201~");
    }

    #[test]
    fn nested_markers_are_stripped_in_linear_time() {
        let n = 200_000;
        let evil = format!("{}{}", "\x1b[201".repeat(n), "~".repeat(n));
        let start = std::time::Instant::now();
        let out = paste_bytes(&evil, false);
        assert!(out.is_empty(), "every layer is a marker");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn without_bracketed_paste_the_text_goes_as_is() {
        assert_eq!(paste_bytes("ls\n", false), b"ls\r");
    }

    #[test]
    fn embedded_delimiters_cannot_close_the_bracket() {
        for evil in [
            "x\x1b[201~rm -rf ~\n",
            "x\u{9b}201~rm -rf ~\n",
            "x\x1b[20\x1b[201~1~rm -rf ~\n",
            "x\x1b[200~y",
            "x\x1b[2\x1b[20\x1b[201~1~0~1~z",
        ] {
            let out = paste_bytes(evil, true);
            let body = &out[6..out.len() - 6];
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(
                !body.contains("\x1b[201~")
                    && !body.contains("\u{9b}201~")
                    && !body.contains("\x1b[200~"),
                "{evil:?} -> {body:?}"
            );
        }
    }
}

#[cfg(test)]
mod attrs_and_cursor {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::Processor;

    fn snap_of(bytes: &[u8]) -> Snapshot {
        let size = TermSize::new(20, 3);
        let mut t = Term::new(pane_config(), &size, VoidListener);
        let mut p: Processor = Processor::new();
        p.advance(&mut t, bytes);
        snapshot_grid(&t)
    }

    #[test]
    fn every_sgr_attribute_survives_into_the_snapshot() {
        let s = snap_of(b"\x1b[3mI\x1b[0m\x1b[4mU\x1b[0m\x1b[4:3mC\x1b[0m\x1b[2mD\x1b[0m\x1b[9mS\x1b[0m\x1b[8mH\x1b[0mN");
        let a = |i: usize| s.cells[0][i].attrs;
        assert_eq!(a(0), Modifier::ITALIC);
        assert_eq!(a(1), Modifier::UNDERLINED);
        assert_eq!(
            a(2),
            Modifier::UNDERLINED,
            "a curly underline is still an underline"
        );
        assert_eq!(a(3), Modifier::DIM);
        assert_eq!(a(4), Modifier::CROSSED_OUT);
        assert_eq!(a(5), Modifier::HIDDEN);
        assert_eq!(
            s.cells[0][5].sym, " ",
            "a hidden password is never sent at all"
        );
        assert_eq!(a(6), Modifier::empty());
    }

    #[test]
    fn the_cursor_carries_visibility_and_shape() {
        let s = snap_of(b"hi");
        assert!(s.cursor_visible);
        assert_eq!(
            s.cursor_shape, 0,
            "never set = the outer terminal's default"
        );
        let s = snap_of(b"\x1b[?25l");
        assert!(!s.cursor_visible);
        assert_eq!(
            snap_of(b"\x1b[6 q").cursor_shape,
            6,
            "vim's insert-mode bar"
        );
        assert_eq!(snap_of(b"\x1b[1 q").cursor_shape, 1);
        assert_eq!(snap_of(b"\x1b[4 q").cursor_shape, 4);
        assert_eq!(
            snap_of(b"\x1b[6 q\x1b[0 q").cursor_shape,
            0,
            "reset to default"
        );
    }
}
