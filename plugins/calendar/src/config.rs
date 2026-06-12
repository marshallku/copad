//! Plugin runtime configuration sourced from environment variables.
//!
//! The plugin avoids hard-coding any Google OAuth client_id / secret
//! because copad is OSS and embedding shared credentials would let anyone
//! impersonate "copad" in OAuth consent screens. Users supply their own
//! Google Cloud project credentials.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub client_id: String,
    pub client_secret: String,
    pub account_label: String,
    pub lead_minutes: Vec<u32>,
    pub poll_interval: Duration,
    pub lookahead_hours: u32,
    pub require_secure_store: bool,
    pub plaintext_path: std::path::PathBuf,
    /// Set when env validation failed and RPC started in minimal mode — carried so the daemon can
    /// surface the cause instead of every action silently returning `not_authenticated`.
    pub fatal_error: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        // Validate the non-credential knobs first — a malformed value here is a genuine config
        // error regardless of whether credentials are set, and is the only case `run_rpc` toasts.
        let account_label =
            std::env::var("COPAD_CALENDAR_ACCOUNT").unwrap_or_else(|_| "default".to_string());
        validate_account_label(&account_label)?;

        let lead_minutes = parse_lead_minutes(
            &std::env::var("COPAD_CALENDAR_LEAD_MINUTES").unwrap_or_else(|_| "10".to_string()),
        )?;

        let poll_secs: u64 = parse_nonzero_int("COPAD_CALENDAR_POLL_SECS", 60)?;
        let lookahead_hours: u32 = parse_nonzero_int("COPAD_CALENDAR_LOOKAHEAD_HOURS", 24)?;
        let require_secure_store = parse_bool("COPAD_CALENDAR_REQUIRE_SECURE_STORE", false)?;

        let plaintext_path = default_plaintext_path(&account_label);

        // The OAuth client id/secret are required to do anything, but "not set yet" is the normal
        // pre-auth state (`auth_status` reports configured=false / `is_minimal()`), NOT a
        // misconfiguration — leave them empty and quiet rather than erroring, so run_rpc doesn't
        // raise a config-error toast on a fresh, never-authed calendar. `run_auth` checks presence
        // itself before starting the device-code flow. Mirrors slack/jira, where unset credentials
        // fall back to the store without a config error.
        let client_id = std::env::var("COPAD_CALENDAR_CLIENT_ID").unwrap_or_default();
        let client_secret = std::env::var("COPAD_CALENDAR_CLIENT_SECRET").unwrap_or_default();

        Ok(Self {
            client_id,
            client_secret,
            account_label,
            lead_minutes,
            poll_interval: Duration::from_secs(poll_secs),
            lookahead_hours,
            require_secure_store,
            plaintext_path,
            fatal_error: None,
        })
    }

    /// Error-free minimal config (tests only — production always carries the validation error via
    /// [`Config::minimal_with_error`]).
    #[cfg(test)]
    pub fn minimal() -> Self {
        Self::minimal_with_error_opt(None)
    }

    /// Like [`Config::minimal`] but carries the validation error so the daemon can surface it.
    pub fn minimal_with_error(error: String) -> Self {
        Self::minimal_with_error_opt(Some(error))
    }

    fn minimal_with_error_opt(fatal_error: Option<String>) -> Self {
        Self {
            client_id: String::new(),
            client_secret: String::new(),
            account_label: "default".to_string(),
            lead_minutes: vec![10],
            poll_interval: Duration::from_secs(60),
            lookahead_hours: 24,
            require_secure_store: false,
            plaintext_path: default_plaintext_path("default"),
            fatal_error,
        }
    }

    pub fn is_minimal(&self) -> bool {
        self.client_id.is_empty() || self.client_secret.is_empty()
    }
}

/// `[A-Za-z0-9_\-.@]+` minus reserved `.`/`..`. Trust-boundary: the
/// label is interpolated into both the keyring entry name and the
/// plaintext file path (`calendar-token-<account>.json`).
fn validate_account_label(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("COPAD_CALENDAR_ACCOUNT: cannot be empty".to_string());
    }
    if s == "." || s == ".." {
        return Err(format!(
            "COPAD_CALENDAR_ACCOUNT: {s:?} is reserved (use a normal label)"
        ));
    }
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@');
        if !ok {
            return Err(format!(
                "COPAD_CALENDAR_ACCOUNT: invalid character {c:?} \
                 (allowed: ASCII alphanumeric and _ - . @)"
            ));
        }
    }
    Ok(())
}

fn parse_lead_minutes(raw: &str) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let n: u32 = trimmed
            .parse()
            .map_err(|_| format!("COPAD_CALENDAR_LEAD_MINUTES: invalid integer {trimmed:?}"))?;
        if n == 0 {
            return Err("COPAD_CALENDAR_LEAD_MINUTES: 0 is not a meaningful lead time".to_string());
        }
        out.push(n);
    }
    if out.is_empty() {
        return Err("COPAD_CALENDAR_LEAD_MINUTES: at least one value required".to_string());
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Strictly-positive — zero would silently degrade poll-interval (tight
/// loop) or lookahead-window (empty) on a caller typo.
fn parse_nonzero_int<T>(var: &str, default: T) -> Result<T, String>
where
    T: std::str::FromStr + PartialEq + Default + Copy,
{
    match std::env::var(var) {
        Ok(v) => {
            let parsed: T = v.parse().map_err(|_| format!("{var}: invalid integer"))?;
            if parsed == T::default() {
                return Err(format!("{var}: must be > 0"));
            }
            Ok(parsed)
        }
        Err(_) => Ok(default),
    }
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

fn default_plaintext_path(account: &str) -> std::path::PathBuf {
    let base = dirs_config_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("copad")
        .join(format!("calendar-token-{account}.json"))
}

/// `$XDG_CONFIG_HOME` → `$HOME/.config` → `None`.
fn dirs_config_dir() -> Option<std::path::PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(std::path::PathBuf::from(xdg));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(std::path::PathBuf::from(home).join(".config"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lead_minutes_accepts_comma_list() {
        let v = parse_lead_minutes("10,60,5").unwrap();
        assert_eq!(v, vec![5, 10, 60]);
    }

    #[test]
    fn parse_lead_minutes_dedupes() {
        let v = parse_lead_minutes("10,10,60").unwrap();
        assert_eq!(v, vec![10, 60]);
    }

    #[test]
    fn parse_lead_minutes_strips_whitespace() {
        let v = parse_lead_minutes(" 10 , 60 ,  5 ").unwrap();
        assert_eq!(v, vec![5, 10, 60]);
    }

    #[test]
    fn parse_lead_minutes_rejects_zero() {
        assert!(parse_lead_minutes("0").is_err());
        assert!(parse_lead_minutes("10,0").is_err());
    }

    #[test]
    fn parse_lead_minutes_rejects_empty() {
        assert!(parse_lead_minutes("").is_err());
        assert!(parse_lead_minutes(" , , ").is_err());
    }

    #[test]
    fn parse_lead_minutes_rejects_non_int() {
        assert!(parse_lead_minutes("abc").is_err());
        assert!(parse_lead_minutes("10,abc").is_err());
    }

    #[test]
    fn validate_account_label_accepts_normal_labels() {
        for s in ["default", "personal", "work@gmail.com", "a.b-c_d", "x123"] {
            validate_account_label(s).unwrap_or_else(|e| panic!("{s:?} rejected: {e}"));
        }
    }

    #[test]
    fn validate_account_label_rejects_path_separators() {
        for s in [
            "/etc/passwd",
            "../foo",
            "a/b",
            "a\\b",
            "x\0y",
            "foo bar",
            "/../",
            "..",
            ".",
            "",
        ] {
            assert!(validate_account_label(s).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn parse_nonzero_int_rejects_zero() {
        // SAFETY: tests run single-threaded for env mutation
        unsafe { std::env::set_var("TEST_NZI_ZERO", "0") };
        assert!(parse_nonzero_int::<u64>("TEST_NZI_ZERO", 60).is_err());
        unsafe { std::env::set_var("TEST_NZI_NEG", "abc") };
        assert!(parse_nonzero_int::<u64>("TEST_NZI_NEG", 60).is_err());
        unsafe { std::env::remove_var("TEST_NZI_OK") };
        assert_eq!(parse_nonzero_int::<u64>("TEST_NZI_OK", 60).unwrap(), 60);
        unsafe { std::env::set_var("TEST_NZI_OK", "120") };
        assert_eq!(parse_nonzero_int::<u64>("TEST_NZI_OK", 60).unwrap(), 120);
    }

    #[test]
    fn parse_bool_accepts_common_forms() {
        for (s, expected) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("no", false),
            ("OFF", false),
            ("", false),
        ] {
            // SAFETY: tests run single-threaded for env mutation
            unsafe { std::env::set_var("TEST_PARSE_BOOL", s) };
            assert_eq!(
                parse_bool("TEST_PARSE_BOOL", false).unwrap(),
                expected,
                "input {s:?}"
            );
        }
    }
}
