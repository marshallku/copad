//! The thin mux **client**: connects to the server (spawning one if none is
//! running), forwards key/resize events, and blits the server's cell frames to the
//! local terminal. Detaching (`Ctrl-b d`) or losing the connection just exits the
//! client — the server + shells live on. This is what `copad-mux` (bare) runs.

use std::io::{self, BufRead, BufReader, Stdout, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::cursor::{self, MoveTo, RestorePosition, SavePosition};
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
    Event as CEvent, KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Position, Rect as RRect};
use ratatui::{Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr;

use crate::control::socket_path;
use crate::proto::{ClientMsg, MouseKind, ServerMsg};

/// Re-derive wide-char spacer cells so a client buffer (built from wire deltas that omit the
/// trailing half of every wide glyph) matches the server's composed buffer exactly. For each
/// row: the cell after a width≥2 symbol becomes a blank `skip` spacer; every other cell has its
/// `skip` cleared. Using the SAME width function ratatui uses for its emit keeps the buffer
/// self-consistent with how ratatui will render it — the fix for stale wide glyphs desyncing
/// the row (see term.rs `relay_fidelity_pure_delta_churn`).
pub(crate) fn fix_wide_spacers(buf: &mut ratatui::buffer::Buffer) {
    let (w, h) = (buf.area.width, buf.area.height);
    for y in 0..h {
        let mut prev_wide = false;
        for x in 0..w {
            let Some(cell) = buf.cell_mut(Position::new(x, y)) else {
                continue;
            };
            if prev_wide {
                cell.set_symbol(" ");
                cell.set_skip(true);
                prev_wide = false;
            } else {
                cell.set_skip(false);
                prev_wide = UnicodeWidthStr::width(cell.symbol()) >= 2;
            }
        }
    }
}

/// A blank carrying `c`'s colours — used to erase a cell before writing a glyph that might not
/// fit it. Keeps fg/bg/attributes so erasing does not flash the default background through a
/// coloured region.
fn blank_like(c: &Cell) -> Cell {
    let mut b = Cell::EMPTY;
    b.set_symbol(" ");
    b.fg = c.fg;
    b.bg = c.bg;
    b.modifier = c.modifier;
    b
}

/// Must this frame state the cursor's visibility, given what we last set it to (`None` = we have
/// never set it) and what it should be?
///
/// Only on a change — except that "never set" counts as a change, because the alternate screen
/// INHERITS whatever visibility the shell left behind. Treating unknown as shown is how a cursor
/// hidden before comux started stays hidden for the whole session.
fn cursor_visibility_needs_stating(shown: Option<bool>, want: bool) -> bool {
    shown != Some(want)
}

/// Must the paint baseline be thrown away this frame?
///
/// `painted` is only a useful diff baseline while it describes what is actually ON the terminal.
/// Three things invalidate it, and the middle one is easy to miss: a `full` frame clears the
/// screen; a RESIZE clears it too (`Terminal::resize` does), even when the size ends up back
/// where it started — a queued burst of resize events can read A -> B -> A within one input
/// drain, and comparing the final size against `painted.area` sees no change while the screen
/// has been blanked; and a view of a different size obviously cannot be compared cell for cell.
/// Miss any of them and the next delta skips cells the terminal no longer has.
fn baseline_is_stale(cleared: bool, resized: bool, painted: RRect, view: RRect) -> bool {
    cleared || resized || painted != view
}

/// Map the server's frame onto a viewport-sized ratatui buffer: the frame's cells at the
/// top-left, blanks in the letterbox margin, and a blank in place of any glyph that would spill
/// past the right edge.
///
/// The frame is composed at the SMALLEST attached client's size (tmux-style shared view), so it is
/// routinely a different size from this terminal, in either direction:
///
///  * SMALLER — the ordinary case. The remainder is letterbox margin and must be blanked, or a
///    margin cell the terminal lost would never come back.
///  * BIGGER — the window after this terminal shrinks, since ratatui is resized at once while the
///    server's frame catches up a tick later. Emitting the frame would address columns and rows
///    past the edge.
///
/// And in that second window a TWO-COLUMN glyph can sit in the viewport's LAST column with its
/// spacer outside, so printing it sends its second half off the edge — which the terminal answers
/// by wrapping, and on the bottom row by scrolling the whole screen. Checking the glyph's starting
/// coordinate is not enough; its WIDTH has to fit. The composition never does this to itself (a
/// pane's last column can only hold a one-column symbol — see `term.rs::snapshot_grid`), which is
/// why it only shows up across a resize.
///
/// The single definition of that mapping, shared by the client's draw and its silent repaint so
/// the two cannot disagree, and used by the render-fidelity harness for the same reason.
pub(crate) fn blit_view(src: &Buffer, out: &mut Buffer) {
    let area = out.area;
    for y in 0..area.height {
        for x in 0..area.width {
            let Some(d) = out.cell_mut(Position::new(x, y)) else {
                continue;
            };
            match src.cell(Position::new(x, y)) {
                Some(sc) if x + 1 < area.width || UnicodeWidthStr::width(sc.symbol()) < 2 => {
                    *d = sc.clone();
                }
                // Letterbox margin, or a glyph with nowhere to put its second column.
                _ => {
                    d.reset();
                }
            }
        }
    }
}

/// Re-emit every cell of `src` straight to the terminal, bypassing ratatui's incremental
/// diff — the silent full repaint behind [`crate::proto::FrameMsg::repaint`].
///
/// Deliberately NOT `Terminal::clear` + draw. Clearing first flashes a blank frame, and that
/// flicker — not its cost — is the whole reason the periodic self-heal had to ship disabled.
/// Overwriting every cell with the content ratatui already believes is on screen is invisible,
/// and it is precisely what heals a divergence between that belief and the real terminal: an
/// outer emulator with its own damage tracking, a lossy link, or ratatui's wide-glyph
/// suppression leaking across a row/pane boundary in its flat buffer. Once ratatui's cached
/// buffer has drifted, the stale cell is "unchanged" forever and no delta will ever repaint it.
///
/// `skip` cells are left out, the same rule `Buffer::diff` applies: they are the trailing half
/// of a wide glyph, already covered by printing the leading cell, and printing over one splits
/// the glyph. ratatui's cached buffers are deliberately untouched — the content did not change,
/// so its belief stays accurate and the next delta is still correct.
/// Blit a server frame into `view` and emit every cell of it — what the paint path does when it
/// has no trustworthy baseline (after a clear, or for the self-heal repaint).
///
/// Production reaches this by passing `None` to [`emit_view`] directly; this is the composition
/// of the two production functions, kept for the tests and the render-fidelity harness so they
/// exercise the real `blit_view` + `emit_view` rather than a restatement of them. Both prior
/// harness bugs in this area came from modelling the client instead of calling it.
#[cfg(test)]
pub(crate) fn repaint_all<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    src: &Buffer,
    view: (u16, u16),
    cursor: Option<(u16, u16)>,
) -> io::Result<()> {
    let (vw, vh) = view;
    let mut frame = Buffer::empty(RRect::new(0, 0, vw, vh));
    blit_view(src, &mut frame);
    emit_view(terminal, None, &frame, cursor)
}

/// Emit `cur` to the terminal — every cell when `prev` is `None` (the silent full repaint), or
/// just the cells that differ from `prev` (an ordinary frame).
///
/// This is the client's whole paint path, replacing `Terminal::draw`. comux used to stack TWO
/// incremental diffs: the server shipped `c.last.diff(&composed)`, and the client applied that to
/// its mirror and then let ratatui diff AGAIN to decide what to write. Collapsing them was
/// deferred in decisions #124 until a divergence showed up. One did, and it is visible: the two
/// paths POSITION cells differently.
///
/// `CrosstermBackend::draw` omits the `MoveTo` between cells it believes are adjacent, so a run
/// is placed by the terminal's own cursor advance — it trusts the terminal to have measured every
/// glyph exactly as `unicode-width` did. The repaint has to re-anchor (one Nerd Font icon would
/// otherwise skew a whole row, and the repaint would re-apply that skew on every tick). While the
/// incremental path did not, the two disagreed about where the same text goes — so output landed
/// in one place and the next repaint SNAPPED it somewhere else. Text appearing and then jumping
/// sideways is not a cosmetic artefact of the self-heal; it is two renderers arguing.
///
/// Now there is one renderer. Diffing here also drops ratatui's `Buffer::diff`, whose wide-glyph
/// suppression runs over a FLAT buffer and therefore leaks across row and pane boundaries in a
/// composed screen — the hazard `term.rs::snapshot_grid`'s spacer invariant had to work around.
/// This diff is per-cell and row-bounded, and it ships a wide glyph's leading cell (which covers
/// both columns) while never printing over a spacer.
pub(crate) fn emit_view<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    prev: Option<&Buffer>,
    cur: &Buffer,
    cursor: Option<(u16, u16)>,
) -> io::Result<()> {
    let (vw, vh) = (cur.area.width, cur.area.height);
    // Emit in SEGMENTS, cutting after any cell whose glyph is not plain ASCII.
    //
    // `CrosstermBackend::draw` omits the `MoveTo` between cells it believes are adjacent, so a run
    // is positioned entirely by the terminal's OWN cursor advance — it trusts that the terminal
    // measured every glyph the way `unicode-width` did. That is tolerable for the incremental
    // path, whose runs are short and re-anchored constantly by the next diff. It is not tolerable
    // here: a full repaint makes each row ONE run, so a single glyph the outer terminal measures
    // differently shifts everything after it in that row — and because this repaint repeats on a
    // timer, it would re-apply that shift every few seconds instead of being overwritten by the
    // next delta. Turning a one-cell artefact into a permanently skewed row is the opposite of a
    // self-heal.
    //
    // The everyday case is a Nerd Font / Powerline icon in a shell prompt: Private Use Area, one
    // column by `unicode-width`, two in plenty of fonts. comux cannot see that disagreement — it
    // lives in the outer terminal's font, not in any table we can read, which is also why the
    // `app_render_fidelity` fuzz cannot catch it (its reference emulator shares alacritty's width
    // table). So do not try to predict it: re-anchor after every glyph that could carry it.
    // Starting a new `draw` call resets its cursor tracking, so each segment opens with a
    // `MoveTo` — the damage is bounded to the one cell, and the next repaint CORRECTS the drift
    // instead of entrenching it. ASCII needs no anchor; every terminal agrees it is one column.
    // Owned cells, because the last-column guard below has to emit a blank that exists in no
    // buffer. One small clone per EMITTED cell; a delta emits few, and the full repaint that
    // emits many already allocates its frame.
    let mut segments: Vec<Vec<(u16, u16, Cell)>> = Vec::new();
    let mut seg: Vec<(u16, u16, Cell)> = Vec::new();
    for y in 0..vh {
        // Set after emitting a glyph that could occupy MORE columns than we think, so the next
        // cell we would otherwise have skipped is repainted. Scoped to the row because the client
        // runs with autowrap OFF (see `TermGuard::enter`): an overflow at the last column is
        // clamped, not wrapped into the next row, so the damage stays inside its row.
        let mut repair_next = false;
        for x in 0..vw {
            let Some(c) = cur.cell(Position::new(x, y)) else {
                continue;
            };
            // Never print over a wide glyph's trailing half — the glyph before it already covers
            // that column. (Skipping it also means `repair_next` survives to the first column we
            // believe the glyph does NOT cover, which is the one that can have been clobbered.)
            if c.skip {
                continue;
            }
            // An unchanged cell is not re-sent on an ordinary frame. `prev` is what we believe is
            // ON THE TERMINAL, not what the server believes we hold, so this cannot inherit the
            // server's baseline drifting away from reality.
            let unchanged = prev.is_some_and(|p| p.cell(Position::new(x, y)) == Some(c));
            if unchanged && !repair_next {
                continue;
            }
            repair_next = false;
            let plain_ascii = c.symbol().len() == 1 && c.symbol().is_ascii();
            // The LAST column cannot be repaired after the fact. With autowrap off a terminal
            // that measures this glyph as two columns does not clamp it — it declines to write
            // it at all (alacritty's `Handler::input` returns early), so whatever was there
            // stays. `painted` would then record the glyph we never managed to draw and skip the
            // cell forever, and a repaint could not remove it either. Erasing first makes the
            // outcome independent of what was on screen: either the glyph lands, or the column
            // is blank — the same thing a fresh paint would show, which is the property this
            // whole emitter exists to guarantee.
            if !plain_ascii && x + 1 == vw {
                segments.push(vec![(x, y, blank_like(c))]);
            }
            seg.push((x, y, c.clone()));
            if !plain_ascii {
                // Cut the run, so the NEXT cell is addressed with a `MoveTo` instead of being
                // placed by the terminal's own cursor advance...
                segments.push(std::mem::take(&mut seg));
                // ...and repaint that cell even if it did not change. Anchoring only fixes where
                // the next emitted cell GOES; it does not undo what this glyph overwrote getting
                // there. A one-column cell that becomes a Nerd Font icon the outer terminal draws
                // two columns wide clobbers its neighbour, and if the neighbour is unchanged the
                // delta would leave it clobbered until the next self-heal repaint restored it —
                // which is precisely the "text lands, then jumps" the single emitter exists to
                // remove. Repainting it here keeps the delta and the repaint showing the same
                // screen.
                repair_next = true;
            }
        }
    }
    if !seg.is_empty() {
        segments.push(seg);
    }
    let backend = terminal.backend_mut();
    for seg in &segments {
        backend.draw(seg.iter().map(|(x, y, c)| (*x, *y, c)))?;
    }
    // `Backend::draw` leaves the cursor wherever it printed last; put it back where the frame
    // wants it or the next keystroke echoes in the wrong place. Clamped to the viewport for the
    // same reason the cells are.
    if let Some((cx, cy)) = cursor
        && cx < vw
        && cy < vh
    {
        backend.set_cursor_position(Position::new(cx, cy))?;
    }
    // Disambiguated: `CrosstermBackend` implements both `Backend::flush` and `io::Write::flush`.
    Backend::flush(backend)
}

/// Standard base64 (RFC 4648, `+`/`/`, `=` padding) of arbitrary bytes — for the OSC 52
/// clipboard payload. Hand-rolled to avoid a dependency for ~15 trivial, stable lines;
/// round-trip-locked by a unit test so malformed padding can't slip through silently.
fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Set the system clipboard via OSC 52: `ESC ] 52 ; c ; <base64> BEL`. Written to stdout and
/// flushed (before+after) so it doesn't interleave with a ratatui draw. Most terminals honor it
/// (iTerm2/kitty/wezterm/alacritty); a terminal that ignores it simply doesn't copy (no error).
/// NOTE: an ENCLOSING tmux/screen needs clipboard passthrough (`set-clipboard on`) to forward it.
fn write_osc52(text: &str) -> io::Result<()> {
    let mut out = io::stdout();
    out.flush()?;
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

/// Ask the TERMINAL its true text-area size, bypassing the OS pty winsize (`TIOCGWINSZ`) — the
/// recovery path when a stale winsize survives a sleep/wake that never propagated the new size to
/// the remote pty (so `terminal.size()` keeps returning the OLD value). crossterm 0.28 silently
/// DROPS the `CSI 8;h;w t` reply to a `CSI 18 t` query (its parser errors on the `t` final byte
/// and clears the buffer — see `event/source/unix/tty.rs`), so that report can't be read through
/// the event loop. Instead we use the report crossterm DOES parse: park the cursor far past the
/// bottom-right (the terminal clamps it to the real last cell), read where it landed via DSR
/// (`crossterm::cursor::position`), then restore it. Size = clamped position + 1.
/// `SavePosition`/`RestorePosition` round-trip to the same spot, so ratatui's cursor cache stays
/// valid. Returns `None` if the terminal didn't respond or didn't clamp, so a non-conforming
/// terminal can never inject a bogus giant size. Timing: a terminal that simply doesn't answer DSR
/// bounds the call at crossterm's ~2s poll timeout (then `position()` returns `Err` → `None`); the
/// only unbounded path is a PERSISTENT stdin poll error inside crossterm's retry loop, which means
/// the fd is already broken and the client is dead regardless. Triggered only on the rare
/// stale-size signals (focus regain / resume-from-sleep), so the 2s worst case is not hit in steady
/// state.
fn probe_true_size() -> Option<(u16, u16)> {
    let mut out = io::stdout();
    execute!(out, SavePosition, MoveTo(PROBE_CORNER, PROBE_CORNER)).ok()?;
    let pos = cursor::position();
    let _ = execute!(out, RestorePosition);
    let (col, row) = pos.ok()?;
    size_from_clamped_cursor(col, row)
}

/// The far corner we park the cursor at so the terminal clamps it to the real bottom-right cell.
const PROBE_CORNER: u16 = 9998;

/// Upper bound (exclusive) on a probed dimension. Real terminals are well under this; a larger
/// value means the terminal didn't clamp (echoing `PROBE_CORNER` back) or is lying. This is a
/// trust boundary: the value flows into `Terminal::resize`, which allocates cols×rows cells TWICE,
/// so an unbounded dimension is a memory-exhaustion vector. 1000×1000 is already generous.
const PROBE_MAX: u16 = 1000;

/// Turn the clamped cursor position from [`probe_true_size`] into a `(cols, rows)` size, or `None`
/// if it looks unreliable. Rejects any dimension at/above [`PROBE_MAX`] — which covers both the
/// non-clamping terminal (echoes `PROBE_CORNER`) and any other absurd value — so a hostile or buggy
/// terminal can never drive a giant allocation. Pure so the guard is unit-tested without a live
/// terminal.
fn size_from_clamped_cursor(col: u16, row: u16) -> Option<(u16, u16)> {
    if col >= PROBE_MAX || row >= PROBE_MAX {
        return None;
    }
    Some((col.saturating_add(1).max(1), row.saturating_add(1).max(1)))
}

/// Restores the host terminal (raw mode off + leave alt screen) on drop — so a
/// panic or an abrupt server exit never leaves the user's terminal wedged. Mouse
/// capture is enabled lazily via [`TermGuard::enable_mouse`] when the server's `Hello`
/// says so (server-authoritative), and disabled on drop only if it was enabled.
struct TermGuard {
    mouse: bool,
}

impl TermGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // Focus reporting lets us re-probe the true terminal size when the window regains focus
        // (e.g. after the display woke from sleep) — the sanctioned recovery for a stale OS
        // winsize. Harmless on terminals that ignore it.
        execute!(io::stdout(), EnterAlternateScreen, EnableFocusChange)?;
        // Autowrap OFF (DECAWM, `ESC [ ? 7 l`) for the life of the alt screen.
        //
        // comux positions every cell itself and never relies on the terminal wrapping for it, so
        // wrapping can only do damage. A glyph the outer terminal draws WIDER than we measured it
        // — a Nerd Font icon in a prompt is the everyday case — would otherwise overflow the last
        // column, and a wrapping terminal moves the WHOLE glyph to the next row, clobbering two
        // cells there; in the bottom-right corner it scrolls the entire screen, which no amount
        // of repainting cells can undo. With wrapping off the terminal clamps instead, so the
        // worst a mismeasured glyph can do is cover its own row's neighbour — which `emit_view`
        // repairs. Restored on the way out.
        write!(io::stdout(), "\u{1b}[?7l")?;
        io::stdout().flush()?;
        Ok(Self { mouse: false })
    }

    /// Turn on mouse capture (wheel scrollback + click-to-focus/navigate). Trade-off:
    /// takes over native selection; most terminals let you hold Shift to bypass. Called
    /// once when the server's `Hello { mouse: true }` arrives.
    fn enable_mouse(&mut self) -> io::Result<()> {
        if !self.mouse {
            execute!(io::stdout(), EnableMouseCapture)?;
            self.mouse = true;
        }
        Ok(())
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.mouse {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        let _ = write!(io::stdout(), "\u{1b}[?7h"); // restore autowrap (see `enter`)
        let _ = execute!(io::stdout(), DisableFocusChange, LeaveAlternateScreen);
    }
}

/// Connect to the running server, spawning a detached one if none answers, then run
/// the attach loop until detach / server exit.
pub fn run() -> io::Result<()> {
    run_with(AttachOpts::default())
}

/// How [`run_with`] should behave when nothing is listening on the socket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttachOpts {
    /// Refuse to birth a server; fail instead.
    ///
    /// Bare `comux` is connect-or-spawn, which is right for a human at a terminal and wrong
    /// for anything supervised. A server BIRTHED by a daemon inherits that daemon's kernel
    /// session and environment — and a long-lived supervisor has deliberately scrubbed the
    /// volatile session vars out of its own (see `update_environment`), so such a server
    /// would then own every pane the user ever opens with no desktop session behind it.
    /// Nothing about that is visible after the fact; it can only be undone by killing the
    /// server. A caller that cannot guarantee it is a human at a console sets this.
    pub no_spawn: bool,
}

/// [`run`], with explicit control over the connect-or-spawn decision.
pub fn run_with(opts: AttachOpts) -> io::Result<()> {
    // Print any config warnings NOW, before raw/alt-screen — an auto-spawned server's
    // stderr is /dev/null, so this is the user's reliable view of config diagnostics.
    // (The effective mouse setting is the SERVER's, delivered in its `Hello`; the client
    // never applies its own local config to behavior — only surfaces its warnings.)
    let (_cfg, warnings) = crate::config::MuxConfig::load();
    for w in &warnings {
        eprintln!("comux config: {w}");
    }
    let sock = socket_path();
    let stream = if opts.no_spawn {
        connect_only(&sock)?
    } else {
        connect_or_spawn(&sock)?
    };
    run_attached(stream)
}

/// Connect to `sock` and fail if nothing answers, rather than starting a server.
///
/// Deliberately ONE attempt with no retry: the caller asked not to create a server, so there
/// is nothing that could appear by waiting. The message names the socket because the usual
/// cause is a caller resolving a different one than the server bound.
fn connect_only(sock: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(sock).map_err(|e| {
        io::Error::new(
            io::ErrorKind::NotConnected,
            format!(
                "no running comux at {} ({e}), and --no-spawn was given so none was started",
                sock.display()
            ),
        )
    })
}

/// Connect to `sock`; if nothing is listening, spawn a server and retry with backoff.
/// Re-spawns periodically during the wait: a server spawned while a PRIOR one is still
/// shutting down loses the flock race and exits, so a single spawn can silently do nothing
/// (e.g. right after `kill-server`). Re-spawning every ~500ms guarantees one eventually
/// wins the freed flock. Only the flock winner binds; the losers exit harmlessly.
fn connect_or_spawn(sock: &Path) -> io::Result<UnixStream> {
    if let Ok(s) = UnixStream::connect(sock) {
        return Ok(s);
    }
    // We're about to BIRTH a new server (no one was listening). A server started from an
    // SSH session freezes that session's kernel context (logind seat / macOS bootstrap):
    // per-pane `update_environment` refreshes the SSH/display ENV, but local-only
    // privileges (polkit shutdown, GUI-app access like Claude in Chrome) follow where the
    // server was born and can't be moved per-pane. Nudge the user once, before the spawn.
    if std::env::var_os("SSH_CONNECTION").is_some()
        && std::env::var_os("COPAD_MUX_QUIET_SSH").is_none()
    {
        eprintln!(
            "comux: starting the server from an SSH session — for local-only features \
             (Claude in Chrome, system power actions) start it from a local console instead. \
             (COPAD_MUX_QUIET_SSH=1 silences this.)"
        );
    }
    spawn_server()?;
    let mut last_spawn = std::time::Instant::now();
    for _ in 0..160 {
        if let Ok(s) = UnixStream::connect(sock) {
            return Ok(s);
        }
        if last_spawn.elapsed() >= Duration::from_millis(500) {
            spawn_server()?;
            last_spawn = std::time::Instant::now();
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "comux server did not come up",
    ))
}

/// Ensure a server is running at `sock` (spawn one detached + wait), WITHOUT attaching —
/// for control commands like `new-session` that should start the mux if it isn't up yet
/// (tmux `new-session` starts the server). Reuses [`connect_or_spawn`].
pub fn ensure_running(sock: &Path) -> io::Result<()> {
    connect_or_spawn(sock).map(|_| ())
}

/// Spawn `copad-mux server` detached (new session, stdio to /dev/null) so it outlives
/// this client's terminal.
fn spawn_server() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid() in the child detaches it from this controlling terminal so it
    // survives the client exiting; it touches no shared state beyond the syscall.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

fn send(w: &mut UnixStream, msg: &ClientMsg) -> io::Result<()> {
    let line = serde_json::to_string(msg).map_err(io::Error::other)?;
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

/// Drive ratatui's render area to `(cols, rows)` AND tell the server. With a `Viewport::Fixed`
/// terminal, `resize` reallocates the buffers and clears the screen — a clean full repaint at the
/// new size, independent of the (possibly stale) OS winsize — which is exactly what we want on any
/// size change (real resize, missed-event poll, or true-size probe). Errors are swallowed: a failed
/// resize/send self-corrects on the next size signal.
fn apply_size(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    wr: &mut UnixStream,
    cols: u16,
    rows: u16,
) {
    let _ = terminal.resize(RRect::new(0, 0, cols, rows));
    let _ = send(wr, &ClientMsg::Resize { cols, rows });
}

/// The attach loop: forward input, apply incoming frames, draw.
fn run_attached(stream: UnixStream) -> io::Result<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = write!(io::stdout(), "\u{1b}[?7h"); // restore autowrap (see `TermGuard::enter`)
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default_hook(info);
    }));

    let mut guard = TermGuard::enter()?;
    // Fixed viewport (NOT Fullscreen): ratatui must not autoresize its render area to the OS
    // winsize each draw, because the self-heal below can drive the area to the terminal's TRUE
    // size while the OS winsize is still stale (sleep/wake). We own the size and apply it via
    // `apply_size`. Seed from the OS size (correct at attach).
    let (tw, th) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let (mut cols, mut rows) = (tw.max(1), th.max(1));
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(RRect::new(0, 0, cols, rows)),
        },
    )?;

    let mut wr = stream.try_clone()?;
    send(
        &mut wr,
        &ClientMsg::Attach {
            cols,
            rows,
            pid: Some(std::process::id()),
        },
    )?;

    // The server's `Hello` is always its first message. Consume it SYNCHRONOUSLY here —
    // before the input loop starts — and reply with our `Env` so a fast pane-creation
    // keystroke can never race ahead of the environment handshake (tmux update-environment).
    // The SAME reader is then moved into the reader thread so no buffered bytes are lost.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut pending_first: Option<ServerMsg> = None;
    {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return Ok(()), // server gone before it said hello
                Ok(_) => {}
            }
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            match serde_json::from_str::<ServerMsg>(t) {
                Ok(ServerMsg::Hello {
                    mouse,
                    update_environment,
                }) => {
                    // Server-authoritative: only now (not from local config) do we decide
                    // whether to capture the mouse, so every client agrees with the server.
                    if mouse {
                        let _ = guard.enable_mouse();
                    }
                    // Reply with our live values for exactly the vars the server asked for,
                    // reading each via `var_os` (never `env::vars()`, which panics on a
                    // non-UTF-8 entry) and keeping only those present and valid UTF-8.
                    let env: Vec<(String, String)> = update_environment
                        .into_iter()
                        .filter_map(|name| {
                            std::env::var_os(&name)
                                .and_then(|v| v.into_string().ok())
                                .map(|v| (name, v))
                        })
                        .collect();
                    // Copad exports both of these into every tab's shell, so a client
                    // hosted by it can name the exact tab it occupies. Read here, in the
                    // CLIENT, for the same reason the env above is: the server froze its own
                    // environment at birth and may not even be running under a GUI.
                    let copad_host = std::env::var("COPAD_SOCKET")
                        .ok()
                        .zip(std::env::var("COPAD_PANEL_ID").ok())
                        .filter(|(s, p)| !s.is_empty() && !p.is_empty());
                    let _ = send(
                        &mut wr,
                        &ClientMsg::Env {
                            vars: env,
                            copad_host,
                        },
                    );
                    break;
                }
                Ok(ServerMsg::Bye) => return Ok(()),
                // Shouldn't precede Hello, but forward anything else so no frame is lost.
                Ok(other) => {
                    pending_first = Some(other);
                    break;
                }
                Err(_) => continue,
            }
        }
    }

    // Reader thread: server frames → channel; dropping the sender on EOF signals the
    // main loop (recv → Disconnected) that the server went away.
    let (tx, rx) = mpsc::channel::<ServerMsg>();
    {
        if let Some(m) = pending_first.take() {
            let _ = tx.send(m);
        }
        std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<ServerMsg>(t)
                    && tx.send(msg).is_err()
                {
                    break;
                }
            }
        });
    }

    // Client-side framebuffer sized to the SERVER's frame — which may be SMALLER than
    // this terminal when another attached client is smaller (tmux-style shared view).
    // The margin is letterboxed blank. Starts as a placeholder until the first frame.
    let mut buf = Buffer::empty(RRect::new(0, 0, 1, 1));
    let mut have_frame = false;
    let mut cursor: Option<(u16, u16)> = None;
    // A `full` frame means "repaint everything" (attach / resize / takeover / Ctrl-b r).
    // Honor it by clearing the ratatui terminal before the next draw so its diff baseline
    // is wiped and EVERY cell is re-emitted — otherwise a cell the real terminal lost
    // (nested emulator, resize, alt-screen transition) lingers as a ghost.
    let mut force_clear = false;
    // A SILENT full repaint is due: re-emit every cell without clearing (see `repaint_all`).
    // Set by the server's periodic `FrameMsg::repaint` (`reconcile_secs`) and by the
    // client-local timer below.
    let mut force_repaint = false;
    // Our own record of what the terminal is showing — the single diff baseline now that the
    // client no longer paints through `Terminal::draw` (see `emit_view`).
    let mut painted = Buffer::empty(RRect::new(0, 0, 1, 1));
    // Whether the cursor is currently shown, so show/hide is emitted on the edge only.
    //
    // `None` = UNKNOWN, which is the honest starting state and not an implementation detail:
    // entering the alternate screen does not reset cursor visibility, so a shell that hid it
    // (`printf '\033[?25l'; comux`) hands us an invisible cursor. Assuming "shown" there means
    // the first frame matches, nothing is emitted, and the cursor never comes back — a
    // regression against `Terminal::draw`, which asserted visibility on every frame that had a
    // cursor. Unknown forces the first frame to state it either way.
    let mut cursor_shown: Option<bool> = None;
    // A resize has cleared the screen since the last paint (see `baseline_is_stale`).
    let mut resized = false;
    // Client-local silent-repaint period, DEFAULT OFF — additive to the cadence the server
    // drives, so a client whose OUTER emulator drifts faster than the rest can heal itself
    // without changing `reconcile_secs` for every attached client.
    //
    // This is the old `COPAD_MUX_REDRAW_MS` self-heal, which had to default OFF because it
    // did `Clear(All)` first and flashed a blank frame every tick. It no longer clears, so
    // setting it now costs a repaint's worth of escapes and nothing visible.
    let local_repaint = std::env::var("COPAD_MUX_REDRAW_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::ZERO, Duration::from_millis);
    let mut last_repaint = Instant::now();
    // Periodic winsize reconciliation. A terminal normally delivers a `CEvent::Resize` when it
    // is resized, but that event can be LOST — crossterm coalescing a rapid burst, or an outer
    // terminal that updated the tty's winsize without a clean SIGWINCH to us. When it is lost we
    // keep composing at the STALE size and the status bar lands off the true bottom (reported
    // over SSH from Windows Terminal after a sleep/wake). So once a second we re-read the tty size
    // and adopt it WHEN IT CHANGES from the last value we saw — i.e. a genuine winsize update the
    // OS got but we missed the event for. Comparing against the last OS value (not the render size)
    // is deliberate: `probe_true_size` below can set the render size to a value the OS winsize does
    // NOT yet agree with (a stale winsize), and this poll must not clobber that recovery back to the
    // stale OS value — it stays quiet until the OS winsize genuinely moves. `last_os_size` seeds
    // from the attach size (itself a TIOCGWINSZ read) and advances on every acknowledged OS change
    // (this poll and `CEvent::Resize`), never on a probe. See docs/troubleshooting.md.
    let size_poll = Duration::from_millis(1000);
    let mut last_size_poll = Instant::now();
    let mut last_os_size = (cols, rows);
    // A deeper recovery for when the tty winsize itself is STALE — the sleep/wake case where the
    // outer terminal never propagated the new size to the remote pty, so the OS size above keeps
    // returning the OLD value and the 1s poll can't help. We instead ask the TERMINAL its true
    // size directly (see `probe_true_size`), triggered by a `FocusGained` event: the window
    // regaining focus is exactly when the user returns after a wake and when a stale size is
    // likely, and it is delivered over SSH (unlike a suspend, which Rust's monotonic `Instant`
    // can't even observe — it pauses across suspend — and which anyway never freezes THIS process
    // when it is the far end of an SSH session whose local terminal slept). Bypasses the OS
    // winsize entirely.

    loop {
        // 1) forward input
        let mut need_redraw = false;
        // Any size change (event / poll / probe) updates cols+rows here, then a single
        // `apply_size` at the end drives ratatui + the server once — no mid-drain flicker.
        let mut size_changed = false;
        // Set by a `FocusGained` event to run the true-size probe (1c) this iteration.
        let mut probe_needed = false;
        if event::poll(Duration::from_millis(16))? {
            loop {
                match event::read()? {
                    CEvent::Key(k) if k.kind != KeyEventKind::Release => {
                        let _ = send(&mut wr, &ClientMsg::Key(k));
                    }
                    CEvent::Mouse(m) => {
                        // Forward wheel + left-click at their cell; the server maps to
                        // a pane (letterbox is top-left aligned, so coords pass through).
                        let kind = match m.kind {
                            MouseEventKind::ScrollUp => Some(MouseKind::ScrollUp),
                            MouseEventKind::ScrollDown => Some(MouseKind::ScrollDown),
                            MouseEventKind::Down(MouseButton::Left) => Some(MouseKind::Click),
                            // Button-held motion + release drive the drag-selection (crossterm's
                            // mouse capture enables button-event tracking, so these are reported).
                            MouseEventKind::Drag(MouseButton::Left) => Some(MouseKind::Drag),
                            MouseEventKind::Up(MouseButton::Left) => Some(MouseKind::Up),
                            // Right button drives the chrome context menu (tmux display-menu:
                            // hold → hover → release-to-select).
                            MouseEventKind::Down(MouseButton::Right) => Some(MouseKind::RightClick),
                            MouseEventKind::Drag(MouseButton::Right) => Some(MouseKind::RightDrag),
                            MouseEventKind::Up(MouseButton::Right) => Some(MouseKind::RightUp),
                            _ => None,
                        };
                        if let Some(kind) = kind {
                            let _ = send(
                                &mut wr,
                                &ClientMsg::Mouse {
                                    x: m.column,
                                    y: m.row,
                                    kind,
                                },
                            );
                        }
                    }
                    CEvent::Resize(w, h) => {
                        let os = (w.max(1), h.max(1));
                        // Gate on the OS value actually CHANGING, exactly like the 1s poll — so a
                        // delayed/duplicate resize event carrying the stale winsize can't clobber a
                        // `probe_true_size` recovery. Keeping `last_os_size` in step also stops the
                        // poll from re-detecting this as a change and resending redundantly.
                        if os != last_os_size {
                            last_os_size = os;
                            (cols, rows) = os;
                            size_changed = true;
                        }
                    }
                    // Window regained focus (e.g. the display woke): the OS winsize may be stale,
                    // so probe the terminal's true size below.
                    CEvent::FocusGained => probe_needed = true,
                    _ => {}
                }
                if !event::poll(Duration::from_millis(0))? {
                    break;
                }
            }
        }

        // 1b) reconcile a resize event we may have MISSED: re-read the tty size once a second and,
        // if the OS winsize CHANGED since we last saw it (a genuine update whose event we missed),
        // adopt it. Gated on the OS value moving — not on it differing from the render size — so a
        // stale OS winsize can't undo a `probe_true_size` recovery (see `last_os_size` above).
        if last_size_poll.elapsed() >= size_poll {
            last_size_poll = Instant::now();
            if let Ok((w, h)) = ratatui::crossterm::terminal::size() {
                let os = (w.max(1), h.max(1));
                if os != last_os_size {
                    last_os_size = os;
                    if os != (cols, rows) {
                        (cols, rows) = os;
                        size_changed = true;
                    }
                }
            }
        }

        // 1c) deeper recovery: when a stale OS winsize is LIKELY (resume from sleep, or the window
        // regaining focus), ask the terminal its true size directly and reconcile if it disagrees
        // with what we track — recovering the case the tty-winsize poll above cannot see.
        if probe_needed
            && let Some((w, h)) = probe_true_size()
            && (w, h) != (cols, rows)
        {
            (cols, rows) = (w, h);
            size_changed = true;
        }

        // Drive ratatui + the server ONCE for whatever moved the size this iteration.
        if size_changed {
            apply_size(&mut terminal, &mut wr, cols, rows);
            // `Terminal::resize` clears the screen, so the paint baseline is gone — whether or
            // not the size ended up different from the one it already had.
            resized = true;
            need_redraw = true;
        }

        // 2) apply incoming frames (buffer follows the server's frame size)
        let mut dirty = false;
        loop {
            match rx.try_recv() {
                Ok(ServerMsg::Frame(f)) => {
                    let fsize = RRect::new(0, 0, f.cols.max(1), f.rows.max(1));
                    if f.full {
                        buf = Buffer::empty(fsize);
                        force_clear = true;
                    } else if buf.area != fsize {
                        // A delta for a size we don't hold yet — wait for its full.
                        continue;
                    }
                    for c in &f.cells {
                        if let Some(cell) = buf.cell_mut(Position::new(c.x, c.y)) {
                            c.apply_to(cell);
                        }
                    }
                    // Rebuild wide-char spacer structure so the client buffer EXACTLY matches
                    // the server's — the wire omits trailing spacer cells (ratatui's diff drops
                    // the cell after a wide glyph), so without this a wide char that MOVED leaves
                    // a stale width-2 glyph behind, and ratatui's own emit then skips the real
                    // cell after it (a narrow char vanishes / the row shifts). See term.rs
                    // `relay_fidelity_pure_delta_churn`.
                    fix_wide_spacers(&mut buf);
                    cursor = f.cursor;
                    have_frame = true;
                    dirty = true;
                    // The server asked for a silent full repaint (self-heal). Sticky until the
                    // draw below honours it, so a repaint frame coalesced with later deltas in
                    // one drain is still acted on.
                    force_repaint |= f.repaint;
                }
                // A drag-selection copy: set the SYSTEM clipboard via OSC 52 through this
                // client's own terminal (works over SSH). A one-shot, non-rendering control —
                // written straight to stdout (flushed) outside ratatui's draw.
                Ok(ServerMsg::Copy { text }) => {
                    let _ = write_osc52(&text);
                }
                // Hello is consumed synchronously during the handshake above; a server
                // never sends a second one, so this arm is unreachable in practice.
                Ok(ServerMsg::Hello { .. }) => {}
                Ok(ServerMsg::Bye) => return Ok(()),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()), // server gone
            }
        }

        // 3) draw — blit the server frame into this terminal's top-left, blanking the
        // letterbox margin when our terminal is bigger than the shared (min) frame.
        // Client-local silent repaint due (see `local_repaint`)? Drive a draw for it, so it
        // still fires on a screen so idle that no frame is arriving at all.
        if !local_repaint.is_zero() && last_repaint.elapsed() >= local_repaint {
            force_repaint = true;
            need_redraw = true;
        }

        if (dirty || need_redraw) && have_frame {
            // A `full` frame resets the diff baseline: clear the screen + ratatui's cached
            // previous-buffer so the upcoming draw re-emits every cell (no lingering ghost).
            // This is the ONLY path that clears — its cells were applied over an EMPTY buffer,
            // so whatever the clear does not wipe would linger. The self-heal repaint below
            // never clears (that flash is what kept it disabled for a year).
            let cleared = force_clear;
            if cleared {
                terminal.clear()?;
                force_clear = false;
            }
            // What we believe is ON THE TERMINAL. Reset wherever the screen was blanked under
            // us — a `full` frame's clear, and a resize (`Terminal::resize` clears too) — so the
            // next emit repaints from a baseline that matches reality rather than one that
            // merely matches the last frame.
            let view = RRect::new(0, 0, cols, rows);
            if baseline_is_stale(cleared, resized, painted.area, view) {
                painted = Buffer::empty(view);
            }
            resized = false;
            let mut next = Buffer::empty(painted.area);
            blit_view(&buf, &mut next);
            // One renderer: every cell after a clear or a self-heal repaint, otherwise just what
            // changed — through the SAME emitter either way, so the two can never disagree about
            // where a glyph goes. Showing the cursor only when the frame has one mirrors what
            // `Terminal::draw` did.
            let baseline = if cleared || force_repaint {
                None
            } else {
                Some(&painted)
            };
            emit_view(&mut terminal, baseline, &next, cursor)?;
            painted = next;
            let want_cursor = cursor.is_some_and(|(cx, cy)| cx < cols && cy < rows);
            if cursor_visibility_needs_stating(cursor_shown, want_cursor) {
                if want_cursor {
                    terminal.show_cursor()?;
                } else {
                    terminal.hide_cursor()?;
                }
                cursor_shown = Some(want_cursor);
            }
            if force_repaint || cleared {
                last_repaint = Instant::now();
            }
            force_repaint = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AttachOpts, PROBE_CORNER, PROBE_MAX, base64, connect_only, emit_view, io, repaint_all,
        size_from_clamped_cursor,
    };
    use ratatui::backend::CrosstermBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::{Position, Rect};
    use ratatui::style::{Color, Style};
    use ratatui::{Terminal, TerminalOptions, Viewport};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A `Write` sink we can read back, to assert on the exact escape bytes emitted.
    #[derive(Clone)]
    struct Sink(Rc<RefCell<Vec<u8>>>);
    impl io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_delta_repaints_the_neighbour_an_over_wide_glyph_can_clobber() {
        // Anchoring fixes where the NEXT emitted cell goes; it does not undo what the glyph
        // overwrote getting there. A one-column cell that becomes a Nerd Font icon the outer
        // terminal draws two columns wide clobbers its neighbour — and if that neighbour did not
        // change, a pure diff leaves it clobbered until the next self-heal repaint restores it.
        // That restore IS the "text lands, then jumps" this emitter exists to remove, so the
        // delta has to repaint the neighbour itself.
        let area = Rect::new(0, 0, 5, 1);
        let mut prev = Buffer::empty(area);
        prev.set_string(0, 0, "aXbcd", Style::default());
        let mut cur = Buffer::empty(area);
        cur.set_string(0, 0, "a\u{e0b0}bcd", Style::default()); // only column 1 changed

        let sink = Sink(Rc::new(RefCell::new(Vec::new())));
        let mut term = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .expect("terminal");
        emit_view(&mut term, Some(&prev), &cur, None).expect("emit");
        let out = String::from_utf8(sink.0.borrow().clone()).expect("utf8");

        assert!(
            out.contains('\u{e0b0}'),
            "the changed cell is emitted: {out:?}"
        );
        assert!(
            out.contains("\u{1b}[1;3Hb"),
            "the unchanged neighbour is repainted, anchored, because the icon may have covered              its column: {out:?}"
        );
        // Only the neighbour, though — the rest of the unchanged row is left alone.
        assert!(
            !out.contains('c') && !out.contains('d'),
            "a delta must not turn into a full repaint: {out:?}"
        );
    }

    #[test]
    fn the_last_column_is_erased_before_a_glyph_that_might_not_fit_it() {
        // The one cell with no neighbour to repair. With autowrap off, a terminal that measures
        // this glyph as two columns declines to write it at all rather than clamping — so the
        // previous contents would survive, `painted` would record the glyph we never drew, and
        // neither a later delta nor a repaint would ever touch the cell again. Erasing first
        // makes the result the same whatever was there before.
        let area = Rect::new(0, 0, 3, 1);
        let mut prev = Buffer::empty(area);
        prev.set_string(0, 0, "abX", Style::default());
        let mut cur = Buffer::empty(area);
        cur.set_string(0, 0, "ab\u{e0b0}", Style::default()); // only the last column changed

        let sink = Sink(Rc::new(RefCell::new(Vec::new())));
        let mut term = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .expect("terminal");
        emit_view(&mut term, Some(&prev), &cur, None).expect("emit");
        let out = String::from_utf8(sink.0.borrow().clone()).expect("utf8");

        let erase = out
            .find("\u{1b}[1;3H ")
            .unwrap_or_else(|| panic!("last column erased first: {out:?}"));
        let glyph = out.find('\u{e0b0}').expect("the glyph is still emitted");
        assert!(erase < glyph, "the erase comes BEFORE the glyph: {out:?}");
        // A mid-row glyph needs no erase — it has a neighbour that gets repaired instead.
        let mut mid = Buffer::empty(area);
        mid.set_string(0, 0, "a\u{e0b0}X", Style::default());
        let sink2 = Sink(Rc::new(RefCell::new(Vec::new())));
        let mut term2 = Terminal::with_options(
            CrosstermBackend::new(sink2.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .expect("terminal");
        emit_view(&mut term2, Some(&prev), &mid, None).expect("emit");
        let out2 = String::from_utf8(sink2.0.borrow().clone()).expect("utf8");
        assert!(
            !out2.contains("\u{1b}[1;2H "),
            "a mid-row glyph is not preceded by an erase: {out2:?}"
        );
    }

    #[test]
    fn a_repair_never_crosses_a_row_boundary() {
        // With autowrap off (see `TermGuard::enter`) a glyph that overflows the last column is
        // clamped, not wrapped — so the damage stays inside its row and the first cell of the
        // NEXT row needs no repair. Carrying the repair across the boundary would repaint a cell
        // on every row for nothing, and would also be a half-measure for a hazard that is
        // prevented rather than mitigated: a wrapping terminal moves the WHOLE glyph down, which
        // costs two cells, and in the bottom-right corner scrolls the screen.
        let area = Rect::new(0, 0, 3, 2);
        let mut prev = Buffer::empty(area);
        prev.set_string(0, 0, "abX", Style::default());
        prev.set_string(0, 1, "cde", Style::default());
        let mut cur = Buffer::empty(area);
        cur.set_string(0, 0, "ab\u{e0b0}", Style::default()); // only the LAST cell of row 0 changed
        cur.set_string(0, 1, "cde", Style::default()); // row 1 untouched

        let sink = Sink(Rc::new(RefCell::new(Vec::new())));
        let mut term = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .expect("terminal");
        emit_view(&mut term, Some(&prev), &cur, None).expect("emit");
        let out = String::from_utf8(sink.0.borrow().clone()).expect("utf8");

        assert!(
            out.contains('\u{e0b0}'),
            "the changed cell is emitted: {out:?}"
        );
        assert!(
            !out.contains('c'),
            "the next row's first cell is not dragged into the delta: {out:?}"
        );
    }

    #[test]
    fn the_first_frame_states_the_cursor_even_if_it_already_looks_right() {
        use super::cursor_visibility_needs_stating;
        // Entering the alternate screen does not reset cursor visibility, so `printf
        // '\033[?25l'; comux` hands us an invisible one. "Never set" therefore cannot be
        // optimised away as "already correct" — that is how the cursor stays gone all session.
        assert!(cursor_visibility_needs_stating(None, true));
        assert!(cursor_visibility_needs_stating(None, false));
        // Afterwards it is edge-triggered, so an idle screen does not re-emit it every frame.
        assert!(!cursor_visibility_needs_stating(Some(true), true));
        assert!(!cursor_visibility_needs_stating(Some(false), false));
        assert!(cursor_visibility_needs_stating(Some(false), true));
        assert!(cursor_visibility_needs_stating(Some(true), false));
    }

    #[test]
    fn the_paint_baseline_is_thrown_away_whenever_the_screen_was_blanked() {
        use super::baseline_is_stale;
        let view = Rect::new(0, 0, 80, 24);
        // Steady state: keep the baseline, or every frame becomes a full repaint.
        assert!(!baseline_is_stale(false, false, view, view));
        // A `full` frame cleared the screen.
        assert!(baseline_is_stale(true, false, view, view));
        // A different view size cannot be compared cell for cell.
        assert!(baseline_is_stale(
            false,
            false,
            Rect::new(0, 0, 80, 23),
            view
        ));
        // THE easy one to miss: a queued burst of resize events can read A -> B -> A inside one
        // input drain. The size ends up unchanged, so an area comparison sees nothing — but
        // `Terminal::resize` ran and blanked the screen, so the baseline is a lie.
        assert!(
            baseline_is_stale(false, true, view, view),
            "a same-size resize still clears the screen"
        );
    }

    #[test]
    fn an_ordinary_frame_anchors_exactly_like_a_repaint_does() {
        // This is the property whose absence the owner could SEE: `ls` output landing in one
        // place and then jumping sideways a moment later.
        //
        // The incremental path used to paint through `Terminal::draw` — one unanchored run per
        // row — while the self-heal repaint re-anchored after every non-ASCII glyph (it has to,
        // or one Nerd Font icon skews a whole row on every tick). Two renderers that position
        // the same text differently produce exactly that jump. There is one emitter now, so the
        // anchoring is identical whether a frame is a delta or a full repaint.
        let area = Rect::new(0, 0, 6, 1);
        let prev = Buffer::empty(area);
        let mut cur = Buffer::empty(area);
        // Fills the row exactly, so every cell differs from the empty baseline — otherwise the
        // delta legitimately omits the trailing blank and the two cannot be compared byte for
        // byte.
        cur.set_string(0, 0, "ab\u{e0b0}cde", Style::default());

        let emit = |baseline: Option<&Buffer>| {
            let sink = Sink(Rc::new(RefCell::new(Vec::new())));
            let mut term = Terminal::with_options(
                CrosstermBackend::new(sink.clone()),
                TerminalOptions {
                    viewport: Viewport::Fixed(area),
                },
            )
            .expect("terminal");
            emit_view(&mut term, baseline, &cur, None).expect("emit");
            String::from_utf8(sink.0.borrow().clone()).expect("utf8")
        };

        let delta = emit(Some(&prev));
        let repaint = emit(None);
        // Same anchors either way — the cell after the icon is addressed explicitly in BOTH.
        for (what, out) in [("delta", &delta), ("repaint", &repaint)] {
            assert!(
                out.contains("\u{1b}[1;4H"),
                "{what} must re-anchor after a non-ASCII glyph: {out:?}"
            );
        }
        // Here every cell changed, so the two agree byte for byte. That equality is the point:
        // it is what guarantees nothing moves when the repaint lands on top of a delta.
        assert_eq!(
            delta, repaint,
            "a delta covering every cell must paint them exactly as a repaint would"
        );

        // And it really is a diff — an unchanged cell is not re-sent.
        let quiet = {
            let sink = Sink(Rc::new(RefCell::new(Vec::new())));
            let mut term = Terminal::with_options(
                CrosstermBackend::new(sink.clone()),
                TerminalOptions {
                    viewport: Viewport::Fixed(area),
                },
            )
            .expect("terminal");
            emit_view(&mut term, Some(&cur), &cur, None).expect("emit");
            String::from_utf8(sink.0.borrow().clone()).expect("utf8")
        };
        assert!(
            !quiet.contains('a') && !quiet.contains('\u{e0b0}'),
            "an unchanged screen emits no cells: {quiet:?}"
        );
    }

    #[test]
    fn a_silent_repaint_re_anchors_after_a_glyph_the_terminal_may_measure_differently() {
        // The repaint must never let ONE glyph skew the rest of a row.
        //
        // `CrosstermBackend::draw` positions a run of adjacent cells by the terminal's own cursor
        // advance, emitting no `MoveTo` between them — it trusts the terminal to have measured
        // every glyph exactly as `unicode-width` did. A full repaint makes each row one such run,
        // so a Nerd Font / Powerline icon (Private Use Area: one column here, two in plenty of
        // fonts) would shift everything after it — every few seconds, forever, because this
        // repaint repeats. comux cannot read the outer terminal's font, so the only safe move is
        // to re-anchor after any glyph that could carry the disagreement.
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "ab\u{e0b0}cd", Style::default()); // U+E0B0 = the Powerline separator

        let sink = Sink(Rc::new(RefCell::new(Vec::new())));
        let mut term = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .expect("terminal");
        repaint_all(&mut term, &buf, (area.width, area.height), None).expect("repaint");
        let out = String::from_utf8(sink.0.borrow().clone()).expect("utf8");

        // The cell AFTER the icon is addressed explicitly (1-based row 1, column 4) instead of
        // being left to wherever the terminal's cursor ended up.
        assert!(
            out.contains("\u{1b}[1;4H"),
            "the cell after a non-ASCII glyph must be re-anchored: {out:?}"
        );
        // ...and the plain-ASCII run before it is still emitted as one run, so the anchoring
        // costs nothing on ordinary text.
        let first = out.find("\u{1b}[1;1H").expect("row start");
        let icon = out.find('\u{e0b0}').expect("icon emitted");
        assert!(
            !out[first..icon].contains("\u{1b}[1;2H"),
            "ASCII needs no anchor — every terminal agrees it is one column: {out:?}"
        );
    }

    #[test]
    fn a_silent_repaint_paints_the_viewport_not_the_server_frame() {
        // The two sizes are routinely different and writing the wrong one corrupts the screen in
        // BOTH directions, so the repaint clips exactly like the draw closure it follows.
        let paint = |view: Rect, src: &Buffer, cursor| {
            let sink = Sink(Rc::new(RefCell::new(Vec::new())));
            let mut term = Terminal::with_options(
                CrosstermBackend::new(sink.clone()),
                TerminalOptions {
                    viewport: Viewport::Fixed(view),
                },
            )
            .expect("terminal");
            repaint_all(&mut term, src, (view.width, view.height), cursor).expect("repaint");
            String::from_utf8(sink.0.borrow().clone()).expect("utf8")
        };

        // (1) The frame is BIGGER than the viewport — the window right after this terminal
        // shrinks, when ratatui has been resized but the server's frame has not caught up.
        // Emitting the frame would write past the edge and wrap.
        let mut big = Buffer::empty(Rect::new(0, 0, 6, 3));
        big.set_string(4, 2, "Z", Style::default());
        let out = paint(Rect::new(0, 0, 4, 2), &big, Some((5, 2)));
        assert!(
            !out.contains('Z'),
            "a cell outside the viewport must not be emitted: {out:?}"
        );
        assert!(
            !out.contains("\u{1b}[3;"),
            "nothing may be addressed below the viewport's last row: {out:?}"
        );

        // (1b) And a TWO-COLUMN glyph STARTING on the viewport's last column is just as bad,
        // which checking only its starting coordinate misses: its second half goes off the edge,
        // the terminal wraps, and on the bottom row it scrolls the whole screen. The composition
        // never produces this; a retained frame across a shrink does.
        let mut wide = Buffer::empty(Rect::new(0, 0, 6, 2));
        wide.set_string(0, 0, "가", Style::default()); // fully inside — must survive
        wide.set_string(3, 1, "가", Style::default()); // starts on the last viewport column
        let out = paint(Rect::new(0, 0, 4, 2), &wide, None);
        assert_eq!(
            out.matches('가').count(),
            1,
            "the glyph that fits is emitted and the one that would spill is not: {out:?}"
        );

        // (2) The frame is SMALLER than the viewport — the ordinary letterbox, because the
        // composition is sized to the SMALLEST attached client. The margin must be blanked, or a
        // margin cell the terminal lost would never come back.
        let mut small = Buffer::empty(Rect::new(0, 0, 2, 1));
        small.set_string(0, 0, "ab", Style::default());
        let out = paint(Rect::new(0, 0, 4, 3), &small, None);
        assert!(
            out.contains("\u{1b}[3;1H"),
            "the margin rows are painted: {out:?}"
        );
        assert_eq!(
            out.matches('a').count(),
            1,
            "and the frame's own content is still emitted once: {out:?}"
        );
    }

    #[test]
    fn a_silent_repaint_re_emits_every_cell_and_never_clears() {
        // The two properties that let this run on a timer by default. (1) It re-emits
        // EVERYTHING, including cells ratatui's diff believes are already correct — that
        // belief is exactly what has gone stale. (2) It never clears: `Clear(All)` flashes a
        // blank frame, and that flicker is the only reason the old periodic self-heal had to
        // ship disabled.
        let area = Rect::new(0, 0, 6, 2);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "ab", Style::default().fg(Color::Red));
        buf.set_string(0, 1, "가X", Style::default());

        let sink = Sink(Rc::new(RefCell::new(Vec::new())));
        let mut term = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .expect("terminal");

        // Draw it once so ratatui's cached buffer matches — the state in which its own diff
        // would emit NOTHING on a second draw. The repaint must still emit everything.
        let src = buf.clone();
        term.draw(|f| {
            let out = f.buffer_mut();
            for y in 0..area.height {
                for x in 0..area.width {
                    if let (Some(a), Some(b)) = (
                        src.cell(Position::new(x, y)),
                        out.cell_mut(Position::new(x, y)),
                    ) {
                        *b = a.clone();
                    }
                }
            }
        })
        .expect("draw");
        sink.0.borrow_mut().clear();

        repaint_all(&mut term, &buf, (area.width, area.height), Some((3, 1))).expect("repaint");
        let out = String::from_utf8(sink.0.borrow().clone()).expect("utf8");

        assert!(
            out.contains('a') && out.contains('b'),
            "plain cells re-emitted: {out:?}"
        );
        assert!(out.contains('가'), "the wide glyph re-emitted: {out:?}");
        assert!(
            out.contains('X'),
            "the cell after the wide glyph re-emitted: {out:?}"
        );
        // The trailing half of `가` must NOT be printed over — that splits the glyph.
        assert_eq!(
            out.matches('가').count(),
            1,
            "the wide glyph is emitted once, its spacer skipped: {out:?}"
        );
        // No clear of any flavour: `2J` (all), `1J`/`0J` (partial), `3J` (scrollback).
        for clear in ["[2J", "[1J", "[0J", "[3J", "[J"] {
            assert!(
                !out.contains(clear),
                "a silent repaint must not clear (found {clear}): {out:?}"
            );
        }
        // And the cursor is put back where the frame wants it, not left after the last cell.
        assert!(
            out.contains("\u{1b}[2;4H"),
            "cursor restored to the frame position (1-based row 2, col 4): {out:?}"
        );
    }

    #[test]
    fn size_from_clamped_cursor_converts_and_guards() {
        // A normal clamped bottom-right cell → size is position + 1.
        assert_eq!(size_from_clamped_cursor(79, 23), Some((80, 24)));
        assert_eq!(size_from_clamped_cursor(0, 0), Some((1, 1)));
        // Just under the cap is accepted (the largest plausible real terminal).
        assert_eq!(
            size_from_clamped_cursor(PROBE_MAX - 1, PROBE_MAX - 1),
            Some((PROBE_MAX, PROBE_MAX))
        );
        // A terminal that didn't clamp echoes the target corner back → rejected.
        assert_eq!(size_from_clamped_cursor(PROBE_CORNER, PROBE_CORNER), None);
        // Absurd values BELOW the corner but at/above the cap are also rejected — the
        // memory-exhaustion guard (else ~1e8 cells could reach Terminal::resize).
        assert_eq!(size_from_clamped_cursor(9997, 9997), None);
        assert_eq!(size_from_clamped_cursor(PROBE_MAX, 10), None);
        assert_eq!(size_from_clamped_cursor(10, PROBE_MAX), None);
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors + padding cases (0/1/2 trailing bytes).
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // Non-ASCII payload (a drag-copy can contain UTF-8) encodes its bytes.
        assert_eq!(base64("가".as_bytes()), "6rCA");
    }

    /// The guarantee `--no-spawn` exists for: a supervised caller must never be the process
    /// that BIRTHS a server, because such a server inherits the supervisor's kernel session
    /// and scrubbed environment and then owns every pane the user opens afterwards.
    #[test]
    fn connect_only_refuses_instead_of_creating_a_server() {
        let dir = std::env::temp_dir().join(format!("cmx-ns-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("s");
        let _ = std::fs::remove_file(&sock);

        let err = connect_only(&sock).expect_err("must not connect");
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        // The socket is the observable proof: a spawn would have bound it.
        assert!(!sock.exists(), "connect_only must not create {sock:?}");
        // The message names the socket, because the usual cause is a caller resolving a
        // different path than the server bound.
        assert!(
            err.to_string().contains(&sock.display().to_string()),
            "message must name the socket, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_only_connects_when_a_server_is_listening() {
        let dir = std::env::temp_dir().join(format!("cmx-ok-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("s");
        let _ = std::fs::remove_file(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");

        connect_only(&sock).expect("must connect to a listening socket");

        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attach_opts_default_still_spawns() {
        // `run()` must keep the connect-or-spawn behaviour a human at a terminal relies on.
        assert!(!AttachOpts::default().no_spawn);
    }
}
