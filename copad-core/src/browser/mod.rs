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
//! Tests in this tree that redirect `HOME` / `XDG_STATE_HOME` must share `TEST_ENV_LOCK`: the
//! environment is process-wide, and per-module locks race each other.

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

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
