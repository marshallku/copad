use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ============================================================================
// v3 session model (decision #64 — SSH-first / Model 1 simplification).
//
// A flat tab list; each tab is a split tree of TYPED panes. Persistence is
// LAYOUT-ONLY: tab list + split layout (orientation + normalized ratio) + each
// terminal's cwd. Terminals restore as FRESH shells — process/scrollback
// persistence is the user's own tmux, not copad's (decision #63). This replaces
// the v1 (terminal-only) and v2 (workspace → sub-tab → pane, tmux-backed) models,
// both deleted. Old v1/v2 files are rejected by the loader and a fresh session
// starts — the accepted wipe-fresh transition.
// ============================================================================

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

/// A split tree: a typed leaf pane, or a branch of two subtrees. `ratio` is a
/// normalized 0..1 divider position (never platform pixels) so a split's size
/// restores faithfully across Linux and macOS.
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

/// Typed pane content. Non-terminal panes are copad-native (tmux never backs
/// them). A terminal persists only its cwd — it restores as a fresh shell.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneContent {
    Terminal {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    Webview {
        /// An explicitly approved canonical (origin-only) URL — never the live
        /// URL (creds / OAuth codes / signed URLs).
        url: String,
    },
    Plugin {
        name: String,
        /// The specific panel within a multi-panel plugin, so restore targets
        /// the original panel rather than the plugin's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        panel_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    /// The agent-status cockpit pane (Linux). No persisted state beyond its kind.
    Cockpit,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SplitOrientation {
    Horizontal,
    Vertical,
}

/// Divider-ratio clamp band so a persisted extreme / corrupt value can't
/// collapse a pane below usability.
pub const MIN_RATIO: f32 = 0.05;
pub const MAX_RATIO: f32 = 0.95;

pub fn clamp_ratio(r: f32) -> f32 {
    if r.is_finite() {
        r.clamp(MIN_RATIO, MAX_RATIO)
    } else {
        0.5
    }
}

/// Clamp every divider ratio in a tree (defense against hand-edited / corrupt
/// files) so downstream layout never sees an out-of-band value.
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

/// Parse + validate a session document. Split from `load` so it is unit-testable
/// without the filesystem. Rejects unknown versions outright (a best-effort parse
/// of a foreign schema risks a half-restored state worse than starting fresh) —
/// this is what makes old v1/v2 files fall back to a fresh session (wipe-fresh).
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

/// Remove any persisted session file. Called when the closing window has no
/// panels left so a stale snapshot doesn't restore on next launch.
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

/// Walk a `SplitSnap` and return the cwd of the leftmost (DFS pre-order)
/// TERMINAL leaf, skipping non-terminal (webview / plugin / cockpit) leaves so a
/// non-terminal-first tree still seeds a later terminal's cwd. Used at restore:
/// the cwd of the first terminal seeds the panel that opens the tab.
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
        // v1 doc (version 1) and v2 doc (version 2) both fall back to fresh.
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
        // A webview-first branch must still find the terminal's cwd in `second`.
        let s = branch(
            SplitOrientation::Horizontal,
            0.5,
            webview("https://x"),
            term("/deep"),
        );
        assert_eq!(leftmost_cwd(&s), Some("/deep".to_string()));
        // Nested: leftmost terminal wins.
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
}
