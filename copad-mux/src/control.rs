//! The control API: a Unix-socket protocol that lets `copad-mux ctl <cmd>` drive
//! a running TUI (like `tmux`/`tmx`). This module holds the wire types, the socket
//! path resolution, and the CLI client. The server side lives in [`crate::tui`]
//! and honors the single-writer rule (spec §1): the socket thread never touches
//! `State` — it hands requests to the main loop over an mpsc channel.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

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
    /// (`None` → shown by its generated `sN` id).
    NewSession {
        #[serde(default)]
        name: Option<String>,
    },
    /// Rename the session at `index` (as printed by `list-sessions`). An empty `name`
    /// clears it back to the generated id.
    RenameSession { index: usize, name: String },
    /// Make the session at `index` (as printed by `list-sessions`) active.
    SelectSession { index: usize },
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

/// A control response. `ok=false` carries `error`; `list` fills `panes`+`focused`;
/// `list-tabs` fills `tabs`+`active_tab`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resp {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
            panes: None,
            focused: None,
            tabs: None,
            active_tab: None,
            sessions: None,
            active_session: None,
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

/// The `copad-mux ctl ...` CLI client: parse args, round-trip one request over the
/// socket, print the response. Returns a process exit code.
pub fn run_client(args: &[String]) -> i32 {
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
            "usage: copad-mux ctl <list|split|resize|focus|close|send|list-tabs|new-tab|select-tab|\
             list-sessions|new-session [name]|rename-session <index> <name>|select-session|\
             kill-server> [args]"
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
            }
        }
        "rename-session" | "rename" => {
            let Some(idx) = rest.get(1).and_then(|s| s.parse::<usize>().ok()) else {
                eprintln!("usage: copad-mux ctl rename-session <index> <name...>");
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
                eprintln!("usage: copad-mux ctl select-session <index>");
                return 2;
            };
            Req::SelectSession { index: idx }
        }
        "select-tab" => {
            let Some(idx) = rest.get(1).and_then(|s| s.parse::<usize>().ok()) else {
                eprintln!("usage: copad-mux ctl select-tab <index>");
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
                eprintln!("usage: copad-mux ctl {cmd} <index>");
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
                eprintln!("usage: copad-mux ctl resize <index> <left|right|up|down>");
                return 2;
            };
            Req::ResizePane {
                index: idx,
                dir: dir.to_string(),
            }
        }
        "send" | "send-keys" => {
            let Some(idx) = rest.get(1).and_then(|s| s.parse::<usize>().ok()) else {
                eprintln!("usage: copad-mux ctl send <index> <text...>");
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
            eprintln!("copad-mux ctl: unknown command '{other}'");
            return 2;
        }
    };

    let resp = match round_trip(&req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("copad-mux ctl: {e}");
            return 1;
        }
    };

    if json_out {
        println!("{}", serde_json::to_string(&resp).unwrap_or_default());
    } else {
        print_human(&req, &resp);
    }
    if resp.ok { 0 } else { 1 }
}

fn round_trip(req: &Req) -> Result<Resp, String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        format!(
            "no running copad-mux at {} ({e}). Start one, or set COPAD_MUX_SOCK.",
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
        return Err("empty response from copad-mux".to_string());
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
