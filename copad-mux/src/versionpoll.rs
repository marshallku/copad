//! Background poller for the status-bar "update available" hint.
//!
//! Network and parse failures leave the shared value untouched: an offline machine never shows
//! a spurious indicator, and a transient error never clears a found update.
//! `COPAD_MUX_UPDATE_CHECK=0` (or `update_check = false`) starts no thread.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use semver::Version;
use serde::Deserialize;

const REPO: &str = "marshallku/copad";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

const POLL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionStatus {
    /// Latest release version without the leading `v`, e.g. `"1.0.2"`.
    pub latest: String,
}

pub type Shared = Arc<Mutex<Option<VersionStatus>>>;

pub fn idle() -> Shared {
    Arc::new(Mutex::new(None))
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

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

fn evaluate(current: &Version, latest: Version) -> Option<VersionStatus> {
    (latest > *current).then(|| VersionStatus {
        latest: latest.to_string(),
    })
}

pub fn spawn() -> Shared {
    let shared = idle();
    if std::env::var("COPAD_MUX_UPDATE_CHECK").is_ok_and(|v| v == "0") {
        return shared;
    }
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

    /// Hits the real GitHub API; run with `cargo test -p copad-mux -- --ignored versionpoll`.
    #[test]
    #[ignore]
    fn live_fetch_parses_a_release() {
        let latest = fetch_latest().expect("should fetch + parse the latest release tag");
        assert!(latest >= v("1.0.0"), "unexpected latest: {latest}");
    }
}
