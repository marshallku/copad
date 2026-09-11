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

use crate::picker;

/// A control request. Wire form: one JSON object per line, tagged by `cmd`
/// (e.g. `{"cmd":"list"}`, `{"cmd":"split","dir":"right"}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Req {
    /// List panes of the active tab.
    List,
    /// Split a pane. `dir` = `"right"` (side by side) | `"down"` (stacked).
    ///
    /// `from` names the pane to split, by TOKEN, and must be in the active tab. Without it
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
    },
    /// Grow the pane at `index` toward `dir` (`left`/`right`/`up`/`down`) by nudging
    /// its split divider.
    ResizePane { index: usize, dir: String },
    /// Focus the pane at `index` (as printed by `list`).
    Focus { index: usize },
    /// Close the pane at `index`.
    Close { index: usize },
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
    },
    /// List the workspace's tabs.
    ListTabs,
    /// Create a new tab and make it active.
    NewTab,
    /// Make the tab at `index` (as printed by `list-tabs`) active.
    SelectTab { index: usize },
    /// Close the tab at `index` (as printed by `list-tabs`) and reap its shells —
    /// the `Ctrl-b &` / context-menu "close tab" action, reachable from a script.
    /// Refused when it is the session's LAST tab (a session always keeps ≥1; kill the
    /// session instead). Unlike the TUI there is no confirm — tmux `kill-window` parity.
    CloseTab { index: usize },
    /// Rename the tab at `index` (as printed by `list-tabs`), or the ACTIVE tab when
    /// `index` is `None` — so a shell inside a pane can rename its own tab without
    /// knowing its position. An empty `name` clears back to the process/index label.
    RenameTab {
        #[serde(default)]
        index: Option<usize>,
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
    },
    /// Rename the session at `index` (as printed by `list-sessions`), or the ACTIVE
    /// session when `index` is `None`. An empty `name` clears it back to the
    /// generated id.
    RenameSession {
        #[serde(default)]
        index: Option<usize>,
        name: String,
    },
    /// Make the session at `index` (as printed by `list-sessions`) active.
    SelectSession { index: usize },
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
    KillSession { index: usize },
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
    /// The session (space) the agent lives in.
    #[serde(default)]
    pub space: String,
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
    /// What the agent was last seen DOING (`Bash: run the tests`, `running: cargo test`),
    /// read from the tool's own structured log.
    ///
    /// ABSENT means "no reading", never "doing nothing" — the agent may be a tool whose log
    /// we cannot read, one that has run no tools yet, or one whose log format has moved. Do
    /// not branch on its absence; it is a hint for a human scanning a list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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
    /// `capture-pane`: the token of the pane actually read. Echoed so a caller that let
    /// the target default to the focused pane can tell WHICH pane answered — focus is a
    /// mutable UI choice, and a silently-retargeted read is indistinguishable from a
    /// correct one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

impl Resp {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
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
        Self {
            ok: false,
            error: Some(msg.into()),
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
    if let Ok(p) = std::env::var("COPAD_MUX_SOCK") {
        return PathBuf::from(p);
    }
    runtime_dir().join("sock")
}

/// The `comux ctl ...` CLI client: parse args, round-trip one request over the
/// socket, print the response. Returns a process exit code.
pub fn run_client(args: &[String]) -> i32 {
    // `worktree` has its own nested grammar (subcommands + `--from`/`--plain`/`-d`), so it
    // is parsed BEFORE the flat `--json`-stripping path below could reinterpret its flags.
    if args.first().map(|s| s.as_str()) == Some("worktree") {
        return run_worktree_client(&args[1..]);
    }

    let mut json_out = false;
    let mut rest: Vec<&String> = Vec::new();
    for a in args {
        if a == "--json" {
            json_out = true;
        } else {
            rest.push(a);
        }
    }
    let Some(cmd) = rest.first().map(|s| s.as_str()) else {
        eprintln!(
            "usage: comux <list|split|resize|focus|close|send|capture-pane|wait-output|list-agents|wait-agent|\
             list-tabs|new-tab|select-tab|\
             close-tab|rename-tab [index] <name>|list-sessions|new-session [name]|\
             rename-session [index] <name>|select-session|kill-session|\
             worktree <create|list|rm>|reload|health|kill-server> [args]"
        );
        return 2;
    };

    // `wait-output` owns its own exit codes (124 on deadline) and issues MANY requests,
    // so it short-circuits the one-request-one-response path below rather than producing a
    // `Req` for it.
    if cmd == "skill" {
        // EMBEDDED, not read from disk: the skill has to be available from an installed
        // binary with no repo checkout, and it must describe THIS build's verbs rather than
        // whatever a stale file next to it says.
        println!("{SKILL_MD}");
        return 0;
    }
    if cmd == "wait-agent" {
        return match parse_wait_agent_args(&rest) {
            Ok(args) => run_wait_agent(args),
            Err(code) => code,
        };
    }
    if cmd == "wait-output" || cmd == "wait" {
        return match parse_wait_args(&rest) {
            Ok(args) => run_wait_output(args),
            Err(code) => code,
        };
    }

    let req = match cmd {
        "list" => Req::List,
        "health" => Req::Health,
        "host" => Req::Host,
        "reload" | "source-file" => Req::ReloadConfig,
        "kill-server" => Req::KillServer,
        "list-tabs" | "tabs" => Req::ListTabs,
        "new-tab" => Req::NewTab,
        "list-sessions" | "sessions" => Req::ListSessions,
        "list-agents" | "agents" => Req::ListAgents { target: None },
        "new-session" => {
            // Optional name: everything after the verb, space-joined (tmux `new -s`).
            let name = rest
                .iter()
                .skip(1)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            Req::NewSession {
                name: (!name.trim().is_empty()).then(|| name.trim().to_string()),
                // Start the session's shell where the CLI was invoked (like `tmx $name`).
                cwd: std::env::current_dir()
                    .ok()
                    .and_then(|p| p.to_str().map(|s| s.to_string())),
            }
        }
        "rename-session" | "rename" => {
            let Some((index, name)) = parse_rename(&rest) else {
                eprintln!("usage: comux rename-session [index] <name...>   (no index = active)");
                return 2;
            };
            Req::RenameSession { index, name }
        }
        "rename-tab" => {
            let Some((index, name)) = parse_rename(&rest) else {
                eprintln!("usage: comux rename-tab [index] <name...>   (no index = active)");
                return 2;
            };
            Req::RenameTab { index, name }
        }
        // The index-taking verbs below share one shape: an omitted index opens the fuzzy
        // picker over a live listing instead of failing with a usage line (see
        // `pick_index`), while a MALFORMED index still fails — the user meant something.
        "select-session" => {
            let idx = match pick_index(rest.get(1), json_out, Target::Session) {
                Ok(i) => i,
                Err(code) => return code,
            };
            Req::SelectSession { index: idx }
        }
        "select-tab" => {
            let idx = match pick_index(rest.get(1), json_out, Target::Tab) {
                Ok(i) => i,
                Err(code) => return code,
            };
            Req::SelectTab { index: idx }
        }
        // The two destructive selection verbs. They picker on an omitted index like
        // their non-destructive siblings rather than defaulting to the ACTIVE tab /
        // session the way `rename-*` does: an explicit pick is what makes a bare
        // `comux close-tab` safe to type.
        "close-tab" | "kill-tab" => {
            let idx = match pick_index(rest.get(1), json_out, Target::TabClose) {
                Ok(i) => i,
                Err(code) => return code,
            };
            Req::CloseTab { index: idx }
        }
        "kill-session" => {
            let idx = match pick_index(rest.get(1), json_out, Target::SessionKill) {
                Ok(i) => i,
                Err(code) => return code,
            };
            Req::KillSession { index: idx }
        }
        "split" => {
            // -h/--horizontal → side by side (right); -v/--vertical → stacked (down).
            let mut dir = "right";
            let mut from: Option<String> = None;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "-v" | "--vertical" | "down" => dir = "down",
                    "-h" | "--horizontal" | "right" => dir = "right",
                    "--from" => match rest.get(i + 1) {
                        Some(v) => {
                            from = Some((*v).to_string());
                            i += 1;
                        }
                        None => {
                            eprintln!("comux split: --from needs a pane token");
                            return 2;
                        }
                    },
                    "--json" => {}
                    other => {
                        eprintln!(
                            "comux split: unexpected argument '{other}'\n\
                             usage: comux split [-v|-h] [--from <pane-token>] [--json]"
                        );
                        return 2;
                    }
                }
                i += 1;
            }
            Req::Split {
                from,
                dir: dir.to_string(),
            }
        }
        "focus" | "close" => {
            let target = if cmd == "focus" {
                Target::PaneFocus
            } else {
                Target::PaneClose
            };
            let idx = match pick_index(rest.get(1), json_out, target) {
                Ok(i) => i,
                Err(code) => return code,
            };
            if cmd == "focus" {
                Req::Focus { index: idx }
            } else {
                Req::Close { index: idx }
            }
        }
        "capture-pane" | "capture" => {
            // Addressed by identity like `jump`/`notify` rather than by picker: this verb
            // exists for scripts and agents, and an interactive picker in the middle of a
            // pipeline is the wrong affordance. A bare `comux capture-pane` still works —
            // it reads the focused pane.
            let mut target: Option<String> = None;
            let mut index: Option<usize> = None;
            let mut lines: Option<usize> = None;
            let mut it = rest.iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--json" => {}
                    "-S" | "--lines" => match it.next().map(|v| v.parse::<usize>()) {
                        Some(Ok(n)) => lines = Some(n),
                        _ => {
                            eprintln!("comux capture-pane: -S/--lines needs a number");
                            return 2;
                        }
                    },
                    "--index" => match it.next().map(|v| v.parse::<usize>()) {
                        Some(Ok(n)) => index = Some(n),
                        _ => {
                            eprintln!("comux capture-pane: --index needs a number");
                            return 2;
                        }
                    },
                    "-h" | "--help" => {
                        eprintln!("{CAPTURE_USAGE}");
                        return 2;
                    }
                    other if other.starts_with('-') => {
                        eprintln!("comux capture-pane: unknown flag {other}\n{CAPTURE_USAGE}");
                        return 2;
                    }
                    other => {
                        if target.is_some() {
                            eprintln!(
                                "comux capture-pane: give at most one target\n{CAPTURE_USAGE}"
                            );
                            return 2;
                        }
                        target = Some(other.to_string());
                    }
                }
            }
            if target.is_some() && index.is_some() {
                eprintln!("comux capture-pane: give a target or --index, not both");
                return 2;
            }
            Req::CapturePane {
                target,
                index,
                lines,
            }
        }
        "jump" => {
            // Addressed by identity, not position, so there is no index listing to fuzzy-pick
            // from when it is omitted (unlike `focus`/`select-tab`): `comux list --json`
            // prints each pane's token, and a pane's own shell carries `$COPAD_MUX_PANE`.
            let Some(target) = rest.get(1).filter(|t| !t.starts_with('-')) else {
                eprintln!(
                    "usage: comux jump <pane-token|terminal-id> [--no-raise]\n\
                     hint:  a pane's own shell has it in $COPAD_MUX_PANE; \
                     `comux list --json` prints the rest"
                );
                return 2;
            };
            Req::Jump {
                target: target.to_string(),
            }
        }
        "notify" => {
            let mut target = std::env::var("COPAD_MUX_PANE").unwrap_or_default();
            let mut kind = "done".to_string();
            let mut body = Vec::new();
            let mut it = rest.iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--pane" => match it.next() {
                        Some(v) => target = (*v).clone(),
                        None => {
                            eprintln!("comux notify: --pane needs a value");
                            return 2;
                        }
                    },
                    "--kind" => match it.next() {
                        Some(v) => kind = (*v).clone(),
                        None => {
                            eprintln!("comux notify: --kind needs a value");
                            return 2;
                        }
                    },
                    other => body.push(other.to_string()),
                }
            }
            if target.is_empty() {
                // Never fall back to the active pane: a notification attributed to the
                // wrong pane sends its click somewhere misleading.
                eprintln!(
                    "usage: comux notify [--pane <token>] [--kind done|blocked] <body...>\n\
                     comux: no pane given and $COPAD_MUX_PANE is unset (not inside a comux pane?)"
                );
                return 2;
            }
            Req::Notify {
                target,
                kind,
                body: body.join(" "),
            }
        }
        "resize" => {
            let idx = rest.get(1).and_then(|s| s.parse::<usize>().ok());
            let dir = rest.get(2).map(|s| s.as_str());
            let (Some(idx), Some(dir)) = (idx, dir) else {
                eprintln!("usage: comux resize <index> <left|right|up|down>");
                return 2;
            };
            Req::ResizePane {
                index: idx,
                dir: dir.to_string(),
            }
        }
        "send" | "send-keys" => {
            let Some(first) = rest.get(1) else {
                eprintln!("{SEND_USAGE}");
                return 2;
            };
            let (target, index) = match classify_pane_arg(first) {
                Some(PaneArg::Index(i)) => (None, Some(i)),
                Some(PaneArg::Token(t)) => (Some(t), None),
                None => {
                    eprintln!("comux send: '{first}' is neither a pane index nor a pane token");
                    return 2;
                }
            };
            let text = rest
                .iter()
                .skip(2)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            Req::SendKeys {
                target,
                index,
                text,
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
    if resp.ok { 0 } else { 1 }
}

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
    let copad_focused = match resp.copad_host.as_ref() {
        Some((sock, panel)) => crate::copadlink::focus_panel(sock, panel),
        None => false,
    };
    if !fall_back_after_copad(copad_focused) {
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
fn fall_back_after_copad(copad_focused: bool) -> bool {
    !copad_focused
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

const SEND_USAGE: &str = "usage: comux send <pane-token|index> <text...>\n\
     \x20      a TOKEN (from `comux list --json`, or a pane's own $COPAD_MUX_PANE) names one\n\
     \x20      pane anywhere in the mux; an INDEX is a position in the ACTIVE tab and\n\
     \x20      retargets when the user switches tabs. `send` does not append Enter.";

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
            Target::Session => "comux select-session [index]   (no index → fuzzy picker)",
            Target::SessionKill => "comux kill-session [index]   (no index → fuzzy picker)",
            Target::Tab => "comux select-tab [index]   (no index → fuzzy picker)",
            Target::TabClose => "comux close-tab [index]   (no index → fuzzy picker)",
            Target::PaneFocus => "comux focus [index]   (no index → fuzzy picker)",
            Target::PaneClose => "comux close [index]   (no index → fuzzy picker)",
        }
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
    fn rows(self) -> Result<Vec<(picker::Item, usize)>, i32> {
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
                let tabs = query(&Req::ListTabs)?.tabs.unwrap_or_default();
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
                let panes = query(&Req::List)?.panes.unwrap_or_default();
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
fn pick_index(arg: Option<&&String>, json: bool, target: Target) -> Result<usize, i32> {
    if let Some(a) = arg {
        return a.parse::<usize>().map_err(|_| {
            eprintln!("usage: {}", target.usage());
            2
        });
    }
    prompt_ok(json, target.usage())?;
    let rows = target.rows()?;
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
        Req::List => {
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
        Req::ListTabs => {
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
        // Message-carrying verbs (e.g. `reload`) print their outcome; everything else
        // just confirms with `ok`.
        _ => match &resp.message {
            Some(m) => println!("{m}"),
            None => println!("ok"),
        },
    }
}

/// `rename-*` CLI argument shape, shared by tabs and sessions: `rename-x <name...>`
/// targets the ACTIVE one; `rename-x <index> <name...>` targets by list index. `rest`
/// includes the verb at `[0]`. A leading integer is read as an index only when a name
/// follows it, so `rename-tab 2` names the active tab "2" rather than erroring on a
/// missing name. An explicit empty name (`rename-tab ""`) clears back to the default.
fn parse_rename(rest: &[&String]) -> Option<(Option<usize>, String)> {
    let args = &rest[1..];
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
    eprintln!(
        "usage:\n\
         \x20 comux worktree create <branch> [--from <ref>] [--no-attach] [--json]\n\
         \x20 comux worktree list [--plain|--json]\n\
         \x20 comux worktree rm [<path|branch>] [-f|--force] [-d|--delete-branch] [--json]\n\
         \n\
         omitting the `rm` target opens a fuzzy picker over the repo's worktrees."
    );
}

/// `comux worktree <sub> …` — a nested grammar parsed independently of the flat client.
fn run_worktree_client(args: &[String]) -> i32 {
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        print_worktree_usage();
        return 2;
    };
    let rest = &args[1..];
    match sub {
        "create" | "new" | "add" => worktree_create_client(rest),
        "list" | "ls" => worktree_list_client(rest),
        "rm" | "remove" => worktree_rm_client(rest),
        "help" | "-h" | "--help" => {
            print_worktree_usage();
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
        unsafe { std::env::set_var("COPAD_MUX_SOCK", &path) };
        let out = f();
        unsafe { std::env::remove_var("COPAD_MUX_SOCK") };
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

    /// Build the `&[&String]` shape `run_client` passes to `parse_rename`.
    fn rename_args(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_rename_targets_active_or_index() {
        let check = |args: &[&str], want: Option<(Option<usize>, &str)>| {
            let owned = rename_args(args);
            let refs: Vec<&String> = owned.iter().collect();
            assert_eq!(
                parse_rename(&refs),
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
            serde_json::to_string(&Req::CloseTab { index: 1 }).unwrap(),
            r#"{"cmd":"close-tab","index":1}"#
        );
        assert_eq!(
            serde_json::to_string(&Req::KillSession { index: 2 }).unwrap(),
            r#"{"cmd":"kill-session","index":2}"#
        );
        assert!(matches!(
            serde_json::from_str::<Req>(r#"{"cmd":"close-tab","index":3}"#).unwrap(),
            Req::CloseTab { index: 3 }
        ));
        assert!(matches!(
            serde_json::from_str::<Req>(r#"{"cmd":"kill-session","index":0}"#).unwrap(),
            Req::KillSession { index: 0 }
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
            Req::RenameSession { index: Some(1), ref name } if name == "api"
        ));
        let new: Req = serde_json::from_str(r#"{"cmd":"rename-tab","name":"build"}"#)
            .expect("index-less form must parse");
        assert!(matches!(
            new,
            Req::RenameTab { index: None, ref name } if name == "build"
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
            } => {
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
     \x20      [--index N] [--timeout S] [--lines N] [--interval MS]\n\
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
                    "-h" | "--help" => {
                        eprintln!("{WAIT_USAGE}");
                        return Err(2);
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
    Ok(WaitArgs {
        target,
        index,
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
fn run_wait_output(args: WaitArgs) -> i32 {
    let deadline = args.timeout.map(|t| Instant::now() + t);
    let expired = |d: Option<Instant>| d.is_some_and(|d| Instant::now() >= d);

    let mut conn = match WaitConn::connect(deadline) {
        Ok(c) => c,
        Err(WaitErr::TimedOut) => return EXIT_TIMEOUT,
        Err(WaitErr::Failed(e)) => {
            eprintln!("comux wait-output: {e}");
            return 1;
        }
    };

    // Pinned after the FIRST response, whatever the caller passed. Pinning a defaulted
    // target stops a mid-wait focus change retargeting the wait; pinning an EXPLICIT one
    // matters too, because a raw terminal id can be reused by a different pane across a
    // server restart while a pane token is incarnation-qualified.
    let mut pinned: Option<String> = None;
    loop {
        if expired(deadline) {
            return EXIT_TIMEOUT;
        }
        let req = Req::CapturePane {
            target: pinned.clone().or_else(|| args.target.clone()),
            index: pinned.is_none().then_some(args.index).flatten(),
            lines: Some(args.lines),
        };
        let resp = match conn.request(&req, deadline) {
            Ok(r) => r,
            Err(WaitErr::TimedOut) => return EXIT_TIMEOUT,
            Err(WaitErr::Failed(e)) => {
                eprintln!("comux wait-output: {e}");
                return 1;
            }
        };
        if !resp.ok {
            eprintln!(
                "comux wait-output: {}",
                resp.error.as_deref().unwrap_or("(unspecified)")
            );
            return 1;
        }
        if pinned.is_none() {
            // A pane with no token cannot be re-addressed, so the wait would silently
            // follow the focus — refuse rather than guess.
            match resp.pane.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => pinned = Some(t.to_string()),
                None => {
                    eprintln!(
                        "comux wait-output: the pane has no identity to pin to \
                         (spawned before pane tokens existed); pass an explicit target"
                    );
                    return 1;
                }
            }
        }
        if let Some(line) = last_match(resp.text.as_deref().unwrap_or(""), &args.pattern) {
            // Re-check before ACCEPTING: a match read after the budget ran out is a late
            // answer, and the caller has already given up on it.
            if expired(deadline) {
                return EXIT_TIMEOUT;
            }
            println!("{line}");
            return 0;
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
            return EXIT_TIMEOUT;
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
     --status working|ready|blocked [--timeout S] [--interval MS]\n\
     \n\
     BEST-EFFORT CURRENT-STATE wait, NOT turn-completion detection. It returns as soon as\n\
     the agent is IN that status, including a status it was already in before the call —\n\
     so right after sending a prompt, `--status ready` can match the PREVIOUS turn.\n\
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
}

/// Parse `wait-agent`'s arguments. `Err(code)` is the process exit code.
fn parse_wait_agent_args(rest: &[&String]) -> Result<WaitAgentArgs, i32> {
    let mut target: Option<String> = None;
    let mut status: Option<String> = None;
    let mut timeout_secs: u64 = 300;
    let mut interval_ms: u64 = 250;
    let mut i = 1;
    while i < rest.len() {
        let a = rest[i].as_str();
        let mut takes_value = true;
        match a {
            "--status" => match rest.get(i + 1) {
                Some(v) => status = Some((*v).to_string()),
                None => {
                    eprintln!("comux wait-agent: --status needs a value");
                    return Err(2);
                }
            },
            "--timeout" | "--interval" => {
                let n = match rest.get(i + 1).map(|v| v.parse::<u64>()) {
                    Some(Ok(n)) => n,
                    _ => {
                        eprintln!("comux wait-agent: {a} needs a number");
                        return Err(2);
                    }
                };
                if a == "--timeout" {
                    timeout_secs = n;
                } else {
                    interval_ms = n;
                }
            }
            _ => {
                takes_value = false;
                match a {
                    "--json" => {}
                    "-h" | "--help" => {
                        eprintln!("{WAIT_AGENT_USAGE}");
                        return Err(2);
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
    })
}

/// Run `wait-agent`. Shares [`WaitConn`] and the deadline machinery with `wait-output`.
fn run_wait_agent(args: WaitAgentArgs) -> i32 {
    let deadline = args.timeout.map(|t| Instant::now() + t);
    let expired = |d: Option<Instant>| d.is_some_and(|d| Instant::now() >= d);
    let mut conn = match WaitConn::connect(deadline) {
        Ok(c) => c,
        Err(WaitErr::TimedOut) => return EXIT_TIMEOUT,
        Err(WaitErr::Failed(e)) => {
            eprintln!("comux wait-agent: {e}");
            return 1;
        }
    };
    let req = Req::ListAgents {
        target: Some(args.target.clone()),
    };
    loop {
        if expired(deadline) {
            return EXIT_TIMEOUT;
        }
        let resp = match conn.request(&req, deadline) {
            Ok(r) => r,
            Err(WaitErr::TimedOut) => return EXIT_TIMEOUT,
            Err(WaitErr::Failed(e)) => {
                eprintln!("comux wait-agent: {e}");
                return 1;
            }
        };
        if !resp.ok {
            // The pane is GONE — the server resolves the target every request, so this also
            // catches a pane that closed mid-wait.
            eprintln!(
                "comux wait-agent: {}",
                resp.error.as_deref().unwrap_or("(unspecified)")
            );
            return 1;
        }
        // An empty listing means the pane exists but is not classified as an agent yet (or
        // any more). Keep waiting: classification lags a launch by up to one sweep.
        if let Some(hit) = resp
            .agents
            .unwrap_or_default()
            .into_iter()
            .find(|a| a.status == args.status)
        {
            if expired(deadline) {
                return EXIT_TIMEOUT;
            }
            println!("{} {} {}s", hit.status, hit.tool, hit.for_secs);
            return 0;
        }
        let nap = match deadline {
            Some(d) => args
                .interval
                .min(d.saturating_duration_since(Instant::now())),
            None => args.interval,
        };
        if nap.is_zero() {
            return EXIT_TIMEOUT;
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
            title: "tab 1".into(),
            tool: "claude".into(),
            status: status.into(),
            for_secs: 42,
            detail: None,
        }
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

    #[test]
    fn a_copad_that_did_not_focus_must_not_suppress_the_generic_raise() {
        // The bug this guards: treating ANY reply from copad as "handled". A copad that does
        // not own the panel answers `focused: false` — a successful call that found nothing —
        // and the click must still fall through to activating the application.
        assert!(super::fall_back_after_copad(false));
        assert!(!super::fall_back_after_copad(true));
        // No copad at all is the same as a copad that did not focus.
        assert!(super::fall_back_after_copad(false));
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

    /// Writes must be addressed by token. An index is a position in the SERVER's active tab,
    /// which the user can change mid-run, and a raw terminal id is recycled across server
    /// restarts — both retarget silently.
    #[test]
    fn the_skill_addresses_writes_by_token() {
        assert!(
            SKILL_MD.contains("**Use tokens. Always.**"),
            "the skill must state the preference outright"
        );
        // Matched on fragments that cannot straddle a line wrap — the prose is hard-wrapped,
        // so a longer literal silently stops matching the moment the paragraph reflows.
        assert!(
            SKILL_MD.contains("no way to make an index-addressed")
                && SKILL_MD.contains("race-free"),
            "the skill must admit the index race cannot be closed, only narrowed"
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
            } => {
                assert_eq!(target, None);
                assert_eq!(index, Some(0));
                assert_eq!(text, "ls");
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
        };
        let line = serde_json::to_string(&req).unwrap();
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
            Req::Split { dir, from } => {
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
