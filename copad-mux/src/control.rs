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
    /// Rename the session at `index` (as printed by `list-sessions`). An empty `name`
    /// clears it back to the generated id.
    RenameSession { index: usize, name: String },
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
    /// Shut the persistent server down (drops every shell). The only key-free way to
    /// stop a detached server short of exiting its last shell.
    KillServer,
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
             list-sessions|new-session [name]|rename-session <index> <name>|select-session|\
             worktree <create|list|rm>|kill-server> [args]"
        );
        return 2;
    };

    let req = match cmd {
        "list" => Req::List,
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
            let Some(idx) = rest.get(1).and_then(|s| s.parse::<usize>().ok()) else {
                eprintln!("usage: comux rename-session <index> <name...>");
                return 2;
            };
            let name = rest
                .iter()
                .skip(2)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            Req::RenameSession {
                index: idx,
                name: name.trim().to_string(),
            }
        }
        "select-session" => {
            let Some(idx) = rest.get(1).and_then(|s| s.parse::<usize>().ok()) else {
                eprintln!("usage: comux select-session <index>");
                return 2;
            };
            Req::SelectSession { index: idx }
        }
        "select-tab" => {
            let Some(idx) = rest.get(1).and_then(|s| s.parse::<usize>().ok()) else {
                eprintln!("usage: comux select-tab <index>");
                return 2;
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
            let Some(idx) = rest.get(1).and_then(|s| s.parse::<usize>().ok()) else {
                eprintln!("usage: comux {cmd} <index>");
                return 2;
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

fn round_trip(req: &Req) -> Result<Resp, String> {
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
                "{:<3} {:<9} {:<16} {:<6} AGENTS",
                "IDX", "ACTIVE", "TAB", "PANES"
            );
            for t in &tabs {
                println!(
                    "{:<3} {:<9} {:<16} {:<6} {}",
                    t.index,
                    if t.index == active { "*active" } else { "" },
                    t.id,
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
        _ => println!("ok"),
    }
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
         \x20 comux worktree create <branch> [--from <ref>] [--json]\n\
         \x20 comux worktree list [--plain|--json]\n\
         \x20 comux worktree rm <path|branch> [-f|--force] [-d|--delete-branch] [--json]"
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
        eprintln!("usage: comux worktree create <branch> [--from <ref>]");
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
    match round_trip(&req) {
        Ok(resp) => print_worktree_result(&resp, json),
        Err(e) => {
            eprintln!("comux: {e}");
            1
        }
    }
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
    // A running server annotates `live`; with no server there are no comux sessions, so
    // list locally (all `live=false`) — matching tmx's "works with no server" behavior.
    if server_running() {
        let req = Req::WorktreeList { cwd: caller_cwd() };
        match round_trip(&req) {
            Ok(resp) if resp.ok => {
                print_worktrees(&resp.worktrees.unwrap_or_default(), json, plain);
                0
            }
            Ok(resp) => {
                eprintln!(
                    "error: {}",
                    resp.error.as_deref().unwrap_or("(unspecified)")
                );
                1
            }
            Err(e) => {
                eprintln!("comux: {e}");
                1
            }
        }
    } else {
        worktree_list_local(json, plain)
    }
}

fn worktree_list_local(json: bool, plain: bool) -> i32 {
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
    let infos: Vec<WorktreeInfo> = entries
        .iter()
        .map(|e| WorktreeInfo {
            path: e.path.display().to_string(),
            branch: e.branch.clone().unwrap_or_default(),
            is_main: e.is_main,
            live: false,
            locked: e.locked,
        })
        .collect();
    print_worktrees(&infos, json, plain);
    0
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
    let Some(target) = target else {
        eprintln!("usage: comux worktree rm <path|branch> [-f] [-d]");
        return 2;
    };

    // A running server owns liveness + the removal (single writer). With no server there
    // are no live sessions; take the server flock so none can start under us, then remove
    // locally — race-free, and without leaving a spurious server behind.
    match crate::server::try_acquire_lock() {
        Some(_guard) => worktree_rm_local(target, force, delete_branch, json),
        None => {
            let req = Req::WorktreeRm {
                target: target.to_string(),
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
