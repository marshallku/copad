//! The headless mux **server**: owns the [`App`] (authoritative `State` + PTYs),
//! binds the shared socket, serves both one-shot `ctl` requests and streaming client
//! attachments, and keeps running across client detaches so shells survive the
//! terminal that launched them. Started implicitly by [`crate::client`] or explicitly
//! via `copad-mux server`.
//!
//! Ownership is atomic: a would-be server takes an exclusive `flock` on
//! `<runtime>/lock`; only the lock holder may unlink a stale socket + bind (no
//! TOCTOU race between competing starts). Same-uid peers only (`getpeereid`).
//!
//! Multiple clients may attach at once (tmux-style shared view): the app is sized to
//! the SMALLEST attached client so all of them see the whole thing, one composite is
//! broadcast to every client (each diffed against its own baseline), and all share
//! input. Detach (`Ctrl-b d`) removes only the client that pressed it.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RRect;

use crate::control::{self, runtime_dir, socket_path};
use crate::proto::{ClientMsg, FrameMsg, ServerMsg, WireCell};
use crate::tui::{App, KeyAction};

/// ~30 Hz frame cadence. The PTY runtime exposes no damage signal, so we render
/// unconditionally on this tick and rely on the buffer diff to make idle output
/// (an empty delta) free on the wire.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// A message funneled to the single-writer main loop from a connection thread.
enum Incoming {
    /// A one-shot control request + its reply channel.
    Ctl {
        req: control::Req,
        reply: Sender<control::Resp>,
    },
    /// A streaming client opened (its first line was `attach`).
    Attach {
        id: u64,
        cols: u16,
        rows: u16,
        out: SyncSender<ServerMsg>,
        /// A clone of the connection, so the main loop can `shutdown` it to force a
        /// detach even when the bounded frame queue can't take a `Bye`.
        conn: UnixStream,
    },
    /// A forwarded message from an attached client.
    Client { id: u64, msg: ClientMsg },
    /// A client connection closed.
    Disconnect { id: u64 },
}

/// The currently-attached streaming client (v1: at most one at a time).
struct Client {
    id: u64,
    out: SyncSender<ServerMsg>,
    /// A clone of the socket, shut down on detach so the client (and the server's
    /// own reader thread) unblock even if `Bye` couldn't be queued.
    conn: UnixStream,
    /// The buffer the client is known to hold (diff baseline). Advanced only when a
    /// frame is actually enqueued, so a dropped (channel-full) frame re-diffs and
    /// catches up without desync.
    last: Buffer,
    /// The next frame must be a `full` baseline repaint (set on attach + resize).
    needs_full: bool,
    /// The cursor position the client last received, so a cursor-only move (no cell
    /// change) still gets shipped instead of leaving a stale cursor.
    last_cursor: Option<(u16, u16)>,
    /// This client's OWN `Ctrl-b` prefix state — per-connection so a chord can't span
    /// clients when input is shared.
    prefix: bool,
    epoch: u64,
    cols: u16,
    rows: u16,
}

/// The uid on the other end of a Unix socket (same on Linux + macOS via getpeereid).
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    (rc == 0).then_some(uid)
}

/// Take the exclusive server lock (held for the process lifetime). `Err(AddrInUse)`
/// when another server already holds it — the caller should exit quietly.
fn acquire_lock(path: &Path) -> io::Result<File> {
    // The lock file is a pure flock anchor — never read/written, so don't truncate.
    let f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another copad-mux server is already running",
        ));
    }
    Ok(f)
}

/// Prepare the private runtime dir (0700) unless the caller manages the socket path
/// via `$COPAD_MUX_SOCK`; returns `(socket_path, lock_path)`.
fn prepare_paths() -> io::Result<(PathBuf, PathBuf)> {
    let sock = socket_path();
    if std::env::var_os("COPAD_MUX_SOCK").is_none() {
        let dir = runtime_dir();
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let lock = sock.with_extension("lock");
    Ok((sock, lock))
}

/// Run the server to completion (exits when its last shell exits, on `kill-server`,
/// or if another server already owns the lock).
pub fn run() -> io::Result<()> {
    let (sock, lock_path) = prepare_paths()?;
    // Atomic ownership: only the flock holder may touch the socket file.
    let _lock = match acquire_lock(&lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Ok(()), // lost the race
        Err(e) => return Err(e),
    };
    // Safe to clear a stale socket now — the lock guarantees we are the only server.
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));

    let sock_env = vec![(
        "COPAD_MUX_SOCK".to_string(),
        sock.to_string_lossy().to_string(),
    )];
    // Headless default size until the first client attaches (then reflowed).
    let mut app = App::new(80, 24, sock_env)?;

    let (tx, rx) = mpsc::channel::<Incoming>();
    spawn_accept_loop(listener, tx);

    // Multiple clients may attach at once (tmux-style shared view). The app is sized
    // to the SMALLEST attached client so everyone sees the whole thing; the same
    // composite is broadcast to all (each with its own diff baseline).
    let mut clients: Vec<Client> = Vec::new();
    let mut kill = false;
    let mut last_frame = Instant::now();

    loop {
        match rx.recv_timeout(FRAME_INTERVAL) {
            Ok(msg) => {
                handle_incoming(msg, &mut app, &mut clients, &mut kill);
                // Bound the drain so a key/ctl flood can't starve rendering, PTY
                // reaping, or shutdown — fall back to the frame tick after a batch.
                let mut budget = 256u32;
                while budget > 0
                    && let Ok(m) = rx.try_recv()
                {
                    handle_incoming(m, &mut app, &mut clients, &mut kill);
                    budget -= 1;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if kill {
            break;
        }
        // The server itself quits only when the last shell exits (app empty) — NOT
        // when the last client detaches (the whole point of detach: shells live on).
        if app.reap_exited() {
            break;
        }
        app.reconcile_popup();
        app.maybe_refresh_labels();
        if last_frame.elapsed() >= FRAME_INTERVAL {
            last_frame = Instant::now();
            push_frames(&mut app, &mut clients);
        }
    }

    for c in clients.drain(..) {
        detach_client(c);
    }
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

/// Re-derive the shared viewport = the SMALLEST attached client (min cols, min rows),
/// resize the app to it, and — if the size changed — force a full repaint to every
/// client. With no clients attached the size freezes (G3). Returns nothing; callers
/// push frames afterwards.
fn recompute_viewport(app: &mut App, clients: &mut [Client]) {
    if clients.is_empty() {
        return; // detached: freeze at the last size
    }
    let cols = clients.iter().map(|c| c.cols).min().unwrap_or(80).max(1);
    let rows = clients.iter().map(|c| c.rows).min().unwrap_or(24).max(1);
    let (cur_c, cur_r) = app.size();
    if (cols, rows) != (cur_c, cur_r) {
        app.resize(cols, rows);
        for c in clients.iter_mut() {
            c.needs_full = true;
            c.last = Buffer::empty(RRect::new(0, 0, cols, rows));
        }
    }
}

/// Accept connections forever, assigning each a unique (never-reused) id and a
/// handler thread that funnels into `tx`.
fn spawn_accept_loop(listener: UnixListener, tx: Sender<Incoming>) {
    std::thread::spawn(move || {
        let next_id = AtomicU64::new(1);
        for stream in listener.incoming().flatten() {
            let id = next_id.fetch_add(1, Ordering::SeqCst);
            let tx = tx.clone();
            std::thread::spawn(move || handle_conn(stream, id, tx));
        }
    });
}

/// Read a connection's first line to select its role: `ctl` (one-shot request/reply)
/// or `attach` (streaming client). Rejects cross-uid peers.
fn handle_conn(stream: UnixStream, id: u64, tx: Sender<Incoming>) {
    // Fail CLOSED: reject cross-uid peers AND peers whose credentials can't be
    // established (this socket permits input injection, takeover, and shutdown).
    match peer_uid(&stream) {
        Some(peer) if peer == unsafe { libc::getuid() } => {}
        _ => return,
    }
    let Ok(rd) = stream.try_clone() else { return };
    let mut reader = BufReader::new(rd);
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let first = first.trim().to_string();
    if first.is_empty() {
        return;
    }
    // A `{"cmd":…}` line is a control request; a `{"t":"attach",…}` opens a stream.
    if let Ok(req) = serde_json::from_str::<control::Req>(&first) {
        serve_ctl(Some(req), reader, stream, tx);
    } else if let Ok(ClientMsg::Attach { cols, rows }) = serde_json::from_str::<ClientMsg>(&first) {
        serve_client(id, cols, rows, reader, stream, tx);
    } else {
        let mut w = stream;
        let _ = writeln!(
            w,
            "{}",
            json(control::Resp::err("bad hello (expected a cmd or attach)"))
        );
    }
}

fn json<T: serde::Serialize>(v: T) -> String {
    serde_json::to_string(&v).unwrap_or_else(|_| "{\"ok\":false}".to_string())
}

/// One-shot control loop: for each request line, round-trip through the main loop.
fn serve_ctl(
    mut pending: Option<control::Req>,
    mut reader: BufReader<UnixStream>,
    mut writer: UnixStream,
    tx: Sender<Incoming>,
) {
    loop {
        let req = match pending.take() {
            Some(r) => r,
            None => {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                match serde_json::from_str::<control::Req>(t) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = writeln!(
                            writer,
                            "{}",
                            json(control::Resp::err(format!("bad request: {e}")))
                        );
                        let _ = writer.flush();
                        continue;
                    }
                }
            }
        };
        let (rtx, rrx) = mpsc::channel();
        if tx.send(Incoming::Ctl { req, reply: rtx }).is_err() {
            return;
        }
        let resp = rrx
            .recv()
            .unwrap_or_else(|_| control::Resp::err("mux shutting down"));
        if writeln!(writer, "{}", json(resp)).is_err() {
            return;
        }
        let _ = writer.flush();
    }
}

/// Streaming client session: register, spawn a writer thread draining frames to the
/// socket, and forward every subsequent `ClientMsg` to the main loop.
fn serve_client(
    id: u64,
    cols: u16,
    rows: u16,
    mut reader: BufReader<UnixStream>,
    writer: UnixStream,
    tx: Sender<Incoming>,
) {
    // A clone the main loop can shut down to force-detach this client reliably.
    let Ok(conn) = writer.try_clone() else { return };
    // bounded(1): a slow/suspended client can never grow the server's memory —
    // frames coalesce (the main loop skips + re-diffs on the next tick).
    let (out_tx, out_rx) = mpsc::sync_channel::<ServerMsg>(1);
    if tx
        .send(Incoming::Attach {
            id,
            cols,
            rows,
            out: out_tx,
            conn,
        })
        .is_err()
    {
        return;
    }
    let mut wstream = writer;
    let writer_handle = std::thread::spawn(move || {
        for msg in out_rx {
            let is_bye = matches!(msg, ServerMsg::Bye);
            if writeln!(wstream, "{}", json(&msg)).is_err() || wstream.flush().is_err() {
                break;
            }
            if is_bye {
                break;
            }
        }
    });

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
        match serde_json::from_str::<ClientMsg>(t) {
            Ok(ClientMsg::Attach { .. }) => {} // a second attach on the same conn is ignored
            Ok(msg) => {
                if tx.send(Incoming::Client { id, msg }).is_err() {
                    break;
                }
            }
            Err(_) => {}
        }
    }
    let _ = tx.send(Incoming::Disconnect { id });
    let _ = writer_handle.join();
}

/// Detach a client for good: send `Bye` (best effort, may not fit the bounded queue)
/// AND shut the socket down so both the client and the server's own reader thread
/// unblock via EOF — guaranteeing the client leaves even when the queue is full.
fn detach_client(c: Client) {
    let _ = c.out.try_send(ServerMsg::Bye);
    let _ = c.conn.shutdown(std::net::Shutdown::Both);
}

/// Apply one funneled message to the app / clients on the single-writer loop.
fn handle_incoming(msg: Incoming, app: &mut App, clients: &mut Vec<Client>, kill: &mut bool) {
    match msg {
        Incoming::Ctl { req, reply } => {
            if matches!(req, control::Req::KillServer) {
                *kill = true;
                let _ = reply.send(control::Resp::ok());
                return;
            }
            let resp = app.handle_control(&req);
            let _ = reply.send(resp);
        }
        Incoming::Attach {
            id,
            cols,
            rows,
            out,
            conn,
        } => {
            // Shared attach: ADD the client (no takeover), then re-fit to the smallest.
            clients.push(Client {
                id,
                out,
                conn,
                last: Buffer::empty(RRect::new(0, 0, cols.max(1), rows.max(1))),
                needs_full: true,
                last_cursor: None,
                prefix: false,
                epoch: 0,
                cols,
                rows,
            });
            recompute_viewport(app, clients);
        }
        Incoming::Client { id, msg } => {
            // Only accept from a currently-attached client (ignore stale ids).
            if !clients.iter().any(|c| c.id == id) {
                return;
            }
            match msg {
                ClientMsg::Key(k) => {
                    // All attached clients share input (tmux-style), but each carries
                    // its OWN prefix state so a `Ctrl-b` from one client can't be
                    // completed by another's key. Detach removes ONLY the client that
                    // pressed the chord; the others keep going.
                    let mut action = KeyAction::Continue;
                    if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                        action = app.feed_key(k, &mut c.prefix);
                    }
                    if action == KeyAction::Detach
                        && let Some(pos) = clients.iter().position(|c| c.id == id)
                    {
                        detach_client(clients.remove(pos));
                        recompute_viewport(app, clients);
                    }
                }
                ClientMsg::Mouse { x, y, kind } => {
                    // Scroll the pane under the cursor / click-to-focus — shared, so
                    // any client's wheel drives the one composite.
                    app.mouse_at(x, y, kind);
                }
                ClientMsg::Resize { cols, rows } => {
                    if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                        c.cols = cols;
                        c.rows = rows;
                    }
                    recompute_viewport(app, clients);
                }
                ClientMsg::Detach => {
                    if let Some(pos) = clients.iter().position(|c| c.id == id) {
                        detach_client(clients.remove(pos));
                        recompute_viewport(app, clients);
                    }
                }
                ClientMsg::Attach { .. } => {}
            }
        }
        Incoming::Disconnect { id } => {
            // The socket is already gone — drop the client WITHOUT another shutdown.
            if let Some(pos) = clients.iter().position(|c| c.id == id) {
                clients.remove(pos);
                recompute_viewport(app, clients);
            }
        }
    }
}

/// Render the app ONCE and broadcast the changed cells (or a full baseline) to every
/// attached client — each diffed against its OWN last-sent buffer, so a freshly
/// attached client gets a full frame while up-to-date ones get small deltas. No-op
/// with no clients (the server renders only for someone watching).
fn push_frames(app: &mut App, clients: &mut [Client]) {
    if clients.is_empty() {
        return;
    }
    let (cols, rows) = app.size();
    let area = RRect::new(0, 0, cols.max(1), rows.max(1));
    let mut buf = Buffer::empty(area);
    let cursor = app.render_to(&mut buf).map(|p| (p.x, p.y));

    for c in clients.iter_mut() {
        if c.last.area != area {
            c.last = Buffer::empty(area);
            c.needs_full = true;
        }
        let changed = c.last.diff(&buf);
        // Send when cells changed, a baseline is due, OR the cursor moved.
        if changed.is_empty() && !c.needs_full && cursor == c.last_cursor {
            continue;
        }
        let cells: Vec<WireCell> = changed
            .iter()
            .map(|(x, y, cell)| WireCell {
                x: *x,
                y: *y,
                sym: cell.symbol().to_string(),
                fg: cell.fg,
                bg: cell.bg,
                mods: cell.modifier,
                skip: cell.skip,
            })
            .collect();
        let frame = FrameMsg {
            epoch: c.epoch,
            cols,
            rows,
            full: c.needs_full,
            cells,
            cursor,
        };
        match c.out.try_send(ServerMsg::Frame(frame)) {
            // Advance this client's baseline only once it actually has the frame.
            Ok(()) => {
                c.last = buf.clone();
                c.needs_full = false;
                c.last_cursor = cursor;
            }
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}
