//! Env-based configuration for the LLM plugin (Anthropic provider).
//!
//! v1 supports a single provider (Anthropic). Future expansion for
//! OpenAI / local models would add a `provider` discriminator that
//! selects which client + token store to use.

use std::path::PathBuf;
use std::time::Duration;

/// Override per call via the `model` param or globally via
/// `COPAD_LLM_DEFAULT_MODEL`.
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Capped under the supervisor's 120s `DEFAULT_ACTION_TIMEOUT`; the 10s
/// margin covers onAction lazy-spawn + handshake + response transit.
/// Higher values get rejected at config-load (would never be honored
/// end-to-end). Bump `DEFAULT_ACTION_TIMEOUT` first if you need more.
const MAX_HTTP_TIMEOUT_SECS: u64 = 110;

#[derive(Debug, Clone)]
pub struct Config {
    /// Empty when env not set; plugin then falls back to the keyring-stored
    /// value at action time (slack/calendar pattern).
    pub api_key: String,
    /// Default model used when `llm.complete` doesn't pass one.
    pub default_model: String,
    /// Default max_tokens for completions that don't specify.
    /// Anthropic requires this on every request.
    pub default_max_tokens: u32,
    /// `messages` HTTP timeout. Margins under the supervisor's 120s
    /// action timeout for thinking-mode / extended responses.
    pub http_timeout: Duration,
    pub account_label: String,
    pub require_secure_store: bool,
    pub plaintext_path: PathBuf,
    pub usage_log_path: PathBuf,
    /// `false` when the user-supplied label was malformed and paths fell
    /// back to `"default"`. `llm.usage` MUST refuse in that state so it
    /// can't read a different account's log than the user intended.
    /// Distinct from `fatal_error` (gates network-touching `llm.complete`).
    pub account_resolved: bool,
    /// Set on env-validation failure. `llm.complete` refuses;
    /// `llm.auth_status` surfaces the error (same shape as Slack).
    pub fatal_error: Option<String>,
}

impl Config {
    /// Never errors — env failures accumulate into `fatal_error` while
    /// each setting substitutes a safe default. The account label is
    /// validated BEFORE deriving paths so a bad `COPAD_LLM_ACCOUNT`
    /// can't silently redirect `llm.usage` to another account's log.
    pub fn from_env() -> Self {
        let mut errors: Vec<String> = Vec::new();

        // Account label first — it determines paths.
        let raw_account =
            std::env::var("COPAD_LLM_ACCOUNT").unwrap_or_else(|_| "default".to_string());
        let (account_label, account_resolved) = match validate_account_label(&raw_account) {
            Ok(_) => (raw_account, true),
            Err(e) => {
                errors.push(e);
                ("default".to_string(), false)
            }
        };
        let plaintext_path = default_plaintext_path(&account_label);
        let usage_log_path = default_usage_log_path(&account_label);

        // Token: empty if missing; non-empty must match prefix.
        let mut api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        if !api_key.is_empty() && !api_key.starts_with("sk-ant-") {
            errors.push("ANTHROPIC_API_KEY, when set, must start with sk-ant-".to_string());
            api_key.clear();
        }

        // Reject `COPAD_LLM_DEFAULT_MODEL=""` — using the empty
        // string as a model id would only surface as a remote API
        // error per call. Accumulate the error locally and fall
        // back to the build-time default so `auth` / completions
        // can still run when the user removes the bad env var.
        let default_model = match std::env::var("COPAD_LLM_DEFAULT_MODEL") {
            Ok(s) if s.is_empty() => {
                errors.push(
                    "COPAD_LLM_DEFAULT_MODEL: empty string is not a valid model id".to_string(),
                );
                DEFAULT_MODEL.to_string()
            }
            Ok(s) => s,
            Err(_) => DEFAULT_MODEL.to_string(),
        };

        let default_max_tokens =
            parse_nonzero_int_with_default("COPAD_LLM_DEFAULT_MAX_TOKENS", 4096u32, &mut errors);
        let mut http_timeout_secs: u64 = parse_nonzero_int_with_default(
            "COPAD_LLM_HTTP_TIMEOUT_SECS",
            MAX_HTTP_TIMEOUT_SECS,
            &mut errors,
        );
        if http_timeout_secs > MAX_HTTP_TIMEOUT_SECS {
            errors.push(format!(
                "COPAD_LLM_HTTP_TIMEOUT_SECS={http_timeout_secs} exceeds supervisor cap \
                 ({MAX_HTTP_TIMEOUT_SECS}s; supervisor action_timeout is 120s with 10s \
                 margin for spawn/init/transit). Bump DEFAULT_ACTION_TIMEOUT in \
                 copad-linux/src/service_supervisor.rs first if you genuinely need longer."
            ));
            http_timeout_secs = MAX_HTTP_TIMEOUT_SECS;
        }
        let require_secure_store =
            parse_bool_with_default("COPAD_LLM_REQUIRE_SECURE_STORE", false, &mut errors);

        Self {
            api_key,
            default_model,
            default_max_tokens,
            http_timeout: Duration::from_secs(http_timeout_secs),
            account_label,
            account_resolved,
            require_secure_store,
            plaintext_path,
            usage_log_path,
            fatal_error: if errors.is_empty() {
                None
            } else {
                Some(errors.join("; "))
            },
        }
    }
}

pub fn validate_account_label(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("COPAD_LLM_ACCOUNT: cannot be empty".to_string());
    }
    if s == "." || s == ".." {
        return Err(format!(
            "COPAD_LLM_ACCOUNT: {s:?} is reserved (use a normal label)"
        ));
    }
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@');
        if !ok {
            return Err(format!(
                "COPAD_LLM_ACCOUNT: invalid character {c:?} \
                 (allowed: ASCII alphanumeric and _ - . @)"
            ));
        }
    }
    Ok(())
}

fn parse_nonzero_int_with_default<T>(var: &str, default: T, errors: &mut Vec<String>) -> T
where
    T: std::str::FromStr + PartialEq + Default + Copy,
{
    match std::env::var(var) {
        Ok(v) => match v.parse::<T>() {
            Ok(parsed) if parsed != T::default() => parsed,
            Ok(_) => {
                errors.push(format!("{var}: must be > 0"));
                default
            }
            Err(_) => {
                errors.push(format!("{var}: invalid integer"));
                default
            }
        },
        Err(_) => default,
    }
}

fn parse_bool_with_default(var: &str, default: bool, errors: &mut Vec<String>) -> bool {
    match std::env::var(var) {
        Ok(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" | "" => false,
            _ => {
                errors.push(format!("{var}: expected boolean, got {v:?}"));
                default
            }
        },
        Err(_) => default,
    }
}

fn default_plaintext_path(account: &str) -> PathBuf {
    config_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("copad")
        .join(format!("llm-token-{account}.json"))
}

/// Under `$XDG_DATA_HOME` (operational data), distinct from the token
/// store under `$XDG_CONFIG_HOME` (configuration).
fn default_usage_log_path(account: &str) -> PathBuf {
    data_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("copad")
        .join(format!("llm-usage-{account}.jsonl"))
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

fn data_home() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home).join(".local").join("share"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_label_accepts_normal() {
        for s in ["default", "personal", "work@x.com", "a.b-c_d"] {
            validate_account_label(s).unwrap_or_else(|e| panic!("{s:?} rejected: {e}"));
        }
    }

    #[test]
    fn account_label_rejects_bad() {
        for s in ["/etc", "../foo", "a/b", "..", ".", ""] {
            assert!(validate_account_label(s).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn nonzero_int_with_default_rejects_zero_via_errors() {
        // SAFETY: tests run single-threaded for env mutation
        unsafe { std::env::set_var("TEST_LLM_NZI_ZERO", "0") };
        let mut errors: Vec<String> = Vec::new();
        let v = parse_nonzero_int_with_default::<u64>("TEST_LLM_NZI_ZERO", 60, &mut errors);
        assert_eq!(v, 60);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("must be > 0"));
    }
}
