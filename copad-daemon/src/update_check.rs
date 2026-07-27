//! Daily GitHub-release update check for copadd.
//!
//! On start and every 24h after, fetches the `releases/latest` tag and — when it
//! is strictly newer than this build's `CARGO_PKG_VERSION` — publishes an
//! `update.available` bus event (so a GUI status bar / other consumers can show
//! a badge) and fires a native desktop toast via [`copad_core::notifier`]
//! (notify-send / osascript), ONCE per distinct new version so it doesn't nag
//! every poll. This is the update path for users who installed copad from the
//! GitHub Releases tarball (`install.sh`) — there's no package manager to tell
//! them a new version shipped.
//!
//! Runs on a dedicated thread (network I/O off the daemon's hot paths).
//! Network / parse failures are ignored (fail-silent — offline never toasts, a
//! transient error never nags). `COPAD_UPDATE_CHECK=0` disables it entirely (no
//! thread, no network).

use std::sync::Arc;
use std::time::Duration;

use copad_core::event_bus::{Event, EventBus};
use copad_core::notifier::{Level, Notifier};
use semver::Version;
use serde::Deserialize;

const REPO: &str = "marshallku/copad";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// One check a day — releases move slowly and this keeps GitHub's unauthenticated
/// rate limit (60/h) a non-issue even across many daemons on one IP.
const POLL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

/// Fetch the latest release tag as a parsed [`Version`]. `None` on any network /
/// HTTP / parse failure so the caller keeps waiting rather than reacting.
fn fetch_latest() -> Option<Version> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("copadd/{CURRENT}"))
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;
    let release: Release = client
        .get(&url)
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .ok()?;
    let tag = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    Version::parse(tag).ok()
}

/// The toast body — how to actually update, per the common install channels.
fn toast_body(latest: &str) -> String {
    format!(
        "copad {latest} is available. Update: `coctl update apply` (Linux), `brew upgrade --cask copad` (macOS), or re-run install.sh."
    )
}

/// File remembering the last release we toasted about, so "once per version"
/// holds across daemon restarts. Under the persistent state dir
/// (`~/.local/state/copad/` on Linux).
fn state_file() -> std::path::PathBuf {
    copad_core::paths::state_dir().join("last-update-notified")
}

/// The highest release version we already toasted about, if any. `None` on a
/// fresh machine or any read/parse error (→ we'll toast once, the safe direction).
fn read_last_notified() -> Option<Version> {
    let s = std::fs::read_to_string(state_file()).ok()?;
    Version::parse(s.trim()).ok()
}

/// Persist the version we just toasted about. Best-effort: a write failure only
/// means we might re-toast once after a restart, never a crash.
fn write_last_notified(version: &str) {
    let path = state_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, version);
}

/// Spawn the detached daily update checker. `COPAD_UPDATE_CHECK=0` makes it a
/// no-op. `notifier` is `None` on platforms without a desktop-notification
/// backend (the bus event still fires).
pub fn spawn(event_bus: Arc<EventBus>, notifier: Option<Arc<dyn Notifier>>) {
    if std::env::var("COPAD_UPDATE_CHECK").is_ok_and(|v| v == "0") {
        return;
    }
    // A build whose own version doesn't parse can't compare — never poll.
    let Ok(current) = Version::parse(CURRENT) else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            // High-water mark of the newest version we've already toasted about,
            // seeded from disk so "notify once" survives daemon restarts (login,
            // reboot, crash) AND a backward move of GitHub's `latest` (a deleted
            // or replaced release) can't re-notify a version we already showed —
            // we only toast strictly ABOVE the high-water. The bus event still
            // fires each poll so a late-attaching GUI can pick up the badge.
            let mut notified_hw: Option<Version> = read_last_notified();
            loop {
                if let Some(latest) = fetch_latest()
                    && latest > current
                {
                    let latest_s = latest.to_string();
                    event_bus.publish(Event::new(
                        "update.available",
                        "daemon",
                        serde_json::json!({ "current": CURRENT, "latest": latest_s }),
                    ));
                    // Toast only when strictly newer than the high-water — and
                    // advance/persist it ONLY on a successful notification, so a
                    // transient notify-send / osascript failure retries next poll
                    // instead of being suppressed.
                    let already_notified = notified_hw.as_ref().is_some_and(|hw| latest <= *hw);
                    if let Some(n) = &notifier
                        && !already_notified
                    {
                        match n.notify(
                            "copad update available",
                            &toast_body(&latest_s),
                            Level::Info,
                        ) {
                            Ok(()) => {
                                write_last_notified(&latest_s);
                                notified_hw = Some(latest);
                            }
                            Err(e) => log::warn!("update-check: notify failed: {e:?}"),
                        }
                    }
                }
                std::thread::sleep(POLL);
            }
        });
    if let Err(e) = spawned {
        log::warn!("update-check: failed to spawn poller thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_body_names_the_version_and_channels() {
        let body = toast_body("1.2.3");
        assert!(body.contains("1.2.3"));
        assert!(body.contains("coctl update apply"));
        assert!(body.contains("brew upgrade"));
    }

    /// Live GitHub round-trip: hits the real releases API + parses the tag.
    /// `#[ignore]` so CI / offline runs don't depend on the network.
    #[test]
    #[ignore]
    fn live_fetch_parses_a_release() {
        let latest = fetch_latest().expect("should fetch + parse the latest release tag");
        assert!(
            latest >= Version::parse("1.0.0").unwrap(),
            "unexpected latest: {latest}"
        );
    }
}
