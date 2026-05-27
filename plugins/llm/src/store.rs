//! Single-token Anthropic credential store.
//!
//! Mirrors the calendar/slack token-store pattern: keyring preferred
//! (Linux Secret Service / macOS Keychain), plaintext 0600 fallback
//! at `$XDG_CONFIG_HOME/copad/llm-token-<account>.json`. The
//! `TokenSet` carries the API key plus optional identity (whether
//! the key has been validated against the API and what account it
//! belongs to — Anthropic's `messages` doesn't reveal the account
//! id, so identity is just "validated_at" timestamp for now).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::Config;

const KEYRING_SERVICE: &str = "copad-llm";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenSet {
    pub api_key: String,
    /// RFC3339 of last `auth` validation; surfaced via `llm.auth_status`.
    pub validated_at: Option<String>,
}

pub trait TokenStore: Send + Sync {
    fn load(&self) -> Option<TokenSet>;
    fn save(&self, tokens: &TokenSet) -> Result<(), String>;
    #[allow(dead_code)]
    fn clear(&self) -> Result<(), String>;
    fn kind(&self) -> &'static str;
}

pub fn open_store(config: &Config) -> Box<dyn TokenStore> {
    match KeyringStore::open(&config.account_label) {
        Ok(s) => Box::new(s),
        Err(e) => {
            if config.require_secure_store {
                eprintln!(
                    "[llm] secure keyring unavailable AND COPAD_LLM_REQUIRE_SECURE_STORE=1: {e}"
                );
                Box::new(BrokenStore { reason: e })
            } else {
                eprintln!(
                    "[llm] secure keyring unavailable, falling back to plaintext at {}: {e}",
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
    pub fn open(account: &str) -> Result<Self, String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, account)
            .map_err(|e| format!("keyring entry: {e}"))?;
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
                    eprintln!("[llm] keyring entry malformed: {e}");
                    None
                }
            },
            Err(keyring::Error::NoEntry) => None,
            Err(e) => {
                eprintln!("[llm] keyring backend FAILED while reading tokens: {e}");
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
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                eprintln!(
                    "[llm] plaintext token store unreadable at {}: {e}",
                    self.path.display()
                );
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "[llm] plaintext token store malformed at {}: {e}",
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

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn write_atomic_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".llm-token-{}-{}.tmp", std::process::id(), seq,));
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
            api_key: "sk-ant-test".into(),
            validated_at: Some("2026-04-27T12:00:00Z".into()),
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
            assert_eq!(mode, 0o600);
        }

        store.clear().unwrap();
        assert!(store.load().is_none());
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
                t.api_key = format!("sk-ant-{i}");
                s.save(&t).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(store.load().is_some());
    }

    #[test]
    fn broken_store_reports_reason_in_save() {
        let s = BrokenStore {
            reason: "no D-Bus".into(),
        };
        assert!(s.save(&sample()).unwrap_err().contains("no D-Bus"));
    }
}
