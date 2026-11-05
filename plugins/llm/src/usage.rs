//! Append-only JSONL usage log + read-side aggregation.
//!
//! Each `llm.complete` records a single line:
//!   {"ts":"2026-04-27T12:00:00Z","model":"claude-sonnet-4-6","input_tokens":12,"output_tokens":4,"source":"trigger:meeting-prep"}
//!
//! Single-syscall append via libc::write on an O_APPEND fd, the same atomicity contract as the KB
//! plugin's kb.append. On short write we error rather than retry so the one-record-per-line
//! invariant holds.
//!
//! `aggregate` scans the file linearly and rolls up by model, optionally within a time range; no
//! index, which is fine for personal-volume usage.

use std::fs;
use std::os::fd::AsRawFd;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageRecord {
    pub ts: String,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Caller label from `params.source`, passed through untouched (group spend by trigger).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

pub fn append(path: &Path, record: &UsageRecord) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut line = serde_json::to_string(record).map_err(|e| format!("serialize: {e}"))?;
    line.push('\n');
    let bytes = line.as_bytes();
    let f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let fd = f.as_raw_fd();
    // SAFETY: `f` outlives the write, so we own `fd` for its duration. write(2) on an O_APPEND fd
    // is atomic against other O_APPEND writers up to PIPE_BUF (typically 4096), which is what
    // keeps concurrent `llm.complete` calls from interleaving.
    let written = unsafe { libc::write(fd, bytes.as_ptr() as *const _, bytes.len()) };
    if written < 0 {
        return Err(format!(
            "write {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    if written as usize != bytes.len() {
        // A short write errors instead of retrying: a follow-up write on the same fd could be
        // sandwiched between two other writers' bytes.
        return Err(format!(
            "short write to {}: wrote {} of {} bytes",
            path.display(),
            written,
            bytes.len()
        ));
    }
    drop(f);
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelStats {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Aggregate {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub by_model: std::collections::BTreeMap<String, ModelStats>,
}

/// `since`/`until` are inclusive/exclusive RFC3339; `model_filter`
/// is exact-match. Malformed JSONL lines are counted as `parse_errors`
/// and skipped — aggregation never fails on a partial file.
pub fn aggregate(
    path: &Path,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    model_filter: Option<&str>,
) -> Result<(Aggregate, u64), String> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Aggregate::default(), 0));
        }
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut agg = Aggregate::default();
    let mut parse_errors: u64 = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record: UsageRecord = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };
        if let Some(filter) = model_filter
            && record.model != filter
        {
            continue;
        }
        // Always validate the timestamp: a malformed `ts` makes the record undatable, and counting
        // it silently corrupts the totals.
        let ts = match DateTime::parse_from_rfc3339(&record.ts) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };
        if let Some(s) = since
            && ts < s
        {
            continue;
        }
        if let Some(u) = until
            && ts >= u
        {
            continue;
        }
        agg.calls += 1;
        agg.input_tokens += record.input_tokens as u64;
        agg.output_tokens += record.output_tokens as u64;
        let entry = agg.by_model.entry(record.model.clone()).or_default();
        entry.calls += 1;
        entry.input_tokens += record.input_tokens as u64;
        entry.output_tokens += record.output_tokens as u64;
    }
    Ok((agg, parse_errors))
}

pub fn aggregate_to_json(agg: &Aggregate, parse_errors: u64) -> Value {
    let by_model: serde_json::Map<String, Value> = agg
        .by_model
        .iter()
        .map(|(m, s)| {
            (
                m.clone(),
                serde_json::json!({
                    "calls": s.calls,
                    "input_tokens": s.input_tokens,
                    "output_tokens": s.output_tokens,
                }),
            )
        })
        .collect();
    serde_json::json!({
        "calls": agg.calls,
        "input_tokens": agg.input_tokens,
        "output_tokens": agg.output_tokens,
        "by_model": Value::Object(by_model),
        "parse_errors": parse_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn rec(model: &str, ts: &str, ti: u32, to: u32, source: Option<&str>) -> UsageRecord {
        UsageRecord {
            ts: ts.to_string(),
            model: model.to_string(),
            input_tokens: ti,
            output_tokens: to,
            source: source.map(str::to_string),
        }
    }

    #[test]
    fn append_then_aggregate_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("usage.jsonl");
        append(&path, &rec("m1", "2026-01-01T00:00:00Z", 10, 5, None)).unwrap();
        append(
            &path,
            &rec("m2", "2026-01-02T00:00:00Z", 20, 10, Some("trg")),
        )
        .unwrap();
        append(&path, &rec("m1", "2026-01-03T00:00:00Z", 5, 3, None)).unwrap();
        let (agg, errs) = aggregate(&path, None, None, None).unwrap();
        assert_eq!(errs, 0);
        assert_eq!(agg.calls, 3);
        assert_eq!(agg.input_tokens, 35);
        assert_eq!(agg.output_tokens, 18);
        assert_eq!(agg.by_model.len(), 2);
        let m1 = &agg.by_model["m1"];
        assert_eq!(m1.calls, 2);
        assert_eq!(m1.input_tokens, 15);
        let m2 = &agg.by_model["m2"];
        assert_eq!(m2.calls, 1);
    }

    #[test]
    fn aggregate_filters_by_model() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("u.jsonl");
        append(&path, &rec("m1", "2026-01-01T00:00:00Z", 10, 5, None)).unwrap();
        append(&path, &rec("m2", "2026-01-02T00:00:00Z", 20, 10, None)).unwrap();
        let (agg, _) = aggregate(&path, None, None, Some("m1")).unwrap();
        assert_eq!(agg.calls, 1);
        assert_eq!(agg.by_model.len(), 1);
    }

    #[test]
    fn aggregate_filters_by_time_range() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("u.jsonl");
        append(&path, &rec("m1", "2026-01-01T00:00:00Z", 10, 5, None)).unwrap();
        append(&path, &rec("m1", "2026-01-15T00:00:00Z", 20, 10, None)).unwrap();
        append(&path, &rec("m1", "2026-02-01T00:00:00Z", 5, 3, None)).unwrap();
        let since = DateTime::parse_from_rfc3339("2026-01-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let until = DateTime::parse_from_rfc3339("2026-01-20T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (agg, _) = aggregate(&path, Some(since), Some(until), None).unwrap();
        assert_eq!(agg.calls, 1);
        assert_eq!(agg.input_tokens, 20);
    }

    #[test]
    fn aggregate_counts_parse_errors_separately_from_data() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("u.jsonl");
        fs::write(
            &path,
            "{\"ts\":\"2026-01-01T00:00:00Z\",\"model\":\"m1\",\"input_tokens\":1,\"output_tokens\":2}\n\
             not-json\n\
             {\"ts\":\"bad-ts\",\"model\":\"m1\",\"input_tokens\":1,\"output_tokens\":1}\n\
             {\"ts\":\"2026-01-02T00:00:00Z\",\"model\":\"m1\",\"input_tokens\":3,\"output_tokens\":4}\n",
        )
        .unwrap();
        let since = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (agg, errs) = aggregate(&path, Some(since), None, None).unwrap();
        assert_eq!(agg.calls, 2);
        // bad-ts (parse fail under time filter) + not-json
        assert_eq!(errs, 2);
    }

    #[test]
    fn aggregate_skips_malformed_ts_even_without_time_filter() {
        // Regression: without `since` / `until`, a record with valid JSON but a bad ts must still
        // be rejected so aggregate totals stay trustworthy.
        let dir = tempdir().unwrap();
        let path = dir.path().join("u.jsonl");
        fs::write(
            &path,
            "{\"ts\":\"2026-01-01T00:00:00Z\",\"model\":\"m1\",\"input_tokens\":1,\"output_tokens\":2}\n\
             {\"ts\":\"bad-ts\",\"model\":\"m1\",\"input_tokens\":99,\"output_tokens\":99}\n",
        )
        .unwrap();
        let (agg, errs) = aggregate(&path, None, None, None).unwrap();
        assert_eq!(agg.calls, 1);
        assert_eq!(agg.input_tokens, 1);
        assert_eq!(agg.output_tokens, 2);
        assert_eq!(errs, 1);
    }

    #[test]
    fn aggregate_returns_empty_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let (agg, errs) = aggregate(&path, None, None, None).unwrap();
        assert_eq!(agg.calls, 0);
        assert_eq!(errs, 0);
    }

    #[test]
    fn append_skips_extra_newlines_in_aggregate() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("u.jsonl");
        fs::write(&path, "\n\n").unwrap();
        append(&path, &rec("m1", "2026-01-01T00:00:00Z", 1, 1, None)).unwrap();
        let (agg, errs) = aggregate(&path, None, None, None).unwrap();
        assert_eq!(agg.calls, 1);
        assert_eq!(errs, 0);
    }
}
