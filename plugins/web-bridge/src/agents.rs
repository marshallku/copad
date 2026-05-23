//! Agent state surface. Reads Claude/Codex UI status from the
//! undocumented `~/.claude/sessions/<PID>.json` file, walks the process
//! tree to find the claude/codex descendant of a tmux pane, and reads
//! the cross-tool `~/.cache/claude-attention/queue.jsonl` notification
//! inbox.
//!
//! Logic mirrors `~/dev/tmx/src/agents/` (session_meta + proc + attention
//! modules) but trimmed to just the data layer — no UI, no aggregation.
//! When tmx adds a JSON output mode this whole module becomes a shell-out.
//! Until then we re-implement the same lookups so the web dashboard
//! doesn't have to spawn `tmx` per overview tick.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Busy,
    Idle,
    Waiting,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionMeta {
    pub status: SessionStatus,
    pub session_id: String,
    pub updated_at_ms: i64,
}

#[derive(serde::Deserialize)]
struct RawSession {
    #[serde(default)]
    status: String,
    #[serde(default, rename = "sessionId")]
    session_id: String,
    #[serde(default, rename = "updatedAt")]
    updated_at: i64,
}

fn session_path(pid: u32) -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(format!(".claude/sessions/{pid}.json")))
}

pub fn read_session_meta(pid: u32) -> Option<SessionMeta> {
    let bytes = fs::read(session_path(pid)?).ok()?;
    parse_session(&bytes)
}

fn parse_session(bytes: &[u8]) -> Option<SessionMeta> {
    let raw: RawSession = serde_json::from_slice(bytes).ok()?;
    let status = match raw.status.as_str() {
        "busy" => SessionStatus::Busy,
        "idle" => SessionStatus::Idle,
        "waiting" => SessionStatus::Waiting,
        _ => return None,
    };
    Some(SessionMeta {
        status,
        session_id: raw.session_id,
        updated_at_ms: raw.updated_at,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentProc {
    pub pid: u32,
    pub name: String,
}

pub struct ProcSnapshot {
    sys: System,
}

impl ProcSnapshot {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, Self::refresh_kind());
        Self { sys }
    }

    fn refresh_kind() -> ProcessRefreshKind {
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet)
    }

    /// BFS over descendants of `root_pid`. Returns the most recently
    /// started descendant whose process name matches one of `targets`.
    /// `None` when the root has no matching descendant (the normal case
    /// for a shell-only pane).
    pub fn find_descendant(&self, root_pid: u32, targets: &[&str]) -> Option<AgentProc> {
        let mut best: Option<&Process> = None;
        let mut stack = vec![Pid::from_u32(root_pid)];
        while let Some(pid) = stack.pop() {
            for (other_pid, proc) in self.sys.processes() {
                if proc.parent() != Some(pid) {
                    continue;
                }
                if let Some(name) = exe_basename(proc)
                    && targets.contains(&name)
                {
                    best = match best {
                        None => Some(proc),
                        Some(prev) if proc.start_time() > prev.start_time() => Some(proc),
                        Some(_) => best,
                    };
                }
                stack.push(*other_pid);
            }
        }
        best.map(|p| AgentProc {
            pid: p.pid().as_u32(),
            name: exe_basename(p).unwrap_or_default().to_string(),
        })
    }
}

impl Default for ProcSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

fn exe_basename(proc: &Process) -> Option<&str> {
    // Prefer the kernel-set comm name (`/proc/<pid>/comm`) over the
    // exe path basename — Claude Code's exe is at
    // `.../versions/<semver>/cli.js` so `file_name()` yields the
    // version string instead of `claude`. See tmx's proc.rs for the
    // detailed rationale.
    proc.name().to_str().or_else(|| {
        proc.exe()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AttentionEntry {
    pub ts: i64,
    pub kind: String,
    pub source: String,
    pub title: String,
    pub body: String,
    pub session_id: String,
    pub tmux_session: String,
}

#[derive(serde::Deserialize)]
struct RawAttention {
    ts: i64,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    tmux_session: String,
}

fn attention_queue_path() -> Option<PathBuf> {
    // Match the shell hooks (notify-stop.sh etc.) exactly: they fall
    // back to $HOME/.cache even on macOS, NOT ~/Library/Caches. So
    // we deliberately don't use dirs::cache_dir().
    let cache = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache")))?;
    Some(cache.join("claude-attention/queue.jsonl"))
}

pub fn read_attention_queue(cutoff_secs: i64) -> Vec<AttentionEntry> {
    let Some(path) = attention_queue_path() else {
        return Vec::new();
    };
    let Ok(bytes) = fs::read(&path) else {
        return Vec::new();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = now.saturating_sub(cutoff_secs);
    parse_attention(&bytes, cutoff)
}

fn parse_attention(bytes: &[u8], cutoff: i64) -> Vec<AttentionEntry> {
    let mut entries: Vec<AttentionEntry> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<RawAttention>(line).ok())
        .filter(|r| r.ts >= cutoff)
        .map(|r| AttentionEntry {
            ts: r.ts,
            kind: r.kind,
            source: r.source,
            title: r.title,
            body: r.body,
            session_id: r.session_id,
            tmux_session: r.tmux_session,
        })
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.ts));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_known_busy() {
        let json = br#"{"pid":1,"sessionId":"s-1","status":"busy","updatedAt":1700000000000}"#;
        let m = parse_session(json).expect("parses");
        assert_eq!(m.status, SessionStatus::Busy);
        assert_eq!(m.session_id, "s-1");
        assert_eq!(m.updated_at_ms, 1700000000000);
    }

    #[test]
    fn parse_session_known_idle_and_waiting() {
        let m = parse_session(br#"{"status":"idle"}"#).unwrap();
        assert_eq!(m.status, SessionStatus::Idle);
        let m = parse_session(br#"{"status":"waiting"}"#).unwrap();
        assert_eq!(m.status, SessionStatus::Waiting);
    }

    #[test]
    fn parse_session_unknown_status_returns_none() {
        // Future Claude versions might add states; refuse to guess.
        assert!(parse_session(br#"{"status":"thinking"}"#).is_none());
    }

    #[test]
    fn parse_session_malformed_returns_none() {
        assert!(parse_session(b"").is_none());
        assert!(parse_session(b"not json").is_none());
        assert!(parse_session(br#"{"status":"bu"#).is_none());
    }

    #[test]
    fn parse_session_missing_status_returns_none() {
        assert!(parse_session(br#"{"sessionId":"s"}"#).is_none());
    }

    #[test]
    fn parse_session_tolerates_extra_fields() {
        let json = br#"{"pid":22650,"sessionId":"31e571fa","cwd":"/x","startedAt":1779149382456,"version":"2.1.143","status":"busy","updatedAt":1779165593130}"#;
        let m = parse_session(json).unwrap();
        assert_eq!(m.session_id, "31e571fa");
    }

    fn att_line(ts: i64, kind: &str, body: &str) -> String {
        format!(
            r#"{{"ts":{ts},"kind":"{kind}","source":"claude","title":"t","body":"{body}","session_id":"sid","tmux_session":"s"}}"#
        )
    }

    #[test]
    fn parse_attention_drops_below_cutoff_sorts_desc() {
        let now: i64 = 10_000;
        let cutoff = now - 3600;
        let raw = format!(
            "{}\n{}\n{}\n",
            att_line(now - 60, "notification", "fresh"),
            att_line(now - 7200, "stop", "stale"),
            att_line(now - 300, "codex-turn", "mid"),
        );
        let entries = parse_attention(raw.as_bytes(), cutoff);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].body, "fresh");
        assert_eq!(entries[1].body, "mid");
    }

    #[test]
    fn parse_attention_skips_malformed_lines() {
        let raw = b"{\"ts\":100,\"kind\":\"notification\",\"body\":\"ok\"}\nnot-json\n{\"ts\":200,\"kind\":\"stop\",\"body\":\"ok2\"}\n";
        let entries = parse_attention(raw, 0);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].body, "ok2");
    }

    #[test]
    fn parse_attention_empty_and_blank() {
        assert!(parse_attention(b"", 0).is_empty());
        assert!(parse_attention(b"\n\n", 0).is_empty());
    }

    #[test]
    fn parse_attention_missing_ts_skipped() {
        // ts has no #[serde(default)] — required.
        let raw = br#"{"kind":"stop","body":"no-ts"}
{"ts":42,"kind":"stop","body":"ok"}
"#;
        let entries = parse_attention(raw, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].body, "ok");
    }

    #[test]
    fn proc_snapshot_no_target_returns_none() {
        let snap = ProcSnapshot::new();
        let me = std::process::id();
        assert!(snap.find_descendant(me, &["nonexistent_xyz_abc"]).is_none());
    }
}
