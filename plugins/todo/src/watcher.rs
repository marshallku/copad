//! Polling file-watcher that emits `todo.created` / `todo.changed`
//! / `todo.completed` / `todo.deleted` events.
//!
//! Why polling, not `notify`/inotify, for v1: the calendar plugin
//! already established the polling pattern in this crate family,
//! so reusing it keeps the dep graph small and the failure modes
//! familiar. The visible cost is a 1-2s detection latency on
//! external (vim) edits — fine for a workflow tool. If a future
//! iteration shows the latency hurts the loop ("clicked Done in
//! vim, panel still says Open"), swap in `notify` without
//! changing the event surface.
//!
//! Algorithm:
//! 1. Wait for `initialized`.
//! 2. Snapshot the store: `{(workspace, id) → (mtime, status)}`.
//!    The first tick is treated as the baseline — we don't emit
//!    `todo.created` for everything that already exists at startup
//!    (that would spam triggers on every copad restart).
//! 3. Each subsequent tick re-scans and diffs:
//!    - new key → `todo.created`
//!    - same key, mtime changed → `todo.changed` (+ `todo.completed`
//!      if status transitioned `* → done`)
//!    - missing key → `todo.deleted`
//! 4. All payloads are the full `Todo` json (`Todo::to_json`) so
//!    consumers don't need to re-read the file.
//!
//! Note on completion semantics: a brand-new todo created with
//! `status: done` (rare but possible — a user logging an
//! already-completed task) emits `todo.created` only, NOT
//! `todo.completed`. The `completed` event is for transitions
//! observed after birth, which is what triggers conditioned on
//! "task just got finished" actually want.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Config;
use crate::store::{Store, validate_id};
use crate::todo::{self, Status};
use crate::{Writer, emit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Snapshot {
    /// ns since Unix epoch (FS resolution is typically ms/us, but ns
    /// resolves rapid double-writes that share a second).
    mtime_ns: i128,
    status: Status,
}

pub struct Watcher {
    config: Arc<Config>,
    store: Arc<Store>,
    writer: Writer,
    initialized: Arc<AtomicBool>,
    /// Set from main on stdin EOF / `shutdown`. Polled between scans
    /// and during sleep so the loop exits promptly, drops its `writer`
    /// clone, and lets the writer thread drain cleanly.
    shutdown: Arc<AtomicBool>,
}

impl Watcher {
    pub fn new(
        config: Arc<Config>,
        store: Arc<Store>,
        writer: Writer,
        initialized: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            config,
            store,
            writer,
            initialized,
            shutdown,
        }
    }

    pub fn run(&self) {
        while !self.initialized.load(Ordering::SeqCst) {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if self.config.fatal_error.is_some() {
            // Config is broken; don't pretend to watch. Actions
            // surface the error directly.
            eprintln!("[todo] watcher idle: config has fatal_error");
            return;
        }
        let mut prev = match self.scan() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[todo] initial scan failed: {e}");
                HashMap::new()
            }
        };
        while !self.shutdown.load(Ordering::SeqCst) {
            // Sleep in 100ms chunks so a shutdown signal mid-poll
            // is noticed within ~100ms instead of waiting for the
            // full poll_interval (default 2s). Without this,
            // graceful shutdown would block the writer drain for
            // up to one full poll cycle.
            let mut elapsed = Duration::ZERO;
            while elapsed < self.config.poll_interval {
                if self.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                let chunk = Duration::from_millis(100).min(self.config.poll_interval - elapsed);
                thread::sleep(chunk);
                elapsed += chunk;
            }
            let next = match self.scan() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[todo] scan failed: {e}");
                    continue;
                }
            };
            self.diff_and_emit(&prev, &next);
            prev = next;
        }
    }

    /// Emit events for every difference between `prev` and `next`.
    fn diff_and_emit(
        &self,
        prev: &HashMap<(String, String), Snapshot>,
        next: &HashMap<(String, String), Snapshot>,
    ) {
        for (key, snap) in next {
            match prev.get(key) {
                None => {
                    self.publish("todo.created", &key.0, &key.1);
                }
                Some(old) if old.mtime_ns != snap.mtime_ns => {
                    self.publish("todo.changed", &key.0, &key.1);
                    if old.status != Status::Done && snap.status == Status::Done {
                        self.publish("todo.completed", &key.0, &key.1);
                    }
                }
                _ => {}
            }
        }
        for key in prev.keys() {
            if !next.contains_key(key) {
                // Deleted. The file is gone, so we can't read it
                // for a fresh payload — emit a minimal payload
                // with just the keys consumers need.
                self.publish_payload(
                    "todo.deleted",
                    json!({
                        "id": key.1,
                        "workspace": key.0,
                    }),
                );
            }
        }
    }

    fn publish(&self, kind: &str, workspace: &str, id: &str) {
        let payload = match self.store.read(workspace, id) {
            Ok(t) => t.to_json(),
            Err(e) => {
                let (code, msg) = e.code_message();
                eprintln!(
                    "[todo] {kind} payload re-read failed for {workspace}/{id}: {code}/{msg}"
                );
                json!({ "id": id, "workspace": workspace })
            }
        };
        self.publish_payload(kind, payload);
    }

    fn publish_payload(&self, kind: &str, payload: Value) {
        let frame = json!({
            "method": "event.publish",
            "params": {
                "kind": kind,
                "payload": payload,
            }
        });
        emit(&self.writer, &frame.to_string());
    }

    fn scan(&self) -> Result<HashMap<(String, String), Snapshot>, String> {
        scan_root(self.store.root())
    }
}

fn scan_root(root: &std::path::Path) -> Result<HashMap<(String, String), Snapshot>, String> {
    let mut out = HashMap::new();
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(format!("readdir {}: {e}", root.display())),
    };
    for ws_entry in entries.flatten() {
        // file_type() is lstat-based (symlink-aware). metadata()
        // would follow the symlink and let a `~/docs/todos/leak`
        // → `/etc` redirect surface as legitimate todos. Same
        // posture as Store::list_all.
        let ws_ft = match ws_entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ws_ft.is_symlink() || !ws_ft.is_dir() {
            continue;
        }
        let Some(ws_name) = ws_entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if crate::config::validate_workspace(&ws_name).is_err() {
            continue;
        }
        let ws_path: PathBuf = ws_entry.path();
        let files = match fs::read_dir(&ws_path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for f in files.flatten() {
            let ft = match f.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() || !ft.is_file() {
                continue;
            }
            let path = f.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            let Some(id) = name.strip_suffix(".md") else {
                continue;
            };
            if validate_id(id).is_err() {
                continue;
            }
            // Open with O_NOFOLLOW so a swap to a symlink between
            // file_type() above and the read here can't redirect
            // us out (TOCTOU). Status comes from frontmatter
            // parse; mtime from fstat on the same fd to ensure
            // the snapshot is internally consistent.
            let mut file = match fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
            {
                Ok(f) => f,
                Err(_) => continue,
            };
            use std::io::Read;
            let mut content = String::new();
            if file.read_to_string(&mut content).is_err() {
                continue;
            }
            let meta = match file.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let parsed = todo::parse(&content, id, &ws_name);
            let mtime_ns = meta.mtime() as i128 * 1_000_000_000i128 + meta.mtime_nsec() as i128;
            out.insert(
                (ws_name.clone(), id.to_string()),
                Snapshot {
                    mtime_ns,
                    status: parsed.status,
                },
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Read-back sink for watcher tests (mirrors main.rs's TestSink).
    struct TestSink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for TestSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn mk_watcher() -> (tempfile::TempDir, Arc<Store>, Watcher, Arc<Mutex<Vec<u8>>>) {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().join("todos")).unwrap());
        let cfg = Arc::new(Config {
            root: store.root().to_path_buf(),
            default_workspace: "default".into(),
            poll_interval: Duration::from_millis(50),
            fatal_error: None,
        });
        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer: Writer = Arc::new(Mutex::new(Box::new(TestSink(captured.clone()))));
        let w = Watcher::new(
            cfg,
            store.clone(),
            writer,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
        );
        (dir, store, w, captured)
    }

    #[test]
    fn diff_emits_created_for_new_keys() {
        let (_d, store, w, captured) = mk_watcher();
        store
            .create(
                "default",
                Some("T-1".into()),
                "x",
                "",
                crate::todo::Priority::Normal,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        let next = scan_root(store.root()).unwrap();
        w.diff_and_emit(&HashMap::new(), &next);
        let bytes = captured.lock().unwrap().clone();
        let line = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .next()
            .expect("expected at least one frame");
        let frame: Value = serde_json::from_str(line).unwrap();
        assert_eq!(frame["method"], "event.publish");
        assert_eq!(frame["params"]["kind"], "todo.created");
        assert_eq!(frame["params"]["payload"]["id"], "T-1");
    }

    #[test]
    fn diff_emits_completed_on_status_done_transition() {
        let (_d, store, w, captured) = mk_watcher();
        store
            .create(
                "default",
                Some("T-2".into()),
                "x",
                "",
                crate::todo::Priority::Normal,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        let prev = scan_root(store.root()).unwrap();
        // Sleep just enough that mtime nanoseconds shift on
        // filesystems with us granularity.
        thread::sleep(Duration::from_millis(20));
        store
            .set_status("default", "T-2", crate::todo::Status::Done)
            .unwrap();
        let next = scan_root(store.root()).unwrap();
        w.diff_and_emit(&prev, &next);
        let bytes = captured.lock().unwrap().clone();
        let text = std::str::from_utf8(&bytes).unwrap();
        let mut kinds: Vec<String> = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line).unwrap();
            kinds.push(v["params"]["kind"].as_str().unwrap().to_string());
        }
        assert!(kinds.contains(&"todo.changed".to_string()));
        assert!(kinds.contains(&"todo.completed".to_string()));
    }

    #[test]
    fn diff_emits_deleted_for_missing_keys() {
        let (_d, store, w, captured) = mk_watcher();
        store
            .create(
                "default",
                Some("T-3".into()),
                "x",
                "",
                crate::todo::Priority::Normal,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        let prev = scan_root(store.root()).unwrap();
        store.delete("default", "T-3").unwrap();
        let next = scan_root(store.root()).unwrap();
        w.diff_and_emit(&prev, &next);
        let bytes = captured.lock().unwrap().clone();
        let line = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .next()
            .expect("expected at least one frame");
        let frame: Value = serde_json::from_str(line).unwrap();
        assert_eq!(frame["params"]["kind"], "todo.deleted");
        assert_eq!(frame["params"]["payload"]["id"], "T-3");
    }

    #[test]
    fn brand_new_done_todo_does_not_emit_completed() {
        let (_d, store, w, captured) = mk_watcher();
        store
            .create(
                "default",
                Some("T-4".into()),
                "already done",
                "",
                crate::todo::Priority::Normal,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        store
            .set_status("default", "T-4", crate::todo::Status::Done)
            .unwrap();
        let next = scan_root(store.root()).unwrap();
        w.diff_and_emit(&HashMap::new(), &next);
        let bytes = captured.lock().unwrap().clone();
        let text = std::str::from_utf8(&bytes).unwrap();
        let mut kinds: Vec<String> = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line).unwrap();
            kinds.push(v["params"]["kind"].as_str().unwrap().to_string());
        }
        assert_eq!(kinds, vec!["todo.created".to_string()]);
    }
}
