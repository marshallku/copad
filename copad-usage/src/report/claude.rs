//! Scan `~/.claude/projects/<slug>/<uuid>.jsonl` for token usage.
//!
//! Claude Code writes the SAME assistant `message.id` on several JSONL lines (streaming/rewrite
//! snapshots, up to 5-7x, with identical final usage); summing lines overstated output tokens
//! ~2.75x. So records are deduped by `message.id`, keeping the snapshot with the largest total,
//! before the window filter and the sum.

use super::model::{RawUsage, Record, Tool, Warnings};
use chrono::{DateTime, Local};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeLine {
    /// `message.id`; `None` only if Claude ever omits it (kept un-deduped).
    pub id: Option<String>,
    pub ts: DateTime<Local>,
    pub model: String,
    pub usage: RawUsage,
}

/// A usage field: absent/null is `Some(0)` (an omitted category); a valid `u64` is `Some(n)`; the
/// wrong type (string, float, negative) is `None`, rejecting the whole record as malformed rather
/// than miscounting it as zero.
fn uget(v: &Value, k: &str) -> Option<u64> {
    match v.get(k) {
        None | Some(Value::Null) => Some(0),
        Some(x) => x.as_u64(),
    }
}

pub fn parse_claude_line(line: &str) -> Result<Option<ClaudeLine>, ()> {
    let v: Value = serde_json::from_str(line).map_err(|_| ())?;
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return Ok(None);
    }
    let Some(msg) = v.get("message") else {
        return Ok(None);
    };
    let Some(usage_v) = msg.get("usage") else {
        return Ok(None);
    };
    // A non-object `usage` is format drift: reject rather than read every field as 0.
    if !usage_v.is_object() {
        return Err(());
    }
    let usage = RawUsage {
        input: uget(usage_v, "input_tokens").ok_or(())?,
        cache_write: uget(usage_v, "cache_creation_input_tokens").ok_or(())?,
        cache_read: uget(usage_v, "cache_read_input_tokens").ok_or(())?,
        output: uget(usage_v, "output_tokens").ok_or(())?,
    };
    if usage.total() == 0 {
        return Ok(None);
    }
    let ts_str = v.get("timestamp").and_then(|t| t.as_str()).ok_or(())?;
    let ts = DateTime::parse_from_rfc3339(ts_str)
        .map_err(|_| ())?
        .with_timezone(&Local);
    let model = msg
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let id = msg.get("id").and_then(|i| i.as_str()).map(str::to_string);
    Ok(Some(ClaudeLine {
        id,
        ts,
        model,
        usage,
    }))
}

/// Recursively collect `*.jsonl` under `dir`. Subagent transcripts live at
/// `<project>/<session>/subagents/agent-*.jsonl`; a depth-1 scan undercounts them.
fn collect_jsonl(dir: &Path, out: &mut Vec<std::path::PathBuf>, warns: &mut Warnings) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        warns.unreadable_files += 1;
        return;
    };
    for e in rd.flatten() {
        // A dir entry's `file_type()` does not follow symlinks, so a symlinked directory is a
        // leaf and a symlink cycle cannot recurse forever.
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            collect_jsonl(&p, out, warns);
        } else if ft.is_file() && p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}

pub fn scan(root: &Path, since: Option<DateTime<Local>>) -> (Vec<Record>, Warnings) {
    let mut dedup: HashMap<String, (DateTime<Local>, String, RawUsage)> = HashMap::new();
    let mut warns = Warnings::default();
    let mut fallback = 0u64;
    let since_sys = since.map(SystemTime::from);

    let mut files = Vec::new();
    collect_jsonl(root, &mut files, &mut warns);
    for fp in files {
        if let Some(cut) = since_sys
            && let Ok(mt) = fp.metadata().and_then(|m| m.modified())
            && mt < cut
        {
            continue;
        }
        // Read bytes and decode lossily per line: an active file whose last line is a concurrent
        // append cut mid-UTF-8-char would make `read_to_string` reject the whole file. Lossy
        // decoding only breaks the truncated tail (skipped with a warning).
        let Ok(bytes) = std::fs::read(&fp) else {
            warns.unreadable_files += 1;
            continue;
        };
        for raw in bytes.split(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(raw);
            if line.trim().is_empty() {
                continue;
            }
            match parse_claude_line(&line) {
                Ok(Some(cl)) => {
                    let key = cl.id.unwrap_or_else(|| {
                        fallback += 1;
                        format!("__noid_{fallback}")
                    });
                    dedup
                        .entry(key)
                        .and_modify(|e| {
                            if cl.usage.total() > e.2.total() {
                                *e = (cl.ts, cl.model.clone(), cl.usage);
                            }
                        })
                        .or_insert((cl.ts, cl.model, cl.usage));
                }
                Ok(None) => {}
                Err(()) => warns.skipped_lines += 1,
            }
        }
    }

    let mut out = Vec::new();
    for (_id, (ts, model, usage)) in dedup {
        if let Some(s) = since
            && ts < s
        {
            continue;
        }
        out.push(Record {
            tool: Tool::Claude,
            model,
            usage,
        });
    }
    (out, warns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn assistant_line(id: &str, ts: &str, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","model":"claude-opus-4-8","usage":{{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":5,"output_tokens":{out}}}}}}}"#
        )
    }

    #[test]
    fn parse_valid_and_non_assistant() {
        let line = assistant_line("msg_1", "2026-07-20T12:00:00.000Z", 100);
        let parsed = parse_claude_line(&line).unwrap().unwrap();
        assert_eq!(parsed.id.as_deref(), Some("msg_1"));
        assert_eq!(parsed.usage.output, 100);
        assert_eq!(parsed.usage.cache_read, 5);
        assert!(parse_claude_line(r#"{"type":"user"}"#).unwrap().is_none());
        assert!(parse_claude_line("{not json").is_err());
        // wrong-type usage field is rejected, not silently zeroed
        let bad = r#"{"type":"assistant","timestamp":"2026-07-20T12:00:00.000Z","message":{"id":"m","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":"oops"}}}"#;
        assert!(parse_claude_line(bad).is_err());
        let bad_container = r#"{"type":"assistant","timestamp":"2026-07-20T12:00:00.000Z","message":{"id":"m","model":"claude-opus-4-8","usage":"invalid"}}"#;
        assert!(parse_claude_line(bad_container).is_err());
    }

    #[test]
    fn scan_dedupes_repeated_message_id() {
        // the same message.id written 3x counts once
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj-a");
        std::fs::create_dir(&proj).unwrap();
        let mut f = std::fs::File::create(proj.join("s.jsonl")).unwrap();
        let dup = assistant_line("msg_dup", "2026-07-20T12:00:00.000Z", 500);
        writeln!(f, "{dup}").unwrap();
        writeln!(f, "{dup}").unwrap();
        writeln!(f, "{dup}").unwrap();
        let other = assistant_line("msg_other", "2026-07-20T12:01:00.000Z", 200);
        writeln!(f, "{other}").unwrap();
        drop(f);

        let (records, warns) = scan(dir.path(), None);
        let total_output: u64 = records.iter().map(|r| r.usage.output).sum();
        assert_eq!(total_output, 700);
        assert_eq!(records.len(), 2);
        assert!(warns.is_empty());
    }

    #[test]
    fn scan_counts_malformed_final_line() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj-b");
        std::fs::create_dir(&proj).unwrap();
        let mut f = std::fs::File::create(proj.join("s.jsonl")).unwrap();
        writeln!(
            f,
            "{}",
            assistant_line("m1", "2026-07-20T12:00:00.000Z", 10)
        )
        .unwrap();
        write!(f, r#"{{"type":"assistant","messag"#).unwrap();
        drop(f);
        let (records, warns) = scan(dir.path(), None);
        assert_eq!(records.len(), 1);
        assert_eq!(warns.skipped_lines, 1);
    }

    #[test]
    fn scan_preserves_valid_lines_when_tail_is_truncated_multibyte() {
        // A final line cut mid-multibyte-char must not lose the valid records before it.
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj-utf8");
        std::fs::create_dir(&proj).unwrap();
        let mut f = std::fs::File::create(proj.join("s.jsonl")).unwrap();
        f.write_all(assistant_line("m_ok", "2026-07-20T12:00:00.000Z", 77).as_bytes())
            .unwrap();
        f.write_all(b"\n").unwrap();
        // truncated tail: `{"` then a lone 0xED (start of a 3-byte char) = invalid UTF-8
        f.write_all(&[0x7b, 0x22, 0xED]).unwrap();
        drop(f);

        let (records, warns) = scan(dir.path(), None);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].usage.output, 77);
        assert_eq!(warns.skipped_lines, 1); // only the truncated tail skipped
    }

    #[test]
    fn scan_recurses_into_nested_subagent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj-c");
        let nested = proj.join("session-1/subagents");
        std::fs::create_dir_all(&nested).unwrap();
        let mut top = std::fs::File::create(proj.join("session-1.jsonl")).unwrap();
        writeln!(
            top,
            "{}",
            assistant_line("m_top", "2026-07-20T12:00:00.000Z", 100)
        )
        .unwrap();
        let mut sub = std::fs::File::create(nested.join("agent-abc.jsonl")).unwrap();
        writeln!(
            sub,
            "{}",
            assistant_line("m_sub", "2026-07-20T12:01:00.000Z", 40)
        )
        .unwrap();
        drop((top, sub));

        let (records, _) = scan(dir.path(), None);
        let total: u64 = records.iter().map(|r| r.usage.output).sum();
        assert_eq!(records.len(), 2);
        assert_eq!(total, 140);
    }
}
