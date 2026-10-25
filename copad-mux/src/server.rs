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

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect as RRect};

use crate::control::{self, runtime_dir, socket_path};
use crate::hostapi::{self, Hub};
use crate::model::ClientId;
use crate::proto::{ClientMsg, FrameMsg, Replaces, ServerMsg, WireCell};
use crate::tui::{App, KeyAction};
use copad_mux_proto::{self as l0, Feature, PubMsg, SubMsg};

/// ~30 Hz max frame cadence: the loop wakes at least this often to check for changes,
/// but only COMPOSES a frame when a dirty signal fired since the last render (PTY
/// `Wakeup`, input, chrome-data change, clock rollover) — an idle attached session
/// composes nothing. The per-client buffer diff then keeps the wire delta minimal.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// The fastest the attached terminals' bell is rung on a pane's behalf.
const BELL_INTERVAL: Duration = Duration::from_secs(1);

/// Loop cadence while **no client is attached**. With nobody watching there is no
/// frame to compose, so waking 30 times a second only pays for the loop body itself —
/// which a detached server does for as long as it lives (days, for a server whose
/// panes outlive the terminal that launched them). Slowing to this interval keeps the
/// server responsive to the things that still matter while detached — a client
/// attaching, a `ctl` request, a pane exiting, and the agent-status sweep that drives
/// turn notifications (itself already throttled to 500 ms) — because each of those
/// arrives as a channel message that wakes `recv_timeout` immediately rather than
/// waiting out the tick.
const IDLE_INTERVAL: Duration = Duration::from_millis(500);

/// How often the chrome/label sweep runs while a client is attached. It enumerates
/// every process on the machine, so it is the dominant cost of an otherwise idle
/// server — measured at ~1.5% of a core on its own.
const LABEL_INTERVAL: Duration = Duration::from_millis(500);

/// The same sweep while **detached**. Nothing renders the labels it produces; its one
/// remaining consumer is the agent-turn notifier, for which a couple of seconds of
/// extra latency on a desktop toast is imperceptible. This is what keeps a server
/// that sits detached for days from burning a CPU percent the whole time.
const IDLE_LABEL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a full **baseline resync** pass takes: the walk that re-establishes, band by
/// band, what the server believes each client holds. Far slower than the silent repaint
/// (`reconcile_secs`) because the two cost wildly different amounts. A repaint ships a flag
/// and the client re-emits cells it already has; a resync band ships every cell in those rows
/// as ndjson with full per-cell style — tens of kilobytes for a large screen. So the cheap
/// signal runs on the user-visible cadence and the expensive one trickles.
///
/// It exists because the two failure modes live on opposite sides of the client's buffer: a
/// repaint fixes a buffer-vs-screen divergence but would faithfully re-emit a buffer that is
/// itself wrong, which is what a resync fixes.
const RESYNC_PASS: Duration = Duration::from_secs(60);

/// Silence after which a client that opted into heartbeats is detached (decisions #167).
const HEARTBEAT_EXPIRY: Duration = Duration::from_secs(30);

/// Replies a client may have waiting before a further Command is refused UNEXECUTED.
const REPLY_QUEUE: usize = 8;

/// Bytes of replies a client may have waiting before the same refusal.
const REPLY_BYTES: usize = 1 << 20;

/// Queued replies beyond which the client is plainly not reading: it is disconnected rather
/// than left with requests that will never be answered (decisions #167).
const REPLY_HARD: usize = REPLY_QUEUE * 2;

/// Bands a [`RESYNC_PASS`] is split into, so no single frame carries a whole screen.
const RESYNC_BANDS: u16 = 8;

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
        /// The client process's pid, when it advertised one — the start of the walk to
        /// the terminal emulator a notification jump should raise.
        pid: Option<u32>,
        out: SyncSender<ServerMsg>,
        /// A clone of the connection, so the main loop can `shutdown` it to force a
        /// detach even when the bounded frame queue can't take a `Bye`.
        conn: UnixStream,
        heartbeat: bool,
        replaces: Option<Replaces>,
    },
    /// A forwarded message from an attached client.
    Client { id: u64, msg: ClientMsg },
    /// A client connection closed.
    Disconnect { id: u64 },
    /// An L0 host API subscriber connected (decisions #166).
    Subscribe {
        id: u64,
        outbox: std::sync::Arc<hostapi::Outbox>,
        conn: UnixStream,
    },
    /// A line from a subscriber.
    Sub { id: u64, msg: SubMsg },
    /// A subscriber's connection ended (EOF, write failure, or stall).
    SubGone { id: u64 },
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
    /// The cursor `(visible, shape)` that frame carried.
    last_look: (bool, u8),
    /// This client's OWN `Ctrl-b` prefix state — per-connection so a chord can't span
    /// clients when input is shared.
    prefix: crate::tui::Prefix,
    epoch: u64,
    cols: u16,
    rows: u16,
    /// When this client is next due a silent full repaint (`reconcile_secs`). Set forward
    /// on every repaint AND on every baseline reset, so a `full` frame — which already puts
    /// every cell on screen — doesn't get a redundant repaint right behind it.
    repaint_at: Instant,
    /// When this client's next baseline-resync band is due (see [`RESYNC_PASS`]).
    resync_at: Instant,
    /// Which band the resync walk is on; wraps at the bottom of the screen.
    resync_band: u16,
    /// This client process's pid, when advertised at attach — the walk-up start for
    /// raising its terminal window after a notification jump.
    pid: Option<u32>,
    /// This client's refreshed session vars (tmux `update-environment`), sent right after
    /// attach via [`ClientMsg::Env`]. Adopted as the pane-spawn env source when this client's
    /// input is dispatched, so a pane it creates inherits ITS live SSH/display session.
    env: Vec<(String, String)>,
    /// Text awaiting delivery to this client as a `Copy` (OSC 52) — from its own drag-selection
    /// or from a pane program's OSC 52 write. Held here and retried each loop because the frame
    /// channel is cap-1: a drag redraw may be queued when the copy is ready, so `try_send` can
    /// transiently fail.
    ///
    /// The `u64` is a clipboard sequence ([`term::next_clipboard_seq`]) and BOTH sources draw
    /// from it, so latest-wins is decided by when the copy was actually produced. Without it, a
    /// pane that writes the clipboard every tick would overwrite — and with backpressure, starve
    /// — an undelivered drag copy that happened later.
    pending_copy: Option<(u64, String)>,
    /// Whether this client's terminal has focus (assumed until it says otherwise).
    focused: bool,
    /// The title last sent to this client, and one waiting out channel backpressure.
    last_title: Option<String>,
    pending_title: Option<String>,
    /// A bell waiting to be rung on this client's terminal.
    pending_bell: bool,
    /// The binding tag it reported (`ClientMsg::Env::host_tag`): how a host UI finds this
    /// attachment in the L0 `clients` topic (decisions #166).
    tag: Option<String>,
    /// It pings; detach it after [`HEARTBEAT_EXPIRY`] of silence (decisions #167).
    heartbeat: bool,
    /// When it last sent anything.
    last_seen: Instant,
    /// Replies / pongs waiting for the cap-1 frame channel, oldest first, and their size.
    /// Drained before frames; bounded by admission in [`handle_command`].
    replies: std::collections::VecDeque<ServerMsg>,
    reply_bytes: usize,
    /// The reply queue hit [`REPLY_HARD`]: pruned on the next drain.
    overflowed: bool,
}

/// What a client's `attach` line said about itself: its id (server-minted), the terminal
/// size to open at, and its own pid (for window raising). Grouped so the per-connection
/// thread takes one parameter for the connection's identity rather than four.
struct AttachInfo {
    id: u64,
    cols: u16,
    rows: u16,
    pid: Option<u32>,
    /// Refuse before registering unless the server is this incarnation (decisions #166).
    expect_incarnation: Option<String>,
    heartbeat: bool,
    replaces: Option<Replaces>,
}

/// Per-connection handshake config threaded from [`run`] to each attaching client: the
/// server's authoritative mouse setting plus the `update_environment` variable names
/// advertised in `Hello` (tmux `update-environment`). Cheap to clone (one `bool` + an `Arc`).
#[derive(Clone)]
struct AttachCfg {
    mouse: bool,
    env_names: std::sync::Arc<Vec<String>>,
}

/// The uid on the other end of a Unix socket. `libc::getpeereid` is only bound for
/// BSD/macOS in the `libc` crate, so Linux uses the `SO_PEERCRED` socket option instead.
/// (`UnixStream::peer_cred` would be cross-platform but is still unstable.)
#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut cred).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

#[cfg(not(target_os = "linux"))]
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

/// Try to take the server's exclusive flock WITHOUT starting a server. `Some(guard)`
/// means no server is running (and none can start while the returned file is held);
/// `None` means a server currently owns it. Used by `comux worktree rm` to remove a
/// worktree locally, race-free, when there's no server — without leaving one behind.
pub fn try_acquire_lock() -> Option<File> {
    let (_sock, lock_path) = prepare_paths().ok()?;
    acquire_lock(&lock_path).ok()
}

/// Run the server to completion (exits when its last shell exits, on `kill-server`,
/// or if another server already owns the lock).
pub fn run() -> io::Result<()> {
    // Before anything opens a descriptor: lift the soft fd limit. A server inherits it
    // from whatever spawned it, and macOS still defaults to 256 — which at ~5
    // descriptors per pane wedges the server at ~48 panes, every later new-tab /
    // new-session / split failing to spawn its shell. Best-effort by design: a kernel
    // that refuses the raise still runs, just with fewer panes.
    match crate::fdlimit::raise() {
        Ok((before, after)) if after > before => {
            eprintln!("comux: raised fd limit {before} -> {after}");
        }
        Ok(_) => {}
        Err(e) => eprintln!("comux: could not raise fd limit: {e}"),
    }
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

    let sock_env = vec![
        (
            "COPAD_MUX_SOCK".to_string(),
            sock.to_string_lossy().to_string(),
        ),
        // A tmux-`$TMUX`-style marker: set ONLY in shells spawned as comux panes, so a
        // command run inside the mux (e.g. `comux worktree create`) can tell it is already
        // attached and must not spawn a nested client. Distinct from `COPAD_MUX_SOCK`, which
        // doubles as a user-facing socket override and so can't imply "inside comux".
        ("COPAD_MUX".to_string(), "1".to_string()),
    ];
    // Load user config (~/.config/copad/mux.toml); surface any warnings to stderr
    // (a foreground `copad-mux server` shows them — an auto-spawned server's stderr is
    // /dev/null, so the client prints its own copy of the diagnostics too).
    let (cfg, warnings) = crate::config::MuxConfig::load();
    for w in &warnings {
        eprintln!("comux config: {w}");
    }
    let mouse = cfg.mouse;
    // The variable names the server refreshes from each attaching client (tmux
    // `update-environment`), advertised in every `Hello`. Shared read-only across
    // connection threads.
    let env_names = std::sync::Arc::new(cfg.update_environment.clone());
    // Capture the daemon's OWN (birth) values for those names, then scrub them from the
    // daemon's environment. The snapshot seeds the first pane (so it's unchanged from
    // today); the scrub means every LATER pane starts without the session the server was
    // born in, receiving refreshed values only from the attaching client. This runs while
    // still single-threaded — before `App::new` spawns PTY event-loop threads and before
    // the accept/poll threads below — so mutating the process environment here is sound.
    let mut boot_env: Vec<(String, String)> = Vec::new();
    for name in &cfg.update_environment {
        // A non-UTF-8 value can't ride the String-typed pane env nor the client handshake,
        // so it could never be refreshed anyway: skip it here, which ALSO skips the scrub
        // below — leaving it in the daemon env (inherited as-is) so the boot pane keeps it.
        if let Some(val) = std::env::var_os(name)
            && let Ok(s) = val.into_string()
        {
            boot_env.push((name.clone(), s));
            // SAFETY: no other threads exist yet (see the ordering note above), so there is
            // no concurrent reader/writer of the environment.
            unsafe {
                std::env::remove_var(name);
            }
        }
    }
    // Second scrub, different contract: `never_inherit` names are agent SESSION markers
    // (`CLAUDE_CODE_CHILD_SESSION` & co). They are removed from the daemon env and NOT
    // recorded into `boot_env`, so unlike `update_environment` they don't seed the first /
    // restored pane either — a server born inside a Claude Code session would otherwise mark
    // every `claude` in every pane as a nested child, and Claude Code silently drops those
    // transcripts (no `--resume` entry). Nothing re-injects them: `config::from_raw` keeps
    // them out of `update_environment`, which is the only list a client can refresh.
    // This is the AUTHORITATIVE scrub — it covers every launch path (auto-spawned server and
    // a hand-run `comux server` alike) and runs before anything is spawned, so `spawn_server`
    // deliberately does not filter its own child env.
    for name in &cfg.never_inherit {
        if std::env::var_os(name).is_some() {
            // SAFETY: single-threaded at this point — same ordering guarantee as above.
            unsafe {
                std::env::remove_var(name);
            }
        }
    }
    // Session persistence (continuum-style): a background writer autosaves the layout so a
    // reboot/crash can restore it (App::new already restored on boot). Disabled when
    // `persist = false` or `autosave_secs = 0`.
    let persist_enabled = cfg.persist;
    let autosave_on = cfg.autosave_secs > 0;
    let state_path = crate::persist::state_path();
    // The writer exists whenever persistence is on (it also does the final save on
    // kill-server); periodic autosaves only fire when `autosave_secs > 0`.
    let saver = persist_enabled.then(|| crate::persist::Saver::new(state_path.clone()));
    let autosave = Duration::from_secs(cfg.autosave_secs.max(1) as u64);
    // Headless default size until the first client attaches (then reflowed).
    let mut app = App::new(80, 24, sock_env, boot_env, cfg)?;
    // Kick off the background usage/limits poller (Claude 5h+weekly · Codex weekly)
    // that feeds the status bar. Detached thread; `COPAD_MUX_USAGE=0` disables it.
    app.start_usage_poll();
    // And the background GitHub-release update checker (status-bar `⬆ x.y.z`
    // hint). Detached thread; `COPAD_MUX_UPDATE_CHECK=0` / `update_check = false`
    // disables it.
    app.start_agent_poll();
    app.start_git_poll();
    app.start_host_poll();
    app.start_version_poll();

    let (tx, rx) = mpsc::channel::<Incoming>();
    spawn_accept_loop(listener, tx, AttachCfg { mouse, env_names });

    // Multiple clients may attach at once (tmux-style shared view). The app is sized
    // to the SMALLEST attached client so everyone sees the whole thing; the same
    // composite is broadcast to all (each with its own diff baseline).
    let mut clients: Vec<Client> = Vec::new();
    // L0 host API subscribers + their chrome claims (decisions #166).
    let mut hub = Hub::default();
    let mut kill = false;
    let mut last_frame = Instant::now();
    let mut last_bell = Instant::now() - BELL_INTERVAL;
    let mut last_min = app.clock_minute();
    let mut last_save = Instant::now();
    // Idle-skip: only compose+diff a frame when something that affects it changed since
    // the last render (tmx-style). Start dirty so the first frame is always drawn.
    let mut dirty = true;
    // Whether a client was attached on the previous iteration, so the detached -> attached
    // edge can force a label sweep (see IDLE_LABEL_INTERVAL).
    let mut was_attached = false;

    loop {
        // Detached (no clients) → idle cadence; anything that needs prompt handling
        // still arrives as a message and interrupts the wait.
        let tick = if !clients.is_empty() {
            FRAME_INTERVAL
        } else if !hub.is_empty() {
            // A host is watching the lists: publish at its cadence, not the idle one.
            hostapi::PUBLISH_INTERVAL
        } else {
            IDLE_INTERVAL
        };
        match rx.recv_timeout(tick) {
            Ok(msg) => {
                dirty |= handle_incoming(msg, &mut app, &mut clients, &mut hub, &mut kill);
                // Bound the drain so a key/ctl flood can't starve rendering, PTY
                // reaping, or shutdown — fall back to the frame tick after a batch.
                let mut budget = 256u32;
                while budget > 0
                    && let Ok(m) = rx.try_recv()
                {
                    dirty |= handle_incoming(m, &mut app, &mut clients, &mut hub, &mut kill);
                    budget -= 1;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if kill {
            // A render-channel `kill-server` was answered into the reply queue: make sure the
            // writer threads have WRITTEN it before the sockets close (decisions #167).
            flush_before_shutdown(&mut app, &mut clients);
            break;
        }
        // Chrome a host draws: re-derive what the shared frame hides from who is attached
        // and who claims them, BEFORE any frame is composed this pass (decisions #166).
        dirty |= hub.expire();
        dirty |= sync_host_chrome(&mut app, &mut clients, &mut hub);
        app.forget_overlays(&hub.prune_overlays());
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
        dirty |= app.reconcile_copy(); // copy mode's pane went away or swapped screens
        // A client just attached after a detached stretch: the sweep has been running on
        // the wide idle interval, so force one now instead of letting the first frame
        // show tab labels and agent statuses that are seconds stale.
        let attached = !clients.is_empty();
        if attached && !was_attached {
            app.invalidate_labels();
        }
        was_attached = attached;
        // Sidebar/status data actually changed. Swept far less often while detached —
        // see IDLE_LABEL_INTERVAL.
        dirty |= app.maybe_refresh_labels(if clients.is_empty() && hub.is_empty() {
            IDLE_LABEL_INTERVAL
        } else {
            LABEL_INTERVAL
        });
        // Again after the sweep: it can re-sort the switcher's agents under the selection.
        dirty |= app.reconcile_popup();
        dirty |= app.maybe_auto_roll_usage(); // usage carousel auto-advance (usage_rotate_secs)
        dirty |= app.maybe_expire_message(); // a status-bar message timed out
        dirty |= app.poll_agent_sessions(); // resume-picker scan landed (Ctrl-b R)
        dirty |= app.poll_dir_listing(); // new-session browser listing landed (Ctrl-b C)
        resend_delegated(&mut app, &mut hub); // a host-drawn browser / resume picker's rows landed
        app.maybe_probe_session_dirs(); // re-check session start dirs off-loop (Ctrl-b c)
        let pane_dirty = app.drain_pane_dirty(); // a VISIBLE pane's screen advanced (PTY output)
        dirty |= pane_dirty;
        // A full-screen app (nvim/less/htop/…) just ENTERED or LEFT the alternate screen:
        // force a full repaint for every client so both the app's first screen and the
        // restored primary screen are clean. Either grid swap is where incremental-diff
        // residue shows (the client's unicode-width render desyncs from alacritty's grid on
        // wide graphemes during the app's life) and is also a large wholesale change an outer
        // terminal (Windows Terminal over SSH, a nested emulator) can drop cells on; a full
        // frame is the proven residue-clear (same path as Ctrl-b r). Gated on `pane_dirty` so
        // the per-pane alt-screen poll only runs when a pane actually advanced.
        if pane_dirty && app.take_alt_screen_transition() {
            for c in clients.iter_mut() {
                c.needs_full = true;
                c.last = Buffer::empty(c.last.area);
            }
            dirty = true;
        }
        let min = app.clock_minute();
        dirty |= min != last_min; // status-bar HH:MM rolled over
        // A frame dropped under backpressure (or a fresh attach) leaves the client
        // behind; reschedule a render to catch it up.
        dirty |= clients.iter().any(|c| c.needs_full || c.pending);
        // A self-heal signal is due (silent repaint / resync band). This is the one dirty
        // source that fires with nothing on screen having changed — deliberately, since
        // residue is only visible when nothing is changing.
        dirty |= reconcile_due(&app, &clients);
        // Deliver any pending drag-selection clipboard copies (OSC 52) BEFORE frames, so a
        // one-shot copy takes priority for the cap-1 channel slot and can't be perpetually
        // starved by sustained frame output. Non-blocking (a suspended client can't stall us):
        // on `Full` it retries next loop; a delivered copy just delays that client's frame by
        // one tick (which then re-sends as a normal delta).
        // A pane program's OSC 52 write (nvim yank, a `yank` helper) goes to EVERY attached
        // client, which re-emits it to its own terminal — tmux `set-clipboard` semantics.
        // Unlike a drag-copy there is no originating client to target: the write came from
        // inside the mux, so every viewer's clipboard is an equally valid destination.
        if let Some((seq, text)) = app.drain_pane_clipboard() {
            for c in clients.iter_mut() {
                queue_copy(c, seq, &text);
            }
        }
        dirty |= expire_silent_clients(&mut app, &mut clients);
        drain_pending_replies(&mut app, &mut clients);
        drain_pending_copies(&mut app, &mut clients);
        // tmux `bell-action any` + `set-titles`: ring every attached terminal when a pane
        // rings (at most once a second — a `yes $'\a'` must not machine-gun the dock), and keep
        // its title on the focused pane's.
        if app.take_new_bells() && last_bell.elapsed() >= BELL_INTERVAL {
            last_bell = Instant::now();
            for c in clients.iter_mut() {
                c.pending_bell = true;
            }
        }
        let title = app.outer_title();
        for c in clients.iter_mut() {
            if title.is_some() && title != c.last_title {
                c.last_title = title.clone();
                c.pending_title = title.clone();
            }
        }
        drain_pending_notices(&mut app, &mut clients);
        // Focus follows membership too: a client attaching (assumed focused), detaching or
        // being pruned changes "is any terminal showing this focused" as surely as a focus
        // event does. No clients = nobody looking = unfocused. Cheap when nothing changed.
        app.set_outer_focus(clients.iter().any(|c| c.focused));
        // L0 publication: changed topics into every subscriber's slots, plus events.
        if hub.due() {
            let (values, notifs) = topic_values(&app, &hub, &clients);
            let flashes = app.take_flashes();
            for over in hub.publish(values, flashes, &notifs) {
                app.forget_overlays(&hub.take_overlays_of(over));
                hub.remove(over);
            }
        } else if hub.is_empty() {
            // Nobody to tell: don't let a later subscriber receive stale outcome lines.
            app.take_flashes();
        }
        if dirty && last_frame.elapsed() >= FRAME_INTERVAL {
            last_frame = Instant::now();
            last_min = min;
            push_frames(&mut app, &mut clients);
            dirty = false;
        }
        // Periodic autosave: hand a fresh snapshot to the off-loop writer. Reset the timer
        // from now (not fixed cadence) so a delayed loop can't trigger catch-up bursts.
        // Read-only, so it does NOT set `dirty` (never defeats the idle-skip).
        if autosave_on
            && let Some(saver) = &saver
            && last_save.elapsed() >= autosave
        {
            saver.request(app.snapshot());
            last_save = Instant::now();
        }
    }

    // Persist on shutdown. On an explicit `kill-server` (not a last-shell exit, which empties
    // the app) the latest layout is pushed as the writer's LAST item and the writer is joined
    // — one writer, so renames are serialized and the newest snapshot wins (no race with a
    // separate save). Bounded (3s total) so a hung/networked filesystem can't strand the
    // process holding its flock; on that pathology we exit and rely on the last autosave.
    // Otherwise just stop the writer.
    match saver {
        Some(s) if kill && persist_enabled && !app.is_empty() => {
            s.finish(app.snapshot(), Duration::from_secs(3));
        }
        other => drop(other),
    }

    for c in clients.drain(..) {
        detach_client(c);
    }
    let _ = std::fs::remove_file(&sock);
    // Exit NOW instead of returning (which would drop `app` → every `PaneTerm::drop` joins
    // its PTY io-thread; a single wedged shell would then hang teardown forever, leaving a
    // live process + held flock + no socket — blocking restart). The save + socket removal
    // are done; the OS reaps the threads/PTYs and releases the flock on exit.
    std::process::exit(0);
}

/// The render attachments as the host API sees them.
fn attach_views(clients: &[Client]) -> Vec<hostapi::AttachView> {
    clients
        .iter()
        .map(|c| hostapi::AttachView {
            id: c.id,
            tag: c.tag.clone(),
            cols: c.cols,
            rows: c.rows,
        })
        .collect()
}

/// Every L0 topic, freshly computed and serialized (plus the notifications value itself,
/// which the publisher also reads for `notify` events).
fn topic_values(
    app: &App,
    hub: &Hub,
    clients: &[Client],
) -> (hostapi::TopicValues, l0::Notifications) {
    let notifs = app.host_notifications();
    let values = hostapi::TopicValues(vec![
        (l0::topic::TREE, hostapi::ser(&app.host_tree())),
        (l0::topic::AGENTS, hostapi::ser(&app.host_agents())),
        (l0::topic::NOTIFICATIONS, hostapi::ser(&notifs)),
        (l0::topic::STATUS, hostapi::ser(&app.host_status())),
        (
            l0::topic::CLIENTS,
            hostapi::ser(&hub.clients_topic(&attach_views(clients))),
        ),
    ]);
    (values, notifs)
}

/// Apply the shared-frame hiding rule. Drops claims on attachments that are gone, then
/// hides a feature only while every attached client is claimed for it. On a change every
/// client gets a full baseline — the content area moved, so geometry, hit zones and diff
/// baselines reset together. Returns whether anything changed.
fn sync_host_chrome(app: &mut App, clients: &mut [Client], hub: &mut Hub) -> bool {
    let views = attach_views(clients);
    hub.retain_attachments(&views);
    let hidden = hub.hidden(&views);
    let changed = app.set_host_hidden(
        hidden.contains(&Feature::Sidebar),
        hidden.contains(&Feature::Status),
    );
    if changed {
        for c in clients.iter_mut() {
            c.needs_full = true;
            c.last = Buffer::empty(c.last.area);
        }
    }
    changed
}

/// Queue a reply for the cap-1 channel; [`drain_pending_replies`] delivers it. A client
/// already [`REPLY_HARD`] behind is marked for disconnection instead — every reply it would
/// be owed is then unanswerable, and saying so by closing beats leaving requests hanging or
/// growing the queue (pings included) without bound.
fn queue_reply(c: &mut Client, msg: ServerMsg) {
    if c.replies.len() >= REPLY_HARD {
        c.overflowed = true;
        return;
    }
    c.reply_bytes += serde_json::to_string(&msg).map_or(0, |s| s.len());
    c.replies.push_back(msg);
}

/// A render-channel Command (decisions #167): run it in this connection's order, unless the
/// client is not draining its replies — then refuse it WITHOUT executing, so a client never
/// has to wonder whether a mutation it got no answer for happened.
fn handle_command(
    client: u64,
    cid: String,
    req: serde_json::Value,
    app: &mut App,
    clients: &mut [Client],
    kill: &mut bool,
) -> bool {
    let Some(c) = clients.iter_mut().find(|c| c.id == client) else {
        return false;
    };
    if c.replies.len() >= REPLY_QUEUE || c.reply_bytes >= REPLY_BYTES {
        // The refusal is small; past REPLY_HARD the client is disconnected (queue_reply).
        let resp = control::Resp::err_code("busy", "too many unanswered commands");
        queue_reply(
            c,
            ServerMsg::Reply {
                id: cid,
                resp: serde_json::to_value(resp).unwrap_or_default(),
            },
        );
        return false;
    }
    let (resp, mutates) = match serde_json::from_value::<control::Req>(req) {
        Err(e) => (control::Resp::err(format!("bad request: {e}")), false),
        Ok(control::Req::KillServer) => {
            *kill = true;
            (control::Resp::ok(), false)
        }
        Ok(req) => {
            let mutates = ctl_mutates(&req);
            (app.handle_control(&req), mutates)
        }
    };
    if let Some(c) = clients.iter_mut().find(|c| c.id == client) {
        queue_reply(
            c,
            ServerMsg::Reply {
                id: cid,
                resp: serde_json::to_value(resp).unwrap_or_default(),
            },
        );
    }
    mutates
}

/// Deliver queued replies ahead of frames, without blocking; a vanished client is pruned.
fn drain_pending_replies(app: &mut App, clients: &mut Vec<Client>) {
    let mut dead: Vec<u64> = clients
        .iter()
        .filter(|c| c.overflowed)
        .map(|c| c.id)
        .collect();
    for c in clients.iter_mut().filter(|c| !c.overflowed) {
        while let Some(msg) = c.replies.pop_front() {
            let size = serde_json::to_string(&msg).map_or(0, |s| s.len());
            match c.out.try_send(msg) {
                Ok(()) => c.reply_bytes = c.reply_bytes.saturating_sub(size),
                Err(TrySendError::Full(msg)) => {
                    c.replies.push_front(msg);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    dead.push(c.id);
                    break;
                }
            }
        }
    }
    prune_dead_clients(app, clients, &dead);
}

/// Before shutdown: deliver every queued reply, then prove each writer has written it. The
/// frame channel is cap-1 and the writer writes in order, so once a `Bye` is in the channel a
/// SECOND send can only succeed after the writer has taken that `Bye` — which it does only
/// after writing and flushing everything queued before it. Bounded to ~1s; a client that
/// never reads simply loses its reply, as it would have anyway.
fn flush_before_shutdown(app: &mut App, clients: &mut Vec<Client>) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline && clients.iter().any(|c| !c.replies.is_empty()) {
        drain_pending_replies(app, clients);
        std::thread::sleep(Duration::from_millis(5));
    }
    for c in clients.iter() {
        let mut sent_bye = false;
        while Instant::now() < deadline {
            match c.out.try_send(ServerMsg::Bye) {
                Ok(()) if sent_bye => break,
                Ok(()) => sent_bye = true,
                Err(TrySendError::Full(_)) => std::thread::sleep(Duration::from_millis(5)),
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
    }
}

/// Detach clients that promised heartbeats and went silent (a suspended phone). Returns
/// whether any went.
fn expire_silent_clients(app: &mut App, clients: &mut Vec<Client>) -> bool {
    let silent: Vec<u64> = clients
        .iter()
        .filter(|c| c.heartbeat && c.last_seen.elapsed() > HEARTBEAT_EXPIRY)
        .map(|c| c.id)
        .collect();
    prune_dead_clients(app, clients, &silent)
}

/// If `client`'s host draws overlays, hand it every overlay `client`'s input just opened
/// (decisions #170); they never reach the shared frame.
fn delegate_overlays(app: &mut App, hub: &mut Hub, client: u64, before: crate::tui::OverlayMarks) {
    let Some(sub) = hub.host_of(client, Feature::Overlays) else {
        return;
    };
    for (ov, spec) in app.delegate_new_overlays(before) {
        if !hub.send_overlay(sub, ov, client, spec) {
            app.forget_overlays(&hub.take_overlays_of(sub));
            hub.remove(sub);
            return;
        }
    }
}

/// Hand overlays an answer opened straight to subscriber `sub` (no marks diff: they never
/// were TUI state).
fn send_overlays(
    app: &mut App,
    hub: &mut Hub,
    sub: u64,
    client: u64,
    overlays: Vec<(u64, copad_mux_proto::OverlaySpec)>,
) {
    for (ov, spec) in overlays {
        if !hub.send_overlay(sub, ov, client, spec) {
            app.forget_overlays(&hub.take_overlays_of(sub));
            hub.remove(sub);
            return;
        }
    }
}

/// Re-send each host-drawn overlay whose off-loop data landed (decisions #172), under its
/// own id, to the host showing it.
fn resend_delegated(app: &mut App, hub: &mut Hub) {
    for (ov, spec) in app.poll_delegated() {
        if let Some((sub, client)) = hub.overlay_owner(ov)
            && !hub.send_overlay(sub, ov, client, spec)
        {
            app.forget_overlays(&hub.take_overlays_of(sub));
            hub.remove(sub);
        }
    }
}

/// A pane an answer spawns (a new session, a resumed conversation) gets the env of the client
/// it is for, exactly as one its key spawned would.
fn adopt_client_env(app: &mut App, clients: &[Client], client: u64) {
    if let Some(e) = clients
        .iter()
        .find(|c| c.id == client)
        .map(|c| c.env.clone())
    {
        app.set_client_env(e);
    }
}

/// One line from a subscriber. Returns whether it may have changed what is rendered.
fn handle_sub(
    id: u64,
    msg: SubMsg,
    app: &mut App,
    clients: &mut [Client],
    hub: &mut Hub,
    kill: &mut bool,
) -> bool {
    match msg {
        SubMsg::Cmd { id: cid, req } => {
            let (resp, mutates) = match serde_json::from_value::<control::Req>(req) {
                Err(e) => (control::Resp::err(format!("bad request: {e}")), false),
                Ok(control::Req::KillServer) => {
                    *kill = true;
                    (control::Resp::ok(), false)
                }
                Ok(req) => {
                    let mutates = ctl_mutates(&req);
                    (app.handle_control(&req), mutates)
                }
            };
            let reply = PubMsg::Reply {
                id: cid,
                resp: serde_json::to_value(resp).unwrap_or_default(),
            };
            if !hub.send(id, &reply) {
                hub.remove(id);
            }
            mutates
        }
        SubMsg::HostReady {
            attachment,
            incarnation,
            features,
        } => {
            if let Err((code, message)) = hub.claim(
                id,
                attachment,
                &incarnation,
                features,
                &attach_views(clients),
            ) {
                let err = PubMsg::Error {
                    code: code.into(),
                    message,
                };
                if !hub.send(id, &err) {
                    hub.remove(id);
                }
            }
            false // sync_host_chrome on the next pass reports the real change
        }
        SubMsg::HostGone { attachment } => {
            hub.release(id, attachment);
            false
        }
        SubMsg::Ping { id: n } => {
            if !hub.send(id, &PubMsg::Pong { id: n }) {
                hub.remove(id);
            }
            false
        }
        SubMsg::OverlayResult {
            overlay,
            choice,
            text,
        } => {
            // Only the host the overlay was handed to may answer it.
            match hub.take_overlay(id, overlay, &attach_views(clients)) {
                Some((true, client)) => {
                    adopt_client_env(app, clients, client);
                    let marks = app.overlay_marks();
                    let (changed, next) = app.overlay_result(overlay, choice, text);
                    // What the answer opened goes back to the same host: a browser's next
                    // directory, or a menu item's rename prompt (decisions #172).
                    send_overlays(app, hub, id, client, next);
                    delegate_overlays(app, hub, client, marks);
                    changed
                }
                // Ours, but ownership ended (detached / released): the delegated state goes.
                Some((false, _)) => {
                    app.forget_overlays(&[overlay]);
                    false
                }
                // Not this subscriber's overlay: someone else's stays untouched.
                None => false,
            }
        }
        SubMsg::Open {
            attachment,
            what,
            target,
        } => {
            // Only the host that draws this attachment's overlays may open them for it.
            let attached = clients.iter().any(|c| c.id == attachment);
            let res = if attached && hub.host_of(attachment, Feature::Overlays) == Some(id) {
                adopt_client_env(app, clients, attachment);
                let marks = app.overlay_marks();
                let res = app.open_for_host(&what, target.as_deref());
                delegate_overlays(app, hub, attachment, marks);
                res
            } else {
                Err(format!(
                    "attachment {attachment} is not claimed for overlays by this host"
                ))
            };
            // Said like any refused action: a flash (an `error` line would read as a refused
            // claim to the host).
            if let Err(message) = res {
                app.flash(message, true);
            }
            true
        }
        SubMsg::Subscribe { .. } => false,
    }
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
fn spawn_accept_loop(listener: UnixListener, tx: Sender<Incoming>, cfg: AttachCfg) {
    std::thread::spawn(move || {
        let next_id = AtomicU64::new(1);
        for stream in listener.incoming().flatten() {
            let id = next_id.fetch_add(1, Ordering::SeqCst);
            let tx = tx.clone();
            let cfg = cfg.clone();
            std::thread::spawn(move || handle_conn(stream, id, tx, cfg));
        }
    });
}

/// Read a connection's first line to select its role: `ctl` (one-shot request/reply)
/// or `attach` (streaming client). Rejects cross-uid peers.
fn handle_conn(stream: UnixStream, id: u64, tx: Sender<Incoming>, cfg: AttachCfg) {
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
    } else if let Ok(ClientMsg::Attach {
        cols,
        rows,
        pid,
        expect_incarnation,
        heartbeat,
        replaces,
    }) = serde_json::from_str::<ClientMsg>(&first)
    {
        serve_client(
            AttachInfo {
                id,
                cols,
                rows,
                pid,
                expect_incarnation,
                heartbeat,
                replaces,
            },
            reader,
            stream,
            tx,
            cfg,
        );
    } else if let Ok(SubMsg::Subscribe { .. }) = serde_json::from_str::<SubMsg>(&first) {
        serve_sub(id, reader, stream, tx);
    } else {
        let mut w = stream;
        let _ = writeln!(
            w,
            "{}",
            json(control::Resp::err("bad hello (expected a cmd or attach)"))
        );
    }
}

/// An L0 host API subscriber (decisions #166): register an outbox with the main loop, run
/// its writer on its own thread, and forward every line it sends. The main loop queues the
/// welcome + snapshot before the subscriber can receive anything else.
fn serve_sub(id: u64, mut reader: BufReader<UnixStream>, stream: UnixStream, tx: Sender<Incoming>) {
    let Ok(conn) = stream.try_clone() else { return };
    let outbox = hostapi::Outbox::new();
    if tx
        .send(Incoming::Subscribe {
            id,
            outbox: outbox.clone(),
            conn,
        })
        .is_err()
    {
        return;
    }
    let gone_tx = tx.clone();
    hostapi::spawn_writer(outbox.clone(), stream, move || {
        let _ = gone_tx.send(Incoming::SubGone { id });
    });
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // A runaway line from a confused peer must not grow without bound.
        if line.len() > MAX_SUB_LINE {
            break;
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // An unknown message (a newer host) is skipped, not fatal.
        if let Ok(msg) = serde_json::from_str::<SubMsg>(t)
            && tx.send(Incoming::Sub { id, msg }).is_err()
        {
            break;
        }
    }
    outbox.kill();
    let _ = tx.send(Incoming::SubGone { id });
}

/// The longest line a subscriber may send (a `cmd` carrying a `send --paste` is the big one).
const MAX_SUB_LINE: usize = 4 << 20;

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
    info: AttachInfo,
    mut reader: BufReader<UnixStream>,
    mut writer: UnixStream,
    tx: Sender<Incoming>,
    cfg: AttachCfg,
) {
    let AttachInfo {
        id,
        cols,
        rows,
        pid,
        expect_incarnation,
        heartbeat,
        replaces,
    } = info;
    // A host that subscribed first names the server it expects: a restart in between must
    // fail HERE, before the attachment registers and resizes the shared view.
    if let Some(want) = expect_incarnation
        && want != crate::term::incarnation()
    {
        let refused = ServerMsg::Refused {
            code: "stale".into(),
            message: format!(
                "expected server {want}, this is {}; resubscribe",
                crate::term::incarnation()
            ),
        };
        let _ = writeln!(writer, "{}", json(&refused));
        return;
    }
    // A clone the main loop can shut down to force-detach this client reliably.
    let Ok(conn) = writer.try_clone() else { return };
    // Server-authoritative handshake FIRST (before any frame): tell the client whether
    // to enable mouse capture (the server owns the effective setting) and which env vars
    // to send back (tmux `update-environment`).
    let hello = ServerMsg::Hello {
        mouse: cfg.mouse,
        update_environment: (*cfg.env_names).clone(),
        paste: true,
        focus: true,
        colors: true,
        incarnation: crate::term::incarnation().to_string(),
        attachment: id,
        commands: true,
    };
    if writeln!(writer, "{}", json(&hello)).is_err() || writer.flush().is_err() {
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
            pid,
            out: out_tx,
            conn,
            heartbeat,
            replaces,
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

/// Drop a departing client's pid from the window-raise candidates.
///
/// Called on EVERY removal path (key detach, explicit detach, disconnect, dead-client
/// prune) — not just disconnect. A client removed by one of the other paths never reaches
/// the `Disconnect` arm's bookkeeping (its late `Disconnect` finds no client and no-ops),
/// so its exited pid would otherwise stay the newest candidate forever and shadow the
/// client that is still attached.
fn forget_client(app: &mut App, c: &Client) {
    if let Some(p) = c.pid {
        app.forget_client_pid(p);
    }
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
        control::Req::List { .. }
            | control::Req::ListTabs { .. }
            | control::Req::ListSessions
            | control::Req::Health
            | control::Req::Host
            // Read-only, and the one verb callers POLL. Treating it as a mutation would
            // mark the render dirty on every poll and defeat the loop's idle skip — the
            // detached-CPU throttle exists precisely to stop that kind of busy-work.
            | control::Req::CapturePane { .. }
            // Also read-only and also polled (by `wait-agent`); it reads CACHED labels and
            // statuses, so it neither sweeps processes nor touches a PTY.
            | control::Req::ListAgents { .. }
            | control::Req::ListTree { .. }
            // Read-only, and pollable (`marks --json`).
            | control::Req::Marks
            // Inbox traffic is polled by `inbox --wait` and draws nothing.
            | control::Req::Inbox { .. }
            | control::Req::Deliver { .. }
    )
}

/// Apply one funneled message to the app / clients on the single-writer loop. Returns
/// whether it changed anything the render depends on (so the loop composes a frame).
fn handle_incoming(
    msg: Incoming,
    app: &mut App,
    clients: &mut Vec<Client>,
    hub: &mut Hub,
    kill: &mut bool,
) -> bool {
    match msg {
        Incoming::Subscribe { id, outbox, conn } => {
            // Someone is about to look at the agents list: don't hand them a sweep that is
            // seconds old because the server was idle.
            app.invalidate_labels();
            let (values, _) = topic_values(app, hub, clients);
            hub.add(
                hostapi::Sub {
                    id,
                    outbox,
                    conn,
                    last_seen: Instant::now(),
                },
                values,
                env!("CARGO_PKG_VERSION"),
            );
            false
        }
        Incoming::SubGone { id } => {
            app.forget_overlays(&hub.take_overlays_of(id));
            hub.remove(id);
            false
        }
        Incoming::Sub { id, msg } => {
            hub.touch(id);
            handle_sub(id, msg, app, clients, hub, kill)
        }
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
            pid,
            out,
            conn,
            heartbeat,
            replaces,
        } => {
            // A reconnecting client replaces its previous attachment — through the full
            // prune path (pid bookkeeping, owned overlays, viewport, prefix repaint), and only
            // when the tag matches, so a stale id cannot evict somebody else (decisions #167).
            if let Some(Replaces { attachment, tag }) = replaces
                && clients
                    .iter()
                    .any(|c| c.id == attachment && c.tag.as_deref() == Some(tag.as_str()))
            {
                prune_dead_clients(app, clients, &[attachment]);
            }
            // Shared attach: ADD the client (no takeover), then re-fit to the smallest.
            clients.push(Client {
                id,
                out,
                conn,
                last: Buffer::empty(RRect::new(0, 0, cols.max(1), rows.max(1))),
                needs_full: true,
                pending: false,
                last_cursor: None,
                last_look: (true, 0),
                prefix: crate::tui::Prefix::default(),
                epoch: 0,
                // The attach frame is `full` — every cell, applied over a cleared screen — so
                // both self-heal walks start one interval OUT rather than firing immediately
                // behind it. (When the self-heal is off these are never read.)
                repaint_at: Instant::now() + app.repaint_period().unwrap_or(RESYNC_PASS),
                resync_at: Instant::now() + RESYNC_PASS / u32::from(RESYNC_BANDS),
                resync_band: 0,
                cols,
                rows,
                pid,
                env: Vec::new(),
                pending_copy: None,
                focused: true,
                last_title: None,
                pending_title: None,
                pending_bell: false,
                tag: None,
                heartbeat,
                last_seen: Instant::now(),
                replies: std::collections::VecDeque::new(),
                reply_bytes: 0,
                overflowed: false,
            });
            if let Some(p) = pid {
                app.set_client_pid(p);
            }
            recompute_viewport(app, clients);
            true // a new client needs a (full) frame
        }
        Incoming::Client { id, msg } => {
            // Only accept from a currently-attached client (ignore stale ids).
            let Some(c) = clients.iter_mut().find(|c| c.id == id) else {
                return false;
            };
            c.last_seen = Instant::now();
            // A key or a mouse event is a human acting on the CURRENT view, which is the only
            // honest evidence that the focused pane has been seen. Bells are acknowledged
            // here rather than anywhere in the frame path: composing, queueing, or even
            // flushing a frame all say the server emitted something, not that anyone looked.
            if matches!(
                msg,
                ClientMsg::Key(_) | ClientMsg::Paste { .. } | ClientMsg::Mouse { .. }
            ) {
                app.ack_focused_bell();
            }
            match msg {
                ClientMsg::Focus { gained } => {
                    if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                        c.focused = gained;
                    }
                    app.set_outer_focus(clients.iter().any(|c| c.focused));
                    false
                }
                ClientMsg::Colors { fg, bg } => {
                    crate::term::set_outer_colors(fg, bg);
                    false
                }
                ClientMsg::Env {
                    vars,
                    copad_host,
                    host_tag,
                } => {
                    if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                        c.tag = host_tag.filter(|t| !t.is_empty());
                    }
                    // tmux update-environment: store this client's session vars and adopt
                    // them now, so the next pane it spawns inherits its live SSH/display env.
                    let pid = clients.iter().find(|c| c.id == id).and_then(|c| c.pid);
                    if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                        c.env = vars.clone();
                    }
                    app.set_client_env(vars);
                    // Keyed by the CLIENT's pid, the same key `raise_target` picks a terminal
                    // by, so the two can never disagree about which client is being raised.
                    if let (Some(pid), Some((sock, panel))) = (pid, copad_host) {
                        app.set_client_copad(pid, sock, panel);
                    }
                    false
                }
                ClientMsg::Key(k) => {
                    // Any key input ends an in-progress drag-selection (the user moved on).
                    app.clear_selection();
                    // A pane spawned by THIS client's key must inherit THIS client's env
                    // (not whichever client attached last) — adopt it before dispatch (C4).
                    if let Some(e) = clients.iter().find(|c| c.id == id).map(|c| c.env.clone()) {
                        app.set_client_env(e);
                    }
                    // All attached clients share input (tmux-style), but each carries
                    // its OWN prefix state so a `Ctrl-b` from one client can't be
                    // completed by another's key. Detach removes ONLY the client that
                    // pressed the chord; the others keep going.
                    let mut action = KeyAction::Continue;
                    let marks = app.overlay_marks();
                    if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                        action = app.feed_key(k, &mut c.prefix);
                        // A copy-mode yank goes to the client whose key it was, exactly like
                        // the text a drag-release returns.
                        if let Some(text) = app.take_key_copy() {
                            queue_copy(c, crate::term::next_clipboard_seq(), &text);
                        }
                    }
                    delegate_overlays(app, hub, id, marks);
                    match action {
                        KeyAction::Detach => {
                            if let Some(pos) = clients.iter().position(|c| c.id == id) {
                                let gone = clients.remove(pos);
                                forget_client(app, &gone);
                                detach_client(gone);
                                // Reap the departing client's owned overlays (a menu it
                                // opened must not linger capturing others' input).
                                app.clear_selection_of(ClientId(id));
                                app.close_menu_of(ClientId(id));
                                recompute_viewport(app, clients);
                            }
                        }
                        // Force a full repaint to the client that asked: clear its diff
                        // baseline so the next frame carries EVERY cell and is flagged
                        // `full` (the client then clears its terminal). Fixes drift/ghosts
                        // from a resize, alt-screen transition, or a nested emulator.
                        KeyAction::Redraw => {
                            if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                                c.needs_full = true;
                                c.last = Buffer::empty(c.last.area);
                            }
                        }
                        KeyAction::Continue => {}
                    }
                    true // a key may change any visible state
                }
                ClientMsg::Paste { text } => {
                    // Typed input in every respect but its framing: same selection clear,
                    // same env adoption (a paste into an open prompt can commit it and
                    // spawn a pane), same per-client prefix.
                    app.clear_selection();
                    if let Some(e) = clients.iter().find(|c| c.id == id).map(|c| c.env.clone()) {
                        app.set_client_env(e);
                    }
                    let mut action = KeyAction::Continue;
                    let marks = app.overlay_marks();
                    if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                        action = app.feed_paste(&text, &mut c.prefix);
                        // Replayed keys can yank in copy mode; the copy is this client's.
                        if let Some(text) = app.take_key_copy() {
                            queue_copy(c, crate::term::next_clipboard_seq(), &text);
                        }
                    }
                    delegate_overlays(app, hub, id, marks);
                    if action == KeyAction::Detach
                        && let Some(pos) = clients.iter().position(|c| c.id == id)
                    {
                        let gone = clients.remove(pos);
                        forget_client(app, &gone);
                        detach_client(gone);
                        app.clear_selection_of(ClientId(id));
                        app.close_menu_of(ClientId(id));
                        recompute_viewport(app, clients);
                    }
                    true
                }
                ClientMsg::Mouse { x, y, kind } => {
                    // Scroll the pane under the cursor / click-to-focus / drag-select — shared,
                    // so any client's wheel drives the one composite. A drag-release returns the
                    // selected pane text, which we hand back to THIS client to copy (OSC 52).
                    let marks = app.overlay_marks();
                    if let Some(text) = app.mouse_at(ClientId(id), x, y, kind)
                        && let Some(c) = clients.iter_mut().find(|c| c.id == id)
                    {
                        queue_copy(c, crate::term::next_clipboard_seq(), &text);
                    }
                    // A context-menu item can open a prompt (rename tab/session).
                    delegate_overlays(app, hub, id, marks);
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
                        let gone = clients.remove(pos);
                        forget_client(app, &gone);
                        detach_client(gone);
                        // Same overlay reaping as the key-detach / disconnect paths.
                        app.clear_selection_of(ClientId(id));
                        app.close_menu_of(ClientId(id));
                        recompute_viewport(app, clients);
                    }
                    true
                }
                ClientMsg::Command { id: cid, req } => {
                    handle_command(id, cid, req, app, clients, kill)
                }
                ClientMsg::Ping { id: n } => {
                    if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                        queue_reply(c, ServerMsg::Pong { id: n });
                    }
                    false
                }
                ClientMsg::Attach { .. } => false,
            }
        }
        Incoming::Disconnect { id } => {
            // The socket is already gone — drop the client WITHOUT another shutdown.
            if let Some(pos) = clients.iter().position(|c| c.id == id) {
                let gone = clients.remove(pos);
                forget_client(app, &gone);
                // Drop a drag-selection / context menu the departing client owned (no
                // live owner to finish them).
                app.clear_selection_of(ClientId(id));
                app.close_menu_of(ClientId(id));
                recompute_viewport(app, clients);
            }
            true
        }
    }
}

/// Remove clients whose writer thread has gone away (their `out` channel reported
/// `Disconnected`), clean up any overlays they still owned, and re-fit the viewport.
///
/// This is the recovery for a HALF-DEAD connection: when a client's socket dies such
/// that writes fail but the reader thread stays blocked in `read_line` (no EOF), the
/// `Disconnect` message from `serve_client` never arrives. Without this, the dead client
/// lingers in `clients` and its (often smaller) size permanently pins `recompute_viewport`'s
/// `min()`, so the shared view can never grow past the stale client — the very bug this fixes.
/// A `Disconnected` sender is an unambiguous signal: the writer thread only ends on a write
/// failure or after sending `Bye` (a clean detach, which already removed the client), so
/// pruning here is always correct.
fn prune_dead_clients(app: &mut App, clients: &mut Vec<Client>, dead: &[u64]) -> bool {
    if dead.is_empty() {
        return false;
    }
    // Tear each dead client DOWN via `detach_client` (socket `shutdown`), not a bare drop:
    // the matching reader thread in `serve_client` holds its OWN dup of the socket, so
    // dropping only the server's `conn` clone leaves it blocked forever in `read_line` on
    // the half-dead fd (a thread leak). `shutdown(Both)` unblocks it so it exits — its late
    // `Disconnect` then finds no client and is a harmless no-op. The `Bye` try_send inside
    // `detach_client` also just fails (the channel is already `Disconnected`), which is fine.
    let mut i = 0;
    while i < clients.len() {
        if dead.contains(&clients[i].id) {
            let c = clients.remove(i);
            forget_client(app, &c);
            // Drop any drag-selection / context menu the departing client owned.
            app.clear_selection_of(ClientId(c.id));
            app.close_menu_of(ClientId(c.id));
            detach_client(c);
        } else {
            i += 1;
        }
    }
    recompute_viewport(app, clients);
    // A prune can turn a frame that was ALREADY SENT into a lie. The status bar's `^b` is
    // composed from "some attached client is mid-chord", so pruning the client that held the
    // prefix leaves every survivor painted with an indicator belonging to nobody — and both
    // prune sites run at points where `dirty` has already been decided, so nothing would
    // repaint it. `App::prefix_armed` is what the last composed frame said; when it no longer
    // matches the pruned list, flag the survivors so the loop composes one more frame
    // (`dirty |= clients.iter().any(|c| c.needs_full || c.pending)`). Terminating: each prune
    // strictly shrinks `clients`. Centralized here rather than at the call sites so a future
    // prune path cannot reintroduce the stuck indicator.
    if app.prefix_armed() != clients.iter().any(|c| c.prefix.is_armed()) {
        for c in clients.iter_mut() {
            c.pending = true;
        }
    }
    true
}

/// Try to deliver each client's pending drag-selection copy (OSC 52) without blocking. The
/// frame channel is cap-1 and shared with frames, so a `Copy` can transiently fail to enqueue;
/// leave it pending (retry next tick) on `Full`, prune the client on `Disconnected`.
fn drain_pending_copies(app: &mut App, clients: &mut Vec<Client>) {
    let mut dead = Vec::new();
    for c in clients.iter_mut() {
        let Some((seq, text)) = c.pending_copy.take() else {
            continue;
        };
        match c.out.try_send(ServerMsg::Copy { text: text.clone() }) {
            Ok(()) => {}
            // Retry next loop — but through `queue_copy`, so a copy produced while this one
            // was blocked isn't demoted back to the older text.
            Err(TrySendError::Full(_)) => queue_copy(c, seq, &text),
            Err(TrySendError::Disconnected(_)) => dead.push(c.id), // writer gone; prune
        }
    }
    prune_dead_clients(app, clients, &dead);
}

/// Deliver pending bells and titles like copies: non-blocking, retried while the cap-1
/// channel is full, a vanished client pruned.
fn drain_pending_notices(app: &mut App, clients: &mut Vec<Client>) {
    let mut dead = Vec::new();
    for c in clients.iter_mut() {
        if c.pending_bell {
            match c.out.try_send(ServerMsg::Bell) {
                Ok(()) => c.pending_bell = false,
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => dead.push(c.id),
            }
        }
        if let Some(text) = c.pending_title.take() {
            match c.out.try_send(ServerMsg::Title { text: text.clone() }) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => c.pending_title = Some(text),
                Err(TrySendError::Disconnected(_)) => dead.push(c.id),
            }
        }
    }
    prune_dead_clients(app, clients, &dead);
}

/// Queue `text` for delivery to `c` as an OSC 52 copy, keeping only the LATEST by clipboard
/// sequence. The clipboard has one slot, so an older copy must never displace a newer one —
/// which is exactly what an unguarded assignment does when a pane writes the clipboard on
/// the same tick a drag copy is still waiting out channel backpressure.
fn queue_copy(c: &mut Client, seq: u64, text: &str) {
    if c.pending_copy.as_ref().is_none_or(|(prev, _)| seq > *prev) {
        c.pending_copy = Some((seq, text.to_string()));
    }
}

/// A cell value no compose path can produce, used to mark part of a client's baseline
/// "unknown" so the next diff re-sends every cell there — **including the blanks**.
///
/// Wiping the band to `Buffer::empty` instead would defeat the whole point: an empty cell
/// equals the blank a compose produces, so the diff would skip exactly the position whose
/// stale glyph we are trying to clear — and "a cell that should be blank keeps showing its
/// old glyph" is the reported symptom. A NUL symbol can't collide with real content:
/// `snapshot_grid` coerces pane graphemes and the chrome writes printable text.
fn unknown_cell() -> Cell {
    let mut c = Cell::EMPTY;
    c.set_symbol("\u{0}");
    c
}

/// The rows covered by resync band `i` of a `rows`-tall screen, or `None` once `i` is past
/// the bottom (the caller wraps to band 0). Bands are ceil-sized so the last one is the short
/// remainder rather than the walk overrunning the screen.
fn resync_band_rows(rows: u16, i: u16) -> Option<(u16, u16)> {
    if rows == 0 {
        return None;
    }
    let h = rows.div_ceil(RESYNC_BANDS).max(1);
    let y0 = i.checked_mul(h)?;
    if y0 >= rows {
        return None;
    }
    Some((y0, h.min(rows - y0)))
}

/// Mark `h` rows of `last` unknown from `y0` down (see [`unknown_cell`]).
fn invalidate_band(last: &mut Buffer, y0: u16, h: u16) {
    for y in y0..y0.saturating_add(h) {
        for x in 0..last.area.width {
            if let Some(cell) = last.cell_mut(Position::new(x, y)) {
                *cell = unknown_cell();
            }
        }
    }
}

/// Whether any attached client is due a self-heal signal — a silent repaint or the next
/// baseline-resync band — so the main loop composes a frame even with nothing else dirty.
/// Always false when the self-heal is off, so it can never keep a detached-but-idle server
/// (or one with `reconcile_secs = 0`) spinning.
fn reconcile_due(app: &App, clients: &[Client]) -> bool {
    if app.repaint_period().is_none() {
        return false;
    }
    let now = Instant::now();
    clients
        .iter()
        .any(|c| now >= c.repaint_at || now >= c.resync_at)
}

/// Render the app ONCE and broadcast the changed cells (or a full baseline) to every
/// attached client — each diffed against its OWN last-sent buffer, so a freshly
/// attached client gets a full frame while up-to-date ones get small deltas. No-op
/// with no clients (the server renders only for someone watching).
fn push_frames(app: &mut App, clients: &mut Vec<Client>) {
    if clients.is_empty() {
        return;
    }
    // The prefix is per-client state, but the composed frame is shared, so the status
    // bar's `^b` indicator shows whether ANY client is mid-chord. Recomputed here rather
    // than pushed from the key path because a client can also leave the prefix armed and
    // then detach — reading the live client list each frame can't go stale.
    let armed = clients.iter().any(|c| c.prefix.is_armed());
    app.set_prefix_armed(armed);

    // Self-heal cadence, read live so `comux reload` changes it on the running server.
    let period = app.repaint_period();
    let now = Instant::now();

    let (cols, rows) = app.size();
    let area = RRect::new(0, 0, cols.max(1), rows.max(1));
    let mut buf = Buffer::empty(area);
    let cursor = app.render_to(&mut buf).map(|p| (p.x, p.y));
    let look = app.cursor_look();

    let mut dead = Vec::new();
    for c in clients.iter_mut() {
        if c.last.area != area {
            c.last = Buffer::empty(area);
            c.needs_full = true;
        }
        // `comux reload` can shorten `reconcile_secs` under a client already waiting on a
        // deadline computed from the OLD value, so cap it: going from an hour to 3s must not
        // leave every attached client waiting out the hour. Growing the period just lets one
        // already-due repaint fire early, after which the new cadence applies.
        if let Some(p) = period {
            let cap = now + p;
            if c.repaint_at > cap {
                c.repaint_at = cap;
            }
        }
        // Baseline resync: forget what this client is believed to hold for one band of rows,
        // so the diff below re-ships every cell in them. Skipped when a `full` frame is
        // already pending — that re-ships the entire screen anyway.
        if period.is_some() && !c.needs_full && !c.pending && now >= c.resync_at {
            let (band, next) = match resync_band_rows(rows, c.resync_band) {
                Some(b) => (b, c.resync_band.saturating_add(1)),
                // Past the bottom: wrap and take band 0 on this same tick, so the walk never
                // spends an interval doing nothing.
                None => (resync_band_rows(rows, 0).unwrap_or((0, 0)), 1),
            };
            invalidate_band(&mut c.last, band.0, band.1);
            c.resync_band = next;
            c.resync_at = now + RESYNC_PASS / u32::from(RESYNC_BANDS);
        }
        let changed = c.last.diff(&buf);
        // Silent full repaint due? Three gates, each removing a case where it is pointless or
        // harmful:
        //   * behind a `full` frame, which already repaints everything;
        //   * to a client that did not get the LAST frame (`pending`) — it is already behind, and
        //     a self-heal is a luxury, never something to pile onto a struggling client;
        //   * on a frame that carries changed cells at all. A repaint exists for residue you are
        //     LOOKING at, and while content is flowing the delta is already overwriting cells. It
        //     also keeps a screenful of escapes out of a busy stream: the client writes to its
        //     terminal synchronously, so padding an active stream is how one on a slow link ends
        //     up blocked in `write` and late to forward the user's input.
        // An idle attached screen has an empty delta most ticks, so this still heals within a
        // second of anything pausing.
        let repaint = period.is_some()
            && !c.needs_full
            && !c.pending
            && changed.is_empty()
            && now >= c.repaint_at;
        // Send when cells changed, a baseline is due, a repaint is due, OR the cursor moved.
        // A repaint alone is a ~60-byte line carrying no cells, which is the point: it asks
        // the client to re-emit what it already holds.
        if changed.is_empty()
            && !c.needs_full
            && !repaint
            && cursor == c.last_cursor
            && look == c.last_look
        {
            // Nothing to say — and that also settles a client marked `pending`. `pending` means
            // it did not receive the LAST frame, but `c.last` was never advanced for that frame,
            // so an empty diff against it proves the client's content is already current: the
            // dropped frame was superseded. Leaving the flag set would be a live-lock, because
            // `dirty |= any(pending)` keeps the loop composing every tick while the repaint and
            // resync gates (both `!c.pending`) guarantee nothing is ever sent to clear it. A
            // dropped EMPTY repaint frame is exactly that case.
            c.pending = false;
            continue;
        }
        let cells: Vec<WireCell> = changed
            .iter()
            .map(|(x, y, cell)| WireCell::of(*x, *y, cell))
            .collect();
        let frame = FrameMsg {
            epoch: c.epoch,
            cols,
            rows,
            full: c.needs_full,
            cells,
            cursor,
            cursor_visible: look.0,
            cursor_shape: look.1,
            repaint,
        };
        match c.out.try_send(ServerMsg::Frame(frame)) {
            // Advance this client's baseline only once it actually has the frame.
            Ok(()) => {
                c.last = buf.clone();
                // Anything that put every cell on this client's screen restarts the repaint
                // clock — a `full` frame no less than a `repaint` one.
                if let Some(p) = period
                    && (c.needs_full || repaint)
                {
                    c.repaint_at = now + p;
                }
                c.needs_full = false;
                c.pending = false;
                c.last_cursor = cursor;
                c.last_look = look;
            }
            // Coalesced under backpressure: the client did NOT get this frame. Leave
            // `last` un-advanced (so the next diff is the delta that catches it up) and
            // just flag it pending so the main loop reschedules a render. Do NOT set
            // needs_full — that would send a `full` frame carrying only a delta and wipe
            // the client's buffer.
            Err(TrySendError::Full(_)) => {
                c.pending = true;
                // A self-heal is best-effort: charge a failed repaint to the clock so a client
                // whose queue stays full is retried next INTERVAL rather than every tick.
                if let Some(p) = period
                    && repaint
                {
                    c.repaint_at = now + p;
                }
            }
            // The writer thread is gone: the socket died with writes failing while the
            // reader thread stayed blocked (no `Disconnect` will arrive). Prune it below so
            // its size stops pinning the shared viewport.
            Err(TrySendError::Disconnected(_)) => dead.push(c.id),
        }
    }
    // Pruning here also re-checks the just-sent frame against the surviving clients: this
    // loop only discovers a half-dead client DURING delivery, i.e. after composing from a
    // list that still contained it. See `prune_dead_clients`.
    prune_dead_clients(app, clients, &dead);
}

#[cfg(test)]
mod tests {
    use super::{Client, ctl_mutates, push_frames};
    use crate::config::MuxConfig;
    use crate::control::Req;
    use crate::proto::ServerMsg;
    use crate::tui::App;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect as RRect;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Keeps the peer ends of a test client's channel + socket alive: while held the frame
    /// sender stays connected, so a client is "live"; dropping the guard closes the receiver
    /// and its `out` sender reports `Disconnected` on the next send — a half-dead connection
    /// whose writer thread has gone away.
    struct ClientGuard {
        _rx: mpsc::Receiver<ServerMsg>,
        _peer: UnixStream,
    }

    /// Build a minimal `Client` at `(cols, rows)` plus its liveness guard. Drop the guard
    /// to simulate the writer thread dying (frame receiver gone) without an EOF/`Disconnect`.
    fn test_client(id: u64, cols: u16, rows: u16) -> (Client, ClientGuard) {
        let (out, rx) = mpsc::sync_channel::<ServerMsg>(1);
        let (conn, peer) = UnixStream::pair().expect("socketpair");
        let client = Client {
            id,
            out,
            conn,
            last: Buffer::empty(RRect::new(0, 0, cols.max(1), rows.max(1))),
            needs_full: true,
            pending: false,
            last_cursor: None,
            last_look: (true, 0),
            prefix: crate::tui::Prefix::default(),
            epoch: 0,
            repaint_at: Instant::now() + Duration::from_secs(3600),
            resync_at: Instant::now() + Duration::from_secs(3600),
            resync_band: 0,
            cols,
            rows,
            pid: None,
            env: Vec::new(),
            pending_copy: None,
            focused: true,
            last_title: None,
            pending_title: None,
            pending_bell: false,
            tag: None,
            heartbeat: false,
            last_seen: Instant::now(),
            replies: std::collections::VecDeque::new(),
            reply_bytes: 0,
            overflowed: false,
        };
        (
            client,
            ClientGuard {
                _rx: rx,
                _peer: peer,
            },
        )
    }

    fn cmd_app() -> App {
        let (mut cfg, _) = MuxConfig::load_from(std::path::Path::new("/nonexistent/mux.toml"));
        cfg.persist = false;
        App::new(80, 24, Vec::new(), Vec::new(), cfg).expect("app")
    }

    /// Render-channel commands (decisions #167): answered in order, and refused UNEXECUTED
    /// once the client stops draining replies — never executed-then-dropped.
    #[test]
    fn commands_reply_in_order_and_a_backlog_refuses_without_executing() {
        let mut app = cmd_app();
        let (c, guard) = test_client(1, 80, 24);
        let mut clients = vec![c];
        let mut kill = false;
        let new_session = || serde_json::json!({"cmd": "new-session", "detached": true});
        for i in 0..super::REPLY_QUEUE {
            super::handle_command(
                1,
                format!("c{i}"),
                new_session(),
                &mut app,
                &mut clients,
                &mut kill,
            );
        }
        let sessions = app.host_tree().sessions.len();
        assert_eq!(sessions, 1 + super::REPLY_QUEUE);
        // The client has not read a single reply: the next one is refused, not run.
        super::handle_command(
            1,
            "over".into(),
            new_session(),
            &mut app,
            &mut clients,
            &mut kill,
        );
        assert_eq!(
            app.host_tree().sessions.len(),
            sessions,
            "a refused command must not run"
        );
        let last = clients[0].replies.back().unwrap();
        let ServerMsg::Reply { id, resp } = last else {
            panic!("{last:?}")
        };
        assert_eq!(id, "over");
        assert_eq!(resp["code"], "busy");
        // Draining delivers the oldest first.
        super::drain_pending_replies(&mut app, &mut clients);
        let first = guard._rx.try_recv().unwrap();
        assert!(
            matches!(first, ServerMsg::Reply { ref id, .. } if id == "c0"),
            "{first:?}"
        );
    }

    #[test]
    fn a_client_that_never_reads_is_disconnected_not_left_hanging() {
        let mut app = cmd_app();
        let (c, _g) = test_client(1, 80, 24);
        let mut clients = vec![c];
        for i in 0..=super::REPLY_HARD as u64 {
            super::queue_reply(&mut clients[0], ServerMsg::Pong { id: i });
        }
        assert_eq!(clients[0].replies.len(), super::REPLY_HARD, "bounded");
        assert!(clients[0].overflowed);
        super::drain_pending_replies(&mut app, &mut clients);
        assert!(clients.is_empty(), "an overflowed client is pruned");
    }

    #[test]
    fn a_silent_heartbeat_client_is_detached_and_a_quiet_ordinary_one_is_not() {
        let mut app = cmd_app();
        let (mut a, _ga) = test_client(1, 80, 24);
        let (mut b, _gb) = test_client(2, 80, 24);
        a.heartbeat = true;
        a.last_seen = Instant::now() - super::HEARTBEAT_EXPIRY - Duration::from_secs(1);
        b.last_seen = a.last_seen; // no heartbeat promised: silence means nothing
        let mut clients = vec![a, b];
        assert!(super::expire_silent_clients(&mut app, &mut clients));
        assert_eq!(clients.iter().map(|c| c.id).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn a_newer_copy_wins_and_an_older_one_cannot_demote_it() {
        let (mut c, _guard) = test_client(1, 80, 24);

        super::queue_copy(&mut c, 5, "drag selection");
        assert_eq!(
            c.pending_copy.as_ref().map(|(s, t)| (*s, t.as_str())),
            Some((5, "drag selection"))
        );

        // A pane's OSC 52 write that happened EARLIER must not replace it — that is the
        // starvation/regression path: an undelivered drag copy silently becoming older text.
        super::queue_copy(&mut c, 4, "stale pane write");
        assert_eq!(
            c.pending_copy.as_ref().map(|(_, t)| t.as_str()),
            Some("drag selection")
        );

        // A genuinely later write does win — the clipboard has one slot.
        super::queue_copy(&mut c, 6, "later pane write");
        assert_eq!(
            c.pending_copy.as_ref().map(|(_, t)| t.as_str()),
            Some("later pane write")
        );
    }

    #[test]
    fn a_backpressured_copy_stays_pending_and_keeps_its_sequence() {
        let (mut c, guard) = test_client(1, 80, 24);
        // Fill the cap-1 channel so the copy's `try_send` reports `Full`.
        c.out
            .try_send(ServerMsg::Copy {
                text: "occupant".into(),
            })
            .expect("fill");

        let (mut cfg, _) = MuxConfig::load_from(std::path::Path::new("/nonexistent/mux.toml"));
        cfg.persist = false;
        let mut app = App::new(80, 24, Vec::new(), Vec::new(), cfg).expect("app");

        super::queue_copy(&mut c, 9, "blocked copy");
        let mut clients = vec![c];
        super::drain_pending_copies(&mut app, &mut clients);

        // Still queued for the next tick, and still at seq 9 — so an older pane write that
        // arrives in the meantime cannot take its place.
        let pending = clients[0].pending_copy.clone();
        assert_eq!(
            pending.as_ref().map(|(s, t)| (*s, t.as_str())),
            Some((9, "blocked copy"))
        );
        super::queue_copy(&mut clients[0], 8, "older");
        assert_eq!(
            clients[0].pending_copy.as_ref().map(|(_, t)| t.as_str()),
            Some("blocked copy")
        );
        drop(guard);
    }

    /// The rendered status bar (the frame's last row) as a plain string.
    fn status_row(app: &App) -> String {
        let (cols, rows) = app.size();
        let mut buf = Buffer::empty(RRect::new(0, 0, cols, rows));
        app.render_to(&mut buf);
        let y = rows - 1;
        (0..cols)
            .filter_map(|x| buf.cell(ratatui::layout::Position::new(x, y)))
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn the_status_bar_flags_an_armed_prefix_and_clears_it_again() {
        // The prefix is per-CLIENT state that the shared frame cannot see, so `push_frames`
        // has to fold it into the App each tick. Without that the `^b` mode indicator would
        // never appear — and, worse, could stick after the client that armed it went away.
        let (mut cfg, _) = MuxConfig::load_from(std::path::Path::new("/nonexistent/mux.toml"));
        cfg.persist = false;
        let mut app = App::new(80, 24, Vec::new(), Vec::new(), cfg).expect("app");

        let (idle, _idle_guard) = test_client(1, 80, 24);
        let (mut armed, _armed_guard) = test_client(2, 80, 24);
        armed.prefix = crate::tui::Prefix::armed();
        let mut clients = vec![idle, armed];

        push_frames(&mut app, &mut clients);
        assert!(
            status_row(&app).contains("^b"),
            "one client mid-chord must show the indicator: {:?}",
            status_row(&app)
        );

        // The chord completed (or the key was not bound): the flag must go with it.
        clients[1].prefix = crate::tui::Prefix::default();
        push_frames(&mut app, &mut clients);
        assert!(
            !status_row(&app).contains("^b"),
            "a resolved chord must clear the indicator: {:?}",
            status_row(&app)
        );

        // Armed again, then that client vanishes without ever resolving the chord — the
        // indicator is recomputed from the live client list, so it cannot outlive them.
        clients[1].prefix = crate::tui::Prefix::armed();
        push_frames(&mut app, &mut clients);
        assert!(status_row(&app).contains("^b"));
        clients.pop();
        push_frames(&mut app, &mut clients);
        assert!(
            !status_row(&app).contains("^b"),
            "a departed client must not leave the prefix flag stuck: {:?}",
            status_row(&app)
        );
    }

    /// A client removed by the dead-client PRUNE never reaches the `Disconnect` arm (its
    /// late `Disconnect` finds no client and no-ops), so if the prune didn't clear its pid
    /// the exited terminal would stay the newest raise candidate and shadow the client that
    /// is still attached — notification clicks would stop raising anything.
    #[test]
    fn pruning_a_dead_client_drops_its_raise_candidate() {
        let (mut cfg, _) = MuxConfig::load_from(std::path::Path::new("/nonexistent/mux.toml"));
        cfg.persist = false;
        let mut app = App::new(80, 24, Vec::new(), Vec::new(), cfg).expect("app");

        let (mut live, _live_guard) = test_client(1, 80, 24);
        let (mut dying, dying_guard) = test_client(2, 80, 24);
        live.pid = Some(1111);
        dying.pid = Some(2222);
        app.set_client_pid(1111);
        app.set_client_pid(2222);
        drop(dying_guard); // writer gone — discovered half-dead during delivery
        let mut clients = vec![live, dying];

        push_frames(&mut app, &mut clients);
        assert_eq!(clients.len(), 1, "the half-dead client is pruned");
        assert_eq!(
            app.raise_candidates(),
            &[1111],
            "the pruned client's pid must not outlive it"
        );
    }

    #[test]
    fn pruning_the_client_that_held_the_prefix_reschedules_a_frame() {
        // The nastier version of the same stuck-indicator risk: a client is only discovered
        // to be half-dead (writer gone, no `Disconnect`) DURING delivery, which is after the
        // frame was composed from a list that still contained it. If that client held the
        // prefix, every survivor has just been painted a `^b` belonging to nobody — and with
        // the loop about to clear `dirty`, nothing would ever repaint it.
        let (mut cfg, _) = MuxConfig::load_from(std::path::Path::new("/nonexistent/mux.toml"));
        cfg.persist = false;
        let mut app = App::new(80, 24, Vec::new(), Vec::new(), cfg).expect("app");

        let (live, _live_guard) = test_client(1, 80, 24);
        let (mut dying, dying_guard) = test_client(2, 80, 24);
        dying.prefix = crate::tui::Prefix::armed();
        drop(dying_guard); // its writer is gone — the send will report `Disconnected`
        let mut clients = vec![live, dying];

        push_frames(&mut app, &mut clients);
        assert_eq!(clients.len(), 1, "the half-dead client is pruned");
        assert!(
            clients[0].pending,
            "the survivor must be flagged for another frame — it was painted a `^b` that \
             the prune just invalidated"
        );

        // That rescheduled frame is what actually clears it.
        push_frames(&mut app, &mut clients);
        assert!(
            !status_row(&app).contains("^b"),
            "the corrected frame must drop the indicator: {:?}",
            status_row(&app)
        );
    }

    #[test]
    fn the_clipboard_delivery_path_also_unsticks_the_prefix() {
        // `drain_pending_copies` is the OTHER place a half-dead client is discovered, and it
        // runs AFTER the loop has already decided `dirty` — so pruning the prefix-holder
        // there is just as capable of stranding a lit `^b` on the survivors. Hence the
        // invalidation lives in `prune_dead_clients`, not at one call site.
        let (mut cfg, _) = MuxConfig::load_from(std::path::Path::new("/nonexistent/mux.toml"));
        cfg.persist = false;
        let mut app = App::new(80, 24, Vec::new(), Vec::new(), cfg).expect("app");
        app.set_prefix_armed(true); // what the frame already on screen says

        let (live, _live_guard) = test_client(1, 80, 24);
        let (mut dying, dying_guard) = test_client(2, 80, 24);
        dying.prefix = crate::tui::Prefix::armed();
        super::queue_copy(&mut dying, 1, "a drag copy it will never receive");
        drop(dying_guard);
        let mut clients = vec![live, dying];

        super::drain_pending_copies(&mut app, &mut clients);

        assert_eq!(clients.len(), 1, "the half-dead client is pruned here too");
        assert!(
            clients[0].pending,
            "the survivor must be flagged for a frame that drops the stale `^b`"
        );
    }

    #[test]
    fn dead_client_is_pruned_so_the_viewport_can_grow() {
        // Regression: a half-dead client (writer gone, no `Disconnect`) used to linger in
        // the client list and pin `recompute_viewport`'s `min()` at its small size forever,
        // so the shared view could never grow. `push_frames` must now prune it on the
        // `Disconnected` send and re-fit to the remaining (larger) client.
        // A non-existent path yields the built-in defaults (the private `default()` isn't
        // reachable here); persistence off so we boot a fresh workspace and never restore
        // the user's saved sessions.
        let (mut cfg, _) = MuxConfig::load_from(std::path::Path::new("/nonexistent/mux.toml"));
        cfg.persist = false;
        let mut app = App::new(80, 24, Vec::new(), Vec::new(), cfg).expect("app");

        // A live client that wants a big view, and a dead one stuck at a small size.
        let (big, _big_guard) = test_client(1, 200, 60);
        let (small, small_guard) = test_client(2, 80, 24);
        drop(small_guard); // the small client's writer is gone — it's now half-dead
        let mut clients = vec![big, small];

        super::recompute_viewport(&mut app, &mut clients);
        assert_eq!(app.size(), (80, 24), "the small dead client pins the min");

        push_frames(&mut app, &mut clients);

        assert_eq!(clients.len(), 1, "the dead client is pruned");
        assert_eq!(clients[0].id, 1);
        assert_eq!(
            app.size(),
            (200, 60),
            "the viewport grows to the surviving client"
        );
    }

    #[test]
    fn read_only_ctl_requests_do_not_force_a_render() {
        // Read-only queries must NOT dirty the frame (else `ctl list` polling defeats
        // idle-skip). Everything that changes state must.
        assert!(!ctl_mutates(&Req::List { caller: None }));
        assert!(!ctl_mutates(&Req::ListTabs { session: None }));
        assert!(!ctl_mutates(&Req::ListSessions));
        assert!(ctl_mutates(&Req::NewTab {
            session: None,
            from: None,
            cwd: None,
            name: None,
            no_focus: false,
            command: Vec::new(),
            env: Vec::new(),
            keep: false,
            owner: None,
        }));
        assert!(!ctl_mutates(&Req::Inbox {
            pane: "a-1".into(),
            ack: None
        }));
        assert!(ctl_mutates(&Req::Split {
            from: None,
            dir: "right".into(),
            no_focus: false,
            cwd: None,
            command: Vec::new(),
            env: Vec::new(),
            keep: false,
        }));
        assert!(ctl_mutates(&Req::Focus {
            index: 0,
            caller: None
        }));
        assert!(ctl_mutates(&Req::NewSession {
            name: None,
            cwd: None,
            detached: false,
        }));
    }

    /// Build an app whose self-heal cadence is `secs` (0 = off), with persistence disabled so
    /// the test never restores (or saves over) the user's real sessions.
    fn test_app(secs: u32) -> App {
        let (mut cfg, _) = MuxConfig::load_from(std::path::Path::new("/nonexistent/mux.toml"));
        cfg.persist = false;
        cfg.reconcile_secs = secs;
        App::new(80, 24, Vec::new(), Vec::new(), cfg).expect("app")
    }

    /// Drain whatever `push_frames` enqueued for a client, newest last.
    fn drain(rx: &mpsc::Receiver<ServerMsg>) -> Vec<crate::proto::FrameMsg> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let ServerMsg::Frame(f) = m {
                out.push(f);
            }
        }
        out
    }

    #[test]
    fn resync_bands_tile_the_screen_exactly_and_then_wrap() {
        // Every row must belong to exactly one band, or the walk would leave a stripe it never
        // resyncs — the one place a permanently-wrong baseline could hide from the self-heal.
        for rows in [1u16, 7, 8, 9, 24, 50, 200] {
            let mut covered = vec![0u32; rows as usize];
            let mut i = 0u16;
            while let Some((y0, h)) = super::resync_band_rows(rows, i) {
                assert!(h >= 1, "rows={rows} band={i} is empty");
                for y in y0..y0 + h {
                    covered[y as usize] += 1;
                }
                i += 1;
            }
            assert!(
                covered.iter().all(|n| *n == 1),
                "rows={rows} not tiled exactly once: {covered:?}"
            );
            assert!(i <= super::RESYNC_BANDS, "rows={rows} used {i} bands");
            // Past the last band the walk reports exhaustion so the caller wraps to 0.
            assert!(super::resync_band_rows(rows, i).is_none());
        }
        assert_eq!(
            super::resync_band_rows(0, 0),
            None,
            "a 0-row screen has no bands"
        );
    }

    #[test]
    fn a_resync_band_resends_cells_that_are_blank() {
        // THE point of the sentinel. The reported symptom is a stale glyph sitting where the
        // truth is a BLANK, so a resync that only re-sends non-blank cells would walk right
        // past it. Wiping the band to `Buffer::empty` instead of `unknown_cell` does exactly
        // that, because an empty cell equals the blank the compose produces.
        let area = RRect::new(0, 0, 4, 2);
        let blank = Buffer::empty(area); // what the server composed: all blanks
        let mut baseline = blank.clone(); // what the client is believed to hold: same

        assert!(
            baseline.diff(&blank).is_empty(),
            "precondition: an up-to-date baseline ships nothing"
        );

        super::invalidate_band(&mut baseline, 0, 1);
        let changed = baseline.diff(&blank);
        assert_eq!(
            changed.len(),
            4,
            "every cell of the invalidated row re-ships, blanks included"
        );
        assert!(
            changed.iter().all(|(_, y, _)| *y == 0),
            "only the invalidated band ships: {:?}",
            changed.iter().map(|(x, y, _)| (*x, *y)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_idle_client_gets_a_repaint_frame_carrying_no_cells() {
        // The silent repaint's whole economy: it asks the client to re-emit what it already
        // holds, so it must cost a flag and not a screenful of cells. If this ever starts
        // shipping cells, the cadence stops being free.
        let mut app = test_app(3);
        let (mut c, guard) = test_client(1, 80, 24);
        c.needs_full = false;
        c.last = Buffer::empty(RRect::new(0, 0, 80, 24));
        let mut clients = vec![c];

        // Bring the client fully up to date first (its baseline is empty, the app is not).
        push_frames(&mut app, &mut clients);
        let _ = drain(&guard._rx);
        assert!(!clients[0].needs_full, "the catch-up frame landed");

        // Nothing changed, but the repaint clock is due.
        clients[0].repaint_at = Instant::now() - Duration::from_secs(1);
        push_frames(&mut app, &mut clients);
        let frames = drain(&guard._rx);
        assert_eq!(
            frames.len(),
            1,
            "a due repaint sends a frame even with no delta"
        );
        assert!(frames[0].repaint, "and it is flagged as a repaint");
        assert!(!frames[0].full, "a repaint is not a clearing full frame");
        assert!(
            frames[0].cells.is_empty(),
            "a repaint ships no cells, got {}",
            frames[0].cells.len()
        );
        assert!(
            clients[0].repaint_at > Instant::now(),
            "the clock restarts, so it does not fire every tick"
        );
    }

    #[test]
    fn a_full_frame_is_not_also_flagged_repaint_and_restarts_the_clock() {
        // A `full` frame already paints every cell over a cleared screen. Flagging it
        // `repaint` too would make the client emit the entire screen a second time.
        let mut app = test_app(3);
        let (mut c, guard) = test_client(1, 80, 24);
        c.repaint_at = Instant::now() - Duration::from_secs(1); // due
        assert!(c.needs_full, "a fresh client starts needing a baseline");
        let mut clients = vec![c];

        push_frames(&mut app, &mut clients);
        let frames = drain(&guard._rx);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].full);
        assert!(
            !frames[0].repaint,
            "the baseline repaint is not doubled up with a silent one"
        );
        assert!(
            clients[0].repaint_at > Instant::now(),
            "a full frame restarts the repaint clock too — it painted everything"
        );
    }

    #[test]
    fn a_dropped_repaint_does_not_live_lock_the_render_loop() {
        // `pending` means the client missed the LAST frame, and `dirty |= any(pending)` keeps the
        // loop composing until it is cleared. But the repaint and resync gates are both
        // `!c.pending`, and an up-to-date client's diff is empty — so nothing would ever be sent
        // to clear it and the server would recompose at the frame rate forever. A dropped EMPTY
        // repaint frame walks straight into that, which is why the nothing-to-say path settles
        // the flag: an empty diff against an un-advanced baseline PROVES the client is current.
        let mut app = test_app(3);
        let (mut c, guard) = test_client(1, 80, 24);
        c.needs_full = false;
        c.last = Buffer::empty(RRect::new(0, 0, 80, 24));
        let mut clients = vec![c];
        push_frames(&mut app, &mut clients); // catch the client up
        let _ = drain(&guard._rx);

        // A repaint comes due while the client's cap-1 queue is already occupied.
        clients[0].repaint_at = Instant::now() - Duration::from_secs(1);
        clients[0]
            .out
            .try_send(ServerMsg::Bye)
            .expect("occupy the queue");
        push_frames(&mut app, &mut clients);
        assert!(clients[0].pending, "the repaint frame was dropped");
        assert!(
            clients[0].repaint_at > Instant::now(),
            "a failed repaint is charged to the clock, so a stuck client is retried next              interval rather than every tick"
        );

        // The queue drains; nothing on screen changed.
        let _ = drain(&guard._rx);
        push_frames(&mut app, &mut clients);
        assert!(
            !clients[0].pending,
            "an empty diff settles `pending` — otherwise the loop never goes idle again"
        );
        assert!(
            !super::reconcile_due(&app, &clients),
            "and with the flag settled and the clock charged, the loop has nothing left to do"
        );
    }

    #[test]
    fn the_self_heal_never_ticks_when_it_is_switched_off_or_nobody_is_watching() {
        // `reconcile_due` is the only dirty source that fires with the screen unchanged, so it
        // is also the only one that could keep an idle server composing forever. Both guards
        // matter: `reconcile_secs = 0` (user opted out) and no clients (detached, which the
        // CPU throttle depends on).
        let off = test_app(0);
        let (mut c, _g) = test_client(1, 80, 24);
        c.repaint_at = Instant::now() - Duration::from_secs(60);
        c.resync_at = Instant::now() - Duration::from_secs(60);
        let clients = vec![c];
        assert!(
            !super::reconcile_due(&off, &clients),
            "reconcile_secs = 0 must never mark the frame dirty"
        );

        let on = test_app(3);
        assert!(
            !super::reconcile_due(&on, &[]),
            "a detached server has no client to heal"
        );
        assert!(
            super::reconcile_due(&on, &clients),
            "an attached client past its due time is the case this exists for"
        );
    }
}
