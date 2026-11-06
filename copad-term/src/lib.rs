//! C-ABI bridge wrapping `alacritty_terminal::Term` and its PTY event loop. Host-agnostic;
//! `copad-macos` is the current consumer. ABI contract: `docs/macos-renderer-migration-plan.md`
//! section D3.
//!
//! Pointer ownership:
//!
//! - `*mut CopadHandle` / `*mut CopadSnapshot` are Rust heap allocations; free with the matching
//!   `_destroy` exactly once (NULL is a no-op).
//! - Borrowed `*const CopadRun` / `*const u8` from snapshot accessors are valid until
//!   `copad_snapshot_destroy`.
//! - Static strings (`copad_term_version`) live for the program; no free.
//!
//! Threading: `Arc<FairMutex<Term>>` is shared between alacritty's PTY reader thread and
//! snapshot callers. Snapshots lock briefly, copy out the visible rows, then release.

use copad_graphics as graphics;
use std::collections::HashMap;
use std::ffi::{CStr, c_char};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Side;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::ClipboardType;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options as TtyOptions, Pty, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Rgb};

/// `#[repr(C)]` for a stable FFI layout. Cells reference the row's contiguous utf8 buffer via
/// `utf8_offset` + `utf8_len` to avoid per-cell allocation.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CopadRun {
    pub start_col: u16,
    pub end_col: u16,
    pub utf8_offset: u32,
    pub utf8_len: u32,
    pub fg_rgba: u32,
    pub bg_rgba: u32, // sentinel 0 = default-bg
    pub flags: u16,
    pub underline_style: u8,
    pub reserved: u8,
    pub underline_color_rgba: u32,
    /// 1-based index into the snapshot's `hyperlinks` (0 = no OSC 8 link); resolve via
    /// `copad_snapshot_hyperlink_uri`.
    pub hyperlink_id: u32,
}

pub mod flags {
    pub const BOLD: u16 = 1 << 0;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const INVERSE: u16 = 1 << 3;
    pub const DIM: u16 = 1 << 4;
    pub const STRIKE: u16 = 1 << 5;
    pub const BLINK: u16 = 1 << 6;
    pub const WIDE_LEADING: u16 = 1 << 7;
    pub const WIDE_TRAILING: u16 = 1 << 8;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CopadCursor {
    pub row: u16,
    pub col: u16,
    pub style: u8, // 0=hidden 1=block 2=bar 3=underline
    pub blink: u8,
    pub _reserved: u16,
}

/// Active selection bounds from `copad_snapshot_selection`. end_row and end_col are INCLUSIVE
/// (alacritty's `SelectionRange`); the renderer must honor that or the last selected cell goes
/// unhighlighted. When `present == 0` the other fields are meaningless. `is_block` is 1 for
/// `SelectionType::Block` (not wired in v1).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CopadSelectionRange {
    pub start_row: u16,
    pub start_col: u16,
    pub end_row: u16,
    pub end_col: u16,
    pub is_block: u8,
    pub present: u8,
    pub _reserved: u16,
}

/// Current find match in viewport coordinates, like `CopadSelectionRange` minus `is_block`.
/// `present == 0` means cleared or the last `search_next` found nothing. Multi-row matches
/// (pattern crossing an autowrap) carry both endpoints.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CopadSearchRange {
    pub start_row: u16,
    pub start_col: u16,
    pub end_row: u16,
    pub end_col: u16,
    pub present: u8,
    pub _reserved: [u8; 3],
}

/// Heap-allocated UTF-8 buffer handed to the caller (selection, scrollback). Free with
/// `copad_string_destroy` exactly once; the paired type avoids the ptr+len-without-capacity UB
/// of raw `Vec<u8>` round-tripping.
pub struct CopadString {
    data: Box<[u8]>,
}

/// `EventListener` for `alacritty_terminal::Term`. Captures only what the renderer needs (OSC 52
/// writes, PTY replies, child exit); the rest is dropped on purpose.
#[derive(Clone)]
struct CopadListener {
    /// Single slot, last write wins; the renderer polls every vsync.
    pending_clipboard: Arc<std::sync::Mutex<Option<String>>>,
    /// Sender into the PTY write path, for `Event::PtyWrite` replies (DSR, DA, colour queries).
    /// `None` until `copad_term_create` injects it after `EventLoop::channel()`; earlier events
    /// are dropped, which is fine since no startup query can be pending.
    sender: Arc<std::sync::Mutex<Option<EventLoopSender>>>,
    /// Palette for OSC 4 and OSC 10/11/12 queries, indexed by `vte::ansi::NamedColor as usize`:
    /// 0-15 ANSI, 16-255 cube, 256 fg, 257 bg, 258 cursor. Unpopulated entries
    /// (`copad_term_set_palette_entry`) make `ColorRequest` a no-op rather than answering with a
    /// stale alacritty default.
    palette: Arc<std::sync::Mutex<HashMap<usize, Rgb>>>,
    /// One-shot latch set on `Event::ChildExit`. Polled by `copad_term_take_child_exit` so the
    /// renderer can broadcast `panel.exited`; clear-on-take.
    child_exited: Arc<std::sync::atomic::AtomicBool>,
    /// Image switch + cell pixel size, shared with the kitty graphics handler (decisions #189).
    graphics: Arc<graphics::Shared>,
    /// Grid size in cells for `CSI 14 t` / `CSI 16 t` replies (alacritty gives only a formatter).
    grid_size: Arc<Mutex<(u16, u16)>>,
    /// OSC 0/2 since the last `copad_term_take_title`: `Some(None)` is a reset. Last wins.
    pending_title: Arc<std::sync::Mutex<Option<Option<String>>>>,
    /// BEL since the last `copad_term_take_bell`.
    bell: Arc<std::sync::atomic::AtomicBool>,
    /// Set once on `Event::ChildExit` and never cleared, unlike `child_exited`: after it the pid
    /// may be reaped and reused, so `copad_term_child_pid` stops reporting it.
    child_gone: Arc<std::sync::atomic::AtomicBool>,
}

/// Catppuccin Mocha defaults, same as `copad_core::theme::Theme::default()`, duplicated as
/// `(index, r, g, b)` to keep copad-core out of this crate. Pre-seeds the palette so an OSC 4
/// query in the window between `copad_term_create` returning and the host calling
/// `copad_term_set_palette_entry` still gets an answer. Host overrides win.
#[rustfmt::skip]
const DEFAULT_PALETTE_ROWS: &[(usize, u8, u8, u8)] = &[
    (0,  0x45, 0x47, 0x5a),
    (1,  0xf3, 0x8b, 0xa8),
    (2,  0xa6, 0xe3, 0xa1),
    (3,  0xf9, 0xe2, 0xaf),
    (4,  0x89, 0xb4, 0xfa),
    (5,  0xf5, 0xc2, 0xe7),
    (6,  0x94, 0xe2, 0xd5),
    (7,  0xba, 0xc2, 0xde),
    (8,  0x58, 0x5b, 0x70),
    (9,  0xf3, 0x8b, 0xa8),
    (10, 0xa6, 0xe3, 0xa1),
    (11, 0xf9, 0xe2, 0xaf),
    (12, 0x89, 0xb4, 0xfa),
    (13, 0xf5, 0xc2, 0xe7),
    (14, 0x94, 0xe2, 0xd5),
    (15, 0xa6, 0xad, 0xc8),
    (256, 0xcd, 0xd6, 0xf4),
    (257, 0x1e, 0x1e, 0x2e),
    (258, 0x89, 0xb4, 0xfa),
];

impl CopadListener {
    fn new() -> Self {
        let mut palette = HashMap::with_capacity(DEFAULT_PALETTE_ROWS.len());
        for &(idx, r, g, b) in DEFAULT_PALETTE_ROWS {
            palette.insert(idx, Rgb { r, g, b });
        }
        Self {
            pending_clipboard: Arc::new(std::sync::Mutex::new(None)),
            sender: Arc::new(std::sync::Mutex::new(None)),
            palette: Arc::new(std::sync::Mutex::new(palette)),
            child_exited: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            graphics: Arc::new(graphics::Shared::default()),
            grid_size: Arc::new(Mutex::new((1, 1))),
            pending_title: Arc::new(std::sync::Mutex::new(None)),
            bell: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            child_gone: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// The PTY window size: the grid in cells plus the cell size the host reported (1x1 until
    /// it has), which is what `TIOCGWINSZ` hands programs sizing images.
    fn window_size(&self) -> WindowSize {
        let (num_cols, num_lines) = *self.grid_size.lock().unwrap();
        let w = self.graphics.cell_width.load(Ordering::Relaxed);
        let h = self.graphics.cell_height.load(Ordering::Relaxed);
        WindowSize {
            num_lines,
            num_cols,
            cell_width: w.clamp(1, u16::MAX as u32) as u16,
            cell_height: h.clamp(1, u16::MAX as u32) as u16,
        }
    }

    fn set_sender(&self, sender: EventLoopSender) {
        *self.sender.lock().unwrap() = Some(sender);
    }

    /// Writes bytes back to the child for `Event::PtyWrite` + `Event::ColorRequest`. No-op until
    /// the sender is injected (no query can arrive before `copad_term_create` returns).
    fn send_to_pty(&self, bytes: Vec<u8>) {
        if let Some(sender) = self.sender.lock().unwrap().as_ref() {
            let _ = sender.send(Msg::Input(bytes.into()));
        }
    }
}

impl EventListener for CopadListener {
    fn send_event(&self, event: Event) {
        match event {
            // CLIPBOARD (OSC 52 `c`) only. PRIMARY (`p`) is a low-consequence X11 buffer with no
            // macOS equivalent; aliasing it to NSPasteboard would let a program clobber the real
            // clipboard through a selector meaning something else. Same call as comux's `term.rs`.
            Event::ClipboardStore(ClipboardType::Selection, _) => {}
            Event::ClipboardStore(ClipboardType::Clipboard, text) => {
                // Last write wins; the renderer takes it via `copad_term_take_clipboard_request`.
                *self.pending_clipboard.lock().unwrap() = Some(text);
            }
            Event::PtyWrite(reply) => {
                // Already formatted by alacritty (DSR 6n, DA 0c, ...); just forward. Without it
                // nvim logs "Did not detect DSR response from terminal" and takes slower paths.
                self.send_to_pty(reply.into_bytes());
            }
            Event::ColorRequest(index, format_reply) => {
                // OSC 4/10/11/12: apps ask what color an index renders as (e.g. nvim's
                // `&background=dark`). Resolve against the host palette. No reply on a miss:
                // a stale alacritty default would be wrong, and apps treat silence as "use my
                // default".
                let rgb = self.palette.lock().unwrap().get(&index).copied();
                if let Some(rgb) = rgb {
                    let reply = format_reply(rgb);
                    self.send_to_pty(reply.into_bytes());
                }
            }
            Event::ChildExit(_status) => {
                // Latch the flag; the renderer polls it and broadcasts `panel.exited`
                // (`copad-core::context` clears per-panel cwd / active state on it). A latch is
                // fine: ChildExit fires at most once per Term.
                self.child_gone.store(true, Ordering::Relaxed);
                self.child_exited.store(true, Ordering::Relaxed);
            }
            Event::Title(title) => *self.pending_title.lock().unwrap() = Some(Some(title)),
            Event::ResetTitle => *self.pending_title.lock().unwrap() = Some(None),
            Event::Bell => self.bell.store(true, Ordering::Relaxed),
            // `CSI 14 t` / `CSI 16 t`: image tools size output with these. Only answered once
            // the host reported a cell size; a 1x1 answer would be worse than none.
            Event::TextAreaSizeRequest(format_reply)
                if self.graphics.cell_width.load(Ordering::Relaxed) != 0 =>
            {
                self.send_to_pty(format_reply(self.window_size()).into_bytes());
            }
            _ => {
                // MouseCursorDirty / CursorBlinkingChange / Wakeup / Exit: intentionally dropped.
            }
        }
    }
}

struct Row {
    utf8: Vec<u8>,
    runs: Vec<CopadRun>,
}

pub struct CopadHandle {
    /// Shared with alacritty's reader thread; keep lock duration short on the snapshot path so
    /// the reader isn't starved.
    term: Arc<FairMutex<Term<CopadListener>>>,
    listener: CopadListener,
    sender: EventLoopSender,
    /// Owns the PTY + parser loop; joined in `copad_term_destroy` after `Msg::Shutdown`.
    io_thread: Option<JoinHandle<(EventLoop<Pty, CopadListener>, State)>>,
    /// Child PID (typically the login shell), captured at construction so `copad_term_child_cwd`
    /// can query the kernel without the Pty handle. Stale after exit: `proc_pidinfo` errors and
    /// the FFI returns NULL.
    child_pid: i32,
    /// Hash of the cursor row's renderable content plus cursor style/blink/show, for
    /// `copad_term_take_damage`. Catches changes the line-bounds filter drops: cursor-cell
    /// mutation collapsing to `(line, col, col)` damage, zero-width combining marks (Term::input
    /// skips `damage_point`), and DECSCUSR / `\e[?25l/h` transitions with no grid damage.
    last_redraw_state_hash: AtomicU64,
    /// Last `Grid::display_offset()`. Alacritty damage only fires for live-region writes, but a
    /// scroll changes what every row displays; a changed offset promotes to Full damage.
    last_display_offset: AtomicI32,
    /// Last selection range in viewport coordinates. Damage callers need old-union-new so a
    /// selection shrink or clear repaints rows highlighted last frame.
    last_selection_range: Mutex<CopadSelectionRange>,
    /// In-terminal find state. `pattern`/`case_sensitive` cache the compiled regex so
    /// back-to-back next/prev reuse the `RegexSearch` (building four lazy DFAs is costly).
    /// All fields share one Mutex so the snapshot path reads them atomically.
    search_state: Mutex<SearchState>,
    /// The settings that live in alacritty's `Config`; kept together because `set_options`
    /// takes the WHOLE config and one setter must not reset another.
    options: Mutex<TermOptions>,
    /// kitty virtual placements, which Unicode placeholder cells show (decisions #194).
    virtuals: graphics::Virtuals,
}

/// The per-pane settings the GUI can change after creation.
#[derive(Clone, Copy)]
struct TermOptions {
    kitty_keyboard: bool,
    scrollback: usize,
}

impl Default for TermOptions {
    fn default() -> Self {
        Self {
            kitty_keyboard: true,
            scrollback: Config::default().scrolling_history,
        }
    }
}

impl TermOptions {
    fn config(self) -> Config {
        Config {
            kitty_keyboard: self.kitty_keyboard,
            scrolling_history: self.scrollback,
            ..Config::default()
        }
    }
}

/// The most scrollback a pane may keep; `[terminal] scrollback` is clamped to it.
pub const MAX_SCROLLBACK: usize = 100_000;

fn apply_options(h: &CopadHandle, change: impl FnOnce(&mut TermOptions)) {
    let config = {
        let Ok(mut o) = h.options.lock() else {
            return;
        };
        change(&mut o);
        o.config()
    };
    h.term.lock().set_options(config);
}

/// Per-handle search bookkeeping. `regex` matches `pattern` under `case_sensitive`; rebuilt only
/// when the query changes.
#[derive(Default)]
struct SearchState {
    regex: Option<alacritty_terminal::term::search::RegexSearch>,
    pattern: String,
    case_sensitive: bool,
    /// In alacritty grid coordinates (NOT viewport); mapped at snapshot time so scrolling into
    /// history repositions the highlight.
    current_match: Option<std::ops::RangeInclusive<Point>>,
}

pub struct CopadSnapshot {
    cols: u16,
    rows: Vec<Row>,
    cursor: CopadCursor,
    selection: CopadSelectionRange,
    /// Find match in viewport coordinates; `present == 0` when none or scrolled out of view.
    search_match: CopadSearchRange,
    /// OSC 8 URIs visible in this snapshot; a run's `hyperlink_id` is a 1-based index (0 = none).
    /// Deduped by alacritty's `Hyperlink::id`.
    hyperlinks: Vec<String>,
    /// Visible image runs in draw order, and the images they index (held so the pixel pointers
    /// handed out stay valid until the snapshot is destroyed).
    graphics: Vec<graphics::CopadGraphic>,
    graphic_images: Vec<Arc<alacritty_terminal::term::cell::GraphicImage>>,
}

/// One image a snapshot's runs draw from. `pixels` is premultiplied RGBA8, `width * height * 4`
/// bytes, borrowed from the snapshot. `key` is unique per decoded image for the process: cache
/// GPU uploads by it.
#[repr(C)]
pub struct CopadGraphicImage {
    pub key: u64,
    pub width: u32,
    pub height: u32,
    pub pixels: *const u8,
}

/// Spawns a PTY running `shell` (or the user's `$SHELL`) under an `alacritty_terminal::Term`
/// driven by an `EventLoop` thread. Returns NULL on shell-spawn failure.
/// # Safety
///
/// `shell` and `cwd` may be NULL or point to valid C strings. They
/// are copied; caller retains ownership.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_create(
    cols: u16,
    rows: u16,
    shell: *const c_char,
    cwd: *const c_char,
    panel_id: *const c_char,
    socket_path: *const c_char,
) -> *mut CopadHandle {
    unsafe { create_with(cols, rows, shell, cwd, panel_id, socket_path, &[], &[]) }
}

/// `copad_term_create` with a per-child environment for embedders: `set` holds `KEY=VALUE`
/// pairs applied after copad's defaults (so a host's `TERM_PROGRAM` wins), `remove` keys the
/// child must not inherit. No `COPAD_PANEL_ID` / `COPAD_SOCKET` are stamped. Entries that are
/// not UTF-8, or `set` entries without `=`, are skipped.
///
/// # Safety
///
/// `shell`/`cwd` as for `copad_term_create`; `set` and `remove` point to `set_count` /
/// `remove_count` valid C strings (or are NULL with a count of 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_create_env(
    cols: u16,
    rows: u16,
    shell: *const c_char,
    cwd: *const c_char,
    set: *const *const c_char,
    set_count: usize,
    remove: *const *const c_char,
    remove_count: usize,
) -> *mut CopadHandle {
    let set: Vec<(String, String)> = unsafe { c_strings(set, set_count) }
        .into_iter()
        .filter_map(|pair| {
            pair.split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect();
    let remove = unsafe { c_strings(remove, remove_count) };
    unsafe {
        create_with(
            cols,
            rows,
            shell,
            cwd,
            ptr::null(),
            ptr::null(),
            &set,
            &remove,
        )
    }
}

/// # Safety
///
/// `list` points to `count` valid C strings, or `count` is 0.
unsafe fn c_strings(list: *const *const c_char, count: usize) -> Vec<String> {
    if list.is_null() || count == 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(list, count) }
        .iter()
        .filter(|p| !p.is_null())
        .filter_map(|&p| {
            unsafe { CStr::from_ptr(p) }
                .to_str()
                .ok()
                .map(str::to_owned)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_with(
    cols: u16,
    rows: u16,
    shell: *const c_char,
    cwd: *const c_char,
    panel_id: *const c_char,
    socket_path: *const c_char,
    set: &[(String, String)],
    remove: &[String],
) -> *mut CopadHandle {
    let safe_cols = cols.max(1);
    let safe_rows = rows.max(1);

    let mut tty_opts = TtyOptions::default();
    // Stamp panel identity and GUI socket into the child's env: `COPAD_PANEL_ID` routes the
    // `copad-cwd` hook's `chpwd` notifications to the right panel; `COPAD_SOCKET` makes `coctl`
    // dial our per-instance GUI socket rather than the well-known daemon path (without it the
    // hook short-circuits).
    // SAFETY: caller contract: panel_id / socket_path, if non-null, are NUL-terminated UTF-8
    // strings valid for the call.
    let panel_id_str: Option<String> = if panel_id.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(panel_id) }
            .to_str()
            .ok()
            .map(str::to_owned)
    };
    if let Some(s) = &panel_id_str {
        tty_opts.env.insert("COPAD_PANEL_ID".into(), s.clone());
    }
    let socket_str: Option<String> = if socket_path.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(socket_path) }
            .to_str()
            .ok()
            .map(str::to_owned)
    };
    if let Some(s) = &socket_str {
        tty_opts.env.insert("COPAD_SOCKET".into(), s.clone());
    }
    // Force a known TERM: an inherited foreign one (e.g. `xterm-ghostty`) missing from the
    // macOS terminfo DB makes zsh fall back through unrelated entries and mis-emit cursor/clear
    // sequences, showing duplicated keystrokes. xterm-256color is always present.
    tty_opts.env.insert("TERM".into(), "xterm-256color".into());
    // For programs that pick features by terminal name (yazi's image driver); the app removes
    // any other terminal's identity it was started with.
    tty_opts.env.insert("TERM_PROGRAM".into(), "copad".into());
    tty_opts.env.insert(
        "TERM_PROGRAM_VERSION".into(),
        env!("CARGO_PKG_VERSION").into(),
    );
    // Inject a default UTF-8 locale when none was inherited (as Ghostty does). Copad.app from
    // Finder/Dock has launchd's environment with no LANG/LC_*, and /etc/zprofile only sets it
    // for login shells, so non-login children (tmux panes) land in plain `C` and Unicode
    // glyphs (powerline, Nerd Font icons) render as `_`. An explicit parent locale is kept.
    if std::env::var_os("LANG").is_none()
        && std::env::var_os("LC_ALL").is_none()
        && std::env::var_os("LC_CTYPE").is_none()
    {
        tty_opts.env.insert("LANG".into(), "C.UTF-8".into());
    }
    for key in remove {
        tty_opts.env.remove(key);
    }
    tty_opts.env_remove = remove.to_vec();
    for (key, value) in set {
        tty_opts.env.insert(key.clone(), value.clone());
    }
    // May be NULL; alacritty then falls back to the system default shell.
    let shell_prog: Option<String> = if shell.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(shell) }
            .to_str()
            .ok()
            .map(str::to_owned)
    };
    if let Some(sh) = shell_prog {
        // LOGIN shell (`-l`): Copad.app from Finder/Dock inherits launchd's minimal PATH, and
        // only a login shell runs `/etc/zprofile` + `~/.zprofile` (Homebrew's `brew shellenv`)
        // before `~/.zshrc`. Without it an early `.zshrc` PATH probe fails ("works after
        // `exec zsh`, fails on a fresh pane"). `-l` is accepted by zsh/bash/fish/dash/tcsh.
        tty_opts.shell = Some(Shell::new(sh, vec!["-l".to_string()]));
    }
    if !cwd.is_null()
        && let Ok(s) = unsafe { CStr::from_ptr(cwd) }.to_str()
    {
        tty_opts.working_directory = Some(PathBuf::from(s));
    }

    let listener = CopadListener::new();
    *listener.grid_size.lock().unwrap() = (safe_cols, safe_rows);
    listener.graphics.enabled.store(true, Ordering::Relaxed);
    let window_size = listener.window_size();

    let pty = match tty::new(&tty_opts, window_size, 0) {
        Ok(p) => p,
        Err(_) => return ptr::null_mut(),
    };
    // Capture before the pty moves into EventLoop (which reaps the child on Shutdown). The pid
    // stays valid for `proc_pidinfo` until the shell exits, then `copad_term_child_cwd` returns NULL.
    let child_pid = pty.child().id() as i32;

    let term_size = TermSize::new(safe_cols as usize, safe_rows as usize);
    // `kitty_keyboard` keeps per-screen flag stacks and answers `CSI ? u`; the GUI encodes keys
    // against them via `copad_term_encode_key`.
    let config = TermOptions::default().config();
    let mut term = Term::new(config, &term_size, listener.clone());
    let mut images = graphics::Graphics::new(listener.graphics.clone(), {
        let listener = listener.clone();
        Box::new(move |reply: String| listener.send_to_pty(reply.into_bytes()))
    });
    let virtuals = images.virtuals();
    term.set_apc_handler(Some(Box::new(move |term, data| {
        images.handle(term, data);
    })));
    let term = Arc::new(FairMutex::new(term));

    let event_loop = match EventLoop::new(Arc::clone(&term), listener.clone(), pty, false, false) {
        Ok(el) => el,
        Err(_) => return ptr::null_mut(),
    };
    let sender = event_loop.channel();
    // Late-bound: `EventLoop::channel()` exists only after `EventLoop::new`, so the listener
    // can't hold the sender at construction. Earlier PtyWrite events drop harmlessly (no child
    // query yet).
    listener.set_sender(sender.clone());
    let io_thread = event_loop.spawn();

    Box::into_raw(Box::new(CopadHandle {
        term,
        listener,
        sender,
        io_thread: Some(io_thread),
        child_pid,
        last_redraw_state_hash: AtomicU64::new(0),
        last_display_offset: AtomicI32::new(0),
        last_selection_range: Mutex::new(CopadSelectionRange::default()),
        search_state: Mutex::new(SearchState::default()),
        options: Mutex::new(TermOptions::default()),
        virtuals,
    }))
}

/// macOS only: the PTY child's cwd via `proc_pidinfo(PROC_PIDVNODEPATHINFO)`. Returns a
/// NUL-terminated UTF-8 string to free with `copad_string_destroy`, or NULL if the shell has
/// exited, the syscall failed, or the platform isn't macOS. Works without OSC 7 from the shell.
///
/// # Safety
///
/// `handle` must be NULL or a pointer returned by `copad_term_create`
/// and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_child_cwd(handle: *mut CopadHandle) -> *mut CopadString {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return ptr::null_mut();
    };
    #[cfg(target_os = "macos")]
    {
        // EPERM on un-entitled dev builds: `proc_pidinfo(PROC_PIDVNODEPATHINFO)` needs the same
        // audit token, root, or the Apple-signed-only `com.apple.private.security.proc-info`.
        // The caller falls back to its initialCwd until install-macos.sh embeds an entitlement.
        let Some(cwd) = macos_child_cwd(h.child_pid) else {
            return ptr::null_mut();
        };
        Box::into_raw(Box::new(CopadString {
            data: cwd.into_bytes().into_boxed_slice(),
        }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = h;
        ptr::null_mut()
    }
}

#[cfg(target_os = "macos")]
fn macos_child_cwd(pid: i32) -> Option<String> {
    use libc::{PROC_PIDVNODEPATHINFO, proc_pidinfo, proc_vnodepathinfo};
    use std::ffi::c_void;
    use std::mem;
    let mut info: proc_vnodepathinfo = unsafe { mem::zeroed() };
    let n = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut proc_vnodepathinfo).cast::<c_void>(),
            mem::size_of::<proc_vnodepathinfo>() as i32,
        )
    };
    if n <= 0 {
        return None;
    }
    // `vip_path` is `[[c_char; 32]; 32]`; flatten and trim at the first NUL (the kernel
    // guarantees NUL-termination within MAXPATHLEN).
    let raw: &[u8] = unsafe {
        std::slice::from_raw_parts(
            info.pvi_cdir.vip_path.as_ptr().cast::<u8>(),
            mem::size_of_val(&info.pvi_cdir.vip_path),
        )
    };
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    if end == 0 {
        return None;
    }
    std::str::from_utf8(&raw[..end]).ok().map(str::to_owned)
}

/// Frees a handle: sends `Msg::Shutdown`, joins the reader, drops the Term. NULL is safe.
///
/// # Safety
///
/// Must be called exactly once per handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_destroy(handle: *mut CopadHandle) {
    if handle.is_null() {
        return;
    }
    let mut handle = unsafe { Box::from_raw(handle) };
    // Best-effort: if the reader already exited the send fails but join still cleans up.
    let _ = handle.sender.send(Msg::Shutdown);
    // The join runs on a detached thread, never the caller (the main thread, via the view
    // controller's deinit). Joining returns alacritty's `EventLoop`, whose `Pty::drop` SIGHUPs
    // the shell then `waitpid`s with NO timeout, so a shell that survives SIGHUP (e.g. `zsh -l`
    // still sourcing rc files) would freeze the app. Same fix as comux's `PaneTerm::drop`.
    if let Some(jh) = handle.io_thread.take() {
        let pid = handle.child_pid;
        let (count, _) = &REAPERS;
        *count.lock().unwrap() += 1;
        std::thread::spawn(move || {
            reap(jh, pid);
            let (count, done) = &REAPERS;
            *count.lock().unwrap() -= 1;
            done.notify_all();
        });
    }
}

/// Reaper threads `copad_term_destroy` started that haven't finished, for `copad_term_wait_reaps`.
static REAPERS: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());

/// Blocks until every reaper `copad_term_destroy` started has finished, or `timeout_ms` passed;
/// true when none is left. For app quit: exiting would cut a reaper short before it SIGKILLs a
/// shell that ignores SIGHUP.
#[unsafe(no_mangle)]
pub extern "C" fn copad_term_wait_reaps(timeout_ms: u32) -> bool {
    let (count, done) = &REAPERS;
    let guard = count.lock().unwrap();
    let (guard, _) = done
        .wait_timeout_while(
            guard,
            std::time::Duration::from_millis(timeout_ms.into()),
            |n| *n > 0,
        )
        .unwrap();
    *guard == 0
}

/// The PTY child's pid, or -1 once it exited (it may be reaped and the pid reused then).
///
/// # Safety
///
/// `handle` must be NULL or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_child_pid(handle: *mut CopadHandle) -> i32 {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    if h.listener.child_gone.load(Ordering::Relaxed) {
        -1
    } else {
        h.child_pid
    }
}

/// OSC 0/2 since the last call: 0 nothing new, 1 a title (`*out` set; free it with
/// `copad_string_destroy`), 2 the title was reset. Last wins.
///
/// # Safety
///
/// `handle` must be NULL or a live handle; `out` a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_take_title(
    handle: *mut CopadHandle,
    out: *mut *mut CopadString,
) -> i32 {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return 0;
    };
    match h.listener.pending_title.lock().unwrap().take() {
        None => 0,
        Some(None) => 2,
        Some(Some(title)) => {
            if !out.is_null() {
                let boxed = Box::new(CopadString {
                    data: title.into_bytes().into_boxed_slice(),
                });
                unsafe { *out = Box::into_raw(boxed) };
            }
            1
        }
    }
}

/// Whether a BEL arrived since the last call (clears on read).
///
/// # Safety
///
/// `handle` must be NULL or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_take_bell(handle: *mut CopadHandle) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.listener.bell.swap(false, Ordering::Relaxed)
}

/// How long a closed terminal's shell gets to exit on SIGHUP before SIGKILL.
const REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Joins the io-thread, then SIGHUPs the shell and SIGKILLs it after [`REAP_GRACE`] if still
/// running, before the `Pty` (whose drop `waitpid`s) goes. Signals go out only after the join
/// and only while the shell is un-reaped: alacritty reaps a shell that exits on its own, and
/// once the io-thread has returned only this thread can reap it, so the pid cannot be
/// recycled under us. Mirrors comux's `term::reap`.
fn reap<T>(io_thread: JoinHandle<T>, pid: i32) {
    let joined = io_thread.join();
    // The shell leads its own session (forkpty); its jobs have groups of their own in it. Hang
    // them all up as a closing terminal does, while the shell — running or a zombie — is not yet
    // reaped and so keeps the session id ours. No SIGKILL for them: a `nohup` job keeps running.
    #[cfg(target_os = "macos")]
    if pid > 1 && our_child(pid) {
        for group in session_groups(pid) {
            unsafe { libc::killpg(group, libc::SIGHUP) };
        }
    }
    #[cfg(target_os = "macos")]
    if pid > 1 && unreaped(pid) {
        unsafe { libc::kill(pid, libc::SIGHUP) };
        let deadline = std::time::Instant::now() + REAP_GRACE;
        while unreaped(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if unreaped(pid) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (pid, REAP_GRACE);
    drop(joined);
}

/// Whether `pid` is still our child, running or exited, but not yet reaped (`waitid` without
/// `ECHILD`); its pid can't be reused until it is.
#[cfg(target_os = "macos")]
fn our_child(pid: i32) -> bool {
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == 0
    }
}

/// The process groups in session `session`, from every process macOS lists.
#[cfg(target_os = "macos")]
fn session_groups(session: i32) -> Vec<i32> {
    let n = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0i32; n as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if n <= 0 {
        return Vec::new();
    }
    let mut groups: Vec<i32> = pids[..(n as usize).min(pids.len())]
        .iter()
        .filter(|&&p| p > 0 && unsafe { libc::getsid(p) } == session)
        .map(|&p| unsafe { libc::getpgid(p) })
        .filter(|&g| g > 0)
        .collect();
    groups.sort_unstable();
    groups.dedup();
    groups
}

/// Whether our child `pid` is still running and un-reaped, checked without reaping (`WNOWAIT`):
/// an exited child is a zombie with status pending, a reaped one is `ECHILD`.
#[cfg(target_os = "macos")]
fn unreaped(pid: i32) -> bool {
    // SAFETY: a zeroed siginfo_t is a valid out-buffer; with `WNOHANG` and no state change it
    // stays zeroed, so `si_signo` tells "still running" from "exited".
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let rc = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        rc == 0 && info.si_signo == 0
    }
}

/// Feeds input bytes to the PTY via the event-loop channel.
///
/// # Safety
///
/// `bytes` must point to `len` readable bytes (or be NULL when len=0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_input(handle: *mut CopadHandle, bytes: *const u8, len: usize) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    if len == 0 || bytes.is_null() {
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(bytes, len) };
    let _ = h.sender.send(Msg::Input(slice.to_vec().into()));
}

/// Populates one entry of the OSC colour-query palette used by `Event::ColorRequest`. Index
/// follows `vte::ansi::NamedColor`: 0-15 ANSI, 16-255 cube, 256 fg, 257 bg, 258 cursor.
/// Re-calling overwrites (theme hot-reload). Unpopulated indices produce no reply, so apps use
/// their own defaults rather than a colour we don't draw.
///
/// # Safety
///
/// `handle` must come from `copad_term_create`. Color components are
/// 8-bit linear RGB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_set_palette_entry(
    handle: *mut CopadHandle,
    index: u16,
    r: u8,
    g: u8,
    b: u8,
) -> i32 {
    if handle.is_null() {
        return -1;
    }
    // SAFETY: caller contract: handle came from `copad_term_create` and is not freed.
    let h = unsafe { &*handle };
    h.listener
        .palette
        .lock()
        .unwrap()
        .insert(index as usize, Rgb { r, g, b });
    0
}

/// Resizes the terminal grid, clamping zero dimensions to 1 so the PTY always sees a positive
/// `winsize`. Its pixel size comes from the host-reported cell size (1x1 until reported).
///
/// # Safety
///
/// `handle` must come from `copad_term_create` and not have been
/// freed. Safe to pass NULL — returns early in that case.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_resize(handle: *mut CopadHandle, cols: u16, rows: u16) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let safe_cols = cols.max(1);
    let safe_rows = rows.max(1);

    *h.listener.grid_size.lock().unwrap() = (safe_cols, safe_rows);
    let ws = h.listener.window_size();
    // `Msg::Resize` only SIGWINCHes the child via `pty.on_resize`; the Term grid is a separate
    // resize and must be done explicitly under the term lock (as alacritty's `on_resize` does).
    let _ = h.sender.send(Msg::Resize(ws));
    let term_size = TermSize::new(safe_cols as usize, safe_rows as usize);
    h.term.lock().resize(term_size);
}

/// Whether the grid was damaged since the last call; always resets damage state. Intended loop:
/// display tick, `copad_term_take_damage`, then `copad_term_snapshot` + redraw if true.
///
/// Cursor-only filter: `Term::damage` always marks the cursor cell (a hint for blinking-cursor
/// renderers), which would make the signal permanently true. Damage counts only when it covers
/// more than the cursor's single cell; cursor movement still counts since the region then
/// includes the previous cursor cell.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by `copad_term_create` and not yet
/// destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_take_damage(handle: *mut CopadHandle) -> bool {
    // Wrapper over `copad_term_take_damage_rows`: any non-zero outcome needs a repaint. Callers
    // MUST NOT call both entries in one frame; the rows variant advances the `last_*` state too.
    let mut buf: [u16; 1] = [0];
    let result = unsafe { copad_term_take_damage_rows(handle, buf.as_mut_ptr(), buf.len() as u16) };
    result != 0
}

/// Outcome of the per-row damage drain, typed so the orchestration doesn't decode a magic integer.
enum DamageRowsOutcome {
    /// Whole viewport must redraw (display_offset shifted, `TermDamage::Full`, or more dirty
    /// rows than the caller's buffer holds).
    Full,
    /// Distinct dirty viewport rows written to the caller's buffer, unordered; `0..=cap`.
    Count(u16),
}

/// Drains alacritty's per-row damage iterator into the caller's buffer, merged with sources its
/// bounds filter can't surface: cursor-row content/metadata (via `hash_redraw_state`),
/// scrollback offset change (promotes to `Full`), and the selection union old-and-new (a
/// shrink or clear must repaint rows no longer selected). Split from the FFI shim so tests can
/// use a synthetic `Term<VoidListener>`; previous state goes in and updated state comes back in
/// the return for the shim to write to the handle.
fn compute_damage_rows<L: EventListener>(
    term: &mut Term<L>,
    prev_display_offset: i32,
    prev_selection: CopadSelectionRange,
    prev_cursor_hash: u64,
    out: &mut [u16],
) -> (DamageRowsOutcome, i32, CopadSelectionRange, u64) {
    let cursor_point = term.grid().cursor.point;
    let current_offset = term.grid().display_offset() as i32;
    let current_selection = selection_range_for_ffi(term);
    let current_cursor_hash = hash_redraw_state(term, cursor_point);

    // Scrolling into/out of history changes every row. Per-row damage would lag the viewport
    // change and under-report, leaving stale pixels, so go Full and reset alacritty's damage.
    if prev_display_offset != current_offset {
        term.reset_damage();
        return (
            DamageRowsOutcome::Full,
            current_offset,
            current_selection,
            current_cursor_hash,
        );
    }

    let cursor_state_changed = current_cursor_hash != prev_cursor_hash;
    let screen_lines = term.screen_lines();
    let cursor_line = cursor_point.line.0.max(0) as usize;
    let cursor_col = cursor_point.column.0;

    // Dirty rows go in a fixed 256-bit bitmap; out-of-range returns `Full`.
    let mut dirty_bitmap: [u64; 4] = [0; 4];
    let bitmap_max_row: usize = 256;
    let mut full = false;

    let mark_row = |bitmap: &mut [u64; 4], row: usize| -> Result<(), ()> {
        if row >= bitmap_max_row {
            return Err(());
        }
        bitmap[row / 64] |= 1u64 << (row % 64);
        Ok(())
    };

    match term.damage() {
        alacritty_terminal::term::TermDamage::Full => full = true,
        alacritty_terminal::term::TermDamage::Partial(iter) => {
            for d in iter {
                // Skip the cursor-cell-only hint alacritty emits on every `damage()` call; the
                // cursor is handled by `cursor_state_changed` below.
                if d.line == cursor_line && d.left == cursor_col && d.right == cursor_col {
                    continue;
                }
                // `damage()` only reports the live region (scrollback writes fire nothing); map
                // to a viewport row via `current_offset`, stable since the offset-change bail.
                let viewport_row = d.line as i32 + current_offset;
                if viewport_row < 0 || viewport_row as usize >= screen_lines {
                    continue;
                }
                if mark_row(&mut dirty_bitmap, viewport_row as usize).is_err() {
                    full = true;
                    break;
                }
            }
        }
    }
    term.reset_damage();

    if full {
        return (
            DamageRowsOutcome::Full,
            current_offset,
            current_selection,
            current_cursor_hash,
        );
    }

    // Cursor row dirty. Its viewport index uses the same offset map as snapshot rows, so a user
    // scrolled past the live cursor doesn't get a stray "row 0".
    if cursor_state_changed {
        let cursor_viewport_row = cursor_point.line.0 + current_offset;
        if cursor_viewport_row >= 0 && (cursor_viewport_row as usize) < screen_lines {
            let _ = mark_row(&mut dirty_bitmap, cursor_viewport_row as usize);
        }
    }

    // Selection union: old and new rows both repaint, or a shrink leaves stale overlay.
    for sel in [&prev_selection, &current_selection] {
        if sel.present != 1 {
            continue;
        }
        let lo = sel.start_row.min(sel.end_row) as usize;
        let hi = sel.start_row.max(sel.end_row) as usize;
        for r in lo..=hi {
            if r >= screen_lines {
                break;
            }
            let _ = mark_row(&mut dirty_bitmap, r);
        }
    }

    // Past the buffer cap, promote to Full rather than hand back a partial list with gaps.
    let cap = out.len();
    let mut count: usize = 0;
    for (word_idx, word) in dirty_bitmap.iter().enumerate() {
        let mut bits = *word;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let row = word_idx * 64 + bit;
            if count >= cap {
                return (
                    DamageRowsOutcome::Full,
                    current_offset,
                    current_selection,
                    current_cursor_hash,
                );
            }
            out[count] = row as u16;
            count += 1;
        }
    }

    (
        DamageRowsOutcome::Count(count as u16),
        current_offset,
        current_selection,
        current_cursor_hash,
    )
}

/// Per-row damage drain: viewport rows needing repaint since the last call. Returns `-1` for
/// Full repaint (scrollback offset changed, `TermDamage::Full`, or more dirty rows than `cap`),
/// else the count `0..=cap`; the first `count` u16 slots of `out_buf` hold distinct row indices
/// in unspecified order. Returns `0` with the buffer untouched when nothing changed. Resets
/// alacritty's damage state.
///
/// # Safety
///
/// * `handle` must be NULL or a valid pointer returned by `copad_term_create` and not yet
///   destroyed.
/// * `out_buf` must point to writable storage for `cap` `u16` slots when `cap > 0`; with
///   `cap == 0` it MAY be null and the function returns `-1` immediately.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_take_damage_rows(
    handle: *mut CopadHandle,
    out_buf: *mut u16,
    cap: u16,
) -> i32 {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return 0;
    };
    if cap == 0 {
        // No room for rows: drain the damage state anyway so the next call starts clean.
        let mut term = h.term.lock();
        term.reset_damage();
        return -1;
    }

    let prev_offset = h.last_display_offset.load(Ordering::Relaxed);
    let prev_selection = h
        .last_selection_range
        .lock()
        .map(|guard| *guard)
        .unwrap_or_default();
    let prev_cursor_hash = h.last_redraw_state_hash.load(Ordering::Relaxed);

    let mut term = h.term.lock();
    // SAFETY: caller contract: `out_buf` has `cap` writable u16 slots. The term lock is held,
    // so concurrent FFI calls can't race on it (the caller is a single render thread).
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_buf, cap as usize) };
    let (outcome, new_offset, new_selection, new_cursor_hash) = compute_damage_rows(
        &mut term,
        prev_offset,
        prev_selection,
        prev_cursor_hash,
        out_slice,
    );
    drop(term);

    h.last_display_offset.store(new_offset, Ordering::Relaxed);
    if let Ok(mut guard) = h.last_selection_range.lock() {
        *guard = new_selection;
    }
    h.last_redraw_state_hash
        .store(new_cursor_hash, Ordering::Relaxed);

    match outcome {
        DamageRowsOutcome::Full => -1,
        DamageRowsOutcome::Count(n) => i32::from(n),
    }
}

/// Projects `term.selection` into the inclusive-bounds struct the renderer paints from; default
/// (present=0) when there is none or it doesn't resolve (empty drag, scrolled past). Generic
/// over `EventListener` so tests can use `Term<VoidListener>`.
fn selection_range_for_ffi<L: EventListener>(term: &Term<L>) -> CopadSelectionRange {
    let Some(sel) = term.selection.as_ref() else {
        return CopadSelectionRange::default();
    };
    let Some(range): Option<SelectionRange> = sel.to_range(term) else {
        return CopadSelectionRange::default();
    };
    let display_offset = term.grid().display_offset() as i32;
    let last_row = term.screen_lines().saturating_sub(1) as i32;
    let last_col = term.columns().saturating_sub(1) as u16;
    let start_view = range.start.line.0 + display_offset;
    let end_view = range.end.line.0 + display_offset;
    if end_view < 0 || start_view > last_row {
        return CopadSelectionRange::default();
    }
    let (start_row, start_col) = if start_view < 0 {
        if range.is_block {
            (0u16, range.start.column.0 as u16)
        } else {
            (0u16, 0u16)
        }
    } else {
        (start_view as u16, range.start.column.0 as u16)
    };
    let (end_row, end_col) = if end_view > last_row {
        if range.is_block {
            (last_row as u16, range.end.column.0 as u16)
        } else {
            (last_row as u16, last_col)
        }
    } else {
        (end_view as u16, range.end.column.0 as u16)
    };
    CopadSelectionRange {
        start_row,
        start_col,
        end_row,
        end_col,
        is_block: u8::from(range.is_block),
        present: 1,
        _reserved: 0,
    }
}

/// Hashes everything the snapshot path exposes: cursor row contents, cursor pos/style/blink/
/// visibility, selection bounds and scrollback offset. Lets `compute_damage_rows` catch changes
/// the line-bounds filter collapses under the unconditional `damage_cursor` hint (DECSCUSR,
/// selection start/extend/clear, blink). Caller holds the term lock. An out-of-range cursor
/// hashes to a fixed sentinel. Generic over `EventListener` for `Term<VoidListener>` tests.
fn hash_redraw_state<L: EventListener>(term: &Term<L>, cursor: Point) -> u64 {
    let line = cursor.line;
    if line.0 < 0 || (line.0 as usize) >= term.screen_lines() {
        return 0;
    }
    let cols = term.columns();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    {
        let grid = term.grid();
        for c in 0..cols {
            let cell = &grid[Point::new(line, Column(c))];
            cell.c.hash(&mut hasher);
            cell.flags.bits().hash(&mut hasher);
            color_to_rgba(cell.fg).hash(&mut hasher);
            color_to_rgba(cell.bg).hash(&mut hasher);
            if let Some(extras) = cell.zerowidth() {
                for ch in extras {
                    ch.hash(&mut hasher);
                }
            }
            if let Some(uc) = cell.underline_color() {
                color_to_rgba(uc).hash(&mut hasher);
            }
            if let Some(h) = cell.hyperlink() {
                h.id().hash(&mut hasher);
                h.uri().hash(&mut hasher);
            } else {
                0u8.hash(&mut hasher);
            }
        }
    }
    cursor.line.0.hash(&mut hasher);
    cursor.column.0.hash(&mut hasher);
    let cs = term.cursor_style();
    (cs.shape as u8).hash(&mut hasher);
    cs.blinking.hash(&mut hasher);
    term.mode()
        .contains(TermMode::SHOW_CURSOR)
        .hash(&mut hasher);
    let sel = selection_range_for_ffi(term);
    sel.present.hash(&mut hasher);
    if sel.present == 1 {
        sel.start_row.hash(&mut hasher);
        sel.start_col.hash(&mut hasher);
        sel.end_row.hash(&mut hasher);
        sel.end_col.hash(&mut hasher);
        sel.is_block.hash(&mut hasher);
    }
    term.grid().display_offset().hash(&mut hasher);
    hasher.finish()
}

/// Takes a snapshot of the visible viewport. Lock duration is bounded by walking `rows x cols`
/// cells and copying them into the snapshot's owned buffers.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_snapshot(handle: *mut CopadHandle) -> *mut CopadSnapshot {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return ptr::null_mut();
    };

    let term = h.term.lock();
    let cols = term.columns() as u16;
    let rows_count = term.screen_lines() as u16;
    let grid = term.grid();

    // Dedup key is `(id, uri)`, not id alone: alacritty preserves explicit OSC 8 ids, so two
    // distinct URIs can share one.
    let mut hyperlinks: Vec<String> = Vec::new();
    let mut hyperlink_index_by_key: std::collections::HashMap<(String, String), u32> =
        std::collections::HashMap::new();

    // When scrolled into history, viewport row 0 maps to live line `-display_offset`; Grid
    // indexing walks into scrollback for negative lines, so the snapshot is what the user sees.
    let display_offset = grid.display_offset() as i32;

    let mut snapshot_rows = Vec::with_capacity(rows_count as usize);
    for line_idx in 0..rows_count as i32 {
        let line = Line(line_idx - display_offset);
        let row = walk_row(
            grid,
            line,
            cols,
            &mut hyperlinks,
            &mut hyperlink_index_by_key,
        );
        snapshot_rows.push(row);
    }

    let cursor_point = term.grid().cursor.point;
    // `cursor_style()` honors DECSCUSR and vi-mode; SHOW_CURSOR gates rendering. HollowBlock
    // collapses to Block: hollow-on-blur is window focus state, drawn by the renderer.
    let cs = term.cursor_style();
    let show_cursor = term
        .mode()
        .contains(alacritty_terminal::term::TermMode::SHOW_CURSOR);
    let style = if !show_cursor {
        0
    } else {
        match cs.shape {
            CursorShape::Hidden => 0,
            CursorShape::Block | CursorShape::HollowBlock => 1,
            CursorShape::Beam => 2,
            CursorShape::Underline => 3,
        }
    };
    // Same display_offset mapping as the rows. Scrolled into history the cursor can sit outside
    // the viewport: `style = 0` (hidden) so no stray block is drawn over scrollback. Position
    // is still emitted.
    let cursor_viewport_row = cursor_point.line.0 + display_offset;
    let cursor_visible = cursor_viewport_row >= 0 && (cursor_viewport_row as u16) < rows_count;
    let cursor = CopadCursor {
        row: cursor_viewport_row.max(0) as u16,
        col: cursor_point.column.0 as u16,
        style: if cursor_visible { style } else { 0 },
        blink: if cs.blinking { 1 } else { 0 },
        _reserved: 0,
    };

    let selection = selection_range_for_ffi(&term);
    let virtuals = h.virtuals.lock().unwrap().clone();
    let placeholders = graphics::Placeholders {
        virtuals: &virtuals,
        cell: h.listener.graphics.cell(),
    };
    let (graphics, graphic_images) = graphics::collect(
        grid,
        display_offset,
        rows_count as usize,
        Some(&placeholders),
    );

    drop(term);

    // Locked strictly AFTER the term lock is released so lock order matches
    // `copad_term_search_next` (search_state THEN term); otherwise they could AB-BA deadlock.
    // A match scrolled out of view collapses to `present == 0`.
    let search_match = search_match_for_ffi(&h.search_state, display_offset, rows_count, cols);

    Box::into_raw(Box::new(CopadSnapshot {
        cols,
        rows: snapshot_rows,
        cursor,
        selection,
        search_match,
        hyperlinks,
        graphics,
        graphic_images,
    }))
}

/// Projects the find match (alacritty grid coords) into the viewport range the renderer
/// paints; empty if there is no match or it is entirely off-screen.
fn search_match_for_ffi(
    state: &Mutex<SearchState>,
    display_offset: i32,
    rows_count: u16,
    cols: u16,
) -> CopadSearchRange {
    let guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return CopadSearchRange::default(),
    };
    let Some(range) = guard.current_match.as_ref() else {
        return CopadSearchRange::default();
    };
    let start_view = range.start().line.0 + display_offset;
    let end_view = range.end().line.0 + display_offset;
    let last_row = rows_count.saturating_sub(1) as i32;
    if end_view < 0 || start_view > last_row {
        return CopadSearchRange::default();
    }
    // Clip to the visible range. A clipped endpoint makes the boundary row a "middle row" of
    // the wrapped match, which paints the full row width: a start clipped above begins at col 0,
    // an end clipped below runs through the last column.
    let (start_row_clamped, start_col_clamped) = if start_view < 0 {
        (0u16, 0u16)
    } else {
        (start_view as u16, range.start().column.0 as u16)
    };
    let (end_row_clamped, end_col_clamped) = if end_view > last_row {
        // Match extends below the viewport: the last visible row is a "middle row" of the wrap,
        // painting col 0 through the rightmost column (`cols - 1`, 0 if cols == 0).
        (last_row as u16, cols.saturating_sub(1))
    } else {
        (end_view as u16, range.end().column.0 as u16)
    };
    CopadSearchRange {
        start_row: start_row_clamped,
        start_col: start_col_clamped,
        end_row: end_row_clamped,
        end_col: end_col_clamped,
        present: 1,
        _reserved: [0; 3],
    }
}

/// Walks one display line into a `Row`, grouping consecutive cells with identical attributes
/// AND the same single-byte ASCII char into one run, so the renderer makes one CTLine per span
/// (idle/scrollback frames are mostly spaces). Deliberately conservative: uniform ASCII only,
/// so the cursor-cell glyph re-render gets the right char; no wide chars or combining marks.
fn walk_row(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    line: Line,
    cols: u16,
    hyperlinks: &mut Vec<String>,
    hyperlink_index_by_key: &mut std::collections::HashMap<(String, String), u32>,
) -> Row {
    let mut utf8: Vec<u8> = Vec::new();
    let mut runs: Vec<CopadRun> = Vec::new();
    // Per run, the ASCII byte all its cells share, or None if non-uniform (multi-byte, combining,
    // wide or mixed). Only Some entries can be extended by `try_extend_last_run`.
    let mut run_uniform: Vec<Option<u8>> = Vec::new();
    let mut col: u16 = 0;

    while col < cols {
        let point = Point::new(line, Column(col as usize));
        let cell = &grid[point];

        // Trailing half of a wide char carries no glyph; the leading half's run absorbs it.
        if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
            col += 1;
            continue;
        }

        let span_cols = if cell.flags.contains(CellFlags::WIDE_CHAR) {
            2
        } else {
            1
        };

        let mut run_flags = cell_flags_to_ffi(cell.flags);
        if span_cols == 2 {
            run_flags |= flags::WIDE_LEADING;
        }

        let (fg, bg) = (color_to_rgba(cell.fg), color_to_rgba(cell.bg));
        // `underline_color()` can be Spec (`\e[58;2;...m`), Indexed or a named colour, so use
        // the fg/bg encoder rather than dropping non-Spec values.
        let underline_color = cell.underline_color().map(color_to_rgba).unwrap_or(0);
        let underline_style = underline_style(cell.flags);
        let hyperlink_id = cell
            .hyperlink()
            .map(|h| {
                let key = (h.id().to_owned(), h.uri().to_owned());
                if let Some(idx) = hyperlink_index_by_key.get(&key) {
                    return *idx;
                }
                hyperlinks.push(key.1.clone());
                let new_idx = hyperlinks.len() as u32; // 1-based
                hyperlink_index_by_key.insert(key, new_idx);
                new_idx
            })
            .unwrap_or(0);

        // Aggregation eligibility: single-cell ASCII with no combining marks. Anything else gets
        // its own run so cursor-cell glyph extraction stays "pick the run's bytes".
        let has_zw = cell.zerowidth().is_some_and(|z| !z.is_empty());
        let cell_byte: Option<u8> = if span_cols == 1 && !has_zw && cell.c.is_ascii() {
            Some(cell.c as u8)
        } else {
            None
        };

        if let Some(b) = cell_byte
            && let (Some(last), Some(last_uniform)) = (runs.last_mut(), run_uniform.last_mut())
            && *last_uniform == Some(b)
            && last.fg_rgba == fg
            && last.bg_rgba == bg
            && last.flags == run_flags
            && last.underline_color_rgba == underline_color
            && last.underline_style == underline_style
            && last.hyperlink_id == hyperlink_id
        {
            // Extend by one column; utf8 stays uniform since the same byte is appended.
            utf8.push(b);
            last.utf8_len += 1;
            last.end_col += 1;
            col += 1;
            continue;
        }

        let utf8_offset = utf8.len() as u32;
        let mut buf = [0u8; 4];
        if cell.c == graphics::PLACEHOLDER {
            // A kitty Unicode placeholder: the image is drawn there, not the character.
            utf8.push(b' ');
        } else {
            utf8.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
            // Combining marks (CellExtra.zerowidth) fold into the run's utf8 so CoreText shapes
            // them with their base glyph.
            for combine in cell.zerowidth().unwrap_or(&[]) {
                utf8.extend_from_slice(combine.encode_utf8(&mut buf).as_bytes());
            }
        }
        let utf8_len = utf8.len() as u32 - utf8_offset;

        runs.push(CopadRun {
            start_col: col,
            end_col: col + span_cols as u16,
            utf8_offset,
            utf8_len,
            fg_rgba: fg,
            bg_rgba: bg,
            flags: run_flags,
            underline_style,
            reserved: 0,
            underline_color_rgba: underline_color,
            hyperlink_id,
        });
        run_uniform.push(cell_byte);

        col += span_cols as u16;
    }

    Row { utf8, runs }
}

/// `CopadRun.underline_style`: 0 none, 1 single, 2 double, 3 curly, 4 dotted, 5 dashed
/// (SGR 4:0..4:5, which alacritty keeps as distinct flags).
fn underline_style(f: CellFlags) -> u8 {
    if f.contains(CellFlags::UNDERCURL) {
        3
    } else if f.contains(CellFlags::DOUBLE_UNDERLINE) {
        2
    } else if f.contains(CellFlags::DOTTED_UNDERLINE) {
        4
    } else if f.contains(CellFlags::DASHED_UNDERLINE) {
        5
    } else if f.intersects(CellFlags::ALL_UNDERLINES) {
        1
    } else {
        0
    }
}

fn cell_flags_to_ffi(f: CellFlags) -> u16 {
    let mut out = 0u16;
    if f.contains(CellFlags::BOLD) {
        out |= flags::BOLD;
    }
    if f.contains(CellFlags::ITALIC) {
        out |= flags::ITALIC;
    }
    if f.contains(CellFlags::INVERSE) {
        out |= flags::INVERSE;
    }
    if f.contains(CellFlags::DIM) {
        out |= flags::DIM;
    }
    if f.contains(CellFlags::STRIKEOUT) {
        out |= flags::STRIKE;
    }
    if f.contains(CellFlags::HIDDEN) { /* nothing in our flag set; renderer can decide */ }
    // ALL_UNDERLINES covers every variant; the UNDERLINE bit is collapsed here and
    // `CopadRun::underline_style` carries which one.
    if f.intersects(CellFlags::ALL_UNDERLINES) {
        out |= flags::UNDERLINE;
    }
    out
}

/// Encoding of the `fg_rgba` / `bg_rgba` u32 fields; the high byte tags the kind:
///
/// - `0x00_00_00_00`: default (renderer uses theme fg/bg).
/// - `0x01_00_00_NN`: indexed palette colour N. Swift resolves 0-15 from `theme.palette`,
///   16-231 from the 6x6x6 xterm cube, 232-255 from the 24-step grayscale ramp.
/// - `0xFF_RR_GG_BB`: direct RGB, always opaque.
///
/// A tag is needed because the older "alpha=0 means indexed" trick was ambiguous with RGB
/// colours whose R channel is 0 (`\\e[38;2;0;200;255m`), which went down the indexed path. Other
/// tag values are reserved.
const TAG_INDEXED: u32 = 0x01_00_00_00;
const TAG_DIRECT: u32 = 0xFF_00_00_00;

fn color_to_rgba(color: AnsiColor) -> u32 {
    match color {
        AnsiColor::Named(NamedColor::Foreground) | AnsiColor::Named(NamedColor::Background) => 0,
        AnsiColor::Named(named) => match named_to_indexed(named) {
            Some(idx) => TAG_INDEXED | idx as u32,
            None => 0,
        },
        AnsiColor::Indexed(idx) => TAG_INDEXED | idx as u32,
        AnsiColor::Spec(rgb) => {
            TAG_DIRECT | ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | (rgb.b as u32)
        }
    }
}

/// Maps `NamedColor` to ANSI palette indices the Swift side resolves, keeping bright/dim
/// honest (bright red is 9, not 1). `None` for non-palette colors (DimFg, Cursor, ...) so the
/// caller uses the default sentinel.
fn named_to_indexed(named: NamedColor) -> Option<u8> {
    let idx: u8 = match named {
        NamedColor::Black => 0,
        NamedColor::Red => 1,
        NamedColor::Green => 2,
        NamedColor::Yellow => 3,
        NamedColor::Blue => 4,
        NamedColor::Magenta => 5,
        NamedColor::Cyan => 6,
        NamedColor::White => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        _ => return None,
    };
    Some(idx)
}

///
/// # Safety
///
/// `snap` must be NULL or a valid pointer returned by
/// `copad_term_snapshot` and not yet destroyed. Calling twice is UB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_destroy(snap: *mut CopadSnapshot) {
    if snap.is_null() {
        return;
    }
    let _ = unsafe { Box::from_raw(snap) };
}

/// # Safety
///
/// `snap` must be NULL or a valid pointer returned by
/// `copad_term_snapshot` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_rows(snap: *const CopadSnapshot) -> u16 {
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return 0;
    };
    s.rows.len() as u16
}

/// # Safety
///
/// `snap` must be NULL or a valid pointer returned by
/// `copad_term_snapshot` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_cols(snap: *const CopadSnapshot) -> u16 {
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return 0;
    };
    s.cols
}

/// Borrowed pointer to the row's run array, valid until `copad_snapshot_destroy`. Returns 0 and
/// sets `*out_runs` to NULL if the row is out of range.
///
/// # Safety
///
/// `out_runs` must point to writable storage for one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_row_runs(
    snap: *const CopadSnapshot,
    row: u16,
    out_runs: *mut *const CopadRun,
) -> usize {
    if out_runs.is_null() {
        return 0;
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        unsafe { *out_runs = ptr::null() };
        return 0;
    };
    let Some(row_data) = s.rows.get(row as usize) else {
        unsafe { *out_runs = ptr::null() };
        return 0;
    };
    unsafe { *out_runs = row_data.runs.as_ptr() };
    row_data.runs.len()
}

/// Borrowed pointer to the row's utf8 bytes + length; same lifetime.
/// # Safety
///
/// `out_len` must point to writable storage for one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_row_utf8(
    snap: *const CopadSnapshot,
    row: u16,
    out_len: *mut usize,
) -> *const u8 {
    if out_len.is_null() {
        return ptr::null();
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        unsafe { *out_len = 0 };
        return ptr::null();
    };
    match s.rows.get(row as usize) {
        Some(row_data) => {
            unsafe { *out_len = row_data.utf8.len() };
            row_data.utf8.as_ptr()
        }
        None => {
            unsafe { *out_len = 0 };
            ptr::null()
        }
    }
}

/// # Safety
///
/// `out` must point to writable storage for one `CopadCursor`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_cursor(snap: *const CopadSnapshot, out: *mut CopadCursor) {
    if out.is_null() {
        return;
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return;
    };
    unsafe { *out = s.cursor };
}

/// Fills `*out` with the active selection bounds; check `out.present`.
///
/// # Safety
///
/// `out` must point to writable storage for one `CopadSelectionRange`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_selection(
    snap: *const CopadSnapshot,
    out: *mut CopadSelectionRange,
) {
    if out.is_null() {
        return;
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return;
    };
    unsafe { *out = s.selection };
}

/// Copies out the current find match in viewport coordinates, read every frame to paint the
/// highlight under glyphs. `present == 0` means no match.
///
/// # Safety
///
/// `out` must point to writable storage for one `CopadSearchRange`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_search_match(
    snap: *const CopadSnapshot,
    out: *mut CopadSearchRange,
) {
    if out.is_null() {
        return;
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return;
    };
    unsafe { *out = s.search_match };
}

/// Backslash-escapes regex metacharacters, like `regex::escape`, without pulling in the
/// `regex` crate (alacritty uses `regex-automata`).
fn escape_regex_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Finds the next/previous match of `pattern` in the grid and scrollback. `pattern` is a fixed
/// string (the caller escapes regex chars; the find bar exposes no regex in v1).
///
/// `case_sensitive == false` wraps it in `(?i:...)`; `forward` picks `regex_search_right` vs
/// `regex_search_left`. The search starts after the current match's end (forward) or before its
/// start (backward), else at the term cursor, and wraps around the grid. On a hit it caches the
/// match, scrolls the match-start row on-screen and returns `true`; on a miss it clears the
/// cache and returns `false`.
///
/// The compiled `RegexSearch` is reused when `(pattern, case_sensitive)` is unchanged, since
/// alacritty builds four lazy DFAs per search.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed. `pattern` must point
/// to a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_search_next(
    handle: *mut CopadHandle,
    pattern: *const c_char,
    case_sensitive: bool,
    forward: bool,
) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    if pattern.is_null() {
        return false;
    }
    // SAFETY: caller contract: pattern is a NUL-terminated UTF-8 string.
    let raw_pattern = match unsafe { CStr::from_ptr(pattern) }.to_str() {
        Ok(s) if !s.is_empty() => s,
        _ => {
            if let Ok(mut state) = h.search_state.lock() {
                state.current_match = None;
            }
            return false;
        }
    };

    let mut state = match h.search_state.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };

    // Escape regex metacharacters so `foo.bar` doesn't match `fooXbar`.
    let escaped = escape_regex_literal(raw_pattern);

    // Force case bias explicitly: alacritty's RegexSearch::new is SMART-CASE (insensitive iff no
    // uppercase), so `case_sensitive=true` for "foo" would still match "FOO" without the
    // `(?-i:...)` override.
    let cache_key_pattern = if case_sensitive {
        format!("(?-i:{escaped})")
    } else {
        format!("(?i:{escaped})")
    };
    let cache_hit = state.regex.is_some()
        && state.pattern == cache_key_pattern
        && state.case_sensitive == case_sensitive;
    if !cache_hit {
        match alacritty_terminal::term::search::RegexSearch::new(&cache_key_pattern) {
            Ok(r) => {
                state.regex = Some(r);
                state.pattern = cache_key_pattern;
                state.case_sensitive = case_sensitive;
                // Query changed: the cached match is from another pattern, and stepping past it
                // could skip a valid hit (extending "abc" to "abcd" would step past the "abc" match
                // and miss the overlapping "abcd"). Reset so the search starts at the term cursor.
                state.current_match = None;
            }
            Err(_) => {
                // Invalid pattern (only possible from unbalanced parens in user input despite the
                // wrap): bail leaving current_match so the previous highlight stays.
                return false;
            }
        }
    }

    let mut term = h.term.lock();
    let cols = term.columns();
    let total_lines = term.total_lines();
    let topmost_line = term.topmost_line();
    let bottommost_line = term.bottommost_line();

    // Origin: past the cached match's boundary in the chosen direction, else the term cursor so
    // the first search finds the "foo" nearest where the user was looking.
    let cursor_point = term.grid().cursor.point;
    let start_point = match (forward, state.current_match.as_ref()) {
        (true, Some(m)) => {
            // Step one cell past the match end, wrapping to the top past the bottom-right corner.
            let mut p = *m.end();
            if p.column.0 + 1 >= cols {
                p.column = Column(0);
                p.line = if p.line == bottommost_line {
                    topmost_line
                } else {
                    Line(p.line.0 + 1)
                };
            } else {
                p.column = Column(p.column.0 + 1);
            }
            p
        }
        (false, Some(m)) => {
            let mut p = *m.start();
            if p.column.0 == 0 {
                p.column = Column(cols.saturating_sub(1));
                p.line = if p.line == topmost_line {
                    bottommost_line
                } else {
                    Line(p.line.0 - 1)
                };
            } else {
                p.column = Column(p.column.0 - 1);
            }
            p
        }
        (_, None) => cursor_point,
    };

    // Bounds are the full grid (live + scrollback). `regex_search_*` take `&mut RegexSearch`
    // (lazy DFAs mutate cache state), so take the regex out of the Option for the search and
    // put it back.
    let mut regex = state
        .regex
        .take()
        .expect("regex set above when cache_hit was false");

    let found = if forward {
        // Forward search wraps by hand (alacritty doesn't): start_point to bottom, then top to
        // start_point.
        let bottom_right = Point {
            line: bottommost_line,
            column: Column(cols.saturating_sub(1)),
        };
        let first = term.regex_search_right(&mut regex, start_point, bottom_right);
        first.or_else(|| {
            let top_left = Point {
                line: topmost_line,
                column: Column(0),
            };
            // Don't return the same match twice if the only match is at start_point.
            let mut prev_end = start_point;
            if prev_end.column.0 == 0 {
                prev_end.column = Column(cols.saturating_sub(1));
                prev_end.line = if prev_end.line == topmost_line {
                    bottommost_line
                } else {
                    Line(prev_end.line.0 - 1)
                };
            } else {
                prev_end.column = Column(prev_end.column.0 - 1);
            }
            term.regex_search_right(&mut regex, top_left, prev_end)
        })
    } else {
        let top_left = Point {
            line: topmost_line,
            column: Column(0),
        };
        let first = term.regex_search_left(&mut regex, start_point, top_left);
        first.or_else(|| {
            let bottom_right = Point {
                line: bottommost_line,
                column: Column(cols.saturating_sub(1)),
            };
            let mut prev_start = start_point;
            if prev_start.column.0 + 1 >= cols {
                prev_start.column = Column(0);
                prev_start.line = if prev_start.line == bottommost_line {
                    topmost_line
                } else {
                    Line(prev_start.line.0 + 1)
                };
            } else {
                prev_start.column = Column(prev_start.column.0 + 1);
            }
            term.regex_search_left(&mut regex, prev_start, bottom_right)
        })
    };
    state.regex = Some(regex);
    // Keeps the binding so the `let _` below avoids an unused warning that would block clippy.
    let _ = total_lines;

    state.current_match = found.clone();

    // Scroll the viewport to show the match row. display_offset counts rows above the live
    // bottom (0 = live). For line `L` (negative = scrollback), bringing it to the bottom row
    // needs `(screen_lines-1) - L`, clamped to [0, total_lines-screen_lines].
    if let Some(ref m) = found {
        let screen_lines = term.screen_lines() as i32;
        let match_line = m.start().line.0;
        let needed_offset = if match_line < 0 {
            (-match_line) as usize
        } else if match_line >= screen_lines {
            // Shouldn't happen (live max is screen_lines-1); defensive.
            0
        } else {
            0 // Match already in live viewport.
        };
        let max_offset = term.history_size();
        let final_offset = needed_offset.min(max_offset);
        let current_offset = term.grid().display_offset();
        if current_offset != final_offset {
            // Scroll via Term::scroll_display: ::Top / ::Bottom / ::Lines(N) (positive = into
            // history).
            let delta = final_offset as i32 - current_offset as i32;
            if delta != 0 {
                term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
            }
        }
    }

    found.is_some()
}

/// Drops the cached regex and current match; the highlight vanishes next frame.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_search_clear(handle: *mut CopadHandle) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    if let Ok(mut state) = h.search_state.lock() {
        state.regex = None;
        state.pattern.clear();
        state.current_match = None;
    }
}

/// Selection kind for `copad_term_selection_start`, as named constants for Swift/C consumers;
/// SIMPLE is taken by the match's fallback arm.
#[allow(dead_code)]
const SELECTION_SIMPLE: u8 = 0;
const SELECTION_SEMANTIC: u8 = 1;
const SELECTION_LINES: u8 = 2;
const SELECTION_BLOCK: u8 = 3;

/// Side of the cell: 0 = Left, 1 = Right (mirrors alacritty's `Side`), so the renderer can
/// compute it from the pixel offset without bringing the enum across FFI.
const SIDE_LEFT: u8 = 0;

fn parse_side(side: u8) -> Side {
    if side == SIDE_LEFT {
        Side::Left
    } else {
        Side::Right
    }
}

fn selection_point(term: &Term<CopadListener>, row: u16, col: u16) -> Point {
    // The renderer passes viewport-relative rows; subtract display_offset to get alacritty's
    // Line (scrolled back, row 0 is `Line(-display_offset)`). Otherwise drag/click on
    // scrolled-back content selects the live grid at the same row.
    let display_offset = term.grid().display_offset() as i32;
    let line = Line(row as i32 - display_offset);
    let cols = term.columns();
    let column = Column((col as usize).min(cols.saturating_sub(1)));
    Point::new(line, column)
}

/// Starts a selection at (`row`, `col`), replacing any existing one. `kind` is
/// `SELECTION_SIMPLE` / `SEMANTIC` / `LINES`; anything else is simple.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_selection_start(
    handle: *mut CopadHandle,
    row: u16,
    col: u16,
    side: u8,
    kind: u8,
) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let mut term = h.term.lock();
    let point = selection_point(&term, row, col);
    let ty = match kind {
        SELECTION_SEMANTIC => SelectionType::Semantic,
        SELECTION_LINES => SelectionType::Lines,
        SELECTION_BLOCK => SelectionType::Block,
        _ => SelectionType::Simple,
    };
    term.selection = Some(Selection::new(ty, point, parse_side(side)));
}

/// Extends the current selection to `(row, col, side)`; no-op if none is in progress.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_selection_update(
    handle: *mut CopadHandle,
    row: u16,
    col: u16,
    side: u8,
) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let mut term = h.term.lock();
    let point = selection_point(&term, row, col);
    if let Some(sel) = term.selection.as_mut() {
        sel.update(point, parse_side(side));
    }
}

/// Clears the active selection.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_selection_clear(handle: *mut CopadHandle) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    h.term.lock().selection = None;
}

/// Selects the visible viewport (Cmd+A) as a Simple selection from (0, 0) to (last_line,
/// last_col).
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_selection_all(handle: *mut CopadHandle) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let mut term = h.term.lock();
    // The visible viewport, which when scrolled into history is scrollback lines: top is
    // `Line(-display_offset)`, bottom that plus `screen_lines - 1`. Matches iTerm2/Terminal.app
    // Cmd+A (only what the user can see).
    let display_offset = term.grid().display_offset() as i32;
    let screen_lines = term.screen_lines() as i32;
    let top = Line(-display_offset);
    let bottom = Line(screen_lines - 1 - display_offset);
    let last_col = Column(term.columns().saturating_sub(1));
    let start = Point::new(top, Column(0));
    let end = Point::new(bottom, last_col);
    let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
    sel.update(end, Side::Right);
    term.selection = Some(sel);
}

/// Heap-allocated UTF-8 copy of the current selection, or NULL when none. Free with
/// `copad_string_destroy` exactly once.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_selection_string(handle: *mut CopadHandle) -> *mut CopadString {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return ptr::null_mut();
    };
    let term = h.term.lock();
    let Some(s) = term.selection_to_string() else {
        return ptr::null_mut();
    };
    if s.is_empty() {
        return ptr::null_mut();
    }
    Box::into_raw(Box::new(CopadString {
        data: s.into_bytes().into_boxed_slice(),
    }))
}

/// Renders the last `lines` scrollback rows as plain text, `\n` between rows, no trailing
/// newline, matching SwiftTerm's `getLine(row: -N..0)` walk so `terminal.history` has the same
/// shape on both backends. Scrolling into history does not shift it: it is always the N rows
/// above the original viewport top (`terminal.read` gives what is visible).
///
/// Returns a non-NULL zero-length `CopadString` when `lines == 0` or no scrollback exists, so
/// the JSON shape stays stable. Free with `copad_string_destroy` exactly once.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_history(
    handle: *mut CopadHandle,
    lines: usize,
) -> *mut CopadString {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return ptr::null_mut();
    };
    let term = h.term.lock();
    let cols = term.columns();
    let text = read_history_text(term.grid(), cols, lines);
    Box::into_raw(Box::new(CopadString {
        data: text.into_bytes().into_boxed_slice(),
    }))
}

/// Joins `lines` rows of scrollback above the viewport top into plain text, oldest first. Split
/// from the FFI so tests can use a synthetic Term. Clamps `lines` to `history_size()`; NUL
/// cells render as `' '`.
fn read_history_text(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    cols: usize,
    lines: usize,
) -> String {
    let take = lines.min(grid.history_size());
    if take == 0 || cols == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(take * cols + take.saturating_sub(1));
    for i in (1..=take).rev() {
        let line = Line(-(i as i32));
        for col in 0..cols {
            let cell = &grid[Point::new(line, Column(col))];
            let ch = cell.c;
            out.push(if ch == '\0' { ' ' } else { ch });
        }
        if i > 1 {
            out.push('\n');
        }
    }
    out
}

/// Borrowed pointer to the string's bytes (NOT NUL-terminated); `*out_len` is the byte length.
/// Both are valid until `copad_string_destroy`.
///
/// # Safety
///
/// `out_len` must point to writable storage for one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_string_bytes(
    s: *const CopadString,
    out_len: *mut usize,
) -> *const u8 {
    if out_len.is_null() {
        return ptr::null();
    }
    let Some(s) = (unsafe { s.as_ref() }) else {
        unsafe { *out_len = 0 };
        return ptr::null();
    };
    unsafe { *out_len = s.data.len() };
    s.data.as_ptr()
}

/// Frees a `CopadString`. NULL-safe.
///
/// # Safety
///
/// Must be called exactly once per pointer returned by an FFI method
/// that hands out `*mut CopadString`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_string_destroy(s: *mut CopadString) {
    if s.is_null() {
        return;
    }
    let _ = unsafe { Box::from_raw(s) };
}

/// True if any mouse-reporting mode is active. The renderer then defers to the TUI (vim, less,
/// htop, tmux) instead of consuming drags for selection, unless the user holds Shift.
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_mouse_mode_active(handle: *mut CopadHandle) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    let term = h.term.lock();
    use alacritty_terminal::term::TermMode as M;
    term.mode()
        .intersects(M::MOUSE_REPORT_CLICK | M::MOUSE_DRAG | M::MOUSE_MOTION)
}

pub const COPAD_MOUSE_ENC_NONE: u8 = 0;
pub const COPAD_MOUSE_ENC_LEGACY: u8 = 1;
pub const COPAD_MOUSE_ENC_SGR: u8 = 2;
pub const COPAD_MOUSE_ENC_UTF8: u8 = 3;

/// Mouse-event encoding negotiated by the TUI, for choosing the byte sequence of forwarded
/// mouse events. Encodings are mutually exclusive, so at most one:
///
///   0 = NONE (no reporting; do not forward)
///   1 = LEGACY (X10 `\e[M<cb><cc><cr>`, coords offset by 32, max 223)
///   2 = SGR    (`\e[<cb;cc;cr;{M|m}`, no coord cap; preferred)
///   3 = UTF8   (`\e[M<cb><cc><cr>` with multi-byte coords)
///
/// Non-zero only if a reporting-mode bit (CLICK / DRAG / MOTION) is also on, as in
/// `mouse_mode_active`.
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_mouse_encoding(handle: *mut CopadHandle) -> u8 {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return COPAD_MOUSE_ENC_NONE;
    };
    let term = h.term.lock();
    use alacritty_terminal::term::TermMode as M;
    let mode = term.mode();
    if !mode.intersects(M::MOUSE_REPORT_CLICK | M::MOUSE_DRAG | M::MOUSE_MOTION) {
        return COPAD_MOUSE_ENC_NONE;
    }
    if mode.contains(M::SGR_MOUSE) {
        COPAD_MOUSE_ENC_SGR
    } else if mode.contains(M::UTF8_MOUSE) {
        COPAD_MOUSE_ENC_UTF8
    } else {
        COPAD_MOUSE_ENC_LEGACY
    }
}

pub const COPAD_MOUSE_LEVEL_NONE: u8 = 0;
pub const COPAD_MOUSE_LEVEL_CLICK: u8 = 1;
pub const COPAD_MOUSE_LEVEL_DRAG: u8 = 2;
pub const COPAD_MOUSE_LEVEL_MOTION: u8 = 3;

/// Highest mouse-reporting level negotiated by the TUI. The xterm modes are mutually exclusive,
/// so this is a scalar rather than the flag bitmask:
///
///   0 = NONE (renderer keeps the mouse for selection)
///   1 = CLICK   (`\e[?1000h`) press + release only
///   2 = DRAG    (`\e[?1002h`) adds motion while a button is held
///   3 = MOTION  (`\e[?1003h`) all motion
///
/// MOTION implies DRAG implies CLICK: send press/release at any level, drag events at
/// DRAG/MOTION, bare motion only at MOTION.
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_mouse_report_level(handle: *mut CopadHandle) -> u8 {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return COPAD_MOUSE_LEVEL_NONE;
    };
    let term = h.term.lock();
    use alacritty_terminal::term::TermMode as M;
    let mode = term.mode();
    // Highest wins. alacritty masks the previous mode on enable so only one bit is normally
    // set; highest-first is robust to overlap.
    if mode.contains(M::MOUSE_MOTION) {
        COPAD_MOUSE_LEVEL_MOTION
    } else if mode.contains(M::MOUSE_DRAG) {
        COPAD_MOUSE_LEVEL_DRAG
    } else if mode.contains(M::MOUSE_REPORT_CLICK) {
        COPAD_MOUSE_LEVEL_CLICK
    } else {
        COPAD_MOUSE_LEVEL_NONE
    }
}

/// True if bracketed paste (`\e[?2004h`) is on: the renderer wraps pasted text in
/// `\e[200~ ... \e[201~` so paste-aware programs can tell it from typing.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_bracketed_paste_active(handle: *mut CopadHandle) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.term
        .lock()
        .mode()
        .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
}

/// The kitty keyboard protocol flags the visible screen negotiated (`CSI > flags u`); 0 when none,
/// so keys go out in the legacy encoding.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_keyboard_flags(handle: *mut CopadHandle) -> u8 {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return 0;
    };
    kitty_flags(*h.term.lock().mode())
}

fn kitty_flags(mode: TermMode) -> u8 {
    use copad_keys::flags;
    [
        (TermMode::DISAMBIGUATE_ESC_CODES, flags::DISAMBIGUATE),
        (TermMode::REPORT_EVENT_TYPES, flags::EVENT_TYPES),
        (TermMode::REPORT_ALTERNATE_KEYS, flags::ALTERNATE_KEYS),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, flags::ALL_KEYS),
        (TermMode::REPORT_ASSOCIATED_TEXT, flags::ASSOCIATED_TEXT),
    ]
    .into_iter()
    .filter(|(m, _)| mode.contains(*m))
    .fold(0, |acc, (_, f)| acc | f)
}

/// Turn the kitty keyboard protocol off (`[terminal] kitty_keyboard = false`) or back on.
/// Off, `CSI > flags u` is ignored and `CSI ? u` goes unanswered, so programs fall back to
/// the legacy encoding.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_set_kitty_keyboard(handle: *mut CopadHandle, enabled: bool) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    apply_options(h, |o| o.kitty_keyboard = enabled);
}

/// How many lines of scrollback the pane keeps (`[terminal] scrollback`), clamped to
/// [`MAX_SCROLLBACK`]. Every kept line costs its full width in cells (24 bytes each), so
/// this is the main lever on a pane's memory. Lowering it drops the oldest lines for good;
/// raising it later does not bring them back.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_set_scrollback(handle: *mut CopadHandle, lines: u32) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    apply_options(h, |o| o.scrollback = (lines as usize).min(MAX_SCROLLBACK));
}

/// One macOS key event, as `copad_term_encode_key` needs it. Strings are UTF-8, not
/// NUL-terminated, and may be NULL when their length is 0.
#[repr(C)]
pub struct CopadKeyInput {
    /// The hardware virtual key code (`NSEvent.keyCode`).
    pub key_code: u16,
    /// kitty modifier bits: shift 1, alt 2, ctrl 4, super 8, caps lock 64.
    pub mods: u8,
    /// 0 press, 1 repeat, 2 release.
    pub action: u8,
    /// What the key types with no modifiers (`characters(byApplyingModifiers: [])`).
    pub unmodified: *const u8,
    pub unmodified_len: usize,
    /// What it types with Shift (`characters(byApplyingModifiers: .shift)`).
    pub shifted: *const u8,
    pub shifted_len: usize,
    /// The text this event types — empty when Ctrl / Option-as-Alt made it a chord.
    pub text: *const u8,
    pub text_len: usize,
}

/// Send the event's text through the normal text path (also: the protocol is off).
pub const COPAD_KEY_TEXT: u8 = 0;
/// `out[..*out_len]` holds the bytes to send.
pub const COPAD_KEY_BYTES: u8 = 1;
/// Send nothing.
pub const COPAD_KEY_NOTHING: u8 = 2;

unsafe fn ffi_str<'a>(ptr: *const u8, len: usize) -> &'a str {
    if ptr.is_null() || len == 0 {
        return "";
    }
    // SAFETY: caller contract, `ptr` points to `len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).unwrap_or("")
}

/// Encodes a key event for the kitty keyboard protocol the program negotiated: `COPAD_KEY_TEXT`
/// when the protocol is off (flags 0) or the key just types its text, `COPAD_KEY_BYTES` with
/// the bytes in `out`, or `COPAD_KEY_NOTHING`.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer from `copad_term_create`; `input` must point to a
/// valid `CopadKeyInput` whose strings follow its contract; `out` must have `out_cap` writable
/// bytes and `out_len` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_encode_key(
    handle: *mut CopadHandle,
    input: *const CopadKeyInput,
    out: *mut u8,
    out_cap: usize,
    out_len: *mut usize,
) -> u8 {
    let (Some(h), Some(input)) = (unsafe { handle.as_ref() }, unsafe { input.as_ref() }) else {
        return COPAD_KEY_TEXT;
    };
    let (flags, app_cursor) = {
        let term = h.term.lock();
        let mode = *term.mode();
        (kitty_flags(mode), mode.contains(TermMode::APP_CURSOR))
    };
    let unmodified = unsafe { ffi_str(input.unmodified, input.unmodified_len) };
    let shifted = unsafe { ffi_str(input.shifted, input.shifted_len) };
    let text = unsafe { ffi_str(input.text, input.text_len) };
    let Some(key) = copad_keys::layout::macos_key(input.key_code, unmodified) else {
        return if text.is_empty() {
            COPAD_KEY_NOTHING
        } else {
            COPAD_KEY_TEXT
        };
    };
    let ev = copad_keys::KeyEvent {
        key,
        shifted: copad_keys::layout::single_char(shifted),
        base: copad_keys::layout::us_base_macos(input.key_code),
        mods: input.mods,
        action: match input.action {
            1 => copad_keys::Action::Repeat,
            2 => copad_keys::Action::Release,
            _ => copad_keys::Action::Press,
        },
        text,
    };
    // Protocol off: only keys without text get their legacy form here (arrows in DECCKM's, Home,
    // F5 as `CSI 15 ~`, Shift+Tab as `CSI Z`). Everything else — text, Ctrl letters, Option
    // chords the host maps to readline's `ESC b` — stays the host's.
    if flags == 0 && !legacy_functional(&ev) {
        return COPAD_KEY_TEXT;
    }
    match copad_keys::encode(&ev, flags, app_cursor) {
        copad_keys::Encoded::Text => COPAD_KEY_TEXT,
        copad_keys::Encoded::Nothing => COPAD_KEY_NOTHING,
        copad_keys::Encoded::Bytes(bytes) => {
            if out.is_null() || out_len.is_null() || bytes.len() > out_cap {
                return COPAD_KEY_NOTHING;
            }
            // SAFETY: caller contract — `out` has `out_cap` writable bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len());
                *out_len = bytes.len();
            }
            COPAD_KEY_BYTES
        }
    }
}

/// A key `copad_term_encode_key` encodes even with the kitty protocol off: no text, and no Option
/// (the host maps Option+arrows to readline's word moves).
fn legacy_functional(ev: &copad_keys::KeyEvent) -> bool {
    use copad_keys::Key;
    if ev.mods & copad_keys::mods::ALT != 0 || ev.action == copad_keys::Action::Release {
        return false;
    }
    match ev.key {
        Key::Left | Key::Right | Key::Up | Key::Down => true,
        Key::Home | Key::End | Key::PageUp | Key::PageDown | Key::Insert | Key::Delete => true,
        Key::F(_) => true,
        Key::Tab => ev.mods & copad_keys::mods::SHIFT != 0,
        _ => false,
    }
}

/// Scroll kind for `copad_term_scroll`, mirroring `alacritty_terminal::grid::Scroll` so the enum
/// stays off the FFI.
#[allow(dead_code)]
const SCROLL_DELTA: u8 = 0;
const SCROLL_PAGE_UP: u8 = 1;
const SCROLL_PAGE_DOWN: u8 = 2;
const SCROLL_TOP: u8 = 3;
const SCROLL_BOTTOM: u8 = 4;

/// Scrolls the viewport. `kind` is one of `SCROLL_*`; `delta` applies only to `SCROLL_DELTA`
/// (lines; positive brings older content into view).
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_scroll(handle: *mut CopadHandle, kind: u8, delta: i32) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    use alacritty_terminal::grid::Scroll;
    let scroll = match kind {
        SCROLL_PAGE_UP => Scroll::PageUp,
        SCROLL_PAGE_DOWN => Scroll::PageDown,
        SCROLL_TOP => Scroll::Top,
        SCROLL_BOTTOM => Scroll::Bottom,
        _ => Scroll::Delta(delta),
    };
    h.term.lock().scroll_display(scroll);
}

/// Takes the pending OSC 52 clipboard-store request (`\e]52;c;<base64>\a`), or NULL. Free with
/// `copad_string_destroy`; the caller gates the NSPasteboard write on `[security] osc52`.
/// Single slot: a burst coalesces to the last write.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_take_clipboard_request(
    handle: *mut CopadHandle,
) -> *mut CopadString {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return ptr::null_mut();
    };
    let Some(text) = h.listener.pending_clipboard.lock().unwrap().take() else {
        return ptr::null_mut();
    };
    Box::into_raw(Box::new(CopadString {
        data: text.into_bytes().into_boxed_slice(),
    }))
}

/// True iff the PTY child has exited since the last call; clears the latch on read. The
/// renderer then broadcasts `panel.exited`, which copad-core ContextService and daemon
/// subscribers use to clear per-panel cwd / active-doc state.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_take_child_exit(handle: *mut CopadHandle) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.listener
        .child_exited
        .swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Number of distinct OSC 8 URIs in this snapshot; `CopadRun.hyperlink_id` is a 1-based index
/// in `[1, count]`, 0 meaning none.
///
/// # Safety
///
/// `snap` must be NULL or a valid pointer returned by
/// `copad_term_snapshot` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_hyperlink_count(snap: *const CopadSnapshot) -> u32 {
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return 0;
    };
    s.hyperlinks.len() as u32
}

/// Borrowed pointer to the URI bytes for a 1-based hyperlink id, valid until the snapshot is
/// destroyed (copy first). NULL with `*out_len = 0` when out of range.
///
/// # Safety
///
/// `out_len` must point to writable storage for one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_hyperlink_uri(
    snap: *const CopadSnapshot,
    hyperlink_id: u32,
    out_len: *mut usize,
) -> *const u8 {
    if out_len.is_null() {
        return ptr::null();
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        unsafe { *out_len = 0 };
        return ptr::null();
    };
    if hyperlink_id == 0 {
        unsafe { *out_len = 0 };
        return ptr::null();
    }
    let idx = (hyperlink_id as usize).saturating_sub(1);
    let Some(uri) = s.hyperlinks.get(idx) else {
        unsafe { *out_len = 0 };
        return ptr::null();
    };
    unsafe { *out_len = uri.len() };
    uri.as_ptr()
}

/// Image runs in the snapshot, in draw order.
///
/// # Safety
///
/// `snap` must be NULL or a live snapshot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_graphic_count(snap: *const CopadSnapshot) -> u32 {
    unsafe { snap.as_ref() }.map_or(0, |s| s.graphics.len() as u32)
}

/// Copy image run `index` into `out`; false when out of range.
///
/// # Safety
///
/// `snap` must be NULL or a live snapshot; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_graphic(
    snap: *const CopadSnapshot,
    index: u32,
    out: *mut graphics::CopadGraphic,
) -> bool {
    let Some(g) = unsafe { snap.as_ref() }.and_then(|s| s.graphics.get(index as usize)) else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    unsafe { *out = *g };
    true
}

/// Image `index` of the snapshot (the `image` of a run). Pixels are borrowed from the snapshot.
///
/// # Safety
///
/// `snap` must be NULL or a live snapshot; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_snapshot_graphic_image(
    snap: *const CopadSnapshot,
    index: u32,
    out: *mut CopadGraphicImage,
) -> bool {
    let Some(image) = unsafe { snap.as_ref() }.and_then(|s| s.graphic_images.get(index as usize))
    else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    unsafe {
        *out = CopadGraphicImage {
            key: image.key,
            width: image.width,
            height: image.height,
            pixels: image.rgba.as_ptr(),
        }
    };
    true
}

/// The pane gained or lost keyboard focus. A program that asked for focus reports
/// (`CSI ? 1004 h`) gets `CSI I` / `CSI O`; the terminal's own focus flag follows.
///
/// # Safety
///
/// `handle` must be NULL or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_set_focused(handle: *mut CopadHandle, focused: bool) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let reports = {
        let mut term = h.term.lock();
        let changed = term.is_focused != focused;
        term.is_focused = focused;
        changed && term.mode().contains(TermMode::FOCUS_IN_OUT)
    };
    if reports {
        h.listener.send_to_pty(if focused {
            b"\x1b[I".to_vec()
        } else {
            b"\x1b[O".to_vec()
        });
    }
}

/// `[terminal] images`: whether programs may draw kitty-graphics images. Off, every graphics
/// command is ignored without a reply, so programs see no support.
///
/// # Safety
///
/// `handle` must be NULL or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_set_images(handle: *mut CopadHandle, enabled: bool) {
    if let Some(h) = unsafe { handle.as_ref() } {
        h.listener
            .graphics
            .enabled
            .store(enabled, Ordering::Relaxed);
    }
}

/// The cell size in device pixels: what images are sized against and what `TIOCGWINSZ` and
/// `CSI 14 t` / `CSI 16 t` report. Call on creation and on every font or scale change.
///
/// # Safety
///
/// `handle` must be NULL or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_set_cell_pixels(
    handle: *mut CopadHandle,
    width: u32,
    height: u32,
) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let g = &h.listener.graphics;
    let old_width = g.cell_width.swap(width, Ordering::Relaxed);
    let old_height = g.cell_height.swap(height, Ordering::Relaxed);
    if (old_width, old_height) == (width, height) {
        return;
    }
    let _ = h.sender.send(Msg::Resize(h.listener.window_size()));
}

#[unsafe(no_mangle)]
pub extern "C" fn copad_term_version() -> *const c_char {
    static VERSION: &CStr = c"copad-term 0.2.0 (Phase 2 — PTY + grid)";
    VERSION.as_ptr()
}

#[cfg(test)]
mod color_encoding_tests {
    use super::*;
    use alacritty_terminal::vte::ansi::Rgb;

    #[test]
    fn default_named_colors_use_sentinel_zero() {
        assert_eq!(color_to_rgba(AnsiColor::Named(NamedColor::Foreground)), 0);
        assert_eq!(color_to_rgba(AnsiColor::Named(NamedColor::Background)), 0);
    }

    #[test]
    fn named_palette_colors_carry_indexed_tag() {
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::Red)),
            TAG_INDEXED | 1
        );
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::Yellow)),
            TAG_INDEXED | 3
        );
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::BrightRed)),
            TAG_INDEXED | 9
        );
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::BrightWhite)),
            TAG_INDEXED | 15
        );
    }

    #[test]
    fn indexed_256_carries_indexed_tag() {
        assert_eq!(color_to_rgba(AnsiColor::Indexed(0)), TAG_INDEXED);
        assert_eq!(color_to_rgba(AnsiColor::Indexed(245)), TAG_INDEXED | 245);
        assert_eq!(color_to_rgba(AnsiColor::Indexed(255)), TAG_INDEXED | 255);
    }

    /// Regression: RGB with R=0 was mis-decoded as indexed (high byte 0); it now carries the
    /// 0xFF direct tag.
    #[test]
    fn rgb_with_zero_red_does_not_collide_with_indexed() {
        let skyblue = color_to_rgba(AnsiColor::Spec(Rgb {
            r: 0,
            g: 200,
            b: 255,
        }));
        assert_eq!(skyblue >> 24, 0xFF, "direct-color tag must be set");
        assert_eq!((skyblue >> 16) & 0xFF, 0);
        assert_eq!((skyblue >> 8) & 0xFF, 200);
        assert_eq!(skyblue & 0xFF, 255);

        let pure_green = color_to_rgba(AnsiColor::Spec(Rgb { r: 0, g: 255, b: 0 }));
        assert_eq!(pure_green >> 24, 0xFF);
        assert_eq!(pure_green, TAG_DIRECT | (255 << 8));
    }

    #[test]
    fn rgb_round_trip_preserves_channels() {
        let red = color_to_rgba(AnsiColor::Spec(Rgb { r: 255, g: 0, b: 0 }));
        assert_eq!(red, TAG_DIRECT | (255 << 16));

        let black = color_to_rgba(AnsiColor::Spec(Rgb { r: 0, g: 0, b: 0 }));
        assert_eq!(black, TAG_DIRECT);
        assert_ne!(black, 0);
    }

    #[test]
    fn named_unmappable_falls_back_to_default() {
        // DimFg, Cursor etc. aren't in the 16-colour palette and collapse to the default.
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::DimForeground)),
            0
        );
    }

    /// Underline colour uses the fg/bg encoder, so indexed (`\e[58;5;Nm`) and direct
    /// (`\e[58;2;...m`) round-trip instead of becoming "use fg".
    #[test]
    fn underline_color_uses_same_encoding_as_fg() {
        assert_eq!(
            color_to_rgba(AnsiColor::Spec(Rgb {
                r: 10,
                g: 20,
                b: 30
            })),
            TAG_DIRECT | (10 << 16) | (20 << 8) | 30,
        );
        assert_eq!(color_to_rgba(AnsiColor::Indexed(5)), TAG_INDEXED | 5);
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::Red)),
            TAG_INDEXED | 1,
        );
    }

    #[test]
    fn underline_styles_reach_the_renderer() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::Config;
        use alacritty_terminal::vte::ansi::Processor;
        let mut term = Term::new(Config::default(), &TermSize::new(12, 2), VoidListener);
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(
            &mut term,
            b"\x1b[4ma\x1b[4:2mb\x1b[4:3mc\x1b[4:4md\x1b[4:5me\x1b[24mf",
        );
        let row = walk_row(
            term.grid(),
            Line(0),
            12,
            &mut Vec::new(),
            &mut HashMap::new(),
        );
        let styles: Vec<u8> = row.runs.iter().take(6).map(|r| r.underline_style).collect();
        assert_eq!(styles, vec![1, 2, 3, 4, 5, 0]);

        for style in 1..=5 {
            let mut term = Term::new(Config::default(), &TermSize::new(12, 2), VoidListener);
            let seq = format!("\x1b[58;2;255;0;0m\x1b[4:{style}mab");
            Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new()
                .advance(&mut term, seq.as_bytes());
            let row = walk_row(
                term.grid(),
                Line(0),
                12,
                &mut Vec::new(),
                &mut HashMap::new(),
            );
            assert_eq!(
                row.runs[0].underline_color_rgba,
                TAG_DIRECT | 0xff0000,
                "style {style}"
            );
        }
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::Handler;

    /// A 4x4 Term with generous scrollback; drive content via `Handler::input`.
    fn fixture_term() -> Term<VoidListener> {
        let size = TermSize::new(4, 4);
        let cfg = Config {
            scrolling_history: 32,
            ..Default::default()
        };
        Term::new(cfg, &size, VoidListener)
    }

    /// Pushes N rows labeled "rXX" via Handler::input + linefeed + carriage_return, so the grid
    /// and scrollback wire up as for live PTY bytes.
    fn push_lines<T: alacritty_terminal::event::EventListener>(term: &mut Term<T>, n: usize) {
        for i in 1..=n {
            // 4-col label "rNN "; avoids N > 99, where alacritty would wrap at col 4.
            let label = format!("r{i:02} ");
            for ch in label.chars().take(4) {
                Handler::input(term, ch);
            }
            Handler::linefeed(term);
            Handler::carriage_return(term);
        }
    }

    #[test]
    fn scrollback_option_caps_shrinks_and_survives_the_kitty_toggle() {
        let opts = |kitty_keyboard, scrollback| TermOptions {
            kitty_keyboard,
            scrollback,
        };
        let mut term = Term::new(opts(true, 20).config(), &TermSize::new(4, 4), VoidListener);
        push_lines(&mut term, 60);
        assert_eq!(term.grid().history_size(), 20);
        // Toggling the other option keeps the limit.
        term.set_options(opts(false, 20).config());
        push_lines(&mut term, 60);
        assert_eq!(term.grid().history_size(), 20);
        term.set_options(opts(false, 5).config());
        assert_eq!(term.grid().history_size(), 5);
        term.set_options(opts(false, 30).config());
        assert_eq!(term.grid().history_size(), 5);
        push_lines(&mut term, 10);
        assert_eq!(term.grid().history_size(), 15);
        term.set_options(opts(false, 0).config());
        push_lines(&mut term, 10);
        assert_eq!(term.grid().history_size(), 0);
    }

    #[test]
    fn scrollback_shrinks_the_primary_grid_while_the_alt_screen_is_up() {
        let opts = TermOptions {
            kitty_keyboard: true,
            scrollback: 20,
        };
        let mut term = Term::new(opts.config(), &TermSize::new(4, 4), VoidListener);
        push_lines(&mut term, 40);
        Handler::set_private_mode(
            &mut term,
            alacritty_terminal::vte::ansi::NamedPrivateMode::SwapScreenAndSetRestoreCursor.into(),
        );
        term.set_options(
            TermOptions {
                scrollback: 3,
                ..opts
            }
            .config(),
        );
        Handler::unset_private_mode(
            &mut term,
            alacritty_terminal::vte::ansi::NamedPrivateMode::SwapScreenAndSetRestoreCursor.into(),
        );
        assert_eq!(term.grid().history_size(), 3);
    }

    #[test]
    fn empty_history_returns_empty_string() {
        let term = fixture_term();
        let text = read_history_text(term.grid(), term.columns(), 10);
        assert_eq!(text, "");
    }

    #[test]
    fn zero_lines_returns_empty_string() {
        let mut term = fixture_term();
        push_lines(&mut term, 10);
        let text = read_history_text(term.grid(), term.columns(), 0);
        assert_eq!(text, "");
    }

    #[test]
    fn clamps_to_history_size_without_panic() {
        let mut term = fixture_term();
        // 6 rows into a 4-row viewport: the trailing linefeed leaves the cursor on an empty
        // bottom row, so the viewport is {r04, r05, r06, ""} and scrollback {r01, r02, r03}.
        push_lines(&mut term, 6);
        let history_size = term.grid().history_size();
        assert_eq!(
            history_size, 3,
            "expected 3 scrollback rows from 6 pushes in a 4-row viewport"
        );
        let text = read_history_text(term.grid(), term.columns(), 100);
        assert_eq!(text.matches('\n').count(), 2);
        let row_count = text.split('\n').count();
        assert_eq!(row_count, 3);
    }

    #[test]
    fn returns_oldest_first_and_excludes_viewport() {
        let mut term = fixture_term();
        // 10 pushes into a 4-row viewport: viewport {r08, r09, r10, ""}, scrollback r01..r07, so
        // the last 3 scrollback rows are r05..r07.
        push_lines(&mut term, 10);
        let text = read_history_text(term.grid(), term.columns(), 3);
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("r05"), "first line was {:?}", lines[0]);
        assert!(
            lines[1].starts_with("r06"),
            "second line was {:?}",
            lines[1]
        );
        assert!(lines[2].starts_with("r07"), "third line was {:?}", lines[2]);
        assert!(!text.contains("r08"));
        assert!(!text.contains("r10"));
    }

    #[test]
    fn nul_cells_render_as_space() {
        let mut term = fixture_term();
        Handler::input(&mut term, 'X');
        Handler::linefeed(&mut term);
        Handler::carriage_return(&mut term);
        push_lines(&mut term, 5);
        let text = read_history_text(term.grid(), term.columns(), 6);
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(
            lines[0], "X   ",
            "expected NUL → space padding, got {:?}",
            lines[0]
        );
    }

    #[test]
    fn no_trailing_newline() {
        let mut term = fixture_term();
        push_lines(&mut term, 10);
        let text = read_history_text(term.grid(), term.columns(), 4);
        assert!(!text.ends_with('\n'), "history must not end with newline");
    }
}

#[cfg(test)]
mod damage_rows_tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::Handler;

    fn fixture_term() -> Term<VoidListener> {
        let size = TermSize::new(8, 4);
        let cfg = Config {
            scrolling_history: 32,
            ..Default::default()
        };
        Term::new(cfg, &size, VoidListener)
    }

    fn type_text<T: EventListener>(term: &mut Term<T>, s: &str) {
        for ch in s.chars() {
            Handler::input(term, ch);
        }
    }

    fn empty_sel() -> CopadSelectionRange {
        CopadSelectionRange::default()
    }

    /// The first call after writes must report damage: alacritty starts every Term with
    /// TermDamage::Full pending until the first reset.
    #[test]
    fn initial_full_damage_drains_to_full_or_rows() {
        let mut term = fixture_term();
        type_text(&mut term, "hi");
        let mut buf = [0u16; 16];
        let (outcome, _, _, _) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Not Count(0): the writes must produce some damage signal.
        match outcome {
            DamageRowsOutcome::Full => {}
            DamageRowsOutcome::Count(n) => assert!(n >= 1, "expected >=1 dirty row, got 0"),
        }
    }

    /// An idle term after a clean drain reports zero dirty rows; the cursor-cell hint alacritty
    /// re-asserts on every `damage()` must be filtered out or an idle terminal redraws every vsync.
    #[test]
    fn idle_terminal_reports_zero_dirty_rows() {
        let mut term = fixture_term();
        type_text(&mut term, "x");
        let mut buf = [0u16; 16];
        let (_, off1, sel1, hash1) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        let (outcome, _, _, _) = compute_damage_rows(&mut term, off1, sel1, hash1, &mut buf);
        match outcome {
            DamageRowsOutcome::Count(0) => {}
            DamageRowsOutcome::Count(n) => panic!("expected 0 dirty rows on idle, got {n}"),
            DamageRowsOutcome::Full => panic!("expected 0 dirty rows on idle, got Full"),
        }
    }

    /// Cap=0 gives Full: no room for partial damage, so drain state and signal full repaint.
    #[test]
    fn zero_cap_returns_full_and_drains_damage() {
        let mut term = fixture_term();
        type_text(&mut term, "data");
        let mut buf = [0u16; 0];
        let (outcome, _, _, _) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        assert!(matches!(outcome, DamageRowsOutcome::Full));
    }

    /// A scrollback offset change gives Full even with no live writes; driven with a stale
    /// prev_display_offset.
    #[test]
    fn display_offset_change_promotes_to_full() {
        let mut term = fixture_term();
        for i in 0..6 {
            type_text(&mut term, &format!("r{i}"));
            Handler::linefeed(&mut term);
            Handler::carriage_return(&mut term);
        }
        let mut buf = [0u16; 16];
        let (_, off1, sel1, hash1) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Claim the previous offset differed, as after a scroll-back.
        let (outcome, _, _, _) = compute_damage_rows(&mut term, off1 + 1, sel1, hash1, &mut buf);
        assert!(matches!(outcome, DamageRowsOutcome::Full));
    }

    /// Selection union: a cleared selection on row 1 must still report row 1 dirty so the
    /// overlay is repainted off those cells.
    #[test]
    fn old_selection_rows_remain_dirty_after_clear() {
        let mut term = fixture_term();
        type_text(&mut term, "data");
        let mut buf = [0u16; 16];
        let (_, off1, _, hash1) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Synthetic prev selection on row 1, cols 0..=3, as painted last frame.
        let prev = CopadSelectionRange {
            start_row: 1,
            start_col: 0,
            end_row: 1,
            end_col: 3,
            is_block: 0,
            present: 1,
            _reserved: 0,
        };
        let (outcome, _, new_sel, _) = compute_damage_rows(&mut term, off1, prev, hash1, &mut buf);
        // No current selection, but the union must still include row 1 from prev.
        assert_eq!(new_sel.present, 0);
        match outcome {
            DamageRowsOutcome::Count(n) => {
                let rows = &buf[..n as usize];
                assert!(
                    rows.contains(&1u16),
                    "expected row 1 in dirty rows (old selection), got {rows:?}"
                );
            }
            DamageRowsOutcome::Full => {}
        }
    }

    /// A changed cursor hash (stale prev of 0) must dirty the cursor's viewport row even when
    /// alacritty only emits the cursor-cell hint, which compute_damage_rows filters out.
    #[test]
    fn cursor_state_change_marks_cursor_row_dirty() {
        let mut term = fixture_term();
        type_text(&mut term, "abc");
        // Pass 0 as prev_cursor_hash so the second call sees a change.
        let mut buf = [0u16; 16];
        let (_, off1, sel1, _) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Idle in alacritty, but the differing prev_cursor_hash must mark the cursor row dirty.
        let cursor_row = term.grid().cursor.point.line.0;
        assert!(cursor_row >= 0);
        let cursor_row_u16 = cursor_row as u16;
        let (outcome, _, _, _) = compute_damage_rows(&mut term, off1, sel1, 0, &mut buf);
        match outcome {
            DamageRowsOutcome::Count(n) => {
                let rows = &buf[..n as usize];
                assert!(
                    rows.contains(&cursor_row_u16),
                    "expected cursor row {cursor_row_u16} in dirty rows, got {rows:?}"
                );
            }
            DamageRowsOutcome::Full => {}
        }
    }

    /// Damage resets after each drain: write, drain, idle leaves Count(0).
    #[test]
    fn damage_drain_is_idempotent() {
        let mut term = fixture_term();
        type_text(&mut term, "first");
        let mut buf = [0u16; 16];
        let (_, off1, sel1, hash1) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        let (outcome, _, _, _) = compute_damage_rows(&mut term, off1, sel1, hash1, &mut buf);
        assert!(
            matches!(outcome, DamageRowsOutcome::Count(0)),
            "expected Count(0) on idempotent re-drain"
        );
    }

    /// Buffer cap overflow gives Full (cap=1 with at least two dirty rows).
    #[test]
    fn cap_overflow_promotes_to_full() {
        let mut term = fixture_term();
        type_text(&mut term, "a");
        Handler::linefeed(&mut term);
        Handler::carriage_return(&mut term);
        type_text(&mut term, "b");
        let mut buf = [0u16; 1];
        let (outcome, _, _, _) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Full either directly from alacritty or via cap overflow; both are correct.
        assert!(matches!(outcome, DamageRowsOutcome::Full));
    }
}

#[cfg(test)]
mod search_match_projection_tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line, Point};
    use std::sync::Mutex;

    fn make_state(start: (i32, usize), end: (i32, usize)) -> Mutex<SearchState> {
        let state = SearchState {
            current_match: Some(
                Point {
                    line: Line(start.0),
                    column: Column(start.1),
                }..=Point {
                    line: Line(end.0),
                    column: Column(end.1),
                },
            ),
            ..Default::default()
        };
        Mutex::new(state)
    }

    #[test]
    fn empty_match_returns_absent() {
        let state = Mutex::new(SearchState::default());
        let out = search_match_for_ffi(&state, 0, 24, 80);
        assert_eq!(out.present, 0);
    }

    #[test]
    fn match_in_live_viewport_projects_with_zero_offset() {
        // Grid line 5, cols 2..=7, no scroll.
        let state = make_state((5, 2), (5, 7));
        let out = search_match_for_ffi(&state, 0, 24, 80);
        assert_eq!(out.present, 1);
        assert_eq!(out.start_row, 5);
        assert_eq!(out.end_row, 5);
        assert_eq!(out.start_col, 2);
        assert_eq!(out.end_col, 7);
    }

    #[test]
    fn match_in_scrollback_with_user_scroll_appears_at_top() {
        // Grid line -3 with display_offset 5 gives viewport row 2.
        let state = make_state((-3, 0), (-3, 3));
        let out = search_match_for_ffi(&state, 5, 24, 80);
        assert_eq!(out.present, 1);
        assert_eq!(out.start_row, 2);
        assert_eq!(out.end_row, 2);
    }

    #[test]
    fn match_above_viewport_collapses_to_absent() {
        // Line -10 scrolled 3 gives viewport row -7: off-screen.
        let state = make_state((-10, 0), (-10, 3));
        let out = search_match_for_ffi(&state, 3, 24, 80);
        assert_eq!(out.present, 0);
    }

    #[test]
    fn match_below_viewport_collapses_to_absent() {
        // Line 30 with rows_count=24 is out of range.
        let state = make_state((30, 0), (30, 3));
        let out = search_match_for_ffi(&state, 0, 24, 80);
        assert_eq!(out.present, 0);
    }

    #[test]
    fn multi_row_match_clips_endpoints_to_viewport() {
        // Match spans line -2..=1. The off-screen start row makes the first VISIBLE row a "middle
        // row" of the wrap: start_col becomes 0, NOT the original column (10).
        let state = make_state((-2, 10), (1, 5));
        let out = search_match_for_ffi(&state, 0, 24, 80);
        assert_eq!(out.present, 1);
        assert_eq!(out.start_row, 0);
        assert_eq!(out.end_row, 1);
        assert_eq!(out.start_col, 0, "clipped start row should begin at col 0");
        // The end row is the original end, so end_col carries through.
        assert_eq!(out.end_col, 5);
    }

    #[test]
    fn multi_row_match_clipped_at_bottom_extends_to_viewport_right() {
        // Match spans line 22..=30 in a 24x80 viewport: end clips to row 23 and extends through
        // col 79.
        let state = make_state((22, 4), (30, 5));
        let out = search_match_for_ffi(&state, 0, 24, 80);
        assert_eq!(out.present, 1);
        assert_eq!(out.start_row, 22);
        assert_eq!(out.start_col, 4);
        assert_eq!(out.end_row, 23);
        assert_eq!(
            out.end_col, 79,
            "clipped end row should extend through last col"
        );
    }
}

#[cfg(test)]
mod clipboard_policy_tests {
    use super::*;

    /// OSC 52 `c` — the real clipboard — is stashed for the renderer to pick up.
    #[test]
    fn clipboard_store_is_pending_for_the_renderer() {
        let l = CopadListener::new();
        l.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "copied".into(),
        ));
        assert_eq!(
            l.pending_clipboard.lock().unwrap().take(),
            Some("copied".to_string())
        );
    }

    /// OSC 52 `p` (PRIMARY) is DROPPED, not aliased onto the pasteboard: macOS has no PRIMARY,
    /// so honoring it would let a program clobber the real clipboard (decisions #101).
    #[test]
    fn primary_selection_is_dropped_not_aliased() {
        let l = CopadListener::new();
        l.send_event(Event::ClipboardStore(
            ClipboardType::Selection,
            "primary".into(),
        ));
        assert_eq!(*l.pending_clipboard.lock().unwrap(), None);
    }

    /// Last write wins: a burst must not queue stale payloads ahead of the newest.
    #[test]
    fn later_clipboard_store_replaces_the_pending_one() {
        let l = CopadListener::new();
        l.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "old".into(),
        ));
        l.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "new".into(),
        ));
        assert_eq!(
            l.pending_clipboard.lock().unwrap().take(),
            Some("new".to_string())
        );
    }

    /// A PRIMARY write must not clear a CLIPBOARD write that is still waiting to be drained.
    #[test]
    fn primary_does_not_evict_a_pending_clipboard_store() {
        let l = CopadListener::new();
        l.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "keep".into(),
        ));
        l.send_event(Event::ClipboardStore(
            ClipboardType::Selection,
            "primary".into(),
        ));
        assert_eq!(
            l.pending_clipboard.lock().unwrap().take(),
            Some("keep".to_string())
        );
    }
}

#[cfg(test)]
mod osc52_parse_tests {
    use super::*;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;

    fn term_with(listener: CopadListener) -> Term<CopadListener> {
        Term::new(Config::default(), &TermSize::new(20, 5), listener)
    }

    /// Through the ANSI parser: a shell's `printf '\033]52;c;<b64>\a'` must land in
    /// `pending_clipboard`.
    #[test]
    fn osc52_clipboard_escape_reaches_pending_clipboard() {
        let listener = CopadListener::new();
        let mut term = term_with(listener.clone());
        let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::default();
        for b in b"\x1b]52;c;Y29wYWQ=\x07" {
            parser.advance(&mut term, &[*b]);
        }
        assert_eq!(
            listener.pending_clipboard.lock().unwrap().take(),
            Some("copad".to_string())
        );
    }

    /// Same escape targeting PRIMARY (`p`) is parsed and then dropped by the listener.
    #[test]
    fn osc52_primary_escape_is_dropped() {
        let listener = CopadListener::new();
        let mut term = term_with(listener.clone());
        let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::default();
        for b in b"\x1b]52;p;Y29wYWQ=\x07" {
            parser.advance(&mut term, &[*b]);
        }
        assert_eq!(*listener.pending_clipboard.lock().unwrap(), None);
    }
}

#[cfg(test)]
mod kitty_keyboard_tests {
    use super::*;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    #[derive(Clone, Default)]
    struct Replies(Arc<std::sync::Mutex<Vec<String>>>);

    impl EventListener for Replies {
        fn send_event(&self, event: Event) {
            if let Event::PtyWrite(s) = event {
                self.0.lock().unwrap().push(s);
            }
        }
    }

    fn term(kitty: bool) -> (Term<Replies>, Replies) {
        let replies = Replies::default();
        let config = Config {
            kitty_keyboard: kitty,
            ..Config::default()
        };
        (
            Term::new(config, &TermSize::new(20, 5), replies.clone()),
            replies,
        )
    }

    fn feed(term: &mut Term<Replies>, bytes: &[u8]) {
        let mut parser = Processor::<StdSyncHandler>::default();
        parser.advance(term, bytes);
    }

    /// The handshake Claude Code performs: query, then push disambiguate.
    #[test]
    fn negotiation_reaches_the_flags_the_encoder_reads() {
        let (mut t, replies) = term(true);
        feed(&mut t, b"\x1b[?u");
        assert_eq!(*replies.0.lock().unwrap(), vec!["\x1b[?0u".to_string()]);
        feed(&mut t, b"\x1b[>1u");
        assert_eq!(kitty_flags(*t.mode()), copad_keys::flags::DISAMBIGUATE);
        feed(&mut t, b"\x1b[>31u");
        assert_eq!(kitty_flags(*t.mode()), copad_keys::flags::ALL);
        feed(&mut t, b"\x1b[<u");
        assert_eq!(kitty_flags(*t.mode()), 1);
        feed(&mut t, b"\x1b[?1049h");
        assert_eq!(kitty_flags(*t.mode()), 0);
        feed(&mut t, b"\x1b[?1049l");
        assert_eq!(kitty_flags(*t.mode()), 1);
    }

    /// 0.26.0 evicted from the title stack at the 4097th push and panicked; copad builds the
    /// patched copy in vendor/alacritty_terminal.
    #[test]
    fn a_flood_of_pushes_evicts_from_the_keyboard_stack() {
        let (mut t, _) = term(true);
        let flood = b"\x1b[>1u".repeat(4200);
        feed(&mut t, &flood);
        feed(&mut t, b"\x1b[>5u");
        assert_eq!(kitty_flags(*t.mode()), 5);
    }

    #[test]
    fn turned_off_the_protocol_is_neither_answered_nor_applied() {
        let (mut t, replies) = term(false);
        feed(&mut t, b"\x1b[?u\x1b[>1u");
        assert!(replies.0.lock().unwrap().is_empty());
        assert_eq!(kitty_flags(*t.mode()), 0);
    }
}
