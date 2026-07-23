//! Background poller for the status-bar usage/limits readout.
//!
//! The value is `coctl usage --limits --oneline` — Claude 5h + weekly (a live
//! OAuth call) and Codex weekly (newest rollout snapshot). That shells out and
//! does network I/O, so it MUST NOT run on the render loop: a dedicated thread
//! refreshes it every [`POLL`] into a shared string the status bar reads under a
//! cheap lock. `COPAD_MUX_USAGE=0` disables it (mirrors `COPAD_MUX_NOTIFY`).

use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often to re-poll. The 5h / weekly windows move slowly and each poll is a
/// process spawn + network round-trip, so a minute is plenty.
const POLL: Duration = Duration::from_secs(60);

/// Latest-wins usage string shared with the render loop. Empty = show nothing
/// (not yet fetched, disabled, or currently unavailable).
pub type Shared = Arc<Mutex<String>>;

/// An empty handle with no poller behind it (default before `spawn`, and in
/// tests that construct an `App` without a server).
pub fn idle() -> Shared {
    Arc::new(Mutex::new(String::new()))
}

/// Spawn the detached poller thread and return the handle the status bar reads.
/// `COPAD_MUX_USAGE=0` returns an idle handle that stays empty forever.
pub fn spawn() -> Shared {
    let shared = idle();
    if std::env::var("COPAD_MUX_USAGE").is_ok_and(|v| v == "0") {
        return shared;
    }
    let out = shared.clone();
    // Detached: lives for the server's lifetime, reaped by process exit. It never
    // holds the lock across the sleep, so it can't block teardown.
    let _ = std::thread::Builder::new()
        .name("usage-poll".into())
        .spawn(move || {
            let coctl = coctl_path();
            loop {
                if let Some(s) = fetch(&coctl)
                    && let Ok(mut g) = out.lock()
                {
                    *g = s;
                }
                std::thread::sleep(POLL);
            }
        });
    shared
}

/// Run `coctl usage --limits --oneline`. `Some(text)` = fresh value (empty text
/// = ran fine but nothing to show → clear the readout). `None` = the command
/// couldn't run / errored → keep the previous value rather than blanking on a
/// transient failure.
fn fetch(coctl: &OsStr) -> Option<String> {
    let out = Command::new(coctl)
        .args(["usage", "--limits", "--oneline"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s == "no limits" {
        return Some(String::new());
    }
    if s.is_empty() {
        return None;
    }
    Some(s)
}

/// Prefer the `coctl` next to the running `comux` binary (install scripts drop
/// them together) — the server is often launched from a desktop entry / cron
/// with a PATH that lacks `~/.local/bin`. Fall back to bare `coctl` on PATH.
fn coctl_path() -> OsString {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("coctl");
        if sibling.is_file() {
            return sibling.into_os_string();
        }
    }
    "coctl".into()
}
