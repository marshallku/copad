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

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Position, Rect as RRect};
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
        execute!(io::stdout(), EnterAlternateScreen)?;
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
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Connect to the running server, spawning a detached one if none answers, then run
/// the attach loop until detach / server exit.
pub fn run() -> io::Result<()> {
    // Print any config warnings NOW, before raw/alt-screen — an auto-spawned server's
    // stderr is /dev/null, so this is the user's reliable view of config diagnostics.
    // (The effective mouse setting is the SERVER's, delivered in its `Hello`; the client
    // never applies its own local config to behavior — only surfaces its warnings.)
    let (_cfg, warnings) = crate::config::MuxConfig::load();
    for w in &warnings {
        eprintln!("comux config: {w}");
    }
    let sock = socket_path();
    let stream = connect_or_spawn(&sock)?;
    run_attached(stream)
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

/// The attach loop: forward input, apply incoming frames, draw.
fn run_attached(stream: UnixStream) -> io::Result<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default_hook(info);
    }));

    let mut guard = TermGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;
    let size = terminal.size()?;
    let (mut cols, mut rows) = (size.width.max(1), size.height.max(1));

    let mut wr = stream.try_clone()?;
    send(&mut wr, &ClientMsg::Attach { cols, rows })?;

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
                    let _ = send(&mut wr, &ClientMsg::Env { vars: env });
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
    // Optional self-healing full repaint, DEFAULT OFF. The wide-char spacer desync that used to
    // force this is now root-fixed (see `fix_wide_spacers`), so the periodic clear+repaint —
    // whose `Clear(All)` flashes a blank frame each tick (visible flicker) — is no longer worth
    // its cost by default. Kept as an opt-in escape hatch for any residual OUTER-emulator drift
    // (e.g. copad-term GPU damage tracking): set `COPAD_MUX_REDRAW_MS=<ms>` to re-enable. The
    // manual `Ctrl-b r` redraw covers the occasional case without the steady flicker.
    let self_heal = std::env::var("COPAD_MUX_REDRAW_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::ZERO, Duration::from_millis);
    let mut last_repaint = Instant::now();
    // Periodic winsize reconciliation. A terminal normally delivers a `CEvent::Resize` when it
    // is resized, but that event can be LOST — crossterm coalescing a rapid burst, or an outer
    // terminal that updated the tty's winsize without a clean SIGWINCH to us. When it is lost we
    // keep composing at the STALE size and the status bar lands off the true bottom (reported
    // over SSH from Windows Terminal after a sleep/wake). So once a second we re-read the tty
    // size directly and, if it moved without an event, send the resize ourselves. NOTE: this
    // only recovers cases where the tty winsize (TIOCGWINSZ) is actually current — if the outer
    // terminal never propagated the new size to the remote pty at all (the sleep/wake case where
    // sshd got no window-change), the OS still reports the old size and only a real resize
    // (re-maximize) or reattach fixes it. See docs/troubleshooting.md.
    let size_poll = Duration::from_millis(1000);
    let mut last_size_poll = Instant::now();

    loop {
        // 1) forward input
        let mut need_redraw = false;
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
                        cols = w.max(1);
                        rows = h.max(1);
                        // The server re-fits to the smallest client; our frame buffer
                        // follows the SERVER size, so don't rebuild it — just
                        // re-letterbox into the new terminal size on the next draw.
                        let _ = send(&mut wr, &ClientMsg::Resize { cols, rows });
                        need_redraw = true;
                    }
                    _ => {}
                }
                if !event::poll(Duration::from_millis(0))? {
                    break;
                }
            }
        }

        // 1b) reconcile a resize event we may have MISSED: re-read the tty size once a second
        // and, if it moved without a `CEvent::Resize`, drive the same path an event would.
        if last_size_poll.elapsed() >= size_poll {
            last_size_poll = Instant::now();
            if let Ok(sz) = terminal.size() {
                let (w, h) = (sz.width.max(1), sz.height.max(1));
                if (w, h) != (cols, rows) {
                    cols = w;
                    rows = h;
                    let _ = send(&mut wr, &ClientMsg::Resize { cols, rows });
                    need_redraw = true;
                }
            }
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
                            cell.set_symbol(&c.sym);
                            cell.fg = c.fg;
                            cell.bg = c.bg;
                            cell.modifier = c.mods;
                            cell.set_skip(c.skip);
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
        if (dirty || need_redraw) && have_frame {
            // Force a full repaint on a `full` frame OR when the self-heal interval has
            // elapsed since the last one (drift correction — see `self_heal` above).
            if !force_clear && !self_heal.is_zero() && last_repaint.elapsed() >= self_heal {
                force_clear = true;
            }
            // A `full` frame / self-heal resets the diff baseline: clear the screen + ratatui's
            // cached previous-buffer so the upcoming draw re-emits every cell (no lingering ghost).
            if force_clear {
                terminal.clear()?;
                force_clear = false;
                last_repaint = Instant::now();
            }
            let src = buf.clone();
            let cur = cursor;
            terminal.draw(|frame| {
                let area = frame.area();
                let out = frame.buffer_mut();
                for y in 0..area.height {
                    for x in 0..area.width {
                        let Some(dst) = out.cell_mut(Position::new(x, y)) else {
                            continue;
                        };
                        if x < src.area.width && y < src.area.height {
                            if let Some(s) = src.cell(Position::new(x, y)) {
                                *dst = s.clone();
                            }
                        } else {
                            dst.reset(); // letterbox margin
                        }
                    }
                }
                if let Some((cx, cy)) = cur
                    && cx < area.width
                    && cy < area.height
                {
                    frame.set_cursor_position(Position::new(cx, cy));
                }
            })?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::base64;

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
}
