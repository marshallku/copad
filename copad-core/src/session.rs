use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const SESSION_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Session {
    pub version: u32,
    pub tabs: Vec<TabSnap>,
    pub current_tab: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TabSnap {
    pub custom_title: Option<String>,
    pub root: SplitSnap,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SplitSnap {
    Terminal {
        cwd: Option<String>,
    },
    Branch {
        orientation: SplitOrientation,
        position: i32,
        first: Box<SplitSnap>,
        second: Box<SplitSnap>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SplitOrientation {
    Horizontal,
    Vertical,
}

pub fn session_path() -> PathBuf {
    crate::paths::state_dir().join("session.json")
}

pub fn load() -> Option<Session> {
    let path = session_path();
    let raw = std::fs::read_to_string(&path).ok()?;
    let session: Session = serde_json::from_str(&raw)
        .map_err(|e| eprintln!("[copad] session parse failed: {e}"))
        .ok()?;
    // Reject unknown versions outright — best-effort parsing of a future
    // schema risks producing a half-restored state worse than starting
    // fresh.
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
    Some(session)
}

/// Remove any persisted session file. Called when the closing window
/// has no terminal panels left so a stale snapshot doesn't restore on
/// next launch.
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
/// `Terminal` leaf. Used at restore-time: the cwd of the first leaf is
/// applied to the panel that seeds the tab; subsequent splits supply
/// their own leftmost-leaf cwd to each new panel.
pub fn leftmost_cwd(snap: &SplitSnap) -> Option<String> {
    match snap {
        SplitSnap::Terminal { cwd } => cwd.clone(),
        SplitSnap::Branch { first, .. } => leftmost_cwd(first),
    }
}

// ============================================================================
// v2 session model (decision #61) — native workspace-session → sub-tab → pane
// tree with typed leaves (terminal / webview / plugin). ADDITIVE alongside v1:
// the GUI still reads/writes the v1 `Session` until later slices wire v2 in, so
// existing consumers keep compiling. The session STRUCTURE is copad-native
// (it holds non-terminal panels tmux can't); tmux backs terminal leaves only.
// ============================================================================

/// On-disk version for the v2 model.
pub const SESSION_VERSION_V2: u32 = 2;

/// Top-level v2 document: many workspace sessions.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SessionFileV2 {
    pub version: u32,
    pub sessions: Vec<WorkspaceSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
}

/// A workspace-scoped session (the owner's "tab"): a workspace dir + ordered
/// sub-tabs. `workspace` is an absolute lexical path used as a DEFAULT context,
/// NOT a containment/security boundary (decision #61 C7).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WorkspaceSession {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub sub_tabs: Vec<SubTab>,
    /// Persisted by stable id, not a positional index (decision #61 I3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_sub_tab_id: Option<String>,
}

/// A sub-tab (opt+N navigation, tmux-window analog) holding a pane tree.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SubTab {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub root: PaneNode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane_id: Option<String>,
}

/// A pane tree: a leaf pane or a split of two subtrees. Splits are ALWAYS
/// copad-native (decision #61) — `ratio` is a normalized 0..1 divider position,
/// never platform pixels.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum PaneNode {
    Leaf(Pane),
    Branch {
        orientation: SplitOrientation,
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

/// A leaf pane: a stable id + typed content. The id is minted once by the
/// runtime BEFORE the PTY starts and persisted, so restore reattaches the
/// right process (decision #61 C2).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Pane {
    pub id: String,
    pub content: PaneContent,
}

/// Typed pane content. `nvim` and friends are terminal LAUNCH PROFILES, not
/// separate kinds (decision #61 I2). Non-terminal panes are copad-native.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneContent {
    Terminal {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch: Option<LaunchProfile>,
        /// Stable tmux session reference — the terminal leaf's persistent
        /// identity (decision #61). `None` until the leaf is tmux-backed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmux_ref: Option<String>,
    },
    Webview {
        /// An explicitly approved canonical URL only (decision #61 C6) — never
        /// blindly the live URL (creds / OAuth codes / signed URLs).
        url: String,
    },
    Plugin {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

/// What a terminal leaf runs.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchProfile {
    Shell,
    Nvim,
}

/// Divider-ratio clamp band so a persisted extreme / corrupt value can't
/// collapse a pane below usability (decision #61 I4).
pub const MIN_RATIO: f32 = 0.05;
pub const MAX_RATIO: f32 = 0.95;

pub fn clamp_ratio(r: f32) -> f32 {
    if r.is_finite() {
        r.clamp(MIN_RATIO, MAX_RATIO)
    } else {
        0.5
    }
}

/// Lightweight version peek so the loader dispatches on schema version before
/// committing to a concrete type (decision #61 C3). The v1 loader hard-rejected
/// any non-1 version, which would make a v2 file un-migratable.
#[derive(Deserialize)]
struct VersionEnvelope {
    version: u32,
}

/// Load the session file as the v2 model, migrating a v1 file forward. `None`
/// when absent, unparseable, or an unknown future version.
pub fn load_v2() -> Option<SessionFileV2> {
    let raw = std::fs::read_to_string(session_path()).ok()?;
    parse_v2(&raw)
}

/// Parse + version-dispatch a session document into the v2 model. Split from
/// `load_v2` so it is unit-testable without the filesystem.
pub fn parse_v2(raw: &str) -> Option<SessionFileV2> {
    let env: VersionEnvelope = serde_json::from_str(raw)
        .map_err(|e| eprintln!("[copad] session version peek failed: {e}"))
        .ok()?;
    match env.version {
        SESSION_VERSION_V2 => serde_json::from_str::<SessionFileV2>(raw)
            .map_err(|e| eprintln!("[copad] session v2 parse failed: {e}"))
            .ok()
            .map(|mut f| {
                f.normalize();
                f
            }),
        SESSION_VERSION => {
            let v1: Session = serde_json::from_str(raw)
                .map_err(|e| eprintln!("[copad] session v1 parse failed: {e}"))
                .ok()?;
            Some(migrate_v1_to_v2(&v1))
        }
        other => {
            eprintln!("[copad] session version {other} unknown — ignoring");
            None
        }
    }
}

/// Serialize a v2 document to pretty JSON. Split out so the save path and its
/// round-trip test share one encoder.
pub fn serialize_v2(file: &SessionFileV2) -> Option<String> {
    serde_json::to_string_pretty(file)
        .map_err(|e| eprintln!("[copad] session v2 serialize failed: {e}"))
        .ok()
}

/// Persist the v2 document to the session path via a temp-file + atomic rename
/// (same durability as v1 `save`). Debounce / single-writer coordination is the
/// CALLER's responsibility (decision #61 C4) — core only does the atomic write.
pub fn save_v2(file: &SessionFileV2) {
    let path = session_path();
    let Some(parent) = path.parent() else { return };
    if let Err(e) = std::fs::create_dir_all(parent) {
        eprintln!("[copad] session v2 save: mkdir {} failed: {e}", parent.display());
        return;
    }
    let Some(json) = serialize_v2(file) else { return };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, json) {
        eprintln!("[copad] session v2 write {} failed: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        eprintln!("[copad] session v2 rename failed: {e}");
        let _ = std::fs::remove_file(&tmp);
    }
}

impl SessionFileV2 {
    /// Clamp divider ratios across every tree so downstream layout never sees
    /// an out-of-band value (defense against hand-edited / corrupt files).
    pub fn normalize(&mut self) {
        fn walk(n: &mut PaneNode) {
            if let PaneNode::Branch {
                ratio, first, second, ..
            } = n
            {
                *ratio = clamp_ratio(*ratio);
                walk(first);
                walk(second);
            }
        }
        for s in &mut self.sessions {
            for st in &mut s.sub_tabs {
                walk(&mut st.root);
            }
        }
    }
}

/// Migrate a v1 snapshot to v2: all v1 tabs become sub-tabs of ONE default
/// workspace session (preserves the flat tab bar as opt+N sub-tabs). IDs are
/// deterministic so the migration is stable + golden-testable; the runtime
/// mints fresh ids for new sessions/panes. v1 pixel divider positions aren't
/// faithfully restorable today (a sentinel + re-equalize on macOS), so branches
/// migrate to an even 0.5 ratio — matching current behavior.
pub fn migrate_v1_to_v2(v1: &Session) -> SessionFileV2 {
    let sub_tabs: Vec<SubTab> = v1
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut n = 0usize;
            SubTab {
                id: format!("sub-{i}"),
                name: t.custom_title.clone(),
                root: migrate_split(&t.root, i, &mut n),
                focused_pane_id: None,
            }
        })
        .collect();
    let active_idx = v1.current_tab.min(sub_tabs.len().saturating_sub(1));
    let active_sub_tab_id = sub_tabs.get(active_idx).map(|s| s.id.clone());
    SessionFileV2 {
        version: SESSION_VERSION_V2,
        sessions: vec![WorkspaceSession {
            id: "sess-0".to_string(),
            name: None,
            workspace: None,
            sub_tabs,
            active_sub_tab_id,
        }],
        active_session_id: Some("sess-0".to_string()),
    }
}

fn migrate_split(snap: &SplitSnap, sub: usize, n: &mut usize) -> PaneNode {
    match snap {
        SplitSnap::Terminal { cwd } => {
            let id = format!("pane-{sub}-{n}");
            *n += 1;
            PaneNode::Leaf(Pane {
                id,
                content: PaneContent::Terminal {
                    cwd: cwd.clone(),
                    launch: None,
                    tmux_ref: None,
                },
            })
        }
        SplitSnap::Branch {
            orientation,
            first,
            second,
            ..
        } => PaneNode::Branch {
            orientation: *orientation,
            ratio: 0.5,
            first: Box::new(migrate_split(first, sub, n)),
            second: Box::new(migrate_split(second, sub, n)),
        },
    }
}

#[cfg(test)]
mod v2_tests {
    use super::*;

    #[test]
    fn v2_round_trips_mixed_tree() {
        let f = SessionFileV2 {
            version: SESSION_VERSION_V2,
            active_session_id: Some("sess-0".into()),
            sessions: vec![WorkspaceSession {
                id: "sess-0".into(),
                name: Some("copad".into()),
                workspace: Some("/Users/x/dev/copad".into()),
                active_sub_tab_id: Some("sub-0".into()),
                sub_tabs: vec![SubTab {
                    id: "sub-0".into(),
                    name: None,
                    focused_pane_id: Some("p1".into()),
                    root: PaneNode::Branch {
                        orientation: SplitOrientation::Horizontal,
                        ratio: 0.5,
                        first: Box::new(PaneNode::Leaf(Pane {
                            id: "p1".into(),
                            content: PaneContent::Terminal {
                                cwd: Some("/tmp".into()),
                                launch: Some(LaunchProfile::Nvim),
                                tmux_ref: Some("copad-p1".into()),
                            },
                        })),
                        second: Box::new(PaneNode::Leaf(Pane {
                            id: "p2".into(),
                            content: PaneContent::Webview {
                                url: "https://example.com".into(),
                            },
                        })),
                    },
                }],
            }],
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: SessionFileV2 = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn migrate_v1_tabs_become_sub_tabs_of_one_session() {
        let v1 = Session {
            version: SESSION_VERSION,
            current_tab: 1,
            tabs: vec![
                TabSnap {
                    custom_title: Some("a".into()),
                    root: SplitSnap::Terminal {
                        cwd: Some("/a".into()),
                    },
                },
                TabSnap {
                    custom_title: None,
                    root: SplitSnap::Branch {
                        orientation: SplitOrientation::Vertical,
                        position: 300,
                        first: Box::new(SplitSnap::Terminal {
                            cwd: Some("/b".into()),
                        }),
                        second: Box::new(SplitSnap::Terminal {
                            cwd: Some("/c".into()),
                        }),
                    },
                },
            ],
        };
        let v2 = migrate_v1_to_v2(&v1);
        assert_eq!(v2.version, SESSION_VERSION_V2);
        assert_eq!(v2.sessions.len(), 1, "all v1 tabs land in one session");
        let s = &v2.sessions[0];
        assert_eq!(s.sub_tabs.len(), 2, "one sub-tab per v1 tab");
        assert_eq!(s.active_sub_tab_id.as_deref(), Some("sub-1"));
        match &s.sub_tabs[0].root {
            PaneNode::Leaf(p) => match &p.content {
                PaneContent::Terminal { cwd, tmux_ref, .. } => {
                    assert_eq!(cwd.as_deref(), Some("/a"));
                    assert!(tmux_ref.is_none(), "migrated leaves aren't tmux-backed yet");
                }
                _ => panic!("expected terminal"),
            },
            _ => panic!("expected leaf"),
        }
        match &s.sub_tabs[1].root {
            PaneNode::Branch { ratio, .. } => assert_eq!(*ratio, 0.5),
            _ => panic!("expected branch"),
        }
    }

    #[test]
    fn parse_dispatches_on_version() {
        let v1_json = r#"{"version":1,"current_tab":0,"tabs":[{"custom_title":null,"root":{"type":"terminal","cwd":"/x"}}]}"#;
        let f = parse_v2(v1_json).expect("v1 doc migrates to v2");
        assert_eq!(f.version, SESSION_VERSION_V2);
        assert_eq!(f.sessions[0].sub_tabs.len(), 1);
        // Unknown future version is rejected, not half-parsed.
        assert!(parse_v2(r#"{"version":999,"sessions":[]}"#).is_none());
    }

    #[test]
    fn parse_clamps_out_of_band_ratio() {
        let doc = r#"{"version":2,"sessions":[{"id":"s","sub_tabs":[{"id":"t","root":{"node":"branch","orientation":"horizontal","ratio":9.0,"first":{"node":"leaf","id":"p1","content":{"kind":"terminal"}},"second":{"node":"leaf","id":"p2","content":{"kind":"terminal"}}}}]}]}"#;
        let f = parse_v2(doc).expect("v2 doc parses");
        match &f.sessions[0].sub_tabs[0].root {
            PaneNode::Branch { ratio, .. } => {
                assert!(*ratio <= MAX_RATIO && *ratio >= MIN_RATIO, "ratio clamped");
            }
            _ => panic!("expected branch"),
        }
    }

    #[test]
    fn serialized_v2_round_trips_through_the_version_loader() {
        // Closes the save<->load loop: what save_v2 writes must be exactly what
        // the version-dispatch loader reads back (version 2 branch of parse_v2).
        let f = SessionFileV2 {
            version: SESSION_VERSION_V2,
            active_session_id: Some("sess-0".into()),
            sessions: vec![WorkspaceSession {
                id: "sess-0".into(),
                name: Some("copad".into()),
                workspace: Some("/w".into()),
                active_sub_tab_id: Some("sub-0".into()),
                sub_tabs: vec![SubTab {
                    id: "sub-0".into(),
                    name: None,
                    focused_pane_id: Some("p1".into()),
                    root: PaneNode::Branch {
                        orientation: SplitOrientation::Vertical,
                        ratio: 0.5,
                        first: Box::new(PaneNode::Leaf(Pane {
                            id: "p1".into(),
                            content: PaneContent::Terminal {
                                cwd: None,
                                launch: Some(LaunchProfile::Shell),
                                tmux_ref: None,
                            },
                        })),
                        second: Box::new(PaneNode::Leaf(Pane {
                            id: "p2".into(),
                            content: PaneContent::Plugin {
                                name: "kb".into(),
                                version: Some("1".into()),
                            },
                        })),
                    },
                }],
            }],
        };
        let json = serialize_v2(&f).expect("serialize");
        let back = parse_v2(&json).expect("version loader reads our own output");
        assert_eq!(f, back);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cwd: &str) -> SplitSnap {
        SplitSnap::Terminal {
            cwd: Some(cwd.to_string()),
        }
    }

    fn branch(o: SplitOrientation, first: SplitSnap, second: SplitSnap) -> SplitSnap {
        SplitSnap::Branch {
            orientation: o,
            position: 400,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn round_trip_single_terminal() {
        let s = Session {
            version: SESSION_VERSION,
            tabs: vec![TabSnap {
                custom_title: None,
                root: term("/home/x"),
            }],
            current_tab: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn round_trip_nested_split_tree() {
        let s = Session {
            version: SESSION_VERSION,
            tabs: vec![
                TabSnap {
                    custom_title: Some("editor".to_string()),
                    root: branch(
                        SplitOrientation::Horizontal,
                        branch(SplitOrientation::Vertical, term("/a"), term("/b")),
                        term("/c"),
                    ),
                },
                TabSnap {
                    custom_title: None,
                    root: term("/d"),
                },
            ],
            current_tab: 1,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn leftmost_cwd_unwraps_nested_first() {
        let s = branch(
            SplitOrientation::Horizontal,
            branch(SplitOrientation::Vertical, term("/a"), term("/b")),
            term("/c"),
        );
        assert_eq!(leftmost_cwd(&s), Some("/a".to_string()));
    }

    #[test]
    fn leftmost_cwd_returns_none_for_unset_cwd() {
        let s = SplitSnap::Terminal { cwd: None };
        assert_eq!(leftmost_cwd(&s), None);
    }

    #[test]
    fn schema_rejects_unknown_version_on_load_helper() {
        // load() is filesystem-bound; we exercise the version-mismatch
        // branch through a direct deserialize + check, mirroring load().
        let json = r#"{"version":999,"tabs":[{"custom_title":null,"root":{"type":"terminal","cwd":"/x"}}],"current_tab":0}"#;
        let parsed: Session = serde_json::from_str(json).unwrap();
        assert_ne!(parsed.version, SESSION_VERSION);
    }
}
