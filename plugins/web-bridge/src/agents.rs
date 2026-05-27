//! Shell-out to `tmx agents --json` for the agent + attention +
//! codex-job snapshot. tmx 1.x is the source of truth; we just parse
//! its JSON and surface what the dashboard needs.
//!
//! Why a shell-out instead of inline:
//!   * Single source of truth — tmx's Claude/Codex classification,
//!     process-tree walk, zombie filtering, repo-marker scanning, and
//!     attention queue parsing all live in one project and evolve
//!     together. Re-implementing them here would drift.
//!   * No `~/.claude/sessions/*.json` schema coupling — when Claude
//!     renames a field, tmx absorbs the breakage and we get an
//!     unchanged JSON shape.
//!
//! On `tmx` not installed / not on PATH: `read_snapshot` returns
//! `Err`; callers degrade gracefully (panes still render from
//! `tmux list-panes`, agent enrichment + attention strip simply
//! show empty).

use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TmxSnapshot {
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub attention: Vec<Attention>,
    #[serde(default)]
    pub global_blocked: u32,
    #[serde(default)]
    pub captured_at_ms: i64,
    /// Populated by `enrich_codex_jobs` after the shell-out — tmx's
    /// `agents --json` doesn't include codex-companion jobs yet, so we
    /// still read them locally and stuff them into the snapshot here.
    #[serde(default, skip)]
    pub codex_jobs: Vec<CodexJob>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub pane: AgentPane,
    /// `claude` / `codex` / `shell` / `other`. tmx classifies — we
    /// just relay so the SPA can color cards or filter.
    pub kind: String,
    /// `ready` / `busy` / `waiting` / `idle` / `unknown`. tmx's
    /// vocabulary; SPA maps to pill colors.
    pub status: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub repo_name: String,
    #[serde(default)]
    pub flags: AgentFlags,
    /// Free-text extra (e.g. "pid 12345"); for display only.
    #[serde(default)]
    pub extra: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPane {
    pub session: String,
    pub window: u32,
    pub pane: u32,
    /// Pane PID = what `tmux list-panes -F '#{pane_pid}'` would
    /// return. We join on this when matching tmx agents back to our
    /// own `tmux list-panes` results, since tmx doesn't emit `%N`
    /// pane ids.
    #[serde(default)]
    pub pane_pid: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentFlags {
    #[serde(default)]
    pub has_intent: bool,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub reviewed_fresh: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attention {
    pub ts: i64,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub tmux_target: String,
    #[serde(default)]
    pub tmux_session: String,
}

/// codex-companion background job. Surfaced in the overview as its own
/// card section. tmx's `agents --json` doesn't yet emit these so we
/// keep the local reader of `~/.claude/state/codex-companion/state/*`.
#[derive(Debug, Clone, Serialize)]
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
    /// True for any non-terminal status (running / queued / unknown).
    /// tmx's terminal set: completed | failed | cancelled | canceled.
    pub fn is_active(&self) -> bool {
        !matches!(
            self.status.as_str(),
            "completed" | "failed" | "cancelled" | "canceled"
        )
    }
}

#[derive(Deserialize)]
struct CodexStateFile {
    jobs: Option<Vec<RawCodexJob>>,
}

#[derive(Deserialize)]
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

/// Walk `~/.claude/state/codex-companion/state/<workspace>/state.json`,
/// concat all `jobs` arrays, keep non-terminal only, newest-started
/// first. Empty Vec on missing dir.
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
/// `YYYY-MM-DDTHH:MM:SS.fffZ` output. Mirrors tmx's parser.
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

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64
}

/// Shell out to `tmx agents --json` and parse the snapshot. Returns
/// `Err` if tmx is missing, the call fails, or the JSON doesn't
/// parse — caller treats this as "agent enrichment unavailable" and
/// degrades to plain pane rendering.
///
/// systemd user units start with a stripped PATH that often omits
/// `~/.local/bin` and `~/.cargo/bin` — the two places `tmx` is
/// typically installed. We try the PATH lookup first, then fall back
/// to those candidate paths so the plugin works without the user
/// having to edit their user unit's environment.
pub fn read_snapshot() -> Result<TmxSnapshot, String> {
    let mut candidates: Vec<String> = vec!["tmx".to_string()];
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/tmx").to_string_lossy().into_owned());
        candidates.push(home.join(".cargo/bin/tmx").to_string_lossy().into_owned());
    }
    let mut last_err = String::from("no candidate tried");
    for bin in &candidates {
        match Command::new(bin).args(["agents", "--json"]).output() {
            Ok(out) if out.status.success() => return parse_snapshot(&out.stdout),
            Ok(out) => {
                last_err = format!(
                    "{bin} exit={:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Err(e) => last_err = format!("{bin}: {e}"),
        }
    }
    Err(last_err)
}

fn parse_snapshot(bytes: &[u8]) -> Result<TmxSnapshot, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("parse tmx snapshot: {e}"))
}

impl TmxSnapshot {
    /// O(N) lookup of an agent by the pane PID it was observed
    /// running in. Returns the first match — there shouldn't ever be
    /// duplicates since `pane_pid` is unique per pane.
    pub fn agent_for_pane_pid(&self, pid: u32) -> Option<&Agent> {
        self.agents.iter().find(|a| a.pane.pane_pid == Some(pid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from live `tmx agents --json` 2026-05-24. Uses the
    /// real vocabulary (`working`/`awaiting-decision`/`ready`/`idle`)
    /// and the real top-level fields (no `codex_jobs` — tmx doesn't
    /// emit it; we populate that locally via `read_codex_jobs`).
    const SAMPLE: &str = r#"{
        "agents": [
            {
                "id": "pane:copad:0.0",
                "pane": {"session":"copad","window":0,"pane":0,"pane_pid":99},
                "kind": "claude",
                "status": "working",
                "cwd": "/home/me/dev/copad",
                "repo_name": "copad",
                "flags": {"has_intent": true, "blocked": false, "reviewed_fresh": true},
                "extra": "pid 12345"
            },
            {
                "id": "pane:docs:0.0",
                "pane": {"session":"docs","window":0,"pane":0,"pane_pid":100},
                "kind": "shell",
                "status": "idle",
                "cwd": "/home/me/docs",
                "repo_name": "docs",
                "flags": {"has_intent": false, "blocked": false, "reviewed_fresh": false},
                "extra": ""
            }
        ],
        "attention": [
            {"ts": 1700000000, "kind": "stop", "source": "claude", "title": "Claude · copad", "body": "Turn finished",
             "session_id": "sid-1", "tmux_target": "copad:0", "tmux_session": "copad"}
        ],
        "global_blocked": 2,
        "captured_at_ms": 1700000000000,
        "panes_error": null
    }"#;

    #[test]
    fn parse_full_snapshot_all_fields() {
        let s = parse_snapshot(SAMPLE.as_bytes()).expect("parses");
        assert_eq!(s.agents.len(), 2);
        assert_eq!(s.attention.len(), 1);
        assert_eq!(s.global_blocked, 2);
        assert_eq!(s.captured_at_ms, 1700000000000);

        let claude = &s.agents[0];
        assert_eq!(claude.id, "pane:copad:0.0");
        assert_eq!(claude.kind, "claude");
        assert_eq!(claude.status, "working");
        assert_eq!(claude.cwd, "/home/me/dev/copad");
        assert_eq!(claude.repo_name, "copad");
        assert_eq!(claude.extra, "pid 12345");
        assert!(claude.flags.has_intent);
        assert!(!claude.flags.blocked);
        assert!(claude.flags.reviewed_fresh);
        assert_eq!(claude.pane.session, "copad");
        assert_eq!(claude.pane.window, 0);
        assert_eq!(claude.pane.pane, 0);
        assert_eq!(claude.pane.pane_pid, Some(99));

        let att = &s.attention[0];
        assert_eq!(att.ts, 1700000000);
        assert_eq!(att.kind, "stop");
        assert_eq!(att.source, "claude");
        assert_eq!(att.title, "Claude · copad");
        assert_eq!(att.body, "Turn finished");
        assert_eq!(att.session_id, "sid-1");
        assert_eq!(att.tmux_target, "copad:0");
        assert_eq!(att.tmux_session, "copad");
    }

    #[test]
    fn agent_for_pane_pid_finds_match() {
        let s = parse_snapshot(SAMPLE.as_bytes()).unwrap();
        let a = s.agent_for_pane_pid(99).unwrap();
        assert_eq!(a.kind, "claude");
        assert!(s.agent_for_pane_pid(7777).is_none());
    }

    #[test]
    fn parse_handles_missing_optional_top_keys() {
        let minimal = r#"{"agents":[],"attention":[]}"#;
        let s = parse_snapshot(minimal.as_bytes()).expect("parses");
        assert!(s.agents.is_empty());
        assert!(s.codex_jobs.is_empty()); // local-populated, default empty
        assert_eq!(s.global_blocked, 0);
    }

    #[test]
    fn codex_job_is_active_excludes_terminal() {
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
    fn read_codex_jobs_in_filters_active_and_sorts() {
        use std::fs;
        use tempfile::tempdir;
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("copad-abcd");
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
        assert!(jobs[0].started_at_ms.is_some());
    }

    #[test]
    fn read_codex_jobs_in_missing_dir_empty() {
        assert!(read_codex_jobs_in(std::path::Path::new("/nope/xyz")).is_empty());
    }

    #[test]
    fn parse_iso8601_round_trip() {
        let ms = parse_iso8601_millis("2026-05-19T01:56:52.454Z").unwrap();
        let days = days_from_civil(2026, 5, 19) - 719_468;
        let secs = days * 86_400 + 3600 + 56 * 60 + 52;
        assert_eq!(ms, secs * 1000 + 454);
    }

    #[test]
    fn parse_rejects_invalid_json() {
        assert!(parse_snapshot(b"not json").is_err());
        assert!(parse_snapshot(b"").is_err());
    }

    #[test]
    fn parse_attention_required_ts_only() {
        // ts is the only required attention field — rest default to "".
        let raw = r#"{"agents":[],"attention":[{"ts":42}]}"#;
        let s = parse_snapshot(raw.as_bytes()).unwrap();
        assert_eq!(s.attention.len(), 1);
        assert_eq!(s.attention[0].ts, 42);
        assert_eq!(s.attention[0].title, "");
    }
}
