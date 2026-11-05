//! Session persistence: save the mux's session/tab/split-layout structure to disk and
//! restore it when the server next starts.
//!
//! The snapshot is the last-known layout and is always restored when `persist` is on (delete
//! the file, or set `persist = false`, for a fresh start). There is no tombstone on teardown,
//! so no delete competes with the writer.
//!
//! Saved: session names + active flags, tab names + active flags, each tab's BSP split tree
//! (structure + ratios), per-leaf working directory. Not saved: scrollback, focus-within-tab.
//!
//! The writer runs OFF the render loop (a dedicated thread fed a coalescing `SyncSender(1)`),
//! writes a temp file, `fsync`s it, renames onto the target, then `fsync`s the parent directory:
//! a completed save survives power loss and a crash mid-write leaves the previous snapshot.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::model::Dir;

pub const SCHEMA_VERSION: u32 = 1;

// Validation bounds: the file is untrusted, never build unbounded state from it.
pub const MAX_DEPTH: usize = 8;
pub const MAX_LEAVES_PER_TAB: usize = 32;
pub const MAX_TOTAL_PANES: usize = 100;
/// Reject a snapshot file larger than this BEFORE parsing, so a hostile or corrupt file can't
/// exhaust memory during deserialization (the per-tree caps only apply during restore).
pub const MAX_FILE_BYTES: u64 = 1 << 20;
/// Defense-in-depth session cap after parse (the file-size cap already bounds this).
pub const MAX_SESSIONS: usize = 64;
/// Max length of a restored `command` injected into a shell (guards against a hostile file).
pub const MAX_COMMAND_LEN: usize = 512;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistState {
    pub version: u32,
    #[serde(default)]
    pub saved_at: u64,
    #[serde(default)]
    pub active_session: usize,
    pub sessions: Vec<PSession>,
    /// Pinned pane slots (`Ctrl-b m`), by the leaf `pane_ref` they point at (decisions #148).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<PMark>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PMark {
    pub slot: u8,
    pub pane_ref: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PSession {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub active_tab: usize,
    pub tabs: Vec<PTab>,
    /// The session's start directory ([`crate::model::Workspace::dir`]). Missing in older
    /// snapshots — restore then falls back to the first pane's cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PTab {
    #[serde(default)]
    pub name: Option<String>,
    pub layout: PLayout,
    /// The `pane_ref` of the leaf (in any tab of any session) whose pane owns this tab.
    /// Pane tokens do not survive a restart, so the relation is saved by position and
    /// re-attached to whatever pane that leaf restores as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<u32>,
}

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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_ref: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Branch {
        dir: Dir,
        ratio: f32,
        first: Box<PLayout>,
        second: Box<PLayout>,
    },
}

impl PLayout {
    /// The cwd of the leftmost leaf: a pre-`dir` snapshot's best guess at where its session
    /// started.
    pub fn first_cwd(&self) -> Option<&str> {
        match self {
            PLayout::Leaf { cwd, .. } => cwd.as_deref(),
            PLayout::Branch { first, .. } => first.first_cwd(),
        }
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            PLayout::Leaf { .. } => 1,
            PLayout::Branch { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }
}

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

/// Load and parse the snapshot, or `None` if missing, unreadable, malformed or a newer schema:
/// a bad file must NEVER block startup.
pub fn load() -> Option<PersistState> {
    load_from(&state_path())
}

pub fn load_from(path: &std::path::Path) -> Option<PersistState> {
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

/// Handle to the background save thread. Dropping it closes the queue and JOINS the writer,
/// so a queued save completes before exit; `write_durably` is atomic, so a crash can't corrupt.
pub struct Saver {
    tx: Option<SyncSender<PersistState>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Saver {
    pub fn new(path: PathBuf) -> Self {
        // Capacity 1 coalesces: a request arriving during an in-flight write is dropped and
        // the next periodic autosave picks up the latest state.
        let (tx, rx) = std::sync::mpsc::sync_channel::<PersistState>(1);
        let handle = std::thread::spawn(move || {
            let mut warned = false;
            // The last successful write minus its timestamp: an idle server's layout is
            // unchanged every 15 s and each write is two fsyncs, so skip writes that would
            // change only `saved_at` (unless the file has gone missing).
            let mut last: Option<Vec<u8>> = None;
            for state in rx {
                let res = serialize(&PersistState {
                    saved_at: 0,
                    ..state.clone()
                })
                .and_then(|key| {
                    if last.as_deref() == Some(key.as_slice()) && path.exists() {
                        return Ok(());
                    }
                    write_durably(&path, &state)?;
                    last = Some(key);
                    Ok(())
                });
                if let Err(e) = res
                    && !warned
                {
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

    pub fn request(&self, state: PersistState) {
        if let Some(tx) = &self.tx {
            match tx.try_send(state) {
                Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }

    /// Shutdown save: push `final_state` as the writer's LAST item, then close and join, all
    /// within `timeout`. The same single writer performs every rename in order, so the newest
    /// snapshot is the last write on disk. On timeout (hung/networked filesystem) it detaches
    /// and the last autosave remains.
    pub fn finish(mut self, final_state: PersistState, timeout: Duration) {
        let start = Instant::now();
        if let Some(tx) = self.tx.as_ref() {
            let mut pending = Some(final_state);
            while let Some(s) = pending.take() {
                match tx.try_send(s) {
                    Ok(()) => break,
                    Err(TrySendError::Full(s)) => {
                        if start.elapsed() >= timeout {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                        pending = Some(s);
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
        }
        self.tx.take();
        if let Some(h) = self.handle.take() {
            while !h.is_finished() && start.elapsed() < timeout {
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        // Close the channel FIRST so the writer drains its last item and ends, then
        // BOUNDED-join: a writer wedged in `fsync` (slow/networked FS) must NOT hang teardown.
        self.tx.take();
        if let Some(h) = self.handle.take() {
            for _ in 0..300 {
                if h.is_finished() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

/// Synchronously and durably write `state`. Used for the final save on `kill-server`, where the
/// coalescing async queue could drop the latest snapshot behind an in-flight one.
pub fn save_blocking(path: &std::path::Path, state: &PersistState) -> std::io::Result<()> {
    write_durably(path, state)
}

/// Monotonic sequence for UNIQUE temp file names, so concurrent writers in one process (the
/// autosave thread and a bounded final save) never share a temp path.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Durably write `state` to `path`: temp file, `fsync`, rename, `fsync` parent dir. The temp
/// file is UNIQUE per write (`...json.<pid>.<seq>.tmp`) and removed on any error.
fn write_durably(path: &std::path::Path, state: &PersistState) -> std::io::Result<()> {
    write_bytes_durably(path, &serialize(state)?)
}

fn serialize(state: &PersistState) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(state).map_err(std::io::Error::other)
}

fn write_bytes_durably(path: &std::path::Path, json: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("state path has no parent dir"))?;
    std::fs::create_dir_all(parent)?;

    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}.{seq}.tmp", std::process::id()));
    let res = (|| -> std::io::Result<()> {
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
        f.write_all(json)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        // Propagate a failure: otherwise we'd report a durable save the rename might not survive.
        let dir = std::fs::File::open(parent)?;
        dir.sync_all()?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Dir;

    fn sample() -> PersistState {
        PersistState {
            marks: Vec::new(),
            version: SCHEMA_VERSION,
            saved_at: 12345,
            active_session: 1,
            sessions: vec![
                PSession {
                    name: Some("local".into()),
                    active_tab: 0,
                    dir: Some("/srv/local".into()),
                    tabs: vec![PTab {
                        name: None,
                        layout: PLayout::Branch {
                            dir: Dir::Right,
                            ratio: 0.4,
                            first: Box::new(PLayout::Leaf {
                                cwd: Some("/tmp".into()),
                                command: Some(vec!["claude".into(), "--resume".into()]),
                                pane_ref: Some(0),
                                name: None,
                            }),
                            second: Box::new(PLayout::Leaf {
                                cwd: None,
                                command: None,
                                pane_ref: None,
                                name: None,
                            }),
                        },
                        owner: None,
                    }],
                },
                PSession {
                    name: Some("api".into()),
                    active_tab: 1,
                    dir: None,
                    tabs: vec![
                        PTab {
                            name: None,
                            layout: PLayout::Leaf {
                                cwd: None,
                                command: None,
                                pane_ref: None,
                                name: None,
                            },
                            owner: None,
                        },
                        PTab {
                            name: Some("logs".into()),
                            layout: PLayout::Leaf {
                                cwd: Some("/var".into()),
                                command: None,
                                pane_ref: None,
                                name: None,
                            },
                            owner: Some(0),
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

    #[test]
    fn an_unchanged_autosave_does_not_rewrite_the_file() {
        let dir = std::env::temp_dir().join(format!("copad-mux-test-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("state.json");
        let saver = Saver::new(p.clone());
        let settle = || std::thread::sleep(Duration::from_millis(200));
        let inode = |p: &std::path::Path| {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(p).map(|m| m.ino()).ok()
        };
        saver.request(sample());
        settle();
        let first = inode(&p).expect("first save lands");
        saver.request(PersistState {
            saved_at: sample().saved_at + 15,
            ..sample()
        });
        settle();
        assert_eq!(inode(&p), Some(first));
        std::fs::remove_file(&p).unwrap();
        saver.request(sample());
        settle();
        assert_eq!(load_from(&p), Some(sample()));
        let before = inode(&p);
        let mut changed = sample();
        changed.active_session += 1;
        saver.request(changed.clone());
        settle();
        assert_ne!(inode(&p), before);
        assert_eq!(load_from(&p), Some(changed));
        drop(saver);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
