//! The thin mux **client**: connects to the server (spawning one if none is
//! running), forwards key/resize events, and blits the server's cell frames to the
//! local terminal. Detaching (`Ctrl-b d`) or losing the connection just exits the
//! client — the server + shells live on. This is what `copad-mux` (bare) runs.

use std::io::{self, BufRead, BufReader, Stdout, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event as CEvent, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Position, Rect as RRect};

use crate::control::socket_path;
use crate::proto::{ClientMsg, ServerMsg};

/// Restores the host terminal (raw mode off + leave alt screen) on drop — so a
/// panic or an abrupt server exit never leaves the user's terminal wedged.
struct TermGuard;

impl TermGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
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

/// Connect to the running server, spawning a detached one if none answers, then run
/// the attach loop until detach / server exit.
pub fn run() -> io::Result<()> {
    let sock = socket_path();
    let stream = connect_or_spawn(&sock)?;
    run_attached(stream)
}

/// Connect to `sock`; if nothing is listening, spawn a server and retry with backoff.
fn connect_or_spawn(sock: &Path) -> io::Result<UnixStream> {
    if let Ok(s) = UnixStream::connect(sock) {
        return Ok(s);
    }
    spawn_server()?;
    // The server takes ~a few ms to flock + bind. Racing clients each spawn one, but
    // only the flock winner binds; everyone connects to it. ~2s budget.
    for _ in 0..80 {
        if let Ok(s) = UnixStream::connect(sock) {
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "copad-mux server did not come up",
    ))
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
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    let _guard = TermGuard::enter()?;
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

    // Client-side framebuffer, kept in lockstep with the server's composition.
    let mut buf = Buffer::empty(RRect::new(0, 0, cols, rows));
    let mut cursor: Option<(u16, u16)> = None;

    loop {
        // 1) forward input
        if event::poll(Duration::from_millis(16))? {
            loop {
                match event::read()? {
                    CEvent::Key(k) if k.kind != KeyEventKind::Release => {
                        let _ = send(&mut wr, &ClientMsg::Key(k));
                    }
                    CEvent::Resize(w, h) => {
                        cols = w.max(1);
                        rows = h.max(1);
                        // Rebuild the local buffer; the server sends a matching full
                        // frame next, and stale-size frames are dropped until then.
                        buf = Buffer::empty(RRect::new(0, 0, cols, rows));
                        let _ = send(&mut wr, &ClientMsg::Resize { cols, rows });
                    }
                    _ => {}
                }
                if !event::poll(Duration::from_millis(0))? {
                    break;
                }
            }
        }

        // 2) apply incoming frames
        let mut dirty = false;
        loop {
            match rx.try_recv() {
                Ok(ServerMsg::Frame(f)) => {
                    // Drop frames from before the last local resize (wrong size).
                    if f.cols != cols || f.rows != rows {
                        continue;
                    }
                    if f.full {
                        buf = Buffer::empty(RRect::new(0, 0, cols, rows));
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
                    cursor = f.cursor;
                    dirty = true;
                }
                Ok(ServerMsg::Bye) => return Ok(()),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()), // server gone
            }
        }

        // 3) draw (ratatui diffs our buffer against the TTY's last frame)
        if dirty {
            terminal.draw(|frame| {
                *frame.buffer_mut() = buf.clone();
                if let Some((x, y)) = cursor {
                    frame.set_cursor_position(Position::new(x, y));
                }
            })?;
        }
    }
}
