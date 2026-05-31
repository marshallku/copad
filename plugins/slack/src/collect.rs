//! Knowledge-base capture for `collect`-profile channels.
//!
//! When a human message arrives in a channel registered with
//! `ChannelProfile::Collect`, one JSONL line is appended to
//! `<collect_dir>/<channel_id>.jsonl`. Files live under XDG_DATA_HOME
//! (not config) because the captured stream is data.
//!
//! Append uses `O_APPEND | O_CREAT` so concurrent writers within the
//! same plugin process see line-atomic appends for payloads under
//! `PIPE_BUF` (4 KB on macOS/Linux). Slack messages can exceed that
//! when blocks/files are attached, so the writer flushes after each
//! line and treats failures as best-effort (log to stderr, drop the
//! line). Multi-process collectors are not in scope — only one
//! plugin instance per workspace.
//!
//! Each line carries the same identity Slack guarantees `(channel,
//! ts)`-unique, plus the team/event ids so downstream `jq` consumers
//! can dedupe by `event_id` if redelivery happens after a crash.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::channels::is_valid_channel_id;

#[derive(Debug, Clone, Serialize)]
pub struct CollectLine {
    pub channel: String,
    pub ts: String,
    pub user: String,
    pub text: String,
    pub thread_ts: Option<String>,
    pub team_id: Option<String>,
    pub event_id: Option<String>,
    pub captured_at_ms: u64,
}

pub struct CollectStore {
    base_dir: PathBuf,
}

impl CollectStore {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Append one line to `<base_dir>/<channel_id>.jsonl`. Channel id is
    /// re-validated here even though ChannelStore checks on persist —
    /// `channel_id` here ultimately comes from a Slack event payload,
    /// which is external input.
    pub fn append(&self, line: &CollectLine) -> Result<(), String> {
        if !is_valid_channel_id(&line.channel) {
            return Err(format!(
                "collect: rejected non-Slack channel id {:?}",
                line.channel
            ));
        }
        let path = self.base_dir.join(format!("{}.jsonl", line.channel));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let mut buf = serde_json::to_vec(line).map_err(|e| format!("serialize: {e}"))?;
        buf.push(b'\n');
        append_0600(&path, &buf).map_err(|e| format!("append {}: {e}", path.display()))
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn append_0600(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tempfile::tempdir;

    fn sample(channel: &str, ts: &str, user: &str, text: &str) -> CollectLine {
        CollectLine {
            channel: channel.into(),
            ts: ts.into(),
            user: user.into(),
            text: text.into(),
            thread_ts: None,
            team_id: Some("T0123".into()),
            event_id: Some("Ev0".into()),
            captured_at_ms: now_ms(),
        }
    }

    #[test]
    fn append_creates_file_and_dir() {
        let dir = tempdir().unwrap();
        let store = CollectStore::new(dir.path().join("nested").join("ws"));
        store
            .append(&sample("C0123ABC", "1700000000.000100", "U999", "hello"))
            .unwrap();
        let path = dir.path().join("nested").join("ws").join("C0123ABC.jsonl");
        assert!(path.exists());
    }

    #[test]
    fn multi_append_yields_valid_jsonl() {
        let dir = tempdir().unwrap();
        let store = CollectStore::new(dir.path().to_path_buf());
        for i in 0..5 {
            store
                .append(&sample(
                    "C0123ABC",
                    &format!("1700000000.{:06}", i),
                    "U999",
                    &format!("msg-{i}"),
                ))
                .unwrap();
        }
        let path = dir.path().join("C0123ABC.jsonl");
        let raw = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 5);
        for (i, l) in lines.iter().enumerate() {
            let v: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("line {i}: {e}"));
            assert_eq!(v["channel"], "C0123ABC");
            assert_eq!(v["user"], "U999");
            assert!(v["text"].as_str().unwrap().starts_with("msg-"));
        }
    }

    #[test]
    fn rejects_invalid_channel_id() {
        let dir = tempdir().unwrap();
        let store = CollectStore::new(dir.path().to_path_buf());
        let bad = sample("../etc/passwd", "1700000000.000100", "U999", "boom");
        assert!(store.append(&bad).is_err());
        // Nothing should have been written
        assert!(!dir.path().join("../etc/passwd.jsonl").exists());
    }

    #[test]
    fn permissions_are_0600_on_unix() {
        let dir = tempdir().unwrap();
        let store = CollectStore::new(dir.path().to_path_buf());
        store
            .append(&sample("C01ABC", "1700000000.000100", "U999", "hi"))
            .unwrap();
        let path = dir.path().join("C01ABC.jsonl");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn appends_preserve_existing_lines() {
        // A second CollectStore for the same path must not truncate
        // the file; this is the core "append not overwrite" contract.
        let dir = tempdir().unwrap();
        let store_a = CollectStore::new(dir.path().to_path_buf());
        store_a
            .append(&sample("C01", "1700000000.000100", "U1", "from-a"))
            .unwrap();
        let store_b = CollectStore::new(dir.path().to_path_buf());
        store_b
            .append(&sample("C01", "1700000000.000200", "U2", "from-b"))
            .unwrap();
        let raw = fs::read_to_string(dir.path().join("C01.jsonl")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("from-a"));
        assert!(lines[1].contains("from-b"));
    }
}
