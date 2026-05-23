//! tmux shell-out surface for plugins/web-bridge (Slice 3.1).
//!
//! tmux is the primary data model for the dashboard: `list_panes` for
//! the overview cards, `capture_pane` for per-pane previews, and
//! `send_text` (via `load-buffer` + `paste-buffer`) for one-shot
//! commands from overview mode. Live attach (xterm.js bidirectional)
//! spawns `tmux attach-session` inside a portable_pty PTY pair from
//! the WS handler in `main.rs`.
//!
//! `send_text` uses the same load-buffer/paste-buffer pattern as
//! `nestty-linux::socket::handle_claude_start` (socket.rs:1710). Going
//! through a buffer makes multiline + ANSI / quoting safe;
//! `send-keys -l` requires per-character escaping that's easy to get
//! wrong. `paste-buffer -p` enables bracketed paste so the receiving
//! shell can distinguish typed input from pasted blocks; `-d` deletes
//! the buffer after paste so they don't pile up across calls.

use serde::Serialize;
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Serialize)]
pub struct TmuxPane {
    pub session: String,
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub pane_id: String,
    pub pane_active: bool,
    pub cwd: String,
}

/// `tmux list-panes -a -F …` — all panes across all sessions in one
/// shell-out. Returns `Ok(vec![])` when no tmux server is running so
/// callers don't have to special-case the empty-overview path.
pub fn list_panes() -> Result<Vec<TmuxPane>, String> {
    let out = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{window_id}\t#{window_index}\t#{window_name}\t#{pane_id}\t#{pane_active}\t#{pane_current_path}",
        ])
        .output()
        .map_err(|e| format!("spawn tmux: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("no server running") || stderr.contains("error connecting") {
            return Ok(Vec::new());
        }
        return Err(format!("tmux list-panes failed: {}", stderr.trim()));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(parse_list_panes(&stdout))
}

pub fn parse_list_panes(stdout: &str) -> Vec<TmuxPane> {
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let mut cols = line.splitn(7, '\t');
            let session = cols.next()?.to_string();
            let window_id = cols.next()?.to_string();
            let window_index = cols.next()?.parse().ok()?;
            let window_name = cols.next()?.to_string();
            let pane_id = cols.next()?.to_string();
            let pane_active = cols.next()? == "1";
            let cwd = cols.next()?.to_string();
            Some(TmuxPane {
                session,
                window_id,
                window_index,
                window_name,
                pane_id,
                pane_active,
                cwd,
            })
        })
        .collect()
}

/// `tmux capture-pane -p -e -t <pane_id> -S -<last_n>` — last N lines
/// of pane history with ANSI escapes preserved. `-e` keeps colors so
/// the overview card can render them; the SPA strips on demand.
pub fn capture_pane(pane_id: &str, last_n: u32) -> Result<String, String> {
    let out = Command::new("tmux")
        .args([
            "capture-pane",
            "-p",
            "-e",
            "-t",
            pane_id,
            "-S",
            &format!("-{last_n}"),
        ])
        .output()
        .map_err(|e| format!("spawn tmux: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("tmux capture-pane {pane_id}: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Safe arbitrary-text send via `load-buffer` (stdin) + `paste-buffer`.
/// Mirrors `nestty-linux::socket::handle_claude_start` (socket.rs:1710)
/// — multiline / quoted / special-char input round-trips intact.
pub fn send_text(target: &str, text: &str) -> Result<(), String> {
    let buf_name = format!("nestty-web-{}", uuid::Uuid::new_v4());
    let mut load = Command::new("tmux")
        .args(["load-buffer", "-b", &buf_name, "-"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn tmux load-buffer: {e}"))?;
    if let Some(mut stdin) = load.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("write tmux stdin: {e}"))?;
    }
    let status = load
        .wait()
        .map_err(|e| format!("wait tmux load-buffer: {e}"))?;
    if !status.success() {
        return Err(format!("tmux load-buffer failed: {status}"));
    }
    let paste = Command::new("tmux")
        .args(["paste-buffer", "-p", "-d", "-b", &buf_name, "-t", target])
        .status()
        .map_err(|e| format!("spawn tmux paste-buffer: {e}"))?;
    if !paste.success() {
        return Err(format!("tmux paste-buffer failed: {paste}"));
    }
    Ok(())
}

/// Resolve the session name a given `pane_id` (`%N`) belongs to, by
/// scanning a `list_panes` result. Used by the attach WS handler to
/// invoke `tmux attach-session -t <session>` + `select-pane -t %N`.
pub fn find_pane<'a>(panes: &'a [TmuxPane], pane_id: &str) -> Option<&'a TmuxPane> {
    panes.iter().find(|p| p.pane_id == pane_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_panes_two_panes() {
        let sample = "main\t@0\t0\twork\t%0\t1\t/home/marshall/dev/nestty\n\
                     main\t@1\t1\tlogs\t%1\t0\t/var/log\n";
        let panes = parse_list_panes(sample);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].session, "main");
        assert_eq!(panes[0].window_id, "@0");
        assert_eq!(panes[0].window_index, 0);
        assert_eq!(panes[0].window_name, "work");
        assert_eq!(panes[0].pane_id, "%0");
        assert!(panes[0].pane_active);
        assert_eq!(panes[0].cwd, "/home/marshall/dev/nestty");
        assert_eq!(panes[1].pane_id, "%1");
        assert!(!panes[1].pane_active);
    }

    #[test]
    fn parse_list_panes_empty_stdin_yields_empty() {
        assert!(parse_list_panes("").is_empty());
        assert!(parse_list_panes("\n").is_empty());
    }

    #[test]
    fn parse_list_panes_skips_malformed_lines() {
        // Missing the trailing cwd column — drop the row, keep good ones.
        let sample = "ok\t@0\t0\tn\t%0\t1\t/a\n\
                     bad\t@1\t1\tn\t%1\t1\n\
                     ok2\t@2\t2\tn\t%2\t0\t/b\n";
        let panes = parse_list_panes(sample);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%0");
        assert_eq!(panes[1].pane_id, "%2");
    }

    #[test]
    fn parse_list_panes_session_name_with_spaces() {
        // session_name can contain spaces; tab-separation must survive.
        let sample = "my session\t@0\t0\tw\t%0\t1\t/home\n";
        let panes = parse_list_panes(sample);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].session, "my session");
    }

    #[test]
    fn find_pane_returns_match() {
        let panes = vec![
            TmuxPane {
                session: "s".into(),
                window_id: "@0".into(),
                window_index: 0,
                window_name: "w".into(),
                pane_id: "%0".into(),
                pane_active: true,
                cwd: "/".into(),
            },
            TmuxPane {
                session: "s".into(),
                window_id: "@1".into(),
                window_index: 1,
                window_name: "w2".into(),
                pane_id: "%5".into(),
                pane_active: false,
                cwd: "/tmp".into(),
            },
        ];
        assert_eq!(find_pane(&panes, "%5").map(|p| p.window_index), Some(1));
        assert!(find_pane(&panes, "%99").is_none());
    }
}
