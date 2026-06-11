//! C-ABI bridge wrapping `alacritty_terminal::Term` + its PTY event
//! loop. Consumers are `copad-macos`'s renderer for now; the FFI is
//! deliberately host-agnostic so other UIs can attach later.
//!
//! See `docs/macos-renderer-migration-plan.md` §D3 for the ABI
//! contract and §Phase 2 for what's wired here.
//!
//! Pointer ownership:
//!
//! - `*mut CopadHandle` / `*mut CopadSnapshot` — heap allocations
//!   owned by Rust; free with the matching `_destroy` function
//!   exactly once. Passing NULL to `_destroy` is a no-op.
//! - Borrowed `*const CopadRun` / `*const u8` from snapshot
//!   accessors — valid until `copad_snapshot_destroy`.
//! - Static strings (`copad_term_version`) — valid for program
//!   lifetime, no free required.
//!
//! Threading: `Arc<FairMutex<Term>>` is shared between the PTY reader
//! thread (alacritty's `EventLoop`) and snapshot callers. Snapshots
//! lock briefly, copy out the visible rows, then release; renderers
//! consume them without holding the lock.

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
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options as TtyOptions, Pty, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Rgb};

/// Mirrors §D3 of the migration plan. `#[repr(C)]` so the layout is
/// stable across the FFI boundary. Per-cell allocation is avoided by
/// referencing into the row's contiguous utf8 buffer via
/// `utf8_offset` + `utf8_len`.
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
    /// 1-based index into the snapshot's `hyperlinks` vec; 0 means no
    /// OSC 8 link on this run. Renderer resolves the URI via
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

/// Cursor position + style reported by `copad_snapshot_cursor`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CopadCursor {
    pub row: u16,
    pub col: u16,
    pub style: u8, // 0=hidden 1=block 2=bar 3=underline
    pub blink: u8,
    pub _reserved: u16,
}

/// Active selection bounds reported by `copad_snapshot_selection`.
/// Both end_row and end_col are INCLUSIVE — alacritty's
/// `SelectionRange` is inclusive on both ends, and the Swift renderer
/// needs to honor that when painting the highlight (otherwise the
/// last selected cell goes unhighlighted, visible on any single-line
/// drag or word selection).
///
/// When `present == 0`, the other fields are meaningless. `is_block`
/// is 1 for `SelectionType::Block` selections (deferred for v1 — only
/// Simple / Semantic / Lines wired today), 0 otherwise.
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

/// Current in-terminal search match in viewport coordinates. Shape
/// mirrors `CopadSelectionRange` (minus `is_block`, which doesn't
/// apply to substring/regex matches). `present == 0` means no
/// active match — either the search was cleared or the most recent
/// `search_next` returned no hit. Multi-row matches (rare — happens
/// when the pattern crosses an autowrap) carry both endpoints.
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

/// Opaque heap-allocated UTF-8 byte buffer. Returned by FFI methods
/// that hand the caller a copy of terminal content (selection,
/// scrollback). Free with `copad_string_destroy` exactly once.
/// Pairing the destroy function with the type avoids the "ptr+len
/// without capacity" UB trap of raw `Vec<u8>` round-tripping.
pub struct CopadString {
    data: Box<[u8]>,
}

/// Custom `EventListener` for `alacritty_terminal::Term`. Captures
/// the events the renderer actually needs to react to (OSC 52
/// clipboard writes for now; OSC 52 reads, title changes, and bell
/// can land here later). Most events are dropped on purpose —
/// alacritty fires them frequently and our renderer doesn't need
/// most of them.
#[derive(Clone)]
struct CopadListener {
    /// Most-recent OSC 52 clipboard-store request. Single-slot is
    /// fine: bursts of OSC 52 are rare, and the renderer polls every
    /// vsync. Older pending requests get coalesced — matches the
    /// "last write wins" semantics most emulators have.
    pending_clipboard: Arc<std::sync::Mutex<Option<String>>>,
    /// Late-bound sender into the PTY write path. Used to forward
    /// `Event::PtyWrite` (terminal replies — DSR cursor position,
    /// DA terminal attributes, color queries, etc.) back to the
    /// child process. `None` until `copad_term_create` injects the
    /// sender after `EventLoop::channel()` — until then PtyWrite
    /// events are dropped, which is correct (no listener wired yet
    /// means no startup queries to answer).
    sender: Arc<std::sync::Mutex<Option<EventLoopSender>>>,
    /// Active palette for OSC 4 (`\e]4;n;?\e\\`) and OSC 10/11/12
    /// (foreground / background / cursor) color queries. Indexed by
    /// `vte::ansi::NamedColor as usize` — 0-15 = ANSI 8 normal + 8
    /// bright, 16-255 = 256-color cube, 256 = foreground, 257 =
    /// background, 258 = cursor. Entries the host hasn't populated
    /// (`copad_term_set_palette_entry`) make `ColorRequest` no-op
    /// instead of replying with a stale alacritty default — apps then
    /// fall back to their own defaults rather than getting a wrong
    /// answer.
    palette: Arc<std::sync::Mutex<HashMap<usize, Rgb>>>,
    /// One-shot latch: set when alacritty's EventLoop observes
    /// `Event::ChildExit` (the PTY child — typically the user's
    /// shell — terminated). Polled by the FFI's
    /// `copad_term_take_child_exit` so the renderer can broadcast
    /// `panel.exited` on the bus. Atomic + clear-on-take: a second
    /// poll after the first returns false even though the underlying
    /// signal is permanent.
    child_exited: Arc<std::sync::atomic::AtomicBool>,
}

/// Catppuccin Mocha defaults — same palette
/// `copad_core::theme::Theme::default()` exposes. Duplicated here as
/// `(index, r, g, b)` rows to avoid pulling copad-core into the
/// otherwise-lean copad-term crate. Used to pre-seed the palette
/// inside `CopadListener::new` so a child shell that emits an OSC 4
/// query in the race window between `copad_term_create` returning
/// and Swift calling `copad_term_set_palette_entry` still gets a
/// reasonable answer (Mocha colors) rather than no-reply. Host
/// override-on-theme-apply still wins.
#[rustfmt::skip]
const DEFAULT_PALETTE_ROWS: &[(usize, u8, u8, u8)] = &[
    // 0-7 normal ANSI
    (0,  0x45, 0x47, 0x5a),
    (1,  0xf3, 0x8b, 0xa8),
    (2,  0xa6, 0xe3, 0xa1),
    (3,  0xf9, 0xe2, 0xaf),
    (4,  0x89, 0xb4, 0xfa),
    (5,  0xf5, 0xc2, 0xe7),
    (6,  0x94, 0xe2, 0xd5),
    (7,  0xba, 0xc2, 0xde),
    // 8-15 bright
    (8,  0x58, 0x5b, 0x70),
    (9,  0xf3, 0x8b, 0xa8),
    (10, 0xa6, 0xe3, 0xa1),
    (11, 0xf9, 0xe2, 0xaf),
    (12, 0x89, 0xb4, 0xfa),
    (13, 0xf5, 0xc2, 0xe7),
    (14, 0x94, 0xe2, 0xd5),
    (15, 0xa6, 0xad, 0xc8),
    // NamedColor::Foreground / Background / Cursor (vte indices)
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
        }
    }

    fn set_sender(&self, sender: EventLoopSender) {
        *self.sender.lock().unwrap() = Some(sender);
    }

    /// Common path used by `Event::PtyWrite` + `Event::ColorRequest`
    /// to write bytes back to the child shell. Silent no-op when the
    /// sender hasn't been injected yet (extremely early-boot only —
    /// no query reaches us before `copad_term_create` returns).
    fn send_to_pty(&self, bytes: Vec<u8>) {
        if let Some(sender) = self.sender.lock().unwrap().as_ref() {
            let _ = sender.send(Msg::Input(bytes.into()));
        }
    }
}

impl EventListener for CopadListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::ClipboardStore(_kind, text) => {
                // Drop the previous pending request if any (last write
                // wins). The renderer takes it on the next tick via
                // `copad_term_take_clipboard_request`.
                *self.pending_clipboard.lock().unwrap() = Some(text);
            }
            Event::PtyWrite(reply) => {
                // alacritty_terminal already formatted the reply
                // (`\e[<row>;<col>R` for DSR 6n, `\e[?6c` for DA 0c,
                // etc.) — we just forward the bytes back to the child.
                // Without this hop nvim logs `"Did not detect DSR
                // response from terminal"` on startup and falls back
                // to slower paths.
                self.send_to_pty(reply.into_bytes());
            }
            Event::ColorRequest(index, format_reply) => {
                // OSC 4 / 10 / 11 / 12 — apps query "what color does
                // index N actually render as?" so they can pick
                // contrast (e.g. nvim's `&background=dark`). alacritty
                // hands us the formatter; we resolve the index against
                // the host-supplied palette and feed the result back
                // via the same Msg::Input path PtyWrite uses.
                //
                // No reply on miss: better than answering with a stale
                // alacritty default that doesn't match what we actually
                // draw on screen. Apps treat no-reply as "use my
                // built-in default".
                let rgb = self.palette.lock().unwrap().get(&index).copied();
                if let Some(rgb) = rgb {
                    let reply = format_reply(rgb);
                    self.send_to_pty(reply.into_bytes());
                }
            }
            Event::ChildExit(_status) => {
                // Shell exited. Flip the latched flag so the renderer
                // can poll-pull this on its next `displayLinkFired`
                // tick and broadcast `panel.exited` to the bus —
                // matching the cross-platform cleanup contract
                // (`copad-core::context` clears per-panel cwd / active
                // state on this event). Latch (vs. counter) is fine:
                // ChildExit fires at most once per Term lifetime.
                self.child_exited.store(true, Ordering::Relaxed);
            }
            _ => {
                // Title / Bell / MouseCursorDirty / TextAreaSizeRequest /
                // CursorBlinkingChange / Wakeup / Exit — intentionally
                // dropped; the renderer doesn't react to them today.
            }
        }
    }
}

struct Row {
    utf8: Vec<u8>,
    runs: Vec<CopadRun>,
}

pub struct CopadHandle {
    /// Shared between the PTY reader thread (alacritty's EventLoop)
    /// and snapshot callers. Lock duration must stay short on the
    /// snapshot path so the reader thread isn't starved.
    term: Arc<FairMutex<Term<CopadListener>>>,
    /// Listener clone we keep here so the FFI can poll for pending
    /// OSC 52 / future events without having to lock the term.
    listener: CopadListener,
    /// Sender into the event loop's mpsc — drives input writes,
    /// resize, and shutdown.
    sender: EventLoopSender,
    /// Reader thread that owns the PTY + parser loop. Joined in
    /// `copad_term_destroy` after sending `Msg::Shutdown`.
    io_thread: Option<JoinHandle<(EventLoop<Pty, CopadListener>, State)>>,
    /// PID of the PTY's child process (typically the user's login
    /// shell). Captured at handle construction so `copad_term_child_cwd`
    /// can query the kernel for the shell's current working directory
    /// without having to thread the Pty handle out of the alacritty
    /// EventLoop. Falls stale if the shell exits — `proc_pidinfo`
    /// returns an error in that case and the FFI returns NULL.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    child_pid: i32,
    /// Last observed hash of the cursor row's renderable content plus
    /// cursor metadata (style/blink/show). Used by
    /// `copad_term_take_damage` to catch three classes of changes the
    /// line-bounds filter would otherwise drop: (1) cursor-cell
    /// mutation collapsing to `(line, col, col)` damage — same shape
    /// as alacritty's unconditional `damage_cursor` hint; (2) zero-
    /// width combining marks (alacritty's `Term::input` skips
    /// `damage_point` on that branch); (3) DECSCUSR / `\e[?25l/h`
    /// cursor metadata transitions that produce zero grid damage.
    last_redraw_state_hash: AtomicU64,
    /// Last observed `Grid::display_offset()`. The per-row damage path
    /// uses this to detect scroll into / out of history — alacritty's
    /// damage tracking only fires on writes to the live region, but a
    /// scroll changes what every viewport row DISPLAYS. Diverging from
    /// the previous offset promotes the result to Full damage (every
    /// viewport row is different content).
    last_display_offset: AtomicI32,
    /// Last observed selection range, in viewport coordinates. Damage-
    /// rows callers need the union of (old ∪ new) so that a selection
    /// SHRINK or CLEAR repaints the rows that were highlighted last
    /// frame — `selection_range_for_ffi` only exposes the current
    /// range, so without remembering the previous we'd leave stale
    /// surface2 overlay painted on cells that are no longer selected.
    last_selection_range: Mutex<CopadSelectionRange>,
    /// In-terminal find state. `pattern`/`case_sensitive` cache the
    /// last compiled regex so back-to-back next/prev calls reuse the
    /// `RegexSearch` (build is non-trivial — alacritty constructs four
    /// lazy DFAs internally). `current_match` is the alacritty grid
    /// range of the most recent hit; `None` until the first
    /// `search_next` succeeds. All three are owned by the same Mutex
    /// so the snapshot path can read them atomically.
    search_state: Mutex<SearchState>,
}

/// Per-handle search bookkeeping. `regex` is the compiled
/// `RegexSearch` matching `pattern` under the `case_sensitive`
/// flag; we rebuild only when the user-facing query changes.
#[derive(Default)]
struct SearchState {
    regex: Option<alacritty_terminal::term::search::RegexSearch>,
    pattern: String,
    case_sensitive: bool,
    /// Most recent hit in alacritty grid coordinates (NOT viewport).
    /// Viewport mapping happens at snapshot time so a scroll into
    /// history correctly hides / repositions the highlight.
    current_match: Option<std::ops::RangeInclusive<Point>>,
}

pub struct CopadSnapshot {
    cols: u16,
    rows: Vec<Row>,
    cursor: CopadCursor,
    selection: CopadSelectionRange,
    /// Current in-terminal find match, projected into viewport
    /// coordinates the same way `selection` is. `present == 0` when
    /// no find is active or the active match scrolled out of view.
    search_match: CopadSearchRange,
    /// OSC 8 hyperlink URIs visible in this snapshot. The per-run
    /// `hyperlink_id` is 1-based index into this vec (0 = no link).
    /// Deduped by alacritty's `Hyperlink::id` so a hyperlink spanning
    /// many cells only stores its URI once.
    hyperlinks: Vec<String>,
}

/// Create a terminal handle: spawn a PTY running the requested shell
/// (or the user's `$SHELL`), construct an `alacritty_terminal::Term`
/// at the given size, hand both to an `EventLoop` running in a
/// dedicated thread. Returns NULL on shell-spawn failure (e.g. shell
/// path missing).
///
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
    let safe_cols = cols.max(1);
    let safe_rows = rows.max(1);

    let mut tty_opts = TtyOptions::default();
    // Stamp panel identity + GUI socket into the child shell's env.
    // Pair: `COPAD_PANEL_ID` lets the in-shell `copad-cwd` hook
    // route `chpwd` notifications to the right panel; `COPAD_SOCKET`
    // tells `coctl` which Unix socket to dial. Without the socket
    // env the hook short-circuits because `coctl` would otherwise
    // hit the well-known daemon path (not our per-instance GUI socket
    // owning the panel).
    if !panel_id.is_null() {
        // SAFETY: caller contract — panel_id, if non-null, is a NUL-
        // terminated UTF-8 string valid for the call.
        if let Ok(s) = unsafe { CStr::from_ptr(panel_id) }.to_str() {
            tty_opts.env.insert("COPAD_PANEL_ID".into(), s.to_owned());
        }
    }
    if !socket_path.is_null() {
        // SAFETY: caller contract.
        if let Ok(s) = unsafe { CStr::from_ptr(socket_path) }.to_str() {
            tty_opts.env.insert("COPAD_SOCKET".into(), s.to_owned());
        }
    }
    // Force a known-good TERM for the child shell. Without this we
    // inherit whatever the parent process had (e.g. `xterm-ghostty`
    // when Copad.app was launched from a Ghostty-bootstrapped GUI
    // session). Foreign terminfo entries that aren't in the macOS
    // system DB make zsh fall back through unrelated entries
    // (`'network': unknown terminal type` on this user's box) and
    // can mis-emit cursor / clear sequences, surfacing as duplicated
    // keystrokes on screen. xterm-256color is always present, gives
    // us 256-colour support, and is what every other terminal
    // emulator we ship alongside (alacritty / iterm2 / Terminal.app)
    // exports by default.
    tty_opts.env.insert("TERM".into(), "xterm-256color".into());
    // Match Ghostty: inject a default UTF-8 locale into the PTY child
    // when none was inherited. Copad.app launched from Finder /
    // Spotlight / Dock comes up with launchd's environment, which has
    // no LANG / LC_*. /etc/zprofile sets LANG=C.UTF-8 but only for
    // login shells — tmux pane shells and other non-login children
    // can therefore land in plain `C`, and tmux's per-client UTF-8
    // probe at attach time inherits whatever the launching shell had.
    // Ghostty injects LANG by default so its users never see this;
    // without the same injection here, Unicode glyphs (powerline,
    // Nerd Font icons, the Claude Code banner) get rendered as `_`
    // placeholders in non-login children. Skip when the parent already
    // specifies any locale so explicit user choice (launchctl setenv,
    // wrapper script) is preserved.
    if std::env::var_os("LANG").is_none()
        && std::env::var_os("LC_ALL").is_none()
        && std::env::var_os("LC_CTYPE").is_none()
    {
        tty_opts.env.insert("LANG".into(), "C.UTF-8".into());
    }
    if !shell.is_null() {
        // SAFETY: caller contract — non-null pointer is a NUL-terminated C string.
        if let Ok(s) = unsafe { CStr::from_ptr(shell) }.to_str() {
            tty_opts.shell = Some(Shell::new(s.to_owned(), Vec::new()));
        }
    }
    if !cwd.is_null()
        && let Ok(s) = unsafe { CStr::from_ptr(cwd) }.to_str()
    {
        tty_opts.working_directory = Some(PathBuf::from(s));
    }

    let window_size = WindowSize {
        num_lines: safe_rows,
        num_cols: safe_cols,
        // Cell pixel dims are only used by programs that query
        // `TIOCGWINSZ` for pixel dimensions (mostly image protocols
        // like sixel/kitty). 1×1 is safe for the headless scaffold;
        // the renderer will resize with real values once it's drawing.
        cell_width: 1,
        cell_height: 1,
    };

    let pty = match tty::new(&tty_opts, window_size, 0) {
        Ok(p) => p,
        Err(_) => return ptr::null_mut(),
    };
    // Capture child PID before handing pty to EventLoop. EventLoop
    // consumes pty (and reaps the child on Shutdown), but the PID we
    // capture here stays valid for `proc_pidinfo` queries until the
    // shell actually exits — at which point the syscall just returns
    // an error and `copad_term_child_cwd` returns NULL.
    let child_pid = pty.child().id() as i32;

    let term_size = TermSize::new(safe_cols as usize, safe_rows as usize);
    let listener = CopadListener::new();
    let term = Term::new(Config::default(), &term_size, listener.clone());
    let term = Arc::new(FairMutex::new(term));

    let event_loop = match EventLoop::new(Arc::clone(&term), listener.clone(), pty, false, false) {
        Ok(el) => el,
        Err(_) => return ptr::null_mut(),
    };
    let sender = event_loop.channel();
    // Late-bind the listener's reply path. `EventLoop::channel()` is
    // only available after `EventLoop::new` succeeds, so the listener
    // can't carry the sender at construction time. PtyWrite events
    // fired before this line (extremely unlikely — the event loop is
    // still on `spawn()`) silently drop, which is harmless because no
    // child shell has produced a query yet.
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
    }))
}

/// macOS only — query the PTY child's current working directory via
/// `proc_pidinfo(PROC_PIDVNODEPATHINFO)`. Returns a heap-allocated
/// NUL-terminated UTF-8 string the caller must free with
/// `copad_string_destroy`, or NULL if the shell has exited / the
/// syscall failed / the platform isn't macOS. Cheaper than parsing
/// OSC 7 from the PTY byte stream (which alacritty_terminal's vte
/// handler doesn't currently surface) and works even when the shell
/// doesn't emit OSC 7 at all — the kernel always knows the cwd of
/// any live process.
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
        // EPERM on un-entitled dev builds — `proc_pidinfo(
        // PROC_PIDVNODEPATHINFO)` requires either same audit token,
        // root, or `com.apple.private.security.proc-info` (Apple-
        // signed only). Falls through to caller's initialCwd fallback
        // until install-macos.sh embeds an entitlement plist.
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
    // `vip_path` is `[[c_char; 32]; 32]` — flatten then trim at the
    // first NUL. The kernel guarantees NUL-termination within
    // MAXPATHLEN (1024) bytes.
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

/// Free a handle. Sends `Msg::Shutdown`, joins the reader thread,
/// drops the Term. Safe to pass NULL.
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
    // Best-effort shutdown — if the reader already exited (e.g. PTY
    // child died), the send fails but join still cleans up.
    let _ = handle.sender.send(Msg::Shutdown);
    if let Some(jh) = handle.io_thread.take() {
        let _ = jh.join();
    }
}

/// Feed input bytes to the PTY. The reader thread picks them up via
/// the event-loop channel.
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

/// Resize the PTY + Term grid. `cell_width`/`cell_height` left at 1
/// since this FFI is headless; real pixel sizes land when a renderer
/// attaches.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
/// Populate one entry in the OSC color-query palette consulted by
/// `Event::ColorRequest` replies. Index follows `vte::ansi::NamedColor`:
/// 0-15 ANSI (normal + bright), 16-255 256-color cube, 256 foreground,
/// 257 background, 258 cursor. Re-calling for the same index overwrites
/// the previous value (use on theme hot-reload). Indices the host never
/// populates produce no reply on OSC 4 / 10 / 11 / 12 — apps then fall
/// back to their built-in defaults, which is safer than answering with
/// a color we don't actually draw.
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
    // SAFETY: caller contract — handle came from `copad_term_create`
    // and has not been freed.
    let h = unsafe { &*handle };
    h.listener
        .palette
        .lock()
        .unwrap()
        .insert(index as usize, Rgb { r, g, b });
    0
}

/// Resize the terminal grid. Clamps zero dimensions to 1 so the PTY
/// always sees a positive `winsize`. Pixel dimensions reuse the cell
/// count as a coarse approximation — alacritty only cares about the
/// cell grid for input handling; pixel-aware apps (`stty size`-style
/// queries) get column/row values and ignore the px fields.
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

    let ws = WindowSize {
        num_lines: safe_rows,
        num_cols: safe_cols,
        cell_width: 1,
        cell_height: 1,
    };
    // `Msg::Resize` only forwards to `pty.on_resize` (so the child
    // process sees SIGWINCH). The Term grid is a separate resize and
    // must be done explicitly under the term lock — alacritty's own
    // app does this in `WindowContext::on_resize`.
    let _ = h.sender.send(Msg::Resize(ws));
    let term_size = TermSize::new(safe_cols as usize, safe_rows as usize);
    h.term.lock().resize(term_size);
}

/// Query whether the terminal grid has been damaged since the last
/// call. Returns `true` if any cell changed; `false` if the grid is
/// byte-for-byte what the renderer already drew. Always resets the
/// internal damage state so the next call only sees what changed
/// AFTER this one.
///
/// The renderer's intended loop:
///   CADisplayLink tick → `copad_term_take_damage` → if false, skip;
///   if true, `copad_term_snapshot` + redraw.
///
/// Cursor-only filter: `alacritty_terminal::Term::damage` unconditionally
/// marks the current cursor cell on every call (a hint for renderers
/// that paint a blinking cursor). That makes the raw signal "always
/// damaged," which defeats the gate. We instead treat damage as "real"
/// only when it covers ANY cell other than exactly the cursor's
/// single-cell point — cursor *movement* still counts because the
/// damage region then includes the previous cursor cell, widening
/// beyond left==right==cursor.col.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `copad_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_term_take_damage(handle: *mut CopadHandle) -> bool {
    // Thin wrapper over `copad_term_take_damage_rows`: any non-zero
    // outcome (Full=-1 OR rows>0) means the renderer needs to repaint.
    // Single bookkeeping path so the two FFI entries can't disagree
    // about prev-state — callers MUST NOT call both in the same frame
    // (the rows variant advances `last_*` state too).
    let mut buf: [u16; 1] = [0];
    let result = unsafe { copad_term_take_damage_rows(handle, buf.as_mut_ptr(), buf.len() as u16) };
    result != 0
}

/// Outcome of the per-row damage drain. Separate from the FFI
/// signature so the test helper has a typed result, and so the
/// orchestration in [`copad_term_take_damage_rows`] doesn't recompute
/// "is full" from a magic integer.
enum DamageRowsOutcome {
    /// Whole viewport must redraw (display_offset shifted, or
    /// alacritty reported `TermDamage::Full`, or the dirty row count
    /// would exceed the caller's buffer cap).
    Full,
    /// Distinct viewport row indices that need repaint, written into
    /// the caller-provided buffer in unspecified order. `count` is in
    /// `0..=cap`.
    Count(u16),
}

/// Drain alacritty's per-row damage iterator into the caller's buffer,
/// merged with three damage sources alacritty's bounds filter can't
/// surface on its own:
/// * cursor-row content / cursor metadata (combining marks, DECSCUSR,
///   blink toggle) — caught via the `hash_redraw_state` mechanism;
/// * scrollback offset change — every viewport row maps to different
///   content, so we promote to `Full`;
/// * selection union (old ∪ new) — `selection_range_for_ffi` only
///   exposes the CURRENT range; without remembering the previous one
///   a SHRINK / CLEAR would leave stale `surface2` overlay painted on
///   rows no longer selected.
///
/// Extracted from the FFI shim so unit tests can drive a synthetic
/// `Term<VoidListener>` without standing up a real PTY EventLoop. The
/// caller threads in the previous-state values + receives the updated
/// ones in the return; the FFI shim writes them back to the handle's
/// atomic / mutex fields.
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

    // Scroll into / out of history — every viewport row's content
    // changes. No point computing per-row damage; the partial iterator
    // would only reflect cell writes since the last call (which lag the
    // viewport-display change), so we'd under-report and leave stale
    // pixels. Reset alacritty's damage so the next non-scrolling frame
    // starts clean.
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

    // Collect dirty rows into a small bitmap. u16 row indices in the
    // 0..=screen_lines range — a fixed 256-bit bitmap covers anything
    // reasonable a terminal would render in viewport rows. Out-of-range
    // returns `Full` (the caller's buffer can't hold the row list).
    let mut dirty_bitmap: [u64; 4] = [0; 4]; // 256 rows
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
                // Skip the cursor-cell-only damage hint alacritty emits
                // unconditionally on every `damage()` call. The cursor
                // case is handled by `cursor_state_changed` below, which
                // catches the cases that matter (movement, style change,
                // content mutation under the cursor).
                if d.line == cursor_line && d.left == cursor_col && d.right == cursor_col {
                    continue;
                }
                // alacritty grid line indices are NON-negative for the
                // live region (`damage()` only reports the live area —
                // scrollback writes don't fire damage events). Map to
                // viewport row via display_offset — since we already
                // bailed on offset change above, `current_offset` is
                // stable for the rest of this call.
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

    // Cursor row dirty (cursor moved / blinked / DECSCUSR / combining
    // mark). The cursor row's VIEWPORT index uses the same offset map
    // as snapshot rows + the FFI cursor row reported by snapshot, so a
    // user scrolled past the live cursor doesn't get a stray "row 0"
    // dirty.
    if cursor_state_changed {
        let cursor_viewport_row = cursor_point.line.0 + current_offset;
        if cursor_viewport_row >= 0 && (cursor_viewport_row as usize) < screen_lines {
            let _ = mark_row(&mut dirty_bitmap, cursor_viewport_row as usize);
        }
    }

    // Selection union: any row that WAS or IS in the highlight rect
    // needs repaint. Without the OLD union, shrinking the selection
    // would leave painted surface2 overlay on rows that no longer
    // belong to the range.
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

    // Flatten bitmap → caller's u16 array, writing distinct rows. Once
    // we run out of buffer cap, promote to Full — easier for the
    // renderer to handle than a "partial list" with unknown gaps.
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

/// Per-row damage drain — viewport row indices that need repaint
/// since the last call. Returns:
/// * `-1` — Full repaint required (scrollback offset changed,
///   alacritty signaled `TermDamage::Full`, or the dirty list exceeded
///   `cap`); the renderer should redraw the whole view.
/// * `0..=cap` — exact dirty row count. The first `count` `u16` slots
///   of `out_buf` hold distinct viewport row indices, in unspecified
///   order; rows beyond `count` are untouched.
///
/// Idempotent in the "no events fired since last call" case: returns
/// `0` and leaves the buffer untouched. Resets alacritty's damage
/// state at the end so the next call only sees subsequent changes.
///
/// # Safety
///
/// * `handle` must be NULL or a valid pointer returned by
///   `copad_term_create` and not yet destroyed.
/// * `out_buf` must point to writable storage for at least `cap`
///   `u16` slots when `cap > 0`. When `cap == 0` it MAY be null —
///   the function returns `-1` immediately (the buffer can't hold
///   any rows, equivalent to "promote to full").
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
        // No room to write rows — caller can't ever receive partial
        // damage. Drain the damage state anyway so the next non-zero-
        // cap call sees a clean slate.
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
    // SAFETY: caller contract — `out_buf` points to `cap` writable u16
    // slots. We hold the term lock for the duration so no concurrent
    // FFI call can race on the same buffer (the caller is expected to
    // be a single render thread on the GUI side).
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

/// Project `term.selection` (if any) into the FFI-friendly inclusive-
/// bounds struct the renderer paints from. Returns the default
/// (present=0) when there's no selection or it doesn't resolve to a
/// range (e.g. empty drag, viewport scrolled past the selection).
///
/// Generic over `EventListener` so the damage-rows computation can
/// run against the synthetic `Term<VoidListener>` used by unit tests
/// while the public FFI path stays on `Term<CopadListener>`.
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

/// Hash every renderable field the snapshot path will expose: cursor
/// row contents, cursor metadata (pos/style/blink/visibility), active
/// selection bounds, and scrollback offset. Used by
/// `compute_damage_rows` to catch grid-invisible state transitions
/// (DECSCUSR, selection start/extend/clear, blink toggle) and the
/// cursor-row content cases the line-bounds filter would otherwise
/// collapse with the unconditional `damage_cursor` hint.
///
/// Caller already holds the term lock — no extra synchronization
/// needed. Out-of-range cursor returns a fixed sentinel so the
/// comparison is still stable. Generic over `EventListener` so unit
/// tests can drive a `Term<VoidListener>`.
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

/// Hash every renderable field the snapshot path will expose: cursor
/// row contents, cursor metadata (pos/style/blink/visibility), and
/// active selection bounds. Used by `copad_term_take_damage` to
/// catch grid-invisible state transitions (DECSCUSR, selection
/// start/extend/clear) and the cursor-row content cases the line-
/// bounds filter would otherwise collapse with the unconditional
/// `damage_cursor` hint.
///
/// Take a snapshot of the visible viewport. Lock duration is bounded
/// by the time it takes to walk `rows × cols` cells and copy them
/// into the snapshot's owned buffers.
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

    // Hyperlink dedup map: `(id, uri)` → 1-based index into
    // `hyperlinks`. Keying by id alone is unsafe because explicit
    // OSC 8 ids ARE preserved by alacritty (only missing ids get
    // auto-generated unique values), so two distinct URIs can share
    // an id. Pairing them in the key keeps each URI its own slot.
    let mut hyperlinks: Vec<String> = Vec::new();
    let mut hyperlink_index_by_key: std::collections::HashMap<(String, String), u32> =
        std::collections::HashMap::new();

    // Viewport mapping: when the user has scrolled into history,
    // `display_offset > 0` and viewport row 0 maps to live line
    // `-display_offset`. alacritty's `Grid: Index<Line>` walks into
    // scrollback for negative line values directly, so the snapshot
    // ends up describing whatever the user is currently looking at.
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
    // `cursor_style()` honors DECSCUSR + vi-mode overrides; SHOW_CURSOR
    // gates whether anything renders. HollowBlock collapses to Block
    // here — the renderer draws hollow-on-blur as a separate concern
    // (window focus state, not a TUI request).
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
    // Cursor row needs the same display_offset mapping as the snapshot
    // rows. When the user has scrolled into history, the cursor may
    // sit outside the visible viewport — clamp the displayed style to
    // `0` (hidden) for those cases so we don't draw a stray block on
    // top of scrollback content. Position is still emitted (renderer
    // can clip however it likes); `style = 0` is the canonical
    // "don't draw" signal.
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

    drop(term);

    // Project the active find match into viewport coordinates. Locked
    // strictly AFTER the term lock is released so the global lock
    // order matches `copad_term_search_next` (search_state THEN term).
    // Without that ordering the two paths could AB-BA deadlock when
    // the renderer and find run concurrently. Stale match (rows
    // scrolled out of view) collapses to `present == 0`.
    let search_match = search_match_for_ffi(&h.search_state, display_offset, rows_count, cols);

    Box::into_raw(Box::new(CopadSnapshot {
        cols,
        rows: snapshot_rows,
        cursor,
        selection,
        search_match,
        hyperlinks,
    }))
}

/// Project the current find match (stored in alacritty grid coords)
/// into the viewport-coord range the renderer paints from. Returns
/// the empty range when there's no active match or when the match
/// sits entirely outside the visible viewport — same shape contract
/// as `selection_range_for_ffi`.
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
    // Clip rows to the visible range. When an endpoint is off-screen
    // the visible boundary row is logically a "middle row" of the
    // match — wrap-spanning matches paint the FULL row width on every
    // row except the original first/last. So if start clipped from a
    // negative line, the new first visible row should start at col 0
    // (its actual match content begins at the row's left edge from
    // the prior wrap, not at the off-screen start column). Same on
    // the end side: if end clipped from past last_row, the new last
    // visible row should extend through the last column.
    let (start_row_clamped, start_col_clamped) = if start_view < 0 {
        (0u16, 0u16)
    } else {
        (start_view as u16, range.start().column.0 as u16)
    };
    let (end_row_clamped, end_col_clamped) = if end_view > last_row {
        // Match extends past the bottom of the viewport — the new
        // last visible row is logically a "middle row" of the
        // original wrap, so it paints from col 0 through the
        // viewport's rightmost column. `cols - 1` is that index
        // (clamped to 0 for the degenerate cols == 0 case).
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

/// Walk a single display line into a `Row`. Groups consecutive cells
/// with identical attributes AND identical single-byte ASCII char into
/// one run so the renderer makes one CTLine per span instead of per
/// cell — the dominant cost on idle/scrollback frames where most cells
/// are spaces. The aggregation is intentionally conservative:
/// uniform-ASCII only (so the cursor-cell glyph re-render picks any
/// byte and gets the right char), no wide chars, no combining marks.
fn walk_row(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    line: Line,
    cols: u16,
    hyperlinks: &mut Vec<String>,
    hyperlink_index_by_key: &mut std::collections::HashMap<(String, String), u32>,
) -> Row {
    let mut utf8: Vec<u8> = Vec::new();
    let mut runs: Vec<CopadRun> = Vec::new();
    // Side-channel: for each pushed run, the ASCII byte every cell in
    // that run shares — or None if the run is non-uniform (multi-byte
    // char, combining marks, wide char, or mixed contents). Only
    // Some-valued entries can be extended by `try_extend_last_run`.
    let mut run_uniform: Vec<Option<u8>> = Vec::new();
    let mut col: u16 = 0;

    while col < cols {
        let point = Point::new(line, Column(col as usize));
        let cell = &grid[point];

        // Wide-char trailing cells (the "right half" of a CJK glyph
        // emitted alongside the leading half) carry no glyph and
        // shouldn't generate a run of their own — they're absorbed
        // by the leading half's run.
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
        // `cell.underline_color()` can return any AnsiColor variant
        // (Spec from `\e[58;2;…m`, Indexed from `\e[58;5;Nm`, or a
        // named palette color), so route it through the same encoder
        // as fg/bg instead of dropping non-Spec values.
        let underline_color = cell.underline_color().map(color_to_rgba).unwrap_or(0);
        let underline_style = if cell.flags.intersects(CellFlags::ALL_UNDERLINES) {
            // Phase 2 just exposes "1 = some underline"; richer
            // undercurl/dotted decoding lands with the renderer.
            1
        } else {
            0
        };
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

        // Aggregation eligibility: single-cell, ASCII char, no
        // combining marks. Multi-byte chars, wide chars, and cells
        // with combining marks each get their own run so cursor-cell
        // glyph extraction stays a simple "pick the run's bytes".
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
            // Extend the previous run by one column. utf8 stays
            // uniform because we appended the same byte.
            utf8.push(b);
            last.utf8_len += 1;
            last.end_col += 1;
            col += 1;
            continue;
        }

        let utf8_offset = utf8.len() as u32;
        let mut buf = [0u8; 4];
        utf8.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
        // Combining marks live in CellExtra.zerowidth — fold them
        // into the same run's utf8 so CoreText shapes them with
        // their base glyph.
        for combine in cell.zerowidth().unwrap_or(&[]) {
            utf8.extend_from_slice(combine.encode_utf8(&mut buf).as_bytes());
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
    // ALL_UNDERLINES covers single/double/curly/dotted/dashed; we
    // collapse to the UNDERLINE bit for Phase 2 (style enum at
    // `CopadRun::underline_style` carries the variant).
    if f.intersects(CellFlags::ALL_UNDERLINES) {
        out |= flags::UNDERLINE;
    }
    out
}

/// Encoding scheme for the `fg_rgba` / `bg_rgba` u32 fields. The high
/// byte is a tag that disambiguates three color kinds without growing
/// the ABI:
///
/// - `0x00_00_00_00` — default (renderer materializes to theme fg/bg).
/// - `0x01_00_00_NN` — indexed palette color (N in 0..255). Swift
///   resolves 0-15 from `theme.palette`, 16-231 from the 6×6×6 xterm
///   color cube, 232-255 from the 24-step grayscale ramp.
/// - `0xFF_RR_GG_BB` — direct RGB. Always opaque (alpha forced to 1
///   on decode) because terminal cells don't have a meaningful alpha.
///
/// Tag-based discrimination is required because the older "alpha=0
/// means indexed" trick ambiguated against RGB colors whose R channel
/// is 0 (`\\e[38;2;0;200;255m` and similar), which silently routed
/// them through the indexed path. Other tag values are reserved.
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

/// Map `NamedColor` variants the SGR parser hands us into ANSI
/// palette indices the Swift side already knows how to resolve. Keeps
/// the bright/dim variants honest (bright red is index 9, not 1) so
/// `printf '\033[91mhi'` actually renders bright. Returns `None` for
/// non-palette named colors (DimFg, Cursor, …) so the caller can fall
/// back to the default sentinel.
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

/// Free a snapshot.
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

/// Borrowed pointer to the row's run array. Valid until
/// `copad_snapshot_destroy`. Returns 0 if row is out of range;
/// `*out_runs` set to NULL in that case.
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

/// Borrowed pointer to the row's utf8 bytes + length. Same lifetime.
///
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

/// Fill `*out` with the snapshot's cursor state.
///
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

/// Fill `*out` with the snapshot's active selection bounds. Renderer
/// checks `out.present` to decide whether to paint a highlight.
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

/// Copy out the current find match in viewport coordinates. The
/// renderer reads this every frame so it can paint the highlight
/// underneath glyphs. `present == 0` means no active match — caller
/// should skip the highlight pass.
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

// ---------- In-terminal find ----------

/// Backslash-escape regex metacharacters so the FFI's substring
/// contract is honored. Mirrors the set the `regex` crate's
/// `regex::escape` function escapes (we don't pull in `regex` just
/// for this — alacritty uses `regex-automata` and we don't want to
/// bring another regex crate transitively).
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

/// Find the next/previous match of `pattern` in the terminal grid +
/// scrollback. Pattern is treated as a fixed string (special regex
/// chars are escaped by the caller — copad doesn't expose regex to
/// the find bar in v1).
///
/// `case_sensitive == false` wraps the pattern in `(?i:...)` so
/// alacritty's regex_automata engine matches without case. `forward`
/// chooses `regex_search_right` (next) vs `regex_search_left` (prev).
/// Search starts after the current match's end (forward) / before
/// its start (backward); with no current match it starts at the
/// term cursor — that's where the user is logically looking. Wraps
/// around the grid boundaries (`(start, end)` covers the full
/// scrollback + live region).
///
/// On a hit, updates the cached match + viewport-scrolls so the row
/// containing the match start is on-screen; returns `true`. On miss,
/// clears the cached match and returns `false`.
///
/// Cache reuse: if `(pattern, case_sensitive)` matches the last call,
/// the compiled `RegexSearch` is reused — alacritty builds four
/// lazy DFAs internally, so rebuilding on every keystroke would be
/// wasteful.
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
    // SAFETY: caller contract — pattern is a NUL-terminated UTF-8 string.
    let raw_pattern = match unsafe { CStr::from_ptr(pattern) }.to_str() {
        Ok(s) if !s.is_empty() => s,
        _ => {
            // Empty / invalid pattern → clear and return.
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

    // Substring semantics: escape regex metacharacters in the user's
    // input so `.` `*` `(` `[` etc. match literally. Without this a
    // user typing `foo.bar` would unexpectedly match `fooXbar`.
    let escaped = escape_regex_literal(raw_pattern);

    // Force case bias explicitly. Alacritty's RegexSearch::new applies
    // SMART-CASE by default (case-insensitive iff the pattern has no
    // uppercase), so `case_sensitive=true` on a lowercase query like
    // "foo" would still match "FOO" without the `(?-i:...)` override.
    // Wrap with the corresponding inline flag group so the user's
    // toggle is load-bearing in both directions.
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
                // Query changed — the previously-cached match is from
                // a different pattern and using it as the search
                // origin would skip a valid hit at the same location
                // (e.g. extending "abc" → "abcd" would step past
                // the old "abc" match and miss the "abcd" that
                // overlaps it). Reset so the next search starts at
                // the term cursor instead.
                state.current_match = None;
            }
            Err(_) => {
                // Invalid pattern (extremely rare for substring searches
                // after our `(?i:...)`/`(?-i:...)` wrap — would mean
                // unbalanced parentheses in user input). Bail without
                // changing current_match so the previous highlight stays.
                return false;
            }
        }
    }

    let mut term = h.term.lock();
    let cols = term.columns();
    let total_lines = term.total_lines();
    let topmost_line = term.topmost_line();
    let bottommost_line = term.bottommost_line();

    // Pick the search origin. With a cached match we step past its
    // boundary in the chosen direction; without one we start at the
    // term cursor so the first call after the user types "foo" finds
    // the nearest "foo" to where they were looking.
    let cursor_point = term.grid().cursor.point;
    let start_point = match (forward, state.current_match.as_ref()) {
        (true, Some(m)) => {
            // Step one cell past the match's end. If that overshoots
            // the bottom-right corner of the grid, wrap to the top.
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

    // Search bounds: full grid (live + scrollback), each direction.
    // `regex_search_*` take `&mut RegexSearch` — alacritty's lazy DFAs
    // mutate internal cache state as they evaluate. Take the regex
    // out of the Option for the duration of the search so we hold a
    // single `&mut`, then put it back.
    let mut regex = state
        .regex
        .take()
        .expect("regex set above when cache_hit was false");

    let found = if forward {
        // Forward search wraps: try from start_point to bottom; if
        // nothing, retry from top to start_point. alacritty doesn't
        // wrap natively, so we do two calls.
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
            // Wrap-around guard: don't return the same match twice in a
            // row if the only match in the grid is at start_point.
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
    // Hand the (now-mutated) regex back to the cache.
    state.regex = Some(regex);
    // total_lines is read but unused below if the closure-based
    // logic above changes — keep the binding to avoid an unused-let
    // warning that would block clippy.
    let _ = total_lines;

    state.current_match = found.clone();

    // Scroll viewport so the match row is visible. alacritty's
    // display_offset counts rows above the live region (0 = live
    // bottom). For a match on line `L` (where negative L = scrollback,
    // 0..screen_lines = live), the offset needed to bring L onto the
    // bottom row is `(screen_lines-1) - L` clamped to [0,
    // total_lines-screen_lines].
    if let Some(ref m) = found {
        let screen_lines = term.screen_lines() as i32;
        let match_line = m.start().line.0;
        let needed_offset = if match_line < 0 {
            // Match in scrollback — show it at the top of the viewport.
            (-match_line) as usize
        } else if match_line >= screen_lines {
            // Shouldn't happen (live region max is screen_lines-1),
            // but be defensive.
            0
        } else {
            0 // Match already in live viewport.
        };
        let max_offset = term.history_size();
        let final_offset = needed_offset.min(max_offset);
        let current_offset = term.grid().display_offset();
        if current_offset != final_offset {
            // alacritty exposes scroll via Term::scroll_display(Scroll).
            // ::Top jumps to start of scrollback, ::Bottom to live,
            // ::Lines(N) scrolls N lines (positive = into history).
            let delta = final_offset as i32 - current_offset as i32;
            if delta != 0 {
                term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
            }
        }
    }

    found.is_some()
}

/// Drop the cached regex + current match. The renderer's highlight
/// vanishes on the next frame.
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

// ---------- Selection control ----------

/// Selection kind discriminator for `copad_term_selection_start`.
/// Kept as named constants so Swift / future C consumers can mirror
/// the contract; the SIMPLE variant is consumed via the match's
/// fallback arm.
#[allow(dead_code)]
const SELECTION_SIMPLE: u8 = 0;
const SELECTION_SEMANTIC: u8 = 1;
const SELECTION_LINES: u8 = 2;
const SELECTION_BLOCK: u8 = 3;

/// Side discriminator: 0 = Left of cell, 1 = Right. Mirrors
/// alacritty's `Side` enum so the renderer can compute it from the
/// pixel offset within the cell (left half → Left, right half → Right)
/// without bringing the enum across FFI.
const SIDE_LEFT: u8 = 0;

fn parse_side(side: u8) -> Side {
    if side == SIDE_LEFT {
        Side::Left
    } else {
        Side::Right
    }
}

fn selection_point(term: &Term<CopadListener>, row: u16, col: u16) -> Point {
    // Renderer passes viewport-relative row coordinates (row 0 = top
    // of what's currently visible, regardless of scrollback). Convert
    // to alacritty's absolute Line by subtracting display_offset —
    // when scrolled back, viewport row 0 sits at Line(-display_offset).
    // Without this, drag/click on scrolled-back content selects the
    // live grid at the same row instead of what the user sees.
    let display_offset = term.grid().display_offset() as i32;
    let line = Line(row as i32 - display_offset);
    let cols = term.columns();
    let column = Column((col as usize).min(cols.saturating_sub(1)));
    Point::new(line, column)
}

/// Start a new selection at (`row`, `col`). Replaces any existing
/// selection. `kind` is `SELECTION_SIMPLE` / `SEMANTIC` / `LINES`;
/// anything else falls back to simple.
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

/// Extend the current selection to `(row, col, side)`. No-op if there
/// isn't a selection in progress.
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

/// Clear the active selection.
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

/// Select the entire visible viewport (Cmd+A). Uses a Simple
/// selection from (0, 0) to (last_line, last_col).
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
    // Select the currently-visible viewport — which, when the user is
    // scrolled into history, means scrollback lines. Top of viewport is
    // `Line(-display_offset)`; bottom is that plus `screen_lines - 1`.
    // (Selecting ALL of scrollback regardless of scroll position is a
    // different feature; matching iTerm2 / Terminal.app's Cmd+A: only
    // what the user can see.)
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

/// Heap-allocated UTF-8 buffer of the current selection. Returns NULL
/// when nothing is selected. Caller must free with
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

/// Render the last `lines` rows of scrollback as plain text — `\n`
/// between rows, no trailing newline. Mirrors SwiftTerm's
/// `Terminal.getLine(row: -N..0)` walk so coctl `terminal.history`
/// returns the same shape across both backends.
///
/// When the user has scrolled into history (`display_offset > 0`), the
/// result is still the N rows immediately above the *original*
/// viewport top — NOT shifted to follow the scroll. Callers that want
/// "the scrollback you can see right now" should use `terminal.read`.
///
/// Returns a non-NULL but length-0 `CopadString` when `lines == 0` or
/// when no scrollback exists, so the Swift wrapper can return an empty
/// `String` instead of `nil` and the JSON shape stays stable.
/// Caller must free via `copad_string_destroy` exactly once.
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

/// Walk `lines` rows of scrollback above the viewport top and join
/// cell `c` chars into a plain-text string, oldest row first.
/// Extracted from the FFI so unit tests can drive a synthetic Term
/// without spinning up a real PTY EventLoop — same pattern
/// `walk_row` / `selection_range_for_ffi` use.
///
/// Clamps `lines` to `grid.history_size()`; never panics on
/// out-of-range scrollback indices. NUL cells render as `' '`
/// (matches SwiftTerm's getLine extraction and the existing
/// snapshot row-decode behavior).
fn read_history_text(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    cols: usize,
    lines: usize,
) -> String {
    let take = lines.min(grid.history_size());
    if take == 0 || cols == 0 {
        return String::new();
    }
    // Capacity hint: one char per cell + one '\n' per row except the last.
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

/// Borrowed pointer to the string's bytes (NOT NUL-terminated).
/// `*out_len` receives the byte length. Both are valid until
/// `copad_string_destroy`.
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

/// Free a `CopadString`. NULL-safe.
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

/// True if any of alacritty's mouse-reporting modes is active. Used
/// by the renderer to defer to TUI mouse handlers (vim, less, htop,
/// tmux) instead of consuming the drag for selection — the renderer
/// only takes mouse events when this returns false OR the user holds
/// Shift to explicitly override.
///
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

/// Mouse-event encoding currently negotiated by the TUI. The renderer
/// uses this to choose the byte sequence for forwarded mouse events
/// (scroll wheel today, click/drag in a later phase). Encodings are
/// mutually exclusive on the term side, so this returns at most one.
///
///   0 = NONE (no mouse reporting active — do not forward)
///   1 = LEGACY (X10 `\e[M<cb><cc><cr>`, coords offset by 32, max 223)
///   2 = SGR    (`\e[<cb;cc;cr;{M|m}`, no coord cap — preferred)
///   3 = UTF8   (`\e[M<cb><cc><cr>` with multi-byte coord support)
///
/// SGR is what tmux, vim, and modern programs negotiate by default
/// alongside any reporting mode; LEGACY/UTF8 are fallbacks. The
/// reporting-mode bit (CLICK / DRAG / MOTION) must also be on for
/// this to return non-zero, matching `mouse_mode_active`.
///
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

/// Highest mouse-reporting level currently negotiated by the TUI.
/// The three xterm modes are mutually exclusive on the TUI side
/// (turning one on clears the others), so we collapse to a single
/// scalar instead of exposing the flag bitmask:
///
///   0 = NONE (no reporting; renderer keeps mouse for selection)
///   1 = CLICK   — `\e[?1000h` — press + release only
///   2 = DRAG    — `\e[?1002h` — adds motion ONLY while a button is held
///   3 = MOTION  — `\e[?1003h` — all motion, even with no buttons down
///
/// `MOTION` implies `DRAG` implies `CLICK` for forwarding purposes —
/// the renderer should always send press/release when any level is on,
/// drag events at DRAG/MOTION, and bare-cursor motion only at MOTION.
///
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
    // Highest active wins. alacritty_terminal masks the previous mode
    // when a new one is enabled (see TermMode::set_named_private_mode),
    // so in practice only one bit is set — but checking highest-first
    // keeps the renderer robust against any future overlap.
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

/// True if the terminal has bracketed paste mode enabled (`\e[?2004h`).
/// Renderer wraps Cmd+V'd text in `\e[200~ … \e[201~` when this is
/// true so paste-aware programs (zsh, neovim with `set paste`, etc.)
/// can distinguish pasted bytes from typed bytes.
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

/// Scroll-direction discriminator for `copad_term_scroll`. Mirrors
/// `alacritty_terminal::grid::Scroll` so the renderer doesn't have to
/// bring the enum across the FFI.
#[allow(dead_code)]
const SCROLL_DELTA: u8 = 0;
const SCROLL_PAGE_UP: u8 = 1;
const SCROLL_PAGE_DOWN: u8 = 2;
const SCROLL_TOP: u8 = 3;
const SCROLL_BOTTOM: u8 = 4;

/// Scroll the visible viewport. `kind` is one of `SCROLL_*`; `delta`
/// is only used for `SCROLL_DELTA` (lines; positive = older content
/// scrolls into view, negative = newer). Page / Top / Bottom ignore
/// `delta`.
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

/// Take the most-recent pending OSC 52 clipboard-store request (the
/// `\e]52;c;<base64>\a` sequence programs use to push text into the
/// system clipboard). Returns NULL if nothing is pending. Caller
/// frees the returned string with `copad_string_destroy` and gates
/// the actual NSPasteboard write on the user's `[security] osc52`
/// policy. Single-slot semantics: bursts coalesce to "last write
/// wins" — matches how VTE and iTerm2 handle the same case.
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

/// True iff the PTY child process (typically the user's shell) has
/// exited since the last call. Clears the latch on read so a second
/// poll after the first returns false. The renderer broadcasts
/// `panel.exited` on the event bus when this returns true; consumers
/// (copad-core ContextService, daemon subscribers) rely on that
/// event to clear per-panel cwd / active-doc state.
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

/// Number of distinct OSC 8 hyperlink URIs visible in this snapshot.
/// IDs handed back to the renderer in `CopadRun.hyperlink_id` are
/// 1-based indices in `[1, count]`; 0 means "no hyperlink".
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

/// Borrowed pointer to the URI bytes for the given 1-based hyperlink
/// id. Returns NULL + sets `*out_len = 0` when the id is out of
/// range. Lifetime matches the snapshot — copy out before calling
/// `copad_snapshot_destroy`.
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

    /// Regression test for the original bug: RGB colors with R=0 used
    /// to be mis-decoded as indexed (the high byte was 0, which the old
    /// Swift decoder read as "indexed palette"). Now they carry the
    /// 0xFF direct tag so the decoder can disambiguate.
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
        // Pure-black RGB stays distinguishable from "default" via the tag.
        assert_eq!(black, TAG_DIRECT);
        assert_ne!(black, 0);
    }

    #[test]
    fn named_unmappable_falls_back_to_default() {
        // DimFg, Cursor, etc. aren't in the 16-color palette; the
        // encoder collapses them to the default sentinel so the
        // renderer picks the theme foreground.
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::DimForeground)),
            0
        );
    }

    /// Underline color goes through the same encoder as fg/bg now, so
    /// `\e[58;5;Nm` (indexed) and `\e[58;2;…m` (direct) both round-trip
    /// through the renderer instead of the indexed branch silently
    /// becoming "use fg".
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
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::Handler;

    /// Build a Term wide enough for 4 columns and a 4-row viewport, with
    /// a generous scrollback buffer. Caller drives content via
    /// `<Term as Handler>::input` + `linefeed` / `carriage_return`.
    fn fixture_term() -> Term<VoidListener> {
        let size = TermSize::new(4, 4);
        // Bump scrollback so push_lines tests can exercise it without
        // tripping the default cap.
        let cfg = Config {
            scrolling_history: 32,
            ..Default::default()
        };
        Term::new(cfg, &size, VoidListener)
    }

    /// Push N rows, each labeled "rXX" left-aligned in the 4-col cells.
    /// Drives the parser via Handler::input + linefeed + carriage_return
    /// so the grid + scrollback wire up exactly as they would for live
    /// PTY bytes.
    fn push_lines<T: alacritty_terminal::event::EventListener>(term: &mut Term<T>, n: usize) {
        for i in 1..=n {
            // 4-col label: "r" + two-digit index + space. For N > 99 the
            // labels still fit because alacritty wraps at col 4 (we
            // explicitly avoid that case in tests).
            let label = format!("r{i:02} ");
            for ch in label.chars().take(4) {
                Handler::input(term, ch);
            }
            Handler::linefeed(term);
            Handler::carriage_return(term);
        }
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
        // Push 6 rows into a 4-row viewport. Linefeed after the last input
        // leaves the cursor on the empty bottom row, so the live viewport
        // holds rows {r04, r05, r06, ""} and scrollback ends up with
        // {r01, r02, r03} = 3 rows.
        push_lines(&mut term, 6);
        let history_size = term.grid().history_size();
        assert_eq!(
            history_size, 3,
            "expected 3 scrollback rows from 6 pushes in a 4-row viewport"
        );
        let text = read_history_text(term.grid(), term.columns(), 100);
        // 3 rows joined by 2 newlines, no trailing newline.
        assert_eq!(text.matches('\n').count(), 2);
        let row_count = text.split('\n').count();
        assert_eq!(row_count, 3);
    }

    #[test]
    fn returns_oldest_first_and_excludes_viewport() {
        let mut term = fixture_term();
        // 10 pushes into a 4-row viewport. Cursor ends on the empty
        // bottom row, viewport holds {r08, r09, r10, ""} and scrollback
        // contains r01..r07. Last 3 scrollback rows = r05, r06, r07.
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
        // Viewport content must not bleed in.
        assert!(!text.contains("r08"));
        assert!(!text.contains("r10"));
    }

    #[test]
    fn nul_cells_render_as_space() {
        let mut term = fixture_term();
        // Single-char row leaves cols 1..3 as NUL cells in scrollback.
        Handler::input(&mut term, 'X');
        Handler::linefeed(&mut term);
        Handler::carriage_return(&mut term);
        // Push more rows so "X" gets pushed into scrollback.
        push_lines(&mut term, 5);
        let text = read_history_text(term.grid(), term.columns(), 6);
        // The very first scrollback row (oldest) is the bare "X" row.
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

    /// First call after any writes must report the damaged rows (the
    /// shell prompt etc.) — alacritty starts every Term with
    /// TermDamage::Full pending until first reset_damage, which our
    /// path consumes. We assert the Full→Count transition is sane.
    #[test]
    fn initial_full_damage_drains_to_full_or_rows() {
        let mut term = fixture_term();
        type_text(&mut term, "hi");
        let mut buf = [0u16; 16];
        let (outcome, _, _, _) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Either Full or some rows — but not Count(0): the writes
        // should produce SOME damage signal.
        match outcome {
            DamageRowsOutcome::Full => {}
            DamageRowsOutcome::Count(n) => assert!(n >= 1, "expected >=1 dirty row, got 0"),
        }
    }

    /// After a clean drain, an idle term reports zero dirty rows. The
    /// cursor-cell-only damage hint alacritty re-asserts on every
    /// `damage()` call must be filtered out — otherwise an idle
    /// terminal would force a redraw every vsync.
    #[test]
    fn idle_terminal_reports_zero_dirty_rows() {
        let mut term = fixture_term();
        type_text(&mut term, "x");
        // Prime: consume initial damage.
        let mut buf = [0u16; 16];
        let (_, off1, sel1, hash1) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Now idle: re-call with the updated prev state.
        let (outcome, _, _, _) = compute_damage_rows(&mut term, off1, sel1, hash1, &mut buf);
        match outcome {
            DamageRowsOutcome::Count(0) => {}
            DamageRowsOutcome::Count(n) => panic!("expected 0 dirty rows on idle, got {n}"),
            DamageRowsOutcome::Full => panic!("expected 0 dirty rows on idle, got Full"),
        }
    }

    /// Cap=0 → Full (caller has no room to receive partial damage,
    /// so we drain alacritty's state and signal a full repaint).
    #[test]
    fn zero_cap_returns_full_and_drains_damage() {
        let mut term = fixture_term();
        type_text(&mut term, "data");
        let mut buf = [0u16; 0];
        let (outcome, _, _, _) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        assert!(matches!(outcome, DamageRowsOutcome::Full));
    }

    /// Scrollback offset change → Full. Even if the live region had
    /// no writes, every viewport row maps to different content.
    /// Drive this by pushing enough rows to fill scrollback, then
    /// passing a stale prev_display_offset.
    #[test]
    fn display_offset_change_promotes_to_full() {
        let mut term = fixture_term();
        for i in 0..6 {
            type_text(&mut term, &format!("r{i}"));
            Handler::linefeed(&mut term);
            Handler::carriage_return(&mut term);
        }
        // Drain initial damage.
        let mut buf = [0u16; 16];
        let (_, off1, sel1, hash1) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Now claim the previous offset was different — simulates a
        // scroll-back action having happened between calls.
        let (outcome, _, _, _) = compute_damage_rows(&mut term, off1 + 1, sel1, hash1, &mut buf);
        assert!(matches!(outcome, DamageRowsOutcome::Full));
    }

    /// Selection union: a previously-present selection on row 1 that
    /// has since cleared must still report row 1 dirty so the
    /// surface2 overlay gets repainted off the cells. Simulates the
    /// "shrink / clear" path codex round-1 C1 flagged.
    #[test]
    fn old_selection_rows_remain_dirty_after_clear() {
        let mut term = fixture_term();
        type_text(&mut term, "data");
        let mut buf = [0u16; 16];
        // Prime: cleanly drained, no selection.
        let (_, off1, _, hash1) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Build a synthetic "prev selection on row 1, cols 0..=3" that
        // the renderer would have painted last frame.
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
        // Current term has no selection — new_sel.present == 0 — but
        // the union must include row 1 from the prev selection.
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

    /// Cursor metadata change (we synthesize via a stale prev_cursor_hash
    /// of 0) must report the cursor's viewport row dirty even when
    /// alacritty's per-row iterator only emits the cursor-cell hint
    /// (which compute_damage_rows filters out).
    #[test]
    fn cursor_state_change_marks_cursor_row_dirty() {
        let mut term = fixture_term();
        type_text(&mut term, "abc");
        // Prime to clear initial damage but DON'T cache hash —
        // pass 0 as prev_cursor_hash so the second call sees a change.
        let mut buf = [0u16; 16];
        let (_, off1, sel1, _) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Re-call: alacritty has nothing in its damage iter (idle),
        // but our prev_cursor_hash differs from current → mark cursor
        // row dirty.
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

    /// Damage tracking resets after each drain: a write → drain → idle
    /// cycle leaves Count(0) on the idle call. Guards against the
    /// "reset_damage not called" regression.
    #[test]
    fn damage_drain_is_idempotent() {
        let mut term = fixture_term();
        type_text(&mut term, "first");
        let mut buf = [0u16; 16];
        let (_, off1, sel1, hash1) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Same prev state immediately reused: nothing changed since
        // last drain, expect Count(0).
        let (outcome, _, _, _) = compute_damage_rows(&mut term, off1, sel1, hash1, &mut buf);
        assert!(
            matches!(outcome, DamageRowsOutcome::Count(0)),
            "expected Count(0) on idempotent re-drain"
        );
    }

    /// Buffer cap overflow → Full. Verify by claiming cap=1 with at
    /// least two distinct dirty rows queued via writes that span them.
    #[test]
    fn cap_overflow_promotes_to_full() {
        let mut term = fixture_term();
        // Two distinct rows: write row 0, advance to row 1, write more.
        type_text(&mut term, "a");
        Handler::linefeed(&mut term);
        Handler::carriage_return(&mut term);
        type_text(&mut term, "b");
        let mut buf = [0u16; 1];
        let (outcome, _, _, _) = compute_damage_rows(&mut term, 0, empty_sel(), 0, &mut buf);
        // Either Full directly from alacritty (initial-state path),
        // or via cap overflow. Both are correct.
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
        // Match on grid line 5, cols 2..=7. No scroll → display_offset=0.
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
        // Match on grid line -3 (3 rows into scrollback). User scrolled
        // back 5 rows (display_offset = 5). Viewport row should be
        // -3 + 5 = 2.
        let state = make_state((-3, 0), (-3, 3));
        let out = search_match_for_ffi(&state, 5, 24, 80);
        assert_eq!(out.present, 1);
        assert_eq!(out.start_row, 2);
        assert_eq!(out.end_row, 2);
    }

    #[test]
    fn match_above_viewport_collapses_to_absent() {
        // Match on line -10, user only scrolled 3 → viewport row = -7.
        // Off-screen, present=0.
        let state = make_state((-10, 0), (-10, 3));
        let out = search_match_for_ffi(&state, 3, 24, 80);
        assert_eq!(out.present, 0);
    }

    #[test]
    fn match_below_viewport_collapses_to_absent() {
        // Match on line 30, rows_count=24 → viewport row 30, out of range.
        let state = make_state((30, 0), (30, 3));
        let out = search_match_for_ffi(&state, 0, 24, 80);
        assert_eq!(out.present, 0);
    }

    #[test]
    fn multi_row_match_clips_endpoints_to_viewport() {
        // Match spans line -2..=1 (4 rows). Viewport (24 rows × 80 cols,
        // no scroll): start clamps to row 0 (was -2), end stays at row 1.
        // The original start row (-2) is off-screen, so the new first
        // VISIBLE row is logically a "middle row" of the wrap — start_col
        // becomes 0 (paints from the left edge through the continuation
        // wrap), NOT the original off-screen start column (10).
        let state = make_state((-2, 10), (1, 5));
        let out = search_match_for_ffi(&state, 0, 24, 80);
        assert_eq!(out.present, 1);
        assert_eq!(out.start_row, 0);
        assert_eq!(out.end_row, 1);
        assert_eq!(out.start_col, 0, "clipped start row should begin at col 0");
        // End row IS the original end (row 1, in-viewport), so its
        // end_col carries through unchanged.
        assert_eq!(out.end_col, 5);
    }

    #[test]
    fn multi_row_match_clipped_at_bottom_extends_to_viewport_right() {
        // Match spans line 22..=30 (10 rows). Viewport (24 rows × 80
        // cols): start stays at row 22 with its real start col, end
        // clips to row 23 with end_col extended through col 79 (the
        // viewport's rightmost column).
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
