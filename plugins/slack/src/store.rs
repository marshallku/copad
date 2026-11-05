//! Two-token Slack credential store.
//!
//! Socket Mode needs a Bot User OAuth Token (`xoxb-...`, HTTP API) and an App-Level Token
//! (`xapp-...`, WebSocket). Both are persisted; env values take precedence at startup, but
//! `auth.test`-validated tokens are written here so a restart needs no env.
//!
//! Keyring is preferred (Secret Service / macOS Keychain). On failure it falls back to plaintext
//! at `$XDG_CONFIG_HOME/copad/slack-tokens-<workspace>.json` (mode 0600, atomic replace via
//! per-call temp + rename) with a stderr warning on every open.
//! `COPAD_SLACK_REQUIRE_SECURE_STORE=1` forbids the fallback: token operations error while RPC
//! init still succeeds.

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
    /// `#[serde(default)]` is load-bearing: keyring entries written before this field must still
    /// deserialize.
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
    if !config.use_keychain {
        // Default on macOS (see `Config::use_keychain`). Skipping the probe avoids relying on the
        // 2s timeout in `KeyringStore::open` and the leaked probe thread.
        if config.require_secure_store {
            eprintln!(
                "[slack] secure keyring disabled (COPAD_SLACK_USE_KEYCHAIN=0) but \
                 COPAD_SLACK_REQUIRE_SECURE_STORE=1 — no usable store"
            );
            return Box::new(BrokenStore {
                reason: "keychain disabled and plaintext forbidden".to_string(),
            });
        }
        return Box::new(PlaintextStore::new(config.plaintext_path.clone()));
    }
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

pub struct KeyringStore {
    entry: keyring::Entry,
    // Only read by the macOS `save_macos_open_acl` path.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    workspace: String,
}

impl KeyringStore {
    pub fn open(workspace: &str) -> Result<Self, String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, workspace)
            .map_err(|e| format!("keyring entry: {e}"))?;
        // Probe the backend so a real failure (D-Bus unavailable, locked keyring) surfaces here,
        // not at first save. It runs in a thread with a 2s deadline: macOS Keychain blocks
        // indefinitely on a UI
        // prompt when the binary's signature no longer matches the entry's ACL (every rebuild), and
        // a background plugin has no UI, so a hang would stop it initialising. A timeout is a probe
        // failure, so `open_store` falls back to plaintext.
        let probe_entry = keyring::Entry::new(KEYRING_SERVICE, workspace)
            .map_err(|e| format!("keyring entry (probe): {e}"))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = match probe_entry.get_password() {
                Ok(_) => Ok(()),
                Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(format!("keyring probe: {e}")),
            };
            let _ = tx.send(result);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(Self {
                entry,
                workspace: workspace.to_string(),
            }),
            Ok(Err(e)) => Err(e),
            // The probe thread is leaked (blocked on the Keychain prompt), bounded to one per
            // plugin process.
            Err(_) => Err(
                "keyring probe timed out — likely waiting for a Keychain ACL prompt that \
                 cannot be surfaced from a background plugin process (run the binary from a \
                 foreground terminal once to grant access, or rely on plaintext fallback)"
                    .to_string(),
            ),
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
        #[cfg(target_os = "macos")]
        {
            save_macos_open_acl(&self.workspace, &s)
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.entry
                .set_password(&s)
                .map_err(|e| format!("keyring write: {e}"))
        }
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

/// macOS write that sets the Keychain ACL to "Allow all applications" (`-A`). The default ACL is
/// bound to the writing binary's cdhash, so a rebuild makes the next read prompt, and a
/// background plugin cannot surface that prompt and hangs until killed.
///
/// Trade-off: any process of the same user can read the entry without warning (comparable to the
/// plaintext fallback). The password is visible in the `security` argv briefly, acceptable for
/// `auth` (once per token rotation).
#[cfg(target_os = "macos")]
fn save_macos_open_acl(workspace: &str, json: &str) -> Result<(), String> {
    // Delete first: `add-generic-password` updates data but not an existing entry's ACL, and we
    // need `-A`.
    let _ = std::process::Command::new("/usr/bin/security")
        .args([
            "delete-generic-password",
            "-s",
            KEYRING_SERVICE,
            "-a",
            workspace,
        ])
        .output();
    let output = std::process::Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-A",
            "-s",
            KEYRING_SERVICE,
            "-a",
            workspace,
            "-w",
            json,
        ])
        .output()
        .map_err(|e| format!("security invoke: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("security add-generic-password: {stderr}"));
    }
    Ok(())
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
        // "No token file yet" (NotFound) is quiet; unreadable (EACCES, EIO) is logged for
        // operators.
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

/// Per-process counter so concurrent `save()` calls do not collide on a pid-derived temp path.
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
