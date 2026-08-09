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
    /// Split the focused pane. `dir` = `"right"` (side by side) | `"down"` (stacked).
    Split { dir: String },
    /// Grow the pane at `index` toward `dir` (`left`/`right`/`up`/`down`) by nudging
    /// its split divider.
    ResizePane { index: usize, dir: String },
    /// Focus the pane at `index` (as printed by `list`).
    Focus { index: usize },
    /// Close the pane at `index`.
    Close { index: usize },
    /// Inject `text` as input bytes into the pane at `index` (like `tmux send-keys`).
    SendKeys { index: usize, text: String },
    /// List the workspace's tabs.
    ListTabs,
    /// Create a new tab and make it active.
    NewTab,
    /// Make the tab at `index` (as printed by `list-tabs`) active.
    SelectTab { index: usize },
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
}

/// One pane in a `list` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub index: usize,
    pub id: String,
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
            health: None,
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
            "usage: comux <list|split|resize|focus|close|send|list-tabs|new-tab|select-tab|\
             rename-tab [index] <name>|list-sessions|new-session [name]|\
             rename-session [index] <name>|select-session|\
             worktree <create|list|rm>|reload|health|kill-server> [args]"
        );
        return 2;
    };

    let req = match cmd {
        "list" => Req::List,
        "health" => Req::Health,
        "reload" | "source-file" => Req::ReloadConfig,
        "kill-server" => Req::KillServer,
        "list-tabs" | "tabs" => Req::ListTabs,
        "new-tab" => Req::NewTab,
        "list-sessions" | "sessions" => Req::ListSessions,
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
        "split" => {
            // -h/--horizontal → side by side (right); -v/--vertical → stacked (down).
            let dir = match rest.get(1).map(|s| s.as_str()) {
                Some("-v") | Some("--vertical") | Some("down") => "down",
                _ => "right",
            };
            Req::Split {
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
            let Some(idx) = rest.get(1).and_then(|s| s.parse::<usize>().ok()) else {
                eprintln!("usage: comux send <index> <text...>");
                return 2;
            };
            let text = rest
                .iter()
                .skip(2)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            Req::SendKeys { index: idx, text }
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

    if json_out {
        println!("{}", serde_json::to_string(&resp).unwrap_or_default());
    } else {
        print_human(&req, &resp);
    }
    if resp.ok { 0 } else { 1 }
}

/// Exit code for "the user cancelled the picker" — fzf's (and SIGINT's) convention, so a
/// shell wrapper can tell a deliberate abort apart from a real failure.
const EXIT_CANCELLED: i32 = 130;

/// Which live listing an omitted argument should be fuzzy-picked from.
#[derive(Clone, Copy)]
enum Target {
    Session,
    Tab,
    PaneFocus,
    PaneClose,
}

impl Target {
    /// The usage line printed when we can't prompt (`--json`, non-terminal stderr) or
    /// when an index WAS given but isn't a number.
    fn usage(self) -> &'static str {
        match self {
            Target::Session => "comux select-session [index]   (no index → fuzzy picker)",
            Target::Tab => "comux select-tab [index]   (no index → fuzzy picker)",
            Target::PaneFocus => "comux focus [index]   (no index → fuzzy picker)",
            Target::PaneClose => "comux close [index]   (no index → fuzzy picker)",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Target::Session => "switch to session",
            Target::Tab => "switch to tab",
            Target::PaneFocus => "focus a pane",
            Target::PaneClose => "close a pane",
        }
    }

    /// Message for "the listing came back empty" — a picker with nothing in it is a
    /// failure, not a cancellation.
    fn empty(self) -> &'static str {
        match self {
            Target::Session => "no sessions to pick from",
            Target::Tab => "no tabs to pick from",
            Target::PaneFocus | Target::PaneClose => "no panes to pick from",
        }
    }

    /// Fetch the listing and render it as picker rows paired with the index each row
    /// resolves to (paired so the two can never drift apart while filtering).
    fn rows(self) -> Result<Vec<(picker::Item, usize)>, i32> {
        match self {
            Target::Session => {
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
            Target::Tab => {
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
