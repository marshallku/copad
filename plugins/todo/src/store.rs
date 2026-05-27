//! Filesystem operations on `~/docs/todos/<workspace>/<id>.md`.
//!
//! Atomicity contract:
//! - `create` uses temp + `copad_core::fs_atomic::rename_no_replace`
//!   (Linux `renameat2(RENAME_NOREPLACE)` / macOS
//!   `renamex_np(RENAME_EXCL)`) so concurrent creators never see torn
//!   files; exactly one wins on id collision (loser gets `id_exists`).
//! - `set_status` is read-modify-rewrite. The rewrite uses temp +
//!   `rename` (atomic replace, NO_REPLACE-not-applicable since the
//!   file MUST exist). A concurrent vim save during the same window
//!   loses — we accept this because the user is the only writer
//!   most of the time, and detecting a mid-edit collision properly
//!   needs an mtime check that's racey on its own. Future: file lock.
//! - `delete` is `unlink`; idempotent (NotFound is OK).
//!
//! Trust-boundary defense (mirrors `copad-plugin-kb`):
//! - Workspace label is validated against the same charset before
//!   it's joined into the path.
//! - Id is validated to reject path separators, `..`, leading dot,
//!   nul, leading slash. Always written as `<id>.md`.
//! - Root is force-canonicalized at construction; every resolved
//!   path is re-checked against `root_canonical` to defeat any
//!   symlink placed inside the root.
//! - O_NOFOLLOW on read so a leaf-swap during a read doesn't
//!   redirect us out.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::todo::{self, Priority, Status, Todo};

#[derive(Debug)]
pub enum Err {
    InvalidParams(String),
    InvalidId(String),
    NotFound(String),
    IdExists(String),
    Io(String),
}

impl Err {
    pub fn code_message(&self) -> (&'static str, String) {
        match self {
            Err::InvalidParams(m) => ("invalid_params", m.clone()),
            Err::InvalidId(m) => ("invalid_id", m.clone()),
            Err::NotFound(m) => ("not_found", m.clone()),
            Err::IdExists(m) => ("id_exists", m.clone()),
            Err::Io(m) => ("io_error", m.clone()),
        }
    }
}

pub struct Store {
    root: PathBuf,
    root_canonical: PathBuf,
}

impl Store {
    /// Creates the root if missing (first-run) and stores the canonicalized
    /// form so later paths can be re-checked against `root_canonical`.
    pub fn new(root: PathBuf) -> Result<Self, Err> {
        if let Err(e) = fs::create_dir_all(&root) {
            return Err(Err::Io(format!("mkdir {}: {e}", root.display())));
        }
        let root_canonical = fs::canonicalize(&root)
            .map_err(|e| Err::Io(format!("canonicalize {}: {e}", root.display())))?;
        Ok(Self {
            root,
            root_canonical,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Builds (creates on demand) the per-workspace dir. Refuses if the
    /// workspace path is a symlink, even when its target stays inside
    /// the root: `default → work` would let `create("default", ...)` write
    /// to `work/` while `list`/watcher (real-subdir scans) attribute the
    /// same file to workspace `work`, breaking workspace-handle stability.
    fn workspace_dir(&self, workspace: &str) -> Result<PathBuf, Err> {
        crate::config::validate_workspace(workspace)
            .map_err(|m| Err::InvalidParams(format!("workspace: {m}")))?;
        let dir = self.root.join(workspace);
        // lstat-style check: refuse pre-existing symlinks at the
        // workspace dir position. Non-existence is fine (we'll
        // mkdir below). Anything else (EACCES, ENOTDIR, etc.) is
        // surfaced rather than silently passing.
        match fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(Err::InvalidParams(format!(
                    "workspace path is a symlink (refusing to follow): {}",
                    dir.display()
                )));
            }
            Ok(m) if !m.is_dir() => {
                return Err(Err::InvalidParams(format!(
                    "workspace path is not a directory: {}",
                    dir.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Err::Io(format!("symlink_metadata {}: {e}", dir.display())));
            }
        }
        fs::create_dir_all(&dir).map_err(|e| Err::Io(format!("mkdir {}: {e}", dir.display())))?;
        // After creating, verify the canonical path stays under the root.
        // Belt-and-braces against a TOCTOU swap between the
        // symlink_metadata check and the rename/write — narrow
        // window, but a free check.
        let canon = fs::canonicalize(&dir)
            .map_err(|e| Err::Io(format!("canonicalize {}: {e}", dir.display())))?;
        if !canon.starts_with(&self.root_canonical) {
            return Err(Err::InvalidParams(format!(
                "workspace path escapes root: {}",
                canon.display()
            )));
        }
        Ok(dir)
    }

    fn todo_path(&self, workspace: &str, id: &str) -> Result<PathBuf, Err> {
        validate_id(id)?;
        let dir = self.workspace_dir(workspace)?;
        Ok(dir.join(format!("{id}.md")))
    }

    /// `O_NOFOLLOW` so a leaf-swap symlink can't redirect the read.
    pub fn read(&self, workspace: &str, id: &str) -> Result<Todo, Err> {
        let path = self.todo_path(workspace, id)?;
        let mut f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => Err::NotFound(format!("todo {workspace}/{id}")),
                _ => Err::Io(format!("open {}: {e}", path.display())),
            })?;
        let mut s = String::new();
        f.read_to_string(&mut s)
            .map_err(|e| Err::Io(format!("read {}: {e}", path.display())))?;
        Ok(todo::parse(&s, id, workspace))
    }

    /// Atomic create; returns the persisted `Todo` (echoing the supplied
    /// or generated id). `id_exists` on collision.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        workspace: &str,
        id: Option<String>,
        title: &str,
        body: &str,
        priority: Priority,
        due: Option<String>,
        linked_jira: Option<String>,
        linked_slack: Vec<serde_json::Value>,
        linked_kb: Vec<String>,
        tags: Vec<String>,
        prompt: Option<String>,
    ) -> Result<Todo, Err> {
        let title = title.trim();
        if title.is_empty() {
            return Err(Err::InvalidParams("title is required".to_string()));
        }
        let id = match id {
            Some(s) => {
                validate_id(&s)?;
                s
            }
            None => generate_id(),
        };
        let todo = Todo {
            id: id.clone(),
            status: Status::Open,
            created: now_rfc3339(),
            workspace: workspace.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            priority,
            due,
            linked_jira,
            linked_slack,
            linked_kb,
            tags,
            prompt,
        };
        let content = todo::render_new(&todo);
        let path = self.todo_path(workspace, &id)?;
        atomic_create(&path, content.as_bytes())?;
        Ok(todo)
    }

    /// In-place rewrite of the `status:` frontmatter line. Returns
    /// `(previous, new)` so the watcher can detect `→ done` transitions.
    /// Falls back to a full `render_new` if frontmatter is malformed
    /// (no closing fence, no status line).
    pub fn set_status(
        &self,
        workspace: &str,
        id: &str,
        new_status: Status,
    ) -> Result<(Status, Status), Err> {
        let path = self.todo_path(workspace, id)?;
        let mut f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => Err::NotFound(format!("todo {workspace}/{id}")),
                _ => Err::Io(format!("open {}: {e}", path.display())),
            })?;
        let mut content = String::new();
        f.read_to_string(&mut content)
            .map_err(|e| Err::Io(format!("read {}: {e}", path.display())))?;
        drop(f);
        let prev = todo::parse(&content, id, workspace).status;

        let new_content = match todo::update_status_in_text(&content, new_status) {
            Some(s) => s,
            None => {
                // Fallback: malformed file — rebuild from parsed
                // state with new status.
                let mut t = todo::parse(&content, id, workspace);
                t.status = new_status;
                todo::render_new(&t)
            }
        };
        atomic_replace(&path, new_content.as_bytes())?;
        Ok((prev, new_status))
    }

    /// Full regenerate via `render_new` (canonicalizes ordering, drops
    /// comments) — appropriate for UI form-submit, wrong for the
    /// high-frequency status-toggle path (use `set_status` instead).
    /// Outer `Option` is "None = don't touch"; for `Option<Option<String>>`
    /// fields (`due`, `linked_jira`, `prompt`) an inner empty string clears.
    /// `title: Some("")` is rejected. `body` and `append_subtask` are
    /// mutually exclusive — the latter performs the read-modify-write
    /// inside this call so callers don't widen the race window with a
    /// client-side preflight fetch.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &self,
        workspace: &str,
        id: &str,
        title: Option<String>,
        body: Option<String>,
        append_subtask: Option<String>,
        priority: Option<Priority>,
        due: Option<Option<String>>,
        linked_jira: Option<Option<String>>,
        linked_kb: Option<Vec<String>>,
        tags: Option<Vec<String>>,
        prompt: Option<Option<String>>,
    ) -> Result<Todo, Err> {
        if body.is_some() && append_subtask.is_some() {
            return Err(Err::InvalidParams(
                "'body' and 'append_subtask' are mutually exclusive".into(),
            ));
        }
        if let Some(s) = &append_subtask
            && s.is_empty()
        {
            return Err(Err::InvalidParams(
                "'append_subtask' cannot be empty".into(),
            ));
        }
        let path = self.todo_path(workspace, id)?;
        let mut f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => Err::NotFound(format!("todo {workspace}/{id}")),
                _ => Err::Io(format!("open {}: {e}", path.display())),
            })?;
        let mut content = String::new();
        f.read_to_string(&mut content)
            .map_err(|e| Err::Io(format!("read {}: {e}", path.display())))?;
        drop(f);
        let mut t = todo::parse(&content, id, workspace);
        if let Some(s) = title {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                return Err(Err::InvalidParams("title cannot be empty".into()));
            }
            t.title = trimmed;
        }
        if let Some(b) = body {
            t.body = b;
        }
        if let Some(text) = append_subtask {
            let line = format!("- [ ] {text}");
            if t.body.is_empty() {
                t.body = line;
            } else if t.body.ends_with('\n') {
                t.body.push_str(&line);
            } else {
                t.body.push('\n');
                t.body.push_str(&line);
            }
        }
        if let Some(p) = priority {
            t.priority = p;
        }
        if let Some(d) = due {
            t.due = d.filter(|s| !s.is_empty());
        }
        if let Some(j) = linked_jira {
            t.linked_jira = j.filter(|s| !s.is_empty());
        }
        if let Some(kb) = linked_kb {
            t.linked_kb = kb;
        }
        if let Some(tg) = tags {
            t.tags = tg;
        }
        if let Some(pr) = prompt {
            t.prompt = pr.filter(|s| !s.is_empty());
        }
        let new_content = todo::render_new(&t);
        atomic_replace(&path, new_content.as_bytes())?;
        Ok(t)
    }

    /// Delete a todo file. NotFound is OK (idempotent).
    pub fn delete(&self, workspace: &str, id: &str) -> Result<(), Err> {
        let path = self.todo_path(workspace, id)?;
        match fs::remove_file(&path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Err::Io(format!("remove {}: {e}", path.display()))),
        }
    }

    /// `None` walks every workspace dir; `Some(label)` restricts to one.
    /// Symlinked entries are skipped (same posture as KB search walk).
    pub fn list_all(&self, workspace_filter: Option<&str>) -> Result<Vec<Todo>, Err> {
        let mut todos = Vec::new();
        let workspaces = match workspace_filter {
            Some(ws) => {
                crate::config::validate_workspace(ws)
                    .map_err(|m| Err::InvalidParams(format!("workspace: {m}")))?;
                vec![ws.to_string()]
            }
            None => list_subdirs(&self.root)?,
        };
        for ws in workspaces {
            let dir = self.root.join(&ws);
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Err::Io(format!("readdir {}: {e}", dir.display()))),
            };
            for entry in entries.flatten() {
                // Use `file_type()` (lstat-based) so we identify the
                // entry itself, not what it might point at — that
                // makes the symlink skip below actually skip.
                // `metadata()` follows symlinks; using it here would
                // silently let a symlink through.
                let ft = match entry.file_type() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if ft.is_symlink() || !ft.is_file() {
                    continue;
                }
                let path = entry.path();
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
                match self.read(&ws, id) {
                    Ok(t) => todos.push(t),
                    Err(_) => continue,
                }
            }
        }
        Ok(todos)
    }
}

fn list_subdirs(root: &Path) -> Result<Vec<String>, Err> {
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Err::Io(format!("readdir {}: {e}", root.display()))),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        // `file_type()` is lstat-based (symlink-aware). `metadata()`
        // would follow symlinks and break the skip below.
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str()
            && crate::config::validate_workspace(name).is_ok()
        {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

/// Reject characters that would let an id escape the workspace
/// directory or collide with hidden files / temp files.
pub fn validate_id(id: &str) -> Result<(), Err> {
    if id.is_empty() {
        return Err(Err::InvalidId("id cannot be empty".to_string()));
    }
    if id.starts_with('.') {
        return Err(Err::InvalidId(format!(
            "id {id:?} starts with '.' (reserved for hidden/temp files)"
        )));
    }
    if id.contains('/') || id.contains('\\') || id.contains('\0') {
        return Err(Err::InvalidId(format!(
            "id {id:?} contains path separator or nul"
        )));
    }
    if id == "." || id == ".." {
        return Err(Err::InvalidId(format!("id {id:?} is reserved")));
    }
    for c in id.chars() {
        if c.is_control() {
            return Err(Err::InvalidId(format!("id {id:?} contains control char")));
        }
    }
    Ok(())
}

/// `T-YYYYMMDDHHMMSS-<seq>` — sortable, no RNG dep, distinct under
/// same-second concurrent creates.
pub fn generate_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let now = chrono::Utc::now();
    format!("T-{}-{:04}", now.format("%Y%m%d%H%M%S"), seq % 10_000)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_path(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    parent.join(format!(".todo-tmp-{pid}-{nanos}-{seq}"))
}

/// Atomic create: write to temp, rename in with `RENAME_NOREPLACE`.
/// Returns `Err::IdExists` on collision.
fn atomic_create(target: &Path, content: &[u8]) -> Result<(), Err> {
    let tmp = temp_path(target);
    {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| Err::Io(format!("create temp {}: {e}", tmp.display())))?;
        f.write_all(content)
            .map_err(|e| Err::Io(format!("write temp {}: {e}", tmp.display())))?;
        f.sync_data()
            .map_err(|e| Err::Io(format!("fsync temp {}: {e}", tmp.display())))?;
    }
    match copad_core::fs_atomic::rename_no_replace(&tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            if e.kind() == io::ErrorKind::AlreadyExists {
                Err(Err::IdExists(format!("todo {}", target.display())))
            } else {
                Err(Err::Io(format!(
                    "rename_no_replace -> {}: {e}",
                    target.display()
                )))
            }
        }
    }
}

/// Atomic replace: write to temp, rename in (REPLACES existing).
fn atomic_replace(target: &Path, content: &[u8]) -> Result<(), Err> {
    let tmp = temp_path(target);
    {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| Err::Io(format!("create temp {}: {e}", tmp.display())))?;
        f.write_all(content)
            .map_err(|e| Err::Io(format!("write temp {}: {e}", tmp.display())))?;
        f.sync_data()
            .map_err(|e| Err::Io(format!("fsync temp {}: {e}", tmp.display())))?;
    }
    fs::rename(&tmp, target).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Err::Io(format!("rename -> {}: {e}", target.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn mk_store() -> (tempfile::TempDir, Store) {
        let dir = tempdir().unwrap();
        let s = Store::new(dir.path().join("todos")).unwrap();
        (dir, s)
    }

    #[test]
    fn create_then_read_round_trips() {
        let (_d, s) = mk_store();
        let t = s
            .create(
                "default",
                None,
                "buy milk",
                "extra body",
                Priority::Normal,
                Some("2026-05-01".into()),
                None,
                Vec::new(),
                Vec::new(),
                vec!["personal".into()],
                None,
            )
            .unwrap();
        assert_eq!(t.status, Status::Open);
        assert!(t.id.starts_with("T-"));
        let read = s.read("default", &t.id).unwrap();
        assert_eq!(read.title, "buy milk");
        assert_eq!(read.due.as_deref(), Some("2026-05-01"));
        assert_eq!(read.tags, vec!["personal".to_string()]);
    }

    #[test]
    fn create_with_explicit_id_collides_on_repeat() {
        let (_d, s) = mk_store();
        s.create(
            "default",
            Some("T-fixed".into()),
            "first",
            "",
            Priority::Normal,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap();
        let err = s
            .create(
                "default",
                Some("T-fixed".into()),
                "second",
                "",
                Priority::Normal,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap_err();
        assert!(matches!(err, Err::IdExists(_)));
    }

    #[test]
    fn validate_id_rejects_traversal_and_hidden() {
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "x\0y", ".hidden"] {
            assert!(validate_id(bad).is_err(), "should reject {bad:?}");
        }
        for good in ["T-1", "alpha", "task-2026-04-27"] {
            validate_id(good).unwrap_or_else(|_| panic!("rejected {good:?}"));
        }
    }

    #[test]
    fn set_status_preserves_other_frontmatter() {
        let (_d, s) = mk_store();
        let t = s
            .create(
                "default",
                None,
                "x",
                "",
                Priority::High,
                Some("2026-05-01".into()),
                None,
                Vec::new(),
                Vec::new(),
                vec!["a".into()],
                None,
            )
            .unwrap();
        let (prev, next) = s.set_status("default", &t.id, Status::Done).unwrap();
        assert_eq!(prev, Status::Open);
        assert_eq!(next, Status::Done);
        let after = s.read("default", &t.id).unwrap();
        assert_eq!(after.status, Status::Done);
        assert_eq!(after.priority, Priority::High);
        assert_eq!(after.due.as_deref(), Some("2026-05-01"));
        assert_eq!(after.tags, vec!["a".to_string()]);
    }

    #[test]
    fn list_all_filters_by_workspace() {
        let (_d, s) = mk_store();
        s.create(
            "alpha",
            None,
            "in alpha",
            "",
            Priority::Normal,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap();
        s.create(
            "beta",
            None,
            "in beta",
            "",
            Priority::Normal,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap();
        let all = s.list_all(None).unwrap();
        assert_eq!(all.len(), 2);
        let just_alpha = s.list_all(Some("alpha")).unwrap();
        assert_eq!(just_alpha.len(), 1);
        assert_eq!(just_alpha[0].title, "in alpha");
    }

    #[test]
    fn list_all_skips_temp_dotfiles() {
        let (_d, s) = mk_store();
        s.create(
            "default",
            None,
            "real",
            "",
            Priority::Normal,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap();
        // Drop a stray .todo-tmp-* sibling — must be ignored.
        std::fs::write(
            s.root.join("default").join(".todo-tmp-leftover.md"),
            "garbage",
        )
        .unwrap();
        let all = s.list_all(None).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn delete_is_idempotent() {
        let (_d, s) = mk_store();
        let t = s
            .create(
                "default",
                None,
                "x",
                "",
                Priority::Normal,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        s.delete("default", &t.id).unwrap();
        s.delete("default", &t.id).unwrap();
        assert!(matches!(
            s.read("default", &t.id).unwrap_err(),
            Err::NotFound(_)
        ));
    }

    #[test]
    fn list_all_skips_symlinked_md_files() {
        let (_d, s) = mk_store();
        let outside = tempdir().unwrap();
        let target = outside.path().join("escape.md");
        std::fs::write(&target, "---\nstatus: open\n---\n# escaped\n").unwrap();
        std::fs::create_dir_all(s.root.join("default")).unwrap();
        let link = s.root.join("default").join("hijack.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let all = s.list_all(None).unwrap();
        assert!(
            all.iter().all(|t| t.id != "hijack"),
            "symlinked entry must not surface in list_all, got: {:?}",
            all.iter().map(|t| &t.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn create_rejects_symlinked_workspace_dir() {
        let (_d, s) = mk_store();
        // Pre-create a real "real" workspace, then make a symlink
        // "alias" pointing at it. todo.create("alias", …) must
        // refuse rather than redirect through and cause workspace
        // misattribution at list/watch time.
        s.create(
            "real",
            None,
            "seed",
            "",
            Priority::Normal,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap();
        std::os::unix::fs::symlink(s.root.join("real"), s.root.join("alias")).unwrap();
        let err = s
            .create(
                "alias",
                None,
                "x",
                "",
                Priority::Normal,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap_err();
        assert!(matches!(err, Err::InvalidParams(_)), "got {err:?}");
    }

    #[test]
    fn list_all_skips_symlinked_workspace_dirs() {
        let (_d, s) = mk_store();
        let outside = tempdir().unwrap();
        let outside_ws = outside.path().join("foreign");
        std::fs::create_dir_all(&outside_ws).unwrap();
        std::fs::write(
            outside_ws.join("steal.md"),
            "---\nstatus: open\n---\n# steal\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside_ws, s.root.join("foreign-link")).unwrap();
        let all = s.list_all(None).unwrap();
        assert!(
            all.iter().all(|t| t.id != "steal"),
            "symlinked workspace dir must not surface, got: {:?}",
            all.iter().map(|t| &t.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn workspace_traversal_rejected() {
        let (_d, s) = mk_store();
        let err = s
            .create(
                "../escape",
                None,
                "x",
                "",
                Priority::Normal,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap_err();
        assert!(matches!(err, Err::InvalidParams(_)));
    }
}
