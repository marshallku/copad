//! Plugin runtime configuration sourced from environment variables.
//!
//! Atlassian Cloud uses Basic auth with `email:api_token` — there is
//! no shared OAuth client to embed (which is why we don't have the
//! "users supply their own client_id" wart that calendar carries).
//! Each user generates their own API token at
//! id.atlassian.com/manage-profile/security/api-tokens and pastes
//! it via env, then `copad-plugin-jira auth` validates and persists.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub email: String,
    pub api_token: String,
    pub workspace_label: String,
    pub require_secure_store: bool,
    pub plaintext_path: std::path::PathBuf,
    /// Polling interval (default 300s — Jira Cloud rate-limits
    /// aggressively per-user). Range 60..=3600.
    pub poll_interval: Duration,
    /// JQL `updated > -<n>h` window. Default 24h, range 1..=720.
    pub lookback_hours: u32,
    /// Optional `project in (X, Y)` allowlist. None = all projects
    /// the user has access to.
    pub projects: Option<Vec<String>>,
    /// Default true; set `false` for a status/assignee-only feed (no API
    /// cost reduction since comments arrive inline in the search response).
    pub fetch_comments: bool,
    /// Set on env-validation failure: RPC starts (so the supervisor
    /// handshake completes) but every Jira-touching action returns
    /// `not_authenticated` upfront. Without this gate a stale stored
    /// token from a previous good run could make one call succeed
    /// before the next refresh fails confusingly.
    pub fatal_error: Option<String>,
}

impl Config {
    /// Credential env vars (`COPAD_JIRA_BASE_URL`/`_EMAIL`/`_API_TOKEN`)
    /// are OPTIONAL — RPC mode falls back to the keyring-stored TokenSet,
    /// so a one-time `auth` subcommand is enough. Env wins when set
    /// (useful for testing against a different site). A malformed value
    /// on a *present* env var IS still a hard error: silently falling
    /// back to stored defaults would hide user typos.
    pub fn from_env() -> Result<Self, String> {
        // Trim before checking presence so a whitespace-only value (typo'd
        // shell rc) is treated as "not set" rather than shadowing the store.
        let base_url_raw = std::env::var("COPAD_JIRA_BASE_URL").unwrap_or_default();
        let base_url = base_url_raw.trim().trim_end_matches('/').to_string();
        if !base_url.is_empty() {
            validate_base_url(&base_url)?;
        }

        let email = std::env::var("COPAD_JIRA_EMAIL")
            .unwrap_or_default()
            .trim()
            .to_string();
        let api_token = std::env::var("COPAD_JIRA_API_TOKEN")
            .unwrap_or_default()
            .trim()
            .to_string();

        let workspace_label =
            std::env::var("COPAD_JIRA_WORKSPACE").unwrap_or_else(|_| "default".to_string());
        validate_workspace_label(&workspace_label)?;

        let require_secure_store = parse_bool("COPAD_JIRA_REQUIRE_SECURE_STORE", false)?;

        let poll_secs: u64 = parse_bounded_int("COPAD_JIRA_POLL_SECS", 300, 60, 3600)?;
        let lookback_hours: u32 = parse_bounded_int("COPAD_JIRA_LOOKBACK_HOURS", 24, 1, 720)?;
        let projects = parse_projects(&std::env::var("COPAD_JIRA_PROJECTS").unwrap_or_default())?;
        let fetch_comments = parse_bool("COPAD_JIRA_FETCH_COMMENTS", true)?;

        let plaintext_path = default_plaintext_path(&workspace_label);

        Ok(Self {
            base_url,
            email,
            api_token,
            workspace_label,
            require_secure_store,
            plaintext_path,
            poll_interval: Duration::from_secs(poll_secs),
            lookback_hours,
            projects,
            fetch_comments,
            fatal_error: None,
        })
    }

    /// Builds a Config in fatal-error state (see `fatal_error` field).
    pub fn minimal_with_error(err: String) -> Self {
        Self {
            base_url: String::new(),
            email: String::new(),
            api_token: String::new(),
            workspace_label: "default".to_string(),
            require_secure_store: false,
            plaintext_path: default_plaintext_path("default"),
            poll_interval: Duration::from_secs(300),
            lookback_hours: 24,
            projects: None,
            fetch_comments: true,
            fatal_error: Some(err),
        }
    }

    /// Env-tokens absent (relying entirely on the store).
    pub fn env_creds_empty(&self) -> bool {
        self.email.is_empty() || self.api_token.is_empty() || self.base_url.is_empty()
    }
}

/// Restricts `base_url` to `https://<subdomain>.atlassian.net[/]` shape.
/// Trust-boundary defense: a typo like `https://atlassian.net.evil.example`
/// would POST the API token to an attacker. Self-hosted Server / Data
/// Center is rejected because it needs different auth (PAT/cookies, not
/// Basic) — running Cloud's auth shape there would fail confusingly.
/// Public so `current_credentials` can apply the same gate to stored
/// base_urls (hand-edited keyring entries must not bypass env-time check).
pub fn validate_base_url(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("COPAD_JIRA_BASE_URL: cannot be empty".to_string());
    }
    if !s.starts_with("https://") {
        return Err(format!(
            "COPAD_JIRA_BASE_URL: must start with https:// (got {s:?})"
        ));
    }
    let after_scheme = &s["https://".len()..];
    if after_scheme.is_empty() || after_scheme.starts_with('/') {
        return Err(format!("COPAD_JIRA_BASE_URL: missing host (got {s:?})"));
    }
    // Reject embedded whitespace / control chars — they would either
    // get URL-encoded into garbage or break the request line.
    for c in s.chars() {
        if c.is_control() || c == ' ' || c == '\t' {
            return Err(format!(
                "COPAD_JIRA_BASE_URL: invalid character {c:?} in URL"
            ));
        }
    }
    // The runtime joins this base with `/rest/api/3/...` paths.
    // Allowing a path component (like `https://x.atlassian.net/jira`)
    // would produce `https://x.atlassian.net/jira/rest/api/3/myself`
    // which is not the intended endpoint and would fail every API
    // call. Reject any path so the user sees the issue at config
    // time, not as a confusing runtime 404. A trailing slash with
    // nothing after it is normalized to "" by from_env's
    // trim_end_matches and is accepted here.
    let host = match after_scheme.find('/') {
        None => after_scheme,
        Some(i) => {
            let path = &after_scheme[i..];
            if path != "/" {
                return Err(format!(
                    "COPAD_JIRA_BASE_URL: must be the bare site root (no path); \
                     drop everything after the host. Got {s:?}"
                ));
            }
            &after_scheme[..i]
        }
    };
    // No userinfo / port — `https://user:pass@host.atlassian.net` is
    // semantically suspicious (creds in URL) and `host:443` carries
    // no benefit over the implicit default. Reject both rather than
    // try to parse around them.
    if host.contains('@') || host.contains(':') {
        return Err(format!(
            "COPAD_JIRA_BASE_URL: host must not contain userinfo or port (got {host:?})"
        ));
    }
    // Atlassian Cloud workspaces are always `<subdomain>.atlassian.net`.
    // Custom domains (Premium/Enterprise) still hit `*.atlassian.net`
    // for the REST API. Anything else is either a typo or a Server
    // install we don't support.
    let lower = host.to_ascii_lowercase();
    if !lower.ends_with(".atlassian.net") {
        return Err(format!(
            "COPAD_JIRA_BASE_URL: host must be <workspace>.atlassian.net for Atlassian Cloud (got {host:?}); \
             self-hosted Jira Server is not supported in this plugin"
        ));
    }
    let subdomain_len = lower.len() - ".atlassian.net".len();
    if subdomain_len == 0 {
        return Err(format!(
            "COPAD_JIRA_BASE_URL: missing workspace subdomain (got {host:?})"
        ));
    }
    let subdomain = &lower[..subdomain_len];
    // Workspace subdomains: lowercase letters, digits, hyphens,
    // dots (for region-prefixed forms like `eu.atlassian.net` -
    // matches anything Atlassian routes to a Cloud tenant).
    for c in subdomain.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.');
        if !ok {
            return Err(format!(
                "COPAD_JIRA_BASE_URL: subdomain {subdomain:?} contains invalid character {c:?}"
            ));
        }
    }
    Ok(())
}

/// Trust-boundary: the label is interpolated into the keyring entry name
/// and the plaintext file path. Same charset as calendar/slack/discord.
fn validate_workspace_label(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("COPAD_JIRA_WORKSPACE: cannot be empty".to_string());
    }
    if s == "." || s == ".." {
        return Err(format!(
            "COPAD_JIRA_WORKSPACE: {s:?} is reserved (use a normal label)"
        ));
    }
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@');
        if !ok {
            return Err(format!(
                "COPAD_JIRA_WORKSPACE: invalid character {c:?} \
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

/// Empty/unset → default; out-of-range or non-integer → hard error so a
/// misconfigured shell rc doesn't silently degrade the poller.
fn parse_bounded_int<T>(var: &str, default: T, min: T, max: T) -> Result<T, String>
where
    T: std::str::FromStr + std::fmt::Display + PartialOrd + Copy,
{
    match std::env::var(var) {
        Ok(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Ok(default);
            }
            let parsed: T = trimmed
                .parse()
                .map_err(|_| format!("{var}: invalid integer {trimmed:?}"))?;
            if parsed < min || parsed > max {
                return Err(format!("{var}: {parsed} out of range [{min}, {max}]"));
            }
            Ok(parsed)
        }
        Err(_) => Ok(default),
    }
}

/// Comma-separated allowlist of Jira project keys (`[A-Z][A-Z0-9_]*`).
/// Strict validation so a typo can't silently steer the polling JQL into
/// returning everything or fail obscurely at the API boundary.
fn parse_projects(raw: &str) -> Result<Option<Vec<String>>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for part in trimmed.split(',') {
        let key = part.trim();
        if key.is_empty() {
            continue;
        }
        validate_project_key(key)?;
        out.push(key.to_string());
    }
    if out.is_empty() {
        return Ok(None);
    }
    out.sort_unstable();
    out.dedup();
    Ok(Some(out))
}

fn validate_project_key(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("COPAD_JIRA_PROJECTS: empty project key".to_string());
    }
    let bytes = s.as_bytes();
    if !bytes[0].is_ascii_uppercase() {
        return Err(format!(
            "COPAD_JIRA_PROJECTS: project key {s:?} must start with an uppercase ASCII letter"
        ));
    }
    for &b in bytes {
        if !(b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
            return Err(format!(
                "COPAD_JIRA_PROJECTS: project key {s:?} contains invalid character (allowed: A-Z, 0-9, _)"
            ));
        }
    }
    Ok(())
}

fn default_plaintext_path(workspace: &str) -> std::path::PathBuf {
    let base = dirs_config_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("copad")
        .join(format!("jira-token-{workspace}.json"))
}

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
    use std::sync::Mutex;

    // Tests that mutate `COPAD_JIRA_*` env vars must serialize against
    // each other — cargo runs tests in parallel by default, so the
    // earlier "single-threaded" SAFETY comments were aspirational, not
    // enforced. Poisoned guard is recovered so a panic in one test
    // doesn't cascade-fail the rest.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn validate_base_url_accepts_atlassian_cloud_hosts() {
        validate_base_url("https://yourcompany.atlassian.net").unwrap();
        validate_base_url("https://x.atlassian.net/").unwrap();
        validate_base_url("https://team-foo.atlassian.net").unwrap();
        // Mixed-case host is normalized internally.
        validate_base_url("https://Team-Foo.Atlassian.Net").unwrap();
    }

    #[test]
    fn validate_base_url_rejects_non_atlassian_hosts() {
        // Critical security boundary: the plugin sends Basic auth
        // with the user's API token to whatever host base_url
        // points at. Allowing arbitrary `https://` would let a
        // typo exfiltrate credentials.
        for s in [
            "https://atlassian.net.evil.example",
            "https://jira.company.com",
            "https://example.com",
            "https://atlassian.net",
            "https://.atlassian.net",
        ] {
            assert!(
                validate_base_url(s).is_err(),
                "should reject non-Atlassian host {s:?}"
            );
        }
    }

    #[test]
    fn validate_base_url_rejects_url_with_path() {
        // Runtime joins this base with `/rest/api/3/...`. A path on
        // base_url would corrupt every endpoint URL.
        for s in [
            "https://x.atlassian.net/jira",
            "https://x.atlassian.net/wiki",
            "https://x.atlassian.net/rest/api/3",
            "https://x.atlassian.net/foo/bar",
        ] {
            assert!(
                validate_base_url(s).is_err(),
                "should reject base URL with path {s:?}"
            );
        }
    }

    #[test]
    fn validate_base_url_rejects_userinfo_and_port() {
        for s in [
            "https://user:pass@x.atlassian.net",
            "https://x.atlassian.net:443",
        ] {
            assert!(
                validate_base_url(s).is_err(),
                "should reject host with userinfo/port {s:?}"
            );
        }
    }

    #[test]
    fn validate_base_url_rejects_bad_inputs() {
        for s in [
            "",
            "http://x.atlassian.net",
            "yourcompany.atlassian.net",
            "https://",
            "https:///foo",
            "https://x .atlassian.net",
            "https://x\tatlassian.net",
        ] {
            assert!(validate_base_url(s).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn validate_workspace_label_accepts_normal_labels() {
        for s in ["default", "personal", "work@x.com", "a.b-c_d", "x123"] {
            validate_workspace_label(s).unwrap_or_else(|e| panic!("{s:?} rejected: {e}"));
        }
    }

    #[test]
    fn validate_workspace_label_rejects_path_separators() {
        for s in [
            "/etc/passwd",
            "../foo",
            "a/b",
            "a\\b",
            "x\0y",
            "foo bar",
            "..",
            ".",
            "",
        ] {
            assert!(validate_workspace_label(s).is_err(), "should reject {s:?}");
        }
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
            unsafe { std::env::set_var("TEST_JIRA_PARSE_BOOL", s) };
            assert_eq!(
                parse_bool("TEST_JIRA_PARSE_BOOL", false).unwrap(),
                expected,
                "input {s:?}"
            );
        }
    }

    #[test]
    fn parse_bool_rejects_garbage() {
        // SAFETY: tests run single-threaded for env mutation
        unsafe { std::env::set_var("TEST_JIRA_PARSE_BOOL_BAD", "maybe") };
        assert!(parse_bool("TEST_JIRA_PARSE_BOOL_BAD", false).is_err());
    }

    #[test]
    fn config_minimal_with_error_carries_fatal() {
        let c = Config::minimal_with_error("missing X".to_string());
        assert_eq!(c.fatal_error.as_deref(), Some("missing X"));
        assert!(c.env_creds_empty());
    }

    #[test]
    fn config_strips_trailing_slash_from_base_url() {
        let _g = env_lock();
        unsafe {
            std::env::set_var("COPAD_JIRA_BASE_URL", "https://x.atlassian.net/");
            std::env::set_var("COPAD_JIRA_EMAIL", "a@b.com");
            std::env::set_var("COPAD_JIRA_API_TOKEN", "tok");
            std::env::remove_var("COPAD_JIRA_WORKSPACE");
            std::env::remove_var("COPAD_JIRA_REQUIRE_SECURE_STORE");
        }
        let c = Config::from_env().unwrap();
        assert_eq!(c.base_url, "https://x.atlassian.net");
        assert_eq!(c.workspace_label, "default");
    }

    #[test]
    fn from_env_succeeds_with_empty_credentials() {
        // RPC mode must not fail when env credentials are missing —
        // the plugin falls back to the store. Only `auth` subcommand
        // requires the env (checked separately in main::run_auth).
        let _g = env_lock();
        unsafe {
            std::env::remove_var("COPAD_JIRA_BASE_URL");
            std::env::remove_var("COPAD_JIRA_EMAIL");
            std::env::remove_var("COPAD_JIRA_API_TOKEN");
            std::env::remove_var("COPAD_JIRA_WORKSPACE");
            std::env::remove_var("COPAD_JIRA_REQUIRE_SECURE_STORE");
        }
        let c = Config::from_env().unwrap();
        assert!(c.base_url.is_empty());
        assert!(c.email.is_empty());
        assert!(c.api_token.is_empty());
        assert!(c.env_creds_empty());
        assert!(c.fatal_error.is_none());
    }

    #[test]
    fn from_env_treats_whitespace_only_credentials_as_empty() {
        // Whitespace-only env vars must NOT shadow the store —
        // otherwise current_credentials would prefer "env" over
        // valid stored creds and auth_status would lie about live
        // state until the first 401. Trimmed → empty → fall through
        // to store.
        let _g = env_lock();
        unsafe {
            std::env::set_var("COPAD_JIRA_BASE_URL", "   ");
            std::env::set_var("COPAD_JIRA_EMAIL", "  \t  ");
            std::env::set_var("COPAD_JIRA_API_TOKEN", " \n ");
            std::env::remove_var("COPAD_JIRA_WORKSPACE");
            std::env::remove_var("COPAD_JIRA_REQUIRE_SECURE_STORE");
        }
        let c = Config::from_env().unwrap();
        assert!(c.base_url.is_empty(), "got {:?}", c.base_url);
        assert!(c.email.is_empty(), "got {:?}", c.email);
        assert!(c.api_token.is_empty(), "got {:?}", c.api_token);
        assert!(c.env_creds_empty());
    }

    #[test]
    fn parse_bounded_int_accepts_default_and_in_range() {
        // SAFETY: tests run single-threaded for env mutation
        unsafe { std::env::remove_var("TEST_JIRA_BI_OK") };
        assert_eq!(
            parse_bounded_int::<u64>("TEST_JIRA_BI_OK", 300, 60, 3600).unwrap(),
            300
        );
        unsafe { std::env::set_var("TEST_JIRA_BI_OK", "120") };
        assert_eq!(
            parse_bounded_int::<u64>("TEST_JIRA_BI_OK", 300, 60, 3600).unwrap(),
            120
        );
    }

    #[test]
    fn parse_bounded_int_rejects_out_of_range() {
        // SAFETY: tests run single-threaded for env mutation
        unsafe { std::env::set_var("TEST_JIRA_BI_LO", "30") };
        assert!(parse_bounded_int::<u64>("TEST_JIRA_BI_LO", 300, 60, 3600).is_err());
        unsafe { std::env::set_var("TEST_JIRA_BI_HI", "9999") };
        assert!(parse_bounded_int::<u64>("TEST_JIRA_BI_HI", 300, 60, 3600).is_err());
        unsafe { std::env::set_var("TEST_JIRA_BI_BAD", "abc") };
        assert!(parse_bounded_int::<u64>("TEST_JIRA_BI_BAD", 300, 60, 3600).is_err());
    }

    #[test]
    fn parse_projects_accepts_comma_list() {
        let v = parse_projects("PROJ,APP,IOS2").unwrap().unwrap();
        // Sorted + deduped on output.
        assert_eq!(v, vec!["APP", "IOS2", "PROJ"]);
    }

    #[test]
    fn parse_projects_dedupes_and_strips_whitespace() {
        let v = parse_projects(" PROJ , PROJ , APP ").unwrap().unwrap();
        assert_eq!(v, vec!["APP", "PROJ"]);
    }

    #[test]
    fn parse_projects_empty_returns_none() {
        assert!(parse_projects("").unwrap().is_none());
        assert!(parse_projects("  ").unwrap().is_none());
        assert!(parse_projects(" , , ").unwrap().is_none());
    }

    #[test]
    fn parse_projects_rejects_invalid_keys() {
        for bad in [
            "proj",     // lowercase
            "1PROJ",    // starts with digit
            "PROJ-456", // hyphen
            "PROJ.SUB", // dot
            "PROJ A",   // space
            "PROJ,abc", // mixed: second key invalid
        ] {
            assert!(
                parse_projects(bad).is_err(),
                "should reject project list {bad:?}"
            );
        }
    }

    #[test]
    fn from_env_validates_base_url_format_when_present() {
        // Even with optional env, a present-but-malformed value is
        // still a hard error so a typo doesn't silently mask the
        // stored URL.
        let _g = env_lock();
        unsafe {
            std::env::set_var("COPAD_JIRA_BASE_URL", "ftp://x.atlassian.net");
            std::env::remove_var("COPAD_JIRA_EMAIL");
            std::env::remove_var("COPAD_JIRA_API_TOKEN");
            std::env::remove_var("COPAD_JIRA_WORKSPACE");
            std::env::remove_var("COPAD_JIRA_REQUIRE_SECURE_STORE");
        }
        let err = Config::from_env().unwrap_err();
        assert!(err.contains("https://"), "got {err}");
    }
}
