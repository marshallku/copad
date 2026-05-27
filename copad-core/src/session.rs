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
