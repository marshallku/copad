//! Session persistence: save the mux's session/tab/split-layout structure to disk and
//! restore it when the server next starts, so a reboot or crash brings your workspace
//! back (tmux-resurrect / continuum parity).
//!
//! Lifecycle (continuum-style, the reference the owner cited): a background thread
//! autosaves periodically; on server start, if a snapshot exists and `persist` is on, it
//! is restored. There is NO tombstone-on-teardown — the snapshot is the last-known layout
//! and is always restored (delete the file, or set `persist = false`, for a fresh start).
//! This keeps the design race-free (no delete competing with the writer).
//!
//! What is saved: session names + active flags, tab names + active flags, each tab's BSP
//! split tree (structure + ratios), and per-leaf working directory. NOT saved: running
//! programs (shells restart fresh in their cwd), scrollback, focus-within-tab.
//!
//! Durability: the writer runs OFF the render loop (a dedicated thread fed a coalescing
//! `SyncSender(1)`), writes a temp file, `fsync`s it, renames onto the target, then
//! `fsync`s the parent directory — so a completed save survives power loss, and a crash
//! mid-write leaves the previous good snapshot intact.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, TrySendError};

use serde::{Deserialize, Serialize};

use crate::model::Dir;

/// Bump when the on-disk schema changes incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

// ---- validation bounds (untrusted file: never build unbounded state from it) ----
/// Max split-tree depth restored from a snapshot (deeper subtrees are pruned).
pub const MAX_DEPTH: usize = 8;
/// Max leaves (panes) restored per tab.
pub const MAX_LEAVES_PER_TAB: usize = 32;
/// Max total panes restored across ALL sessions (caps PTY spawn + memory).
pub const MAX_TOTAL_PANES: usize = 100;
/// Reject a snapshot file larger than this BEFORE parsing, so a hostile/corrupt file can't
/// exhaust memory during deserialization (the per-tree caps only apply during restore). A
/// real layout — even 100 panes with long cwds — is a few KiB; 1 MiB is generous slack.
pub const MAX_FILE_BYTES: u64 = 1 << 20;
/// Defense-in-depth cap on session count after parse (the file-size cap already bounds this).
pub const MAX_SESSIONS: usize = 64;
/// Max length of a restored `command` injected into a shell (guards against a hostile file
/// injecting a huge/absurd command line).
pub const MAX_COMMAND_LEN: usize = 512;

/// The whole persisted mux state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistState {
    pub version: u32,
    /// Unix seconds when saved (informational).
    #[serde(default)]
    pub saved_at: u64,
    /// Index into `sessions` of the session that was active.
    #[serde(default)]
    pub active_session: usize,
    pub sessions: Vec<PSession>,
}

/// One persisted session (workspace).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PSession {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub active_tab: usize,
    pub tabs: Vec<PTab>,
}

/// One persisted tab (a BSP layout).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PTab {
    #[serde(default)]
    pub name: Option<String>,
    pub layout: PLayout,
}

/// A persisted split tree: a leaf (with its cwd) or a branch (dir + ratio + children).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PLayout {
    Leaf {
        /// Working directory to restore the shell in (`None`/missing → `$HOME`). Stored
        /// as UTF-8; a non-UTF-8 path is saved as `None` for that pane only.
        #[serde(default)]
        cwd: Option<String>,
        /// The argv of a whitelisted program (agent) that was running in this pane, to
        /// RE-RUN on restore (shell-quoted + injected into the fresh shell). Structural so
        /// argument boundaries survive. `None`/missing = restore a bare shell.
        #[serde(default)]
        command: Option<Vec<String>>,
    },
    Branch {
        dir: Dir,
        ratio: f32,
        first: Box<PLayout>,
        second: Box<PLayout>,
    },
}

impl PLayout {
    /// Count leaves (panes) in this subtree.
    pub fn leaf_count(&self) -> usize {
        match self {
            PLayout::Leaf { .. } => 1,
            PLayout::Branch { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }
}

/// The state file path: `$COPAD_MUX_STATE` if set (tests / multi-instance so they don't
/// clobber a shared file), else `$XDG_STATE_HOME/copad/mux-session.json`, else
/// `$HOME/.local/state/copad/mux-session.json`.
pub fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("COPAD_MUX_STATE")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(|| PathBuf::from(".local/state"));
    base.join("copad").join("mux-session.json")
}

/// Load + parse the snapshot, or `None` if it is missing, unreadable, malformed, or a
/// newer schema — a bad file must NEVER block startup.
pub fn load() -> Option<PersistState> {
    load_from(&state_path())
}

pub fn load_from(path: &std::path::Path) -> Option<PersistState> {
    // Bound BEFORE reading/parsing: an oversized (hostile/corrupt) file must fall back to a
    // fresh session, not exhaust memory during deserialization.
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        eprintln!(
            "comux persist: {} exceeds {MAX_FILE_BYTES} bytes — ignoring",
            path.display()
        );
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    let state: PersistState = serde_json::from_str(&contents).ok()?;
    if state.version != SCHEMA_VERSION || state.sessions.len() > MAX_SESSIONS {
        return None;
    }
    Some(state)
}

/// A handle to the background save thread. Dropping it closes the queue and JOINS the
/// writer, so a queued/in-flight save completes before the server exits (a clean shutdown
/// doesn't lose the last snapshot). A crash still can't corrupt: `write_durably` is atomic.
pub struct Saver {
    tx: Option<SyncSender<PersistState>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Saver {
    /// Spawn the writer thread for `path`.
    pub fn new(path: PathBuf) -> Self {
        // Capacity 1 = coalesce: if a write is in flight, a newer request is dropped and
        // picked up by the next periodic autosave (we only care about the latest state).
        let (tx, rx) = std::sync::mpsc::sync_channel::<PersistState>(1);
        let handle = std::thread::spawn(move || {
            let mut warned = false;
            for state in rx {
                if let Err(e) = write_durably(&path, &state)
                    && !warned
                {
                    // Surface the first failure so persistence isn't silently broken.
                    eprintln!("comux persist: save to {} failed: {e}", path.display());
                    warned = true;
                }
            }
        });
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    /// Queue a snapshot to be written (non-blocking; coalesces under backpressure).
    pub fn request(&self, state: PersistState) {
        if let Some(tx) = &self.tx {
            match tx.try_send(state) {
                Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        // Close the channel FIRST (drop the sender) so the writer's `for state in rx` loop
        // drains its last item and ends, then join it — the final autosave lands on disk.
        self.tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Synchronously + durably write `state` to `path`, blocking until it lands. Used for the
/// final save on explicit `kill-server` (where the coalescing async queue could drop the
/// latest snapshot behind an in-flight one) — this guarantees the LATEST layout is durable.
pub fn save_blocking(path: &std::path::Path, state: &PersistState) -> std::io::Result<()> {
    write_durably(path, state)
}

/// Durably write `state` to `path`: temp file → `fsync` → rename → `fsync` parent dir.
fn write_durably(path: &std::path::Path, state: &PersistState) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("state path has no parent dir"))?;
    std::fs::create_dir_all(parent)?;
    let json = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;

    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(&json)?;
        f.sync_all()?; // the bytes are on disk before the rename
    }
    std::fs::rename(&tmp, path)?;
    // fsync the directory so the rename itself survives power loss. Propagate a failure
    // here — otherwise we'd report a durable save that the rename might not survive.
    let dir = std::fs::File::open(parent)?;
    dir.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Dir;

    fn sample() -> PersistState {
        PersistState {
            version: SCHEMA_VERSION,
            saved_at: 12345,
            active_session: 1,
            sessions: vec![
                PSession {
                    name: Some("local".into()),
                    active_tab: 0,
                    tabs: vec![PTab {
                        name: None,
                        layout: PLayout::Branch {
                            dir: Dir::Right,
                            ratio: 0.4,
                            first: Box::new(PLayout::Leaf {
                                cwd: Some("/tmp".into()),
                                command: Some(vec!["claude".into(), "--resume".into()]),
                            }),
                            second: Box::new(PLayout::Leaf {
                                cwd: None,
                                command: None,
                            }),
                        },
                    }],
                },
                PSession {
                    name: Some("api".into()),
                    active_tab: 1,
                    tabs: vec![
                        PTab {
                            name: None,
                            layout: PLayout::Leaf {
                                cwd: None,
                                command: None,
                            },
                        },
                        PTab {
                            name: Some("logs".into()),
                            layout: PLayout::Leaf {
                                cwd: Some("/var".into()),
                                command: None,
                            },
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn round_trips_through_json() {
        let s = sample();
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: PersistState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn leaf_count_matches_tree() {
        assert_eq!(sample().sessions[0].tabs[0].layout.leaf_count(), 2);
        assert_eq!(sample().sessions[1].tabs[0].layout.leaf_count(), 1);
    }

    #[test]
    fn load_from_missing_or_garbage_is_none() {
        assert!(load_from(std::path::Path::new("/nonexistent/copad/mux-session.json")).is_none());
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "copad-mux-test-garbage-{}.json",
            std::process::id()
        ));
        std::fs::write(&p, b"not json at all {{{").unwrap();
        assert!(load_from(&p).is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wrong_schema_version_is_rejected() {
        let mut s = sample();
        s.version = SCHEMA_VERSION + 1;
        let p =
            std::env::temp_dir().join(format!("copad-mux-test-ver-{}.json", std::process::id()));
        std::fs::write(&p, serde_json::to_string(&s).unwrap()).unwrap();
        assert!(load_from(&p).is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn oversized_file_is_rejected_before_parsing() {
        let p =
            std::env::temp_dir().join(format!("copad-mux-test-big-{}.json", std::process::id()));
        // Valid-ish JSON prefix but way over the size cap → must be ignored, not parsed.
        let big = vec![b' '; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(&p, big).unwrap();
        assert!(load_from(&p).is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn write_durably_then_load_round_trips() {
        let p = std::env::temp_dir().join(format!(
            "copad-mux-test-durable-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        write_durably(&p, &sample()).unwrap();
        assert_eq!(load_from(&p), Some(sample()));
        let _ = std::fs::remove_file(&p);
    }
}
