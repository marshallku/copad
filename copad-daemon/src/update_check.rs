//! Daily GitHub-release update check. Publishes `update.available` on the bus every poll when the
//! latest tag is newer than `CARGO_PKG_VERSION`, and toasts once per distinct version.
//!
//! Network and parse failures are ignored. `COPAD_UPDATE_CHECK=0` disables it (no thread, no
//! network).

use std::sync::Arc;
use std::time::Duration;

use copad_core::event_bus::{Event, EventBus};
use copad_core::notifier::{Level, Notifier};
use semver::Version;
use serde::Deserialize;

const REPO: &str = "marshallku/copad";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// One check a day keeps GitHub's unauthenticated rate limit (60/h) a non-issue.
const POLL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

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

fn toast_body(latest: &str) -> String {
    format!(
        "copad {latest} is available. Update: `coctl update apply` (Linux), `brew upgrade --cask copad` (macOS), or re-run install.sh."
    )
}

/// Remembers the last toasted release so "once per version" holds across daemon restarts.
fn state_file() -> std::path::PathBuf {
    copad_core::paths::state_dir().join("last-update-notified")
}

/// `None` on a fresh machine or any read/parse error (toast once, the safe direction).
fn read_last_notified() -> Option<Version> {
    let s = std::fs::read_to_string(state_file()).ok()?;
    Version::parse(s.trim()).ok()
}

/// Best-effort: a failed write only risks one re-toast after a restart.
fn write_last_notified(version: &str) {
    let path = state_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, version);
}

pub fn spawn(event_bus: Arc<EventBus>, notifier: Option<Arc<dyn Notifier>>) {
    if std::env::var("COPAD_UPDATE_CHECK").is_ok_and(|v| v == "0") {
        return;
    }
    let Ok(current) = Version::parse(CURRENT) else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            // High-water mark of versions already toasted, seeded from disk: notify-once survives
            // restarts, and a backward move of GitHub's `latest` cannot re-notify. The bus event
            // still fires every poll so a late-attaching GUI gets the badge.
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
                    // The high-water mark advances only on a successful notification, so a transient
                    // notify-send / osascript failure retries next poll.
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
