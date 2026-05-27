//! Env-based configuration for the Slack plugin.
//!
//! Two tokens are required:
//! - `COPAD_SLACK_BOT_TOKEN` — `xoxb-...` Bot User OAuth Token. Used for
//!   HTTP API calls (`auth.test` to validate, future `chat.postMessage`).
//! - `COPAD_SLACK_APP_TOKEN` — `xapp-...` App-Level Token with
//!   `connections:write` scope. Used by Socket Mode to open a
//!   WebSocket without needing a public HTTPS endpoint.
//!
//! `COPAD_SLACK_WORKSPACE` is an optional namespacing label so a future
//! multi-workspace copad can run multiple slack plugin instances. v1 reads
//! one workspace at a time.

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub bot_token: String,
    pub app_token: String,
    pub workspace_label: String,
    pub require_secure_store: bool,
    pub plaintext_path: PathBuf,
    /// Initial reconnect delay; exponential backoff up to `reconnect_max`.
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
    /// Set on malformed env (bad token prefix, invalid workspace label,
    /// malformed boolean). Distinct from "tokens missing" (OK — store
    /// fallback). When set: Socket Mode refuses to connect and
    /// `slack.auth_status` surfaces the message. Bad workspace would
    /// steer plaintext writes outside the config dir; bad token prefix
    /// is a user typo we must not mask with stored defaults.
    pub fatal_error: Option<String>,
}

impl Config {
    /// Token env vars are OPTIONAL — Socket Mode falls back to the
    /// keyring-stored TokenSet, so one-time `auth` is enough. Env wins
    /// when set (cross-workspace testing). Invalid prefix on a *present*
    /// env token is still a hard error: a user typo must not be masked
    /// by stored defaults.
    pub fn from_env() -> Result<Self, String> {
        let bot_token = std::env::var("COPAD_SLACK_BOT_TOKEN").unwrap_or_default();
        if !bot_token.is_empty() && !bot_token.starts_with("xoxb-") {
            return Err(
                "COPAD_SLACK_BOT_TOKEN, when set, must be a Bot User OAuth Token (xoxb-...)"
                    .to_string(),
            );
        }
        let app_token = std::env::var("COPAD_SLACK_APP_TOKEN").unwrap_or_default();
        if !app_token.is_empty() && !app_token.starts_with("xapp-") {
            return Err(
                "COPAD_SLACK_APP_TOKEN, when set, must be an App-Level Token (xapp-...) with connections:write"
                    .to_string(),
            );
        }
        let workspace_label =
            std::env::var("COPAD_SLACK_WORKSPACE").unwrap_or_else(|_| "default".to_string());
        validate_workspace_label(&workspace_label)?;
        let require_secure_store = parse_bool("COPAD_SLACK_REQUIRE_SECURE_STORE", false)?;
        let plaintext_path = default_plaintext_path(&workspace_label);

        Ok(Self {
            bot_token,
            app_token,
            workspace_label,
            require_secure_store,
            plaintext_path,
            reconnect_initial: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            fatal_error: None,
        })
    }

    /// Builds a Config in fatal-error state so the supervisor
    /// handshake still completes; token-using actions and Socket Mode
    /// are no-ops. Carrying the message prevents silent fall-through
    /// to `default`-workspace stored credentials.
    pub fn minimal_with_error(error: String) -> Self {
        Self {
            bot_token: String::new(),
            app_token: String::new(),
            workspace_label: "default".to_string(),
            require_secure_store: false,
            plaintext_path: default_plaintext_path("default"),
            reconnect_initial: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            fatal_error: Some(error),
        }
    }

    /// Diagnostic surface only — Socket Mode also tries the store.
    pub fn env_tokens_empty(&self) -> bool {
        self.bot_token.is_empty() || self.app_token.is_empty()
    }
}

/// `[A-Za-z0-9_\-.@]+` minus reserved `.`/`..` — trust-boundary so
/// the label can't escape the plaintext-store dir or collide with
/// reserved keyring entries.
pub fn validate_workspace_label(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("COPAD_SLACK_WORKSPACE: cannot be empty".to_string());
    }
    if s == "." || s == ".." {
        return Err(format!(
            "COPAD_SLACK_WORKSPACE: {s:?} is reserved (use a normal label)"
        ));
    }
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@');
        if !ok {
            return Err(format!(
                "COPAD_SLACK_WORKSPACE: invalid character {c:?} \
                 (allowed: ASCII alphanumeric and _ - . @)"
            ));
        }
    }
    Ok(())
}

fn parse_bool(var: &str, default: bool) -> Result<bool, String> {
    match std::env::var(var) {
        Ok(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" | "" => Ok(false),
            _ => Err(format!("{var}: expected boolean (true/false), got {v:?}")),
        },
        Err(_) => Ok(default),
    }
}

fn default_plaintext_path(workspace: &str) -> PathBuf {
    let base = config_home().unwrap_or_else(|| PathBuf::from("."));
    base.join("copad")
        .join(format!("slack-tokens-{workspace}.json"))
}

fn config_home() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home).join(".config"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_label_accepts_normal() {
        for s in ["default", "work", "team@acme.com", "a.b-c_d"] {
            validate_workspace_label(s).unwrap_or_else(|e| panic!("{s:?} rejected: {e}"));
        }
    }

    #[test]
    fn workspace_label_rejects_bad() {
        for s in [
            "/etc/passwd",
            "../foo",
            "a/b",
            "a\\b",
            "x\0y",
            "..",
            ".",
            "",
        ] {
            assert!(validate_workspace_label(s).is_err(), "should reject {s:?}");
        }
    }
}
