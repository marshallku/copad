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
    /// Focus the pane at `index` (as printed by `list`).
    Focus { index: usize },
    /// Close the pane at `index`.
    Close { index: usize },
    /// Inject `text` as input bytes into the pane at `index` (like `tmux send-keys`).
    SendKeys { index: usize, text: String },
}

/// One pane in a `list` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub index: usize,
    pub id: String,
    pub focused: bool,
    pub cols: u16,
    pub rows: u16,
}

/// A control response. `ok=false` carries `error`; `list` fills `panes`+`focused`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resp {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panes: Option<Vec<PaneInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<usize>,
}

impl Resp {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            panes: None,
            focused: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            panes: None,
            focused: None,
        }
    }
    pub fn list(panes: Vec<PaneInfo>, focused: usize) -> Self {
        Self {
            ok: true,
            error: None,
            panes: Some(panes),
            focused: Some(focused),
        }
    }
}

/// The control-socket path: `$COPAD_MUX_SOCK`, else `$TMPDIR/copad-mux.sock`
/// (TMPDIR is per-user on macOS; `$USER` disambiguates on shared `/tmp`).
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("COPAD_MUX_SOCK") {
        return PathBuf::from(p);
    }
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    PathBuf::from(tmp.trim_end_matches('/')).join(format!("copad-mux-{user}.sock"))
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
        eprintln!("usage: copad-mux ctl <list|split|focus|close|send> [args]");
        return 2;
    };

    let req = match cmd {
        "list" => Req::List,
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
            println!("{:<3} {:<8} {:<9} SIZE", "IDX", "PANE", "FOCUS");
            for p in &panes {
                println!(
                    "{:<3} {:<8} {:<9} {}x{}",
                    p.index,
                    p.id,
                    if p.index == focused { "*focused" } else { "" },
                    p.cols,
                    p.rows,
                );
            }
        }
        _ => println!("ok"),
    }
}
