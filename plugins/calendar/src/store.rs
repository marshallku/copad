//! Token persistence with two backends serializing the same `TokenSet` JSON (so a user can migrate
//! manually):
//! - `KeyringStore`: OS-native secret store (Secret Service / macOS Keychain), preferred. An open
//!   failure is not fatal unless `require_secure_store=true`.
//! - `PlaintextStore`: JSON at `$XDG_CONFIG_HOME/copad/calendar-token-<account>.json`, mode 0600,
//!   used only when the keyring is unavailable and `require_secure_store=false`; warns on stderr
//!   at every open.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::Config;

const KEYRING_SERVICE: &str = "copad-calendar";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix epoch seconds. Refresh fires ~30s early to dodge clock skew.
    pub expires_at_unix: u64,
    pub scope: String,
    pub token_type: String,
}

pub trait TokenStore: Send + Sync {
    fn load(&self) -> Option<TokenSet>;
    fn save(&self, t: &TokenSet) -> Result<(), String>;
    /// Test-only today; on the trait so a future `calendar.logout` has a uniform entry point.
    #[allow(dead_code)]
    fn clear(&self) -> Result<(), String>;
    fn kind(&self) -> &'static str;
}

pub fn open_store(config: &Config) -> Box<dyn TokenStore> {
    match KeyringStore::open(&config.account_label) {
        Ok(s) => Box::new(s),
        Err(e) => {
            if config.require_secure_store {
                // Strict mode: a store whose every operation reports the original failure keeps a
                // useful error path for actions without aborting init.
                eprintln!(
                    "[calendar] secure keyring unavailable AND COPAD_CALENDAR_REQUIRE_SECURE_STORE=1: {e}"
                );
                Box::new(BrokenStore { reason: e })
            } else {
                eprintln!(
                    "[calendar] secure keyring unavailable, falling back to plaintext at {}: {e}",
                    config.plaintext_path.display()
                );
                Box::new(PlaintextStore::new(config.plaintext_path.clone()))
            }
        }
    }
}

pub struct KeyringStore {
    entry: keyring::Entry,
}

impl KeyringStore {
    pub fn open(account: &str) -> Result<Self, String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, account)
            .map_err(|e| format!("keyring entry: {e}"))?;
        // Attempt a get to confirm the secret service is reachable; ignore "no entry", bubble up
        // real errors (e.g. D-Bus unavailable).
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
                        "[calendar] keyring entry malformed (NOT a 'no tokens yet' state — \
                         credential backend may need attention): {e}"
                    );
                    None
                }
            },
            Err(keyring::Error::NoEntry) => None,
            Err(e) => {
                // A runtime backend failure (D-Bus unavailable, locked keyring) differs from "user
                // hasn't auth'd yet".
                eprintln!(
                    "[calendar] keyring backend FAILED while reading tokens — \
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
        let bytes = fs::read(&self.path).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "[calendar] plaintext token store malformed at {}: {e}",
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

/// pid+seq gives a unique temp file per in-flight save; pid alone collides when two saves run
/// concurrently in one process (the loser's rename hits ENOENT).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn write_atomic_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".calendar-token-{}-{}.tmp",
        std::process::id(),
        seq,
    ));
    {
        // `create_new` (O_EXCL) catches the one remaining collision, a leftover from a crashed run
        // with the same (pid, seq); bubble it up rather than overwrite.
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
            access_token: "ya29.foo".into(),
            refresh_token: "1//bar".into(),
            expires_at_unix: 1_900_000_000,
            scope: "https://www.googleapis.com/auth/calendar.readonly".into(),
            token_type: "Bearer".into(),
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
        store.clear().unwrap();
    }

    #[test]
    fn plaintext_concurrent_saves_use_distinct_temp_paths() {
        // Two threads racing on `save()` in one process must not corrupt each other's temp files.
        let dir = tempdir().unwrap();
        let path = dir.path().join("tok.json");
        let store = Arc::new(PlaintextStore::new(path.clone()));
        let mut handles = Vec::new();
        for i in 0..16 {
            let s = store.clone();
            handles.push(std::thread::spawn(move || {
                let mut t = sample_tokens();
                t.access_token = format!("tok-{i}");
                s.save(&t).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Final state is some writer's tokens; no save errored and the file parses.
        assert!(store.load().is_some());
    }

    #[test]
    fn plaintext_overwrite_replaces_atomically() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tok.json");
        let store = PlaintextStore::new(path);

        let mut t1 = sample_tokens();
        t1.access_token = "first".into();
        store.save(&t1).unwrap();

        let mut t2 = sample_tokens();
        t2.access_token = "second".into();
        store.save(&t2).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.access_token, "second");
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
