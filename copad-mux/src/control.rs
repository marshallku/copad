//! The control API: a Unix-socket protocol that lets `comux ctl <cmd>` drive
//! a running TUI (like `tmux`/`tmx`). This module holds the wire types, the socket
//! path resolution, and the CLI client. The server side lives in [`crate::tui`]
//! and honors the single-writer rule (spec §1): the socket thread never touches
//! `State` — it hands requests to the main loop over an mpsc channel.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cliargs::{self, Positionals, Spec, flag, opt};
use crate::picker;

/// A control request. Wire form: one JSON object per line, tagged by `cmd`
/// (e.g. `{"cmd":"list"}`, `{"cmd":"split","dir":"right"}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Req {
    /// List panes of the active tab, or of the caller's tab (see `caller`).
    List {
        /// The CALLER's pane token. An index then counts in the caller's tab, wherever it is,
        /// instead of the tab on screen — so `list` and the index verbs agree for an agent in a
        /// background tab. Absent = the active tab, as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    /// Split a pane. `dir` = `"right"` (side by side) | `"down"` (stacked).
    ///
    /// `from` names the pane to split, by TOKEN, in any tab of any session. Without it
    /// the FOCUSED pane is split — fine for a human pressing a key, a race for a script:
    /// focus is mutable, and the new pane also inherits the split source's cwd, so a focus
    /// change between listing and splitting silently puts the pane in the wrong directory.
    /// An agent should always pass `from`.
    ///
    /// The response carries the created pane's token in `Resp.pane`, so a caller never has
    /// to infer which pane appeared.
    Split {
        dir: String,
        #[serde(default)]
        from: Option<String>,
        /// Leave focus where it is. A split in a tab nobody is looking at never moves focus.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_focus: bool,
        /// Start the new pane here instead of in the split source's cwd.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// `comux split … -- argv`: run this as the new pane's program (as `spawn` does for a
        /// tab) — how a task is started inside an already-open tab without typing into a
        /// shell. Closes with the program unless `keep`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        command: Vec<String>,
        /// `KEY=VALUE` pairs added to the new pane's environment.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keep: bool,
    },
    /// Grow the pane at `index` toward `dir` (`left`/`right`/`up`/`down`) by nudging
    /// its split divider.
    ResizePane {
        index: usize,
        dir: String,
        /// The CALLER's pane token. An index then counts in the caller's tab, wherever it is,
        /// instead of the tab on screen — so `list` and the index verbs agree for an agent in a
        /// background tab. Absent = the active tab, as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    /// Focus the pane at `index` (as printed by `list`).
    Focus {
        index: usize,
        /// The CALLER's pane token. An index then counts in the caller's tab, wherever it is,
        /// instead of the tab on screen — so `list` and the index verbs agree for an agent in a
        /// background tab. Absent = the active tab, as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    /// Close the pane at `index`.
    Close {
        index: usize,
        /// The CALLER's pane token. An index then counts in the caller's tab, wherever it is,
        /// instead of the tab on screen — so `list` and the index verbs agree for an agent in a
        /// background tab. Absent = the active tab, as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    /// Inject `text` as input bytes into a pane (like `tmux send-keys`).
    ///
    /// `target` is a pane TOKEN and resolves anywhere in the mux; `index` is a position in
    /// the ACTIVE TAB. Exactly one must be given — a write must never default to the focused
    /// pane, and an index silently retargets when the user switches tabs.
    ///
    /// Deliberately token-only, unlike the read verbs which also accept a raw terminal id:
    /// terminal ids restart at `term0` every server incarnation, so a recorded one can
    /// address an unrelated pane after a restart. Tolerable for a read, not for a write.
    ///
    /// `index` keeps its old wire shape (`{"cmd":"send-keys","index":0,"text":"…"}`), so a
    /// request from an older client still parses and behaves identically.
    SendKeys {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        index: Option<usize>,
        text: String,
        /// Send `text` the way a terminal PASTES it (bracketed when the program asked for
        /// it, delimiters inside the text stripped) rather than as typed bytes.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        paste: bool,
        /// Press Enter after the text.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        enter: bool,
        /// The CALLER's pane token. An index then counts in the caller's tab, wherever it is,
        /// instead of the tab on screen — so `list` and the index verbs agree for an agent in a
        /// background tab. Absent = the active tab, as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    /// List a session's tabs — the active one, or `session` (a selector).
    ListTabs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Create a new tab. With every field empty (an older client's shape) it is created in
    /// the session on screen and made active — the `Ctrl-b c` behavior.
    ///
    /// `session` (a selector) creates it there instead, and `no_focus` creates it without
    /// selecting it: either way NOTHING anyone is looking at moves — not the user's session,
    /// not that session's active tab. A `no_focus` tab with no `session` lands in the
    /// caller's (`from`'s) session, beside the pane that asked for it.
    ///
    /// The shell starts in `cwd`, else in the caller's (`from`, a pane token) directory, else
    /// in the focused pane's. `name` names the tab.
    NewTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_focus: bool,
        /// `comux spawn`: run this argv as the tab's program instead of a bare shell. It runs
        /// under the user's login shell (so PATH and profile apply) without ever being typed
        /// into one, and the pane closes when it exits — unless `keep`, which leaves a shell.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        command: Vec<String>,
        /// `KEY=VALUE` pairs added to the new pane's environment.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keep: bool,
        /// The pane (a TOKEN) that owns the new tab — what `close-tab --owned-by` and
        /// `list-tree --owner` select on, and where `notify --to owner` delivers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
    },
    /// Make the tab at `index` (as printed by `list-tabs`) active.
    SelectTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
        /// A selector (`s1:t0`, `hub/build`, …; see [`crate::selector`]) instead of `index`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// Close the tab at `index` (as printed by `list-tabs`) and reap its shells —
    /// the `Ctrl-b &` / context-menu "close tab" action, reachable from a script.
    /// Refused when it is the session's LAST tab (a session always keeps ≥1; kill the
    /// session instead). Unlike the TUI there is no confirm — tmux `kill-window` parity.
    CloseTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
        /// A selector (`s1:t0`, `hub/build`, …; see [`crate::selector`]) instead of `index`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// Rename the tab at `index` (as printed by `list-tabs`), or the ACTIVE tab when
    /// `index` is `None` — so a shell inside a pane can rename its own tab without
    /// knowing its position. An empty `name` clears back to the process/index label.
    RenameTab {
        #[serde(default)]
        index: Option<usize>,
        /// The CALLER's pane token (`$COPAD_MUX_PANE`): with no `index`, the tab holding that
        /// pane is renamed — wherever it is — rather than whatever tab the user is looking at.
        /// An older client sends neither, and keeps the legacy "active tab" meaning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        /// A tab selector; beats `pane`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        name: String,
    },
    /// List the sessions (workspaces).
    ListSessions,
    /// Create a new session and switch to it. `name` is the tmux-style display name
    /// (`None` → shown by its generated `sN` id); `cwd` is the directory to start its
    /// shell in (the CLI fills it with the caller's cwd, so `comux new-session` starts
    /// where you ran it — like `tmx`).
    NewSession {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        /// tmux `new-session -d`: create it without switching anyone to it. The response
        /// still names the new session/tab/pane.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        detached: bool,
    },
    /// Rename the session at `index` (as printed by `list-sessions`), or the ACTIVE
    /// session when `index` is `None`. An empty `name` clears it back to the
    /// generated id.
    RenameSession {
        #[serde(default)]
        index: Option<usize>,
        /// The caller's pane token; see [`Req::RenameTab::pane`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        /// A session selector; beats `pane`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        name: String,
    },
    /// Make the session at `index` (as printed by `list-sessions`) active.
    SelectSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
        /// A selector (`s1:t0`, `hub/build`, …; see [`crate::selector`]) instead of `index`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// Jump to a pane ANYWHERE in the mux by identity rather than by position: switch to
    /// its session, select its tab, focus it. `target` is a pane token (the
    /// `$COPAD_MUX_PANE` a pane's own shell carries) or a raw terminal id.
    ///
    /// This is what a desktop notification's click action runs. The response carries the
    /// pid of the terminal emulator hosting an attached client (`raise_pid`), which the
    /// CLI — not the server — then activates: the server's environment has been scrubbed
    /// of the session variables a GUI command needs, and a long-lived daemon is the wrong
    /// process to attribute macOS automation permission to.
    Jump { target: String },
    /// Move the shared view along the navigation history (decisions #140): `back`/`forward`
    /// walk it like a browser, `last` toggles to the previous place like Alt-Tab.
    Nav { dir: NavDir },
    /// Toggle the zoom of the active tab's focused pane (`Ctrl-b z`). Moves the shared view.
    Zoom,
    /// Name a pane by its token (`Ctrl-b T`); an empty name clears it.
    RenamePane { target: String, name: String },
    /// Rearrange the tab holding the caller's pane (or the focused one): `op` is a preset
    /// (`even-horizontal`, `even-vertical`, `main-horizontal`, `main-vertical`, `tiled`),
    /// `next`, `spread`, `swap-up`/`swap-down`, or `rotate`/`rotate-back` (decisions #158).
    Arrange {
        op: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    /// Move the caller's pane (or the focused one) into a new tab of its own (`break-pane`).
    BreakPane {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    /// Move the caller's tab (or the active one) to position `to` (0-based).
    MoveTab {
        to: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    /// Fork a live agent pane's conversation into a new tab of its session (decisions #150).
    Fork {
        target: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_focus: bool,
    },
    /// Pin `target` (a pane token) to `slot` 1–9, or empty the slot with `clear`.
    Mark {
        slot: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clear: bool,
    },
    /// The pinned slots. Read-only.
    Marks,
    /// Go to the pane pinned in `slot`. Moves the shared view.
    GotoMark { slot: u8 },
    /// Raise an agent notification on behalf of a pane: a desktop toast whose click jumps
    /// back to it, plus an entry in the in-app notification center. The entry point for
    /// agent hooks (Claude `Stop`/`Notification`, Codex turn-complete), which know the
    /// exact moment and message the server's ~2 Hz status sweep can only infer.
    Notify {
        /// Pane token / terminal id to attribute it to. Never defaulted to the active
        /// pane — a misattributed jump is worse than a rejected notification.
        target: String,
        /// `done` (turn finished) or `blocked` (awaiting input).
        kind: String,
        /// Toast body. The title is composed server-side from the agent + session.
        body: String,
    },
    /// Kill the session at `index` (as printed by `list-sessions`): drop its tabs and
    /// reap every shell in them, switching to a survivor when it was the active one.
    /// Refused when it is the LAST session (the mux keeps ≥1). Unlike the TUI's
    /// `Ctrl-b X` there is no y/n confirm — tmux `kill-session` parity.
    KillSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
        /// A selector (`s1:t0`, `hub/build`, …; see [`crate::selector`]) instead of `index`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// Create a git worktree for `branch` (sibling of the repo's MAIN worktree) and open
    /// a session in it, switching to it. `cwd` is the caller's dir (the repo is resolved
    /// from it); `from` is the base ref for the new branch (`None` → HEAD).
    WorktreeCreate {
        branch: String,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// List the git worktrees of the repo containing `cwd`, flagging which ones a comux
    /// session is currently inside (`live`).
    WorktreeList {
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Remove the worktree matching `target` (a path or short branch, main excluded).
    /// Refuses a live-session worktree unless `force` (which first kills those sessions);
    /// `delete_branch` also deletes the branch.
    WorktreeRm {
        target: String,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        delete_branch: bool,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Read a pane's text back (`comux capture-pane`) — the counterpart to
    /// [`Req::SendKeys`], and the primitive an agent needs to observe a sibling pane.
    ///
    /// Addressing precedence: `target` (a pane token / terminal id, resolvable ANYWHERE in
    /// the mux) > `index` (into the active tab, as printed by `list`) > the focused pane.
    /// Supplying both `target` and `index` is a usage error rather than a silent
    /// precedence win — a capture of the wrong pane is a wrong ANSWER, and answers get
    /// acted on.
    ///
    /// `lines` counts PHYSICAL grid rows ending at the LIVE screen bottom (not the
    /// displayed viewport, so a human scrolling in copy-mode cannot change what a script
    /// reads), reaching back into scrollback. `None` = the visible screen's worth. `0` is
    /// refused. Read-only: it must stay out of `ctl_mutates`, since this is a verb callers
    /// poll.
    CapturePane {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        index: Option<usize>,
        #[serde(default)]
        lines: Option<usize>,
        /// The CALLER's pane token. An index then counts in the caller's tab, wherever it is,
        /// instead of the tab on screen — so `list` and the index verbs agree for an agent in a
        /// background tab. Absent = the active tab, as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    /// List AI-agent panes across every session (what the sidebar's `agents` half and the
    /// `Ctrl-f` switcher already show), optionally narrowed to one pane.
    ///
    /// `target` is what makes `wait-agent` implementable: a parameterless listing cannot
    /// tell a pane that does NOT EXIST from a pane that exists but is not classified as an
    /// agent yet, and those need opposite handling (fail vs keep waiting — classification is
    /// cached and refreshed only every 500ms attached / 5s detached). With a target the
    /// server resolves it against the live pane set on EVERY request — so a pane closing
    /// mid-wait is caught — and answers:
    /// - unknown pane -> `ok = false`
    /// - pane present, not an agent -> `agents = Some([])`
    /// - pane present and an agent -> `agents = Some([info])`
    ///
    /// Read-only, and reads CACHED state only (no process sweep, no PTY snapshot), so it is
    /// safe to poll and must stay out of `ctl_mutates`.
    ListAgents {
        #[serde(default)]
        target: Option<String>,
    },
    /// The whole mux as one tree — sessions, their tabs, their panes — or one `session`
    /// (a selector). Read-only.
    ListTree {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// Only the tabs this pane (a token) owns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
    },
    /// Close every tab `owner` (a pane token) owns. `dry_run` only reports them. Refused as a
    /// whole when one of them holds `caller`, so a cleanup can never close the pane running it.
    /// Type the same input into many panes (decisions #145): a session's or the whole mux's,
    /// optionally only agents / agents in one status. Every target is resolved and checked
    /// BEFORE any byte is written; writes are queued to each pane like `send-keys`, with no
    /// rollback, so retrying after a lost response can type twice. The caller's own pane is
    /// never a target.
    Broadcast {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        agents: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        paste: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        enter: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dry_run: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    CloseOwned {
        owner: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dry_run: bool,
    },
    /// Leave a message in a pane's inbox — a child reporting to its parent. `to` is a pane
    /// token, or `owner` for the owner of `from`'s tab. No toast: the reader is a program.
    Deliver {
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        kind: String,
        #[serde(default)]
        body: String,
        /// A file the sender left for the reader (an absolute path). A reference only: comux
        /// never reads it, and keeping it until it is read is the sender's job.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    /// Read a pane's inbox. Non-destructive: messages stay until acknowledged, so a reader
    /// that dies before printing them loses nothing. `ack` first drops every message with
    /// `seq <= ack` — at-least-once delivery, deduplicated by `seq`.
    Inbox {
        pane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ack: Option<u64>,
    },
    /// Re-read `mux.toml` and apply the live-reloadable settings to the running server
    /// WITHOUT restarting it — like tmux `source-file`. Keybindings, mouse, sidebar
    /// width, usage/tab-label display, notify, and worktree config take effect on the
    /// next frame. Settings baked in at server start (environment refresh, persistence
    /// cadence, restore lists) are NOT changed — those still need `comux server restart`.
    /// `Resp.message` carries the config path + any parse warnings + the restart hint.
    ReloadConfig,
    /// Runtime counters of the RUNNING server (pane/label coverage + process-sweep
    /// failures), for `comux doctor`. Read-only; safe to poll.
    Health,
    /// The host machine's CPU / memory / GPU / load, as the status surfaces see them.
    /// Read-only; served from the poller's last reading, so it never blocks the loop.
    Host,
    /// Shut the persistent server down (drops every shell). The only key-free way to
    /// stop a detached server short of exiting its last shell.
    KillServer,
}

/// Runtime counters from a live server (`health`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthInfo {
    /// Panes the server currently hosts.
    pub panes: usize,
    /// How many of those have a resolved foreground-process label. A shortfall that
    /// persists means the sweep can see the pane's shell but not classify it.
    pub labeled: usize,
    /// Process sweeps that failed OUTRIGHT since server start. Non-zero means labels
    /// were carried forward rather than refreshed — the condition that used to show
    /// up only as tab names and the sidebar agents list blinking out together.
    pub label_sweeps_failed: u64,
    /// The server's soft `RLIMIT_NOFILE`. Every pane costs about
    /// [`crate::fdlimit::FDS_PER_PANE`] descriptors, so this is the real ceiling on
    /// how many panes the server can host — and the one that used to make new-tab /
    /// new-session fail with no visible reason.
    ///
    /// `serde(default)` on the fd fields: a server started from an OLDER binary
    /// answers `health` without them, and a hard parse failure there would turn a
    /// working (if outdated) server into "health probe failed".
    #[serde(default)]
    pub fd_soft: Option<u64>,
    /// Descriptors the server currently holds open, when countable.
    #[serde(default)]
    pub fd_open: Option<usize>,
    /// Request semantics this server implements beyond the original wire, so a newer CLI
    /// can REFUSE an operation an older server would silently misread (it ignores unknown
    /// fields, so e.g. a caller-targeted rename would rename the user's active tab instead).
    #[serde(default)]
    pub caps: Vec<String>,
}

/// Capability: `rename-tab`/`rename-session` honor `pane` (the caller's pane).
pub const CAP_RENAME_CALLER: &str = "rename-caller";

/// Capability: session/tab verbs honor `target` selectors, and `list-tabs` honors `session`.
pub const CAP_SELECTORS: &str = "selectors";

/// Capability: `list-tree` exists.
pub const CAP_LIST_TREE: &str = "list-tree";

/// Capability: `send` honors `paste` and `enter`.
pub const CAP_SEND_FLAGS: &str = "send-flags";

/// Capability: `new-tab` honors `session`/`cwd`/`name`/`no_focus`, and `split` honors
/// `no_focus` and a `from` outside the active tab.
pub const CAP_PLACEMENT: &str = "placement";

/// Capability: `new-tab` honors `command`/`env`/`keep` (`comux spawn`).
pub const CAP_SPAWN: &str = "spawn";

/// Capability: the index verbs honor `caller` — an index counts in the caller's tab.
pub const CAP_CALLER_INDEX: &str = "caller-index";

/// Capability: tabs carry an `owner` (`new-tab`/`spawn --owner`, `list-tree --owner`,
/// `close-tab --owned-by`).
pub const CAP_OWNER: &str = "owner";

/// Capability: pane inboxes (`notify --to`, `inbox`).
pub const CAP_INBOX: &str = "inbox";

/// Every capability this build's server implements (reported by `health`).
pub const SERVER_CAPS: &[&str] = &[
    CAP_RENAME_CALLER,
    CAP_SELECTORS,
    CAP_LIST_TREE,
    CAP_SEND_FLAGS,
    CAP_PLACEMENT,
    CAP_SPAWN,
    CAP_CALLER_INDEX,
    CAP_OWNER,
    CAP_INBOX,
    CAP_BROADCAST,
    CAP_MARKS,
    CAP_FORK,
    CAP_DETACHED_SESSION,
    CAP_SPLIT_SPAWN,
    CAP_ARRANGE,
];

/// Capability: `new-session --detached` (an older server would ignore the field and switch).
pub const CAP_DETACHED_SESSION: &str = "detached-session";

/// Capability: `split --cwd/--env/--keep -- argv` (an older server would ignore them and
/// answer `ok` with a bare shell — a task reported as started that never ran).
pub const CAP_SPLIT_SPAWN: &str = "split-spawn";

/// The most `send --file` reads. A paste larger than this is a mistake (a binary, a log),
/// not something to type into a shell.
const SEND_FILE_CAP: u64 = 1024 * 1024;

/// One pinned slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkInfo {
    pub slot: u8,
    pub pane: String,
    pub session: String,
    pub tab: String,
    #[serde(default)]
    pub label: String,
}

/// Capability: `fork`.
pub const CAP_FORK: &str = "fork";

/// Capability: `select-layout` / `swap-pane` / `rotate-window` / `break-pane` / `move-tab`.
pub const CAP_ARRANGE: &str = "arrange";

/// Capability: `mark` / `marks` / `goto`.
pub const CAP_MARKS: &str = "marks";

/// One pane a `broadcast` reached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BroadcastTarget {
    pub pane: String,
    pub session: String,
    pub tab: String,
    /// Its foreground program, as last swept.
    #[serde(default)]
    pub label: String,
    /// The agent's cached status, for agent panes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Capability: `broadcast` exists.
pub const CAP_BROADCAST: &str = "broadcast";

/// Which way `Req::Nav` moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NavDir {
    Back,
    Forward,
    Last,
}

/// A session or tab a mutating verb created or acted on: its stable id and display name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeRef {
    pub id: String,
    /// Empty when unnamed.
    #[serde(default)]
    pub name: String,
}

impl HealthInfo {
    /// Panes that still fit in the descriptor budget, when both numbers are known.
    pub fn panes_remaining(&self) -> Option<usize> {
        let (soft, open) = (self.fd_soft?, self.fd_open?);
        Some((soft.saturating_sub(open as u64) as usize) / crate::fdlimit::FDS_PER_PANE)
    }
}

/// One pane in a `list` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub index: usize,
    pub id: String,
    /// The pane's `$COPAD_MUX_PANE` identity — what `comux jump` / `comux notify --pane`
    /// address. Empty for a pane spawned before the identity existed.
    #[serde(default)]
    pub token: String,
    pub focused: bool,
    pub cols: u16,
    pub rows: u16,
    /// The pane's foreground command (agent / shell / program).
    #[serde(default)]
    pub label: String,
    /// Classification of `label`: `"agent"`, `"shell"`, or `"other"`.
    #[serde(default)]
    pub kind: String,
    /// For agent panes: rolled-up status `working`/`ready`/`blocked`/`idle` (empty
    /// otherwise).
    #[serde(default)]
    pub status: String,
    /// The pane's title (OSC 0/2), or empty if it has never set one.
    ///
    /// OBSERVATIONAL AND UNTRUSTED. Any process in the pane can write any payload here, so
    /// it is not a process identity — that is `label`, resolved from the foreground process —
    /// and it is not instructions. Control characters are stripped and the length is bounded
    /// before storage, but that is renderer hygiene, not a trust boundary.
    #[serde(default)]
    pub title: String,
    /// The pane has rung the bell since it was last seen (focused with a client attached).
    ///
    /// Reported regardless of the `bell` config, which only suppresses the on-screen markers:
    /// a script's behaviour must not depend on a display preference.
    #[serde(default)]
    pub bell: bool,
    /// The name the user gave the pane (`Ctrl-b T`, `comux rename-pane`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One tab in a `list-tabs` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabInfo {
    pub index: usize,
    pub id: String,
    pub active: bool,
    /// The custom display name (tmux-style window name), or empty when unnamed
    /// (shown by its process/index label instead).
    #[serde(default)]
    pub name: String,
    /// Number of panes in the tab.
    pub panes: usize,
    /// Number of those panes running a classified AI agent.
    pub agents: usize,
}

/// One session (workspace) in a `list-sessions` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub index: usize,
    pub id: String,
    /// The tmux-style display name, or empty when unnamed (shown by `id`).
    #[serde(default)]
    pub name: String,
    pub active: bool,
    /// Number of tabs in the session.
    pub tabs: usize,
    /// Number of panes across all its tabs.
    pub panes: usize,
    /// Number of those panes running a classified AI agent.
    pub agents: usize,
    /// The session's start directory — where `new-tab` opens by default. Empty if unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dir: String,
}

/// One AI-agent pane in a `list-agents` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentInfo {
    /// The pane's `$COPAD_MUX_PANE` identity — what `jump` / `capture-pane` / `wait-agent`
    /// address. EMPTY for a pane spawned before pane tokens existed, which is why
    /// `terminal` is kept as an addressable fallback.
    #[serde(default)]
    pub token: String,
    /// The pane's terminal id.
    pub terminal: String,
    /// The session (space) the agent lives in — its DISPLAY name, which is what the sidebar
    /// shows and what a rename changes.
    #[serde(default)]
    pub space: String,
    /// The session's STABLE id (`WorkspaceId`), which a rename does not change.
    ///
    /// `space` alone cannot key a join: a caller that lists sessions and agents in two round
    /// trips can have a rename land between them, and two sessions may legitimately carry the
    /// same display name. This is the field to group agents by; `space` is for showing.
    #[serde(default)]
    pub space_id: String,
    /// The tab's custom name, else `tab <n>` — what the sidebar titles the row with.
    #[serde(default)]
    pub title: String,
    /// The agent's foreground command (`claude`, `codex`, …).
    #[serde(default)]
    pub tool: String,
    /// Rolled-up status: `working` / `ready` / `blocked` / `idle`.
    ///
    /// CACHED AND INFERRED. Claude's comes from `~/.claude/sessions/<pid>.json`; everything
    /// else falls back to matching substrings on the pane's screen, so ordinary output that
    /// happens to contain a prompt-like phrase can read as `blocked`. `idle` additionally
    /// means "no recognized UI", which is also what an unresolved reading looks like.
    pub status: String,
    /// Whole seconds the agent has held `status`.
    pub for_secs: u64,
    /// Bumped on every observed status change, mux-wide and never reused within a server's
    /// life: `wait-agent --after-seq <seq>` waits for a change NEWER than this reading. Only
    /// comparable on the server that issued it (see `wait-agent --help`).
    #[serde(default)]
    pub seq: u64,
    /// What the agent was last seen DOING (`Bash: run the tests`, `running: cargo test`),
    /// read from the tool's own structured log.
    ///
    /// ABSENT means "no reading", never "doing nothing" — the agent may be a tool whose log
    /// we cannot read, one that has run no tools yet, or one whose log format has moved. Do
    /// not branch on its absence; it is a hint for a human scanning a list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The tab the agent lives in: its STABLE id (`s4:t13`, a tab selector), its custom name
    /// (empty when unnamed — `title` then shows `tab <n>`), and its index in the session.
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub tab_name: String,
    #[serde(default)]
    pub tab_index: usize,
    /// The pane's index within its tab, in `list` order.
    #[serde(default)]
    pub pane_index: usize,
    /// The agent's OWN conversation id (Claude's session uuid, Codex's rollout uuid) — not a
    /// comux session. Absent when its log has not been resolved (see `agentpoll::conversation`
    /// for how stale it can be).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    /// The file that conversation is logged to, for reading its progress directly instead of
    /// scraping the pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

/// One session in a `list-tree` response, with its tabs and their panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeSession {
    pub index: usize,
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// The session the user is looking at.
    pub active: bool,
    pub tabs: Vec<TreeTab>,
}

/// One tab in a `list-tree` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeTab {
    pub index: usize,
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Its session's active tab.
    pub active: bool,
    /// The token of the pane that owns this tab, while that pane is alive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub panes: Vec<TreePane>,
}

/// One pane in a `list-tree` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreePane {
    pub index: usize,
    /// The `$COPAD_MUX_PANE` identity — the address to use.
    #[serde(default)]
    pub token: String,
    pub terminal: String,
    /// Its tab's focused pane.
    pub focused: bool,
    #[serde(default)]
    pub label: String,
    /// `agent` / `shell` / `other`, as in `list`.
    #[serde(default)]
    pub kind: String,
    /// Agents only: `working` / `ready` / `blocked` / `idle`.
    #[serde(default)]
    pub status: String,
}

/// The host machine, as `comux host` reports it.
///
/// Every field is optional and ABSENT means "could not read", never zero — a readout that
/// prints `cpu 0%` when the probe failed is making a claim about the machine.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct HostInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load1: Option<f64>,
}

/// One git worktree in a `worktree list` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    /// Short branch name, or empty when detached.
    #[serde(default)]
    pub branch: String,
    /// The main worktree (never a removal target).
    pub is_main: bool,
    /// A comux session currently has a pane inside this worktree.
    pub live: bool,
    /// `git worktree lock`ed.
    #[serde(default)]
    pub locked: bool,
}

/// A control response. `ok=false` carries `error`; `list` fills `panes`+`focused`;
/// `list-tabs` fills `tabs`+`active_tab`; `worktree` verbs fill `worktrees`/`message`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resp {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Machine-readable reason for `ok = false`: `not_found`, `ambiguous`, `refused`,
    /// `spawn_failed`, `invalid`, `unsupported`, or `error` when unclassified. (Usage errors
    /// never reach the server; the CLI prints `usage` itself.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// `new-session` / `new-tab` / `split`: the session the result lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<NodeRef>,
    /// `new-session` / `new-tab` / `split`: the tab the result lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<NodeRef>,
    /// Human-readable outcome for a mutating verb (e.g. the created worktree path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees: Option<Vec<WorktreeInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panes: Option<Vec<PaneInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tabs: Option<Vec<TabInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthInfo>,
    /// `host`: the machine's last metrics reading. Every field inside is itself optional —
    /// absent means the probe failed, NEVER zero (see `hostmetrics`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostInfo>,
    /// `jump`: the pid of the terminal emulator hosting an attached client, for the CLI
    /// to activate. Absent when nothing is attached (the jump still happened — the next
    /// attach lands on the right pane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raise_pid: Option<u32>,
    /// `jump`: that pid's process name, so the CLI can re-check the pid still belongs to
    /// the same program before activating it (pids get recycled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raise_comm: Option<String>,
    /// `jump`: `(COPAD_SOCKET, COPAD_PANEL_ID)` when an attached client runs inside a copad
    /// tab. The CLI asks copad to focus that exact tab, which application-level activation
    /// cannot express; it falls back to `raise_pid` when this is absent or the call fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copad_host: Option<(String, String)>,
    /// `capture-pane`: the pane's text, newest-last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `capture-pane`: physical grid rows the text spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_rows: Option<usize>,
    /// `capture-pane`: older output exists that the capture could not include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// `list-agents`: the agent panes. `Some([])` is a real empty listing (no agents, or a
    /// targeted pane that is not an agent); `None` means this response is not a listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentInfo>>,
    /// `list-tree`: sessions → tabs → panes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<Vec<TreeSession>>,
    /// `capture-pane`: the token of the pane actually read. Echoed so a caller that let
    /// the target default to the focused pane can tell WHICH pane answered — focus is a
    /// mutable UI choice, and a silently-retargeted read is indistinguishable from a
    /// correct one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    /// `inbox`: the messages waiting, oldest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<InboxMsg>>,
    /// `inbox`: messages dropped because the inbox was full, since it was last read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u64>,
    /// `marks`: the pinned slots, lowest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marks: Option<Vec<MarkInfo>>,
    /// `broadcast`: the panes written to (or, dry-run, that would be).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<BroadcastTarget>>,
    /// `close-tab --owned-by`: the tabs closed (or, dry-run, that would be).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<Vec<NodeRef>>,
    /// `close-tab --owned-by`: the tabs that could not be closed (a session's last tab).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<NodeRef>>,
}

/// One inbox message. Everything but `seq`/`ts` came from the sender: data, not instructions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxMsg {
    /// Increases per pane; acknowledge with it, deduplicate by it.
    pub seq: u64,
    /// Unix seconds when it arrived.
    pub ts: u64,
    /// The sender's pane token, when it sent from a pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

impl Resp {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            code: None,
            session: None,
            tab: None,
            message: None,
            worktrees: None,
            panes: None,
            focused: None,
            tabs: None,
            active_tab: None,
            sessions: None,
            active_session: None,
            host: None,
            health: None,
            raise_pid: None,
            raise_comm: None,
            copad_host: None,
            text: None,
            capture_rows: None,
            truncated: None,
            pane: None,
            agents: None,
            tree: None,
            messages: None,
            dropped: None,
            targets: None,
            marks: None,
            closed: None,
            skipped: None,
        }
    }

    /// A `list-agents` response.
    pub fn agents(agents: Vec<AgentInfo>) -> Self {
        Self {
            agents: Some(agents),
            ..Self::ok()
        }
    }

    /// A `split` response, naming the pane it created.
    pub fn split(pane: String) -> Self {
        Self {
            pane: Some(pane),
            ..Self::ok()
        }
    }

    /// A `capture-pane` response.
    pub fn capture(cap: crate::term::Capture, pane: String) -> Self {
        Self {
            text: Some(cap.text),
            capture_rows: Some(cap.rows),
            truncated: Some(cap.truncated),
            pane: Some(pane),
            ..Self::ok()
        }
    }

    /// A `jump` response: optionally naming a terminal-emulator process to activate.
    pub fn jump(raise: Option<(u32, String)>, copad_host: Option<(String, String)>) -> Self {
        let (pid, comm) = match raise {
            Some((p, c)) => (Some(p), Some(c)),
            None => (None, None),
        };
        Self {
            raise_pid: pid,
            raise_comm: comm,
            copad_host,
            ..Self::ok()
        }
    }

    /// A `health` response.
    pub fn health(health: HealthInfo) -> Self {
        Self {
            health: Some(health),
            ..Self::ok()
        }
    }

    /// An `ok` response carrying a human-readable outcome message.
    pub fn message(msg: impl Into<String>) -> Self {
        Self {
            message: Some(msg.into()),
            ..Self::ok()
        }
    }

    /// A `worktree list` response.
    pub fn worktree_list(worktrees: Vec<WorktreeInfo>) -> Self {
        Self {
            worktrees: Some(worktrees),
            ..Self::ok()
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self::err_code("error", msg)
    }

    /// A failure with a machine-readable `code` (see [`Resp::code`]).
    pub fn err_code(code: &str, msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            code: Some(code.to_string()),
            ..Self::ok()
        }
    }

    /// A response naming what a create verb made: its session, tab, and pane token.
    pub fn created(session: NodeRef, tab: NodeRef, pane: String) -> Self {
        Self {
            session: Some(session),
            tab: Some(tab),
            pane: Some(pane),
            ..Self::ok()
        }
    }
    pub fn list(panes: Vec<PaneInfo>, focused: usize) -> Self {
        Self {
            panes: Some(panes),
            focused: Some(focused),
            ..Self::ok()
        }
    }
    pub fn tab_list(tabs: Vec<TabInfo>, active_tab: usize) -> Self {
        Self {
            tabs: Some(tabs),
            active_tab: Some(active_tab),
            ..Self::ok()
        }
    }
    pub fn session_list(sessions: Vec<SessionInfo>, active_session: usize) -> Self {
        Self {
            sessions: Some(sessions),
            active_session: Some(active_session),
            ..Self::ok()
        }
    }
}

/// The per-user private runtime directory holding the server socket + lock. Prefer
/// `$XDG_RUNTIME_DIR` (already 0700 on Linux), else `$TMPDIR` (per-user on macOS),
/// else `/tmp`; the `copad-mux-<user>` component is created 0700 by the server so
/// the socket is not world-reachable (the socket accepts input injection + takeover,
/// so it must be a private boundary — `$USER` alone is not one).
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("TMPDIR").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "/tmp".to_string());
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    PathBuf::from(base.trim_end_matches('/')).join(format!("copad-mux-{user}"))
}

/// The control/attach socket path: `$COPAD_MUX_SOCK` if set (caller-managed, e.g.
/// tests), else `<runtime_dir>/sock`.
pub fn socket_path() -> PathBuf {
    // A unit test must never reach the user's LIVE server. Ignoring an unset variable is not
    // enough: every comux pane EXPORTS `COPAD_MUX_SOCK`, so `cargo test` run from inside one
    // read the real socket and a parse test's `split -h` split the owner's tab (three stray
    // shells, 2026-10-23). Test builds read only their own variable, set by the tests that
    // want a specific dead socket, and otherwise a path nothing listens on.
    if cfg!(test) {
        return std::env::var("COPAD_MUX_TEST_SOCK").map_or_else(
            |_| std::env::temp_dir().join("copad-mux-unit-tests-no-listener.sock"),
            PathBuf::from,
        );
    }
    if let Ok(p) = std::env::var("COPAD_MUX_SOCK") {
        return PathBuf::from(p);
    }
    runtime_dir().join("sock")
}

/// The `comux ctl ...` CLI client: parse args, round-trip one request over the
/// socket, print the response. Returns a process exit code.
pub fn run_client(args: &[String]) -> i32 {
    // `apply-layout` composes many requests client-side (decisions #162).
    if args.first().map(|s| s.as_str()) == Some("apply-layout") {
        return apply_layout_client(&args[1..]);
    }
    // `worktree` has its own nested grammar (subcommands + `--from`/`--plain`/`-d`), so it
    // is parsed BEFORE the flat `--json`-stripping path below could reinterpret its flags.
    if args.first().map(|s| s.as_str()) == Some("worktree") {
        let json = args
            .iter()
            .take_while(|a| *a != "--")
            .any(|a| a == "--json");
        if json && wants_help(&args[1..]) {
            return json_help(WORKTREE_USAGE);
        }
        return json_usage(json, "worktree", run_worktree_client(&args[1..]));
    }
    // Fleet verbs are answered ENTIRELY client-side: they read the local config and run `ssh`,
    // and never touch this machine's server. Handled before a request is built so
    // `comux machines` works with no server running at all — which is exactly when you want to
    // ask where your agents are.
    if let Some(code) = fleet_verb(args) {
        return code;
    }

    let (json_out, owned) = cliargs::take_json(args);
    let rest: Vec<&String> = owned.iter().collect();
    let Some(cmd) = rest.first().map(|s| s.as_str()) else {
        if json_out {
            println!(
                "{}",
                serde_json::json!({ "ok": false, "code": "usage", "error": "comux: no command given" })
            );
        }
        eprintln!(
            "usage: comux <list|split|resize|focus|close|send|capture-pane|wait-output|list-agents|wait-agent|agent-log|\
             list-tabs|new-tab|spawn|select-tab|\
             close-tab|rename-tab [index] <name>|list-sessions|new-session [name]|\
             rename-session [index] <name>|select-session|kill-session|\
             worktree <create|list|rm>|reload|health|kill-server> [args]"
        );
        return 2;
    };
    // Every verb below parses through `verb_args`, which answers `--help` (exit 0) and
    // rejects unknown flags / extra arguments (exit 2) BEFORE a request is built — so a
    // malformed invocation can never reach the server and mutate something.
    macro_rules! parsed {
        ($spec:expr) => {
            match verb_args(&$spec, &rest, json_out) {
                Ok(p) => p,
                Err(code) => return code,
            }
        };
    }

    // `wait-output` owns its own exit codes (124 on deadline) and issues MANY requests,
    // so it short-circuits the one-request-one-response path below rather than producing a
    // `Req` for it.
    if cmd == "skill" {
        parsed!(cliargs::bare(
            "usage: comux skill   (prints the agent-facing SKILL.md)"
        ));
        // EMBEDDED, not read from disk: the skill has to be available from an installed
        // binary with no repo checkout, and it must describe THIS build's verbs rather than
        // whatever a stale file next to it says.
        if json_out {
            println!("{}", serde_json::json!({ "ok": true, "skill": SKILL_MD }));
        } else {
            println!("{SKILL_MD}");
        }
        return 0;
    }
    if json_out
        && (cmd == "wait-agent" || cmd == "wait-output" || cmd == "wait")
        && wants_help(&owned[1..])
    {
        return json_help(if cmd == "wait-agent" {
            WAIT_AGENT_USAGE
        } else {
            WAIT_USAGE
        });
    }
    if cmd == "wait-agent" {
        return match parse_wait_agent_args(&rest) {
            Ok(args) => run_wait_agent(args, json_out),
            Err(code) => json_usage(json_out, cmd, code),
        };
    }
    // Like the waits, `agent-log` is answered here: one request for the agent's log path, then
    // the file is read client-side (it is the caller's own user's file; the server never
    // touches it).
    // `inbox` reads, prints, THEN acknowledges, over one connection: see [`run_inbox`].
    if cmd == "inbox" {
        let p = parsed!(INBOX_SPEC);
        let Some(pane) = p.positionals.first().cloned().or_else(caller_pane) else {
            return usage_error(
                json_out,
                "comux inbox: whose? (no pane given and $COPAD_MUX_PANE is unset)",
                INBOX_SPEC.usage,
            );
        };
        let timeout = match p.value("--timeout").map(str::parse::<u64>).transpose() {
            Ok(t) => t.unwrap_or(300),
            Err(_) => {
                return usage_error(
                    json_out,
                    "comux inbox: --timeout needs a number",
                    INBOX_SPEC.usage,
                );
            }
        };
        if p.value("--timeout").is_some() && !p.has("--wait") {
            return usage_error(
                json_out,
                "comux inbox: --timeout goes with --wait",
                INBOX_SPEC.usage,
            );
        }
        // `--timeout 0` waits forever; anything the clock cannot represent is refused rather
        // than left to panic in `Instant + Duration`.
        let limit = (timeout > 0).then(|| Duration::from_secs(timeout));
        if limit.is_some_and(|l| Instant::now().checked_add(l).is_none()) {
            return usage_error(
                json_out,
                "comux inbox: --timeout is too large",
                INBOX_SPEC.usage,
            );
        }
        return run_inbox(&pane, p.has("--wait"), limit, p.has("--peek"), json_out);
    }
    if cmd == "agent-log" {
        let p = parsed!(AGENT_LOG_SPEC);
        let num = |flag: &str, default: usize| -> Result<usize, String> {
            p.value(flag).map_or(Ok(default), |v| {
                v.parse::<usize>()
                    .map_err(|_| format!("comux agent-log: {flag} needs a number, got '{v}'"))
            })
        };
        let (last, max_chars) = match (num("--last", 20), num("--max-chars", 400)) {
            (Ok(l), Ok(m)) => (l, m),
            (Err(e), _) | (_, Err(e)) => return usage_error(json_out, &e, AGENT_LOG_SPEC.usage),
        };
        let Some(pane) = p.positionals.first() else {
            return usage_error(
                json_out,
                "comux agent-log: which pane? (a token from `comux list-agents`)",
                AGENT_LOG_SPEC.usage,
            );
        };
        return agent_log(pane, last, max_chars).finish("agent-log", json_out);
    }
    if cmd == "wait-output" || cmd == "wait" {
        return match parse_wait_args(&rest) {
            Ok(args) => run_wait_output(args, json_out),
            Err(code) => json_usage(json_out, cmd, code),
        };
    }

    let req = match cmd {
        "list" => {
            let p = parsed!(LIST_SPEC);
            match index_caller(p.has("--active"), cmd, json_out) {
                Ok(caller) => Req::List { caller },
                Err(code) => return code,
            }
        }
        "health" => {
            parsed!(cliargs::bare("usage: comux health [--json]"));
            Req::Health
        }
        "host" => {
            parsed!(cliargs::bare("usage: comux host [--json]"));
            Req::Host
        }
        "reload" | "source-file" => {
            parsed!(cliargs::bare("usage: comux reload [--json]"));
            Req::ReloadConfig
        }
        "kill-server" => {
            parsed!(cliargs::bare("usage: comux kill-server"));
            Req::KillServer
        }
        "mark" => {
            let p = parsed!(MARK_SPEC);
            let Some(slot) = p
                .positionals
                .first()
                .and_then(|a| a.parse::<u8>().ok())
                .filter(|n| (1..=9).contains(n))
            else {
                return usage_error(
                    json_out,
                    "comux mark: a slot 1-9 is required",
                    MARK_SPEC.usage,
                );
            };
            let clear = p.has("--clear");
            // Never the focused pane: a pin aimed by focus lands wherever the user is looking.
            let target = match p.value("--pane").map(str::to_string).or_else(caller_pane) {
                Some(t) => Some(t),
                None if clear => None,
                None => {
                    return usage_error(
                        json_out,
                        "comux mark: run it inside a pane, or say which with --pane <token>",
                        MARK_SPEC.usage,
                    );
                }
            };
            if let Err(code) = require_cap(CAP_MARKS, cmd, json_out) {
                return code;
            }
            Req::Mark {
                slot,
                target: if clear { None } else { target },
                clear,
            }
        }
        "marks" => {
            parsed!(cliargs::bare(
                "usage: comux marks [--json]   (the pinned slots)"
            ));
            Req::Marks
        }
        "goto" | "goto-mark" => {
            let p = parsed!(GOTO_SPEC);
            let Some(slot) = p
                .positionals
                .first()
                .and_then(|a| a.parse::<u8>().ok())
                .filter(|n| (1..=9).contains(n))
            else {
                return usage_error(
                    json_out,
                    "comux goto: a slot 1-9 is required",
                    GOTO_SPEC.usage,
                );
            };
            Req::GotoMark { slot }
        }
        "fork" => {
            let p = parsed!(FORK_SPEC);
            let Some(target) = p.positionals.first().cloned().or_else(caller_pane) else {
                return usage_error(
                    json_out,
                    "comux fork: which agent pane? (a token; inside a pane it defaults to yours)",
                    FORK_SPEC.usage,
                );
            };
            if let Err(code) = require_cap(CAP_FORK, cmd, json_out) {
                return code;
            }
            Req::Fork {
                target,
                no_focus: p.has("--no-focus"),
            }
        }
        "zoom" => {
            parsed!(cliargs::bare(
                "usage: comux zoom    (toggle the focused pane filling its tab; Ctrl-b z)"
            ));
            Req::Zoom
        }
        "rename-pane" => {
            let p = parsed!(RENAME_PANE_SPEC);
            // `rename-pane <token> <name…>`, or just `<name…>` for the calling pane.
            let (target, words) = match p.positionals.first() {
                Some(first) if matches!(classify_pane_arg(first), Some(PaneArg::Token(_))) => {
                    (Some(first.clone()), &p.positionals[1..])
                }
                _ => (caller_pane(), &p.positionals[..]),
            };
            let Some(target) = target else {
                return usage_error(
                    json_out,
                    "comux rename-pane: which pane? (a token; inside a pane it defaults to yours)",
                    RENAME_PANE_SPEC.usage,
                );
            };
            if let Err(code) = require_cap(CAP_ARRANGE, cmd, json_out) {
                return code;
            }
            Req::RenamePane {
                target,
                name: words.join(" "),
            }
        }
        "select-layout" | "swap-pane" | "rotate-window" => {
            let (spec, op) = match cmd {
                "select-layout" => {
                    let p = parsed!(SELECT_LAYOUT_SPEC);
                    let op = if p.has("-E") {
                        Some("spread".to_string())
                    } else {
                        p.positionals.first().cloned()
                    };
                    let known = op.as_deref().is_some_and(|o| {
                        o == "next"
                            || o == "spread"
                            || crate::model::LayoutPreset::from_name(o).is_some()
                    });
                    if !known {
                        return usage_error(
                            json_out,
                            "comux select-layout: which layout?",
                            SELECT_LAYOUT_SPEC.usage,
                        );
                    }
                    (p, op.unwrap_or_default())
                }
                "swap-pane" => {
                    let p = parsed!(SWAP_PANE_SPEC);
                    let op = if p.has("-U") { "swap-up" } else { "swap-down" };
                    (p, op.to_string())
                }
                _ => {
                    let p = parsed!(ROTATE_SPEC);
                    let op = if p.has("-D") { "rotate-back" } else { "rotate" };
                    (p, op.to_string())
                }
            };
            if let Err(code) = require_cap(CAP_ARRANGE, cmd, json_out) {
                return code;
            }
            Req::Arrange {
                op,
                caller: spec
                    .value("--pane")
                    .map(str::to_string)
                    .or_else(caller_pane),
            }
        }
        "break-pane" => {
            let p = parsed!(BREAK_PANE_SPEC);
            if let Err(code) = require_cap(CAP_ARRANGE, cmd, json_out) {
                return code;
            }
            Req::BreakPane {
                caller: p.value("--pane").map(str::to_string).or_else(caller_pane),
            }
        }
        "move-tab" => {
            let p = parsed!(MOVE_TAB_SPEC);
            let Some(to) = p.positionals.first().and_then(|s| s.parse::<usize>().ok()) else {
                return usage_error(
                    json_out,
                    "comux move-tab: to which position?",
                    MOVE_TAB_SPEC.usage,
                );
            };
            if let Err(code) = require_cap(CAP_ARRANGE, cmd, json_out) {
                return code;
            }
            Req::MoveTab {
                to,
                caller: p.value("--pane").map(str::to_string).or_else(caller_pane),
            }
        }
        "back" => {
            parsed!(cliargs::bare(
                "usage: comux back    (go to where the view was before; Ctrl-b <)"
            ));
            Req::Nav { dir: NavDir::Back }
        }
        "forward" => {
            parsed!(cliargs::bare(
                "usage: comux forward (undo a `comux back`; Ctrl-b >)"
            ));
            Req::Nav {
                dir: NavDir::Forward,
            }
        }
        "last" => {
            parsed!(cliargs::bare(
                "usage: comux last    (toggle to the previous session/tab; Ctrl-b ;)"
            ));
            Req::Nav { dir: NavDir::Last }
        }
        "list-tabs" | "tabs" => {
            let p = parsed!(LIST_TABS_SPEC);
            let session = p.value("--session").map(str::to_string);
            if session.is_some()
                && let Err(code) = require_cap(CAP_SELECTORS, cmd, json_out)
            {
                return code;
            }
            Req::ListTabs { session }
        }
        "new-tab" => {
            let p = parsed!(NEW_TAB_SPEC);
            let cwd = match p.value("--cwd").map(absolute_dir).transpose() {
                Ok(c) => c,
                Err(e) => return usage_error(json_out, &e, NEW_TAB_SPEC.usage),
            };
            let owner = p.value("--owner").map(str::to_string);
            if owner.is_some()
                && let Err(code) = require_cap(CAP_OWNER, cmd, json_out)
            {
                return code;
            }
            let req = Req::NewTab {
                session: p.value("--session").map(str::to_string),
                from: caller_pane(),
                cwd,
                name: p.value("--name").map(str::to_string),
                no_focus: p.has("--no-focus"),
                command: Vec::new(),
                env: Vec::new(),
                keep: false,
                owner,
            };
            // An older server ignores every field, so it would create the tab in front of the
            // user — exactly what these flags exist to prevent.
            if !p.flags.is_empty()
                && let Err(code) = require_cap(CAP_PLACEMENT, cmd, json_out)
            {
                return code;
            }
            req
        }
        "spawn" => {
            let p = parsed!(SPAWN_SPEC);
            if p.positionals.is_empty() {
                return usage_error(json_out, "comux spawn: nothing to run", SPAWN_SPEC.usage);
            }
            let cwd = match p.value("--cwd").map(absolute_dir).transpose() {
                Ok(c) => c,
                Err(e) => return usage_error(json_out, &e, SPAWN_SPEC.usage),
            };
            let env: Vec<String> = p
                .flags
                .iter()
                .filter(|(f, _)| *f == "--env")
                .filter_map(|(_, v)| v.clone())
                .collect();
            if let Some(e) = env.iter().find_map(|kv| env_pair(kv).err()) {
                return usage_error(json_out, &format!("comux spawn: {e}"), SPAWN_SPEC.usage);
            }
            if let Some(e) = p.positionals.iter().find_map(|a| spawn_arg(a).err()) {
                return usage_error(json_out, &format!("comux spawn: {e}"), SPAWN_SPEC.usage);
            }
            // An older server ignores `command`, so it would answer with an ordinary shell
            // tab and `ok` — a command reported as started that never ran.
            if let Err(code) = require_cap(CAP_SPAWN, cmd, json_out) {
                return code;
            }
            let owner = p.value("--owner").map(str::to_string);
            if owner.is_some()
                && let Err(code) = require_cap(CAP_OWNER, cmd, json_out)
            {
                return code;
            }
            Req::NewTab {
                session: p.value("--session").map(str::to_string),
                from: caller_pane(),
                cwd,
                name: p.value("--name").map(str::to_string),
                // A spawned worker never moves anyone's view.
                no_focus: true,
                command: p.positionals.clone(),
                env,
                keep: p.has("--keep"),
                owner,
            }
        }
        "list-sessions" | "sessions" => {
            parsed!(cliargs::bare("usage: comux list-sessions [--json]"));
            Req::ListSessions
        }
        "list-tree" | "tree" => {
            let p = parsed!(LIST_TREE_SPEC);
            let owner = p.value("--owner").map(str::to_string);
            if owner.is_some()
                && let Err(code) = require_cap(CAP_OWNER, cmd, json_out)
            {
                return code;
            }
            Req::ListTree {
                session: p.value("--session").map(str::to_string),
                owner,
            }
        }
        "list-agents" | "agents" => {
            parsed!(cliargs::bare("usage: comux list-agents [--fleet] [--json]"));
            Req::ListAgents { target: None }
        }
        "new-session" => {
            let p = parsed!(NAME_SPEC_NEW_SESSION);
            // Optional name: every positional, space-joined (tmux `new -s`).
            let name = p.positionals.join(" ");
            let cwd = match p.value("--cwd").map(absolute_dir).transpose() {
                Ok(Some(c)) => Some(c),
                // Start the session's shell where the CLI was invoked (like `tmx $name`).
                Ok(None) => std::env::current_dir()
                    .ok()
                    .and_then(|p| p.to_str().map(|s| s.to_string())),
                Err(e) => return usage_error(json_out, &e, NAME_SPEC_NEW_SESSION.usage),
            };
            let detached = p.has("--detached");
            if detached {
                // A cold start spawns a server from THIS binary, which has the capability —
                // so start it first, or the check below would fail for want of a server.
                if let Err(e) = crate::client::ensure_running(&socket_path()) {
                    eprintln!("comux: could not start server: {e}");
                    return 1;
                }
                if let Err(code) = require_cap(CAP_DETACHED_SESSION, cmd, json_out) {
                    return code;
                }
            }
            Req::NewSession {
                name: (!name.trim().is_empty()).then(|| name.trim().to_string()),
                cwd,
                detached,
            }
        }
        "rename-session" | "rename" => {
            let p = parsed!(RENAME_SESSION_SPEC);
            let Some((index, name)) = rename_args(&p) else {
                return usage_error(
                    json_out,
                    "comux rename-session: a name is required",
                    RENAME_SESSION_SPEC.usage,
                );
            };
            let target = p.value("--target").map(str::to_string);
            let pane = match rename_target(
                index,
                target.as_deref(),
                cmd,
                json_out,
                RENAME_SESSION_SPEC.usage,
            ) {
                Ok(p) => p,
                Err(code) => return code,
            };
            Req::RenameSession {
                index,
                pane,
                target,
                name,
            }
        }
        "rename-tab" => {
            let p = parsed!(RENAME_TAB_SPEC);
            let Some((index, name)) = rename_args(&p) else {
                return usage_error(
                    json_out,
                    "comux rename-tab: a name is required",
                    RENAME_TAB_SPEC.usage,
                );
            };
            let target = p.value("--target").map(str::to_string);
            let pane = match rename_target(
                index,
                target.as_deref(),
                cmd,
                json_out,
                RENAME_TAB_SPEC.usage,
            ) {
                Ok(p) => p,
                Err(code) => return code,
            };
            Req::RenameTab {
                index,
                pane,
                target,
                name,
            }
        }
        // The index-taking verbs below share one shape: an omitted index opens the fuzzy
        // picker over a live listing instead of failing with a usage line (see
        // `pick_index`), while a MALFORMED index still fails — the user meant something.
        "select-session" => {
            let (index, target) = match pick_verb(&rest, json_out, Target::Session) {
                Ok(a) => a,
                Err(code) => return code,
            };
            Req::SelectSession { index, target }
        }
        "select-tab" => {
            let (index, target) = match pick_verb(&rest, json_out, Target::Tab) {
                Ok(a) => a,
                Err(code) => return code,
            };
            Req::SelectTab { index, target }
        }
        // The two destructive selection verbs. They picker on an omitted index like
        // their non-destructive siblings rather than defaulting to the ACTIVE tab /
        // session the way `rename-*` does: an explicit pick is what makes a bare
        // `comux close-tab` safe to type.
        "broadcast" => {
            let p = parsed!(BROADCAST_SPEC);
            let session = p.value("--session").map(str::to_string);
            let all = p.has("--all");
            if session.is_some() == all {
                return usage_error(
                    json_out,
                    "comux broadcast: give exactly one of --session <sel> or --all",
                    BROADCAST_SPEC.usage,
                );
            }
            let dry_run = p.has("--dry-run");
            if dry_run == p.has("--yes") {
                return usage_error(
                    json_out,
                    "comux broadcast: say --dry-run to look or --yes to send",
                    BROADCAST_SPEC.usage,
                );
            }
            let status = p.value("--status").map(str::to_string);
            if let Some(s) = &status
                && !matches!(s.as_str(), "working" | "ready" | "blocked" | "idle")
            {
                return usage_error(
                    json_out,
                    &format!("comux broadcast: unknown --status '{s}'"),
                    BROADCAST_SPEC.usage,
                );
            }
            let text = p.positionals.join(" ");
            if text.is_empty() && !p.has("--enter") {
                return usage_error(
                    json_out,
                    "comux broadcast: nothing to send",
                    BROADCAST_SPEC.usage,
                );
            }
            if let Err(code) = require_cap(CAP_BROADCAST, cmd, json_out) {
                return code;
            }
            Req::Broadcast {
                session,
                all,
                agents: p.has("--agents") || status.is_some(),
                status,
                text,
                paste: p.has("--paste"),
                enter: p.has("--enter"),
                dry_run,
                caller: caller_pane(),
            }
        }
        "close-tab" | "kill-tab" if owned_by_form(&rest) => {
            let p = parsed!(CLOSE_OWNED_SPEC);
            let Some(owner) = p.value("--owned-by").map(str::to_string) else {
                return usage_error(
                    json_out,
                    "comux close-tab: --owned-by needs a pane",
                    CLOSE_OWNED_SPEC.usage,
                );
            };
            let dry_run = p.has("--dry-run");
            if dry_run == p.has("--yes") {
                return usage_error(
                    json_out,
                    "comux close-tab --owned-by: pass --dry-run to list them or --yes to close them",
                    CLOSE_OWNED_SPEC.usage,
                );
            }
            if let Err(code) = require_cap(CAP_OWNER, cmd, json_out) {
                return code;
            }
            Req::CloseOwned {
                owner,
                caller: caller_pane(),
                dry_run,
            }
        }
        "close-tab" | "kill-tab" => {
            let (index, target) = match pick_verb(&rest, json_out, Target::TabClose) {
                Ok(a) => a,
                Err(code) => return code,
            };
            Req::CloseTab { index, target }
        }
        "kill-session" => {
            let (index, target) = match pick_verb(&rest, json_out, Target::SessionKill) {
                Ok(a) => a,
                Err(code) => return code,
            };
            Req::KillSession { index, target }
        }
        "split" => {
            // -h/--horizontal → side by side (right); -v/--vertical → stacked (down). The
            // verb owns `-h`, so only `--help` asks for help here.
            let p = parsed!(SPLIT_SPEC);
            let mut dir = "right";
            for (f, _) in &p.flags {
                match *f {
                    "-v" => dir = "down",
                    "-h" => dir = "right",
                    _ => {}
                }
            }
            // Before `--`: at most a direction. After it: the program to run.
            let split_at = p.literal_at.unwrap_or(p.positionals.len());
            let (head, tail) = p.positionals.split_at(split_at);
            let command = tail.to_vec();
            if let Some(extra) = head.get(1) {
                return usage_error(
                    json_out,
                    &format!(
                        "comux split: unexpected argument '{extra}' (put a program after `--`)"
                    ),
                    SPLIT_SPEC.usage,
                );
            }
            match head.first().map(|s| s.as_str()) {
                None => {}
                Some("down") => dir = "down",
                Some("right") => dir = "right",
                Some(other) => {
                    return usage_error(
                        json_out,
                        &format!("comux split: unexpected argument '{other}'"),
                        SPLIT_SPEC.usage,
                    );
                }
            }
            let no_focus = p.has("--no-focus");
            if no_focus && let Err(code) = require_cap(CAP_PLACEMENT, cmd, json_out) {
                return code;
            }
            let cwd = match p.value("--cwd").map(absolute_dir).transpose() {
                Ok(c) => c,
                Err(e) => return usage_error(json_out, &e, SPLIT_SPEC.usage),
            };
            let env: Vec<String> = p
                .flags
                .iter()
                .filter(|(f, _)| *f == "--env")
                .filter_map(|(_, v)| v.clone())
                .collect();
            if let Some(e) = env.iter().find_map(|kv| env_pair(kv).err()) {
                return usage_error(json_out, &format!("comux split: {e}"), SPLIT_SPEC.usage);
            }
            if let Some(e) = command.iter().find_map(|a| spawn_arg(a).err()) {
                return usage_error(json_out, &format!("comux split: {e}"), SPLIT_SPEC.usage);
            }
            let keep = p.has("--keep");
            if (cwd.is_some() || !env.is_empty() || keep || !command.is_empty())
                && let Err(code) = require_cap(CAP_SPLIT_SPAWN, cmd, json_out)
            {
                return code;
            }
            Req::Split {
                from: p.value("--from").map(str::to_string),
                dir: dir.to_string(),
                no_focus,
                cwd,
                command,
                env,
                keep,
            }
        }
        "focus" | "close" => {
            let target = if cmd == "focus" {
                Target::PaneFocus
            } else {
                Target::PaneClose
            };
            let spec = Spec {
                usage: target.usage(),
                flags: ACTIVE_FLAG,
                positionals: Positionals::AtMost(1),
            };
            let p = parsed!(spec);
            let caller = match index_caller(p.has("--active"), cmd, json_out) {
                Ok(c) => c,
                Err(code) => return code,
            };
            let index = match pick_index(
                p.positionals.first().map(|s| s.as_str()),
                json_out,
                target,
                caller.as_deref(),
            ) {
                Ok(i) => i,
                Err(code) => return code,
            };
            if cmd == "focus" {
                Req::Focus { index, caller }
            } else {
                Req::Close { index, caller }
            }
        }
        "capture-pane" | "capture" => {
            // Addressed by identity like `jump`/`notify` rather than by picker: this verb
            // exists for scripts and agents, and an interactive picker in the middle of a
            // pipeline is the wrong affordance. A bare `comux capture-pane` still works —
            // it reads the focused pane.
            let p = parsed!(CAPTURE_SPEC);
            let num = |flag: &str| -> Result<Option<usize>, i32> {
                match p.value(flag) {
                    None => Ok(None),
                    Some(v) => v.parse::<usize>().map(Some).map_err(|_| {
                        usage_error(
                            json_out,
                            &format!("comux capture-pane: {flag} needs a number"),
                            CAPTURE_USAGE,
                        )
                    }),
                }
            };
            let lines = match num("-S") {
                Ok(v) => v,
                Err(code) => return code,
            };
            let index = match num("--index") {
                Ok(v) => v,
                Err(code) => return code,
            };
            let target = p.positionals.first().cloned();
            if target.is_some() && index.is_some() {
                return usage_error(
                    json_out,
                    "comux capture-pane: give a target or --index, not both",
                    CAPTURE_USAGE,
                );
            }
            // Only an index is counted in a tab; a target names its pane anywhere.
            let caller = match index {
                Some(_) => match index_caller(p.has("--active"), cmd, json_out) {
                    Ok(c) => c,
                    Err(code) => return code,
                },
                None => None,
            };
            Req::CapturePane {
                target,
                index,
                lines,
                caller,
            }
        }
        "jump" => {
            // Addressed by identity, not position, so there is no index listing to fuzzy-pick
            // from when it is omitted (unlike `focus`/`select-tab`): `comux list --json`
            // prints each pane's token, and a pane's own shell carries `$COPAD_MUX_PANE`.
            let p = parsed!(JUMP_SPEC);
            let Some(target) = p.positionals.first() else {
                return usage_error(json_out, "comux jump: a pane is required", JUMP_SPEC.usage);
            };
            Req::Jump {
                target: target.clone(),
            }
        }
        "notify" => {
            let p = parsed!(NOTIFY_SPEC);
            if let Some(to) = p.value("--to") {
                let from = p.value("--pane").map(str::to_string).or_else(caller_pane);
                if to == "owner" && from.is_none() {
                    return usage_error(
                        json_out,
                        "comux notify --to owner: no sending pane ($COPAD_MUX_PANE is unset; pass --pane)",
                        NOTIFY_SPEC.usage,
                    );
                }
                let result = match p.value("--result").map(result_path).transpose() {
                    Ok(r) => r,
                    Err(e) => {
                        return usage_error(
                            json_out,
                            &format!("comux notify: {e}"),
                            NOTIFY_SPEC.usage,
                        );
                    }
                };
                if let Err(code) = require_cap(CAP_INBOX, cmd, json_out) {
                    return code;
                }
                Req::Deliver {
                    to: to.to_string(),
                    from,
                    kind: p.value("--kind").unwrap_or("done").to_string(),
                    body: p.positionals.join(" "),
                    result,
                }
            } else {
                if p.value("--result").is_some() {
                    return usage_error(
                        json_out,
                        "comux notify: --result goes with --to (a toast carries no file)",
                        NOTIFY_SPEC.usage,
                    );
                }
                let target = p
                    .value("--pane")
                    .map(str::to_string)
                    .unwrap_or_else(|| std::env::var("COPAD_MUX_PANE").unwrap_or_default());
                if target.is_empty() {
                    // Never fall back to the active pane: a notification attributed to the
                    // wrong pane sends its click somewhere misleading.
                    return usage_error(
                        json_out,
                        "comux: no pane given and $COPAD_MUX_PANE is unset (not inside a comux pane?)",
                        NOTIFY_SPEC.usage,
                    );
                }
                Req::Notify {
                    target,
                    kind: p.value("--kind").unwrap_or("done").to_string(),
                    body: p.positionals.join(" "),
                }
            }
        }
        "resize" => {
            let p = parsed!(RESIZE_SPEC);
            let idx = p.positionals.first().and_then(|s| s.parse::<usize>().ok());
            let dir = p.positionals.get(1);
            let (Some(idx), Some(dir)) = (idx, dir) else {
                return usage_error(
                    json_out,
                    "comux resize: an index and a direction are required",
                    RESIZE_SPEC.usage,
                );
            };
            let caller = match index_caller(p.has("--active"), cmd, json_out) {
                Ok(c) => c,
                Err(code) => return code,
            };
            Req::ResizePane {
                index: idx,
                dir: dir.clone(),
                caller,
            }
        }
        "send" | "send-keys" => {
            let p = parsed!(SEND_SPEC);
            let Some(first) = p.positionals.first() else {
                return usage_error(json_out, "comux send: a pane is required", SEND_USAGE);
            };
            let (target, index) = match classify_pane_arg(first) {
                Some(PaneArg::Index(i)) => (None, Some(i)),
                Some(PaneArg::Token(t)) => (Some(t), None),
                None => {
                    return usage_error(
                        json_out,
                        &format!("comux send: '{first}' is neither a pane index nor a pane token"),
                        SEND_USAGE,
                    );
                }
            };
            let text = match p.value("--file") {
                Some(_) if p.positionals.len() > 1 => {
                    return usage_error(
                        json_out,
                        "comux send: give --file or text, not both",
                        SEND_USAGE,
                    );
                }
                Some(path) => match read_send_file(path) {
                    Ok(t) => t,
                    Err(e) => {
                        return usage_error(json_out, &format!("comux send: {e}"), SEND_USAGE);
                    }
                },
                None => p.positionals[1..].join(" "),
            };
            let (paste, enter) = (p.has("--paste"), p.has("--enter"));
            if (paste || enter)
                && let Err(code) = require_cap(CAP_SEND_FLAGS, cmd, json_out)
            {
                return code;
            }
            let caller = match index {
                Some(_) => match index_caller(p.has("--active"), cmd, json_out) {
                    Ok(c) => c,
                    Err(code) => return code,
                },
                None => None,
            };
            Req::SendKeys {
                target,
                index,
                text,
                paste,
                enter,
                caller,
            }
        }
        other => {
            eprintln!("comux: unknown command '{other}'");
            return 2;
        }
    };

    // `new-session` starts the server if it isn't running yet (like tmux `new-session`),
    // so `cd dir; comux new-session name` works from a cold start.
    if matches!(req, Req::NewSession { .. })
        && let Err(e) = crate::client::ensure_running(&socket_path())
    {
        eprintln!("comux: could not start server: {e}");
        return 1;
    }

    let resp = match round_trip(&req) {
        Ok(r) => r,
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    serde_json::json!({ "ok": false, "code": "unavailable", "error": e })
                );
            }
            eprintln!("comux: {e}");
            return 1;
        }
    };

    // `kill-server` replies OK the instant the shutdown is *initiated*, but the server then
    // finishes its final save + removes the socket + exits over the next moment. Wait for it
    // to actually be gone so a following `comux` doesn't attach to the dying server (TUI
    // flashes + exits) or race its still-held flock.
    if matches!(req, Req::KillServer)
        && resp.ok
        && !wait_for_server_gone(&socket_path(), Duration::from_secs(5))
    {
        eprintln!(
            "comux: server still shutting down (socket present after 5s) — \
             wait a moment before restarting, or `pkill -x comux`"
        );
        return 1;
    }

    // A jump exists to put the pane in front of the user, which a state switch alone
    // does not do when the terminal is behind another window. The RAISE runs here, in
    // this short-lived process, rather than in the server: the daemon's environment was
    // scrubbed of the session vars a GUI command needs, and it is the wrong process for
    // macOS to attribute automation permission to. Opt out with `--no-raise`.
    if matches!(req, Req::Jump { .. }) && resp.ok && !args.iter().any(|a| a == "--no-raise") {
        maybe_raise(&resp);
    }

    if json_out {
        println!("{}", serde_json::to_string(&resp).unwrap_or_default());
    } else {
        print_human(&req, &resp);
    }
    // A `split` whose response carries no pane token is NOT a success a script can use: the
    // documented recipe is `pane=$(comux split --from …)`, and an empty capture with exit 0
    // reads as "it worked" right up until the token is used to address a pane. An older
    // server (before decision #107) answers exactly that way, and `--json` would otherwise
    // not even print the warning.
    if matches!(req, Req::Split { .. })
        && resp.ok
        && resp.pane.as_deref().unwrap_or_default().is_empty()
    {
        return 1;
    }
    if resp.ok {
        0
    } else if resp.code.as_deref() == Some("ambiguous") {
        // An ambiguous NAME is the caller's to fix, like any other malformed argument.
        2
    } else {
        1
    }
}

/// Parse a verb's arguments (`rest[0]` is the verb) against its grammar. `Err` is the exit
/// code to return: 0 after printing `--help`, 2 after a usage error.
fn verb_args(spec: &Spec, rest: &[&String], json: bool) -> Result<cliargs::Parsed, i32> {
    let verb = rest.first().map(|s| s.as_str()).unwrap_or("");
    let args: Vec<&str> = rest.iter().skip(1).map(|s| s.as_str()).collect();
    match cliargs::parse(spec, &args) {
        Ok(p) => Ok(p),
        Err(cliargs::Stop::Help) => {
            if json {
                println!("{}", serde_json::json!({ "ok": true, "usage": spec.usage }));
            } else {
                println!("{}", spec.usage);
            }
            Err(0)
        }
        Err(cliargs::Stop::Usage(msg)) => Err(usage_error(
            json,
            &format!("comux {verb}: {msg}"),
            spec.usage,
        )),
    }
}

/// Report a malformed invocation and return exit code 2. Under `--json` the error also goes
/// to stdout as `{"ok":false,"code":"usage",...}`, so a caller parsing stdout sees a JSON
/// answer for EVERY outcome rather than an empty string.
fn usage_error(json: bool, msg: &str, usage: &str) -> i32 {
    if json {
        println!(
            "{}",
            serde_json::json!({ "ok": false, "code": "usage", "error": msg })
        );
    }
    eprintln!("{msg}\n{usage}");
    2
}

/// `send --file`: the file (or stdin for `-`), bounded by reading at most one byte past the
/// cap — a size check before an unbounded read would not hold for a growing file or a pipe.
fn read_send_file(path: &str) -> Result<String, String> {
    use std::io::Read;
    let mut buf = Vec::new();
    let read = if path == "-" {
        std::io::stdin()
            .take(SEND_FILE_CAP + 1)
            .read_to_end(&mut buf)
    } else {
        std::fs::File::open(path)
            .map_err(|e| format!("cannot open {path}: {e}"))?
            .take(SEND_FILE_CAP + 1)
            .read_to_end(&mut buf)
    };
    read.map_err(|e| format!("cannot read {path}: {e}"))?;
    if buf.len() as u64 > SEND_FILE_CAP {
        return Err(format!(
            "{path} is larger than {} KiB",
            SEND_FILE_CAP / 1024
        ));
    }
    String::from_utf8(buf).map_err(|_| format!("{path} is not UTF-8 text"))
}

/// `-h`/`--help` before any `--`, for the verbs with their own parsers.
fn wants_help(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "-h" || a == "--help")
}

/// `--help` under `--json`: the usage as a JSON answer, exit 0.
fn json_help(usage: &str) -> i32 {
    println!("{}", serde_json::json!({ "ok": true, "usage": usage }));
    0
}

/// For a verb with its own parser that already printed its usage error to stderr: under
/// `--json`, also answer on stdout, so every usage error has the same JSON shape.
fn json_usage(json: bool, verb: &str, code: i32) -> i32 {
    if json && code == 2 {
        println!(
            "{}",
            serde_json::json!({
                "ok": false,
                "code": "usage",
                "error": format!("comux {verb}: invalid arguments (see stderr)"),
            })
        );
    }
    code
}

/// The calling pane's token (`$COPAD_MUX_PANE`), when the CLI runs inside a comux pane.
fn caller_pane() -> Option<String> {
    std::env::var("COPAD_MUX_PANE")
        .ok()
        .filter(|p| !p.is_empty())
}

/// The pane an index is counted from: the one this command runs in, unless `--active` asks
/// for the tab on screen. Outside a pane there is no caller and the active tab is used, as
/// before. An older server would silently count in the active tab instead — a different
/// pane than `list` showed — so the capability is checked first.
fn index_caller(active: bool, verb: &str, json: bool) -> Result<Option<String>, i32> {
    if active {
        return Ok(None);
    }
    let Some(token) = caller_pane() else {
        return Ok(None);
    };
    require_cap(CAP_CALLER_INDEX, verb, json)?;
    Ok(Some(token))
}

/// Split one `--env` value into `(KEY, VALUE)`. The key must be a portable variable name, and
/// comux's own `COPAD_MUX*` markers are refused: overriding them would make the pane lie about
/// which server and which pane it is.
pub fn env_pair(kv: &str) -> Result<(&str, &str), String> {
    let (k, v) = kv
        .split_once('=')
        .ok_or_else(|| format!("--env '{kv}' is not KEY=VALUE"))?;
    let portable = !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !portable {
        return Err(format!("--env: '{k}' is not a variable name"));
    }
    if k.starts_with("COPAD_MUX") {
        return Err(format!("--env: {k} is comux's own and cannot be set"));
    }
    if v.contains('\0') {
        return Err(format!("--env: {k}'s value contains a NUL byte"));
    }
    Ok((k, v))
}

/// One `spawn` argument: anything but a NUL, which no argv can carry.
pub fn spawn_arg(a: &str) -> Result<(), String> {
    if a.contains('\0') {
        return Err("an argument contains a NUL byte".to_string());
    }
    Ok(())
}

/// `dir` as an absolute path, resolved against the CLI's own cwd (the server's is
/// unrelated), and refused unless it is a directory.
fn absolute_dir(dir: &str) -> Result<String, String> {
    let p = std::env::current_dir()
        .map(|c| c.join(dir))
        .unwrap_or_else(|_| PathBuf::from(dir));
    if !p.is_dir() {
        return Err(format!("not a directory: {dir}"));
    }
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("not a UTF-8 path: {dir}"))
}

/// The caller's own pane for an index-less `rename-*`, so it renames the tab/session the
/// CALLER is in rather than the one the user happens to be looking at. `Ok(None)` when an
/// index was given. Refuses (exit 2) outside a comux pane — there is no "caller" to default
/// to — and (exit 1) against a server too old to honor the field, which would otherwise
/// ignore it and rename the user's active tab.
fn caller_pane_for(
    index: Option<usize>,
    verb: &str,
    json: bool,
    usage: &str,
) -> Result<Option<String>, i32> {
    if index.is_some() {
        return Ok(None);
    }
    let Some(pane) = caller_pane() else {
        return Err(usage_error(
            json,
            &format!(
                "comux {verb}: not inside a comux pane ($COPAD_MUX_PANE is unset) — give an index"
            ),
            usage,
        ));
    };
    require_cap(CAP_RENAME_CALLER, verb, json)?;
    Ok(Some(pane))
}

/// Refuse (exit 1) unless the running server reports `cap`. One extra `health` round trip,
/// paid only by the operations whose new meaning an older server would silently misread.
fn require_cap(cap: &str, verb: &str, json: bool) -> Result<(), i32> {
    let resp = match round_trip(&Req::Health) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("comux: {e}");
            return Err(1);
        }
    };
    let caps = resp.health.map(|h| h.caps).unwrap_or_default();
    if caps.iter().any(|c| c == cap) {
        return Ok(());
    }
    let msg = format!(
        "comux {verb}: the running server predates this operation ({cap}) — \
         `comux server restart` after upgrading"
    );
    if json {
        println!(
            "{}",
            serde_json::json!({ "ok": false, "code": "unsupported", "error": msg })
        );
    }
    eprintln!("{msg}");
    Err(1)
}

/// The argument of a picker-backed index verb: at most one positional, and when it is
/// absent the fuzzy picker opens (see [`pick_index`]).
///
/// For a session/tab verb, a non-numeric argument is a SELECTOR (`s1`, `name:hub`, `s1:t0`,
/// `hub/build`) and comes back as the target, after checking the server understands one.
/// Pane verbs take only an index, so for them a non-number stays a usage error.
fn pick_verb(
    rest: &[&String],
    json: bool,
    target: Target,
) -> Result<(Option<usize>, Option<String>), i32> {
    let spec = Spec {
        usage: target.usage(),
        flags: &[],
        positionals: Positionals::AtMost(1),
    };
    let p = verb_args(&spec, rest, json)?;
    let verb = rest.first().map(|s| s.as_str()).unwrap_or("");
    match p.positionals.first() {
        Some(a) if a.parse::<usize>().is_err() && target.takes_selector() => {
            require_cap(CAP_SELECTORS, verb, json)?;
            Ok((None, Some(a.clone())))
        }
        a => pick_index(a.map(|s| s.as_str()), json, target, None).map(|i| (Some(i), None)),
    }
}

/// `(index, name)` for a `rename-*`. With `--target` the target IS the address, so every
/// positional is the name — `rename-tab -t s1:t0 2026 plan` names the tab "2026 plan" rather
/// than reading `2026` as an index.
fn rename_args(p: &cliargs::Parsed) -> Option<(Option<usize>, String)> {
    if p.value("--target").is_some() {
        return (!p.positionals.is_empty())
            .then(|| (None, p.positionals.join(" ").trim().to_string()));
    }
    parse_rename(&p.positionals)
}

/// Where an index-less `rename-*` points: an explicit `--target` selector, else the caller's
/// pane. Returns the `pane` field to send.
fn rename_target(
    index: Option<usize>,
    target: Option<&str>,
    verb: &str,
    json: bool,
    usage: &str,
) -> Result<Option<String>, i32> {
    match (index, target) {
        (Some(_), Some(_)) => Err(usage_error(
            json,
            &format!("comux {verb}: give an index or --target, not both"),
            usage,
        )),
        (None, Some(_)) => require_cap(CAP_SELECTORS, verb, json).map(|_| None),
        _ => caller_pane_for(index, verb, json, usage),
    }
}

const LIST_TREE_SPEC: Spec = Spec {
    usage: "usage: comux list-tree [--session <s1|name:hub|index>] [--owner <pane-token>] [--json]",
    flags: &[opt(&["--session", "-s"]), opt(&["--owner"])],
    positionals: Positionals::AtMost(0),
};

const LIST_TABS_SPEC: Spec = Spec {
    usage: "usage: comux list-tabs [--session <s1|name:hub|index>] [--json]",
    flags: &[opt(&["--session", "-s"])],
    positionals: Positionals::AtMost(0),
};

const NAME_SPEC_NEW_SESSION: Spec = Spec {
    usage: "usage: comux new-session [name...] [--cwd DIR] [--detached] [--json]   (use `--` before a name starting with '-')\n\
            \x20      --cwd starts it (and every new tab of it) in DIR instead of here;\n\
            \x20      --detached (-d) creates it without switching anyone to it",
    flags: &[opt(&["--cwd", "-c"]), flag(&["--detached", "-d"])],
    positionals: Positionals::Any,
};

const RENAME_SESSION_SPEC: Spec = Spec {
    usage: "usage: comux rename-session [index | -t <s1|name:hub>] <name...>\n\
            \x20      no index = the session of the calling pane",
    flags: &[opt(&["--target", "-t"])],
    positionals: Positionals::Any,
};

const RENAME_TAB_SPEC: Spec = Spec {
    usage: "usage: comux rename-tab [index | -t <s1:t0|hub/build>] <name...>\n\
            \x20      no index = the tab of the calling pane",
    flags: &[opt(&["--target", "-t"])],
    positionals: Positionals::Any,
};

const SPLIT_SPEC: Spec = Spec {
    usage: "usage: comux split [-v|-h] [--from <pane-token>] [--no-focus] [--cwd DIR] [--env K=V]... [--keep] [--json] [-- <argv...>]\n\
            \x20      --from splits that pane wherever it is; --no-focus leaves focus alone;\n\
            \x20      `-- argv` runs that program in the new pane (closes with it unless --keep, which\n\
            \x20      reports its exit status and leaves a shell)",
    flags: &[
        flag(&["-v", "--vertical"]),
        flag(&["-h", "--horizontal"]),
        opt(&["--from"]),
        flag(&["--no-focus", "-d"]),
        opt(&["--cwd", "-c"]),
        opt(&["--env", "-e"]),
        flag(&["--keep", "-k"]),
    ],
    positionals: Positionals::Any,
};

const NEW_TAB_SPEC: Spec = Spec {
    usage: "usage: comux new-tab [--session <s1|name:hub>] [--cwd DIR] [--name N] [--no-focus] [--owner TOKEN] [--json]\n\
            \x20      --session / --no-focus create it without moving anyone's view;\n\
            \x20      the shell starts in --cwd, else the calling pane's directory",
    flags: &[
        opt(&["--session", "-s"]),
        opt(&["--cwd", "-c"]),
        opt(&["--name", "-n"]),
        flag(&["--no-focus", "-d"]),
        opt(&["--owner"]),
    ],
    positionals: Positionals::AtMost(0),
};

const AGENT_LOG_SPEC: Spec = Spec {
    usage: "usage: comux agent-log <pane-token> [--last N] [--max-chars C] [--json]\n\
            \x20      the last N entries (default 20) of that agent's own conversation log, each cut\n\
            \x20      to C chars (default 400; 0 = whole)",
    flags: &[opt(&["--last", "-n"]), opt(&["--max-chars"])],
    positionals: Positionals::AtMost(1),
};

const SPAWN_SPEC: Spec = Spec {
    usage: "usage: comux spawn [--session <s1|name:hub>] [--cwd DIR] [--name N] [--env K=V]... [--keep] [--owner TOKEN] [--json] -- <argv...>\n\
            \x20      runs argv in a new tab without moving anyone's view (in --session, else beside\n\
            \x20      the calling pane); the tab closes when it exits unless --keep, which\n\
            \x20      reports its exit status and leaves a shell",
    flags: &[
        opt(&["--session", "-s"]),
        opt(&["--cwd", "-c"]),
        opt(&["--name", "-n"]),
        opt(&["--env", "-e"]),
        flag(&["--keep", "-k"]),
        opt(&["--owner"]),
    ],
    positionals: Positionals::Any,
};

const CAPTURE_SPEC: Spec = Spec {
    usage: CAPTURE_USAGE,
    flags: &[
        opt(&["-S", "--lines"]),
        opt(&["--index"]),
        flag(&["--active"]),
    ],
    positionals: Positionals::AtMost(1),
};

const JUMP_SPEC: Spec = Spec {
    usage: "usage: comux jump <pane-token|terminal-id> [--no-raise]\n\
            hint:  a pane's own shell has it in $COPAD_MUX_PANE; \
            `comux list --json` prints the rest",
    flags: &[flag(&["--no-raise"])],
    positionals: Positionals::AtMost(1),
};

const NOTIFY_SPEC: Spec = Spec {
    usage: "usage: comux notify [--pane <token>] [--kind done|blocked] [--] <body...>\n\
            \x20      comux notify --to <pane-token|owner> [--kind done|blocked|failed] [--result FILE] [--] <body...>\n\
            \x20      --to leaves the message in that pane's inbox (`comux inbox`) instead of raising a toast;\n\
            \x20      `owner` is the pane that owns your tab",
    flags: &[
        opt(&["--pane"]),
        opt(&["--kind"]),
        opt(&["--to"]),
        opt(&["--result"]),
    ],
    positionals: Positionals::Any,
};

const RENAME_PANE_SPEC: Spec = Spec {
    usage: "usage: comux rename-pane [<pane-token>] <name...>   name a pane (yours by default);\n\
            \x20      the name beats the program on its title (Ctrl-b T); \"\" clears it",
    flags: &[],
    positionals: Positionals::Any,
};

const SELECT_LAYOUT_SPEC: Spec = Spec {
    usage: "usage: comux select-layout <layout> [--pane <token>]   re-arrange your tab (Ctrl-b M-1..5)\n\
            \x20      layouts: even-horizontal even-vertical main-horizontal main-vertical tiled\n\
            \x20               next (Ctrl-b Space)   -E / spread: even out the splits (Ctrl-b E)",
    flags: &[flag(&["-E"]), opt(&["--pane"])],
    positionals: Positionals::AtMost(1),
};

const SWAP_PANE_SPEC: Spec = Spec {
    usage: "usage: comux swap-pane -U|-D [--pane <token>]   trade places with the previous (-U) or\n\
            \x20      next (-D, default) pane in your tab (Ctrl-b { / })",
    flags: &[flag(&["-U"]), flag(&["-D"]), opt(&["--pane"])],
    positionals: Positionals::AtMost(0),
};

const ROTATE_SPEC: Spec = Spec {
    usage: "usage: comux rotate-window [-U|-D] [--pane <token>]   every pane in your tab moves one\n\
            \x20      slot (Ctrl-b C-o / M-o)",
    flags: &[flag(&["-U"]), flag(&["-D"]), opt(&["--pane"])],
    positionals: Positionals::AtMost(0),
};

const BREAK_PANE_SPEC: Spec = Spec {
    usage: "usage: comux break-pane [--pane <token>]   move your pane into a new tab of its own\n\
            \x20      (Ctrl-b B)",
    flags: &[opt(&["--pane"])],
    positionals: Positionals::AtMost(0),
};

const MOVE_TAB_SPEC: Spec = Spec {
    usage: "usage: comux move-tab <position> [--pane <token>]   move your tab to that position\n\
            \x20      (0 = first; Ctrl-b S-Left / S-Right move it one place)",
    flags: &[opt(&["--pane"])],
    positionals: Positionals::AtMost(1),
};

const FORK_SPEC: Spec = Spec {
    usage: "usage: comux fork [<pane-token>] [--no-focus]\n\
            \x20      open a new tab in that agent's session running a FORK of its conversation\n\
            \x20      (claude --resume <id> --fork-session / codex fork <id>). Both agents then share\n\
            \x20      the directory and can overwrite each other's edits.",
    flags: &[flag(&["--no-focus"])],
    positionals: Positionals::AtMost(1),
};

const MARK_SPEC: Spec = Spec {
    usage: "usage: comux mark <1-9> [--pane <token>]   pin your pane (or that one) to a slot\n\
            \x20      comux mark --clear <1-9>          empty the slot",
    flags: &[opt(&["--pane"]), flag(&["--clear"])],
    positionals: Positionals::AtMost(1),
};

const GOTO_SPEC: Spec = Spec {
    usage: "usage: comux goto <1-9>   go to the pane pinned in that slot (Ctrl-b ' <n>)",
    flags: &[],
    positionals: Positionals::AtMost(1),
};

const BROADCAST_SPEC: Spec = Spec {
    usage: "usage: comux broadcast (--session <sel> | --all) [--agents] [--status working|ready|blocked|idle]\n\
            \x20      [--paste] [--enter] (--dry-run | --yes) [--] <text...>\n\
            \x20      types the same input into every matching pane except your own; writes are queued\n\
            \x20      with no rollback (a retry types twice). --agents/--status match the CACHED sweep.\n\
            \x20      Without bracketed paste a newline in the text submits, --enter or not.",
    flags: &[
        opt(&["--session", "-t"]),
        flag(&["--all"]),
        flag(&["--agents"]),
        opt(&["--status"]),
        flag(&["--paste"]),
        flag(&["--enter"]),
        flag(&["--dry-run", "-n"]),
        flag(&["--yes", "-y"]),
    ],
    positionals: Positionals::Any,
};

const CLOSE_OWNED_SPEC: Spec = Spec {
    usage: "usage: comux close-tab --owned-by <pane-token> --dry-run|--yes [--json]\n\
            \x20      closes every tab that pane owns (new-tab/spawn --owner); never the caller's own",
    flags: &[
        opt(&["--owned-by"]),
        flag(&["--dry-run", "-n"]),
        flag(&["--yes", "-y"]),
    ],
    positionals: Positionals::AtMost(0),
};

const INBOX_SPEC: Spec = Spec {
    usage: "usage: comux inbox [<pane-token>] [--wait [--timeout S]] [--peek] [--json]\n\
            \x20      reads the messages `notify --to` left for a pane (default: yours) and acknowledges\n\
            \x20      them once printed; --peek leaves them. Exit 124 if --wait times out",
    flags: &[
        flag(&["--wait", "-w"]),
        opt(&["--timeout"]),
        flag(&["--peek"]),
    ],
    positionals: Positionals::AtMost(1),
};

/// `close-tab --owned-by …` is a different verb from `close-tab [index]` (no picker, no index).
fn owned_by_form(rest: &[&String]) -> bool {
    rest.iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a.as_str() == "--owned-by" || a.starts_with("--owned-by="))
}

/// A `--result` file as an absolute path the reader can open from anywhere. It must exist
/// now; keeping it until it is read is the sender's job.
fn result_path(path: &str) -> Result<String, String> {
    let p = std::env::current_dir()
        .map(|c| c.join(path))
        .unwrap_or_else(|_| PathBuf::from(path));
    if !p.exists() {
        return Err(format!("--result {path}: no such file"));
    }
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("--result {path}: not a UTF-8 path"))
}

/// `--active`: count an index in the tab on screen rather than the caller's.
const ACTIVE_FLAG: &[cliargs::Flag] = &[flag(&["--active"])];

const LIST_SPEC: Spec = Spec {
    usage: "usage: comux list [--active] [--json]\n\
            inside a pane it lists YOUR tab; --active lists the tab on screen",
    flags: &[flag(&["--active"])],
    positionals: Positionals::AtMost(0),
};

const RESIZE_SPEC: Spec = Spec {
    usage: "usage: comux resize <index> <left|right|up|down> [--active]",
    flags: &[flag(&["--active"])],
    positionals: Positionals::AtMost(2),
};

const SEND_SPEC: Spec = Spec {
    usage: SEND_USAGE,
    flags: &[
        flag(&["--enter"]),
        flag(&["--paste"]),
        opt(&["--file"]),
        flag(&["--active"]),
    ],
    positionals: Positionals::Any,
};

/// Activate the terminal window a `jump` response named, after re-checking the pid still
/// belongs to the same program.
///
/// The recheck is not ceremony: the pid was sampled by the server a moment ago, and pids
/// are recycled — without it, a terminal that exited between the response and the click
/// could hand its number to an unrelated process, which we would then bring to the front.
fn maybe_raise(resp: &Resp) {
    // Copad first, when the client is inside one. It is the only host that can be asked for
    // the exact TAB — a pid names the emulator, not one of its tabs — so activating the
    // application instead would land the user on whichever tab happened to be active.
    let copad = match resp.copad_host.as_ref() {
        Some((sock, panel)) => crate::copadlink::focus_panel(sock, panel),
        None => crate::copadlink::FocusOutcome {
            focused: false,
            raised: false,
        },
    };
    if !fall_back_after_copad(copad) {
        return;
    }
    let (Some(pid), Some(comm)) = (resp.raise_pid, resp.raise_comm.as_deref()) else {
        return;
    };
    let still_there = crate::procinfo::ProcTree::snapshot()
        .and_then(|t| t.parent_of(pid))
        .is_some_and(|(_, live)| live == comm);
    if still_there {
        crate::winfocus::raise(pid, &[]);
    }
}

/// Whether the generic application-level raise should still run after the copad attempt.
///
/// One line, extracted only so it can be TESTED: an end-to-end harness can watch comux dial
/// copad, but it cannot watch a window come forward, so the fall-through is exactly the piece
/// no e2e can pin. Treating a refusal as success — the plausible mistake — leaves the user
/// looking at whatever was already frontmost, with nothing anywhere reporting a problem.
fn fall_back_after_copad(copad: crate::copadlink::FocusOutcome) -> bool {
    // Focusing the tab is not the same as being LOOKED at. GTK under Wayland does the first and
    // cannot do the second, so a copad that switched the tab but could not raise itself still
    // needs the generic application raise — which on Linux is `hyprctl`/`wmctrl`, a path that
    // works precisely because it is not the GUI asking on its own behalf.
    !copad.focused || !copad.raised
}

/// The agent-facing operating guide, embedded so `comux skill` works from an installed
/// binary. Install it where your agent looks for skills, e.g.
/// `comux skill > ~/.claude/skills/comux/SKILL.md`.
const SKILL_MD: &str = include_str!("../SKILL.md");

/// How a positional pane argument was understood.
enum PaneArg {
    Index(usize),
    Token(String),
}

/// Classify `send`'s first positional as an index or a pane token.
///
/// A `usize` parse comes FIRST so every invocation that works today keeps working
/// identically, including the leading-`+` form Rust accepts. Only when that fails is the
/// argument considered a token, and a token must contain `-`: the mint is
/// `{pid:x}{secs:x}-{n}` ([`crate::term::next_pane_token`]), so a token always has one and a
/// plain index never does. A numeric string too large for `usize` is therefore REJECTED
/// rather than quietly falling through to token resolution and reporting "unknown pane".
fn classify_pane_arg(arg: &str) -> Option<PaneArg> {
    if let Ok(i) = arg.parse::<usize>() {
        return Some(PaneArg::Index(i));
    }
    let numeric_ish = arg
        .strip_prefix(['+', '-'])
        .unwrap_or(arg)
        .chars()
        .all(|c| c.is_ascii_digit());
    if numeric_ish || !arg.contains('-') {
        return None;
    }
    Some(PaneArg::Token(arg.to_string()))
}

const SEND_USAGE: &str = "usage: comux send <pane-token|index> [--enter] [--paste] [--file PATH|-] [--] [text...]\n\
     \x20      a TOKEN (from `comux list --json`, or a pane's own $COPAD_MUX_PANE) names one\n\
     \x20      pane anywhere in the mux; an INDEX is a position in the ACTIVE tab and\n\
     \x20      retargets when the user switches tabs.\n\
     \x20      --enter   press Enter after the text (without it, nothing is submitted)\n\
     \x20      --paste   send it as a PASTE: bracketed when the program enabled that, so a\n\
     \x20                multi-line prompt arrives as one input. A program that did NOT enable\n\
     \x20                it receives each line as a submitted line, as with any terminal paste\n\
     \x20      --file    the text is this file's contents (`-` = stdin; at most 1 MiB, UTF-8),\n\
     \x20                instead of the arguments\n\
     \x20      text starting with `-` goes after `--`";

/// Exit code for "the user cancelled the picker" — fzf's (and SIGINT's) convention, so a
/// shell wrapper can tell a deliberate abort apart from a real failure.
/// Usage for `capture-pane`. A const because three argument-error paths print it.
const CAPTURE_USAGE: &str = "usage: comux capture-pane [<pane-token|terminal-id>] [--index N] [-S|--lines N] [--json]\n\
     \x20      no target = the focused pane; -S counts PHYSICAL grid rows back from the\n\
     \x20      live screen bottom (not the scrolled view), reaching into scrollback";

const EXIT_CANCELLED: i32 = 130;

/// Which live listing an omitted argument should be fuzzy-picked from.
#[derive(Clone, Copy)]
enum Target {
    Session,
    SessionKill,
    Tab,
    TabClose,
    PaneFocus,
    PaneClose,
}

impl Target {
    /// The usage line printed when we can't prompt (`--json`, non-terminal stderr) or
    /// when an index WAS given but isn't a number.
    fn usage(self) -> &'static str {
        match self {
            Target::Session => "comux select-session [index|s1|name:hub]   (none → fuzzy picker)",
            Target::SessionKill => "comux kill-session [index|s1|name:hub]   (none → fuzzy picker)",
            Target::Tab => "comux select-tab [index|s1:t0|hub/build]   (none → fuzzy picker)",
            Target::TabClose => "comux close-tab [index|s1:t0|hub/build]   (none → fuzzy picker)",
            Target::PaneFocus => "comux focus [index] [--active]   (no index → fuzzy picker)",
            Target::PaneClose => "comux close [index] [--active]   (no index → fuzzy picker)",
        }
    }

    /// Session/tab verbs also accept a selector; pane verbs take only an index.
    fn takes_selector(self) -> bool {
        !matches!(self, Target::PaneFocus | Target::PaneClose)
    }

    fn title(self) -> &'static str {
        match self {
            Target::Session => "switch to session",
            Target::SessionKill => "kill which session?",
            Target::Tab => "switch to tab",
            Target::TabClose => "close which tab?",
            Target::PaneFocus => "focus a pane",
            Target::PaneClose => "close a pane",
        }
    }

    /// Message for "the listing came back empty" — a picker with nothing in it is a
    /// failure, not a cancellation.
    fn empty(self) -> &'static str {
        match self {
            Target::Session | Target::SessionKill => "no sessions to pick from",
            Target::Tab | Target::TabClose => "no tabs to pick from",
            Target::PaneFocus | Target::PaneClose => "no panes to pick from",
        }
    }

    /// Fetch the listing and render it as picker rows paired with the index each row
    /// resolves to (paired so the two can never drift apart while filtering).
    ///
    /// The destructive variants share their sibling's listing and deliberately offer
    /// EVERY row rather than pre-filtering the ones the server would refuse (a session's
    /// last tab, the last session) — unlike `pick_worktree`, where hiding still leaves
    /// candidates. Here the refusable row is typically the only one, so hiding it would
    /// report an empty picker in place of the server's error, which names the reason.
    fn rows(self, caller: Option<&str>) -> Result<Vec<(picker::Item, usize)>, i32> {
        match self {
            Target::Session | Target::SessionKill => {
                let sessions = query(&Req::ListSessions)?.sessions.unwrap_or_default();
                Ok(sessions
                    .iter()
                    .map(|s| {
                        let name = if s.name.is_empty() {
                            s.id.as_str()
                        } else {
                            s.name.as_str()
                        };
                        let mut detail = format!("{} tabs · {} panes", s.tabs, s.panes);
                        if s.agents > 0 {
                            detail.push_str(&format!(" · {} agents", s.agents));
                        }
                        if s.active {
                            detail.push_str(" · active");
                        }
                        (
                            picker::Item::new(format!("{}: {name}", s.index), detail),
                            s.index,
                        )
                    })
                    .collect())
            }
            Target::Tab | Target::TabClose => {
                let tabs = query(&Req::ListTabs { session: None })?
                    .tabs
                    .unwrap_or_default();
                Ok(tabs
                    .iter()
                    .map(|t| {
                        let name = if t.name.is_empty() {
                            t.id.as_str()
                        } else {
                            t.name.as_str()
                        };
                        let mut detail = format!("{} panes", t.panes);
                        if t.agents > 0 {
                            detail.push_str(&format!(" · {} agents", t.agents));
                        }
                        if t.active {
                            detail.push_str(" · active");
                        }
                        (
                            picker::Item::new(format!("{}: {name}", t.index), detail),
                            t.index,
                        )
                    })
                    .collect())
            }
            Target::PaneFocus | Target::PaneClose => {
                let panes = query(&Req::List {
                    caller: caller.map(str::to_string),
                })?
                .panes
                .unwrap_or_default();
                Ok(panes
                    .iter()
                    .map(|p| {
                        let label = if p.label.is_empty() {
                            p.id.as_str()
                        } else {
                            p.label.as_str()
                        };
                        let mut detail = format!("{}x{}", p.cols, p.rows);
                        if !p.status.is_empty() {
                            detail = format!("{} · {detail}", p.status);
                        }
                        if p.focused {
                            detail.push_str(" · focused");
                        }
                        (
                            picker::Item::new(format!("{}: {label}", p.index), detail),
                            p.index,
                        )
                    })
                    .collect())
            }
        }
    }
}

/// Resolve an index argument: parse it when one was given, otherwise fuzzy-pick from
/// the server's live listing. `Err` carries the process exit code (2 usage · 1 failure ·
/// [`EXIT_CANCELLED`] when the user aborted the picker).
/// `caller` scopes a pane listing to the caller's tab, as the index it returns will be.
fn pick_index(
    arg: Option<&str>,
    json: bool,
    target: Target,
    caller: Option<&str>,
) -> Result<usize, i32> {
    if let Some(a) = arg {
        return a.parse::<usize>().map_err(|_| {
            usage_error(
                json,
                &format!("comux: '{a}' is not an index"),
                &format!("usage: {}", target.usage()),
            )
        });
    }
    prompt_ok(json, target.usage())?;
    let rows = target.rows(caller)?;
    choose(target.title(), target.empty(), rows)
}

/// The gate every "argument omitted → picker" path shares: only prompt on a real
/// terminal and outside `--json`. A script or a pipe keeps the old usage error, so it
/// fails fast instead of blocking on a prompt nobody can answer.
fn prompt_ok(json: bool, usage: &str) -> Result<(), i32> {
    if json || !picker::interactive() {
        eprintln!("usage: {usage}");
        return Err(2);
    }
    Ok(())
}

/// Run the picker over `rows` and return the chosen row's value.
fn choose<T>(title: &str, empty: &str, rows: Vec<(picker::Item, T)>) -> Result<T, i32> {
    if rows.is_empty() {
        eprintln!("comux: {empty}");
        return Err(1);
    }
    let (items, values): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
    match picker::pick(title, &items) {
        Ok(Some(i)) => values.into_iter().nth(i).ok_or(1),
        Ok(None) => Err(EXIT_CANCELLED),
        Err(e) => {
            eprintln!("comux: {e}");
            Err(1)
        }
    }
}

/// One round trip for a picker's listing, mapping a transport error or a server refusal
/// to an exit code (the picker can't run without the listing).
fn query(req: &Req) -> Result<Resp, i32> {
    match round_trip(req) {
        Ok(r) if r.ok => Ok(r),
        Ok(r) => {
            eprintln!("error: {}", r.error.as_deref().unwrap_or("(unspecified)"));
            Err(1)
        }
        Err(e) => {
            eprintln!("comux: {e}");
            Err(1)
        }
    }
}

/// `comux server <start|stop|restart|status>` — manage the persistent server's lifecycle
/// so users don't have to hand-roll `kill-server` + a re-attach. `restart` leans on session
/// persistence: the
/// server saves its layout on shutdown and the fresh one restores it, so a restart brings
/// the workspace back (whitelisted agents even resume). Returns a process exit code.
pub fn run_server_admin(action: &str) -> i32 {
    let sock = socket_path();
    match action {
        // `status` is a point-in-time query — a connect probe is exactly right (there's no
        // action whose correctness a later state change could invalidate).
        "status" => {
            if UnixStream::connect(&sock).is_ok() {
                println!("comux server: running ({})", sock.display());
                0
            } else {
                println!("comux server: not running");
                1
            }
        }
        // `start`/`stop` probe ONLY to pick the human message; the end state is guaranteed by
        // `ensure_running`/`ensure_server_stopped`, both idempotent, so a server exiting or
        // appearing between the probe and the action can at worst mislabel — never misact.
        "start" => {
            let already = UnixStream::connect(&sock).is_ok();
            match crate::client::ensure_running(&sock) {
                Ok(()) => {
                    println!(
                        "comux server: {}",
                        if already {
                            "already running"
                        } else {
                            "started"
                        }
                    );
                    0
                }
                Err(e) => {
                    eprintln!("comux: could not start server: {e}");
                    1
                }
            }
        }
        "stop" => {
            let was_running = UnixStream::connect(&sock).is_ok();
            match ensure_server_stopped(&sock) {
                Ok(()) => {
                    println!(
                        "comux server: {}",
                        if was_running {
                            "stopped"
                        } else {
                            "not running"
                        }
                    );
                    0
                }
                Err(code) => code,
            }
        }
        "restart" => {
            // Idempotent stop then start — works whether or not a server was running, and a
            // concurrent exit during the stop is treated as already-stopped, not a failure.
            if let Err(code) = ensure_server_stopped(&sock) {
                return code;
            }
            // `ensure_running` re-spawns on a backoff, which is exactly what handles the
            // flock hand-off from the just-stopped server (see `connect_or_spawn`).
            match crate::client::ensure_running(&sock) {
                Ok(()) => {
                    println!(
                        "comux server: restarted (workspace restored — run `comux` to reattach)"
                    );
                    0
                }
                Err(e) => {
                    eprintln!("comux: could not start server: {e}");
                    1
                }
            }
        }
        other => {
            eprintln!("comux: unknown server command '{other}' (start|stop|restart|status)");
            2
        }
    }
}

/// Ensure no server is listening on `sock`: if one is up, send `KillServer` and block until
/// it has fully exited (final save done, socket removed, flock released) so a following
/// start/restart can't race the dying server. Idempotent — a connect refusal (nothing there,
/// including one that vanished mid-request) is success, since the desired end state (stopped)
/// already holds. `Err(code)` only on a real request/protocol failure or a stuck shutdown.
fn ensure_server_stopped(sock: &Path) -> Result<(), i32> {
    if UnixStream::connect(sock).is_err() {
        return Ok(()); // nothing listening — already stopped
    }
    match round_trip(&Req::KillServer) {
        Ok(r) if r.ok => {}
        Ok(r) => {
            eprintln!(
                "comux: {}",
                r.error.as_deref().unwrap_or("kill-server failed")
            );
            return Err(1);
        }
        // Raced: the server exited between our probe and this request. A fresh connect that
        // also refuses confirms it's gone → treat as already-stopped rather than an error.
        Err(_) if UnixStream::connect(sock).is_err() => return Ok(()),
        Err(e) => {
            eprintln!("comux: {e}");
            return Err(1);
        }
    }
    if !wait_for_server_gone(sock, Duration::from_secs(5)) {
        eprintln!(
            "comux: server still shutting down (socket present after 5s) — \
             wait a moment before restarting, or `pkill -x comux`"
        );
        return Err(1);
    }
    Ok(())
}

/// Block until the server at `path` is fully gone (its socket removed → it's about to exit
/// and release its flock), up to `timeout`. Returns `true` once gone (after a small
/// flock-release grace), or `false` if the socket is STILL present at the deadline (the
/// caller should then report failure rather than let a restart race the lingering server).
fn wait_for_server_gone(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while path.exists() && start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(20));
    }
    if path.exists() {
        return false; // still shutting down at the deadline
    }
    // The server removes the socket immediately before `process::exit`; give the flock a
    // beat to release so the next server's `acquire_lock` succeeds.
    std::thread::sleep(Duration::from_millis(60));
    true
}

pub(crate) fn round_trip(req: &Req) -> Result<Resp, String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        format!(
            "no running comux at {} ({e}). Start one, or set COPAD_MUX_SOCK.",
            path.display()
        )
    })?;
    let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().ok();
    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader
        .read_line(&mut resp_line)
        .map_err(|e| e.to_string())?;
    if resp_line.trim().is_empty() {
        return Err("empty response from comux".to_string());
    }
    serde_json::from_str(resp_line.trim()).map_err(|e| format!("bad response: {e}"))
}

fn print_human(req: &Req, resp: &Resp) {
    if !resp.ok {
        eprintln!(
            "error: {}",
            resp.error.as_deref().unwrap_or("(unspecified)")
        );
        return;
    }
    match req {
        Req::Health => {
            let Some(h) = resp.health.as_ref() else {
                return;
            };
            println!("panes           {}", h.panes);
            println!("labeled         {}", h.labeled);
            println!("sweeps failed   {}", h.label_sweeps_failed);
            if let Some(soft) = h.fd_soft {
                match h.fd_open {
                    Some(open) => println!("fds             {open}/{soft}"),
                    None => println!("fds             ?/{soft}"),
                }
            }
            // The number that actually answers "why won't a new tab open?".
            if let Some(room) = h.panes_remaining() {
                println!("panes headroom  {room}");
            }
        }
        Req::Host => {
            let Some(h) = resp.host.as_ref() else {
                return;
            };
            // A field that could not be read prints nothing at all rather than a dash or a
            // zero, so `comux host | grep cpu` is empty exactly when there is no reading.
            let pct = |label: &str, v: Option<f64>| {
                if let Some(v) = v {
                    println!("{label:<7} {v:.0}%");
                }
            };
            pct("cpu", h.cpu);
            if let (Some(used), Some(total)) = (h.mem_used, h.mem_total) {
                let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
                let share = if total > 0 {
                    used as f64 * 100.0 / total as f64
                } else {
                    0.0
                };
                println!(
                    "mem     {:.0}%  ({:.1}/{:.1} GiB)",
                    share,
                    gib(used),
                    gib(total)
                );
            }
            pct("gpu", h.gpu);
            if let Some(l) = h.load1 {
                println!("load1   {l:.2}");
            }
        }
        Req::Split { .. } => {
            // Print the created pane's token in plain mode too, not only under `--json`:
            // it is the whole point of the response, and a caller that cannot see it falls
            // back to guessing which pane appeared.
            match resp.pane.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => println!("{t}"),
                // An older server answers `split` without a token. Say so rather than
                // printing nothing and letting a script read an empty string as success.
                None => eprintln!(
                    "comux split: this server did not report the new pane's token \
                     (older build — `comux server restart` after upgrading)"
                ),
            }
        }
        // `spawn` prints its pane like `split` does: the token is what the caller came for.
        Req::NewTab { command, .. } if !command.is_empty() => {
            println!("{}", resp.pane.as_deref().unwrap_or_default());
        }
        Req::ListTree { .. } => {
            for s in resp.tree.as_deref().unwrap_or_default() {
                let name = if s.name.is_empty() { "-" } else { &s.name };
                let mark = if s.active { "  *active" } else { "" };
                println!("{} {}{mark}", s.id, name);
                for t in &s.tabs {
                    let name = if t.name.is_empty() { "-" } else { &t.name };
                    let mark = if t.active { "  *active" } else { "" };
                    let owner = t
                        .owner
                        .as_deref()
                        .map(|o| format!("  owner {o}"))
                        .unwrap_or_default();
                    println!("  {} {}{mark}{owner}", t.id, name);
                    for p in &t.panes {
                        println!(
                            "    {:<2} {:<18} {:<8} {:<14} {:<8}{}",
                            p.index,
                            p.token,
                            p.kind,
                            p.label,
                            p.status,
                            if p.focused { " *focused" } else { "" }
                        );
                    }
                }
            }
        }
        Req::ListAgents { .. } => {
            let agents = resp.agents.clone().unwrap_or_default();
            if agents.is_empty() {
                println!("(no agent panes)");
                return;
            }
            println!(
                "{:<12} {:<12} {:<10} {:<9} {:<6} {:<18} DOING",
                "SPACE", "TAB", "TOOL", "STATUS", "FOR", "PANE"
            );
            for a in &agents {
                // `DOING` goes LAST because it is free text containing spaces; every column
                // before it stays splittable by whitespace.
                println!(
                    "{:<12} {:<12} {:<10} {:<9} {:<6} {:<18} {}",
                    a.space,
                    a.title,
                    a.tool,
                    a.status,
                    format!("{}s", a.for_secs),
                    a.token,
                    a.detail.as_deref().unwrap_or("")
                );
            }
        }
        Req::CapturePane { .. } => {
            // Text to STDOUT, notices to STDERR: `comux capture-pane | grep` must stay
            // machine-readable even when the capture was cut short.
            if let Some(t) = resp.text.as_deref() {
                println!("{t}");
            }
            if resp.truncated == Some(true) {
                eprintln!(
                    "comux capture-pane: truncated — older output omitted ({} rows returned)",
                    resp.capture_rows.unwrap_or(0)
                );
            }
        }
        Req::List { .. } => {
            let panes = resp.panes.clone().unwrap_or_default();
            let focused = resp.focused.unwrap_or(usize::MAX);
            println!(
                "{:<3} {:<8} {:<9} {:<8} {:<14} {:<9} SIZE",
                "IDX", "PANE", "FOCUS", "KIND", "LABEL", "STATUS"
            );
            for p in &panes {
                println!(
                    "{:<3} {:<8} {:<9} {:<8} {:<14} {:<9} {}x{}",
                    p.index,
                    p.id,
                    if p.index == focused { "*focused" } else { "" },
                    p.kind,
                    p.label,
                    p.status,
                    p.cols,
                    p.rows,
                );
            }
        }
        Req::ListTabs { .. } => {
            let tabs = resp.tabs.clone().unwrap_or_default();
            let active = resp.active_tab.unwrap_or(usize::MAX);
            println!(
                "{:<3} {:<9} {:<16} {:<16} {:<6} AGENTS",
                "IDX", "ACTIVE", "TAB", "NAME", "PANES"
            );
            for t in &tabs {
                println!(
                    "{:<3} {:<9} {:<16} {:<16} {:<6} {}",
                    t.index,
                    if t.index == active { "*active" } else { "" },
                    t.id,
                    if t.name.is_empty() { "-" } else { &t.name },
                    t.panes,
                    t.agents,
                );
            }
        }
        Req::ListSessions => {
            let sessions = resp.sessions.clone().unwrap_or_default();
            let active = resp.active_session.unwrap_or(usize::MAX);
            println!(
                "{:<3} {:<9} {:<16} {:<16} {:<5} {:<6} AGENTS",
                "IDX", "ACTIVE", "SESSION", "NAME", "TABS", "PANES"
            );
            for s in &sessions {
                println!(
                    "{:<3} {:<9} {:<16} {:<16} {:<5} {:<6} {}",
                    s.index,
                    if s.index == active { "*active" } else { "" },
                    s.id,
                    if s.name.is_empty() { "-" } else { &s.name },
                    s.tabs,
                    s.panes,
                    s.agents,
                );
            }
        }
        Req::Marks => {
            for m in resp.marks.as_deref().unwrap_or_default() {
                println!(
                    "{}  {}  {}/{}  {}",
                    m.slot, m.pane, m.session, m.tab, m.label
                );
            }
        }
        Req::Broadcast { .. } => {
            println!("{}", resp.message.as_deref().unwrap_or("ok"));
            for t in resp.targets.as_deref().unwrap_or_default() {
                println!(
                    "  {}  {}/{}  {}{}",
                    t.pane,
                    t.session,
                    t.tab,
                    t.label,
                    t.status
                        .as_deref()
                        .map(|s| format!(" ({s})"))
                        .unwrap_or_default()
                );
            }
        }
        Req::CloseOwned { .. } => {
            println!("{}", resp.message.as_deref().unwrap_or("ok"));
            for t in resp.closed.as_deref().unwrap_or_default() {
                println!(
                    "  {} {}",
                    t.id,
                    if t.name.is_empty() { "-" } else { &t.name }
                );
            }
        }
        // Message-carrying verbs (e.g. `reload`) print their outcome; everything else
        // just confirms with `ok`.
        _ => match &resp.message {
            Some(m) => println!("{m}"),
            None => println!("ok"),
        },
    }
}

/// `rename-*` CLI argument shape, shared by tabs and sessions: `rename-x <name...>`
/// targets the ACTIVE one; `rename-x <index> <name...>` targets by list index. `args` are
/// the verb's positionals. A leading integer is read as an index only when a name
/// follows it, so `rename-tab 2` names the active tab "2" rather than erroring on a
/// missing name. An explicit empty name (`rename-tab ""`) clears back to the default.
fn parse_rename(args: &[String]) -> Option<(Option<usize>, String)> {
    let (index, name_args) = match args.first().and_then(|s| s.parse::<usize>().ok()) {
        Some(idx) if args.len() > 1 => (Some(idx), &args[1..]),
        _ => (None, args),
    };
    if name_args.is_empty() {
        return None;
    }
    let name = name_args
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Some((index, name.trim().to_string()))
}

/// The caller's cwd as a wire string (the repo is resolved from it server-side).
fn caller_cwd() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
}

/// Is a comux server currently accepting on the control socket?
fn server_running() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}

fn print_worktree_usage() {
    eprintln!("{WORKTREE_USAGE}");
}

const WORKTREE_USAGE: &str = "usage:\n\
         \x20 comux worktree create <branch> [--from <ref>] [--no-attach] [--json]\n\
         \x20 comux worktree list [--plain|--json]\n\
         \x20 comux worktree rm [<path|branch>] [-f|--force] [-d|--delete-branch] [--json]\n\
         \x20 comux worktree land <path|branch> [--merge] [--keep] [--dry-run] [--json]\n\
         \x20                     bring its branch into the main worktree's branch (rebase + fast-forward,\n\
         \x20                     or a merge commit with --merge), then remove it and the landed branch\n\
         \n\
         omitting the `rm` target opens a fuzzy picker over the repo's worktrees.";

/// `comux worktree <sub> …` — a nested grammar parsed independently of the flat client.
fn run_worktree_client(args: &[String]) -> i32 {
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        print_worktree_usage();
        return 2;
    };
    let rest = &args[1..];
    // `--help` anywhere before a `--` answers with the usage and touches nothing — the
    // subcommand parsers below would otherwise reject it as an unknown flag (exit 2).
    if rest
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "-h" || a == "--help")
    {
        println!("{WORKTREE_USAGE}");
        return 0;
    }
    match sub {
        "create" | "new" | "add" => worktree_create_client(rest),
        "list" | "ls" => worktree_list_client(rest),
        "rm" | "remove" => worktree_rm_client(rest),
        "land" => worktree_land_client(rest),
        "help" | "-h" | "--help" => {
            println!("{WORKTREE_USAGE}");
            0
        }
        other => {
            eprintln!("comux worktree: unknown subcommand '{other}'");
            print_worktree_usage();
            2
        }
    }
}

/// Print a mutating-verb response (`--json` → raw Resp; else the message / error).
fn print_worktree_result(resp: &Resp, json: bool) -> i32 {
    if json {
        println!("{}", serde_json::to_string(resp).unwrap_or_default());
    } else if resp.ok {
        if let Some(m) = &resp.message {
            println!("{m}");
        } else {
            println!("ok");
        }
    } else {
        eprintln!(
            "error: {}",
            resp.error.as_deref().unwrap_or("(unspecified)")
        );
    }
    if resp.ok { 0 } else { 1 }
}

fn worktree_create_client(rest: &[String]) -> i32 {
    let mut branch: Option<&str> = None;
    let mut from = String::new();
    let mut json = false;
    let mut no_attach = false;
    let mut flags_done = false;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if !flags_done && a == "--" {
            flags_done = true;
        } else if !flags_done && a.starts_with('-') {
            match a {
                "--from" => {
                    i += 1;
                    match rest.get(i) {
                        Some(v) => from = v.clone(),
                        None => {
                            eprintln!("comux worktree create: --from needs a value");
                            return 2;
                        }
                    }
                }
                "--json" => json = true,
                // tmx `twt --keep-current`: create the session but stay in the current
                // shell (don't drop into it) — implied by `--json` (scripting) too.
                "--no-attach" | "--keep-current" => no_attach = true,
                _ => {
                    eprintln!("comux worktree create: unknown flag '{a}'");
                    return 2;
                }
            }
        } else if branch.is_some() {
            eprintln!("comux worktree create: unexpected extra argument '{a}'");
            return 2;
        } else {
            branch = Some(a);
        }
        i += 1;
    }
    let Some(branch) = branch else {
        eprintln!("usage: comux worktree create <branch> [--from <ref>] [--no-attach]");
        return 2;
    };
    let req = Req::WorktreeCreate {
        branch: branch.to_string(),
        from: (!from.is_empty()).then(|| from.clone()),
        cwd: caller_cwd(),
    };
    // Create opens a session, so it needs a server — start one if none is running.
    if let Err(e) = crate::client::ensure_running(&socket_path()) {
        eprintln!("comux: could not start server: {e}");
        return 1;
    }
    let code = match round_trip(&req) {
        Ok(resp) => print_worktree_result(&resp, json),
        Err(e) => {
            eprintln!("comux: {e}");
            return 1;
        }
    };
    // tmx `twt` parity: when run from a plain shell (NOT inside a comux pane), drop into
    // the freshly-switched session by attaching a client — this call blocks the TUI until
    // detach. Skipped inside comux (the attached view already followed the switch — a
    // nested client would recurse), in `--json`/`--no-attach` mode, and on failure.
    if code == 0
        && !json
        && !no_attach
        && std::env::var_os("COPAD_MUX").is_none()
        && let Err(e) = crate::client::run()
    {
        eprintln!("comux: attach failed: {e}");
        return 1;
    }
    code
}

const APPLY_LAYOUT_USAGE: &str = "usage: comux apply-layout <file.toml> [--name N] [--detached] [--dry-run]\n\
\x20      build a whole session from a layout file: its tabs, each tab's preset layout and the\n\
\x20      lines to type into each pane (see `comux skill`); --dry-run prints what it would build";

/// `comux apply-layout FILE`: build the session a layout file describes, out of the same
/// requests a script would send — `new-session`, `new-tab`, `split`, `select-layout`, `send`
/// — so it needs nothing new from the server and an older one runs it too. Then switch to it
/// (and attach, from a plain shell) unless `--detached`.
fn apply_layout_client(rest: &[String]) -> i32 {
    let mut file = None;
    let (mut name, mut detached, mut dry_run) = (None, false, false);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => {
                println!("{APPLY_LAYOUT_USAGE}");
                return 0;
            }
            "--detached" | "-d" => detached = true,
            "--dry-run" => dry_run = true,
            "--name" => match it.next() {
                Some(n) => name = Some(n.clone()),
                None => {
                    eprintln!("{APPLY_LAYOUT_USAGE}");
                    return 2;
                }
            },
            f if file.is_none() && !f.starts_with('-') => file = Some(f.to_string()),
            _ => {
                eprintln!("{APPLY_LAYOUT_USAGE}");
                return 2;
            }
        }
    }
    let Some(file) = file else {
        eprintln!("{APPLY_LAYOUT_USAGE}");
        return 2;
    };
    let mut plan = match crate::layoutfile::load(Path::new(&file)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("comux apply-layout: {e}");
            return 2;
        }
    };
    if let Some(n) = name {
        plan.session = n;
    }
    let missing = plan.missing_dirs();
    if !missing.is_empty() {
        for d in &missing {
            eprintln!("comux apply-layout: no such directory: {}", d.display());
        }
        return 2;
    }
    if dry_run {
        print!("{}", plan.describe());
        return 0;
    }
    if let Err(e) = crate::client::ensure_running(&socket_path()) {
        eprintln!("comux: could not start server: {e}");
        return 1;
    }
    match build_layout(&plan) {
        Ok(session) => {
            println!(
                "built session {} ({} tab{})",
                plan.session,
                plan.tabs.len(),
                if plan.tabs.len() == 1 { "" } else { "s" }
            );
            if detached {
                return 0;
            }
            if let Err(e) = round_trip(&Req::SelectSession {
                index: None,
                target: Some(session),
            }) {
                eprintln!("comux: {e}");
                return 1;
            }
            // Like `worktree create`: from a plain shell, drop into the new session.
            if std::env::var_os("COPAD_MUX").is_none()
                && let Err(e) = crate::client::run()
            {
                eprintln!("comux: attach failed: {e}");
                return 1;
            }
            0
        }
        Err(e) => {
            eprintln!("comux apply-layout: {e}");
            1
        }
    }
}

/// Run `plan` against the server; returns the new session's id. A failure part-way leaves
/// what was built so far (it is an ordinary session) and says where it stopped.
fn build_layout(plan: &crate::layoutfile::Plan) -> Result<String, String> {
    let call = |req: &Req, what: &str| -> Result<Resp, String> {
        let r = round_trip(req)?;
        if r.ok {
            Ok(r)
        } else {
            Err(format!(
                "{what}: {}",
                r.error.unwrap_or_else(|| "refused".into())
            ))
        }
    };
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let created = call(
        &Req::NewSession {
            name: Some(plan.session.clone()),
            cwd: Some(path(&plan.cwd)),
            detached: true,
        },
        "creating the session",
    )?;
    let (Some(session), Some(first)) = (created.session.map(|s| s.id), created.pane) else {
        return Err("the server did not say which session it created".into());
    };
    for (ti, tab) in plan.tabs.iter().enumerate() {
        let what = |s: &str| format!("tab {}: {s}", ti + 1);
        let head = if ti == 0 {
            if let Some(n) = &tab.name {
                call(
                    &Req::RenameTab {
                        index: None,
                        pane: Some(first.clone()),
                        target: None,
                        name: n.clone(),
                    },
                    &what("naming it"),
                )?;
            }
            // The session's first shell starts in the SESSION's directory (where its new tabs
            // open). When this tab's first pane belongs elsewhere, open a shell there and close
            // the original — the pane order puts the original first in the tab — rather than
            // typing a `cd` that the pane's commands would then have to be chained behind.
            if tab.panes[0].cwd != plan.cwd {
                let moved = call(
                    &Req::Split {
                        dir: "right".into(),
                        from: Some(first.clone()),
                        no_focus: true,
                        cwd: Some(path(&tab.panes[0].cwd)),
                        command: Vec::new(),
                        env: Vec::new(),
                        keep: false,
                    },
                    &what("pane 1"),
                )?
                .pane
                .ok_or_else(|| what("the server did not name a pane"))?;
                call(
                    &Req::Close {
                        index: 0,
                        caller: Some(moved.clone()),
                    },
                    &what("pane 1"),
                )?;
                moved
            } else {
                first.clone()
            }
        } else {
            call(
                &Req::NewTab {
                    session: Some(session.clone()),
                    from: None,
                    // Its first pane's directory: the tab's shell starts there, no `cd` needed.
                    cwd: Some(path(&tab.panes[0].cwd)),
                    name: tab.name.clone(),
                    no_focus: true,
                    command: Vec::new(),
                    env: Vec::new(),
                    keep: false,
                    owner: None,
                },
                &what("creating it"),
            )?
            .pane
            .ok_or_else(|| what("the server did not name its pane"))?
        };
        let mut tokens = vec![head.clone()];
        for (pi, pane) in tab.panes.iter().enumerate().skip(1) {
            // Each split comes off the pane made last, so the tree's pane order — which the
            // preset keeps — is the file's order.
            let last = tokens.last().cloned().unwrap_or_else(|| head.clone());
            let r = call(
                &Req::Split {
                    dir: "right".into(),
                    from: Some(last),
                    no_focus: true,
                    cwd: Some(path(&pane.cwd)),
                    command: Vec::new(),
                    env: Vec::new(),
                    keep: false,
                },
                &what(&format!("pane {}", pi + 1)),
            )?;
            tokens.push(
                r.pane
                    .ok_or_else(|| what("the server did not name a pane"))?,
            );
        }
        if let Some(layout) = tab.layout {
            call(
                &Req::Arrange {
                    op: layout.name().into(),
                    caller: Some(head.clone()),
                },
                &what("arranging it"),
            )?;
        }
        for (pi, (tok, pane)) in tokens.iter().zip(&tab.panes).enumerate() {
            let lines = pane.run.clone();
            for line in lines {
                call(
                    &Req::SendKeys {
                        target: Some(tok.clone()),
                        index: None,
                        text: line,
                        paste: false,
                        enter: true,
                        caller: None,
                    },
                    &what(&format!("typing into pane {}", pi + 1)),
                )?;
            }
        }
    }
    Ok(session)
}

fn worktree_list_client(rest: &[String]) -> i32 {
    let mut json = false;
    let mut plain = false;
    for a in rest {
        match a.as_str() {
            "--json" => json = true,
            "--plain" => plain = true,
            other => {
                eprintln!("comux worktree list: unexpected argument '{other}'");
                return 2;
            }
        }
    }
    if json && plain {
        eprintln!("comux worktree list: --plain and --json conflict");
        return 2;
    }
    match collect_worktrees() {
        Ok(infos) => {
            print_worktrees(&infos, json, plain);
            0
        }
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

/// The git worktrees of the repo containing the cwd — from the server when one is
/// running (so `live` is annotated), else straight from git, matching tmx's "works with
/// no server" behavior. Shared by `worktree list` and the `worktree rm` picker.
///
/// The error string arrives PRE-PREFIXED (`error:` = the server refused · `comux:` =
/// local or transport) so callers just print it and keep the CLI's existing wording.
fn collect_worktrees() -> Result<Vec<WorktreeInfo>, String> {
    if server_running() {
        let req = Req::WorktreeList { cwd: caller_cwd() };
        return match round_trip(&req) {
            Ok(resp) if resp.ok => Ok(resp.worktrees.unwrap_or_default()),
            Ok(resp) => Err(format!(
                "error: {}",
                resp.error.as_deref().unwrap_or("(unspecified)")
            )),
            Err(e) => Err(format!("comux: {e}")),
        };
    }
    let cwd = std::env::current_dir()
        .map_err(|_| "comux: could not resolve current directory".to_string())?;
    let repo = crate::worktree::resolve_repo_root(&cwd).map_err(|e| format!("comux: {e}"))?;
    let entries = crate::worktree::list_entries(&repo).map_err(|e| format!("comux: {e}"))?;
    Ok(entries
        .iter()
        .map(|e| WorktreeInfo {
            path: e.path.display().to_string(),
            branch: e.branch.clone().unwrap_or_default(),
            is_main: e.is_main,
            live: false,
            locked: e.locked,
        })
        .collect())
}

/// `comux worktree land` (decisions #149). Every refusal is checked before anything is
/// rewritten; the steps after that report exactly how far they got.
fn worktree_land_client(rest: &[String]) -> i32 {
    use crate::worktree::{self as wt, Strategy};
    const USAGE: &str =
        "usage: comux worktree land <path|branch> [--merge] [--keep] [--dry-run] [--json]";
    let (mut target, mut merge, mut keep, mut dry_run, mut json) =
        (None, false, false, false, false);
    let mut flags_done = false;
    for a in rest {
        match a.as_str() {
            "--" if !flags_done => flags_done = true,
            "--merge" if !flags_done => merge = true,
            "--keep" if !flags_done => keep = true,
            "--dry-run" | "-n" if !flags_done => dry_run = true,
            "--json" if !flags_done => json = true,
            f if !flags_done && f.starts_with('-') => {
                eprintln!("comux worktree land: unknown flag '{f}'\n{USAGE}");
                return 2;
            }
            t if target.is_none() => target = Some(t.to_string()),
            t => {
                eprintln!("comux worktree land: unexpected extra argument '{t}'\n{USAGE}");
                return 2;
            }
        }
    }
    let Some(target) = target else {
        eprintln!("comux worktree land: which worktree? (a path or its branch)\n{USAGE}");
        return 2;
    };
    let report = |ok: bool, msg: &str| -> i32 {
        if json {
            println!("{}", serde_json::json!({ "ok": ok, "message": msg }));
        } else if ok {
            println!("{msg}");
        } else {
            eprintln!("error: {msg}");
        }
        if ok { 0 } else { 1 }
    };
    let Some(cwd) = std::env::current_dir().ok() else {
        return report(false, "could not resolve the current directory");
    };
    let repo = match wt::resolve_repo_root(&cwd) {
        Ok(r) => r,
        Err(e) => return report(false, &e),
    };
    let entries = match wt::list_entries(&repo) {
        Ok(e) => e,
        Err(e) => return report(false, &e),
    };
    let Some(main) = entries.iter().find(|e| e.is_main).map(|e| e.path.clone()) else {
        return report(false, "no main worktree found");
    };
    let entry = match wt::resolve_target(&entries, &target, &cwd) {
        Ok(e) => e,
        Err(e) => return report(false, &e),
    };
    if wt::canonical_or_lexical(&cwd).starts_with(wt::canonical_or_lexical(&entry.path)) {
        return report(
            false,
            "run it from outside the worktree you are landing (it gets removed)",
        );
    }
    // A live comux session inside: the rebase would rewrite the branch an agent is working on,
    // and the removal would be refused AFTER the history was rewritten. Refused up front,
    // `--keep` or not; land never kills sessions. A failed lookup is an error, not a pass.
    let infos = match collect_worktrees() {
        Ok(i) => i,
        Err(e) => return report(false, &format!("could not check for live sessions: {e}")),
    };
    if infos.iter().any(|w| {
        w.live
            && wt::canonical_or_lexical(Path::new(&w.path)) == wt::canonical_or_lexical(&entry.path)
    }) {
        return report(
            false,
            "a comux session is still working in that worktree — close it first",
        );
    }
    let _lock = match wt::LandLock::acquire(&main) {
        Ok(l) => l,
        Err(e) => return report(false, &e),
    };
    let strategy = if merge {
        Strategy::Merge
    } else {
        Strategy::Rebase
    };
    let plan = match wt::plan_land(&main, &entry, strategy) {
        Ok(p) => p,
        Err(e) => return report(false, &e),
    };
    let how = match (plan.ahead, strategy) {
        (0, _) => "nothing to bring in (already contained)".to_string(),
        (n, Strategy::Rebase) => format!(
            "rebase {n} commit(s) onto '{}' and fast-forward it",
            plan.into
        ),
        (n, Strategy::Merge) => format!(
            "merge {n} commit(s) into '{}' with a merge commit",
            plan.into
        ),
    };
    let cleanup = if keep {
        "keep the worktree and the branch".to_string()
    } else {
        format!(
            "remove {} and delete '{}'",
            plan.worktree.display(),
            plan.branch
        )
    };
    if dry_run {
        return report(
            true,
            &format!("would land '{}': {how}; then {cleanup}", plan.branch),
        );
    }
    if let Err(e) = wt::perform_land(&plan) {
        return report(false, &e);
    }
    if keep {
        return report(
            true,
            &format!(
                "landed '{}' into '{}' (kept the worktree)",
                plan.branch, plan.into
            ),
        );
    }
    // Removal through the server when one runs (it owns liveness), else under its flock.
    let path = plan.worktree.display().to_string();
    let removed = if server_running() {
        let req = Req::WorktreeRm {
            target: path.clone(),
            force: false,
            delete_branch: false,
            cwd: caller_cwd(),
        };
        match round_trip(&req) {
            Ok(r) if r.ok => Ok(()),
            Ok(r) => Err(r.error.unwrap_or_default()),
            Err(e) => Err(e.to_string()),
        }
    } else {
        match crate::server::try_acquire_lock() {
            Some(_guard) => wt::remove(&main, &plan.worktree, false),
            None => Err("a comux server started meanwhile — retry".into()),
        }
    };
    if let Err(e) = removed {
        return report(
            false,
            &format!(
                "landed '{}' into '{}', but removing {path} failed (kept): {e}",
                plan.branch, plan.into
            ),
        );
    }
    match wt::delete_landed_branch(&main, &plan.branch, &plan.into) {
        Ok(()) => report(
            true,
            &format!(
                "landed '{}' into '{}'; removed {path} and the branch",
                plan.branch, plan.into
            ),
        ),
        Err(e) => report(
            false,
            &format!(
                "landed '{}' into '{}' and removed {path}, but the branch was kept: {e}",
                plan.branch, plan.into
            ),
        ),
    }
}

fn worktree_rm_client(rest: &[String]) -> i32 {
    let mut target: Option<&str> = None;
    let mut force = false;
    let mut delete_branch = false;
    let mut json = false;
    let mut flags_done = false;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if !flags_done && a == "--" {
            flags_done = true;
        } else if !flags_done && a.starts_with('-') {
            match a {
                "-f" | "--force" => force = true,
                "-d" | "--delete-branch" => delete_branch = true,
                "--json" => json = true,
                _ => {
                    eprintln!("comux worktree rm: unknown flag '{a}'");
                    return 2;
                }
            }
        } else if target.is_some() {
            eprintln!("comux worktree rm: unexpected extra argument '{a}'");
            return 2;
        } else {
            target = Some(a);
        }
        i += 1;
    }
    // No target → fuzzy-pick one from the repo's worktrees instead of failing with a
    // usage line (the whole point: you rarely remember the sibling path by heart).
    let target: String = match target {
        Some(t) => t.to_string(),
        None => match pick_worktree(json) {
            Ok(t) => t,
            Err(code) => return code,
        },
    };

    // A running server owns liveness + the removal (single writer). With no server there
    // are no live sessions; take the server flock so none can start under us, then remove
    // locally — race-free, and without leaving a spurious server behind.
    match crate::server::try_acquire_lock() {
        Some(_guard) => worktree_rm_local(&target, force, delete_branch, json),
        None => {
            let req = Req::WorktreeRm {
                target: target.clone(),
                force,
                delete_branch,
                cwd: caller_cwd(),
            };
            match round_trip(&req) {
                Ok(resp) => print_worktree_result(&resp, json),
                Err(e) => {
                    eprintln!("comux: {e}");
                    1
                }
            }
        }
    }
}

/// Fuzzy-pick a worktree for a bare `comux worktree rm`. Candidates exclude the main
/// worktree, `git worktree lock`ed ones, and the one the caller is standing in — all
/// three are refused unconditionally by [`crate::worktree::validate_removal`], so
/// offering them would only dead-end. The locked/current count goes in the title so a
/// missing entry is never a mystery; the MAIN worktree is not counted — it is never a
/// removal target anywhere, so reporting it would put a "1 hidden" on every ordinary
/// repo. A `live` worktree IS offered (it is removable with `--force`) and marked.
fn pick_worktree(json: bool) -> Result<String, i32> {
    const USAGE: &str = "comux worktree rm [<path|branch>] [-f] [-d]   (no target → fuzzy picker)";
    prompt_ok(json, USAGE)?;
    let infos = collect_worktrees().map_err(|msg| {
        eprintln!("{msg}");
        1
    })?;
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| crate::worktree::canonical_or_lexical(&p));
    let mut hidden = 0usize;
    let mut rows = Vec::new();
    for w in &infos {
        if w.is_main {
            continue;
        }
        let inside = cwd.as_ref().is_some_and(|c| {
            c.starts_with(crate::worktree::canonical_or_lexical(Path::new(&w.path)))
        });
        if w.locked || inside {
            hidden += 1;
            continue;
        }
        let label = if w.branch.is_empty() {
            "(detached)".to_string()
        } else {
            w.branch.clone()
        };
        let mut detail = w.path.clone();
        if w.live {
            detail.push_str(" · live");
        }
        rows.push((picker::Item::new(label, detail), w.path.clone()));
    }
    let title = if hidden > 0 {
        format!("remove which worktree?  ({hidden} hidden: locked or current)")
    } else {
        "remove which worktree?".to_string()
    };
    choose(&title, "no removable worktrees in this repo", rows)
}

fn worktree_rm_local(target: &str, force: bool, delete_branch: bool, json: bool) -> i32 {
    let Some(cwd) = std::env::current_dir().ok() else {
        eprintln!("comux: could not resolve current directory");
        return 1;
    };
    let repo = match crate::worktree::resolve_repo_root(&cwd) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("comux: {e}");
            return 1;
        }
    };
    let entries = match crate::worktree::list_entries(&repo) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("comux: {e}");
            return 1;
        }
    };
    let entry = match crate::worktree::validate_removal(&entries, target, &cwd, delete_branch) {
        Ok(e) => e,
        Err(e) => {
            let resp = Resp::err(e);
            return print_worktree_result(&resp, json);
        }
    };
    if let Err(e) = crate::worktree::remove(&repo, &entry.path, force) {
        let resp = Resp::err(e);
        return print_worktree_result(&resp, json);
    }
    let resp = finish_branch_delete(&repo, &entry, delete_branch, force);
    print_worktree_result(&resp, json)
}

/// After a worktree was removed, optionally delete its branch and build the outcome
/// response (branch-delete failure is a partial success → `ok=false` with a message
/// naming exactly what happened).
pub fn finish_branch_delete(
    repo: &Path,
    entry: &crate::worktree::Entry,
    delete_branch: bool,
    force: bool,
) -> Resp {
    let mut msg = format!("removed worktree {}", entry.path.display());
    if delete_branch && let Some(b) = &entry.branch {
        match crate::worktree::delete_branch(repo, b, force) {
            Ok(()) => msg.push_str(&format!("; deleted branch {b}")),
            Err(e) => {
                return Resp::err(format!("{msg}, but branch '{b}' was not deleted: {e}"));
            }
        }
    }
    Resp::message(msg)
}

fn print_worktrees(infos: &[WorktreeInfo], json: bool, plain: bool) {
    if json {
        println!("{}", serde_json::to_string(infos).unwrap_or_default());
        return;
    }
    if plain {
        for w in infos {
            println!("{}", w.path);
        }
        return;
    }
    println!(
        "{:<44} {:<20} {:<5} {:<5} LOCKED",
        "PATH", "BRANCH", "MAIN", "LIVE"
    );
    for w in infos {
        println!(
            "{:<44} {:<20} {:<5} {:<5} {}",
            w.path,
            if w.branch.is_empty() { "-" } else { &w.branch },
            if w.is_main { "*" } else { "" },
            if w.live { "*" } else { "" },
            if w.locked { "*" } else { "" },
        );
    }
}

#[cfg(test)]
mod verb_args_tests {
    use super::*;

    /// Run `comux <args>` against a socket with NO listener. A help/usage path returns 0/2
    /// before any request is built; an invocation that wrongly got PAST parsing tries the
    /// dead socket and returns 1 — so the exit code alone says whether a request was sent.
    ///
    /// No env juggling here: a test build's [`socket_path`] already falls back to a path with
    /// no listener, and setting the process-global variable would race other tests.
    fn exit(args: &[&str]) -> i32 {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        run_client(&argv)
    }

    /// Every verb that reaches the request path. `new-session` is deliberately absent: it
    /// starts a server when parsing lets it through, so a regression there would spawn a
    /// stray server — the e2e script covers it against an isolated one instead.
    const VERBS: &[&str] = &[
        "list",
        "health",
        "host",
        "reload",
        "kill-server",
        "back",
        "forward",
        "last",
        "zoom",
        "select-layout",
        "swap-pane",
        "rotate-window",
        "break-pane",
        "move-tab",
        "apply-layout",
        "rename-pane",
        "fork",
        "mark",
        "marks",
        "goto",
        "broadcast",
        "list-tabs",
        "new-tab",
        "spawn",
        "agent-log",
        "inbox",
        "list-sessions",
        "list-agents",
        "rename-session",
        "rename-tab",
        "select-session",
        "select-tab",
        "close-tab",
        "kill-session",
        "split",
        "focus",
        "close",
        "capture-pane",
        "jump",
        "notify",
        "resize",
        "send",
        "skill",
        "wait-output",
        "wait-agent",
    ];

    #[test]
    fn help_exits_zero_without_a_request() {
        for v in VERBS {
            assert_eq!(exit(&[v, "--help"]), 0, "{v} --help");
        }
        assert_eq!(exit(&["worktree", "rm", "--help"]), 0);
    }

    #[test]
    fn unknown_flags_exit_two_without_a_request() {
        for v in VERBS {
            assert_eq!(exit(&[v, "--bogus"]), 2, "{v} --bogus");
        }
    }

    #[test]
    fn verbs_that_take_nothing_refuse_an_argument() {
        for v in [
            "list",
            "health",
            "host",
            "reload",
            "kill-server",
            "back",
            "forward",
            "last",
            "list-tabs",
            "new-tab",
            "list-sessions",
            "list-agents",
            "skill",
        ] {
            assert_eq!(exit(&[v, "extra"]), 2, "{v} extra");
        }
        for v in ["select-session", "close-tab", "focus", "jump"] {
            assert_eq!(exit(&[v, "1", "2"]), 2, "{v} 1 2");
        }
    }

    #[test]
    fn fleet_verbs_parse_before_any_sweep() {
        // Help and bad flags must answer without reading config or running ssh.
        assert_eq!(exit(&["machines", "--help"]), 0);
        assert_eq!(exit(&["machines", "--bogus"]), 2);
        assert_eq!(exit(&["machines", "extra"]), 2);
        assert_eq!(exit(&["list-agents", "--fleet", "--help"]), 0);
        assert_eq!(exit(&["list-agents", "--fleet", "extra"]), 2);
    }

    #[test]
    fn spawn_refuses_bad_input_before_any_request() {
        assert_eq!(exit(&["spawn"]), 2, "nothing to run");
        assert_eq!(exit(&["spawn", "--env", "NOEQ", "--", "true"]), 2);
        assert_eq!(
            exit(&["spawn", "--env", "COPAD_MUX_SOCK=x", "--", "true"]),
            2
        );
        assert_eq!(exit(&["spawn", "--cwd", "/no/such/dir", "--", "true"]), 2);
        assert_eq!(exit(&["spawn", "--keep", "--help"]), 0);
    }

    #[test]
    fn owner_and_inbox_refuse_bad_input_before_any_request() {
        assert_eq!(exit(&["inbox", "p", "--timeout", "x", "--wait"]), 2);
        assert_eq!(
            exit(&["inbox", "p", "--timeout", "5"]),
            2,
            "--timeout needs --wait"
        );
        assert_eq!(exit(&["inbox", "p", "q"]), 2);
        assert_eq!(
            exit(&["close-tab", "--owned-by", "p"]),
            2,
            "neither --dry-run nor --yes"
        );
        assert_eq!(
            exit(&["close-tab", "--owned-by", "p", "--dry-run", "--yes"]),
            2
        );
        assert_eq!(
            exit(&["notify", "--result", "/etc/hosts", "x"]),
            2,
            "--result needs --to"
        );
        assert_eq!(
            exit(&["notify", "--to", "p", "--result", "/no/such/file", "x"]),
            2
        );
        assert_eq!(exit(&["close-tab", "--owned-by", "p", "--help"]), 0);
    }

    #[test]
    fn agent_log_refuses_bad_input_before_any_request() {
        assert_eq!(exit(&["agent-log"]), 2, "no pane");
        assert_eq!(exit(&["agent-log", "p", "--last", "many"]), 2);
        assert_eq!(exit(&["agent-log", "p", "--max-chars", "-1"]), 2);
        assert_eq!(exit(&["agent-log", "p", "q"]), 2);
    }

    #[test]
    fn env_pairs_are_portable_names_and_never_comux_markers() {
        assert_eq!(env_pair("A_1=x=y"), Ok(("A_1", "x=y")));
        assert_eq!(env_pair("EMPTY="), Ok(("EMPTY", "")));
        for bad in [
            "noequals",
            "=v",
            "1A=v",
            "A-B=v",
            "COPAD_MUX=1",
            "COPAD_MUX_PANE=p",
        ] {
            assert!(env_pair(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_value_flag_does_not_eat_help() {
        assert_eq!(exit(&["split", "--from", "--help"]), 0);
        // `split` owns `-h`, so there it is a missing value, not help.
        assert_eq!(exit(&["split", "--from", "-h"]), 2);
        assert_eq!(
            exit(&["notify", "--pane", "a-1", "--kind", "--bogus", "x"]),
            2
        );
    }

    #[test]
    fn a_targeted_rename_takes_every_word_as_the_name() {
        let p = cliargs::parse(&RENAME_TAB_SPEC, &["-t", "s1:t0", "2026", "plan"]).unwrap();
        assert_eq!(rename_args(&p), Some((None, "2026 plan".to_string())));
        let p = cliargs::parse(&RENAME_TAB_SPEC, &["2", "plan"]).unwrap();
        assert_eq!(rename_args(&p), Some((Some(2), "plan".to_string())));
        let p = cliargs::parse(&RENAME_TAB_SPEC, &["-t", "s1:t0"]).unwrap();
        assert_eq!(rename_args(&p), None);
    }

    #[test]
    fn send_file_and_text_are_exclusive_and_the_file_is_bounded() {
        assert_eq!(exit(&["send", "a1-1", "--file", "/etc/hosts", "extra"]), 2);
        assert_eq!(exit(&["send", "a1-1", "--file", "/no/such/file"]), 2);
        let big = std::env::temp_dir().join(format!("comux-send-big-{}", std::process::id()));
        std::fs::write(&big, vec![b'a'; SEND_FILE_CAP as usize + 1]).unwrap();
        assert!(read_send_file(big.to_str().unwrap()).is_err());
        std::fs::write(&big, b"hi\n").unwrap();
        assert_eq!(read_send_file(big.to_str().unwrap()).as_deref(), Ok("hi\n"));
        std::fs::write(&big, [0xff, 0xfe]).unwrap();
        assert!(read_send_file(big.to_str().unwrap()).is_err());
        let _ = std::fs::remove_file(&big);
    }

    #[test]
    fn split_keeps_dash_h_as_horizontal() {
        // Parses (then fails on the dead socket) — `-h` is the split direction, not help.
        assert_eq!(exit(&["split", "-h"]), 1);
        assert_eq!(exit(&["split", "--help"]), 0);
    }

    #[test]
    fn a_body_after_the_separator_may_start_with_a_dash() {
        // Parses, so it reaches (and fails on) the dead socket.
        assert_eq!(exit(&["send", "a1-1", "--", "--json"]), 1);
        assert_eq!(exit(&["send", "a1-1", "--json-ish"]), 2);
    }
}

#[cfg(test)]
mod server_admin_tests {
    use super::*;

    /// Point the socket at a path with no listener so `run_server_admin` sees "not running"
    /// deterministically — the branches that don't spawn a server (status/stop/unknown) are
    /// the ones safe to assert on in a unit test.
    fn with_dead_socket<T>(f: impl FnOnce() -> T) -> T {
        // Unique-ish per test via the thread name so parallel tests don't collide.
        let name = std::thread::current()
            .name()
            .unwrap_or("t")
            .replace(':', "_");
        let path = std::env::temp_dir().join(format!("copad-mux-admin-test-{name}.sock"));
        let _ = std::fs::remove_file(&path);
        // SAFETY: single-threaded within this closure; the var is restored on the way out.
        unsafe { std::env::set_var("COPAD_MUX_TEST_SOCK", &path) };
        let out = f();
        unsafe { std::env::remove_var("COPAD_MUX_TEST_SOCK") };
        out
    }

    #[test]
    fn status_reports_not_running_when_absent() {
        with_dead_socket(|| assert_eq!(run_server_admin("status"), 1));
    }

    #[test]
    fn stop_is_a_noop_success_when_not_running() {
        with_dead_socket(|| assert_eq!(run_server_admin("stop"), 0));
    }

    #[test]
    fn unknown_action_is_a_usage_error() {
        with_dead_socket(|| assert_eq!(run_server_admin("frobnicate"), 2));
    }

    /// Build the positional shape `run_client` passes to `parse_rename`.
    fn rename_args(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_rename_targets_active_or_index() {
        let check = |args: &[&str], want: Option<(Option<usize>, &str)>| {
            let owned = rename_args(&args[1..]);
            assert_eq!(
                parse_rename(&owned),
                want.map(|(i, n)| (i, n.to_string())),
                "args: {args:?}"
            );
        };
        // Plain name → the ACTIVE tab/session.
        check(&["rename-tab", "build"], Some((None, "build")));
        // Leading integer + name → index form, name space-joined.
        check(
            &["rename-tab", "2", "build", "tools"],
            Some((Some(2), "build tools")),
        );
        // A lone integer is a NAME (no name follows it), not a missing-name error.
        check(&["rename-tab", "2"], Some((None, "2")));
        // An explicit empty name clears (Some with empty string), no args is usage.
        check(&["rename-tab", ""], Some((None, "")));
        check(&["rename-tab"], None);
    }

    /// The two destructive verbs must keep their kebab-case wire names: `close-tab` /
    /// `kill-session` are what a script sends, and `kill-session` must stay a DIFFERENT
    /// verb from the long-standing `kill-server` (one drops a workspace, the other the
    /// whole daemon) — a rename that collapsed them would be catastrophic and silent.
    #[test]
    fn close_tab_and_kill_session_round_trip() {
        assert_eq!(
            serde_json::to_string(&Req::CloseTab {
                index: Some(1),
                target: None
            })
            .unwrap(),
            r#"{"cmd":"close-tab","index":1}"#
        );
        assert_eq!(
            serde_json::to_string(&Req::KillSession {
                index: Some(2),
                target: None
            })
            .unwrap(),
            r#"{"cmd":"kill-session","index":2}"#
        );
        assert!(matches!(
            serde_json::from_str::<Req>(r#"{"cmd":"close-tab","index":3}"#).unwrap(),
            Req::CloseTab {
                index: Some(3),
                target: None
            }
        ));
        assert!(matches!(
            serde_json::from_str::<Req>(r#"{"cmd":"kill-session","index":0}"#).unwrap(),
            Req::KillSession {
                index: Some(0),
                target: None
            }
        ));
        assert!(matches!(
            serde_json::from_str::<Req>(r#"{"cmd":"kill-server"}"#).unwrap(),
            Req::KillServer
        ));
    }

    /// An old client's `rename-session` (bare `index`) and a new index-less request must
    /// both deserialize — `index` is `Option` + `#[serde(default)]` for wire back-compat.
    #[test]
    fn rename_reqs_round_trip_with_and_without_index() {
        let old: Req = serde_json::from_str(r#"{"cmd":"rename-session","index":1,"name":"api"}"#)
            .expect("indexed form must parse");
        assert!(matches!(
            old,
            Req::RenameSession { index: Some(1), pane: None, target: None, ref name } if name == "api"
        ));
        let new: Req = serde_json::from_str(r#"{"cmd":"rename-tab","name":"build"}"#)
            .expect("index-less form must parse");
        assert!(matches!(
            new,
            Req::RenameTab { index: None, pane: None, target: None, ref name } if name == "build"
        ));
    }
}

#[cfg(test)]
mod capture_proto_tests {
    use super::*;

    /// `capture-pane`'s three arguments are all optional, so the minimal request a script
    /// writes by hand (`{"cmd":"capture-pane"}`) must parse as "focused pane, visible
    /// screen" rather than failing.
    #[test]
    fn minimal_request_parses() {
        let r: Req = serde_json::from_str(r#"{"cmd":"capture-pane"}"#).expect("must parse");
        match r {
            Req::CapturePane {
                target,
                index,
                lines,
                caller,
            } => {
                assert_eq!(caller, None);
                assert_eq!(target, None);
                assert_eq!(index, None);
                assert_eq!(lines, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn full_request_round_trips() {
        let req = Req::CapturePane {
            target: Some("ab12-3".into()),
            index: None,
            lines: Some(500),
            caller: None,
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(
            line.contains(r#""cmd":"capture-pane""#),
            "wire form: {line}"
        );
        match serde_json::from_str::<Req>(&line).unwrap() {
            Req::CapturePane { target, lines, .. } => {
                assert_eq!(target.as_deref(), Some("ab12-3"));
                assert_eq!(lines, Some(500));
            }
            other => panic!("wrong round-trip: {other:?}"),
        }
    }

    /// The response carries the RESOLVED pane token, so a caller that let the target
    /// default to the focused pane can tell which pane actually answered.
    #[test]
    fn response_carries_text_rows_and_resolved_pane() {
        let resp = Resp::capture(
            crate::term::Capture {
                text: "hello".into(),
                rows: 2,
                truncated: true,
            },
            "ab12-3".into(),
        );
        let line = serde_json::to_string(&resp).unwrap();
        let back: Resp = serde_json::from_str(&line).unwrap();
        assert!(back.ok);
        assert_eq!(back.text.as_deref(), Some("hello"));
        assert_eq!(back.capture_rows, Some(2));
        assert_eq!(back.truncated, Some(true));
        assert_eq!(back.pane.as_deref(), Some("ab12-3"));
    }

    /// The capture fields are skipped when absent, so every OTHER verb's response is
    /// unchanged on the wire and an older client parsing it sees exactly what it did before.
    #[test]
    fn non_capture_responses_carry_no_capture_fields() {
        let line = serde_json::to_string(&Resp::ok()).unwrap();
        for field in ["text", "capture_rows", "truncated", "pane"] {
            assert!(!line.contains(field), "{field} leaked into: {line}");
        }
    }

    /// An OLDER server does not know the verb. There is no graceful degradation to
    /// arrange — it answers with a parse error — so the contract is simply that the
    /// failure is explicit rather than a silent empty capture.
    #[test]
    fn an_unknown_verb_is_a_parse_error_not_a_default() {
        assert!(serde_json::from_str::<Req>(r#"{"cmd":"capture-pane-v2"}"#).is_err());
    }
}

// ---------------------------------------------------------------------------
// `wait-output` — block until a pane's text matches (WU2, decision #104).
// ---------------------------------------------------------------------------

/// Exit code for "the wait timed out". `timeout(1)`'s convention, so a shell can tell a
/// deadline from a real failure. Distinct from [`EXIT_CANCELLED`] (130).
const EXIT_TIMEOUT: i32 = 124;

/// Smallest poll interval accepted. A caller must not be able to turn the wait into a
/// busy-loop: every poll wakes the single-writer loop and takes a pane's terminal lock.
const MIN_INTERVAL: Duration = Duration::from_millis(50);

const WAIT_USAGE: &str = "usage: comux wait-output [<pane-token|terminal-id>] <pattern>\n\
     \x20      [--index N [--active]] [--timeout S] [--lines N] [--interval MS]\n\
     \n\
     BEST-EFFORT. It matches the pane's CURRENT text — including text that was already\n\
     there before the call — and a match can be ERASED between polls by a \\r overwrite,\n\
     a line-erase, or an alt-screen redraw. It is not proof that nothing was missed.\n\
     \n\
     Two traps the verb cannot detect for you: the shell ECHOES the command you send, so a\n\
     marker visible in that command satisfies the wait immediately; and a marker left over\n\
     from a previous run satisfies the next wait. So build the marker from fragments the\n\
     sent command never contains as a whole, and make it fresh each call. `send` does not\n\
     append Enter, so submit the line yourself.\n\
     \n\
     \x20 id=$(date +%s%N)\n\
     \x20 comux send 0 \"make build; printf 'MARK-%s\\n' '$id'\"\n\
     \x20 comux send 0 $'\\n'\n\
     \x20 comux wait-output --index 0 \"MARK-$id\"";

/// Everything `wait-output` parsed off the command line.
#[derive(Debug, PartialEq)]
struct WaitArgs {
    target: Option<String>,
    index: Option<usize>,
    /// The pane `--index` counts from (see [`index_caller`]); checked against the server's
    /// capabilities inside the wait's deadline, not before it.
    caller: Option<String>,
    pattern: String,
    /// `None` = wait forever (`--timeout 0`).
    timeout: Option<Duration>,
    lines: usize,
    interval: Duration,
}

/// The last line of `text` containing `pattern`.
///
/// Per LOGICAL line: `capture-pane` already joins soft-wrapped rows, so splitting on `\n`
/// yields logical lines. "Last" means last **in the captured text**, not necessarily newest
/// in wall-clock terms — cursor movement can rewrite an earlier row after a later one — and
/// the first line may be PARTIAL when a capture budget cut a soft-wrapped line in half.
fn last_match<'a>(text: &'a str, pattern: &str) -> Option<&'a str> {
    text.lines().rfind(|l| l.contains(pattern))
}

/// Parse `wait-output`'s arguments. `Err(code)` is the process exit code to use.
fn parse_wait_args(rest: &[&String]) -> Result<WaitArgs, i32> {
    let mut target: Option<String> = None;
    let mut index: Option<usize> = None;
    let mut active = false;
    let mut positional: Vec<String> = Vec::new();
    let mut timeout_secs: u64 = 300;
    let mut lines: usize = 200;
    let mut interval_ms: u64 = 250;

    // Index-based rather than an iterator: each flag consumes the NEXT argument, and a
    // borrowed iterator cannot be handed to a helper closure without naming its type.
    let mut i = 1;
    let num = |what: &str, v: Option<&&String>| -> Result<u64, i32> {
        match v.map(|v| v.parse::<u64>()) {
            Some(Ok(n)) => Ok(n),
            _ => {
                eprintln!("comux wait-output: {what} needs a number");
                Err(2)
            }
        }
    };
    while i < rest.len() {
        let a = rest[i].as_str();
        // `--` ends flag parsing, so a pattern that starts with `-` can be waited on.
        if a == "--" {
            positional.extend(rest[i + 1..].iter().map(|s| s.to_string()));
            break;
        }
        let mut takes_value = true;
        match a {
            "--timeout" => timeout_secs = num(a, rest.get(i + 1))?,
            "--lines" | "-S" => lines = num(a, rest.get(i + 1))? as usize,
            "--interval" => interval_ms = num(a, rest.get(i + 1))?,
            "--index" => index = Some(num(a, rest.get(i + 1))? as usize),
            _ => {
                takes_value = false;
                match a {
                    "--json" => {}
                    "--active" => active = true,
                    "-h" | "--help" => {
                        println!("{WAIT_USAGE}");
                        return Err(0);
                    }
                    other if other.starts_with('-') => {
                        eprintln!("comux wait-output: unknown flag {other}\n{WAIT_USAGE}");
                        return Err(2);
                    }
                    other => positional.push(other.to_string()),
                }
            }
        }
        i += if takes_value { 2 } else { 1 };
    }

    // `<target> <pattern>`, or just `<pattern>`.
    let pattern = match positional.len() {
        1 => positional.remove(0),
        2 => {
            target = Some(positional.remove(0));
            positional.remove(0)
        }
        _ => {
            eprintln!("{WAIT_USAGE}");
            return Err(2);
        }
    };
    if pattern.is_empty() {
        eprintln!("comux wait-output: the pattern must not be empty");
        return Err(2);
    }
    // Matching is per logical line, so a newline in the pattern could never match.
    if pattern.contains('\n') {
        eprintln!(
            "comux wait-output: the pattern must not contain a newline (matching is per line)"
        );
        return Err(2);
    }
    if target.is_some() && index.is_some() {
        eprintln!("comux wait-output: give a target or --index, not both");
        return Err(2);
    }
    if lines == 0 {
        eprintln!("comux wait-output: --lines must be at least 1");
        return Err(2);
    }
    let interval = Duration::from_millis(interval_ms);
    if interval < MIN_INTERVAL {
        eprintln!(
            "comux wait-output: --interval must be at least {}ms",
            MIN_INTERVAL.as_millis()
        );
        return Err(2);
    }
    // `Instant + Duration` PANICS on overflow, so a huge `--timeout` would abort with 101
    // instead of a usage error. Reject anything the clock cannot represent.
    let timeout = match timeout_secs {
        0 => None, // explicit "wait forever"
        n => {
            let d = Duration::from_secs(n);
            if Instant::now().checked_add(d).is_none() {
                eprintln!("comux wait-output: --timeout {n} is too large to represent");
                return Err(2);
            }
            Some(d)
        }
    };
    let caller = (index.is_some() && !active).then(caller_pane).flatten();
    Ok(WaitArgs {
        target,
        index,
        caller,
        pattern,
        timeout,
        lines,
        interval,
    })
}

/// Run `wait-output`: poll `capture-pane` until the pattern shows up, the deadline passes,
/// or the pane goes away.
///
/// **Best-effort by construction.** It matches the pane's CURRENT text; it is not a
/// guarantee that no matching output was missed. `\r` overwrites, `\x1b[2K` line erasure
/// and alternate-screen redraws can all remove a match without producing a single new row,
/// and the alternate screen has no scrollback to fall back on. Raising `--lines` does not
/// change that. A caller needing a guarantee must write a durable marker somewhere the
/// caller itself can check.
///
/// The pane is PINNED to the token the first response echoes. A default / `--index` target
/// re-resolves against the live focus and active tab on every request, so without pinning a
/// user switching panes mid-wait could make the wait succeed on an unrelated pane.
/// How a wait ended. Printed ONCE by [`WaitOutcome::finish`], as text or — under `--json` —
/// as one JSON object, so a caller parsing stdout gets an answer for every ending, timeouts
/// included.
enum WaitOutcome {
    Matched {
        human: String,
        json: serde_json::Value,
    },
    TimedOut,
    Failed {
        code: String,
        msg: String,
    },
}

impl WaitOutcome {
    fn failed(code: &str, msg: impl Into<String>) -> Self {
        WaitOutcome::Failed {
            code: code.to_string(),
            msg: msg.into(),
        }
    }

    fn from_resp(resp: &Resp) -> Self {
        Self::failed(
            resp.code.as_deref().unwrap_or("error"),
            resp.error.as_deref().unwrap_or("(unspecified)"),
        )
    }

    /// Print and return the exit code: 0 matched, [`EXIT_TIMEOUT`] timed out, 1 failed.
    fn finish(self, verb: &str, json: bool) -> i32 {
        match self {
            WaitOutcome::Matched { human, json: j } => {
                if json {
                    println!("{j}");
                } else {
                    println!("{human}");
                }
                0
            }
            WaitOutcome::TimedOut => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "ok": false, "code": "timeout", "error": "timed out" })
                    );
                }
                EXIT_TIMEOUT
            }
            WaitOutcome::Failed { code, msg } => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "ok": false, "code": code, "error": msg })
                    );
                }
                eprintln!("comux {verb}: {msg}");
                1
            }
        }
    }
}

/// How long a plain (non-waiting) `inbox` gives the server.
const INBOX_IO_BUDGET: Duration = Duration::from_secs(10);

/// `comux inbox`: read a pane's messages, print them, and only then acknowledge them, so a
/// reader that dies or times out before printing loses nothing (the server keeps every
/// message until it is acknowledged). The cost is at-least-once delivery: a reader killed
/// between printing and the ack sees the same messages again — `seq` deduplicates them.
///
/// `--wait` polls from here, never inside the server (a server-side wait would stall its
/// single-writer loop), under one deadline that also bounds the capability check. With
/// `--wait`, `timeout` `None` waits forever; without it the short I/O budget applies.
fn run_inbox(pane: &str, wait: bool, timeout: Option<Duration>, peek: bool, json: bool) -> i32 {
    let deadline = if wait {
        timeout.map(|t| Instant::now() + t)
    } else {
        Some(Instant::now() + INBOX_IO_BUDGET)
    };
    let fail = |code: &str, msg: String| WaitOutcome::failed(code, msg).finish("inbox", json);
    let mut conn = match WaitConn::connect(deadline) {
        Ok(c) => c,
        Err(WaitErr::TimedOut) => return WaitOutcome::TimedOut.finish("inbox", json),
        Err(WaitErr::Failed(e)) => return fail("unavailable", e),
    };
    let caps = match conn.request(&Req::Health, deadline) {
        Ok(r) => r.health.map(|h| h.caps).unwrap_or_default(),
        Err(WaitErr::TimedOut) => return WaitOutcome::TimedOut.finish("inbox", json),
        Err(WaitErr::Failed(e)) => return fail("unavailable", e),
    };
    if !caps.iter().any(|c| c == CAP_INBOX) {
        return fail(
            "unsupported",
            format!(
                "the running server predates this operation ({CAP_INBOX}) — \
                 `comux server restart` after upgrading"
            ),
        );
    }
    let read = Req::Inbox {
        pane: pane.to_string(),
        ack: None,
    };
    let resp = loop {
        let resp = match conn.request(&read, deadline) {
            Ok(r) => r,
            Err(WaitErr::TimedOut) => return WaitOutcome::TimedOut.finish("inbox", json),
            Err(WaitErr::Failed(e)) => return fail("unavailable", e),
        };
        if !resp.ok {
            return WaitOutcome::from_resp(&resp).finish("inbox", json);
        }
        if !wait || resp.messages.as_ref().is_some_and(|m| !m.is_empty()) {
            break resp;
        }
        let nap = match deadline {
            Some(d) => Duration::from_millis(250).min(d.saturating_duration_since(Instant::now())),
            None => Duration::from_millis(250),
        };
        if nap.is_zero() {
            return WaitOutcome::TimedOut.finish("inbox", json);
        }
        std::thread::sleep(nap);
    };
    let messages = resp.messages.unwrap_or_default();
    let dropped = resp.dropped.unwrap_or(0);
    // A response carries one size-bounded batch; the rest wait for the next read.
    let more = resp.truncated == Some(true);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "pane": resp.pane,
                "messages": messages,
                "dropped": dropped,
                "more": more,
            })
        );
    } else {
        if dropped > 0 {
            println!("({dropped} older message(s) dropped: the inbox was full)");
        }
        for m in &messages {
            println!("{}", render_inbox(m));
        }
        if more {
            println!("(more messages waiting: run `comux inbox` again)");
        }
    }
    // Acknowledge only what was printed — and only once it is out of our buffer.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    if let Some(last) = messages.last().map(|m| m.seq).filter(|_| !peek) {
        let ack = Req::Inbox {
            pane: pane.to_string(),
            ack: Some(last),
        };
        // A failed ack only means the same messages come back next time.
        let _ = conn.request(&ack, Some(Instant::now() + INBOX_IO_BUDGET));
    }
    0
}

/// One inbox message for a human. Every field but `seq` is the sender's, so control
/// characters are neutralized before they reach the terminal.
fn render_inbox(m: &InboxMsg) -> String {
    let from = m.from.as_deref().unwrap_or("-");
    let mut line = format!(
        "[{}] {} from {}: {}",
        m.seq,
        crate::agentlog::inert(&m.kind, false),
        crate::agentlog::inert(from, false),
        crate::agentlog::inert(&m.body, true)
    );
    if let Some(r) = &m.result {
        line.push_str(&format!(
            "\n    result: {}",
            crate::agentlog::inert(r, false)
        ));
    }
    line
}

/// `comux agent-log`: find the agent's transcript through the server, then read its tail here.
fn agent_log(pane: &str, last: usize, max_chars: usize) -> WaitOutcome {
    let resp = match round_trip(&Req::ListAgents {
        target: Some(pane.to_string()),
    }) {
        Ok(r) => r,
        Err(e) => return WaitOutcome::failed("unavailable", e),
    };
    if !resp.ok {
        return WaitOutcome::from_resp(&resp);
    }
    let Some(agent) = resp.agents.unwrap_or_default().into_iter().next() else {
        return WaitOutcome::failed("not_found", format!("no agent is running in pane '{pane}'"));
    };
    let Some(path) = agent.transcript_path.clone() else {
        return WaitOutcome::failed(
            "unavailable",
            format!(
                "comux has not found {}'s log yet (it can lag a launch by a sweep, a Codex                  `/new` by ~30s)",
                agent.tool
            ),
        );
    };
    let log = match crate::agentlog::read(Path::new(&path), &agent.tool, last, max_chars) {
        Ok(l) => l,
        Err(e) => return WaitOutcome::failed("error", e),
    };
    let mut human: Vec<String> = log.entries.iter().map(crate::agentlog::render).collect();
    if human.is_empty() {
        human.push(format!("(nothing readable in the end of {path})"));
    }
    WaitOutcome::Matched {
        human: human.join("\n"),
        json: serde_json::json!({
            "ok": true,
            "pane": agent.token,
            "tool": agent.tool,
            "agent_session_id": agent.agent_session_id,
            "transcript_path": path,
            "partial": log.partial,
            "entries": log.entries,
        }),
    }
}

fn run_wait_output(args: WaitArgs, json: bool) -> i32 {
    wait_output(args).finish("wait-output", json)
}

fn wait_output(args: WaitArgs) -> WaitOutcome {
    let deadline = args.timeout.map(|t| Instant::now() + t);
    let expired = |d: Option<Instant>| d.is_some_and(|d| Instant::now() >= d);

    let mut conn = match WaitConn::connect(deadline) {
        Ok(c) => c,
        Err(WaitErr::TimedOut) => return WaitOutcome::TimedOut,
        Err(WaitErr::Failed(e)) => return WaitOutcome::failed("unavailable", e),
    };

    // Pinned after the FIRST response, whatever the caller passed. Pinning a defaulted
    // target stops a mid-wait focus change retargeting the wait; pinning an EXPLICIT one
    // matters too, because a raw terminal id can be reused by a different pane across a
    // server restart while a pane token is incarnation-qualified.
    // Checked on the wait's own connection so the deadline bounds it: an older server would
    // count the index in the active tab, not the caller's.
    if args.caller.is_some() {
        let resp = match conn.request(&Req::Health, deadline) {
            Ok(r) => r,
            Err(WaitErr::TimedOut) => return WaitOutcome::TimedOut,
            Err(WaitErr::Failed(e)) => return WaitOutcome::failed("unavailable", e),
        };
        let caps = resp.health.map(|h| h.caps).unwrap_or_default();
        if !caps.iter().any(|c| c == CAP_CALLER_INDEX) {
            return WaitOutcome::failed(
                "unsupported",
                format!(
                    "the running server predates this operation ({CAP_CALLER_INDEX}) — \
                     `comux server restart` after upgrading, or pass --active"
                ),
            );
        }
    }

    let mut pinned: Option<String> = None;
    loop {
        if expired(deadline) {
            return WaitOutcome::TimedOut;
        }
        let first = pinned.is_none();
        let req = Req::CapturePane {
            target: pinned.clone().or_else(|| args.target.clone()),
            index: first.then_some(args.index).flatten(),
            lines: Some(args.lines),
            caller: first.then(|| args.caller.clone()).flatten(),
        };
        let resp = match conn.request(&req, deadline) {
            Ok(r) => r,
            Err(WaitErr::TimedOut) => return WaitOutcome::TimedOut,
            Err(WaitErr::Failed(e)) => return WaitOutcome::failed("unavailable", e),
        };
        if !resp.ok {
            return WaitOutcome::from_resp(&resp);
        }
        if pinned.is_none() {
            // A pane with no token cannot be re-addressed, so the wait would silently
            // follow the focus — refuse rather than guess.
            match resp.pane.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => pinned = Some(t.to_string()),
                None => {
                    return WaitOutcome::failed(
                        "invalid",
                        "the pane has no identity to pin to \
                         (spawned before pane tokens existed); pass an explicit target",
                    );
                }
            }
        }
        if let Some(line) = last_match(resp.text.as_deref().unwrap_or(""), &args.pattern) {
            // Re-check before ACCEPTING: a match read after the budget ran out is a late
            // answer, and the caller has already given up on it.
            if expired(deadline) {
                return WaitOutcome::TimedOut;
            }
            return WaitOutcome::Matched {
                human: line.to_string(),
                json: serde_json::json!({
                    "ok": true,
                    "pane": pinned,
                    "line": line,
                }),
            };
        }
        // The sleep draws from the same budget, so the deadline can't be overshot by a
        // whole interval at the very end.
        let nap = match deadline {
            Some(d) => args
                .interval
                .min(d.saturating_duration_since(Instant::now())),
            None => args.interval,
        };
        if nap.is_zero() {
            return WaitOutcome::TimedOut;
        }
        std::thread::sleep(nap);
    }
}

/// Why a deadline-bounded exchange gave up.
enum WaitErr {
    TimedOut,
    Failed(String),
}

/// The remaining budget, or [`WaitErr::TimedOut`] if it is already gone. `None` = no
/// deadline at all (`--timeout 0`).
fn remaining(deadline: Option<Instant>) -> Result<Option<Duration>, WaitErr> {
    match deadline {
        None => Ok(None),
        Some(d) => {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                Err(WaitErr::TimedOut)
            } else {
                Ok(Some(left))
            }
        }
    }
}

/// ONE control connection, held open across every poll of a wait.
///
/// `server.rs::serve_ctl` reads request lines in a loop, so a single connection carries
/// arbitrarily many requests. Reusing it is not just cheaper — it means the unbounded part
/// of the exchange (`UnixStream::connect`, which has no timeout knob) happens **once**
/// instead of once per poll.
struct WaitConn {
    stream: UnixStream,
    /// Bytes read from the socket but not yet consumed as a complete line. A chunked read
    /// can overshoot the newline, and the remainder belongs to the NEXT response.
    buf: Vec<u8>,
}

impl WaitConn {
    /// Connect under the deadline.
    ///
    /// `UnixStream::connect` blocks and cannot be interrupted — on Linux it genuinely waits
    /// when the listener's backlog is full, so checking the clock on either side of it
    /// would be theatre. The connect therefore runs on a throwaway thread and the deadline
    /// bounds the *wait for it*. A thread left behind by a timeout dies with this
    /// short-lived process.
    fn connect(deadline: Option<Instant>) -> Result<Self, WaitErr> {
        let left = remaining(deadline)?;
        let path = socket_path();
        let display = path.display().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(UnixStream::connect(&path).map_err(|e| e.to_string()));
        });
        let outcome = match left {
            Some(l) => match rx.recv_timeout(l) {
                Ok(r) => r,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Err(WaitErr::TimedOut),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(WaitErr::Failed("connect thread died".into()));
                }
            },
            None => rx
                .recv()
                .map_err(|_| WaitErr::Failed("connect thread died".into()))?,
        };
        let stream = outcome.map_err(|e| {
            WaitErr::Failed(format!(
                "no running comux at {display} ({e}). Start one, or set COPAD_MUX_SOCK."
            ))
        })?;
        Ok(Self {
            stream,
            buf: Vec::new(),
        })
    }

    /// Send one request and read its response, all within `deadline`.
    fn request(&mut self, req: &Req, deadline: Option<Instant>) -> Result<Resp, WaitErr> {
        let line = serde_json::to_string(req).map_err(|e| WaitErr::Failed(e.to_string()))?;
        self.write_all_by(format!("{line}\n").as_bytes(), deadline)?;
        self.stream.flush().ok();
        let text = self.read_line_by(deadline)?;
        if text.trim().is_empty() {
            return Err(WaitErr::Failed("empty response from comux".into()));
        }
        serde_json::from_str(text.trim()).map_err(|e| WaitErr::Failed(format!("bad response: {e}")))
    }

    /// Write every byte under the deadline.
    ///
    /// NOT `write_all`: that loops over partial writes internally, and each underlying
    /// `write` would get the ORIGINAL socket timeout rather than what is left of the
    /// budget — a peer draining the socket slowly could outlast `--timeout`. A timeout
    /// here is also a DEADLINE, so it must surface as `TimedOut` (exit 124) and not as a
    /// generic failure (exit 1).
    fn write_all_by(&mut self, mut data: &[u8], deadline: Option<Instant>) -> Result<(), WaitErr> {
        while !data.is_empty() {
            self.stream
                .set_write_timeout(remaining(deadline)?)
                .map_err(|e| WaitErr::Failed(e.to_string()))?;
            match std::io::Write::write(&mut self.stream, data) {
                Ok(0) => return Err(WaitErr::Failed("comux closed the connection".into())),
                Ok(n) => data = &data[n..],
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(WaitErr::TimedOut);
                }
                Err(e) => return Err(WaitErr::Failed(e.to_string())),
            }
        }
        Ok(())
    }

    /// Read one newline-terminated line under the deadline.
    ///
    /// The socket timeout is re-armed from the REMAINING budget before every chunk read,
    /// because `set_read_timeout` bounds a single read syscall and not a whole line: a peer
    /// trickling bytes would otherwise keep a line-oriented read alive indefinitely.
    /// Chunked rather than byte-at-a-time because a capture response can reach 1 MiB, and a
    /// syscall per byte would cost seconds.
    fn read_line_by(&mut self, deadline: Option<Instant>) -> Result<String, WaitErr> {
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                return Ok(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
            }
            if self.buf.len() > MAX_RESP_BYTES {
                return Err(WaitErr::Failed("response too large".into()));
            }
            self.stream
                .set_read_timeout(remaining(deadline)?)
                .map_err(|e| WaitErr::Failed(e.to_string()))?;
            let mut chunk = [0u8; 8192];
            match std::io::Read::read(&mut self.stream, &mut chunk) {
                Ok(0) => return Err(WaitErr::Failed("comux closed the connection".into())),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(WaitErr::TimedOut);
                }
                Err(e) => return Err(WaitErr::Failed(e.to_string())),
            }
        }
    }
}

/// Hard ceiling on a single control response the wait path will buffer. `capture-pane`
/// already caps its text server-side; this guards the client against a hostile or broken
/// peer streaming forever instead of sending a newline.
const MAX_RESP_BYTES: usize = 8 << 20; // 8 MiB

#[cfg(test)]
mod wait_output_tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn parse(v: &[&str]) -> Result<WaitArgs, i32> {
        let owned = args(v);
        let refs: Vec<&String> = owned.iter().collect();
        parse_wait_args(&refs)
    }

    #[test]
    fn picks_the_last_matching_logical_line() {
        // "Last" = last in the CAPTURED TEXT. Not necessarily newest in wall-clock terms
        // (cursor movement can rewrite an earlier row after a later one), which is why the
        // contract is worded that way rather than "most recent".
        let text = "build DONE\nnoise\nrerun DONE now\ntail";
        assert_eq!(last_match(text, "DONE"), Some("rerun DONE now"));
    }

    #[test]
    fn no_match_is_none() {
        assert_eq!(last_match("alpha\nbeta", "gamma"), None);
    }

    #[test]
    fn matches_inside_a_joined_softwrap_line() {
        // `capture-pane` joins soft-wrapped rows, so a marker split across the terminal's
        // right edge arrives as ONE logical line and must still match.
        assert_eq!(last_match("aaaMARKERbbb", "MARKER"), Some("aaaMARKERbbb"));
    }

    #[test]
    fn bare_pattern_defaults_to_the_focused_pane() {
        let a = parse(&["wait-output", "READY"]).expect("should parse");
        assert_eq!(a.pattern, "READY");
        assert_eq!(a.target, None);
        assert_eq!(a.index, None);
    }

    #[test]
    fn a_leading_positional_is_the_target() {
        let a = parse(&["wait-output", "ab12-3", "READY"]).expect("should parse");
        assert_eq!(a.target.as_deref(), Some("ab12-3"));
        assert_eq!(a.pattern, "READY");
    }

    #[test]
    fn flags_do_not_get_eaten_as_positionals() {
        // The parser consumes a flag AND its value; a regression here would silently turn
        // "250" into the pattern and wait for a string that never appears.
        let a =
            parse(&["wait-output", "--timeout", "5", "--lines", "10", "OK"]).expect("should parse");
        assert_eq!(a.pattern, "OK");
        assert_eq!(a.timeout, Some(Duration::from_secs(5)));
        assert_eq!(a.lines, 10);
    }

    #[test]
    fn timeout_zero_means_wait_forever() {
        assert_eq!(
            parse(&["wait-output", "--timeout", "0", "X"])
                .unwrap()
                .timeout,
            None
        );
    }

    #[test]
    fn defaults_are_bounded() {
        // The DEFAULT must be a bounded wait: an agent blocking forever on a pattern that
        // will never appear is the failure mode this verb would otherwise introduce.
        let a = parse(&["wait-output", "X"]).unwrap();
        assert_eq!(a.timeout, Some(Duration::from_secs(300)));
        assert_eq!(a.interval, Duration::from_millis(250));
        assert_eq!(a.lines, 200);
    }

    #[test]
    fn rejects_a_busy_loop_interval() {
        assert_eq!(parse(&["wait-output", "--interval", "0", "X"]), Err(2));
        assert_eq!(parse(&["wait-output", "--interval", "10", "X"]), Err(2));
        assert!(parse(&["wait-output", "--interval", "50", "X"]).is_ok());
    }

    #[test]
    fn rejects_a_newline_in_the_pattern() {
        // Matching is per logical line, so such a pattern could never match — failing loudly
        // beats waiting out the full timeout for a reason the caller cannot see.
        assert_eq!(parse(&["wait-output", "a\nb"]), Err(2));
    }

    #[test]
    fn rejects_empty_pattern_and_missing_pattern() {
        assert_eq!(parse(&["wait-output", ""]), Err(2));
        assert_eq!(parse(&["wait-output"]), Err(2));
        assert_eq!(parse(&["wait-output", "a", "b", "c"]), Err(2));
    }

    #[test]
    fn rejects_target_and_index_together() {
        assert_eq!(
            parse(&["wait-output", "ab12-3", "X", "--index", "0"]),
            Err(2)
        );
    }

    #[test]
    fn rejects_zero_lines() {
        assert_eq!(parse(&["wait-output", "--lines", "0", "X"]), Err(2));
    }

    /// 124 is `timeout(1)`'s code and must stay distinct from the picker's cancellation
    /// code, or a shell wrapper cannot tell a deadline from a deliberate abort.
    #[test]
    fn timeout_and_cancel_codes_do_not_collide() {
        assert_ne!(EXIT_TIMEOUT, EXIT_CANCELLED);
    }
}

#[cfg(test)]
mod wait_bounds_tests {
    use super::*;

    /// A `--timeout` too large for `Instant + Duration` must be a USAGE error. Before this
    /// check it panicked (exit 101): `Instant::add` overflows rather than saturating.
    #[test]
    fn an_unrepresentable_timeout_is_refused_not_a_panic() {
        let owned: Vec<String> = ["wait-output", "--timeout", &u64::MAX.to_string(), "X"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let refs: Vec<&String> = owned.iter().collect();
        assert_eq!(parse_wait_args(&refs), Err(2));
    }

    /// A timeout a user might plausibly type must still be accepted.
    #[test]
    fn a_long_but_representable_timeout_is_accepted() {
        let owned: Vec<String> = ["wait-output", "--timeout", "86400", "X"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let refs: Vec<&String> = owned.iter().collect();
        assert_eq!(
            parse_wait_args(&refs).unwrap().timeout,
            Some(Duration::from_secs(86_400))
        );
    }

    /// The recipe is printed for a human to PASTE, so it must survive Rust's string
    /// escaping. An earlier version rendered `$'\\n'` (a literal backslash-n, which never
    /// submits the line) and `[\"panes\"]` (a Python SyntaxError). Assert the RENDERED
    /// text, not the source — that is where the bug lived.
    #[test]
    fn the_help_recipe_renders_as_runnable_shell() {
        assert!(
            !WAIT_USAGE.contains(r"\\"),
            "a double backslash survived into the rendered help — it will not run as printed"
        );
        assert!(
            !WAIT_USAGE.contains("\\\""),
            "an escaped quote survived into the rendered help"
        );
        // `send` does not append Enter, so the recipe must submit with a real newline
        // inside a $'...' word.
        assert!(
            WAIT_USAGE.contains(r"comux send 0 $'\n'"),
            "recipe never submits the command"
        );
        // The wait must address the SAME pane the recipe sent to.
        assert!(
            WAIT_USAGE.contains("comux send 0 ") && WAIT_USAGE.contains("wait-output --index 0"),
            "recipe sends to one pane and waits on another"
        );
    }

    /// `remaining` is what every read/write arms its socket timeout from, so an expired
    /// deadline must report TimedOut instead of handing back a zero timeout — a zero
    /// `set_read_timeout` means "block forever" to the OS, which is the opposite.
    #[test]
    fn an_expired_deadline_never_yields_a_zero_timeout() {
        let past = Instant::now() - Duration::from_secs(1);
        assert!(matches!(remaining(Some(past)), Err(WaitErr::TimedOut)));
        match remaining(Some(Instant::now() + Duration::from_secs(5))) {
            Ok(Some(d)) => assert!(!d.is_zero()),
            other => panic!("expected a live budget, got {:?}", other.is_ok()),
        }
        assert!(matches!(remaining(None), Ok(None)));
    }

    /// The documented recipe must not contain the assembled marker: the whole point is that
    /// the shell's echo of the command cannot satisfy the wait.
    #[test]
    fn the_help_recipe_does_not_leak_the_assembled_marker() {
        assert!(
            WAIT_USAGE.contains("MARK-%s"),
            "recipe lost its fragment form"
        );
        assert!(
            !WAIT_USAGE.contains("printf 'MARK-$id"),
            "the recipe assembles the marker inside the sent command"
        );
        assert!(
            WAIT_USAGE.contains("BEST-EFFORT"),
            "help hides the limitation"
        );
    }
}

// ---------------------------------------------------------------------------
// `wait-agent` — block until an agent pane reaches a status (WU3, decision #105).
// ---------------------------------------------------------------------------

/// Statuses a WAIT may ask for. `idle` is deliberately absent: per `agentstate.rs` it means
/// "no recognized UI", which is also what an unresolved reading looks like — waiting on it
/// is waiting on an ambiguity. It still APPEARS in `list-agents`, with that meaning.
const WAITABLE_STATUSES: &[&str] = &["working", "ready", "blocked"];

const WAIT_AGENT_USAGE: &str = "usage: comux wait-agent <pane-token|terminal-id> \
     --status working|ready|blocked [--after-seq N] [--timeout S] [--interval MS] [--json]\n\
     \n\
     Without --after-seq this is a CURRENT-STATE wait: it returns as soon as the agent is IN\n\
     that status, including one it was already in before the call — so right after sending a\n\
     prompt, `--status ready` can match the PREVIOUS turn.\n\
     \n\
     EDGE-TRIGGERED with --after-seq: every observed status change gets a new `seq` (shown\n\
     by `list-agents --json` and by every wait result). Read it BEFORE you send the prompt,\n\
     then `--after-seq <that>` only matches a status reached by a LATER change:\n\
     \n\
     \x20 seq=$(comux list-agents --json | jq --arg p \"$pane\" '.agents[]|select(.token==$p).seq')\n\
     \x20 comux send \"$pane\" \"run the tests\" && comux send \"$pane\" $'\\n'\n\
     \x20 comux wait-agent \"$pane\" --status ready --after-seq \"$seq\"\n\
     \n\
     Even so it is NOT turn-completion detection: a change that starts AND ends between two\n\
     sweeps is never observed, so a turn shorter than the sweep can be missed (the wait then\n\
     times out; it never matches the old turn). An agent replaced in the same pane counts as\n\
     a change. --after-seq needs a pane TOKEN.\n\
     \n\
     The status is CACHED and INFERRED: Claude's comes from ~/.claude/sessions/<pid>.json,\n\
     everything else from matching substrings on the pane's screen, so ordinary output can\n\
     read as `blocked`. It refreshes every 500ms while a client is attached and every 5s\n\
     while detached, so a shorter --interval cannot recover a transition that happened\n\
     between sweeps.\n\
     \n\
     The target is a PANE, not an agent invocation: an agent that exits and is replaced in\n\
     the same pane is indistinguishable. A pane that does not exist fails at once; a pane\n\
     that exists but is not classified as an agent YET is waited on, since classification\n\
     lags a launch by up to one sweep.\n\
     \n\
     For real turn completion, have the agent's own hook run `comux notify`, or print a\n\
     fresh marker and use `comux wait-output`.";

/// Everything `wait-agent` parsed off the command line.
#[derive(Debug, PartialEq)]
struct WaitAgentArgs {
    target: String,
    status: String,
    timeout: Option<Duration>,
    interval: Duration,
    /// `--after-seq N`: match only a status reached by a transition NEWER than `N`.
    after_seq: Option<u64>,
}

/// Parse `wait-agent`'s arguments. `Err(code)` is the process exit code.
fn parse_wait_agent_args(rest: &[&String]) -> Result<WaitAgentArgs, i32> {
    let mut target: Option<String> = None;
    let mut status: Option<String> = None;
    let mut timeout_secs: u64 = 300;
    let mut interval_ms: u64 = 250;
    let mut after_seq: Option<u64> = None;
    let mut i = 1;
    while i < rest.len() {
        let a = rest[i].as_str();
        // `--` ends flag parsing: what follows is the target.
        if a == "--" {
            for t in &rest[i + 1..] {
                if target.replace(t.to_string()).is_some() {
                    eprintln!("comux wait-agent: give exactly one target\n{WAIT_AGENT_USAGE}");
                    return Err(2);
                }
            }
            break;
        }
        let mut takes_value = true;
        match a {
            "--status" => match rest.get(i + 1) {
                Some(v) => status = Some((*v).to_string()),
                None => {
                    eprintln!("comux wait-agent: --status needs a value");
                    return Err(2);
                }
            },
            "--timeout" | "--interval" | "--after-seq" => {
                let n = match rest.get(i + 1).map(|v| v.parse::<u64>()) {
                    Some(Ok(n)) => n,
                    _ => {
                        eprintln!("comux wait-agent: {a} needs a number");
                        return Err(2);
                    }
                };
                match a {
                    "--timeout" => timeout_secs = n,
                    "--interval" => interval_ms = n,
                    _ => after_seq = Some(n),
                }
            }
            _ => {
                takes_value = false;
                match a {
                    "--json" => {}
                    "-h" | "--help" => {
                        println!("{WAIT_AGENT_USAGE}");
                        return Err(0);
                    }
                    other if other.starts_with('-') => {
                        eprintln!("comux wait-agent: unknown flag {other}\n{WAIT_AGENT_USAGE}");
                        return Err(2);
                    }
                    other if target.is_none() => target = Some(other.to_string()),
                    _ => {
                        eprintln!("comux wait-agent: give exactly one target\n{WAIT_AGENT_USAGE}");
                        return Err(2);
                    }
                }
            }
        }
        i += if takes_value { 2 } else { 1 };
    }
    // No focused-pane default, unlike `wait-output`: there is usually more than one agent,
    // and silently waiting on whichever pane happens to be focused is a wrong answer that
    // looks like a right one.
    let Some(target) = target else {
        eprintln!("{WAIT_AGENT_USAGE}");
        return Err(2);
    };
    let Some(status) = status else {
        eprintln!("comux wait-agent: --status is required\n{WAIT_AGENT_USAGE}");
        return Err(2);
    };
    if !WAITABLE_STATUSES.contains(&status.as_str()) {
        // Failing loudly beats waiting out the whole timeout for a value that can never
        // appear — which is what `--status done` or `--status idle` would otherwise do.
        eprintln!(
            "comux wait-agent: cannot wait for '{status}' (try {})",
            WAITABLE_STATUSES.join(", ")
        );
        return Err(2);
    }
    // A sequence number is only meaningful on the server that issued it. A pane TOKEN is
    // incarnation-qualified, so after a restart it stops resolving and the wait fails; a raw
    // terminal id would resolve to whatever pane now holds it and wait forever on a counter
    // that restarted from zero.
    if after_seq.is_some() && !matches!(classify_pane_arg(&target), Some(PaneArg::Token(_))) {
        eprintln!(
            "comux wait-agent: --after-seq needs a pane TOKEN target, not an index or terminal id"
        );
        return Err(2);
    }
    let interval = Duration::from_millis(interval_ms);
    if interval < MIN_INTERVAL {
        eprintln!(
            "comux wait-agent: --interval must be at least {}ms",
            MIN_INTERVAL.as_millis()
        );
        return Err(2);
    }
    let timeout = match timeout_secs {
        0 => None,
        n => {
            let d = Duration::from_secs(n);
            if Instant::now().checked_add(d).is_none() {
                eprintln!("comux wait-agent: --timeout {n} is too large to represent");
                return Err(2);
            }
            Some(d)
        }
    };
    Ok(WaitAgentArgs {
        target,
        status,
        timeout,
        interval,
        after_seq,
    })
}

/// Run `wait-agent`. Shares [`WaitConn`] and the deadline machinery with `wait-output`.
fn run_wait_agent(args: WaitAgentArgs, json: bool) -> i32 {
    wait_agent(args).finish("wait-agent", json)
}

fn wait_agent(args: WaitAgentArgs) -> WaitOutcome {
    let deadline = args.timeout.map(|t| Instant::now() + t);
    let expired = |d: Option<Instant>| d.is_some_and(|d| Instant::now() >= d);
    let mut conn = match WaitConn::connect(deadline) {
        Ok(c) => c,
        Err(WaitErr::TimedOut) => return WaitOutcome::TimedOut,
        Err(WaitErr::Failed(e)) => return WaitOutcome::failed("unavailable", e),
    };
    let req = Req::ListAgents {
        target: Some(args.target.clone()),
    };
    loop {
        if expired(deadline) {
            return WaitOutcome::TimedOut;
        }
        let resp = match conn.request(&req, deadline) {
            Ok(r) => r,
            Err(WaitErr::TimedOut) => return WaitOutcome::TimedOut,
            Err(WaitErr::Failed(e)) => return WaitOutcome::failed("unavailable", e),
        };
        if !resp.ok {
            // The pane is GONE — the server resolves the target every request, so this also
            // catches a pane that closed mid-wait.
            return WaitOutcome::from_resp(&resp);
        }
        // An empty listing means the pane exists but is not classified as an agent yet (or
        // any more). Keep waiting: classification lags a launch by up to one sweep.
        if let Some(hit) = resp
            .agents
            .unwrap_or_default()
            .into_iter()
            .find(|a| a.status == args.status && args.after_seq.is_none_or(|n| a.seq > n))
        {
            if expired(deadline) {
                return WaitOutcome::TimedOut;
            }
            return WaitOutcome::Matched {
                human: format!(
                    "{} {} {}s seq={}",
                    hit.status, hit.tool, hit.for_secs, hit.seq
                ),
                json: serde_json::json!({ "ok": true, "agent": hit }),
            };
        }
        let nap = match deadline {
            Some(d) => args
                .interval
                .min(d.saturating_duration_since(Instant::now())),
            None => args.interval,
        };
        if nap.is_zero() {
            return WaitOutcome::TimedOut;
        }
        std::thread::sleep(nap);
    }
}

#[cfg(test)]
mod wait_agent_tests {
    use super::*;

    fn parse(v: &[&str]) -> Result<WaitAgentArgs, i32> {
        let owned: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        let refs: Vec<&String> = owned.iter().collect();
        parse_wait_agent_args(&refs)
    }

    #[test]
    fn after_seq_is_parsed_and_needs_a_token() {
        let a = parse(&[
            "wait-agent",
            "ab12-3",
            "--status",
            "ready",
            "--after-seq",
            "9",
        ])
        .unwrap();
        assert_eq!(a.after_seq, Some(9));
        assert_eq!(
            parse(&[
                "wait-agent",
                "term3",
                "--status",
                "ready",
                "--after-seq",
                "9"
            ]),
            Err(2),
            "a terminal id can outlive the server that numbered the change"
        );
        assert_eq!(
            parse(&[
                "wait-agent",
                "ab12-3",
                "--status",
                "ready",
                "--after-seq",
                "x"
            ]),
            Err(2)
        );
    }

    #[test]
    fn requires_a_target_and_a_status() {
        // No focused-pane default here, unlike `wait-output`: with several agents running,
        // waiting on whichever pane happens to be focused is a wrong answer that looks right.
        assert_eq!(parse(&["wait-agent", "--status", "ready"]), Err(2));
        assert_eq!(parse(&["wait-agent", "ab12-3"]), Err(2));
        assert!(parse(&["wait-agent", "ab12-3", "--status", "ready"]).is_ok());
    }

    #[test]
    fn refuses_a_status_that_can_never_arrive() {
        // Waiting out a 300s timeout for a value the server will never report is the
        // failure this check exists to prevent.
        assert_eq!(parse(&["wait-agent", "p", "--status", "done"]), Err(2));
        assert_eq!(parse(&["wait-agent", "p", "--status", "Ready"]), Err(2));
    }

    /// `idle` means "no recognized UI" (agentstate.rs), which is also what an unresolved
    /// reading looks like — so it is listable but not waitable.
    #[test]
    fn idle_is_listable_but_not_waitable() {
        assert_eq!(parse(&["wait-agent", "p", "--status", "idle"]), Err(2));
        assert!(!WAITABLE_STATUSES.contains(&"idle"));
    }

    #[test]
    fn rejects_a_second_target() {
        assert_eq!(
            parse(&["wait-agent", "a", "b", "--status", "ready"]),
            Err(2)
        );
    }

    #[test]
    fn shares_the_interval_floor_and_timeout_bounds() {
        assert_eq!(
            parse(&["wait-agent", "p", "--status", "ready", "--interval", "10"]),
            Err(2)
        );
        assert_eq!(
            parse(&[
                "wait-agent",
                "p",
                "--status",
                "ready",
                "--timeout",
                "18446744073709551615"
            ]),
            Err(2)
        );
        let a = parse(&["wait-agent", "p", "--status", "blocked", "--timeout", "5"]).unwrap();
        assert_eq!(a.timeout, Some(Duration::from_secs(5)));
        assert_eq!(a.target, "p");
        assert_eq!(a.status, "blocked");
    }

    #[test]
    fn flag_values_are_not_mistaken_for_the_target() {
        let a = parse(&[
            "wait-agent",
            "--timeout",
            "9",
            "--status",
            "ready",
            "pane-1",
        ])
        .unwrap();
        assert_eq!(a.target, "pane-1");
        assert_eq!(a.timeout, Some(Duration::from_secs(9)));
    }

    /// The help must not sell turn-completion detection, which this verb cannot do, and
    /// must name the inference the status rests on.
    #[test]
    fn the_help_states_what_it_cannot_do() {
        assert!(WAIT_AGENT_USAGE.contains("NOT turn-completion detection"));
        assert!(WAIT_AGENT_USAGE.contains("CACHED and INFERRED"));
        assert!(
            WAIT_AGENT_USAGE.contains("5s"),
            "detached sweep rate missing"
        );
        assert!(
            WAIT_AGENT_USAGE.contains("comux notify")
                && WAIT_AGENT_USAGE.contains("comux wait-output"),
            "help should point at what DOES detect completion"
        );
    }
}

#[cfg(test)]
mod list_agents_proto_tests {
    use super::*;

    fn info(status: &str) -> AgentInfo {
        AgentInfo {
            token: "ab12-3".into(),
            terminal: "t1".into(),
            space: "work".into(),
            space_id: "s1".into(),
            title: "tab 1".into(),
            tool: "claude".into(),
            status: status.into(),
            for_secs: 42,
            seq: 7,
            detail: None,
            tab_id: "s1:t0".into(),
            tab_name: String::new(),
            tab_index: 0,
            pane_index: 0,
            agent_session_id: None,
            transcript_path: None,
        }
    }

    #[test]
    fn list_tree_parses_with_and_without_a_session() {
        assert!(matches!(
            serde_json::from_str::<Req>(r#"{"cmd":"list-tree"}"#).unwrap(),
            Req::ListTree {
                session: None,
                owner: None
            }
        ));
        assert_eq!(
            serde_json::to_string(&Req::ListTree {
                session: Some("name:hub".into()),
                owner: None,
            })
            .unwrap(),
            r#"{"cmd":"list-tree","session":"name:hub"}"#
        );
    }

    #[test]
    fn an_old_agent_listing_without_location_still_parses() {
        let old = r#"{"token":"a-1","terminal":"t1","status":"ready","for_secs":3}"#;
        let a: AgentInfo = serde_json::from_str(old).unwrap();
        assert_eq!((a.tab_id.as_str(), a.pane_index), ("", 0));
    }

    #[test]
    fn minimal_request_parses_without_a_target() {
        match serde_json::from_str::<Req>(r#"{"cmd":"list-agents"}"#).expect("must parse") {
            Req::ListAgents { target } => assert_eq!(target, None),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn a_targeted_request_round_trips() {
        let req = Req::ListAgents {
            target: Some("ab12-3".into()),
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains(r#""cmd":"list-agents""#), "wire form: {line}");
        match serde_json::from_str::<Req>(&line).unwrap() {
            Req::ListAgents { target } => assert_eq!(target.as_deref(), Some("ab12-3")),
            other => panic!("wrong round-trip: {other:?}"),
        }
    }

    /// `Some([])` and `None` mean different things — "no agents" versus "this response is
    /// not a listing" — and `wait-agent` branches on exactly that, so the distinction has to
    /// survive the wire.
    #[test]
    fn an_empty_listing_is_not_a_missing_listing() {
        let empty = serde_json::to_string(&Resp::agents(vec![])).unwrap();
        assert!(empty.contains(r#""agents":[]"#), "wire form: {empty}");
        let back: Resp = serde_json::from_str(&empty).unwrap();
        assert_eq!(back.agents, Some(vec![]));

        let other = serde_json::to_string(&Resp::ok()).unwrap();
        assert!(!other.contains("agents"), "agents leaked into: {other}");
        assert_eq!(
            serde_json::from_str::<Resp>(&other).unwrap().agents,
            None,
            "a non-listing response must not look like an empty listing"
        );
    }

    #[test]
    fn agent_info_round_trips() {
        let line = serde_json::to_string(&Resp::agents(vec![info("blocked")])).unwrap();
        let back: Resp = serde_json::from_str(&line).unwrap();
        assert_eq!(back.agents, Some(vec![info("blocked")]));
    }
}

#[cfg(test)]
mod skill_tests {

    use crate::copadlink::FocusOutcome;

    #[test]
    fn a_copad_that_did_not_focus_must_not_suppress_the_generic_raise() {
        // The bug this guards: treating ANY reply from copad as "handled". A copad that does
        // not own the panel answers `focused: false` — a successful call that found nothing —
        // and the click must still fall through to activating the application.
        let miss = FocusOutcome {
            focused: false,
            raised: false,
        };
        assert!(super::fall_back_after_copad(miss));
        // No copad at all is the same as a copad that did not focus.
        assert!(super::fall_back_after_copad(FocusOutcome {
            focused: false,
            raised: true
        }));
        assert!(!super::fall_back_after_copad(FocusOutcome {
            focused: true,
            raised: true
        }));
    }

    #[test]
    fn a_copad_that_focused_but_could_not_raise_still_needs_the_generic_raise() {
        // GTK under Wayland: the tab is right, the window is still behind whatever was
        // frontmost. Suppressing the fallback here would trade the old bug (right window,
        // wrong tab) for a new one (right tab, invisible) — the click would look like it did
        // nothing at all.
        assert!(super::fall_back_after_copad(FocusOutcome {
            focused: true,
            raised: false
        }));
    }
    use super::*;

    /// Quoted string literals in `s`. Dispatch arms contain no escaped quotes, so a plain
    /// scan is enough and avoids a regex dependency for one test.
    fn quoted(s: &str) -> Vec<String> {
        let b = s.as_bytes();
        let (mut out, mut i) = (Vec::new(), 0);
        while i < b.len() {
            if b[i] == b'"' {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != b'"' {
                    j += 1;
                }
                if j < b.len() {
                    out.push(s[start..j].to_string());
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        out
    }

    /// A top-level `match` arm: indented EXACTLY one level inside the match body (8 spaces)
    /// and carrying a `=>`. The indentation is what excludes nested matches inside an arm's
    /// BODY — `Some("-v") | Some("down") => "down"` lives in the `split` handler, and a
    /// substring oracle happily accepts `comux down` because of it.
    fn is_top_arm(line: &str) -> bool {
        line.starts_with("        ") && !line.starts_with("         ") && line.contains("=>")
    }

    /// Every verb `comux` actually accepts, read out of the real dispatch in both files.
    ///
    /// Derived, never hand-listed: a maintained list is a superset the moment someone
    /// deletes a verb and forgets it — which is the drift this exists to catch, so the list
    /// cannot be the oracle. Nor is "the string appears somewhere in the dispatch" an oracle;
    /// that accepts `comux down`, `comux right` and `comux done`, whose names occur as
    /// argument VALUES inside arm bodies.
    fn dispatch_verbs() -> std::collections::BTreeSet<String> {
        const CTL: &str = include_str!("control.rs");
        const BIN: &str = include_str!("bin/comux.rs");

        let fail = "dispatch marker moved — fix this test, do not delete it";
        let fn_start = CTL.find("pub fn run_client").expect(fail);
        let arm_start = CTL[fn_start..]
            .find("    let req = match cmd {")
            .expect(fail)
            + fn_start;
        let arm_end = CTL[arm_start..]
            .find(r#"eprintln!("comux: unknown command"#)
            .expect(fail)
            + arm_start;

        let mut out = std::collections::BTreeSet::new();
        // Verbs short-circuited before the match (`skill`, the waits, `worktree`).
        for line in CTL[fn_start..arm_start].lines() {
            if line.contains("cmd == \"") || line.contains("== Some(\"") {
                out.extend(quoted(line));
            }
        }
        // The match arms themselves.
        for line in CTL[arm_start..arm_end].lines().filter(|l| is_top_arm(l)) {
            out.extend(quoted(line.split("=>").next().unwrap_or("")));
        }
        // Verbs the BINARY routes before ever reaching the control client.
        let bin_start = BIN.find("match args.first()").expect(fail);
        for line in BIN[bin_start..].lines().filter(|l| is_top_arm(l)) {
            out.extend(quoted(line.split("=>").next().unwrap_or("")));
        }

        // A refactor that breaks the slicing would otherwise leave an empty oracle that
        // accepts nothing — or, worse, a tiny one that accepts almost nothing while still
        // passing because the skill happens to name only common verbs.
        assert!(
            out.len() > 25,
            "the dispatch oracle found only {} verbs ({out:?}) — the extraction has drifted",
            out.len()
        );
        for expected in [
            "list",
            "send",
            "capture-pane",
            "wait-output",
            "skill",
            "worktree",
        ] {
            assert!(
                out.contains(expected),
                "the oracle missed `{expected}`, which is definitely a verb — extraction is wrong"
            );
        }
        out
    }

    /// Every `comux <verb>` the skill tells an agent to run must be a real verb.
    ///
    /// This is the test that matters for a document the model FOLLOWS: a skill naming a verb
    /// that was renamed or removed sends the agent down a path that fails at runtime, and
    /// nothing else in the build would notice.
    #[test]
    fn every_verb_the_skill_names_exists() {
        let accepted = dispatch_verbs();

        // Only fenced code blocks, and only the code half of a line: prose says "comux is a
        // terminal multiplexer" and a trailing `# … comux focuses it …` is explanation, not
        // an instruction to run.
        let mut named = Vec::new();
        let mut in_code = false;
        for line in SKILL_MD.lines() {
            if line.trim_start().starts_with("```") {
                in_code = !in_code;
                continue;
            }
            if !in_code {
                continue;
            }
            let code = match line.find(" #") {
                Some(at) => &line[..at],
                None => line,
            };
            let mut rest = code;
            while let Some(at) = rest.find("comux ") {
                rest = &rest[at + "comux ".len()..];
                let verb: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                if !verb.is_empty() {
                    named.push(verb);
                }
            }
        }
        assert!(
            named.len() > 10,
            "the extractor found almost nothing ({named:?}) — it has drifted from the skill's \
             formatting and is no longer checking anything"
        );
        for verb in &named {
            assert!(
                accepted.contains(verb),
                "the skill tells an agent to run `comux {verb}`, which no dispatch arm accepts"
            );
        }
    }

    /// The safety gate is the whole reason the skill can be handed to an agent at all: without
    /// `$COPAD_MUX` it would drive the user's real server. Keep it, and keep it near the top.
    #[test]
    fn the_skill_gates_on_being_inside_comux() {
        assert!(SKILL_MD.contains("$COPAD_MUX"), "the env gate is missing");
        let gate = SKILL_MD
            .find("If `$COPAD_MUX` is unset")
            .expect("the skill must say what to do when the gate fails");
        assert!(
            gate < SKILL_MD.len() / 3,
            "the gate must come before the commands it guards"
        );
        assert!(
            SKILL_MD.contains("frontmatter marker") || SKILL_MD.starts_with("---\n"),
            "a skill needs YAML frontmatter to be discoverable"
        );
    }

    /// The marker recipe is the one piece of the skill that is easy to get subtly wrong and
    /// impossible to notice — it fails by SUCCEEDING instantly.
    #[test]
    fn the_skill_teaches_the_fresh_fragmented_marker() {
        assert!(SKILL_MD.contains("DONE-%s"), "the marker is not fragmented");
        assert!(
            !SKILL_MD.contains("printf 'DONE-$id"),
            "the recipe assembles the marker inside the sent command the shell echoes"
        );
        assert!(
            SKILL_MD.contains(r"$'\n'"),
            "the recipe never submits the command"
        );
        assert!(
            SKILL_MD.contains("124"),
            "the skill must tell the agent a timeout is not a match"
        );
    }

    /// Indexes address panes of the SERVER's active tab, which the user can change while the
    /// agent runs. A hard-coded index is therefore never safe — and on a single-pane tab,
    /// index 0 is the AGENT ITSELF, so the recipe would have the agent type into its own
    /// session instead of running the build.
    #[test]
    fn the_skill_never_hard_codes_a_pane_index() {
        for bad in ["comux send 0 ", "comux send 1 ", "--index 0", "--index 1"] {
            assert!(
                !SKILL_MD.contains(bad),
                "the skill hard-codes a pane index (`{bad}`); indexes are active-tab-relative \
                 and index 0 can be the agent's own pane"
            );
        }
    }

    /// The pane you just created must come from the SPLIT RESPONSE, not from inference.
    ///
    /// Two earlier versions of this guide got it wrong: `panes[focused]` is racy (the user
    /// can move focus between the split and the listing), and so is set-difference on
    /// listings (a concurrent tab switch can make exactly one unfamiliar token appear).
    /// Both failures pass every downstream check, because the pane they name really is live
    /// and really is in the active tab.
    #[test]
    fn the_skill_takes_the_new_pane_from_the_split_response() {
        assert!(
            SKILL_MD.contains("comux split --from"),
            "the skill must pin the split SOURCE by identity, or the new pane inherits the \
             wrong cwd when focus moves"
        );
        assert!(
            !SKILL_MD.contains("panes[focused]") && !SKILL_MD.contains("set difference"),
            "the skill has gone back to inferring which pane was created"
        );
        assert!(
            SKILL_MD.contains("do not guess which pane appeared"),
            "the skill must say what to do when the split reports no token"
        );
    }

    /// Writes should be addressed by token. An index is a position — in the caller's tab
    /// (#137), but a pane closing or splitting there still shifts it — and a raw terminal id
    /// is recycled across server restarts; both retarget silently.
    #[test]
    fn the_skill_addresses_writes_by_token() {
        assert!(
            SKILL_MD.contains("**Still prefer tokens.**"),
            "the skill must state the preference outright"
        );
        // Matched on fragments that cannot straddle a line wrap — the prose is hard-wrapped,
        // so a longer literal silently stops matching the moment the paragraph reflows.
        assert!(
            SKILL_MD.contains("An index is a position, not a name")
                && SKILL_MD.contains("nothing warns you"),
            "the skill must admit an index can shift under the caller"
        );
        assert!(
            SKILL_MD.contains("a position in YOUR tab"),
            "the skill must say which tab an index counts in"
        );
        assert!(
            SKILL_MD.contains("Those restart from")
                && SKILL_MD.contains("address an unrelated pane later"),
            "the skill must warn that terminal ids are recycled"
        );
        assert!(
            SKILL_MD.contains("never send to your own pane"),
            "the skill must keep the self-send prohibition"
        );
        // Token addressing reaches panes the user is not looking at — say so.
        assert!(
            SKILL_MD.contains("the user may not see it happen"),
            "the skill must note that a token-addressed write is invisible to the user"
        );
    }
}

#[cfg(test)]
mod pane_arg_tests {
    use super::*;

    #[test]
    fn plain_integers_stay_indexes() {
        // Every invocation that works today must keep working identically — including the
        // leading `+` that Rust's usize parser accepts.
        for (arg, want) in [("0", 0usize), ("12", 12), ("+0", 0)] {
            match classify_pane_arg(arg) {
                Some(PaneArg::Index(i)) => assert_eq!(i, want, "{arg}"),
                other => panic!("{arg} should be an index, got {:?}", other.is_some()),
            }
        }
    }

    #[test]
    fn tokens_are_recognized() {
        match classify_pane_arg("1a2b3c-7") {
            Some(PaneArg::Token(t)) => assert_eq!(t, "1a2b3c-7"),
            _ => panic!("a minted-shape token should classify as a token"),
        }
    }

    /// A numeric string too large for `usize` must be an ERROR, not a silent fall-through to
    /// token resolution — which would report "unknown pane" and send the caller hunting for a
    /// pane that was never the problem.
    #[test]
    fn an_overflowing_index_is_refused_not_treated_as_a_token() {
        assert!(classify_pane_arg("99999999999999999999999999").is_none());
        assert!(classify_pane_arg("-1").is_none());
        assert!(classify_pane_arg("+9999999999999999999999").is_none());
    }

    /// A raw terminal id has no `-`, so it cannot be mistaken for a token — which is what
    /// keeps `send` from accepting an identity that gets recycled across server restarts.
    #[test]
    fn a_terminal_id_is_not_accepted_as_a_token() {
        assert!(classify_pane_arg("term0").is_none());
        assert!(classify_pane_arg("").is_none());
    }

    /// The classification rests on the mint's shape. If `next_pane_token` ever stops putting
    /// a `-` in, or starts producing something that parses as a `usize`, `send` would silently
    /// route tokens to the index path.
    #[test]
    fn the_mint_still_matches_what_classification_assumes() {
        let t = crate::term::next_pane_token();
        assert!(t.contains('-'), "token mint lost its separator: {t}");
        assert!(
            t.parse::<usize>().is_err(),
            "a token must never parse as an index: {t}"
        );
        match classify_pane_arg(&t) {
            Some(PaneArg::Token(got)) => assert_eq!(got, t),
            _ => panic!("a freshly minted token must classify as a token: {t}"),
        }
    }
}

#[cfg(test)]
mod send_split_proto_tests {
    use super::*;

    /// The old wire shape must still parse: a running older client keeps sending it the
    /// moment the server is upgraded, and refusing it would break every one of them.
    #[test]
    fn the_old_send_shape_still_parses() {
        let old = r#"{"cmd":"send-keys","index":0,"text":"ls"}"#;
        match serde_json::from_str::<Req>(old).expect("old send must parse") {
            Req::SendKeys {
                target,
                index,
                text,
                paste,
                enter,
                caller,
            } => {
                assert_eq!(caller, None, "an old send counts in the active tab");
                assert_eq!(target, None);
                assert_eq!(index, Some(0));
                assert_eq!(text, "ls");
                assert!(
                    !paste && !enter,
                    "an old request is neither a paste nor submitted"
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn the_token_send_shape_round_trips() {
        let req = Req::SendKeys {
            target: Some("ab12-3".into()),
            index: None,
            text: "make\n".into(),
            paste: false,
            enter: false,
            caller: None,
        };
        let line = serde_json::to_string(&req).unwrap();
        // Unset flags stay off the wire, so an older server sees exactly the shape it knows.
        assert!(!line.contains("paste") && !line.contains("enter"), "{line}");
        match serde_json::from_str::<Req>(&line).unwrap() {
            Req::SendKeys { target, index, .. } => {
                assert_eq!(target.as_deref(), Some("ab12-3"));
                assert_eq!(index, None);
            }
            other => panic!("wrong round-trip: {other:?}"),
        }
    }

    /// `from` is optional on the wire so an older client's `{"cmd":"split","dir":"right"}`
    /// still means "split the focused pane".
    #[test]
    fn the_old_split_shape_still_parses() {
        match serde_json::from_str::<Req>(r#"{"cmd":"split","dir":"right"}"#).unwrap() {
            Req::Split { dir, from, .. } => {
                assert_eq!(dir, "right");
                assert_eq!(from, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn the_split_response_carries_the_new_pane() {
        let line = serde_json::to_string(&Resp::split("ab12-4".into())).unwrap();
        let back: Resp = serde_json::from_str(&line).unwrap();
        assert!(back.ok);
        assert_eq!(back.pane.as_deref(), Some("ab12-4"));
    }
}

#[cfg(test)]
mod split_exit_tests {
    use super::*;

    /// The documented recipe is `pane=$(comux split --from "$COPAD_MUX_PANE")`. An older
    /// server answers `split` with `ok` and no token, so without this the shell captures an
    /// empty string and a `||` guard never fires — the script proceeds to address a pane
    /// that does not exist.
    #[test]
    fn a_split_response_without_a_token_is_not_a_usable_success() {
        for missing in [None, Some(String::new())] {
            let resp = Resp {
                pane: missing.clone(),
                ..Resp::ok()
            };
            assert!(
                resp.ok,
                "the server still reports success — the CLI is what must refuse it"
            );
            assert!(
                resp.pane.as_deref().unwrap_or_default().is_empty(),
                "fixture is wrong: {missing:?}"
            );
        }
        // And a real one is usable.
        assert!(
            !Resp::split("ab12-4".into())
                .pane
                .as_deref()
                .unwrap_or_default()
                .is_empty()
        );
    }
}

/// `comux machines` / `comux list-agents --fleet`, or `None` when this is not a fleet verb.
/// Returns an exit code the caller passes straight back.
fn fleet_verb(args: &[String]) -> Option<i32> {
    const MACHINES: Spec = cliargs::bare("usage: comux machines [--json]");
    const AGENTS: Spec = Spec {
        usage: "usage: comux list-agents [--fleet] [--json]",
        flags: &[flag(&["--fleet"])],
        positionals: Positionals::AtMost(0),
    };
    let (json, owned) = cliargs::take_json(args);
    let rest: Vec<&String> = owned.iter().collect();
    let machines = match rest.first().map(|s| s.as_str())? {
        "machines" => true,
        "list-agents" | "agents" => false,
        _ => return None,
    };
    let spec = if machines { &MACHINES } else { &AGENTS };
    // Parsed HERE, not only by the flat client: a fleet verb never reaches it, and `--help`
    // must not run an SSH sweep.
    let p = match verb_args(spec, &rest, json) {
        Ok(p) => p,
        Err(code) => return Some(code),
    };
    if machines {
        Some(print_machines(json))
    } else if p.has("--fleet") {
        Some(print_fleet_agents(json))
    } else {
        None
    }
}

/// The configured fleet plus any config warnings, which are printed rather than swallowed: a
/// machine dropped for a bad entry must not just be missing from the list.
fn load_machines() -> (Vec<crate::fleet::Machine>, Vec<String>) {
    let (cfg, warnings) = crate::config::MuxConfig::load();
    (cfg.machines, warnings)
}

fn print_machines(json: bool) -> i32 {
    let (machines, warnings) = load_machines();
    for w in &warnings {
        eprintln!("comux: {w}");
    }
    if json {
        let rows: Vec<serde_json::Value> = machines
            .iter()
            .map(|m| serde_json::json!({ "name": m.name, "ssh": m.ssh, "socket": m.socket }))
            .collect();
        println!("{}", serde_json::json!({ "ok": true, "machines": rows }));
        return 0;
    }
    if machines.is_empty() {
        // Not an error: one machine is the normal case. Say where they would go.
        println!("(no machines configured — add a [machines.<name>] table to mux.toml)");
        return 0;
    }
    println!("{:<16} {:<28} SOCKET", "NAME", "SSH");
    for m in &machines {
        println!(
            "{:<16} {:<28} {}",
            m.name,
            m.ssh,
            m.socket.as_deref().unwrap_or("")
        );
    }
    0
}

fn print_fleet_agents(json: bool) -> i32 {
    use crate::fleet::Reply;
    let (machines, warnings) = load_machines();
    for w in &warnings {
        eprintln!("comux: {w}");
    }
    if machines.is_empty() {
        eprintln!("comux: no machines configured — add a [machines.<name>] table to mux.toml");
        return 2;
    }
    let replies = crate::fleet::query_all(&machines, "list-agents", crate::fleet::DEFAULT_TIMEOUT);
    // An unreachable machine sets the exit code, so a script can branch on "the fleet answered
    // in full" without re-parsing output. The reachable machines are still REPORTED: a partial
    // answer beats none, as long as the gap is visible.
    let mut incomplete = false;
    if json {
        let mut rows = serde_json::Map::new();
        for (name, r) in &replies {
            rows.insert(
                name.clone(),
                match r {
                    Reply::Agents(a) => serde_json::json!({ "agents": a }),
                    Reply::Unreachable(why) => {
                        incomplete = true;
                        serde_json::json!({ "unreachable": why })
                    }
                },
            );
        }
        println!(
            "{}",
            serde_json::json!({ "ok": !incomplete, "machines": rows })
        );
        return i32::from(incomplete);
    }
    println!(
        "{:<12} {:<12} {:<10} {:<9} {:<6} DOING",
        "MACHINE", "SPACE", "TOOL", "STATUS", "FOR"
    );
    for (name, r) in &replies {
        match r {
            Reply::Agents(agents) if agents.is_empty() => println!("{name:<12} (no agents)"),
            Reply::Agents(agents) => {
                for a in agents {
                    let g = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let secs = a.get("for_secs").and_then(|v| v.as_u64()).unwrap_or(0);
                    println!(
                        "{:<12} {:<12} {:<10} {:<9} {:<6} {}",
                        name,
                        g("space"),
                        g("tool"),
                        g("status"),
                        format!("{secs}s"),
                        g("detail")
                    );
                }
            }
            Reply::Unreachable(why) => {
                incomplete = true;
                // Reported, never omitted: "no agents on build" and "could not reach build"
                // look identical in a list that drops failures, and only one needs acting on.
                println!("{name:<12} UNREACHABLE  {why}");
            }
        }
    }
    i32::from(incomplete)
}
