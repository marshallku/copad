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
    /// `session:window_idx` form written by the bash hooks. Lets the
    /// SPA route a tap on this row to the originating pane via the
    /// existing attach flow (parse → lookup in tmuxPanes → enterAttach).
    pub tmux_target: String,
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
    #[serde(default)]
    tmux_target: String,
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

/// One active codex-companion background job, read from
/// `~/.claude/state/codex-companion/state/<workspace>/state.json`.
/// Mirrors tmx's `state::CodexJob` schema so the SPA can display
/// jobs (running / queued) alongside tmux panes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodexJob {
    pub id: String,
    pub title: String,
    pub kind_label: String,
    pub workspace_root: String,
    pub status: String,
    pub started_at_ms: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub pid: Option<u32>,
}

impl CodexJob {
    /// True for any non-terminal status. codex-companion's terminal
    /// set is `completed | failed | cancelled | canceled`; anything
    /// else (running, queued, missing) counts as still doing work.
    pub fn is_active(&self) -> bool {
        !matches!(
            self.status.as_str(),
            "completed" | "failed" | "cancelled" | "canceled"
        )
    }
}

#[derive(serde::Deserialize)]
struct CodexStateFile {
    jobs: Option<Vec<RawCodexJob>>,
}

#[derive(serde::Deserialize)]
struct RawCodexJob {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(rename = "kindLabel", default)]
    kind_label: String,
    #[serde(rename = "workspaceRoot", default)]
    workspace_root: String,
    #[serde(default)]
    status: String,
    #[serde(rename = "startedAt", default)]
    started_at: String,
    #[serde(rename = "updatedAt", default)]
    updated_at: String,
    #[serde(default)]
    pid: Option<u32>,
}

/// Enumerate active codex-companion jobs from every workspace dir.
/// Returns newest-started first. Missing root dir → empty Vec.
pub fn read_codex_jobs() -> Vec<CodexJob> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    read_codex_jobs_in(&home.join(".claude/state/codex-companion/state"))
}

fn read_codex_jobs_in(dir: &std::path::Path) -> Vec<CodexJob> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut jobs: Vec<CodexJob> = Vec::new();
    for entry in read.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        jobs.extend(read_workspace_codex_jobs(&entry.path()));
    }
    jobs.retain(CodexJob::is_active);
    jobs.sort_by_key(|j| std::cmp::Reverse(j.started_at_ms));
    jobs
}

fn read_workspace_codex_jobs(workspace_dir: &std::path::Path) -> Vec<CodexJob> {
    let state_path = workspace_dir.join("state.json");
    let Ok(bytes) = std::fs::read(&state_path) else {
        return Vec::new();
    };
    let Ok(file): Result<CodexStateFile, _> = serde_json::from_slice(&bytes) else {
        return Vec::new();
    };
    file.jobs
        .unwrap_or_default()
        .into_iter()
        .map(|j| CodexJob {
            id: j.id,
            title: j.title,
            kind_label: j.kind_label,
            workspace_root: j.workspace_root,
            status: j.status,
            started_at_ms: parse_iso8601_millis(&j.started_at),
            updated_at_ms: parse_iso8601_millis(&j.updated_at),
            pid: j.pid,
        })
        .collect()
}

/// Minimal ISO-8601 → epoch-millis parser tuned for codex-companion's
/// `YYYY-MM-DDTHH:MM:SS.fffZ` output. Mirrors tmx's parser (same fmt
/// is the only one the companion writes today).
fn parse_iso8601_millis(s: &str) -> Option<i64> {
    if s.len() < 20 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    let mut millis: i64 = 0;
    if s.get(19..20) == Some(".")
        && let Some(frac) = s.get(20..23)
    {
        millis = frac.parse().ok()?;
    }
    let days = days_from_civil(year, month, day) - 719_468;
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec;
    Some(secs * 1000 + millis)
}

/// Howard Hinnant's days_from_civil — exact + alloc-free.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64
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
            tmux_target: r.tmux_target,
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
    fn codex_job_is_active_excludes_terminal_states() {
        let base = CodexJob {
            id: "x".into(),
            title: String::new(),
            kind_label: String::new(),
            workspace_root: String::new(),
            status: "running".into(),
            started_at_ms: None,
            updated_at_ms: None,
            pid: None,
        };
        assert!(base.is_active());
        for s in ["completed", "failed", "cancelled", "canceled"] {
            assert!(
                !CodexJob {
                    status: s.into(),
                    ..base.clone()
                }
                .is_active()
            );
        }
    }

    #[test]
    fn codex_jobs_in_filters_active_and_sorts() {
        use std::fs;
        use tempfile::tempdir;
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("nestty-abcd");
        fs::create_dir_all(&ws).unwrap();
        let state = r#"{
            "jobs": [
                {"id":"old-done","status":"completed","workspaceRoot":"/x","startedAt":"2026-05-18T01:00:00.000Z","updatedAt":"2026-05-18T01:00:01.000Z"},
                {"id":"new-run","status":"running","workspaceRoot":"/y","startedAt":"2026-05-20T01:00:00.000Z","updatedAt":"2026-05-20T01:01:00.000Z","pid":12345}
            ]
        }"#;
        fs::write(ws.join("state.json"), state).unwrap();
        let jobs = read_codex_jobs_in(tmp.path());
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "new-run");
        assert_eq!(jobs[0].pid, Some(12345));
    }

    #[test]
    fn codex_jobs_in_missing_dir_empty() {
        assert!(read_codex_jobs_in(std::path::Path::new("/nope/xyz")).is_empty());
    }

    #[test]
    fn parse_iso8601_known_value_round_trips() {
        let ms = parse_iso8601_millis("2026-05-19T01:56:52.454Z").unwrap();
        let days = days_from_civil(2026, 5, 19) - 719_468;
        let secs = days * 86_400 + 3600 + 56 * 60 + 52;
        assert_eq!(ms, secs * 1000 + 454);
    }

    #[test]
    fn parse_iso8601_rejects_short_string() {
        assert!(parse_iso8601_millis("").is_none());
        assert!(parse_iso8601_millis("2026-05-19").is_none());
    }

    #[test]
    fn proc_snapshot_no_target_returns_none() {
        let snap = ProcSnapshot::new();
        let me = std::process::id();
        assert!(snap.find_descendant(me, &["nonexistent_xyz_abc"]).is_none());
    }
}
