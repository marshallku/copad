//! Token persistence with two backends:
//!
//! - `KeyringStore` — OS-native secret store (Linux Secret Service /
//!   macOS Keychain). Preferred. Failures during open are not fatal —
//!   we fall through to plaintext unless `require_secure_store=true`.
//! - `PlaintextStore` — JSON at `$XDG_CONFIG_HOME/copad/jira-token-<workspace>.json`,
//!   mode 0600. Used only when keyring is unavailable AND
//!   `require_secure_store=false`. Emits a stderr warning on every
//!   open so the user notices.
//!
//! Both stores serialize the same `TokenSet` JSON. Identical structure
//! to calendar/slack/discord — only the `TokenSet` fields differ.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::Config;

const KEYRING_SERVICE: &str = "copad-jira";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenSet {
    pub email: String,
    pub api_token: String,
    /// Stored so the host survives env-var removal after a one-time auth.
    pub base_url: String,
    pub account_id: String,
    pub display_name: String,
}

pub trait TokenStore: Send + Sync {
    fn load(&self) -> Option<TokenSet>;
    fn save(&self, t: &TokenSet) -> Result<(), String>;
    /// Test-only today; on the trait so a future `jira.logout` is uniform.
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
                    "[jira] secure keyring unavailable AND COPAD_JIRA_REQUIRE_SECURE_STORE=1: {e}"
                );
                Box::new(BrokenStore { reason: e })
            } else {
                eprintln!(
                    "[jira] secure keyring unavailable, falling back to plaintext at {}: {e}",
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
        // Test that the underlying secret service is actually reachable
        // by attempting a get; ignore "no entry" but bubble up real
        // errors (e.g. D-Bus unavailable).
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
                        "[jira] keyring entry malformed (NOT a 'no tokens yet' state — \
                         credential backend may need attention): {e}"
                    );
                    None
                }
            },
            Err(keyring::Error::NoEntry) => None,
            Err(e) => {
                eprintln!(
                    "[jira] keyring backend FAILED while reading tokens — \
                     plugin will report not_authenticated, but the underlying \
                     issue is the credential store, not missing tokens: {e}"
                );
                None
            }
        }
    }

    fn save(&self, t: &TokenSet) -> Result<(), String> {
        let s = serde_json::to_string(t).map_err(|e| format!("serialize: {e}"))?;
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
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                // Distinguish unreadable-store failures (EACCES from
                // a chmod race / EIO from disk trouble) from "no
                // tokens yet" — both end up as not_authenticated,
                // but the operator needs to know whether the cause
                // is a credential gap or a broken store. Same posture
                // as slack/discord's plaintext load.
                eprintln!(
                    "[jira] plaintext token store at {} could not be read \
                     (treating as not_authenticated, but the underlying issue \
                     is the store, not missing tokens): {e}",
                    self.path.display()
                );
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "[jira] plaintext token store malformed at {}: {e}",
                    self.path.display()
                );
                None
            }
        }
    }

    fn save(&self, t: &TokenSet) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_vec(t).map_err(|e| format!("serialize: {e}"))?;
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

/// pid+seq → unique temp file per in-flight save (pid alone collides
/// when two saves run concurrently in the same process).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn write_atomic_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".jira-token-{}-{}.tmp", std::process::id(), seq,));
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

    fn sample_tokens() -> TokenSet {
        TokenSet {
            email: "marshall@example.com".into(),
            api_token: "atlassian-token-xyz".into(),
            base_url: "https://x.atlassian.net".into(),
            account_id: "5b1234567890abcdef".into(),
            display_name: "Marshall Ku".into(),
        }
    }

    #[test]
    fn plaintext_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("tok.json");
        let store = PlaintextStore::new(path.clone());
        assert!(store.load().is_none());

        store.save(&sample_tokens()).unwrap();
        assert_eq!(store.load(), Some(sample_tokens()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0600, got 0o{mode:o}");
        }

        store.clear().unwrap();
        assert!(store.load().is_none());
        store.clear().unwrap(); // idempotent
    }

    #[test]
    fn plaintext_concurrent_saves_use_distinct_temp_paths() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tok.json");
        let store = Arc::new(PlaintextStore::new(path.clone()));
        let mut handles = Vec::new();
        for i in 0..16 {
            let s = store.clone();
            handles.push(std::thread::spawn(move || {
                let mut t = sample_tokens();
                t.api_token = format!("tok-{i}");
                s.save(&t).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(store.load().is_some());
    }

    #[test]
    fn plaintext_overwrite_replaces_atomically() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tok.json");
        let store = PlaintextStore::new(path);

        let mut t1 = sample_tokens();
        t1.api_token = "first".into();
        store.save(&t1).unwrap();

        let mut t2 = sample_tokens();
        t2.api_token = "second".into();
        store.save(&t2).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.api_token, "second");
    }

    #[test]
    fn plaintext_load_returns_none_for_garbage() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tok.json");
        fs::write(&path, b"not json").unwrap();
        let store = PlaintextStore::new(path);
        assert!(store.load().is_none());
    }

    #[test]
    fn broken_store_reports_in_save() {
        let s = BrokenStore {
            reason: "no D-Bus".into(),
        };
        assert_eq!(s.kind(), "unavailable");
        assert!(s.load().is_none());
        let err = s.save(&sample_tokens()).unwrap_err();
        assert!(err.contains("no D-Bus"), "got {err}");
    }
}
