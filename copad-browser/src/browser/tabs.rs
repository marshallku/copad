//! The persisted tab model for a browser pane.
//!
//! A session document is never filesystem authority: a history blob's path is always rebuilt
//! here from a validated tab id and generation, so a crafted import cannot redirect a blob
//! read or write through `..`, an absolute path or a symlink. The `O_NOFOLLOW` open and the
//! parent-directory assertion are the caller's half (see [`history_path`]).
//!
//! Blob generations are immutable: overwriting `<id>.bin` in place would destroy the blob the
//! previous committed session references, so a crash between writing the blob and
//! `session.json` would pair old metadata with new history. Each save writes a new generation
//! and a later load garbage-collects the old one.

use serde::{Deserialize, Serialize};

/// Ids come back from an untrusted file, so they are validated on the way in.
const MAX_TAB_ID: usize = 64;

/// Bounds a hand-edited or hostile session file.
pub const MAX_TABS_PER_PANE: usize = 100;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BrowserPaneSnap {
    pub tabs: Vec<BrowserTabSnap>,
    #[serde(default)]
    pub active: usize,
    #[serde(default = "default_profile_id")]
    pub profile: String,
}

fn default_profile_id() -> String {
    super::profile::DEFAULT_PROFILE.to_string()
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BrowserTabSnap {
    pub id: String,
    /// Already passed through [`super::restore::canonicalize_for_restore`].
    pub url: String,
    /// Persisted only under a sensitive policy: a page title can carry as much as a URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_generation: Option<u64>,
    /// Back-list depth at save time: the restore check that proves the blob took. A matching URL
    /// proves nothing, since a plain `load(url)` fallback gives the same URL with no history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_depth: Option<usize>,
    /// Scroll offset at save time. A mismatch must not discard the history blob: a page whose
    /// height changed clamps its scroll, and the back/forward stack is still good.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_y: Option<f64>,
    #[serde(default)]
    pub last_active: u64,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl BrowserTabSnap {
    pub fn new(id: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            url: url.into(),
            title: String::new(),
            pinned: false,
            history_generation: None,
            history_depth: None,
            scroll_y: None,
            last_active: 0,
        }
    }
}

/// Rejected outright rather than sanitised, because a sanitised id could collide with a
/// legitimate one.
pub fn is_valid_tab_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_TAB_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn history_dir() -> std::path::PathBuf {
    super::state_root().join("browser").join("history")
}

/// Rebuild a history blob's path from a validated id and generation; `None` for an invalid id.
/// The caller must also open it with `O_NOFOLLOW` and assert the resolved parent is
/// [`history_dir`].
pub fn history_path(tab_id: &str, generation: u64) -> Option<std::path::PathBuf> {
    if !is_valid_tab_id(tab_id) {
        return None;
    }
    Some(history_dir().join(format!("{tab_id}-{generation}.bin")))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Repairs {
    pub dropped_invalid_id: usize,
    pub dropped_duplicate_id: usize,
    pub dropped_over_cap: usize,
    pub clamped_active: bool,
}

impl Repairs {
    pub fn is_clean(&self) -> bool {
        *self == Self::default()
    }
}

/// Make an untrusted `BrowserPaneSnap` safe to restore: drops invalid and duplicate-id tabs,
/// truncates to [`MAX_TABS_PER_PANE`], clamps `active`, resets an unknown profile to the
/// default. An empty tab list stays empty and means "no browser state", not an error.
pub fn normalize(snap: &mut BrowserPaneSnap) -> Repairs {
    let mut repairs = Repairs::default();

    // Remember WHICH TAB is selected, not its index: the filters below shift indices, so
    // `["a","a","b","c"]` with active=2 ("b") would end up pointing at "c".
    let selected_id = snap.tabs.get(snap.active).map(|t| t.id.clone());

    // Drop the invalid first, and TRUNCATE BEFORE DEDUPLICATING: a crafted document with 10k
    // unique ids would otherwise be deduplicated in full before the cap applied.
    let before_invalid = snap.tabs.len();
    snap.tabs.retain(|t| is_valid_tab_id(&t.id));
    repairs.dropped_invalid_id = before_invalid - snap.tabs.len();

    if snap.tabs.len() > MAX_TABS_PER_PANE {
        repairs.dropped_over_cap = snap.tabs.len() - MAX_TABS_PER_PANE;
        snap.tabs.truncate(MAX_TABS_PER_PANE);
    }

    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(snap.tabs.len());
    let before_dupes = snap.tabs.len();
    snap.tabs.retain(|t| seen.insert(t.id.clone()));
    repairs.dropped_duplicate_id = before_dupes - snap.tabs.len();

    if !super::profile::is_valid_profile_id(&snap.profile) {
        snap.profile = default_profile_id();
    }

    // `active` is pinned to 0 on an empty list so no caller can index with it.
    if snap.tabs.is_empty() {
        if snap.active != 0 {
            repairs.clamped_active = true;
        }
        snap.active = 0;
        return repairs;
    }

    // Re-find the selected tab; clamp only if it did not survive.
    let resolved = selected_id.and_then(|id| snap.tabs.iter().position(|t| t.id == id));
    match resolved {
        Some(i) => {
            if i != snap.active {
                repairs.clamped_active = true;
                snap.active = i;
            }
        }
        None => {
            let limit = snap.tabs.len() - 1;
            if snap.active > limit {
                repairs.clamped_active = true;
                snap.active = limit;
            }
        }
    }

    repairs
}

pub fn live_generations(snap: &BrowserPaneSnap) -> Vec<(String, u64)> {
    snap.tabs
        .iter()
        .filter_map(|t| t.history_generation.map(|g| (t.id.clone(), g)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(ids: &[&str], active: usize) -> BrowserPaneSnap {
        BrowserPaneSnap {
            tabs: ids
                .iter()
                .map(|i| BrowserTabSnap::new(*i, "https://e.com"))
                .collect(),
            active,
            profile: "default".into(),
        }
    }

    #[test]
    fn tab_ids_reject_anything_that_could_become_a_path() {
        assert!(is_valid_tab_id("a1-b2_c3"));
        assert!(!is_valid_tab_id(""));
        assert!(!is_valid_tab_id(".."));
        assert!(!is_valid_tab_id("../../etc/passwd"));
        assert!(!is_valid_tab_id("/abs"));
        assert!(!is_valid_tab_id("a.bin"));
        assert!(!is_valid_tab_id(&"x".repeat(MAX_TAB_ID + 1)));
    }

    #[test]
    fn history_path_is_rebuilt_and_confined() {
        super::super::with_state_root(std::env::temp_dir().join("copad-tabs-path"), || {
            let p = history_path("tab1", 7).expect("valid");
            assert_eq!(p.file_name().unwrap(), "tab1-7.bin");
            assert_eq!(p.parent().unwrap(), history_dir());
            assert!(history_path("../../etc/passwd", 1).is_none());
            assert!(history_path("a/b", 1).is_none());
        });
    }

    #[test]
    fn a_new_generation_is_a_new_file_so_the_previous_one_survives() {
        super::super::with_state_root(std::env::temp_dir().join("copad-tabs-path"), || {
            let old = history_path("tab1", 1).unwrap();
            let new = history_path("tab1", 2).unwrap();
            assert_ne!(old, new);
        });
    }

    #[test]
    fn normalize_drops_invalid_and_duplicate_ids_keeping_the_first() {
        let mut s = snap(&["a", "..", "a", "b"], 0);
        let r = normalize(&mut s);
        assert_eq!(
            s.tabs.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(r.dropped_invalid_id, 1);
        assert_eq!(r.dropped_duplicate_id, 1);
    }

    #[test]
    fn normalize_clamps_an_out_of_range_active_index() {
        let mut s = snap(&["a", "b"], 9);
        let r = normalize(&mut s);
        assert_eq!(s.active, 1);
        assert!(r.clamped_active);
    }

    #[test]
    fn normalize_pins_active_to_zero_on_an_empty_tab_list() {
        let mut s = snap(&[], 5);
        normalize(&mut s);
        assert!(s.tabs.is_empty());
        assert_eq!(s.active, 0);
    }

    #[test]
    fn normalize_resets_an_unknown_profile_to_the_default() {
        let mut s = snap(&["a"], 0);
        s.profile = "../evil".into();
        normalize(&mut s);
        assert_eq!(s.profile, super::super::profile::DEFAULT_PROFILE);
    }

    #[test]
    fn normalize_caps_a_hostile_tab_count() {
        let ids: Vec<String> = (0..MAX_TABS_PER_PANE + 5)
            .map(|i| format!("t{i}"))
            .collect();
        let mut s = BrowserPaneSnap {
            tabs: ids
                .iter()
                .map(|i| BrowserTabSnap::new(i, "https://e.com"))
                .collect(),
            active: 0,
            profile: "default".into(),
        };
        let r = normalize(&mut s);
        assert_eq!(s.tabs.len(), MAX_TABS_PER_PANE);
        assert_eq!(r.dropped_over_cap, 5);
    }

    #[test]
    fn normalize_caps_before_deduplicating_so_the_advertised_limit_bounds_the_work() {
        let mut s = BrowserPaneSnap {
            tabs: (0..MAX_TABS_PER_PANE * 5)
                .map(|i| BrowserTabSnap::new(format!("t{i}"), "https://e.com"))
                .collect(),
            active: 0,
            profile: "default".into(),
        };
        let r = normalize(&mut s);
        assert_eq!(s.tabs.len(), MAX_TABS_PER_PANE);
        assert_eq!(r.dropped_over_cap, MAX_TABS_PER_PANE * 4);
        assert_eq!(r.dropped_duplicate_id, 0);
    }

    #[test]
    fn normalize_keeps_the_selected_tab_selected_when_earlier_tabs_are_dropped() {
        let mut s = snap(&["a", "a", "b", "c"], 2); // "b"
        normalize(&mut s);
        assert_eq!(
            s.tabs.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(s.tabs[s.active].id, "b", "the selected tab must not drift");
    }

    #[test]
    fn normalize_falls_back_to_clamping_when_the_selected_tab_is_dropped() {
        let mut s = snap(&["a", "..", "b"], 1); // the invalid one was selected
        let r = normalize(&mut s);
        assert_eq!(s.tabs.len(), 2);
        assert!(s.active < s.tabs.len());
        assert!(r.dropped_invalid_id > 0);
    }

    #[test]
    fn normalize_reports_a_clean_document_as_clean() {
        let mut s = snap(&["a", "b"], 1);
        assert!(normalize(&mut s).is_clean());
    }

    #[test]
    fn live_generations_lists_only_tabs_that_wrote_a_blob() {
        let mut s = snap(&["a", "b"], 0);
        s.tabs[1].history_generation = Some(3);
        assert_eq!(live_generations(&s), vec![("b".to_string(), 3)]);
    }

    /// Rust half of the cross-language schema check; the Swift half is
    /// `copad-macos/Tests/CopadCoreTests/BrowserSnapshotTests.swift`, and the literal below must
    /// stay byte-identical to the one there. Fully populated on purpose: a fixture of defaults
    /// round-trips even when one side lacks a field. Keys are sorted on both sides. A schema
    /// change updates this fixture in the same commit.
    const SHARED_SWIFT_FIXTURE: &str = r#"{"active":1,"profile":"work","tabs":[{"history_depth":3,"history_generation":7,"id":"tab-a","last_active":1725840000,"pinned":true,"scroll_y":412.5,"title":"Pull Request #42","url":"https://github.com/o/r/pull/42"},{"id":"tab-b","last_active":1725840001,"url":"https://example.com"}]}"#;

    #[test]
    fn the_shared_swift_fixture_round_trips() {
        let pane: BrowserPaneSnap =
            serde_json::from_str(SHARED_SWIFT_FIXTURE).expect("fixture parses");
        assert_eq!(pane.active, 1);
        assert_eq!(pane.profile, "work");
        assert_eq!(pane.tabs.len(), 2);
        assert_eq!(pane.tabs[0].history_generation, Some(7));
        assert_eq!(pane.tabs[0].history_depth, Some(3));
        assert_eq!(pane.tabs[0].scroll_y, Some(412.5));
        assert!(pane.tabs[0].pinned);
        assert_eq!(pane.tabs[0].title, "Pull Request #42");
        assert!(!pane.tabs[1].pinned);
        assert!(pane.tabs[1].title.is_empty());
        assert_eq!(pane.tabs[1].history_generation, None);

        let sorted = serde_json::to_value(&pane).expect("serializes");
        assert_eq!(
            serde_json::to_string(&sorted).expect("re-serializes"),
            SHARED_SWIFT_FIXTURE
        );
    }

    #[test]
    fn a_tab_with_no_browser_state_omits_every_optional_field() {
        let tab = BrowserTabSnap::new("t1", "https://e.com");
        assert_eq!(
            serde_json::to_string(&serde_json::to_value(&tab).unwrap()).unwrap(),
            r#"{"id":"t1","last_active":0,"url":"https://e.com"}"#
        );
    }
}
