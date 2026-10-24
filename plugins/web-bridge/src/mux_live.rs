//! A live L0 subscription to the local comux server (decisions #166) — the board's primary
//! data source.
//!
//! Before this, every `/api/board` poll forked `comux list-sessions` and `comux list-agents`
//! (see [`crate::comux`]). The subscription keeps comux's own topic values in memory instead,
//! so a poll is a read of the latest snapshot: no child, no socket round trip, and the values
//! are the ones comux PUBLISHED rather than two samples taken at different instants.
//!
//! The CLI path stays as the fallback: while the link is down (no server yet, an older comux
//! that does not speak L0, a restart in progress) the board reads exactly as it did before.
//! The link reconnects on its own with a capped backoff.

use copad_mux_proto::link::{Endpoint, Link, LinkEvent};
use copad_mux_proto::{Applied, Model, PubMsg};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the board can read without touching comux.
#[derive(Default)]
pub struct Live {
    pub model: Model,
    /// The link is up AND a snapshot has arrived since the last welcome.
    pub synced: bool,
}

pub type Shared = Arc<Mutex<Live>>;

/// Start the background subscriber. Never returns an error: a missing server is the normal
/// state of a freshly booted machine, and the board falls back to the CLI meanwhile.
pub fn start() -> Shared {
    let shared: Shared = Arc::default();
    let s = shared.clone();
    std::thread::spawn(move || run(s));
    shared
}

fn run(shared: Shared) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let (tx, rx) = std::sync::mpsc::channel();
        let sh = shared.clone();
        let opened = Link::open(&Endpoint::local(), move |ev| {
            let closed = matches!(ev, LinkEvent::Closed(_));
            if let Ok(mut live) = sh.lock() {
                match &ev {
                    LinkEvent::Msg(m) => {
                        if live.model.apply(m) == Applied::Welcome {
                            live.synced = false;
                        }
                        if matches!(m, PubMsg::Snapshot { .. }) {
                            live.synced = true;
                        }
                    }
                    LinkEvent::Closed(_) => live.synced = false,
                }
            }
            if closed {
                let _ = tx.send(());
            }
        });
        match opened {
            Ok(link) => {
                backoff = Duration::from_secs(1);
                let _ = rx.recv(); // until the stream closes
                drop(link);
            }
            Err(_) => backoff = (backoff * 2).min(Duration::from_secs(30)),
        }
        std::thread::sleep(backoff);
    }
}

/// The board's `sessions` + `agents` arrays in the shape `/api/board` has always returned,
/// built from the live model — `None` when the link is not synced (use the CLI instead).
pub fn board(shared: &Shared) -> Option<(Value, Value)> {
    let live = shared.lock().ok()?;
    if !live.synced {
        return None;
    }
    let tree = live.model.tree.as_ref()?;
    let agents = live.model.agents.as_ref()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let sessions: Vec<Value> = tree
        .sessions
        .iter()
        .map(|s| {
            let panes: usize = s.tabs.iter().map(|t| t.panes.len()).sum();
            let n_agents = agents.agents.iter().filter(|a| a.space_id == s.id).count();
            json!({
                "index": s.index,
                "id": s.id,
                "name": s.name,
                "active": s.active,
                "tabs": s.tabs.len(),
                "panes": panes,
                "agents": n_agents,
            })
        })
        .collect();
    let agents: Vec<Value> = agents
        .agents
        .iter()
        .map(|a| {
            json!({
                "token": a.token,
                "terminal": "",
                "space": a.space,
                "space_id": a.space_id,
                "title": a.title,
                "tool": a.tool,
                "status": a.status,
                "for_secs": (now - a.since_unix).max(0),
                "detail": a.detail,
            })
        })
        .collect();
    Some((json!(sessions), json!(agents)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copad_mux_proto::{Agent, Agents, Pane, Session, Tab, TopicValue, Tree};
    use std::collections::BTreeMap;

    fn synced() -> Shared {
        let shared: Shared = Arc::default();
        let tree = Tree {
            active: "s1".into(),
            sessions: vec![Session {
                id: "s1".into(),
                name: "work".into(),
                active: true,
                tabs: vec![Tab {
                    id: "s1:t0".into(),
                    panes: vec![Pane::default(), Pane::default()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let agents = Agents {
            agents: vec![Agent {
                token: "tok".into(),
                space_id: "s1".into(),
                status: "blocked".into(),
                since_unix: 0,
                ..Default::default()
            }],
        };
        let mut topics = BTreeMap::new();
        topics.insert(
            "tree".to_string(),
            TopicValue {
                rev: 1,
                data: serde_json::to_value(tree).unwrap(),
            },
        );
        topics.insert(
            "agents".to_string(),
            TopicValue {
                rev: 2,
                data: serde_json::to_value(agents).unwrap(),
            },
        );
        {
            let mut l = shared.lock().unwrap();
            l.model.apply(&PubMsg::Snapshot { rev: 2, topics });
            l.synced = true;
        }
        shared
    }

    #[test]
    fn the_live_board_keeps_the_cli_shape() {
        let (sessions, agents) = board(&synced()).unwrap();
        assert_eq!(sessions[0]["name"], "work");
        assert_eq!(sessions[0]["panes"], 2);
        assert_eq!(sessions[0]["agents"], 1);
        assert_eq!(agents[0]["token"], "tok");
        assert!(agents[0]["for_secs"].as_i64().unwrap() > 0);
    }

    #[test]
    fn an_unsynced_link_defers_to_the_cli() {
        let s = synced();
        s.lock().unwrap().synced = false;
        assert!(board(&s).is_none());
    }
}
