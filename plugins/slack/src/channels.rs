//! Channel registry: per-channel profile + bot-rpc config.
//!
//! Channels live at `$XDG_CONFIG_HOME/copad/slack-channels-<workspace>.json`
//! (mode 0600, atomic temp+rename — same pattern as PlaintextStore). The
//! file is small (≤ ~30 entries assumed) so every save rewrites it in
//! full; no index / pagination.
//!
//! Profile semantics — the action loop that consumes these is intentionally
//! not in this slice:
//! - `read` — passive observation only
//! - `collect` — earmarked for knowledge-base capture (collect logic TBD)
//! - `bot-rpc` — generic "post a message, wait for a matching response, fire
//!   a follow-up action" pattern. `wait_mode`/`wait_regex`/`wait_user_filter`
//!   / `wait_timeout_ms` define the response-match contract. NOT Jira-specific;
//!   the user's Jira-bot integration is one instance of the pattern.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u32 = 1;
const MAX_NAME_LEN: usize = 80;
const MAX_TEMPLATE_LEN: usize = 4000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelProfile {
    Read,
    Collect,
    BotRpc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WaitMode {
    FirstReply,
    Regex,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BotRpcConfig {
    #[serde(default)]
    pub default_template: String,
    pub wait_mode: WaitMode,
    #[serde(default)]
    pub wait_regex: String,
    #[serde(default)]
    pub wait_user_filter: String,
    #[serde(default)]
    pub wait_timeout_ms: u64,
}

/// Placeholder for the future `collect` profile config (destination,
/// format, filters). Empty in this slice — the field exists so future
/// schema additions don't require a migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollectConfig {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub profile: ChannelProfile,
    #[serde(default)]
    pub bot_rpc: Option<BotRpcConfig>,
    #[serde(default)]
    pub collect: Option<CollectConfig>,
    /// Unix epoch milliseconds. Millisecond resolution lets two upserts
    /// in the same second still differentiate `updated_at` from the
    /// previous value.
    pub added_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelFile {
    pub version: u32,
    #[serde(default)]
    pub channels: Vec<ChannelEntry>,
}

impl Default for ChannelFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            channels: Vec::new(),
        }
    }
}

/// `^[CDGU][A-Z0-9]+$`. C=public channel, D=DM, G=private group, U=user
/// (rare in panel context but accepted for shared-channel mirrors).
pub fn is_valid_channel_id(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    matches!(bytes[0], b'C' | b'D' | b'G' | b'U')
        && bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Channel prefixes that have meaningful `name`s in Slack
/// (public + private channels). DMs and user IDs do not.
pub fn channel_supports_name(id: &str) -> bool {
    matches!(id.as_bytes().first(), Some(b'C') | Some(b'G'))
}

/// Validates an entry before persisting. Returns an `invalid_params`-style
/// message on rejection (caller maps to RPC error code).
pub fn validate_entry(entry: &ChannelEntry) -> Result<(), String> {
    if !is_valid_channel_id(&entry.id) {
        return Err(format!(
            "'id' must match [CDGU][A-Z0-9]+ (Slack channel id); got {:?}",
            entry.id
        ));
    }
    if entry.name.chars().count() > MAX_NAME_LEN {
        return Err(format!("'name' exceeds {MAX_NAME_LEN} chars"));
    }
    match (&entry.profile, &entry.bot_rpc) {
        (ChannelProfile::BotRpc, None) => {
            return Err("profile=bot-rpc requires 'bot_rpc' config".to_string());
        }
        (ChannelProfile::BotRpc, Some(cfg)) => validate_bot_rpc(cfg)?,
        // `bot_rpc` config on a non-bot-rpc profile is preserved (so UI can
        // switch profiles without losing the settings) — not an error.
        _ => {}
    }
    Ok(())
}

fn validate_bot_rpc(cfg: &BotRpcConfig) -> Result<(), String> {
    if cfg.default_template.chars().count() > MAX_TEMPLATE_LEN {
        return Err(format!(
            "'bot_rpc.default_template' exceeds {MAX_TEMPLATE_LEN} chars"
        ));
    }
    if cfg.wait_timeout_ms > MAX_TIMEOUT_MS {
        return Err(format!(
            "'bot_rpc.wait_timeout_ms' exceeds {MAX_TIMEOUT_MS} (1 hour)"
        ));
    }
    if matches!(cfg.wait_mode, WaitMode::Regex) {
        if cfg.wait_regex.is_empty() {
            return Err("'bot_rpc.wait_regex' required when wait_mode=regex".to_string());
        }
        regex::Regex::new(&cfg.wait_regex)
            .map_err(|e| format!("'bot_rpc.wait_regex' compile error: {e}"))?;
    }
    if !cfg.wait_user_filter.is_empty() && !is_valid_channel_id(&cfg.wait_user_filter) {
        // Filter is a Slack user id (Uxxx) — same charset as channel id.
        // Reject malformed input early so the runtime never compares against
        // garbage strings.
        return Err(format!(
            "'bot_rpc.wait_user_filter' must be a Slack user id (Uxxx); got {:?}",
            cfg.wait_user_filter
        ));
    }
    Ok(())
}

pub struct ChannelStore {
    path: PathBuf,
    // The store is touched from at least two places (RPC dispatch + future
    // collect/bot-rpc loops). Serializing through one Mutex avoids torn
    // reads if a save() races a load().
    guard: Mutex<()>,
}

impl ChannelStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            guard: Mutex::new(()),
        }
    }

    pub fn load(&self) -> ChannelFile {
        let _g = self.guard.lock().unwrap_or_else(|e| e.into_inner());
        match fs::read(&self.path) {
            Ok(bytes) => match serde_json::from_slice::<ChannelFile>(&bytes) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!(
                        "[slack] channel file malformed at {}: {e} \
                         — treating as empty (data preserved at .corrupt sibling)",
                        self.path.display()
                    );
                    // Don't silently drop the user's data on a parse error.
                    // Move the corrupt file aside so a future load returns
                    // empty (writable) instead of erroring forever.
                    let _ = fs::rename(&self.path, self.path.with_extension("corrupt"));
                    ChannelFile::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ChannelFile::default(),
            Err(e) => {
                eprintln!(
                    "[slack] channel file unreadable at {}: {e} \
                     — returning empty (channel state will not be persisted until fixed)",
                    self.path.display()
                );
                ChannelFile::default()
            }
        }
    }

    /// Insert or replace by `id`. Returns the canonical entry (with
    /// `added_at`/`updated_at` filled). `added_at` is preserved across
    /// updates so the UI can show stable "added on" timestamps.
    pub fn upsert(&self, mut entry: ChannelEntry) -> Result<ChannelEntry, String> {
        validate_entry(&entry)?;
        let _g = self.guard.lock().unwrap_or_else(|e| e.into_inner());
        let now = unix_now();
        let mut file = self.load_locked();
        let prev_updated = file
            .channels
            .iter()
            .find(|c| c.id == entry.id)
            .map(|c| (c.added_at, c.updated_at));
        if let Some((added_at, _)) = prev_updated {
            entry.added_at = added_at;
        } else if entry.added_at == 0 {
            entry.added_at = now;
        }
        // Strict monotonic `updated_at`: if the wall clock has not
        // advanced past the previous value (same millisecond, or clock
        // skew), still bump by 1. AC requires updated_at to refresh on
        // every upsert; the clock is not a sufficient guarantee.
        entry.updated_at = match prev_updated {
            Some((_, prev)) if now <= prev => prev + 1,
            _ => now,
        };
        if let Some(slot) = file.channels.iter_mut().find(|c| c.id == entry.id) {
            *slot = entry.clone();
        } else {
            file.channels.push(entry.clone());
        }
        self.save_locked(&file)?;
        Ok(entry)
    }

    /// Idempotent: returns `false` when nothing was removed.
    pub fn remove(&self, id: &str) -> Result<bool, String> {
        let _g = self.guard.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.load_locked();
        let before = file.channels.len();
        file.channels.retain(|c| c.id != id);
        if file.channels.len() == before {
            return Ok(false);
        }
        self.save_locked(&file)?;
        Ok(true)
    }

    fn load_locked(&self) -> ChannelFile {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => ChannelFile::default(),
        }
    }

    fn save_locked(&self, file: &ChannelFile) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let bytes =
            serde_json::to_vec_pretty(file).map_err(|e| format!("serialize channels: {e}"))?;
        write_atomic_0600(&self.path, &bytes).map_err(|e| format!("write channels: {e}"))
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Per-process counter so concurrent saves don't collide on a pid-derived
/// temp path. Same pattern as `store::write_atomic_0600`.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn write_atomic_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".slack-channels-{}-{}.tmp",
        std::process::id(),
        seq
    ));
    {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn sample_read(id: &str) -> ChannelEntry {
        ChannelEntry {
            id: id.into(),
            name: String::new(),
            profile: ChannelProfile::Read,
            bot_rpc: None,
            collect: None,
            added_at: 0,
            updated_at: 0,
        }
    }

    fn sample_bot_rpc(id: &str) -> ChannelEntry {
        ChannelEntry {
            id: id.into(),
            name: "team-jira".into(),
            profile: ChannelProfile::BotRpc,
            bot_rpc: Some(BotRpcConfig {
                default_template: "/jira CHA-123".into(),
                wait_mode: WaitMode::Regex,
                wait_regex: r"^(?<key>[A-Z]+-\d+)".into(),
                wait_user_filter: "U0BOT00001".into(),
                wait_timeout_ms: 30_000,
            }),
            collect: None,
            added_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn channel_id_validation() {
        assert!(is_valid_channel_id("C0123ABC"));
        assert!(is_valid_channel_id("D01XYZ"));
        assert!(is_valid_channel_id("G02NOPQ"));
        assert!(is_valid_channel_id("U0H0H0H"));
        assert!(!is_valid_channel_id(""));
        assert!(!is_valid_channel_id("c0123abc"), "lowercase rejected");
        assert!(!is_valid_channel_id("X12345"), "wrong prefix rejected");
        assert!(!is_valid_channel_id("C"), "prefix-only rejected");
        assert!(!is_valid_channel_id("C ABC"), "whitespace rejected");
    }

    #[test]
    fn validate_rejects_bad_regex() {
        let mut e = sample_bot_rpc("C01");
        e.bot_rpc.as_mut().unwrap().wait_regex = "[invalid".into();
        let err = validate_entry(&e).unwrap_err();
        assert!(err.contains("compile error"), "got {err}");
    }

    #[test]
    fn validate_rejects_regex_mode_empty_pattern() {
        let mut e = sample_bot_rpc("C01");
        e.bot_rpc.as_mut().unwrap().wait_regex = String::new();
        let err = validate_entry(&e).unwrap_err();
        assert!(err.contains("required when wait_mode=regex"), "got {err}");
    }

    #[test]
    fn validate_accepts_first_reply_with_empty_regex() {
        let mut e = sample_bot_rpc("C01");
        let cfg = e.bot_rpc.as_mut().unwrap();
        cfg.wait_mode = WaitMode::FirstReply;
        cfg.wait_regex = String::new();
        validate_entry(&e).expect("first-reply mode doesn't need regex");
    }

    #[test]
    fn validate_rejects_botrpc_profile_without_config() {
        let mut e = sample_read("C01");
        e.profile = ChannelProfile::BotRpc;
        e.bot_rpc = None;
        let err = validate_entry(&e).unwrap_err();
        assert!(err.contains("requires 'bot_rpc' config"), "got {err}");
    }

    #[test]
    fn validate_preserves_botrpc_on_read_profile() {
        // Switching profile bot-rpc → read in the UI should keep the
        // bot-rpc config around (so toggling back doesn't lose input).
        let mut e = sample_bot_rpc("C01");
        e.profile = ChannelProfile::Read;
        validate_entry(&e).expect("read profile + stale bot_rpc config is OK");
    }

    #[test]
    fn validate_rejects_timeout_over_max() {
        let mut e = sample_bot_rpc("C01");
        e.bot_rpc.as_mut().unwrap().wait_timeout_ms = MAX_TIMEOUT_MS + 1;
        assert!(validate_entry(&e).is_err());
    }

    #[test]
    fn validate_rejects_bad_user_filter() {
        let mut e = sample_bot_rpc("C01");
        e.bot_rpc.as_mut().unwrap().wait_user_filter = "not_a_slack_id".into();
        assert!(validate_entry(&e).is_err());
    }

    #[test]
    fn round_trip_and_upsert_preserves_added_at() {
        let dir = tempdir().unwrap();
        let store = ChannelStore::new(dir.path().join("nested").join("c.json"));

        let mut e = sample_read("C0123ABC");
        e.name = "team-infra".into();
        let saved = store.upsert(e.clone()).unwrap();
        assert_eq!(saved.added_at, saved.updated_at);
        assert!(saved.added_at > 0);
        let first_added = saved.added_at;

        let mut e2 = saved.clone();
        e2.name = "team-infra-renamed".into();
        let saved2 = store.upsert(e2).unwrap();
        assert_eq!(saved2.added_at, first_added, "added_at preserved");
        // Monotonic guarantee: even if the clock didn't advance, the
        // second upsert's updated_at must be strictly greater.
        assert!(
            saved2.updated_at > saved.updated_at,
            "updated_at strictly advances on repeated upsert"
        );

        let loaded = store.load();
        assert_eq!(loaded.channels.len(), 1);
        assert_eq!(loaded.channels[0].name, "team-infra-renamed");
    }

    #[test]
    fn upsert_updated_at_is_strictly_monotonic_back_to_back() {
        // Exercises the same-millisecond branch of upsert(): N back-to-back
        // upserts on a fast machine will frequently share a wall-clock
        // millisecond. Every call must still produce a strictly increasing
        // updated_at.
        let dir = tempdir().unwrap();
        let store = ChannelStore::new(dir.path().join("c.json"));
        store.upsert(sample_read("C0123ABC")).unwrap();
        let mut prev = store.load().channels[0].updated_at;
        for _ in 0..32 {
            let saved = store.upsert(sample_read("C0123ABC")).unwrap();
            assert!(
                saved.updated_at > prev,
                "expected {} > {prev}",
                saved.updated_at
            );
            prev = saved.updated_at;
        }
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = ChannelStore::new(dir.path().join("c.json"));
        store.upsert(sample_read("C0123ABC")).unwrap();
        assert!(store.remove("C0123ABC").unwrap());
        assert!(!store.remove("C0123ABC").unwrap());
        assert!(!store.remove("CDOES_NOT_EXIST").unwrap());
    }

    #[test]
    fn upsert_rejects_invalid_id() {
        let dir = tempdir().unwrap();
        let store = ChannelStore::new(dir.path().join("c.json"));
        assert!(store.upsert(sample_read("lowercase")).is_err());
        assert!(store.upsert(sample_read("")).is_err());
    }

    #[test]
    fn load_returns_default_on_missing_file() {
        let dir = tempdir().unwrap();
        let store = ChannelStore::new(dir.path().join("never-created.json"));
        let f = store.load();
        assert_eq!(f.version, SCHEMA_VERSION);
        assert!(f.channels.is_empty());
    }

    #[test]
    fn load_quarantines_corrupt_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.json");
        fs::write(&path, b"not json").unwrap();
        let store = ChannelStore::new(path.clone());
        let f = store.load();
        assert!(f.channels.is_empty());
        assert!(path.with_extension("corrupt").exists());
        assert!(!path.exists(), "original rename should clear the path");
    }

    #[test]
    fn entry_deserializes_with_minimal_fields() {
        // Future-proof: a future writer may omit fields we add later. The
        // current shape must round-trip through a minimal JSON without
        // breaking (already-saved files in older formats forward-compat).
        let minimal = r#"{
            "id": "C0123ABC",
            "profile": "read",
            "added_at": 100,
            "updated_at": 200
        }"#;
        let parsed: ChannelEntry = serde_json::from_str(minimal).expect("minimal load");
        assert_eq!(parsed.name, "");
        assert!(parsed.bot_rpc.is_none());
    }

    #[test]
    fn file_deserializes_with_only_version() {
        let s = r#"{"version": 1}"#;
        let f: ChannelFile = serde_json::from_str(s).unwrap();
        assert!(f.channels.is_empty());
    }

    #[test]
    fn concurrent_upsert_does_not_corrupt_file() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ChannelStore::new(dir.path().join("c.json")));
        let mut handles = Vec::new();
        for i in 0..16 {
            let s = store.clone();
            handles.push(std::thread::spawn(move || {
                let mut e = sample_read(&format!("C00000{i:03}"));
                e.name = format!("ch-{i}");
                s.upsert(e).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let f = store.load();
        assert_eq!(f.channels.len(), 16);
    }

    #[test]
    fn permissions_are_0600_on_unix() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.json");
        let store = ChannelStore::new(path.clone());
        store.upsert(sample_read("C01ABC")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn profile_serializes_as_kebab_case() {
        let json = serde_json::to_string(&ChannelProfile::BotRpc).unwrap();
        assert_eq!(json, "\"bot-rpc\"");
        let parsed: ChannelProfile = serde_json::from_str("\"bot-rpc\"").unwrap();
        assert_eq!(parsed, ChannelProfile::BotRpc);
    }

    #[test]
    fn wait_mode_serializes_as_kebab_case() {
        let json = serde_json::to_string(&WaitMode::FirstReply).unwrap();
        assert_eq!(json, "\"first-reply\"");
    }
}
