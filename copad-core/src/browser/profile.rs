//! Browser profiles. The `default` profile must map onto the platform's default website data
//! store, not a store named "default": on macOS `WKWebsiteDataStore(forIdentifier:)` is distinct
//! from `WKWebsiteDataStore.default()`, so adopting a named store would sign existing users out
//! of every site. Named stores are only for profiles the user creates, which start logged out.

use serde::{Deserialize, Serialize};

pub const DEFAULT_PROFILE: &str = "default";

/// Upper bound on a profile id, so an id can be used as a single path segment
/// without truncation surprises.
const MAX_PROFILE_ID: usize = 64;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BrowserProfile {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default = "default_true")]
    pub persistent: bool,
}

fn default_true() -> bool {
    true
}

impl BrowserProfile {
    pub fn default_profile() -> Self {
        Self {
            id: DEFAULT_PROFILE.to_string(),
            label: None,
            persistent: true,
        }
    }

    pub fn is_platform_default(&self) -> bool {
        self.id == DEFAULT_PROFILE
    }
}

/// A profile id becomes a directory name on Linux, so it is held to the same
/// strict charset a tab id is: no separators, no dots, no traversal.
pub fn is_valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_PROFILE_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Storage directory for a NON-default profile. Returns `None` for an invalid
/// id and for `default`, whose storage the platform owns (macOS: the default
/// `WKWebsiteDataStore`; Linux: the shared `NetworkSession` data directory).
pub fn profile_data_dir(id: &str) -> Option<std::path::PathBuf> {
    if !is_valid_profile_id(id) || id == DEFAULT_PROFILE {
        return None;
    }
    Some(
        crate::paths::state_dir()
            .join("browser")
            .join("profiles")
            .join(id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_flagged_as_the_platform_default_store() {
        assert!(BrowserProfile::default_profile().is_platform_default());
        assert!(profile_data_dir(DEFAULT_PROFILE).is_none());
    }

    #[test]
    fn profile_ids_reject_path_separators_and_traversal() {
        assert!(is_valid_profile_id("work"));
        assert!(is_valid_profile_id("client-a_2"));
        assert!(!is_valid_profile_id(""));
        assert!(!is_valid_profile_id(".."));
        assert!(!is_valid_profile_id("a/b"));
        assert!(!is_valid_profile_id("a.b"));
        assert!(!is_valid_profile_id(&"x".repeat(MAX_PROFILE_ID + 1)));
    }

    #[test]
    fn a_named_profile_gets_a_confined_directory() {
        let dir = profile_data_dir("work").expect("valid id");
        assert!(dir.ends_with("browser/profiles/work"));
        assert!(profile_data_dir("../escape").is_none());
    }
}
