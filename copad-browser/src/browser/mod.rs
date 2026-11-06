//! Shared spine under copad's webview panes: models, policy, path rules and redaction used by both
//! GUIs (`docs/browser-workbench-plan.md`).
//!
//! Invariants that must survive edits:
//!
//! - A credential only ever enters a tab that is already in [`TabMode::Protected`], which the GUI
//!   enters by destroying and rebuilding the web view, so no script from the old document survives.
//!   Refusing read-back after a fill is not enough: a listener installed earlier could keep a copy.
//! - Entering and leaving protection are explicit user acts. Load timing and navigation commits
//!   cannot reliably identify a password form or a successful login.
//! - Protection freezes the profile, not the tab: another tab on the same profile shares storage.
//! - Capture is suppressed at the source. The JSONL files are agent-readable, so blocking
//!   `webview.net` reads while they keep filling defends nothing.
//! - A session file is never filesystem authority. Tab ids are validated against a strict
//!   charset and blob paths are always rebuilt here; no string from a session document is joined
//!   as a path.
//! - Redaction is hygiene, not a boundary (a page can `console.log` anything). The boundary is
//!   [`TabMode`], enforced in [`authorize`].
//!
//! Everything on disk lives under the root the host passes to [`set_state_root`] (copad: its
//! `paths::state_dir()`; AIDE: its own folder, so the two never share a profile or a credential
//! index). Tests that move the root must hold `TEST_ENV_LOCK`: the root is process-wide, and
//! per-module locks race each other.

pub mod authorize;
pub mod history;
pub mod netlog;
pub mod profile;
pub mod restore;
pub mod secrets;
pub mod tabs;

pub use authorize::{MethodClass, authorize, classify};
pub use netlog::{ConsoleRecord, LogCaps, NETLOG_COVERAGE, NetRecord};
pub use profile::{BrowserProfile, DEFAULT_PROFILE};
pub use restore::{RestorePolicy, canonical_origin, canonicalize_for_restore};
pub use secrets::{CredentialRef, CredentialSlot, FillRequest, TabMode};
pub use tabs::{BrowserPaneSnap, BrowserTabSnap};

static STATE_ROOT: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);

/// Where browser state (history blobs, logs, the credential index, profiles) lives. Set once at
/// startup, before any pane exists; a later call moves state for calls made after it.
pub fn set_state_root(dir: std::path::PathBuf) {
    *STATE_ROOT.write().unwrap_or_else(|e| e.into_inner()) = Some(dir);
}

/// The root [`set_state_root`] set.
///
/// # Panics
/// When the host never set one: writing credentials or history under a guessed directory would
/// be worse than failing loudly.
pub fn state_root() -> std::path::PathBuf {
    STATE_ROOT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("copad_browser::browser::set_state_root was not called")
}

#[cfg(test)]
pub(crate) fn replace_state_root(dir: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    std::mem::replace(
        &mut *STATE_ROOT.write().unwrap_or_else(|e| e.into_inner()),
        dir,
    )
}

/// Runs `f` with the state root at `root`, holding `TEST_ENV_LOCK`.
#[cfg(test)]
pub(crate) fn with_state_root<T>(root: std::path::PathBuf, f: impl FnOnce() -> T) -> T {
    let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = replace_state_root(Some(root));
    let out = f();
    replace_state_root(prev);
    out
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
