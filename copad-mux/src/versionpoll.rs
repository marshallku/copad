//! Background poller for the status-bar "update available" hint.
//!
//! Hits the GitHub releases API on start and every few hours after, parses the
//! latest release tag, and — when it is strictly newer than this build's
//! `CARGO_PKG_VERSION` — shares a [`VersionStatus`] so the status bar can show a
//! `⬆ x.y.z` indicator. This is the update path for users who installed comux
//! via the remote `install-comux.sh` script: they get a passive nudge instead of
//! having to remember to check the releases page.
//!
//! Releases move slowly and each poll is a network round-trip, so the interval
//! is long. Network / parse failures leave the shared value untouched
//! (fail-silent — an offline machine never shows a spurious indicator, and a
//! transient error never clears a previously-found update). `COPAD_MUX_UPDATE_CHECK=0`
//! (or `update_check = false` in mux.toml) disables it — no thread, no network.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use semver::Version;
use serde::Deserialize;

const REPO: &str = "marshallku/copad";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Releases change rarely; a few checks a day is plenty and keeps GitHub's
/// unauthenticated rate limit (60/h) a non-issue.
const POLL: Duration = Duration::from_secs(6 * 60 * 60);

/// The latest release, shared only once it is known to be newer than the running
/// build. `None` = up to date / unknown (nothing to show).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionStatus {
    /// Latest release version without the leading `v`, e.g. `"1.0.2"`.
    pub latest: String,
}

/// Handle the render loop reads. `None` inside = no update to advertise.
pub type Shared = Arc<Mutex<Option<VersionStatus>>>;

/// A never-updated handle for `new()` / tests / clients (no poll thread).
pub fn idle() -> Shared {
    Arc::new(Mutex::new(None))
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

/// Fetch the latest release tag as a parsed [`Version`]. `None` on any network /
/// HTTP / parse failure so the caller keeps the previous value.
fn fetch_latest() -> Option<Version> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("comux/{CURRENT}"))
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

/// Compare a fetched `latest` against the running build. `Some(status)` only when
/// strictly newer; `None` otherwise (so a same/older tag clears any prior hint).
fn evaluate(current: &Version, latest: Version) -> Option<VersionStatus> {
    (latest > *current).then(|| VersionStatus {
        latest: latest.to_string(),
    })
}

/// Spawn the detached poller thread and return the handle the status bar reads.
/// `COPAD_MUX_UPDATE_CHECK=0` returns an idle handle that stays empty forever.
pub fn spawn() -> Shared {
    let shared = idle();
    if std::env::var("COPAD_MUX_UPDATE_CHECK").is_ok_and(|v| v == "0") {
        return shared;
    }
    // A build whose own version doesn't parse can't compare — never poll.
    let Ok(current) = Version::parse(CURRENT) else {
        return shared;
    };
    let out = shared.clone();
    let _ = std::thread::Builder::new()
        .name("version-poll".into())
        .spawn(move || {
            loop {
                if let Some(latest) = fetch_latest()
                    && let Ok(mut g) = out.lock()
                {
                    *g = evaluate(&current, latest);
                }
                std::thread::sleep(POLL);
            }
        });
    shared
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn newer_release_is_advertised() {
        let status = evaluate(&v("1.0.1"), v("1.0.2"));
        assert_eq!(
            status,
            Some(VersionStatus {
                latest: "1.0.2".into()
            })
        );
    }

    #[test]
    fn same_or_older_clears_hint() {
        assert_eq!(evaluate(&v("1.0.1"), v("1.0.1")), None);
        assert_eq!(evaluate(&v("1.0.1"), v("1.0.0")), None);
    }

    #[test]
    fn semver_not_lexical() {
        // 1.0.10 > 1.0.9 numerically, though it sorts BEFORE as a string.
        assert!(evaluate(&v("1.0.9"), v("1.0.10")).is_some());
    }

    /// Live GitHub round-trip: hits the real releases API + parses the tag.
    /// `#[ignore]` so CI / offline runs don't depend on the network; run with
    /// `cargo test -p copad-mux -- --ignored versionpoll`.
    #[test]
    #[ignore]
    fn live_fetch_parses_a_release() {
        let latest = fetch_latest().expect("should fetch + parse the latest release tag");
        // Any published release is >= the first stable tag.
        assert!(latest >= v("1.0.0"), "unexpected latest: {latest}");
    }
}
