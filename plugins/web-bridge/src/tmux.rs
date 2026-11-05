//! tmux shell-out surface for plugins/web-bridge: `list_panes` for overview cards, `capture_pane`
//! for previews, and `send_text` for one-shot commands. Live attach spawns `tmux attach-session`
//! in a portable_pty pair from the WS handler in `main.rs`.
//!
//! `send_text` uses load-buffer/paste-buffer like `copad-linux::socket::handle_claude_start`
//! (socket.rs:1710): multiline, ANSI and quoting are safe, where `send-keys -l` needs fragile
//! per-character escaping. `paste-buffer -p` enables bracketed paste so the shell can tell pasted
//! blocks from typing; `-d` deletes the buffer so they do not pile up.

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
    /// PID of the pane's foreground process group leader; `None` only if tmux emitted an empty
    /// string.
    /// Root for `find_descendant("claude" | "codex")`.
    pub pane_pid: Option<u32>,
}

/// `tmux list-panes -a -F ...`: all panes in one shell-out. `Ok(vec![])` when no tmux server runs.
pub fn list_panes() -> Result<Vec<TmuxPane>, String> {
    let out = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{window_id}\t#{window_index}\t#{window_name}\t#{pane_id}\t#{pane_active}\t#{pane_current_path}\t#{pane_pid}",
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
            let mut cols = line.splitn(8, '\t');
            let session = cols.next()?.to_string();
            let window_id = cols.next()?.to_string();
            let window_index = cols.next()?.parse().ok()?;
            let window_name = cols.next()?.to_string();
            let pane_id = cols.next()?.to_string();
            let pane_active = cols.next()? == "1";
            let cwd = cols.next()?.to_string();
            // Best-effort: a bad pid only skips agent enrichment; the row still appears.
            let pane_pid = cols.next().and_then(|s| s.parse::<u32>().ok());
            Some(TmuxPane {
                session,
                window_id,
                window_index,
                window_name,
                pane_id,
                pane_active,
                cwd,
                pane_pid,
            })
        })
        .collect()
}

/// `tmux capture-pane -p -e -t <pane_id> -S -<last_n>`: last N lines with ANSI escapes preserved
/// (`-e`) so the card can render colors.
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

/// Safe arbitrary-text send via `load-buffer` (stdin) + `paste-buffer`, with a trailing newline
/// delivered as a real Enter key. Under bracketed paste (modern zsh/bash) a pasted `\n` is
/// inserted literally and does not execute, so the final newline is split off, the body pasted,
/// and `send-keys Enter` submits regardless of paste mode. Interior newlines stay in the body.
pub fn send_text(target: &str, text: &str) -> Result<(), String> {
    // Strip one trailing line terminator as the submit, `\r\n` first so web clients' CRLF does not
    // paste a stray `\r`, then bare `\n`. Interior / lone `\r` bytes stay in the body.
    let (body, submit) = if let Some(rest) = text.strip_suffix("\r\n") {
        (rest, true)
    } else if let Some(rest) = text.strip_suffix('\n') {
        (rest, true)
    } else {
        (text, false)
    };

    if !body.is_empty() {
        let buf_name = format!("copad-web-{}", uuid::Uuid::new_v4());
        let mut load = Command::new("tmux")
            .args(["load-buffer", "-b", &buf_name, "-"])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn tmux load-buffer: {e}"))?;
        if let Some(mut stdin) = load.stdin.take() {
            stdin
                .write_all(body.as_bytes())
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
    }

    if submit {
        let enter = Command::new("tmux")
            .args(["send-keys", "-t", target, "Enter"])
            .status()
            .map_err(|e| format!("spawn tmux send-keys Enter: {e}"))?;
        if !enter.success() {
            return Err(format!("tmux send-keys Enter failed: {enter}"));
        }
    }

    Ok(())
}

/// Find the pane for a `pane_id` (`%N`) in a `list_panes` result, so the attach WS handler can
/// run `tmux attach-session -t <session>` + `select-pane -t %N`.
pub fn find_pane<'a>(panes: &'a [TmuxPane], pane_id: &str) -> Option<&'a TmuxPane> {
    panes.iter().find(|p| p.pane_id == pane_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_panes_two_panes() {
        let sample = "main\t@0\t0\twork\t%0\t1\t/home/marshall/dev/copad\t12345\n\
                     main\t@1\t1\tlogs\t%1\t0\t/var/log\t12346\n";
        let panes = parse_list_panes(sample);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].session, "main");
        assert_eq!(panes[0].window_id, "@0");
        assert_eq!(panes[0].window_index, 0);
        assert_eq!(panes[0].window_name, "work");
        assert_eq!(panes[0].pane_id, "%0");
        assert!(panes[0].pane_active);
        assert_eq!(panes[0].cwd, "/home/marshall/dev/copad");
        assert_eq!(panes[0].pane_pid, Some(12345));
        assert_eq!(panes[1].pane_id, "%1");
        assert!(!panes[1].pane_active);
        assert_eq!(panes[1].pane_pid, Some(12346));
    }

    #[test]
    fn parse_list_panes_empty_stdin_yields_empty() {
        assert!(parse_list_panes("").is_empty());
        assert!(parse_list_panes("\n").is_empty());
    }

    #[test]
    fn parse_list_panes_skips_malformed_lines() {
        // Missing columns up to cwd drop the row; a missing trailing pid is tolerated (None).
        let sample = "ok\t@0\t0\tn\t%0\t1\t/a\t111\n\
                     bad\t@1\t1\tn\t%1\t1\n\
                     ok2\t@2\t2\tn\t%2\t0\t/b\t222\n";
        let panes = parse_list_panes(sample);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%0");
        assert_eq!(panes[0].pane_pid, Some(111));
        assert_eq!(panes[1].pane_id, "%2");
        assert_eq!(panes[1].pane_pid, Some(222));
    }

    #[test]
    fn parse_list_panes_missing_pane_pid_column_tolerated() {
        // Old-format rows without the pid column still parse with pane_pid None.
        let sample = "s\t@0\t0\tn\t%0\t1\t/a\n";
        let panes = parse_list_panes(sample);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_pid, None);
    }

    #[test]
    fn parse_list_panes_session_name_with_spaces() {
        let sample = "my session\t@0\t0\tw\t%0\t1\t/home\t99\n";
        let panes = parse_list_panes(sample);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].session, "my session");
        assert_eq!(panes[0].pane_pid, Some(99));
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
                pane_pid: Some(100),
            },
            TmuxPane {
                session: "s".into(),
                window_id: "@1".into(),
                window_index: 1,
                window_name: "w2".into(),
                pane_id: "%5".into(),
                pane_active: false,
                cwd: "/tmp".into(),
                pane_pid: Some(200),
            },
        ];
        assert_eq!(find_pane(&panes, "%5").map(|p| p.window_index), Some(1));
        assert!(find_pane(&panes, "%99").is_none());
    }
}
