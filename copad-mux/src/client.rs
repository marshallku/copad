//! The thin mux client: connects to the server (spawning one if none is running), forwards
//! key/resize events, and blits the server's cell frames to the local terminal. Detaching or
//! losing the connection just exits the client; the server and shells live on.

use std::io::{self, BufRead, BufReader, Read, Stdout, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::cursor::{self, MoveTo, RestorePosition, SavePosition};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event as CEvent, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::layout::{Position, Rect as RRect};
use ratatui::{Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr;

use crate::control::socket_path;
use crate::proto::{ClientMsg, MouseKind, ServerMsg};

/// Re-derive wide-char spacer cells so a client buffer (built from wire deltas that omit the
/// trailing half of every wide glyph) matches the server's composed buffer: the cell after a
/// width>=2 symbol becomes a blank `skip` spacer, every other cell has `skip` cleared. Uses the
/// width function ratatui emits with (see term.rs `relay_fidelity_pure_delta_churn`).
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

/// A blank carrying `c`'s colours, so erasing a cell does not flash the default background
/// through a coloured region.
fn blank_like(c: &Cell) -> Cell {
    let mut b = Cell::EMPTY;
    b.set_symbol(" ");
    b.fg = c.fg;
    b.bg = c.bg;
    b.modifier = c.modifier;
    b
}

/// Must this frame state the cursor's visibility, given what we last set (`None` = never set)?
/// Only on a change, but "never set" counts as one: the alternate screen INHERITS the shell's
/// visibility, so treating unknown as shown leaves a cursor hidden before comux started hidden.
fn cursor_visibility_needs_stating(shown: Option<bool>, want: bool) -> bool {
    shown != Some(want)
}

/// Must the paint baseline be thrown away this frame? `painted` is only a useful baseline while
/// it describes what is ON the terminal. A `full` frame clears the screen; so does a RESIZE
/// (`Terminal::resize`), even when the size ends where it started (a queued burst can read
/// A -> B -> A within one input drain); and a view of a different size cannot be compared cell
/// for cell. Miss any and the next delta skips cells the terminal no longer has.
fn baseline_is_stale(cleared: bool, resized: bool, painted: RRect, view: RRect) -> bool {
    cleared || resized || painted != view
}

/// Map the server's frame onto a viewport-sized ratatui buffer: the frame's cells at the top-left,
/// blanks in the letterbox margin, and a blank in place of any glyph that would spill past the
/// right edge. The frame is composed at the SMALLEST attached client's size, so it differs from
/// this terminal either way: smaller (the margin must be blanked or a lost margin cell never
/// comes back) or bigger (right after this terminal shrinks, before the server catches up).
/// Then a TWO-COLUMN glyph can sit in the last column with its spacer outside, and the terminal
/// wraps (scrolling the screen on the bottom row), so its WIDTH has to fit, not just its start.
/// The composition never does this itself (see `term.rs::snapshot_grid`). Shared by the draw, the
/// silent repaint and the render-fidelity harness.
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
                _ => {
                    d.reset();
                }
            }
        }
    }
}

/// Blit a server frame into a viewport-sized buffer and emit every cell: the paint path with no
/// trustworthy baseline (after a clear, or the silent self-heal repaint behind
/// [`crate::proto::FrameMsg::repaint`]). The repaint must not `Clear` first: that flash is why the
/// periodic self-heal once shipped disabled. Production calls [`emit_view`] with `None`
/// directly; this composition exists so tests and the render-fidelity harness exercise the real
/// `blit_view` + `emit_view`.
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

/// Emit `cur` to the terminal: every cell when `prev` is `None` (the silent full repaint), or
/// just the cells that differ from `prev` (what is believed ON the terminal). This is the
/// client's whole paint path, in place of `Terminal::draw`, so deltas and repaints position
/// glyphs identically (two renderers placing text differently made output land, then jump).
///
/// The diff is per-cell and row-bounded, unlike `Buffer::diff`, whose wide-glyph suppression runs
/// over a flat buffer and leaks across row and pane boundaries. It ships a wide glyph's leading
/// cell and never prints over a spacer.
pub(crate) fn emit_view<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    prev: Option<&Buffer>,
    cur: &Buffer,
    cursor: Option<(u16, u16)>,
) -> io::Result<()> {
    let (vw, vh) = (cur.area.width, cur.area.height);
    // Emit in SEGMENTS, cutting after any cell whose glyph is not plain ASCII.
    // `CrosstermBackend::draw` positions a run by the terminal's own cursor advance, so a glyph
    // the outer terminal measures differently from `unicode-width` (a Nerd Font / Powerline icon:
    // one column here, two in many fonts) would shift the rest of a full-repaint row, and the
    // timer-driven repaint would re-apply that skew forever. comux cannot see the disagreement
    // (it lives in the outer terminal's font, which is also why the `app_render_fidelity` fuzz
    // cannot catch it), so re-anchor with a fresh `draw` (which opens with a `MoveTo`) after
    // every glyph that could carry it. ASCII needs no anchor.
    // Owned cells, because the last-column guard below emits a blank that exists in no buffer.
    let mut segments: Vec<Vec<(u16, u16, Cell)>> = Vec::new();
    let mut seg: Vec<(u16, u16, Cell)> = Vec::new();
    for y in 0..vh {
        // Set after emitting a glyph that could occupy MORE columns than we think, so the next
        // cell is repainted. Scoped to the row: autowrap is OFF (see `TermGuard::enter`), so an
        // overflow at the last column is clamped, not wrapped into the next row.
        let mut repair_next = false;
        for x in 0..vw {
            let Some(c) = cur.cell(Position::new(x, y)) else {
                continue;
            };
            // Never print over a wide glyph's trailing half. Skipping it also lets `repair_next`
            // survive to the first column the glyph does NOT cover, the one that can be clobbered.
            if c.skip {
                continue;
            }
            // An unchanged cell is not re-sent on an ordinary frame. `prev` is what is ON THE
            // TERMINAL, not what the server believes we hold.
            let unchanged = prev.is_some_and(|p| p.cell(Position::new(x, y)) == Some(c));
            if unchanged && !repair_next {
                continue;
            }
            repair_next = false;
            let plain_ascii = c.symbol().len() == 1 && c.symbol().is_ascii();
            // The LAST column cannot be repaired after the fact: with autowrap off a terminal that
            // measures this glyph as two columns declines to write it (alacritty's
            // `Handler::input` returns early), so the old content stays while `painted` records a
            // glyph we never drew. Erasing first makes the outcome independent of what was there.
            if !plain_ascii && x + 1 == vw {
                segments.push(vec![(x, y, blank_like(c))]);
            }
            seg.push((x, y, c.clone()));
            if !plain_ascii {
                // Cut the run so the NEXT cell is addressed with a `MoveTo`...
                segments.push(std::mem::take(&mut seg));
                // ...and repaint that cell even if unchanged: a one-column cell that becomes an
                // icon drawn two columns wide clobbers its neighbour, which a pure delta would
                // leave clobbered until the next self-heal repaint.
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
    // `Backend::draw` leaves the cursor where it printed last; put it back where the frame wants
    // it, clamped to the viewport.
    if let Some((cx, cy)) = cursor
        && cx < vw
        && cy < vh
    {
        backend.set_cursor_position(Position::new(cx, cy))?;
    }
    // Disambiguated: `CrosstermBackend` implements both `Backend::flush` and `io::Write::flush`.
    Backend::flush(backend)
}

/// Standard base64 (RFC 4648) of arbitrary bytes for the OSC 52 payload; hand-rolled to avoid a
/// dependency.
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

/// Set the system clipboard via OSC 52: `ESC ] 52 ; c ; <base64> BEL`, flushed before and after
/// so it doesn't interleave with a ratatui draw. A terminal that ignores it simply doesn't copy;
/// an ENCLOSING tmux/screen needs clipboard passthrough (`set-clipboard on`).
fn write_osc52(text: &str) -> io::Result<()> {
    let mut out = io::stdout();
    out.flush()?;
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

/// Ask the TERMINAL its true text-area size, bypassing the OS pty winsize: the recovery when a
/// stale winsize survives a sleep/wake. crossterm 0.28 DROPS the `CSI 8;h;w t` reply to
/// `CSI 18 t` (its parser errors on the `t` final byte), so instead park the cursor far past the
/// bottom-right (the terminal clamps it to the real last cell), read where it landed via DSR
/// (`crossterm::cursor::position`) and restore it; size = clamped position + 1. Returns `None`
/// if the terminal didn't respond or didn't clamp. A terminal that never answers DSR bounds the
/// call at crossterm's ~2s poll timeout; it only runs on rare stale-size signals (focus regain,
/// resume).
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

/// Turn the clamped cursor position from [`probe_true_size`] into a `(cols, rows)` size, or
/// `None` if unreliable: a dimension at/above [`PROBE_MAX`] is rejected, so a hostile or buggy
/// terminal cannot drive a giant allocation. Pure so the guard is unit-testable.
fn size_from_clamped_cursor(col: u16, row: u16) -> Option<(u16, u16)> {
    if col >= PROBE_MAX || row >= PROBE_MAX {
        return None;
    }
    Some((col.saturating_add(1).max(1), row.saturating_add(1).max(1)))
}

/// Restores the host terminal (raw mode off, leave alt screen) on drop, so a panic or abrupt
/// server exit never wedges it. Mouse capture is enabled lazily by `enable_mouse` and disabled
/// on drop only if it was.
struct TermGuard {
    mouse: bool,
    paste: bool,
    keys: bool,
    cursor_shaped: bool,
    title_pushed: bool,
    colors: Option<(crate::proto::Rgb8, crate::proto::Rgb8)>,
    /// Keys the user typed while the colour probe was reading stdin that could not be put
    /// back on the terminal's input queue; sent as keys once the server is listening.
    typed: Vec<KeyEvent>,
    graphics: bool,
    images_held: std::collections::HashSet<u32>,
}

impl TermGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // First of all, while the terminal reports nothing on its own yet (focus reporting and
        // mouse capture come later) and before crossterm's reader exists: this reads stdin raw.
        let (colors, typed, graphics) = probe_colors();
        // Focus reporting lets us re-probe the true terminal size when the window regains focus.
        execute!(io::stdout(), EnterAlternateScreen, EnableFocusChange)?;
        // Autowrap OFF (DECAWM) for the life of the alt screen. comux positions every cell itself,
        // so wrapping can only do damage: a glyph the outer terminal draws WIDER than measured
        // overflows the last column and a wrapping terminal moves the WHOLE glyph to the next row
        // (and scrolls the screen from the bottom-right corner). With wrapping off it clamps, and
        // `emit_view` repairs the neighbour. Restored on the way out.
        write!(io::stdout(), "\u{1b}[?7l")?;
        io::stdout().flush()?;
        let keys = enable_key_disambiguation();
        Ok(Self {
            mouse: false,
            paste: false,
            keys,
            cursor_shaped: false,
            title_pushed: false,
            colors,
            typed,
            graphics,
            images_held: Default::default(),
        })
    }

    /// Turn on mouse capture; takes over native selection (most terminals let Shift bypass).
    fn enable_mouse(&mut self) -> io::Result<()> {
        if !self.mouse {
            execute!(io::stdout(), EnableMouseCapture)?;
            self.mouse = true;
        }
        Ok(())
    }

    /// Ask our terminal for bracketed paste, so a paste or dropped file reaches us as ONE
    /// `Event::Paste`, the only form in which the pane's program can tell it was pasted.
    fn enable_paste(&mut self) -> io::Result<()> {
        if !self.paste {
            execute!(io::stdout(), EnableBracketedPaste)?;
            self.paste = true;
        }
        Ok(())
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        // Images live in the terminal, not the alt screen: free what this client uploaded.
        if !self.images_held.is_empty() {
            let mut out = io::stdout();
            for id in &self.images_held {
                let _ = write!(out, "\u{1b}_Ga=d,d=I,i={id},q=2\u{1b}\\");
            }
            let _ = out.flush();
        }
        let _ = disable_raw_mode();
        if self.mouse {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        if self.paste {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
        }
        if self.keys {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        if self.cursor_shaped {
            let _ = write!(io::stdout(), "\u{1b}[0 q");
        }
        if self.title_pushed {
            let _ = write!(io::stdout(), "\u{1b}[23;0t"); // XTWINOPS: pop the saved title
        }
        let _ = write!(io::stdout(), "\u{1b}[?7h"); // restore autowrap (see `enter`)
        let _ = execute!(io::stdout(), DisableFocusChange, LeaveAlternateScreen);
    }
}

/// Read our terminal's foreground and background (OSC 10/11 `?`) so panes asking for them (nvim
/// `background`, Claude Code light/dark) get real colours. A DA1 query rides behind as the
/// sentinel every terminal answers, so one that ignores colour queries costs a round trip, not
/// the timeout. Reads stdin RAW, so it runs before crossterm's event reader starts.
/// `COPAD_MUX_COLOR_PROBE=0` skips it.
fn probe_colors() -> (
    Option<(crate::proto::Rgb8, crate::proto::Rgb8)>,
    Vec<KeyEvent>,
    bool,
) {
    use std::os::fd::AsRawFd;
    if std::env::var("COPAD_MUX_COLOR_PROBE").is_ok_and(|v| v == "0") {
        return (None, Vec::new(), false);
    }
    let mut out = io::stdout();
    // The kitty graphics query rides along (decisions #194).
    if write!(
        out,
        "\u{1b}]10;?\u{1b}\\\u{1b}]11;?\u{1b}\\{GRAPHICS_QUERY}\u{1b}[c"
    )
    .is_err()
        || out.flush().is_err()
    {
        return (None, Vec::new(), false);
    }
    let fd = io::stdin().as_raw_fd();
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut got = Vec::new();
    while !da1_answered(&got) && got.len() < 4096 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 512];
        // SAFETY: `chunk` is a valid writable buffer of the length passed.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        got.extend_from_slice(&chunk[..n as usize]);
    }
    // A keystroke whose bytes straddle the DA1 reply would be cut in two: wait for the rest, as
    // crossterm's parser would. A lone ESC gets a short window (nothing after it IS the Esc key);
    // an `ESC [`/`ESC O` sequence or begun UTF-8 scalar is waited for until complete (over SSH a
    // key's bytes can arrive fragmented).
    let started = Instant::now();
    while let Some(wait) = ends_mid_key(&without_replies(&got)) {
        let timeout = match wait {
            MidKey::LoneEsc => {
                let left = (started + LONE_ESC).saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                left.as_millis() as i32
            }
            MidKey::Begun => -1,
        };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        if unsafe { libc::poll(&mut pfd, 1, timeout) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 64];
        // SAFETY: `chunk` is a valid writable buffer of the length passed.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        got.extend_from_slice(&chunk[..n as usize]);
    }
    let colors = osc_color(&got, b"10").zip(osc_color(&got, b"11"));
    let graphics = got.windows(GRAPHICS_OK.len()).any(|w| w == GRAPHICS_OK);
    // Anything else read here is the user typing during the probe. It belongs to the event
    // reader, which has not started: put it back on the input queue (TIOCSTI; allowed for one's
    // own tty on macOS, on Linux with `legacy_tiocsti`), else decode it into keys the caller sends.
    let rest = without_replies(&got);
    let mut pushed = 0;
    for b in &rest {
        // SAFETY: `fd` is our stdin and `b` a valid byte for the duration of the call.
        if unsafe { libc::ioctl(fd, libc::TIOCSTI, b as *const u8) } != 0 {
            break;
        }
        pushed += 1;
    }
    (colors, decode_typed(&rest[pushed..]), graphics)
}

/// kitty graphics support probe: a 1x1 query, answered `OK` by a terminal that draws images.
const GRAPHICS_QUERY: &str = "\u{1b}_Gi=4242,s=1,v=1,a=q,t=d,f=24;AAAA\u{1b}\\";
const GRAPHICS_OK: &[u8] = b"\x1b_Gi=4242;OK";

/// The terminal's cell size in pixels, from `TIOCGWINSZ` (zero when it does not say).
fn cell_pixels() -> Option<(u16, u16)> {
    use std::os::fd::AsRawFd;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ fills a winsize we own.
    if unsafe { libc::ioctl(io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } != 0 {
        return None;
    }
    if ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return None;
    }
    Some((ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row))
}

fn upload_commands(up: &crate::proto::ImageUpload) -> Vec<u8> {
    let mut out = Vec::with_capacity(up.data.len() + 64);
    let chunks: Vec<&[u8]> = up.data.as_bytes().chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = (i + 1 < chunks.len()) as u8;
        if i == 0 {
            let _ = write!(
                out,
                "\u{1b}_Ga=t,f=32,o=z,i={},s={},v={},q=2,m={more};",
                up.id, up.width, up.height
            );
        } else {
            let _ = write!(out, "\u{1b}_Gm={more};");
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// Place every run, after the cells are drawn: each frame starts from no placements, which also
/// puts back images a terminal erased under text it just rewrote (copad does).
fn place_images(
    runs: &[crate::proto::ImageRun],
    cols: u16,
    rows: u16,
    cell: (u16, u16),
) -> Vec<u8> {
    let mut out = b"\x1b_Ga=d,d=a,q=2\x1b\\".to_vec();
    let (cw, ch) = (cell.0.max(1) as f32, cell.1.max(1) as f32);
    for r in runs.iter().filter(|r| r.x < cols && r.y < rows) {
        let cols_here = r.cols.min(cols - r.x);
        let px = ((r.off[0] * cw) as u32).min(cw as u32 - 1);
        let py = ((r.off[1] * ch) as u32).min(ch as u32 - 1);
        let [sx, sy, sw, sh] = r.src;
        let _ = write!(
            out,
            "\u{1b}[{};{}H\u{1b}_Ga=p,i={},x={sx},y={sy},w={sw},h={sh},c={cols_here},r=1,X={px},Y={py},z={},C=1,q=2\u{1b}\\",
            r.y + 1,
            r.x + 1,
            r.id,
            r.z
        );
    }
    out
}

/// Decode raw keyboard bytes into key events. The probe runs before kitty keys, paste, mouse and
/// focus reporting are on, so input is LEGACY encoding only: UTF-8, C0 controls, `Alt` as an ESC
/// prefix, and the xterm CSI/SS3 cursor, editing and function keys with their modifier parameter.
fn decode_typed(bytes: &[u8]) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let (code, mut mods, len) = match b {
            b'\r' | b'\n' => (KeyCode::Enter, KeyModifiers::NONE, 1),
            b'\t' => (KeyCode::Tab, KeyModifiers::NONE, 1),
            0x7f | 0x08 => (KeyCode::Backspace, KeyModifiers::NONE, 1),
            0x1b => match bytes.get(i + 1) {
                None => (KeyCode::Esc, KeyModifiers::NONE, 1),
                Some(b'[') | Some(b'O') => {
                    let ss3 = bytes[i + 1] == b'O';
                    let end = bytes[i + 2..]
                        .iter()
                        .position(|c| (0x40..=0x7e).contains(c))
                        .map_or(bytes.len() - i, |p| p + 3);
                    if let Some(k) = decode_csi(&bytes[i + 2..i + end], ss3) {
                        out.push(k);
                    }
                    i += end;
                    continue;
                }
                Some(_) => {
                    let inner = decode_typed(&bytes[i + 1..]);
                    if let Some(first) = inner.into_iter().next() {
                        let consumed = encoded_len(&bytes[i + 1..]);
                        out.push(KeyEvent::new(
                            first.code,
                            first.modifiers | KeyModifiers::ALT,
                        ));
                        i += 1 + consumed;
                        continue;
                    }
                    (KeyCode::Esc, KeyModifiers::NONE, 1)
                }
            },
            0 => (KeyCode::Char(' '), KeyModifiers::CONTROL, 1),
            1..=26 => (
                KeyCode::Char((b'a' + b - 1) as char),
                KeyModifiers::CONTROL,
                1,
            ),
            0x1c..=0x1f => (
                KeyCode::Char(['\\', ']', '^', '_'][(b - 0x1c) as usize]),
                KeyModifiers::CONTROL,
                1,
            ),
            _ => {
                let len = encoded_len(&bytes[i..]);
                match std::str::from_utf8(&bytes[i..i + len])
                    .ok()
                    .and_then(|s| s.chars().next())
                {
                    Some(c) if !c.is_control() => (KeyCode::Char(c), KeyModifiers::NONE, len),
                    _ => {
                        i += len;
                        continue;
                    }
                }
            }
        };
        if let KeyCode::Char(c) = code
            && c.is_ascii_uppercase()
        {
            mods |= KeyModifiers::SHIFT;
        }
        out.push(KeyEvent::new(code, mods));
        i += len;
    }
    out
}

fn decode_csi(body: &[u8], ss3: bool) -> Option<KeyEvent> {
    let (&fin, params) = body.split_last()?;
    let params = std::str::from_utf8(params).ok()?;
    let mut nums = params.split(';').map(|p| p.parse::<u32>().ok());
    let first = nums.next().flatten();
    let modp = nums.next().flatten().unwrap_or(1);
    let bits = modp.saturating_sub(1);
    let mut mods = KeyModifiers::NONE;
    if bits & 1 != 0 {
        mods |= KeyModifiers::SHIFT;
    }
    if bits & 2 != 0 {
        mods |= KeyModifiers::ALT;
    }
    if bits & 4 != 0 {
        mods |= KeyModifiers::CONTROL;
    }
    let code = match fin {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        b'Z' if !ss3 => return Some(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        b'~' if !ss3 => match first? {
            1 | 7 => KeyCode::Home,
            2 => KeyCode::Insert,
            3 => KeyCode::Delete,
            4 | 8 => KeyCode::End,
            5 => KeyCode::PageUp,
            6 => KeyCode::PageDown,
            n @ 11..=15 => KeyCode::F((n - 10) as u8),
            n @ 17..=21 => KeyCode::F((n - 11) as u8),
            n @ 23..=24 => KeyCode::F((n - 12) as u8),
            _ => return None,
        },
        _ => return None,
    };
    Some(KeyEvent::new(code, mods))
}

fn encoded_len(bytes: &[u8]) -> usize {
    let n = match bytes.first() {
        Some(b) if *b >= 0xf0 => 4,
        Some(b) if *b >= 0xe0 => 3,
        Some(b) if *b >= 0xc0 => 2,
        _ => 1,
    };
    n.min(bytes.len()).max(1)
}

/// How long a lone ESC at the end of the probe may still turn out to start a sequence.
const LONE_ESC: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MidKey {
    LoneEsc,
    Begun,
}

fn ends_mid_key(buf: &[u8]) -> Option<MidKey> {
    if buf.last() == Some(&0x1b) {
        return Some(MidKey::LoneEsc);
    }
    if let Some(esc) = buf.iter().rposition(|b| *b == 0x1b)
        && matches!(buf.get(esc + 1), Some(b'[') | Some(b'O'))
        && !buf[esc + 2..].iter().any(|c| (0x40..=0x7e).contains(c))
    {
        return Some(MidKey::Begun);
    }
    let tail = buf.iter().rev().take_while(|b| **b & 0xc0 == 0x80).count();
    match buf.len().checked_sub(tail + 1).map(|i| buf[i]) {
        Some(lead) if lead >= 0xc0 && encoded_len(&[lead, 0x80, 0x80, 0x80]) > tail + 1 => {
            Some(MidKey::Begun)
        }
        _ => None,
    }
}

fn without_replies(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        let rest = &buf[i..];
        if rest.starts_with(b"\x1b]10;") || rest.starts_with(b"\x1b]11;") {
            let end = rest
                .iter()
                .enumerate()
                .find_map(|(k, b)| match b {
                    0x07 => Some(k + 1),
                    b'\\' if k > 0 && rest[k - 1] == 0x1b => Some(k + 1),
                    _ => None,
                })
                .unwrap_or(rest.len());
            i += end;
            continue;
        }
        if rest.starts_with(b"\x1b_G") {
            let end = rest
                .windows(2)
                .position(|w| w == b"\x1b\\")
                .map_or(rest.len(), |k| k + 2);
            i += end;
            continue;
        }
        if rest.starts_with(b"\x1b[?") {
            let body = rest[3..]
                .iter()
                .take_while(|b| b.is_ascii_digit() || **b == b';')
                .count();
            if rest.get(3 + body) == Some(&b'c') {
                i += 3 + body + 1;
                continue;
            }
        }
        out.push(buf[i]);
        i += 1;
    }
    out
}

fn da1_answered(buf: &[u8]) -> bool {
    buf.windows(3).enumerate().any(|(i, w)| {
        w == b"\x1b[?"
            && buf[i + 3..]
                .iter()
                .find(|b| !(b.is_ascii_digit() || **b == b';'))
                .is_some_and(|b| *b == b'c')
    })
}

/// Parse an `ESC ] <code> ; rgb:R/G/B` reply out of `buf` (1–4 hex digits per channel,
/// scaled to 8 bits, as XParseColor allows).
fn osc_color(buf: &[u8], code: &[u8]) -> Option<(u8, u8, u8)> {
    let mut head = b"\x1b]".to_vec();
    head.extend_from_slice(code);
    head.extend_from_slice(b";rgb:");
    let at = buf.windows(head.len()).position(|w| w == head.as_slice())? + head.len();
    let body: Vec<u8> = buf[at..]
        .iter()
        .take_while(|b| b.is_ascii_hexdigit() || **b == b'/')
        .copied()
        .collect();
    let body = std::str::from_utf8(&body).ok()?;
    let mut it = body.split('/').map(|h| {
        if h.is_empty() || h.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len())) - 1;
        Some(((v * 255 + max / 2) / max) as u8)
    });
    Some((it.next()??, it.next()??, it.next()??))
}

/// Ask our terminal for the kitty keyboard protocol's "disambiguate" reports when it speaks it,
/// so Shift+Enter, Ctrl+Enter and Ctrl+I vs Tab reach the server as distinct keys. The server
/// re-encodes each key for the pane it is for (`crate::keys`). `COPAD_MUX_EXTENDED_KEYS=0` opts out.
fn enable_key_disambiguation() -> bool {
    if std::env::var("COPAD_MUX_EXTENDED_KEYS").is_ok_and(|v| v == "0") {
        return false;
    }
    if !matches!(supports_keyboard_enhancement(), Ok(true)) {
        return false;
    }
    execute!(
        io::stdout(),
        // Alternate keys too: the terminal then reports the SHIFTED character it produced
        // (`Alt+Shift+1` → `Char('!')` + ALT, layout-aware), which is what a legacy pane must
        // be typed — `ESC !`, not `ESC 1`. crossterm keeps the shifted key and drops SHIFT.
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )
    .is_ok()
}

pub fn run() -> io::Result<()> {
    run_with(AttachOpts::default())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttachOpts {
    /// Refuse to birth a server; fail instead. A server BIRTHED by a daemon inherits that
    /// daemon's kernel session and scrubbed environment (see `update_environment`) and would own
    /// every pane the user opens with no desktop session behind it; only killing it undoes that.
    /// A caller that cannot guarantee it is a human at a console sets this.
    pub no_spawn: bool,
}

pub fn run_with(opts: AttachOpts) -> io::Result<()> {
    // Print config warnings NOW, before raw/alt-screen: an auto-spawned server's stderr is
    // /dev/null. The effective mouse setting is the server's, delivered in its `Hello`.
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

/// `comux proxy --stdio [--no-spawn]`: connect to the server (starting one unless `--no-spawn`)
/// and copy bytes both ways until either side closes. This is how a host UI reaches a REMOTE
/// comux (`ssh -T host comux proxy --stdio`). Nothing may be printed on stdout before the bridge
/// starts: the server's first line is the version handshake, and a stray line (a login banner)
/// reads as a broken remote. Diagnostics go to stderr only. Returns the exit code.
pub fn run_proxy(args: &[String]) -> i32 {
    if !args.iter().any(|a| a == "--stdio") {
        eprintln!("usage: comux proxy --stdio [--no-spawn]");
        return 2;
    }
    let sock = socket_path();
    let conn = if args.iter().any(|a| a == "--no-spawn") {
        connect_only(&sock)
    } else {
        connect_or_spawn(&sock)
    };
    let stream = match conn {
        Ok(s) => s,
        Err(e) => {
            eprintln!("comux proxy: {e}");
            return 1;
        }
    };
    let Ok(mut to_server) = stream.try_clone() else {
        return 1;
    };
    // stdin → server. On EOF half-close, so the server sees the peer leave while its last
    // replies can still drain to stdout.
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_server);
        let _ = to_server.shutdown(std::net::Shutdown::Write);
    });
    // server → stdout, flushed per read: the peer is a protocol, not a pager.
    let mut from_server = stream;
    let mut out = io::stdout().lock();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match from_server.read(&mut buf) {
            Ok(0) | Err(_) => return 0,
            Ok(n) => {
                if out.write_all(&buf[..n]).is_err() || out.flush().is_err() {
                    return 0;
                }
            }
        }
    }
}

fn host_tag(copad_host: Option<&(String, String)>) -> Option<String> {
    std::env::var("COPAD_MUX_HOST_TAG")
        .ok()
        .filter(|t| !t.is_empty())
        .or_else(|| copad_host.map(|(sock, panel)| copad_mux_proto::copad_tag(panel, sock)))
}

/// Connect to `sock` and fail if nothing answers, rather than starting a server. ONE attempt, no
/// retry: nothing can appear by waiting. The message names the socket because the usual cause is
/// a caller resolving a different one than the server bound.
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

/// Connect to `sock`; if nothing is listening, spawn a server and retry with backoff. Re-spawns
/// every ~500ms during the wait: a server spawned while a PRIOR one is still shutting down loses
/// the flock race and exits, so a single spawn can silently do nothing (e.g. right after
/// `kill-server`). Only the flock winner binds.
fn connect_or_spawn(sock: &Path) -> io::Result<UnixStream> {
    if let Ok(s) = UnixStream::connect(sock) {
        return Ok(s);
    }
    // About to BIRTH a server. One started from an SSH session freezes that session's kernel
    // context (logind seat / macOS bootstrap): `update_environment` refreshes the ENV per pane,
    // but local-only privileges (polkit shutdown, GUI-app access) follow where the server was
    // born. Warn once.
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

/// Ensure a server is running at `sock` without attaching (tmux `new-session` starts the
/// server). Reuses [`connect_or_spawn`].
pub fn ensure_running(sock: &Path) -> io::Result<()> {
    connect_or_spawn(sock).map(|_| ())
}

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
/// terminal, `resize` reallocates the buffers and clears the screen: a clean full repaint at the
/// new size, independent of the possibly stale OS winsize. Errors are swallowed; a failed
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

fn run_attached(stream: UnixStream) -> io::Result<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = write!(io::stdout(), "\u{1b}[?7h"); // restore autowrap (see `TermGuard::enter`)
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default_hook(info);
    }));

    let mut guard = TermGuard::enter()?;
    // Fixed viewport (NOT Fullscreen): ratatui must not autoresize to the OS winsize each draw,
    // because the true-size probe can set the area while the OS winsize is still stale. We own
    // the size via `apply_size`.
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
            expect_incarnation: None,
            heartbeat: false,
            replaces: None,
        },
    )?;

    // The server's `Hello` is always its first message. Consume it SYNCHRONOUSLY before the input
    // loop and reply with our `Env`, so a fast pane-creation keystroke cannot race ahead of the
    // environment handshake. The SAME reader moves into the reader thread so no buffered bytes
    // are lost.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut pending_first: Option<ServerMsg> = None;
    let mut send_focus = false;
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
                    paste,
                    focus,
                    colors,
                    graphics,
                    ..
                }) => {
                    send_focus = focus;
                    if paste {
                        let _ = guard.enable_paste();
                    }
                    // Server-authoritative: the mouse setting comes from `Hello`, not local config.
                    if mouse {
                        let _ = guard.enable_mouse();
                    }
                    // Reply with our live values for exactly the vars asked, via `var_os`
                    // (`env::vars()` panics on non-UTF-8), keeping only valid UTF-8.
                    let env: Vec<(String, String)> = update_environment
                        .into_iter()
                        .filter_map(|name| {
                            std::env::var_os(&name)
                                .and_then(|v| v.into_string().ok())
                                .map(|v| (name, v))
                        })
                        .collect();
                    // Read here, in the CLIENT, like the env above: the server froze its
                    // environment at birth and may not even run under a GUI.
                    let copad_host = std::env::var("COPAD_SOCKET")
                        .ok()
                        .zip(std::env::var("COPAD_PANEL_ID").ok())
                        .filter(|(s, p)| !s.is_empty() && !p.is_empty());
                    let host_tag = host_tag(copad_host.as_ref());
                    let _ = send(
                        &mut wr,
                        &ClientMsg::Env {
                            vars: env,
                            copad_host,
                            host_tag,
                        },
                    );
                    if colors && let Some((fg, bg)) = guard.colors {
                        let _ = send(&mut wr, &ClientMsg::Colors { fg, bg });
                    }
                    guard.graphics &= graphics;
                    match cell_pixels() {
                        Some((cell_width, cell_height)) if guard.graphics => {
                            let _ = send(
                                &mut wr,
                                &ClientMsg::Graphics {
                                    cell_width,
                                    cell_height,
                                },
                            );
                        }
                        _ => guard.graphics = false,
                    }
                    for k in std::mem::take(&mut guard.typed) {
                        let _ = send(&mut wr, &ClientMsg::Key(k));
                    }
                    break;
                }
                Ok(ServerMsg::Bye) => return Ok(()),
                Ok(other) => {
                    pending_first = Some(other);
                    break;
                }
                Err(_) => continue,
            }
        }
    }

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

    let mut buf = Buffer::empty(RRect::new(0, 0, 1, 1));
    let mut have_frame = false;
    let mut cursor: Option<(u16, u16)> = None;
    // A `full` frame means "repaint everything": clear the ratatui terminal so its diff baseline
    // is wiped, else a cell the real terminal lost lingers as a ghost.
    let mut force_clear = false;
    let mut force_repaint = false;
    // Our record of what the terminal is showing: the single diff baseline (see `emit_view`).
    let mut painted = Buffer::empty(RRect::new(0, 0, 1, 1));
    // Whether the cursor is currently shown, so show/hide is emitted on the edge only. `None` =
    // UNKNOWN: entering the alternate screen does not reset visibility, so assuming "shown" would
    // leave a cursor the shell hid hidden forever.
    let mut cursor_shown: Option<bool> = None;
    let mut cursor_visible = true;
    let mut cursor_shape: u8 = 0;
    let mut shape_sent: u8 = 0;
    let mut image_runs: Vec<crate::proto::ImageRun> = Vec::new();
    let mut image_commands: Vec<u8> = Vec::new();
    let mut images_placed = false;
    let mut resized = false;
    // Client-local silent-repaint period, default OFF: additive to the server-driven cadence, so
    // one client whose OUTER emulator drifts faster can heal itself without changing
    // `reconcile_secs` for every client (`COPAD_MUX_REDRAW_MS`).
    let local_repaint = std::env::var("COPAD_MUX_REDRAW_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::ZERO, Duration::from_millis);
    let mut last_repaint = Instant::now();
    // Periodic winsize reconciliation: a `CEvent::Resize` can be LOST (crossterm coalescing a
    // burst, or an outer terminal updating winsize without a clean SIGWINCH), leaving us
    // composing at a STALE size (seen over SSH from Windows Terminal after sleep/wake). Once a
    // second re-read the tty size and adopt it WHEN IT CHANGES from the last OS value seen.
    // Comparing against the last OS value (not the render size) is deliberate:
    // `probe_true_size` can set the render size ahead of a stale OS winsize, and this poll must
    // not undo it. `last_os_size` advances on every acknowledged OS change (this poll and
    // `CEvent::Resize`), never on a probe. See docs/troubleshooting.md.
    let size_poll = Duration::from_millis(1000);
    let mut last_size_poll = Instant::now();
    let mut last_os_size = (cols, rows);
    // Deeper recovery when the tty winsize itself is STALE (sleep/wake where the outer terminal
    // never propagated the new size to the remote pty): ask the TERMINAL its true size (see
    // `probe_true_size`), triggered by `FocusGained`, when the user returns after a wake. A
    // suspend cannot be observed instead: `Instant` pauses across it.

    loop {
        let mut need_redraw = false;
        // Any size change updates cols+rows here; one `apply_size` at the end drives ratatui and
        // the server.
        let mut size_changed = false;
        let mut probe_needed = false;
        if event::poll(Duration::from_millis(16))? {
            loop {
                match event::read()? {
                    CEvent::Key(k) if k.kind != KeyEventKind::Release => {
                        let k = if guard.keys {
                            k
                        } else {
                            crate::keys::unalias_legacy_c0(k)
                        };
                        let _ = send(&mut wr, &ClientMsg::Key(k));
                    }
                    CEvent::Paste(text) => {
                        let _ = send(&mut wr, &ClientMsg::Paste { text });
                    }
                    CEvent::Mouse(m) => {
                        // Forward wheel + left-click at their cell; the server maps to a pane (the
                        // letterbox is top-left aligned, so coordinates pass through).
                        let kind = match m.kind {
                            MouseEventKind::ScrollUp => Some(MouseKind::ScrollUp),
                            MouseEventKind::ScrollDown => Some(MouseKind::ScrollDown),
                            MouseEventKind::Down(MouseButton::Left) => Some(MouseKind::Click),
                            MouseEventKind::Drag(MouseButton::Left) => Some(MouseKind::Drag),
                            MouseEventKind::Up(MouseButton::Left) => Some(MouseKind::Up),
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
                        // Gate on the OS value actually CHANGING, like the 1s poll, so a delayed
                        // resize event carrying a stale winsize cannot clobber a `probe_true_size`
                        // recovery.
                        if os != last_os_size {
                            last_os_size = os;
                            (cols, rows) = os;
                            size_changed = true;
                        }
                    }
                    CEvent::FocusGained => {
                        probe_needed = true;
                        if send_focus {
                            let _ = send(&mut wr, &ClientMsg::Focus { gained: true });
                        }
                    }
                    CEvent::FocusLost if send_focus => {
                        let _ = send(&mut wr, &ClientMsg::Focus { gained: false });
                    }
                    _ => {}
                }
                if !event::poll(Duration::from_millis(0))? {
                    break;
                }
            }
        }

        // Reconcile a resize event we may have MISSED. Gated on the OS value moving, so a stale
        // winsize cannot undo a `probe_true_size` recovery (see `last_os_size`).
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

        // When a stale OS winsize is LIKELY (focus regain), ask the terminal its true size and
        // reconcile if it disagrees with what we track.
        if probe_needed
            && let Some((w, h)) = probe_true_size()
            && (w, h) != (cols, rows)
        {
            (cols, rows) = (w, h);
            size_changed = true;
        }

        if size_changed {
            apply_size(&mut terminal, &mut wr, cols, rows);
            // `Terminal::resize` clears the screen, so the paint baseline is gone even if the
            // size ended up the same.
            resized = true;
            need_redraw = true;
        }

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
                    // Rebuild wide-char spacer structure so the client buffer matches the server's:
                    // the wire omits trailing spacers, so a wide char that MOVED would leave a stale
                    // width-2 glyph behind (see term.rs `relay_fidelity_pure_delta_churn`).
                    fix_wide_spacers(&mut buf);
                    // Uploads and drops keep their order even when frames are coalesced; the
                    // runs are the latest frame's.
                    for up in &f.uploads {
                        image_commands.extend(upload_commands(up));
                        guard.images_held.insert(up.id);
                    }
                    for id in &f.drops {
                        let _ = write!(image_commands, "\u{1b}_Ga=d,d=I,i={id},q=2\u{1b}\\");
                        guard.images_held.remove(id);
                    }
                    if let Some(runs) = f.images {
                        image_runs = runs;
                    }
                    cursor = f.cursor;
                    cursor_visible = f.cursor_visible;
                    cursor_shape = f.cursor_shape;
                    have_frame = true;
                    dirty = true;
                    // Sticky until the draw below honours it, so a repaint frame coalesced with
                    // later deltas is still acted on.
                    force_repaint |= f.repaint;
                }
                // Drag-selection copy: set the clipboard via OSC 52 through this client's terminal
                // (works over SSH), written straight to stdout outside ratatui's draw.
                Ok(ServerMsg::Copy { text }) => {
                    let _ = write_osc52(&text);
                }
                Ok(ServerMsg::Bell) => {
                    let mut out = io::stdout();
                    let _ = out.write_all(b"\x07");
                    let _ = out.flush();
                }
                Ok(ServerMsg::Title { text }) => {
                    let mut out = io::stdout();
                    if !guard.title_pushed {
                        let _ = out.write_all(b"\x1b[22;0t");
                        guard.title_pushed = true;
                    }
                    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                    let _ = write!(out, "\u{1b}]2;{clean}\u{7}");
                    let _ = out.flush();
                }
                Ok(ServerMsg::Hello { .. }) => {}
                Ok(ServerMsg::Bye) => return Ok(()),
                // Only answer a Command / Ping, which this client never sends.
                Ok(ServerMsg::Reply { .. } | ServerMsg::Pong { .. }) => {}
                // Only answers an `expect_incarnation`, which this client never sends.
                Ok(ServerMsg::Refused { code, message }) => {
                    return Err(io::Error::other(format!("{code}: {message}")));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()), // server gone
            }
        }

        // Blit the server frame into the top-left, blanking the letterbox margin. A client-local
        // silent repaint drives a draw too, so it fires on a screen so idle that no frame arrives.
        if !local_repaint.is_zero() && last_repaint.elapsed() >= local_repaint {
            force_repaint = true;
            need_redraw = true;
        }

        if (dirty || need_redraw) && have_frame {
            // A `full` frame resets the diff baseline: clear so the draw re-emits every cell. This
            // is the ONLY path that clears (its cells were applied over an EMPTY buffer); the
            // self-heal repaint never does.
            let cleared = force_clear;
            if cleared {
                terminal.clear()?;
                force_clear = false;
            }
            // What we believe is ON THE TERMINAL. Reset wherever the screen was blanked under us
            // (a `full` frame's clear, a resize).
            let view = RRect::new(0, 0, cols, rows);
            if baseline_is_stale(cleared, resized, painted.area, view) {
                painted = Buffer::empty(view);
            }
            resized = false;
            let mut next = Buffer::empty(painted.area);
            blit_view(&buf, &mut next);
            // One renderer: every cell after a clear or self-heal repaint, otherwise just what
            // changed, through the SAME emitter.
            let baseline = if cleared || force_repaint {
                None
            } else {
                Some(&painted)
            };
            // Synchronized output (`?2026`): the frame shows only once complete, so fast scrolling
            // never tears; terminals without it ignore the mode.
            write!(terminal.backend_mut(), "\u{1b}[?2026h")?;
            emit_view(&mut terminal, baseline, &next, cursor)?;
            painted = next;
            if guard.graphics
                && (!image_runs.is_empty() || images_placed || !image_commands.is_empty())
            {
                let out = terminal.backend_mut();
                out.write_all(&std::mem::take(&mut image_commands))?;
                let cell = cell_pixels().unwrap_or((10, 20));
                out.write_all(&place_images(&image_runs, cols, rows, cell))?;
                images_placed = !image_runs.is_empty();
                if let Some((cx, cy)) = cursor.filter(|&(cx, cy)| cx < cols && cy < rows) {
                    write!(out, "\u{1b}[{};{}H", cy + 1, cx + 1)?;
                }
            }
            // A hidden cursor is still PLACED (by `emit_view`): an IME opens its composition
            // window at the terminal cursor whether or not it is drawn.
            let want_cursor =
                cursor_visible && cursor.is_some_and(|(cx, cy)| cx < cols && cy < rows);
            if cursor_shape != shape_sent {
                write!(terminal.backend_mut(), "\u{1b}[{cursor_shape} q")?;
                shape_sent = cursor_shape;
                guard.cursor_shaped |= cursor_shape != 0;
            }
            if cursor_visibility_needs_stating(cursor_shown, want_cursor) {
                if want_cursor {
                    terminal.show_cursor()?;
                } else {
                    terminal.hide_cursor()?;
                }
                cursor_shown = Some(want_cursor);
            }
            write!(terminal.backend_mut(), "\u{1b}[?2026l")?;
            Backend::flush(terminal.backend_mut())?;
            if force_repaint || cleared {
                last_repaint = Instant::now();
            }
            force_repaint = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MidKey;
    use super::{
        AttachOpts, PROBE_CORNER, PROBE_MAX, base64, connect_only, da1_answered, decode_typed,
        emit_view, ends_mid_key, io, osc_color, repaint_all, size_from_clamped_cursor,
        without_replies,
    };
    use ratatui::backend::CrosstermBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::{Position, Rect};
    use ratatui::style::{Color, Style};
    use ratatui::{Terminal, TerminalOptions, Viewport};
    use std::cell::RefCell;
    use std::rc::Rc;

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
    fn colour_replies_parse_at_any_precision_and_da1_ends_the_wait() {
        let reply = b"\x1b]10;rgb:cdcd/d6d6/f4f4\x1b\\\x1b]11;rgb:1e/1e/2e\x07\x1b[?62;22c";
        assert_eq!(osc_color(reply, b"10"), Some((0xcd, 0xd6, 0xf4)));
        assert_eq!(osc_color(reply, b"11"), Some((0x1e, 0x1e, 0x2e)));
        assert_eq!(
            osc_color(b"\x1b]11;rgb:f/0/8\x07", b"11"),
            Some((255, 0, 136))
        );
        assert_eq!(osc_color(b"\x1b]11;rgb:zz/0/0\x07", b"11"), None);
        assert_eq!(
            osc_color(b"\x1b[?62c", b"11"),
            None,
            "a terminal that ignored the query"
        );
        assert!(da1_answered(reply));
        assert!(da1_answered(b"\x1b[?6c"));
        assert!(!da1_answered(b"\x1b]11;rgb:1e/1e/2e\x07"));
        assert!(!da1_answered(b"\x1b[?62;22"), "not finished yet");
        let mixed = b"ab\x1b]10;rgb:1/2/3\x07c\x1b]11;rgb:4/5/6\x1b\\\x1b[?62;22cd\x1b[A";
        assert_eq!(without_replies(mixed), b"abcd\x1b[A");
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let keys: Vec<_> = decode_typed("aZ한\r\x03\x1bx\x1b[A\x7f".as_bytes())
            .into_iter()
            .map(|k| (k.code, k.modifiers))
            .collect();
        assert_eq!(
            keys,
            [
                (KeyCode::Char('a'), KeyModifiers::NONE),
                (KeyCode::Char('Z'), KeyModifiers::SHIFT),
                (KeyCode::Char('한'), KeyModifiers::NONE),
                (KeyCode::Enter, KeyModifiers::NONE),
                (KeyCode::Char('c'), KeyModifiers::CONTROL),
                (KeyCode::Char('x'), KeyModifiers::ALT),
                (KeyCode::Up, KeyModifiers::NONE),
                (KeyCode::Backspace, KeyModifiers::NONE),
            ]
        );
        let keys: Vec<_> = decode_typed(b"\x00\x1c\x1f")
            .into_iter()
            .map(|k| (k.code, k.modifiers))
            .collect();
        assert_eq!(
            keys,
            [
                (KeyCode::Char(' '), KeyModifiers::CONTROL),
                (KeyCode::Char('\\'), KeyModifiers::CONTROL),
                (KeyCode::Char('_'), KeyModifiers::CONTROL),
            ]
        );
        let short = Some(MidKey::LoneEsc);
        let begun = Some(MidKey::Begun);
        assert_eq!(ends_mid_key(b"a\x1b"), short, "maybe the Esc key itself");
        assert_eq!(ends_mid_key(b"a\x1b[1;5"), begun);
        assert_eq!(ends_mid_key("한".as_bytes().split_last().unwrap().1), begun);
        assert_eq!(ends_mid_key(b"a\x1b[A"), None);
        assert_eq!(ends_mid_key("a한".as_bytes()), None);
        let keys: Vec<_> =
            decode_typed(b"\x1b[1;5D\x1bOP\x1b[15~\x1b[24;2~\x1b[3~\x1b[5~\x1b[Z\x1b[1;3H")
                .into_iter()
                .map(|k| (k.code, k.modifiers))
                .collect();
        assert_eq!(
            keys,
            [
                (KeyCode::Left, KeyModifiers::CONTROL),
                (KeyCode::F(1), KeyModifiers::NONE),
                (KeyCode::F(5), KeyModifiers::NONE),
                (KeyCode::F(12), KeyModifiers::SHIFT),
                (KeyCode::Delete, KeyModifiers::NONE),
                (KeyCode::PageUp, KeyModifiers::NONE),
                (KeyCode::BackTab, KeyModifiers::SHIFT),
                (KeyCode::Home, KeyModifiers::ALT),
            ]
        );
    }

    #[test]
    fn a_delta_repaints_the_neighbour_an_over_wide_glyph_can_clobber() {
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
        // The one cell with no neighbour to repair: a terminal measuring this glyph as two columns
        // declines to write it, so the old content would survive. Erasing first makes the result
        // independent of it.
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
        // Autowrap is off, so an overflow at the last column is clamped within its row: the next
        // row's first cell needs no repair.
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
        assert!(!cursor_visibility_needs_stating(Some(true), true));
        assert!(!cursor_visibility_needs_stating(Some(false), false));
        assert!(cursor_visibility_needs_stating(Some(false), true));
        assert!(cursor_visibility_needs_stating(Some(true), false));
    }

    #[test]
    fn the_paint_baseline_is_thrown_away_whenever_the_screen_was_blanked() {
        use super::baseline_is_stale;
        let view = Rect::new(0, 0, 80, 24);
        assert!(!baseline_is_stale(false, false, view, view));
        assert!(baseline_is_stale(true, false, view, view));
        assert!(baseline_is_stale(
            false,
            false,
            Rect::new(0, 0, 80, 23),
            view
        ));
        assert!(
            baseline_is_stale(false, true, view, view),
            "a same-size resize still clears the screen"
        );
    }

    #[test]
    fn an_ordinary_frame_anchors_exactly_like_a_repaint_does() {
        // The visible bug this guards: `ls` output landing in one place then jumping sideways.
        // Deltas and the repaint must anchor identically, which one emitter guarantees.
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
        // One glyph must never skew the rest of a row on repaint: `CrosstermBackend::draw`
        // positions adjacent cells by the terminal's own cursor advance, so an icon the font draws
        // wider would shift the row on every tick. comux cannot read the font, so it re-anchors
        // after any non-ASCII glyph.
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

        assert!(
            out.contains("\u{1b}[1;4H"),
            "the cell after a non-ASCII glyph must be re-anchored: {out:?}"
        );
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

        // (1) The frame is BIGGER than the viewport (right after this terminal shrinks): emitting
        // it would write past the edge and wrap.
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

        // (1b) A TWO-COLUMN glyph STARTING on the last viewport column is just as bad: its second
        // half goes off the edge and the terminal wraps. A retained frame across a shrink does this.
        let mut wide = Buffer::empty(Rect::new(0, 0, 6, 2));
        wide.set_string(0, 0, "가", Style::default()); // fully inside — must survive
        wide.set_string(3, 1, "가", Style::default()); // starts on the last viewport column
        let out = paint(Rect::new(0, 0, 4, 2), &wide, None);
        assert_eq!(
            out.matches('가').count(),
            1,
            "the glyph that fits is emitted and the one that would spill is not: {out:?}"
        );

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
        // The two properties that let this run on a timer by default: it re-emits EVERYTHING
        // (ratatui's belief that cells are correct is what went stale), and it never clears
        // (`Clear(All)` flashes a blank frame).
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
        for clear in ["[2J", "[1J", "[0J", "[3J", "[J"] {
            assert!(
                !out.contains(clear),
                "a silent repaint must not clear (found {clear}): {out:?}"
            );
        }
        assert!(
            out.contains("\u{1b}[2;4H"),
            "cursor restored to the frame position (1-based row 2, col 4): {out:?}"
        );
    }

    #[test]
    fn size_from_clamped_cursor_converts_and_guards() {
        assert_eq!(size_from_clamped_cursor(79, 23), Some((80, 24)));
        assert_eq!(size_from_clamped_cursor(0, 0), Some((1, 1)));
        assert_eq!(
            size_from_clamped_cursor(PROBE_MAX - 1, PROBE_MAX - 1),
            Some((PROBE_MAX, PROBE_MAX))
        );
        assert_eq!(size_from_clamped_cursor(PROBE_CORNER, PROBE_CORNER), None);
        // Absurd values BELOW the corner but at/above the cap are also rejected — the
        // memory-exhaustion guard (else ~1e8 cells could reach Terminal::resize).
        assert_eq!(size_from_clamped_cursor(9997, 9997), None);
        assert_eq!(size_from_clamped_cursor(PROBE_MAX, 10), None);
        assert_eq!(size_from_clamped_cursor(10, PROBE_MAX), None);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
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
        assert!(!sock.exists(), "connect_only must not create {sock:?}");
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
        assert!(!AttachOpts::default().no_spawn);
    }
}
