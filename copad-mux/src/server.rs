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

/// ~30 Hz max frame cadence: the loop wakes at least this often to check for changes,
/// but only COMPOSES a frame when a dirty signal fired since the last render (PTY
/// `Wakeup`, input, chrome-data change, clock rollover) — an idle attached session
/// composes nothing. The per-client buffer diff then keeps the wire delta minimal.
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
    /// The next frame must be a `full` baseline repaint (set on attach + resize). MUST
    /// be paired with `last = empty` so the diff yields every cell.
    needs_full: bool,
    /// A frame was dropped under backpressure (bounded channel full) and NOT acked, so
    /// this client is behind `last` and needs a re-send. Distinct from `needs_full`: the
    /// resend is a normal delta vs the un-advanced `last` (not a full repaint), so it
    /// must NOT wipe the client's buffer. Cleared on the next successful send.
    pending: bool,
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
    // Load user config (~/.config/copad/mux.toml); surface any warnings to stderr
    // (a foreground `copad-mux server` shows them — an auto-spawned server's stderr is
    // /dev/null, so the client prints its own copy of the diagnostics too).
    let (cfg, warnings) = crate::config::MuxConfig::load();
    for w in &warnings {
        eprintln!("copad-mux config: {w}");
    }
    let mouse = cfg.mouse;
    // Session persistence (continuum-style): a background writer autosaves the layout so a
    // reboot/crash can restore it (App::new already restored on boot). Disabled when
    // `persist = false` or `autosave_secs = 0`.
    let persist_enabled = cfg.persist;
    let state_path = crate::persist::state_path();
    let saver = (cfg.persist && cfg.autosave_secs > 0)
        .then(|| crate::persist::Saver::new(state_path.clone()));
    let autosave = Duration::from_secs(cfg.autosave_secs.max(1) as u64);
    // Headless default size until the first client attaches (then reflowed).
    let mut app = App::new(80, 24, sock_env, cfg)?;

    let (tx, rx) = mpsc::channel::<Incoming>();
    spawn_accept_loop(listener, tx, mouse);

    // Multiple clients may attach at once (tmux-style shared view). The app is sized
    // to the SMALLEST attached client so everyone sees the whole thing; the same
    // composite is broadcast to all (each with its own diff baseline).
    let mut clients: Vec<Client> = Vec::new();
    let mut kill = false;
    let mut last_frame = Instant::now();
    let mut last_min = app.clock_minute();
    let mut last_save = Instant::now();
    // Idle-skip: only compose+diff a frame when something that affects it changed since
    // the last render (tmx-style). Start dirty so the first frame is always drawn.
    let mut dirty = true;

    loop {
        match rx.recv_timeout(FRAME_INTERVAL) {
            Ok(msg) => {
                dirty |= handle_incoming(msg, &mut app, &mut clients, &mut kill);
                // Bound the drain so a key/ctl flood can't starve rendering, PTY
                // reaping, or shutdown — fall back to the frame tick after a batch.
                let mut budget = 256u32;
                while budget > 0
                    && let Ok(m) = rx.try_recv()
                {
                    dirty |= handle_incoming(m, &mut app, &mut clients, &mut kill);
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
        let reaped = app.reap_exited();
        if app.is_empty() {
            break;
        }
        // Collect every dirty signal (err toward rendering; missing one shows stale).
        dirty |= reaped; // a reaped pane changed the layout
        dirty |= app.reconcile_popup();
        dirty |= app.reconcile_center();
        dirty |= app.maybe_refresh_labels(); // sidebar/status data actually changed
        dirty |= app.drain_pane_dirty(); // any pane's screen advanced (PTY output)
        let min = app.clock_minute();
        dirty |= min != last_min; // status-bar HH:MM rolled over
        // A frame dropped under backpressure (or a fresh attach) leaves the client
        // behind; reschedule a render to catch it up.
        dirty |= clients.iter().any(|c| c.needs_full || c.pending);
        if dirty && last_frame.elapsed() >= FRAME_INTERVAL {
            last_frame = Instant::now();
            last_min = min;
            push_frames(&mut app, &mut clients);
            dirty = false;
        }
        // Periodic autosave: hand a fresh snapshot to the off-loop writer. Reset the timer
        // from now (not fixed cadence) so a delayed loop can't trigger catch-up bursts.
        // Read-only, so it does NOT set `dirty` (never defeats the idle-skip).
        if let Some(saver) = &saver
            && last_save.elapsed() >= autosave
        {
            saver.request(app.snapshot());
            last_save = Instant::now();
        }
    }

    // Join the periodic writer (drains any queued autosave) BEFORE the final save, so the
    // synchronous save below is strictly last and its LATEST snapshot wins.
    drop(saver);
    // On an explicit `kill-server` (not a last-shell exit, which empties the app), write
    // the latest layout SYNCHRONOUSLY so an immediate reboot restores it even if no
    // periodic autosave fired yet. Synchronous (not the coalescing queue, which could drop
    // it behind an in-flight write) so the latest is guaranteed durable. Safe: no tombstone
    // → nothing races this delete-free write.
    if kill
        && persist_enabled
        && !app.is_empty()
        && let Err(e) = crate::persist::save_blocking(&state_path, &app.snapshot())
    {
        eprintln!("copad-mux persist: final save failed: {e}");
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
fn spawn_accept_loop(listener: UnixListener, tx: Sender<Incoming>, mouse: bool) {
    std::thread::spawn(move || {
        let next_id = AtomicU64::new(1);
        for stream in listener.incoming().flatten() {
            let id = next_id.fetch_add(1, Ordering::SeqCst);
            let tx = tx.clone();
            std::thread::spawn(move || handle_conn(stream, id, tx, mouse));
        }
    });
}

/// Read a connection's first line to select its role: `ctl` (one-shot request/reply)
/// or `attach` (streaming client). Rejects cross-uid peers.
fn handle_conn(stream: UnixStream, id: u64, tx: Sender<Incoming>, mouse: bool) {
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
        serve_client(id, cols, rows, reader, stream, tx, mouse);
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
    mut writer: UnixStream,
    tx: Sender<Incoming>,
    mouse: bool,
) {
    // A clone the main loop can shut down to force-detach this client reliably.
    let Ok(conn) = writer.try_clone() else { return };
    // Server-authoritative handshake FIRST (before any frame): tell the client whether
    // to enable mouse capture, so every client agrees with the server's config even if
    // its own local mux.toml differs (the server owns the effective setting).
    if writeln!(writer, "{}", json(&ServerMsg::Hello { mouse })).is_err() || writer.flush().is_err()
    {
        return;
    }
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

/// Read-only control requests never change what's rendered, so they must NOT trigger a
/// frame recompose (else a status-bar script polling `ctl list` would defeat idle-skip).
fn ctl_mutates(req: &control::Req) -> bool {
    !matches!(
        req,
        control::Req::List | control::Req::ListTabs | control::Req::ListSessions
    )
}

/// Apply one funneled message to the app / clients on the single-writer loop. Returns
/// whether it changed anything the render depends on (so the loop composes a frame).
fn handle_incoming(
    msg: Incoming,
    app: &mut App,
    clients: &mut Vec<Client>,
    kill: &mut bool,
) -> bool {
    match msg {
        Incoming::Ctl { req, reply } => {
            if matches!(req, control::Req::KillServer) {
                *kill = true;
                let _ = reply.send(control::Resp::ok());
                return false;
            }
            let mutates = ctl_mutates(&req);
            let resp = app.handle_control(&req);
            let _ = reply.send(resp);
            mutates
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
                pending: false,
                last_cursor: None,
                prefix: false,
                epoch: 0,
                cols,
                rows,
            });
            recompute_viewport(app, clients);
            true // a new client needs a (full) frame
        }
        Incoming::Client { id, msg } => {
            // Only accept from a currently-attached client (ignore stale ids).
            if !clients.iter().any(|c| c.id == id) {
                return false;
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
                    true // a key may change any visible state
                }
                ClientMsg::Mouse { x, y, kind } => {
                    // Scroll the pane under the cursor / click-to-focus — shared, so
                    // any client's wheel drives the one composite.
                    app.mouse_at(x, y, kind);
                    true
                }
                ClientMsg::Resize { cols, rows } => {
                    if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                        c.cols = cols;
                        c.rows = rows;
                    }
                    recompute_viewport(app, clients);
                    true
                }
                ClientMsg::Detach => {
                    if let Some(pos) = clients.iter().position(|c| c.id == id) {
                        detach_client(clients.remove(pos));
                        recompute_viewport(app, clients);
                    }
                    true
                }
                ClientMsg::Attach { .. } => false,
            }
        }
        Incoming::Disconnect { id } => {
            // The socket is already gone — drop the client WITHOUT another shutdown.
            if let Some(pos) = clients.iter().position(|c| c.id == id) {
                clients.remove(pos);
                recompute_viewport(app, clients);
            }
            true
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
                c.pending = false;
                c.last_cursor = cursor;
            }
            // Coalesced under backpressure: the client did NOT get this frame. Leave
            // `last` un-advanced (so the next diff is the delta that catches it up) and
            // just flag it pending so the main loop reschedules a render. Do NOT set
            // needs_full — that would send a `full` frame carrying only a delta and wipe
            // the client's buffer.
            Err(TrySendError::Full(_)) => {
                c.pending = true;
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ctl_mutates;
    use crate::control::Req;

    #[test]
    fn read_only_ctl_requests_do_not_force_a_render() {
        // Read-only queries must NOT dirty the frame (else `ctl list` polling defeats
        // idle-skip). Everything that changes state must.
        assert!(!ctl_mutates(&Req::List));
        assert!(!ctl_mutates(&Req::ListTabs));
        assert!(!ctl_mutates(&Req::ListSessions));
        assert!(ctl_mutates(&Req::NewTab));
        assert!(ctl_mutates(&Req::Split {
            dir: "right".into()
        }));
        assert!(ctl_mutates(&Req::Focus { index: 0 }));
        assert!(ctl_mutates(&Req::NewSession { name: None }));
    }
}
