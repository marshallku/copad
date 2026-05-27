//! Env-based configuration for the Discord plugin.
//!
//! - `COPAD_DISCORD_BOT_TOKEN` — bot token from
//!   <https://discord.com/developers/applications> → Bot tab → "Reset
//!   Token". Required for `auth` subcommand; optional for the long-
//!   running RPC mode (we read from the keyring there). Discord bot
//!   tokens don't have a documented prefix; we accept anything
//!   non-empty and let the `users/@me` validation catch typos.
//! - `COPAD_DISCORD_WORKSPACE` — keyring-entry namespace. Default
//!   `"default"`. Same role as `COPAD_SLACK_WORKSPACE`: lets one
//!   account host multiple bots whose keyring entries don't collide.
//! - `COPAD_DISCORD_REQUIRE_SECURE_STORE` — `1` / `true` refuses to
//!   fall back to plaintext when the OS keyring is unavailable. Same
//!   posture as the Slack plugin's flag.
//!
//! `from_env` never errors: invalid values land in `fatal_error` and
//! the plugin still completes the `initialize` handshake so the
//! supervisor's `provides` resolution succeeds. Behavior in degraded
//! mode is per-action:
//! - `discord.auth_status` always answers (even with fatal_error
//!   set) so the UI/CLI can show a coherent diagnostic.
//! - `discord.send_message` returns `not_authenticated` when
//!   `fatal_error` is set OR when no credentials are resolvable.
//!   Both states mean "the gateway cannot send right now"; merging
//!   them under one error code keeps the trigger DSL simple at the
//!   cost of slightly fuzzier root-cause attribution (callers can
//!   still diff against `discord.auth_status` for the structured
//!   reason).

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub workspace_label: String,
    /// Env-only seed for the `auth` subcommand — keyring is authoritative
    /// at runtime, so `None` is the expected steady state.
    pub bot_token_env: Option<String>,
    pub plaintext_path: PathBuf,
    pub require_secure_store: bool,
    /// Doubles up to `reconnect_max` on repeated failures; resets on
    /// clean disconnect (server-rotated reconnect, RESUMED).
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
    pub fatal_error: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let mut errors: Vec<String> = Vec::new();
        let workspace_label = std::env::var("COPAD_DISCORD_WORKSPACE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".to_string());
        if let Err(e) = validate_workspace_label(&workspace_label) {
            errors.push(format!("COPAD_DISCORD_WORKSPACE: {e}"));
        }
        let bot_token_env = std::env::var("COPAD_DISCORD_BOT_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let require_secure_store = std::env::var("COPAD_DISCORD_REQUIRE_SECURE_STORE")
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes"))
            .unwrap_or(false);
        let plaintext_path = default_plaintext_path(&workspace_label);
        let fatal_error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };
        Self {
            workspace_label,
            bot_token_env,
            plaintext_path,
            require_secure_store,
            reconnect_initial: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            fatal_error,
        }
    }
}

fn default_plaintext_path(workspace_label: &str) -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("copad")
        .join(format!("discord-token-{workspace_label}.json"))
}

/// `[A-Za-z0-9_\-.@]+` minus reserved `.`/`..` — trust-boundary
/// against a stray label escaping into other keyring namespaces.
fn validate_workspace_label(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("workspace label cannot be empty".to_string());
    }
    if s == "." || s == ".." {
        return Err(format!("{s:?} is reserved"));
    }
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@');
        if !ok {
            return Err(format!(
                "invalid character {c:?} (allowed: ASCII alphanumeric and _ - . @)"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_label_rejects_bad() {
        for s in ["", "..", "a/b", "x\0y", "/etc"] {
            assert!(
                validate_workspace_label(s).is_err(),
                "expected reject {s:?}"
            );
        }
    }

    #[test]
    fn workspace_label_accepts_normal() {
        for s in ["default", "team@acme.com", "a.b-c_d"] {
            validate_workspace_label(s).unwrap_or_else(|e| panic!("{s:?} rejected: {e}"));
        }
    }
}
