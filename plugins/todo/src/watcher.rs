//! Polling file-watcher that emits `todo.created` / `todo.changed` / `todo.completed` /
//! `todo.deleted`.
//!
//! Polling, not inotify: it reuses the calendar plugin's pattern, and external (vim) edits are
//! noticed with 1-2s latency.
//!
//! The first scan is the baseline: nothing is emitted for todos that already exist at startup, or
//! every copad restart would spam triggers. Later ticks diff by `(workspace, id)`: new key ->
//! `todo.created`; mtime changed -> `todo.changed` (+ `todo.completed` if status went `* -> done`);
//! missing key -> `todo.deleted`. Payloads are the full `Todo::to_json`.
//!
//! A todo created already `done` emits only `todo.created`: `completed` is for transitions
//! observed after birth, which is what "task just got finished" triggers want.

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
    /// Set from main on stdin EOF / `shutdown`. Polled between scans and during sleep so the loop
    /// exits promptly.
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
            // Broken config: don't pretend to watch; actions surface the error.
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
            // Sleep in 100ms chunks so a shutdown is noticed within ~100ms instead of up to one
            // poll_interval.
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
                // The file is gone, so emit a minimal payload with just the keys consumers need.
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
            // O_NOFOLLOW so a swap to a symlink between file_type() above and this read can't
            // redirect us out (TOCTOU). mtime comes from fstat on the same fd so the snapshot is
            // internally consistent.
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
