//! Env-based configuration for the todo plugin.
//!
//! - `COPAD_TODO_ROOT` — directory holding per-workspace todo subdirs.
//!   Default: `~/docs/todos`. Force-canonicalized on construction so
//!   later filesystem ops can `resolve_within_root` against it.
//! - `COPAD_TODO_DEFAULT_WORKSPACE` — workspace label used when
//!   `todo.create` / `todo.list` callers omit `workspace`. Default
//!   `"default"`. Validated against the same charset as KB folder
//!   names so a stray `..` can't escape the root.
//! - `COPAD_TODO_POLL_SECS` — file-watcher poll interval in seconds.
//!   Default 2. Bounded `[1, 60]` — 0 would burn a CPU core, > 60
//!   makes vim-edit feedback feel laggy.
//!
//! Like the calendar plugin, `from_env` never errors: invalid values
//! land in `fatal_error` and the plugin still completes `initialize`
//! (so the supervisor's `provides` resolution succeeds) — every action
//! then short-circuits to a clear diagnostic instead of silently
//! dispatching against a half-broken config.

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub default_workspace: String,
    pub poll_interval: Duration,
    /// Set on malformed env (workspace escape, out-of-range poll
    /// interval). Watcher refuses to start; actions return `config_error`.
    /// "Root missing" is NOT fatal — first-run auto-creates.
    pub fatal_error: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let mut errors: Vec<String> = Vec::new();

        let root = match std::env::var("COPAD_TODO_ROOT") {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => default_root(),
        };

        let default_workspace = std::env::var("COPAD_TODO_DEFAULT_WORKSPACE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".to_string());
        if let Err(e) = validate_workspace(&default_workspace) {
            errors.push(format!("COPAD_TODO_DEFAULT_WORKSPACE: {e}"));
        }

        let poll_interval = match std::env::var("COPAD_TODO_POLL_SECS") {
            Ok(s) if !s.is_empty() => match s.parse::<u64>() {
                Ok(n) if (1..=60).contains(&n) => Duration::from_secs(n),
                Ok(n) => {
                    errors.push(format!(
                        "COPAD_TODO_POLL_SECS: {n} out of range (allowed 1..=60)"
                    ));
                    Duration::from_secs(2)
                }
                Err(e) => {
                    errors.push(format!("COPAD_TODO_POLL_SECS: {s:?} not an integer: {e}"));
                    Duration::from_secs(2)
                }
            },
            _ => Duration::from_secs(2),
        };

        let fatal_error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };

        Self {
            root,
            default_workspace,
            poll_interval,
            fatal_error,
        }
    }
}

fn default_root() -> PathBuf {
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join("docs").join("todos");
    }
    PathBuf::from("./docs/todos")
}

/// `[A-Za-z0-9_\-.@]+` minus reserved `.`/`..` (KB's folder rules) —
/// trust-boundary so the label can't escape `root`.
pub fn validate_workspace(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("workspace label cannot be empty".to_string());
    }
    if s == "." || s == ".." {
        return Err(format!("{s:?} is reserved (use a normal label)"));
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
    fn workspace_accepts_normal() {
        for s in ["default", "work", "team@acme.com", "a.b-c_d"] {
            validate_workspace(s).unwrap_or_else(|e| panic!("{s:?} rejected: {e}"));
        }
    }

    #[test]
    fn workspace_rejects_bad() {
        for s in ["..", ".", "", "a/b", "a\\b", "x\0y", "/etc/passwd"] {
            assert!(validate_workspace(s).is_err(), "should reject {s:?}");
        }
    }
}
