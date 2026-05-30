//! Two-token Slack credential store.
//!
//! Slack Socket Mode requires both a Bot User OAuth Token (`xoxb-...`,
//! HTTP API auth) and an App-Level Token (`xapp-...`, WebSocket auth).
//! Both are persisted; the env-var values used at startup take
//! precedence over what's in the store, but `auth.test`-validated
//! tokens are written here so a future restart can run without
//! re-supplying the env each time.
//!
//! Keyring is preferred (Linux Secret Service / macOS Keychain).
//! On failure, falls back to plaintext at
//! `$XDG_CONFIG_HOME/copad/slack-tokens-<workspace>.json` (mode 0600,
//! atomic-replace via per-call temp + rename) with a stderr warning
//! on every open. `COPAD_SLACK_REQUIRE_SECURE_STORE=1` forbids the
//! plaintext fallback entirely — token operations error instead of
//! writing plaintext, while RPC init still succeeds (analogous to
//! calendar plugin's degraded mode).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::Config;

const KEYRING_SERVICE: &str = "copad-slack";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenSet {
    pub bot_token: String,
    pub app_token: String,
    pub team_id: Option<String>,
    pub user_id: Option<String>,
    /// `#[serde(default)]` load-bearing: keyring entries written before
    /// this field existed must still deserialize.
    #[serde(default)]
    pub user_token: String,
}

pub trait TokenStore: Send + Sync {
    fn load(&self) -> Option<TokenSet>;
    fn save(&self, tokens: &TokenSet) -> Result<(), String>;
    /// Test-only today; on the trait so a future `slack.logout` is uniform.
    #[allow(dead_code)]
    fn clear(&self) -> Result<(), String>;
    fn kind(&self) -> &'static str;
}

pub fn open_store(config: &Config) -> Box<dyn TokenStore> {
    match KeyringStore::open(&config.workspace_label) {
        Ok(s) => Box::new(s),
        Err(e) => {
            if config.require_secure_store {
                eprintln!(
                    "[slack] secure keyring unavailable AND COPAD_SLACK_REQUIRE_SECURE_STORE=1: {e}"
                );
                Box::new(BrokenStore { reason: e })
            } else {
                eprintln!(
                    "[slack] secure keyring unavailable, falling back to plaintext at {}: {e}",
                    config.plaintext_path.display()
                );
                Box::new(PlaintextStore::new(config.plaintext_path.clone()))
            }
        }
    }
}

// -- Keyring backend --

pub struct KeyringStore {
    entry: keyring::Entry,
}

impl KeyringStore {
    pub fn open(workspace: &str) -> Result<Self, String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, workspace)
            .map_err(|e| format!("keyring entry: {e}"))?;
        // Probe the secret-service backend so a real failure (D-Bus
        // unavailable, locked keyring) surfaces here rather than at
        // first save.
        match entry.get_password() {
            Ok(_) => Ok(Self { entry }),
            Err(keyring::Error::NoEntry) => Ok(Self { entry }),
            Err(e) => Err(format!("keyring probe: {e}")),
        }
    }
}

impl TokenStore for KeyringStore {
    fn load(&self) -> Option<TokenSet> {
        match self.entry.get_password() {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(t) => Some(t),
                Err(e) => {
                    eprintln!(
                        "[slack] keyring entry malformed (NOT a 'no tokens yet' state — \
                         credential backend may need attention): {e}"
                    );
                    None
                }
            },
            Err(keyring::Error::NoEntry) => None,
            Err(e) => {
                eprintln!(
                    "[slack] keyring backend FAILED while reading tokens — \
                     plugin will report not_authenticated, but the underlying \
                     issue is the credential store, not missing tokens: {e}"
                );
                None
            }
        }
    }

    fn save(&self, tokens: &TokenSet) -> Result<(), String> {
        let s = serde_json::to_string(tokens).map_err(|e| format!("serialize: {e}"))?;
        self.entry
            .set_password(&s)
            .map_err(|e| format!("keyring write: {e}"))
    }

    fn clear(&self) -> Result<(), String> {
        match self.entry.delete_credential() {
            Ok(_) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("keyring clear: {e}")),
        }
    }

    fn kind(&self) -> &'static str {
        "keyring"
    }
}

// -- Plaintext fallback --

pub struct PlaintextStore {
    path: PathBuf,
}

impl PlaintextStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl TokenStore for PlaintextStore {
    fn load(&self) -> Option<TokenSet> {
        // Distinguish "no token file yet" (NotFound — quiet) from
        // "we cannot read the file" (EACCES, EIO, etc. — log so
        // operators can debug). Round-6 cross-review I2.
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                eprintln!(
                    "[slack] plaintext token store unreadable at {}: {e} \
                     (auth_status will report not_authenticated)",
                    self.path.display()
                );
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "[slack] plaintext token store malformed at {}: {e}",
                    self.path.display()
                );
                None
            }
        }
    }

    fn save(&self, tokens: &TokenSet) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_vec(tokens).map_err(|e| format!("serialize: {e}"))?;
        write_atomic_0600(&self.path, &json).map_err(|e| format!("write: {e}"))
    }

    fn clear(&self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove {}: {e}", self.path.display())),
        }
    }

    fn kind(&self) -> &'static str {
        "plaintext"
    }
}

/// Per-process counter so concurrent `save()` calls don't collide on
/// a pid-derived temp path. Same pattern as calendar plugin.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn write_atomic_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".slack-tokens-{}-{}.tmp", std::process::id(), seq,));
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

// -- Sentinel store for require_secure_store mode when keyring is broken --

struct BrokenStore {
    reason: String,
}

impl TokenStore for BrokenStore {
    fn load(&self) -> Option<TokenSet> {
        None
    }
    fn save(&self, _: &TokenSet) -> Result<(), String> {
        Err(format!(
            "secure store required but keyring is unavailable: {}",
            self.reason
        ))
    }
    fn clear(&self) -> Result<(), String> {
        Ok(())
    }
    fn kind(&self) -> &'static str {
        "unavailable"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn sample() -> TokenSet {
        TokenSet {
            bot_token: "xoxb-bot".into(),
            app_token: "xapp-app".into(),
            team_id: Some("T012345".into()),
            user_id: Some("U012345".into()),
            user_token: String::new(),
        }
    }

    #[test]
    fn plaintext_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("toks.json");
        let store = PlaintextStore::new(path.clone());
        assert!(store.load().is_none());
        store.save(&sample()).unwrap();
        assert_eq!(store.load(), Some(sample()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0600, got 0o{mode:o}");
        }

        store.clear().unwrap();
        assert!(store.load().is_none());
        store.clear().unwrap();
    }

    #[test]
    fn plaintext_concurrent_saves_use_distinct_temp_paths() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("toks.json");
        let store = Arc::new(PlaintextStore::new(path));
        let mut handles = Vec::new();
        for i in 0..16 {
            let s = store.clone();
            handles.push(std::thread::spawn(move || {
                let mut t = sample();
                t.bot_token = format!("xoxb-{i}");
                s.save(&t).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(store.load().is_some());
    }

    #[test]
    fn broken_store_reports_in_save() {
        let s = BrokenStore {
            reason: "no D-Bus".into(),
        };
        assert_eq!(s.kind(), "unavailable");
        assert!(s.load().is_none());
        let err = s.save(&sample()).unwrap_err();
        assert!(err.contains("no D-Bus"), "got {err}");
    }

    #[test]
    fn token_set_deserializes_without_user_token_field() {
        let legacy = r#"{
            "bot_token": "xoxb-legacy",
            "app_token": "xapp-legacy",
            "team_id": "T999",
            "user_id": "U999"
        }"#;
        let parsed: TokenSet = serde_json::from_str(legacy).expect("legacy JSON must load");
        assert_eq!(parsed.bot_token, "xoxb-legacy");
        assert_eq!(parsed.app_token, "xapp-legacy");
        assert_eq!(parsed.team_id.as_deref(), Some("T999"));
        assert_eq!(parsed.user_id.as_deref(), Some("U999"));
        assert_eq!(parsed.user_token, "");
    }

    #[test]
    fn token_set_round_trips_user_token() {
        let mut t = sample();
        t.user_token = "xoxp-mine".into();
        let json = serde_json::to_string(&t).unwrap();
        let parsed: TokenSet = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.user_token, "xoxp-mine");
    }
}
