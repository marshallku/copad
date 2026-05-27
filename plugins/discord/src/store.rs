//! Single-token Discord credential store. Slack needs a pair of
//! tokens (Bot OAuth + App-Level for Socket Mode); Discord's bot
//! token authenticates BOTH the HTTP API and the Gateway WebSocket,
//! so the store schema is simpler than Slack's.
//!
//! Keyring is preferred (Linux Secret Service / macOS Keychain).
//! On failure, falls back to plaintext at
//! `$XDG_CONFIG_HOME/copad/discord-token-<workspace>.json` (mode 0600,
//! atomic-replace via per-call temp + rename) with a stderr warning
//! on every open. `COPAD_DISCORD_REQUIRE_SECURE_STORE=1` forbids the
//! plaintext fallback entirely.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;

const KEYRING_SERVICE: &str = "copad-discord";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenSet {
    pub bot_token: String,
    /// Persisted from `/users/@me` so `auth_status` doesn't re-call.
    pub user_id: Option<String>,
    /// Bot username + discriminator (or new-style global name).
    /// Diagnostic only — surfaced by `discord.auth_status`.
    pub username: Option<String>,
}

pub trait TokenStore: Send + Sync {
    fn load(&self) -> Option<TokenSet>;
    fn save(&self, tokens: &TokenSet) -> Result<(), String>;
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
                    "[discord] secure keyring unavailable AND COPAD_DISCORD_REQUIRE_SECURE_STORE=1: {e}"
                );
                Box::new(BrokenStore { reason: e })
            } else {
                eprintln!(
                    "[discord] secure keyring unavailable, falling back to plaintext at {}: {e}",
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
                Ok(set) => Some(set),
                Err(e) => {
                    // Malformed credential blob — surface explicitly
                    // rather than collapsing to "not authenticated".
                    // Field debugging is otherwise indistinguishable
                    // from the never-ran-`auth` case.
                    eprintln!(
                        "[discord] keyring credential is corrupted (failed to deserialize): {e}. \
                         Re-run `copad-plugin-discord auth` to overwrite."
                    );
                    None
                }
            },
            Err(keyring::Error::NoEntry) => None,
            Err(e) => {
                eprintln!("[discord] keyring load failed: {e}");
                None
            }
        }
    }
    fn save(&self, tokens: &TokenSet) -> Result<(), String> {
        let s = serde_json::to_string(tokens).map_err(|e| format!("serialize: {e}"))?;
        self.entry
            .set_password(&s)
            .map_err(|e| format!("keyring save: {e}"))
    }
    fn clear(&self) -> Result<(), String> {
        match self.entry.delete_credential() {
            Ok(()) => Ok(()),
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
                    "[discord] plaintext load read {} failed: {e}",
                    self.path.display()
                );
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(set) => Some(set),
            Err(e) => {
                eprintln!(
                    "[discord] plaintext credential at {} is corrupted: {e}. \
                     Re-run `copad-plugin-discord auth` to overwrite.",
                    self.path.display()
                );
                None
            }
        }
    }
    fn save(&self, tokens: &TokenSet) -> Result<(), String> {
        atomic_write_0600(&self.path, tokens)
    }
    fn clear(&self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove {}: {e}", self.path.display())),
        }
    }
    fn kind(&self) -> &'static str {
        "plaintext"
    }
}

fn atomic_write_0600(path: &Path, tokens: &TokenSet) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let json = serde_json::to_vec_pretty(tokens).map_err(|e| format!("serialize: {e}"))?;
    {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("open temp {}: {e}", tmp.display()))?;
        f.write_all(&json)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all().ok();
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("rename {} -> {}: {e}", tmp.display(), path.display())
    })?;
    Ok(())
}

// -- Broken (keyring required, none available) --

pub struct BrokenStore {
    reason: String,
}

impl TokenStore for BrokenStore {
    fn load(&self) -> Option<TokenSet> {
        None
    }
    fn save(&self, _: &TokenSet) -> Result<(), String> {
        Err(format!(
            "secure store required but unavailable: {}",
            self.reason
        ))
    }
    fn clear(&self) -> Result<(), String> {
        Err(format!(
            "secure store required but unavailable: {}",
            self.reason
        ))
    }
    fn kind(&self) -> &'static str {
        "broken"
    }
}
