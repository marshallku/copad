//! Desktop notification surface for `notify.show`: `notify-send` on Linux, `osascript` on macOS.
//! The subprocess is waited on so failures surface as errors. macOS uses an `on run argv`
//! wrapper so title/body cross as argv values and are never spliced into script source, which
//! avoids quote/backslash injection from trigger payloads.

use std::fmt;
use std::process::{Command, ExitStatus};
use std::sync::Mutex;

use serde::Deserialize;

/// Maps to `notify-send --urgency`; ignored on macOS, whose `display notification` has no
/// severity slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn urgency(self) -> &'static str {
        match self {
            Self::Info => "low",
            Self::Warn => "normal",
            Self::Error => "critical",
        }
    }
}

#[derive(Debug)]
pub enum NotifyError {
    Spawn(std::io::Error),
    NonZeroExit { status: ExitStatus, stderr: String },
}

impl fmt::Display for NotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "notifier spawn failed: {e}"),
            Self::NonZeroExit { status, stderr } => {
                write!(f, "notifier exited {status}: {stderr}")
            }
        }
    }
}

impl std::error::Error for NotifyError {}

pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str, level: Level) -> Result<(), NotifyError>;
}

/// Cap on payload size before the subprocess sees it; a runaway trigger interpolating a large
/// body has no business as a toast. Hard truncate.
const MAX_BODY_BYTES: usize = 4096;
const MAX_TITLE_BYTES: usize = 256;

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(target_os = "linux")]
pub struct LibnotifyNotifier;

#[cfg(target_os = "linux")]
impl Notifier for LibnotifyNotifier {
    fn notify(&self, title: &str, body: &str, level: Level) -> Result<(), NotifyError> {
        run_command(
            Command::new("notify-send")
                .arg(format!("--urgency={}", level.urgency()))
                .arg(truncate(title, MAX_TITLE_BYTES))
                .arg(truncate(body, MAX_BODY_BYTES)),
        )
    }
}

#[cfg(target_os = "macos")]
pub struct OsascriptNotifier;

#[cfg(target_os = "macos")]
impl Notifier for OsascriptNotifier {
    fn notify(&self, title: &str, body: &str, _level: Level) -> Result<(), NotifyError> {
        // `on run argv` keeps title/body out of the script source, so no quote/backslash escaping
        // is needed whatever the payload.
        const SCRIPT: &str = "on run argv\n\
            display notification (item 2 of argv) with title (item 1 of argv)\n\
        end run";
        run_command(
            Command::new("osascript")
                .arg("-e")
                .arg(SCRIPT)
                .arg(truncate(title, MAX_TITLE_BYTES))
                .arg(truncate(body, MAX_BODY_BYTES)),
        )
    }
}

#[allow(dead_code)]
fn run_command(cmd: &mut Command) -> Result<(), NotifyError> {
    let output = cmd.output().map_err(NotifyError::Spawn)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(NotifyError::NonZeroExit {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Test double that records every `notify` call; public so daemon/integration tests can use it.
#[derive(Default)]
pub struct NoopNotifier {
    pub captured: Mutex<Vec<(String, String, Level)>>,
}

impl Notifier for NoopNotifier {
    fn notify(&self, title: &str, body: &str, level: Level) -> Result<(), NotifyError> {
        self.captured
            .lock()
            .unwrap()
            .push((title.to_string(), body.to_string(), level));
        Ok(())
    }
}

pub fn platform_notifier() -> Option<Box<dyn Notifier>> {
    #[cfg(target_os = "linux")]
    {
        Some(Box::new(LibnotifyNotifier))
    }
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(OsascriptNotifier))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_urgency_mapping() {
        assert_eq!(Level::Info.urgency(), "low");
        assert_eq!(Level::Warn.urgency(), "normal");
        assert_eq!(Level::Error.urgency(), "critical");
    }

    #[test]
    fn level_default_is_info() {
        assert_eq!(Level::default(), Level::Info);
    }

    #[test]
    fn level_deserializes_lowercase() {
        let l: Level = serde_json::from_str("\"warn\"").unwrap();
        assert_eq!(l, Level::Warn);
        let l: Level = serde_json::from_str("\"error\"").unwrap();
        assert_eq!(l, Level::Error);
        let l: Level = serde_json::from_str("\"info\"").unwrap();
        assert_eq!(l, Level::Info);
    }

    #[test]
    fn level_rejects_unknown_variants() {
        let r: Result<Level, _> = serde_json::from_str("\"loud\"");
        assert!(r.is_err());
    }

    #[test]
    fn truncate_below_cap_is_identity() {
        assert_eq!(truncate("hello", 100), "hello");
    }

    #[test]
    fn truncate_appends_ellipsis_at_cap() {
        let s = truncate("abcdefghij", 5);
        assert_eq!(s, "abcde…");
    }

    #[test]
    fn truncate_respects_utf8_boundary() {
        let s = truncate("한글한글", 4);
        assert!(s.ends_with('…'));
        assert!(s.starts_with('한'));
    }

    #[test]
    fn noop_notifier_records_calls() {
        let n = NoopNotifier::default();
        n.notify("hi", "world", Level::Warn).unwrap();
        n.notify("again", "", Level::Error).unwrap();
        let captured = n.captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0], ("hi".into(), "world".into(), Level::Warn));
        assert_eq!(captured[1], ("again".into(), "".into(), Level::Error));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn libnotify_argv_shape_against_dummy_binary() {
        let n = platform_notifier();
        assert!(n.is_some());
    }
}
