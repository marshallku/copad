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

    // Reader thread: server frames → channel; dropping the sender on EOF signals the
    // main loop (recv → Disconnected) that the server went away.
    let (tx, rx) = mpsc::channel::<ServerMsg>();
    {
        let rd = stream.try_clone()?;
        std::thread::spawn(move || {
            let mut reader = BufReader::new(rd);
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
    // Self-healing full repaint. A nested/custom outer emulator (e.g. copad hosting comux)
    // can drift from ratatui's incremental cell output over time — the outer terminal renders
    // a cell differently than ratatui's cached previous-buffer believes, and the diff then
    // never repaints it (persistent ghost/garble). A full repaint corrects it, so do one
    // automatically at a low rate WHILE frames are actively flowing, so drift never lingers
    // longer than this interval. It's a purely LOCAL client→terminal re-emit (no server
    // round-trip), and copad renders on vsync so the clear+repaint lands in one burst with no
    // visible flash. Tune / disable with `COPAD_MUX_REDRAW_MS` (0 = off).
    let self_heal = std::env::var("COPAD_MUX_REDRAW_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::from_millis(1000), Duration::from_millis);
    let mut last_repaint = Instant::now();

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
                Ok(ServerMsg::Hello { mouse }) => {
                    // Server-authoritative: only now (not from local config) do we decide
                    // whether to capture the mouse, so every client agrees with the server.
                    if mouse {
                        let _ = guard.enable_mouse();
                    }
                }
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
