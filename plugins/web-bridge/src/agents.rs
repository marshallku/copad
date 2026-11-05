//! Shell-out to `tmx agents --json` for the agent + attention + codex-job snapshot (tmx >= 1.1).
//!
//! Shelled out rather than inlined so tmx stays the single source of truth for Claude/Codex
//! classification, process-tree walk, attention parsing and codex job reading, which evolve
//! together, and so this plugin is not coupled to the `~/.claude/sessions/*.json` schema.
//!
//! With `tmx` missing, `read_snapshot` returns `Err` and callers degrade: panes still render from
//! `tmux list-panes`, and enrichment simply shows empty. A pre-1.1 tmx parses fine, with
//! `codex_jobs` defaulting to empty.

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
    /// Structured codex-companion background jobs from tmx >= 1.1 (alive-filtered there).
    #[serde(default)]
    pub codex_jobs: Vec<CodexJob>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    /// `None` for non-pane rows: tmx emits codex background jobs into `agents[]` with `"pane":
    /// null`, and a required field here failed the whole snapshot parse while one was live.
    pub pane: Option<AgentPane>,
    /// `claude` / `codex` / `shell` / `other`, as classified by tmx.
    pub kind: String,
    /// `ready` / `busy` / `waiting` / `idle` / `unknown`: tmx's vocabulary; the SPA maps it to
    /// pill colors.
    pub status: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub repo_name: String,
    #[serde(default)]
    pub flags: AgentFlags,
    #[serde(default)]
    pub extra: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPane {
    pub session: String,
    pub window: u32,
    pub pane: u32,
    /// Pane PID, as `tmux list-panes -F '#{pane_pid}'` returns. Joined on this when matching tmx
    /// agents to our own pane list, since tmx does not emit `%N` pane ids.
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

/// codex-companion background job from tmx >= 1.1's `codex_jobs` (alive-filtered, newest-first).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexJob {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind_label: String,
    #[serde(default)]
    pub workspace_root: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub started_at_ms: Option<i64>,
    #[serde(default)]
    pub updated_at_ms: Option<i64>,
    #[serde(default)]
    pub pid: Option<u32>,
}

/// Shell out to `tmx agents --json` and parse the snapshot. `Err` (tmx missing, call failed,
/// unparsable JSON) means "enrichment unavailable". systemd user units start with a stripped PATH
/// that often omits `~/.local/bin` and `~/.cargo/bin`, so PATH is tried first, then those paths.
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
    pub fn agent_for_pane_pid(&self, pid: u32) -> Option<&Agent> {
        self.agents
            .iter()
            .find(|a| a.pane.as_ref().is_some_and(|p| p.pane_pid == Some(pid)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from live `tmx agents --json` (tmx 1.1, 2026-06-12), including a codex background
    /// row with `"pane": null` and the structured `codex_jobs` array.
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
            },
            {
                "id": "codex:task-e2e-1",
                "pane": null,
                "kind": "codex",
                "status": "background",
                "cwd": "/home/me/dev/copad",
                "repo_name": "copad",
                "flags": {"has_intent": false, "blocked": false, "reviewed_fresh": false},
                "extra": "running • 0s ago"
            }
        ],
        "attention": [
            {"ts": 1700000000, "kind": "stop", "source": "claude", "title": "Claude · copad", "body": "Turn finished",
             "session_id": "sid-1", "tmux_target": "copad:0", "tmux_session": "copad"}
        ],
        "global_blocked": 2,
        "captured_at_ms": 1700000000000,
        "panes_error": null,
        "codex_jobs": [
            {"id": "task-e2e-1", "title": "E2E probe", "kind_label": "task",
             "workspace_root": "/home/me/dev/copad", "status": "running",
             "started_at_ms": 1781198712000, "updated_at_ms": 1781198712000, "pid": 469373}
        ]
    }"#;

    #[test]
    fn parse_full_snapshot_all_fields() {
        let s = parse_snapshot(SAMPLE.as_bytes()).expect("parses");
        assert_eq!(s.agents.len(), 3);
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
        let pane = claude.pane.as_ref().unwrap();
        assert_eq!(pane.session, "copad");
        assert_eq!(pane.window, 0);
        assert_eq!(pane.pane, 0);
        assert_eq!(pane.pane_pid, Some(99));

        // Codex background row: pane is null and must not fail the parse.
        let codex = &s.agents[2];
        assert_eq!(codex.kind, "codex");
        assert!(codex.pane.is_none());

        assert_eq!(s.codex_jobs.len(), 1);
        let job = &s.codex_jobs[0];
        assert_eq!(job.id, "task-e2e-1");
        assert_eq!(job.title, "E2E probe");
        assert_eq!(job.kind_label, "task");
        assert_eq!(job.workspace_root, "/home/me/dev/copad");
        assert_eq!(job.status, "running");
        assert_eq!(job.started_at_ms, Some(1781198712000));
        assert_eq!(job.pid, Some(469373));

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
        assert!(s.codex_jobs.is_empty());
        assert_eq!(s.global_blocked, 0);
    }

    #[test]
    fn parse_rejects_invalid_json() {
        assert!(parse_snapshot(b"not json").is_err());
        assert!(parse_snapshot(b"").is_err());
    }

    #[test]
    fn parse_attention_required_ts_only() {
        let raw = r#"{"agents":[],"attention":[{"ts":42}]}"#;
        let s = parse_snapshot(raw.as_bytes()).unwrap();
        assert_eq!(s.attention.len(), 1);
        assert_eq!(s.attention[0].ts, 42);
        assert_eq!(s.attention[0].title, "");
    }
}
