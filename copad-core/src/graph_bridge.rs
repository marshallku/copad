//! The comux work-graph page's messages, validated (decisions #204) — the Linux twin of
//! `copad-macos/Sources/CopadCore/ComuxGraphBridge.swift`; keep the two in step.
//!
//! The page draws another program's output (paths, prompts), so everything it posts is checked
//! for shape and bounds before copad acts on it, and a message stamped for a connection the
//! window has since left is dropped: a queued `open` from a remote comux must never run after
//! the window switched to a local one.

use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphMsg {
    Ready,
    Jump {
        token: String,
    },
    Open {
        path: String,
    },
    History {
        token: String,
        before: Option<u64>,
        limit: Option<usize>,
    },
    Mode(String),
    Agent {
        token: String,
    },
}

/// `body` as the page posted it; `None` when it is not one of ours, is malformed, or carries
/// another connection's `epoch`. `ready` is accepted whatever its epoch (it can predate
/// `setHost`).
pub fn parse(body: &Value, epoch: &str) -> Option<GraphMsg> {
    let kind = body.get("type")?.as_str()?;
    if kind == "ready" {
        return Some(GraphMsg::Ready);
    }
    if epoch.is_empty() || body.get("epoch")?.as_str()? != epoch {
        return None;
    }
    match kind {
        "jump" => token(body.get("token")).map(|token| GraphMsg::Jump { token }),
        "agent" => token(body.get("token")).map(|token| GraphMsg::Agent { token }),
        "mode" => match body.get("mode")?.as_str()? {
            m @ ("agent" | "all") => Some(GraphMsg::Mode(m.to_string())),
            _ => None,
        },
        "open" => {
            let p = body.get("path")?.as_str()?;
            (p.starts_with('/') && p.len() <= 4096 && !p.chars().any(char::is_control)).then(|| {
                GraphMsg::Open {
                    path: p.to_string(),
                }
            })
        }
        "history" => Some(GraphMsg::History {
            token: token(body.get("token"))?,
            before: number(body.get("before"), 1_000_000_000_000)?,
            limit: number(body.get("limit"), 1000)?.map(|l| l as usize),
        }),
        _ => None,
    }
}

fn token(v: Option<&Value>) -> Option<String> {
    let t = v?.as_str()?;
    ((1..=128).contains(&t.len()) && t.bytes().all(|b| b > 0x20 && b < 0x7f)).then(|| t.to_string())
}

fn number(v: Option<&Value>, max: u64) -> Option<Option<u64>> {
    match v {
        None | Some(Value::Null) => Some(None),
        Some(Value::Number(n)) => {
            let f = n.as_f64()?;
            (f >= 0.0 && f.fract() == 0.0 && f <= max as f64).then_some(Some(f as u64))
        }
        _ => None,
    }
}

/// The file to hand to the desktop for an `open`: the path must be absolute and resolve to a
/// regular file (a symlink is followed, and its TARGET must be regular).
pub fn openable_file(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return None;
    }
    let real = std::fs::canonicalize(p).ok()?;
    std::fs::metadata(&real).ok()?.is_file().then_some(real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_parse_only_for_the_current_connection() {
        assert_eq!(
            parse(&json!({"type": "ready"}), "k#1"),
            Some(GraphMsg::Ready)
        );
        assert_eq!(
            parse(
                &json!({"type": "jump", "token": "a1-2", "epoch": "k#1"}),
                "k#1"
            ),
            Some(GraphMsg::Jump {
                token: "a1-2".into()
            })
        );
        assert_eq!(
            parse(
                &json!({"type": "jump", "token": "a1-2", "epoch": "k#0"}),
                "k#1"
            ),
            None
        );
        assert_eq!(
            parse(&json!({"type": "jump", "token": "a1-2"}), "k#1"),
            None
        );
        assert_eq!(parse(&json!("jump"), "k#1"), None);
    }

    #[test]
    fn fields_are_bounded() {
        let e = "k#1";
        let p = |v: Value| parse(&v, e);
        assert_eq!(
            p(json!({"type": "jump", "token": "bad token", "epoch": e})),
            None
        );
        assert_eq!(
            p(json!({"type": "history", "token": "t", "before": null, "epoch": e})),
            Some(GraphMsg::History {
                token: "t".into(),
                before: None,
                limit: None
            })
        );
        assert_eq!(
            p(json!({"type": "history", "token": "t", "before": 12, "limit": 1, "epoch": e})),
            Some(GraphMsg::History {
                token: "t".into(),
                before: Some(12),
                limit: Some(1)
            })
        );
        assert_eq!(
            p(json!({"type": "history", "token": "t", "before": -1, "epoch": e})),
            None
        );
        assert_eq!(
            p(json!({"type": "history", "token": "t", "limit": 5000, "epoch": e})),
            None
        );
        assert_eq!(
            p(json!({"type": "history", "token": "t", "before": 1.5, "epoch": e})),
            None
        );
        assert_eq!(
            p(json!({"type": "history", "token": "t", "before": true, "epoch": e})),
            None
        );
        assert_eq!(
            p(json!({"type": "open", "path": "relative", "epoch": e})),
            None
        );
        assert_eq!(
            p(json!({"type": "open", "path": "/a\nb", "epoch": e})),
            None
        );
        assert_eq!(
            p(json!({"type": "mode", "mode": "all", "epoch": e})),
            Some(GraphMsg::Mode("all".into()))
        );
        assert_eq!(p(json!({"type": "mode", "mode": "x", "epoch": e})), None);
    }

    #[cfg(unix)]
    #[test]
    fn only_a_regular_file_opens() {
        let dir = std::env::temp_dir().join(format!("graph-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        std::os::unix::fs::symlink(&file, dir.join("link")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("dirlink")).unwrap();
        let s = |p: &Path| p.to_str().unwrap().to_string();
        assert!(openable_file(&s(&file)).is_some());
        assert!(openable_file(&s(&dir.join("link"))).is_some());
        assert!(openable_file(&s(&dir)).is_none());
        assert!(openable_file(&s(&dir.join("dirlink"))).is_none());
        assert!(openable_file(&s(&dir.join("missing"))).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
