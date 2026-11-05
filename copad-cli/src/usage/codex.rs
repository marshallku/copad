//! Scan `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` for token usage.
//!
//! Each `event_msg`/`token_count` record carries `total_token_usage` (cumulative; summing it would
//! explode) and `last_token_usage` (the per-turn delta). Only the deltas are summed.
//!
//! `last_token_usage.input_tokens` includes the cached portion, so `cached_input_tokens` is moved
//! into `cache_read` (codex has no cache-write meter). `output_tokens` already includes
//! `reasoning_output_tokens` (input+output == total), so reasoning is never added again. The model
//! id comes from an earlier session-config line's `payload.model`; the latest one is carried
//! forward.

use super::model::{RawUsage, Record, Tool, Warnings};
use chrono::{DateTime, Local};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub enum CodexLine {
    Model(String),
    Tokens {
        ts: DateTime<Local>,
        usage: RawUsage,
    },
    Other,
}

/// Absent/null is `Some(0)`; a valid `u64` is `Some(n)`; the wrong type is `None` (rejects the
/// record as malformed instead of miscounting).
fn uget(v: &Value, k: &str) -> Option<u64> {
    match v.get(k) {
        None | Some(Value::Null) => Some(0),
        Some(x) => x.as_u64(),
    }
}

pub fn parse_codex_line(line: &str) -> Result<CodexLine, ()> {
    let v: Value = serde_json::from_str(line).map_err(|_| ())?;
    let Some(payload) = v.get("payload") else {
        return Ok(CodexLine::Other);
    };

    if let Some(m) = payload.get("model").and_then(|m| m.as_str()) {
        return Ok(CodexLine::Model(m.to_string()));
    }

    if payload.get("type").and_then(|t| t.as_str()) == Some("token_count") {
        let Some(last) = payload.get("info").and_then(|i| i.get("last_token_usage")) else {
            return Ok(CodexLine::Other);
        };
        if !last.is_object() {
            return Err(());
        }
        let full_input = uget(last, "input_tokens").ok_or(())?;
        let cached = uget(last, "cached_input_tokens").ok_or(())?.min(full_input);
        let usage = RawUsage {
            input: full_input - cached,
            cache_write: 0,
            cache_read: cached,
            // reasoning_output_tokens is a subset of output_tokens — do NOT add.
            output: uget(last, "output_tokens").ok_or(())?,
        };
        if usage.total() == 0 {
            return Ok(CodexLine::Other);
        }
        let ts_str = v.get("timestamp").and_then(|t| t.as_str()).ok_or(())?;
        let ts = DateTime::parse_from_rfc3339(ts_str)
            .map_err(|_| ())?
            .with_timezone(&Local);
        return Ok(CodexLine::Tokens { ts, usage });
    }

    Ok(CodexLine::Other)
}

fn collect_rollouts(dir: &Path, out: &mut Vec<PathBuf>, warns: &mut Warnings) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        warns.unreadable_files += 1;
        return;
    };
    for e in rd.flatten() {
        // A dir entry's `file_type()` does not follow symlinks: no infinite recursion on a
        // symlinked-directory cycle.
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            collect_rollouts(&p, out, warns);
        } else if ft.is_file()
            && p.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
        {
            out.push(p);
        }
    }
}

/// Walk `root` (`~/.codex/sessions`) recursively, summing per-turn deltas at or after `since`.
pub fn scan(root: &Path, since: Option<DateTime<Local>>) -> (Vec<Record>, Warnings) {
    let mut warns = Warnings::default();
    let mut files = Vec::new();
    collect_rollouts(root, &mut files, &mut warns);
    let since_sys = since.map(SystemTime::from);

    let mut out = Vec::new();
    for fp in files {
        if let Some(cut) = since_sys
            && let Ok(mt) = fp.metadata().and_then(|m| m.modified())
            && mt < cut
        {
            continue;
        }
        // Bytes plus lossy per-line decoding, so a concurrent append cut mid-UTF-8-char cannot
        // reject the whole file (see claude.rs).
        let Ok(bytes) = std::fs::read(&fp) else {
            warns.unreadable_files += 1;
            continue;
        };
        let mut model = "codex".to_string();
        for raw in bytes.split(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(raw);
            if line.trim().is_empty() {
                continue;
            }
            match parse_codex_line(&line) {
                Ok(CodexLine::Model(m)) => model = m,
                Ok(CodexLine::Tokens { ts, usage }) => {
                    // No dup-suppression: one token_count per turn in an append-only log, and a
                    // timestamp+usage heuristic would drop two distinct turns that billed
                    // identically in the same millisecond.
                    if let Some(s) = since
                        && ts < s
                    {
                        continue;
                    }
                    out.push(Record {
                        tool: Tool::Codex,
                        model: model.clone(),
                        usage,
                    });
                }
                Ok(CodexLine::Other) => {}
                Err(()) => warns.skipped_lines += 1,
            }
        }
    }
    (out, warns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn token_line(ts: &str, input: u64, cached: u64, output: u64, reasoning: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"reasoning_output_tokens":{reasoning},"total_tokens":{}}}}}}}}}"#,
            input + output
        )
    }

    #[test]
    fn parse_maps_cached_and_never_double_counts_reasoning() {
        let line = token_line("2026-07-20T03:42:29.059Z", 18842, 9984, 199, 33);
        let CodexLine::Tokens { usage, .. } = parse_codex_line(&line).unwrap() else {
            panic!("expected Tokens");
        };
        assert_eq!(usage.input, 18842 - 9984); // uncached input
        assert_eq!(usage.cache_read, 9984);
        assert_eq!(usage.output, 199); // reasoning NOT added
        assert_eq!(usage.total(), 18842 + 199); // == total_tokens
    }

    #[test]
    fn parse_model_and_other() {
        let m = parse_codex_line(r#"{"payload":{"model":"gpt-5.6-sol"}}"#).unwrap();
        assert_eq!(m, CodexLine::Model("gpt-5.6-sol".into()));
        assert_eq!(
            parse_codex_line(r#"{"payload":{"type":"agent_message"}}"#).unwrap(),
            CodexLine::Other
        );
        assert!(parse_codex_line("{bad").is_err());
        let bad = r#"{"timestamp":"2026-07-20T12:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":"x","output_tokens":5}}}}"#;
        assert!(parse_codex_line(bad).is_err());
        let bad_container = r#"{"timestamp":"2026-07-20T12:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":"nope"}}}"#;
        assert!(parse_codex_line(bad_container).is_err());
    }

    #[test]
    fn scan_sums_all_deltas_and_carries_model() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/07/20");
        std::fs::create_dir_all(&day).unwrap();
        let mut f = std::fs::File::create(day.join("rollout-x.jsonl")).unwrap();
        writeln!(f, r#"{{"payload":{{"model":"gpt-5.6-sol"}}}}"#).unwrap();
        // Every token_count is a real delta: all three are summed, even two that bill identically.
        writeln!(
            f,
            "{}",
            token_line("2026-07-20T12:00:00.000Z", 100, 0, 20, 0)
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            token_line("2026-07-20T12:00:00.000Z", 100, 0, 20, 0)
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            token_line("2026-07-20T12:05:00.000Z", 50, 10, 5, 0)
        )
        .unwrap();
        drop(f);

        let (records, warns) = scan(dir.path(), None);
        assert!(warns.is_empty());
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|r| r.model == "gpt-5.6-sol"));
        let total: u64 = records.iter().map(|r| r.usage.total()).sum();
        assert_eq!(total, 295);
    }
}
