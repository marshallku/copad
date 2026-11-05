use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// Session model (decisions #64): a flat tab list, each tab a split tree of typed panes. Only the
// layout and each terminal's cwd persist; terminals restore as fresh shells (decisions #63).
// Older v1/v2 files are rejected by the loader and a fresh session starts.

pub const SESSION_VERSION: u32 = 3;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Session {
    pub version: u32,
    pub tabs: Vec<TabSnap>,
    pub current_tab: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TabSnap {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_title: Option<String>,
    pub root: SplitSnap,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SplitSnap {
    Leaf {
        content: PaneContent,
    },
    Branch {
        orientation: SplitOrientation,
        ratio: f32,
        first: Box<SplitSnap>,
        second: Box<SplitSnap>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneContent {
    Terminal {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    Webview {
        /// The pane's URL under the active `[browser] restore` policy: by default the canonical
        /// origin only, never the live URL (credentials, OAuth codes, signed URLs); see
        /// `browser::restore`.
        url: String,
        /// Full browser state: tab list, active index, profile. `#[serde(default)]` so v3 files
        /// written before the Browser Workbench keep loading without a version bump. An older
        /// binary drops this field and degrades to single-URL restore (lossy, not corrupt); a
        /// version bump would wipe every user's layout.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<crate::browser::BrowserPaneSnap>,
    },
    Plugin {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        panel_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    Cockpit,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SplitOrientation {
    Horizontal,
    Vertical,
}

/// Divider-ratio clamp band: a persisted extreme value must not collapse a pane.
pub const MIN_RATIO: f32 = 0.05;
pub const MAX_RATIO: f32 = 0.95;

pub fn clamp_ratio(r: f32) -> f32 {
    if r.is_finite() {
        r.clamp(MIN_RATIO, MAX_RATIO)
    } else {
        0.5
    }
}

pub fn normalize_ratios(snap: &mut SplitSnap) {
    if let SplitSnap::Branch {
        ratio,
        first,
        second,
        ..
    } = snap
    {
        *ratio = clamp_ratio(*ratio);
        normalize_ratios(first);
        normalize_ratios(second);
    }
}

pub fn session_path() -> PathBuf {
    crate::paths::state_dir().join("session.json")
}

pub fn load() -> Option<Session> {
    let raw = std::fs::read_to_string(session_path()).ok()?;
    parse_session(&raw)
}

/// Parse and validate a session document. Rejects unknown versions outright: a best-effort parse
/// of a foreign schema risks a half-restored state worse than a fresh start (old v1/v2 files).
pub fn parse_session(raw: &str) -> Option<Session> {
    let mut session: Session = serde_json::from_str(raw)
        .map_err(|e| eprintln!("[copad] session parse failed: {e}"))
        .ok()?;
    if session.version != SESSION_VERSION {
        eprintln!(
            "[copad] session version mismatch (file={}, expected={SESSION_VERSION}) — ignoring",
            session.version
        );
        return None;
    }
    if session.tabs.is_empty() {
        return None;
    }
    for t in &mut session.tabs {
        normalize_ratios(&mut t.root);
    }
    Some(session)
}

pub fn clear() {
    let path = session_path();
    if let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("[copad] session clear failed: {e}");
    }
}

pub fn save(session: &Session) {
    let path = session_path();
    let Some(parent) = path.parent() else { return };
    if let Err(e) = std::fs::create_dir_all(parent) {
        eprintln!(
            "[copad] session save: mkdir {} failed: {e}",
            parent.display()
        );
        return;
    }
    let json = match serde_json::to_string_pretty(session) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[copad] session serialize failed: {e}");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, json) {
        eprintln!("[copad] session write {} failed: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        eprintln!("[copad] session rename failed: {e}");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Leftmost (DFS pre-order) TERMINAL leaf's cwd, skipping webview/plugin/cockpit leaves; seeds
/// the panel that opens the tab on restore.
pub fn leftmost_cwd(snap: &SplitSnap) -> Option<String> {
    match snap {
        SplitSnap::Leaf {
            content: PaneContent::Terminal { cwd },
        } => cwd.clone(),
        SplitSnap::Leaf { .. } => None,
        SplitSnap::Branch { first, second, .. } => {
            leftmost_cwd(first).or_else(|| leftmost_cwd(second))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cwd: &str) -> SplitSnap {
        SplitSnap::Leaf {
            content: PaneContent::Terminal {
                cwd: Some(cwd.to_string()),
            },
        }
    }

    fn webview(url: &str) -> SplitSnap {
        SplitSnap::Leaf {
            content: PaneContent::Webview {
                url: url.to_string(),
                pane: None,
            },
        }
    }

    fn branch(o: SplitOrientation, ratio: f32, first: SplitSnap, second: SplitSnap) -> SplitSnap {
        SplitSnap::Branch {
            orientation: o,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn round_trips_mixed_typed_tree() {
        let s = Session {
            version: SESSION_VERSION,
            current_tab: 1,
            tabs: vec![
                TabSnap {
                    custom_title: Some("editor".into()),
                    root: branch(
                        SplitOrientation::Horizontal,
                        0.6,
                        branch(
                            SplitOrientation::Vertical,
                            0.5,
                            term("/a"),
                            webview("https://example.com"),
                        ),
                        SplitSnap::Leaf {
                            content: PaneContent::Plugin {
                                name: "kb".into(),
                                panel_name: Some("notes".into()),
                                version: Some("1".into()),
                            },
                        },
                    ),
                },
                TabSnap {
                    custom_title: None,
                    root: SplitSnap::Leaf {
                        content: PaneContent::Cockpit,
                    },
                },
            ],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn parse_rejects_old_and_unknown_versions() {
        let v1 = r#"{"version":1,"tabs":[{"custom_title":null,"root":{"type":"terminal","cwd":"/x"}}],"current_tab":0}"#;
        assert!(parse_session(v1).is_none(), "old v1 file rejected → fresh");
        let v2 = r#"{"version":2,"sessions":[]}"#;
        assert!(parse_session(v2).is_none(), "old v2 file rejected → fresh");
        let future = r#"{"version":999,"tabs":[],"current_tab":0}"#;
        assert!(
            parse_session(future).is_none(),
            "unknown future version rejected"
        );
    }

    #[test]
    fn parse_rejects_empty_tab_list() {
        let doc = r#"{"version":3,"tabs":[],"current_tab":0}"#;
        assert!(parse_session(doc).is_none());
    }

    #[test]
    fn parse_clamps_out_of_band_ratio() {
        let doc = r#"{"version":3,"current_tab":0,"tabs":[{"root":{"type":"branch","orientation":"horizontal","ratio":9.0,"first":{"type":"leaf","content":{"kind":"terminal"}},"second":{"type":"leaf","content":{"kind":"terminal"}}}}]}"#;
        let s = parse_session(doc).expect("v3 doc parses");
        match &s.tabs[0].root {
            SplitSnap::Branch { ratio, .. } => {
                assert!(
                    *ratio <= MAX_RATIO && *ratio >= MIN_RATIO,
                    "ratio clamped on load"
                );
            }
            _ => panic!("expected branch"),
        }
    }

    #[test]
    fn leftmost_cwd_skips_non_terminal_leaves() {
        let s = branch(
            SplitOrientation::Horizontal,
            0.5,
            webview("https://x"),
            term("/deep"),
        );
        assert_eq!(leftmost_cwd(&s), Some("/deep".to_string()));
        let s = branch(
            SplitOrientation::Horizontal,
            0.5,
            branch(
                SplitOrientation::Vertical,
                0.5,
                webview("https://y"),
                term("/a"),
            ),
            term("/b"),
        );
        assert_eq!(leftmost_cwd(&s), Some("/a".to_string()));
    }

    #[test]
    fn leftmost_cwd_none_when_no_terminal() {
        let s = branch(
            SplitOrientation::Horizontal,
            0.5,
            webview("https://x"),
            SplitSnap::Leaf {
                content: PaneContent::Cockpit,
            },
        );
        assert_eq!(leftmost_cwd(&s), None);
    }

    #[test]
    fn round_trip_through_parse_after_save_shape() {
        let s = Session {
            version: SESSION_VERSION,
            current_tab: 0,
            tabs: vec![TabSnap {
                custom_title: None,
                root: term("/home/x"),
            }],
        };
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back = parse_session(&json).expect("our own output parses back");
        assert_eq!(s, back);
    }

    #[test]
    fn a_pre_workbench_v3_webview_pane_still_loads() {
        // `pane` is `#[serde(default)]` rather than a version bump because every existing layout
        // file lacks the field.
        let raw = r#"{
            "version": 3,
            "current_tab": 0,
            "tabs": [{ "root": {
                "type": "leaf",
                "content": { "kind": "webview", "url": "https://github.com" }
            }}]
        }"#;
        let s = parse_session(raw).expect("legacy v3 webview pane must still load");
        let SplitSnap::Leaf { content } = &s.tabs[0].root else {
            panic!("expected a leaf");
        };
        assert_eq!(
            content,
            &PaneContent::Webview {
                url: "https://github.com".into(),
                pane: None,
            }
        );
    }

    #[test]
    fn a_pane_without_browser_state_serializes_exactly_as_before() {
        // `skip_serializing_if` keeps the on-disk shape byte-identical for users who never open
        // a second browser tab.
        let json = serde_json::to_string(&PaneContent::Webview {
            url: "https://e.com".into(),
            pane: None,
        })
        .unwrap();
        assert!(!json.contains("pane"), "{json}");
    }

    #[test]
    fn browser_tabs_round_trip_through_a_session_document() {
        use crate::browser::{BrowserPaneSnap, BrowserTabSnap};

        let pane = BrowserPaneSnap {
            tabs: vec![
                BrowserTabSnap::new("tab-a", "https://github.com/o/r/pull/42"),
                BrowserTabSnap::new("tab-b", "https://example.com"),
            ],
            active: 1,
            profile: "default".into(),
        };
        let session = Session {
            version: SESSION_VERSION,
            current_tab: 0,
            tabs: vec![TabSnap {
                custom_title: None,
                root: SplitSnap::Leaf {
                    content: PaneContent::Webview {
                        url: "https://github.com/o/r/pull/42".into(),
                        pane: Some(pane),
                    },
                },
            }],
        };
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(parse_session(&json).expect("round-trips"), session);
    }

    #[test]
    fn an_older_binary_downgrades_a_browser_pane_lossily_but_not_corruptly() {
        // Simulate the old binary by parsing into a shape with no `pane` field and re-serializing:
        // the layout survives, the tab list is dropped, the document stays valid.
        use crate::browser::{BrowserPaneSnap, BrowserTabSnap};

        let with_tabs = PaneContent::Webview {
            url: "https://github.com".into(),
            pane: Some(BrowserPaneSnap {
                tabs: vec![BrowserTabSnap::new("tab-a", "https://github.com")],
                active: 0,
                profile: "default".into(),
            }),
        };
        let json = serde_json::to_string(&with_tabs).unwrap();

        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum OldPaneContent {
            Webview { url: String },
        }
        let old: OldPaneContent = serde_json::from_str(&json).expect("old binary still parses");
        let rewritten = serde_json::to_string(&old).unwrap();

        let back: PaneContent = serde_json::from_str(&rewritten).unwrap();
        assert_eq!(
            back,
            PaneContent::Webview {
                url: "https://github.com".into(),
                pane: None,
            }
        );
    }
}
