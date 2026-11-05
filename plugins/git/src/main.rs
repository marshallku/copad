//! First-party git workspace + worktree plugin for copad.
//!
//! Every action is an argv-vector shell-out to `git`, never a shell string, so caller-supplied
//! data (branch names, base refs, paths) cannot inject extra arguments. Workspaces are configured
//! in `~/.config/copad/workspaces.toml` (override via `COPAD_GIT_WORKSPACES_FILE`).
//!
//! `git.worktree_remove` refuses paths outside any configured `worktree_root`, so a misconfigured
//! trigger can never delete arbitrary directories. An `Ok(payload)` from `git.worktree_add` makes
//! the registry stamp `git.worktree_add.completed` onto the bus; the plugin does not emit it.
//!
//! Activation is `onStartup`: the per-workspace polling watcher emits `git.worktree_created`,
//! `git.worktree_removed`, `git.branch_created`, `git.branch_deleted` and `git.checkout`, and must
//! be alive whenever copad runs. Poll interval: `COPAD_GIT_POLL_MS` (default 2000 ms).

mod config;
mod git;
mod watcher;

use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use serde_json::{Value, json};

use config::{Config, Workspace};
use git::WorktreeInfo;

const PROTOCOL_VERSION: u32 = 1;

fn main() {
    let config = Arc::new(Config::from_env());
    if let Some(err) = &config.fatal_error {
        eprintln!("[git] config error (workspace-bound actions will return config_error): {err}");
    }
    eprintln!(
        "[git] config = {}, workspaces = {}",
        config.config_path.display(),
        config.workspaces.len()
    );

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let (tx, rx) = channel::<String>();
    let writer_tx = tx.clone();
    thread::spawn(move || {
        let mut out = stdout.lock();
        for line in rx.iter() {
            if writeln!(out, "{line}").is_err() || out.flush().is_err() {
                break;
            }
        }
    });

    let initialized = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    // One polling watcher thread per workspace, emitting through the same writer channel actions
    // use. Events are held back until the host's `initialize` -> `initialized` handshake completes.
    watcher::spawn(
        config.clone(),
        tx.clone(),
        stop.clone(),
        initialized.clone(),
    );

    let reader = BufReader::new(stdin.lock());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        let frame: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[git] parse error: {e}");
                continue;
            }
        };
        handle_frame(&frame, &writer_tx, &initialized, &config);
    }
}

fn handle_frame(frame: &Value, tx: &Sender<String>, initialized: &AtomicBool, config: &Config) {
    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    let id = frame.get("id").and_then(Value::as_str).unwrap_or("");
    let params = frame.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let proto = params.get("protocol_version").and_then(Value::as_u64);
            if proto != Some(PROTOCOL_VERSION as u64) {
                send_error(
                    tx,
                    id,
                    "protocol_mismatch",
                    &format!("git plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": [
                        "git.list_workspaces",
                        "git.list_worktrees",
                        "git.worktree_add",
                        "git.worktree_remove",
                        "git.current_branch",
                        "git.status",
                    ],
                    "subscribes": [],
                }),
            );
        }
        "initialized" => {
            initialized.store(true, Ordering::SeqCst);
        }
        "action.invoke" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let action_params = params.get("params").cloned().unwrap_or(Value::Null);
            let result = handle_action(&name, &action_params, config);
            match result {
                Ok(v) => send_response(tx, id, v),
                Err((code, msg)) => send_error(tx, id, &code, &msg),
            }
        }
        "event.dispatch" => {}
        "shutdown" => std::process::exit(0),
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                tx,
                id,
                "unknown_method",
                &format!("git plugin: unknown method {other}"),
            );
        }
        _ => {}
    }
}

fn handle_action(name: &str, params: &Value, config: &Config) -> Result<Value, (String, String)> {
    match name {
        "git.list_workspaces" => action_list_workspaces(config),
        "git.list_worktrees" => action_list_worktrees(params, config),
        "git.worktree_add" => action_worktree_add(params, config),
        "git.worktree_remove" => action_worktree_remove(params, config),
        "git.current_branch" => action_current_branch(params, config),
        "git.status" => action_status(params, config),
        other => Err((
            "action_not_found".into(),
            format!("git plugin does not handle {other}"),
        )),
    }
}

fn action_list_workspaces(config: &Config) -> Result<Value, (String, String)> {
    // list_workspaces is the discovery surface and must succeed even when the config has fatal
    // errors, so callers can detect "up but nothing loadable" without parsing logs. `fatal_error`
    // rides in the payload; workspace-bound actions still short-circuit through
    // `require_workspace`.
    let arr: Vec<Value> = config
        .workspaces
        .iter()
        .map(|w| {
            // Best-effort: one broken workspace must not poison the list, so degraded entries get
            // nullable fields.
            let current_branch = git::current_branch(&w.path).ok();
            let worktree_count = git::list_worktrees(&w.path).map(|v| v.len()).ok();
            let mut obj = serde_json::Map::new();
            obj.insert("name".into(), Value::String(w.name.clone()));
            obj.insert("path".into(), Value::String(w.path.display().to_string()));
            obj.insert("default_base".into(), Value::String(w.default_base.clone()));
            obj.insert(
                "worktree_root".into(),
                Value::String(w.worktree_root.display().to_string()),
            );
            obj.insert(
                "current_branch".into(),
                current_branch.map(Value::String).unwrap_or(Value::Null),
            );
            obj.insert(
                "worktree_count".into(),
                worktree_count
                    .map(|n| Value::Number(n.into()))
                    .unwrap_or(Value::Null),
            );
            Value::Object(obj)
        })
        .collect();
    Ok(json!({
        "workspaces": arr,
        "fatal_error": config.fatal_error.clone(),
    }))
}

fn action_list_worktrees(params: &Value, config: &Config) -> Result<Value, (String, String)> {
    let ws = require_workspace(params, config)?;
    let wts = git::list_worktrees(&ws.path).map_err(git_err_to_action)?;
    Ok(json!({
        "workspace": ws.name,
        "worktrees": wts.iter().map(worktree_to_json).collect::<Vec<_>>(),
    }))
}

fn action_worktree_add(params: &Value, config: &Config) -> Result<Value, (String, String)> {
    let ws = require_workspace(params, config)?;
    let raw_branch = required_string(params, "branch")?;
    // `sanitize_jira` lowercases the branch (keeping slashes for Jira hierarchies) before
    // validation, so a trigger can interpolate `linked_jira` from a Todo payload without
    // pre-lowercasing.
    // Default `false` keeps callers that pass a prepared branch name unchanged.
    let sanitize_jira = optional_bool(params, "sanitize_jira")?.unwrap_or(false);
    let branch = if sanitize_jira {
        git::sanitize_jira_branch(&raw_branch).map_err(git_err_to_action)?
    } else {
        git::validate_branch_name(&raw_branch).map_err(git_err_to_action)?;
        raw_branch
    };
    // Opaque passthrough for chained triggers: the `git.worktree_add.completed` payload is the only
    // way a downstream trigger (typically `claude.start`) sees fields the upstream event carried.
    // Echoed unchanged, and documented fields win on collision (`prompt` cannot shadow `path`).
    let prompt_passthrough = optional_string(params, "prompt")?;

    // Idempotent fast path: if the branch already has a worktree in this repo, echo its recorded
    // path so a re-click flows on to claude.start. The existing path is still validated to be under
    // `worktree_root` with no symlink ancestors (a worktree later replaced by a symlink to `/etc`
    // would otherwise open a tab in the redirected directory); outside the current root it is
    // `invalid_state` rather than silently honored. Stale entries (`prunable`, directory removed)
    // fall through to the create path, where git's own error points at `git worktree prune`.
    if let Ok(existing) = git::list_worktrees(&ws.path)
        && let Some(w) = existing.into_iter().find(|w| {
            if w.branch.as_deref() != Some(&branch) || w.prunable || !w.path.is_dir() {
                return false;
            }
            // Skip the primary checkout: `git worktree list` includes it, and `worktree_add
            // {branch: "main"}` on a repo checked out to `main` would otherwise fail the
            // worktree_root guard instead of
            // returning the usual `branch_exists`.
            canonicalize_existing_or_self(&w.path) != ws.path
        })
    {
        // Mirror the create path's validation chain so the idempotent return is no weaker a trust
        // gate.
        // Order matters:
        // 1. `canonicalize_existing_or_self` + `path_starts_with` on the canonical path first:
        // closes
        //    the `<root>/../etc` bypass and reports a reconfigured worktree_root as
        // `invalid_state`.
        //    It must precede the symlink walker, whose lexical strip_prefix failure would report
        //    `forbidden` and swallow that.
        // 2. `check_no_symlink_ancestors` on the original `w.path`: canonicalize follows symlinks
        // and
        //    would make the check vacuous.
        // 3. `validate_base_ref` on `base`, keeping request validation independent of repo state.
        let canon = canonicalize_existing_or_self(&w.path);
        if !path_starts_with(&canon, &ws.worktree_root) {
            return Err((
                "invalid_state".into(),
                format!(
                    "registered worktree at {} (canonical {}) is outside worktree_root {} — \
                     reconfigure the workspace or remove the worktree",
                    w.path.display(),
                    canon.display(),
                    ws.worktree_root.display()
                ),
            ));
        }
        check_no_symlink_ancestors(&ws.worktree_root, &w.path)?;
        let base = optional_string(params, "base")?.unwrap_or_else(|| ws.default_base.clone());
        if base.is_empty() {
            return Err(("invalid_params".into(), "base cannot be empty".into()));
        }
        git::validate_base_ref(&base).map_err(git_err_to_action)?;
        // `event.base` echoes the current request's base, not the one the worktree was created from
        // (`git worktree list --porcelain` does not record it).
        let mut payload = json!({
            "workspace": ws.name,
            "path": w.path.display().to_string(),
            "branch": branch,
            "base": base,
        });
        if let Some(p) = prompt_passthrough
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert("prompt".to_string(), Value::String(p));
        }
        return Ok(payload);
    }

    let base = optional_string(params, "base")?.unwrap_or_else(|| ws.default_base.clone());
    let target = compute_worktree_target(ws, &branch);
    // Lexical guard against `..`-style escapes (validate_branch_name also rejects `..`).
    if !path_starts_with(&target, &ws.worktree_root) {
        return Err((
            "invalid_branch".into(),
            format!(
                "computed target {} would land outside worktree_root {}",
                target.display(),
                ws.worktree_root.display()
            ),
        ));
    }
    // Symlink-ancestors guard. The lexical check is textual: if `<worktree_root>/feature` is a
    // symlink to `/etc`, `<worktree_root>/feature/x` passes `starts_with` while `git worktree add`
    // follows the link. Walk every existing prefix of `target` under `worktree_root` and reject any
    // symlink component.
    check_no_symlink_ancestors(&ws.worktree_root, &target)?;
    let result = git::worktree_add(&ws.path, &target, &branch, &base).map_err(git_err_to_action)?;
    let mut payload = json!({
        "workspace": ws.name,
        "path": result.path.display().to_string(),
        "branch": result.branch,
        "base": base,
    });
    if let Some(p) = prompt_passthrough
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert("prompt".to_string(), Value::String(p));
    }
    // `git.worktree_add.completed` is auto-emitted by the registry on every successful dispatch;
    // emitting it here too would run chained triggers twice.
    Ok(payload)
}

fn action_worktree_remove(params: &Value, config: &Config) -> Result<Value, (String, String)> {
    let path_str = required_string(params, "path")?;
    let force = optional_bool(params, "force")?.unwrap_or(false);
    let target = PathBuf::from(&path_str);
    // Refuse to delete anything outside the configured workspace `path`s or `worktree_root`s, so a
    // trigger that interpolates the wrong field cannot remove arbitrary directories. Same
    // allow-list as `status {path}`. canonicalize_existing_or_self strips `..` from the tail so
    // `<allowed>/../etc` cannot lexically pass.
    let canon_target = canonicalize_existing_or_self(&target);
    // Workspaces cannot overlap (config.rs rejects pair-wise overlap), so at most one matches and
    // `find` is unambiguous, avoiding the "longest prefix" trap.
    let parent_ws = config.workspaces.iter().find(|w| {
        path_starts_with(&canon_target, &w.path)
            || path_starts_with(&canon_target, &w.worktree_root)
    });
    let ws = match parent_ws {
        Some(w) => w,
        None => {
            // A fatal config error may mean the intended workspace failed to load; report that
            // instead of a generic forbidden.
            let code = if config.fatal_error.is_some() {
                "config_error"
            } else {
                "forbidden"
            };
            let msg = if let Some(err) = &config.fatal_error {
                format!(
                    "{} is not under any configured workspace path or worktree_root; \
                     config also has errors: {err}",
                    canon_target.display()
                )
            } else {
                format!(
                    "{} is not under any configured workspace path or worktree_root; \
                     refusing to remove",
                    canon_target.display()
                )
            };
            return Err((code.to_string(), msg));
        }
    };
    git::worktree_remove(&ws.path, &target, force).map_err(git_err_to_action)?;
    Ok(json!({
        "workspace": ws.name,
        "path": target.display().to_string(),
        "removed": true,
    }))
}

fn action_current_branch(params: &Value, config: &Config) -> Result<Value, (String, String)> {
    let ws = require_workspace(params, config)?;
    let branch = git::current_branch(&ws.path).map_err(git_err_to_action)?;
    Ok(json!({ "workspace": ws.name, "branch": branch }))
}

fn action_status(params: &Value, config: &Config) -> Result<Value, (String, String)> {
    // Exactly one of `workspace` or `path` must be supplied.
    let workspace_arg = optional_string(params, "workspace")?;
    let path_arg = optional_string(params, "path")?;
    let target_path: PathBuf = match (workspace_arg, path_arg) {
        (Some(_), Some(_)) => {
            return Err((
                "invalid_params".into(),
                "supply either 'workspace' or 'path', not both".into(),
            ));
        }
        (Some(name), None) => match config.find(&name) {
            Some(w) => w.path.clone(),
            None => {
                let (code, msg) = if let Some(err) = &config.fatal_error {
                    (
                        "config_error",
                        format!("workspace {name:?} not loaded; config errors: {err}"),
                    )
                } else {
                    (
                        "not_found",
                        format!("workspace {name:?} not in workspaces.toml"),
                    )
                };
                return Err((code.to_string(), msg));
            }
        },
        (None, Some(p)) => {
            let canon = canonicalize_existing_or_self(&PathBuf::from(&p));
            let allowed = config.workspaces.iter().any(|w| {
                path_starts_with(&canon, &w.path) || path_starts_with(&canon, &w.worktree_root)
            });
            if !allowed {
                let (code, msg) = if let Some(err) = &config.fatal_error {
                    (
                        "config_error",
                        format!(
                            "{} is not under any configured workspace path or worktree_root; \
                             config also has errors: {err}",
                            canon.display()
                        ),
                    )
                } else {
                    (
                        "forbidden",
                        format!(
                            "{} is not under any configured workspace path or worktree_root",
                            canon.display()
                        ),
                    )
                };
                return Err((code.to_string(), msg));
            }
            canon
        }
        (None, None) => {
            return Err((
                "invalid_params".into(),
                "supply either 'workspace' or 'path'".into(),
            ));
        }
    };
    let r = git::status(&target_path).map_err(git_err_to_action)?;
    Ok(json!({
        "path": target_path.display().to_string(),
        "branch": r.branch,
        "upstream": r.upstream,
        "ahead": r.ahead,
        "behind": r.behind,
        "staged": r.staged,
        "unstaged": r.unstaged,
        "untracked": r.untracked,
        "dirty": r.dirty,
    }))
}

fn worktree_to_json(w: &WorktreeInfo) -> Value {
    json!({
        "path": w.path.display().to_string(),
        "branch": w.branch,
        "head_sha": w.head_sha,
        "locked": w.locked,
        "prunable": w.prunable,
    })
}

fn require_workspace<'a>(
    params: &Value,
    config: &'a Config,
) -> Result<&'a Workspace, (String, String)> {
    let name = required_string(params, "workspace")?;
    if let Some(w) = config.find(&name) {
        // The workspace itself loaded, so proceed even if other entries have errors; one typo must
        // not brick every other workspace.
        return Ok(w);
    }
    // `not_found` (unknown name, a typo) vs `config_error` (the name may match a block that failed
    // to load, so the user knows to check workspaces.toml).
    let (code, msg) = if let Some(err) = &config.fatal_error {
        (
            "config_error",
            format!("workspace {name:?} not loaded; config errors: {err}"),
        )
    } else {
        (
            "not_found",
            format!("workspace {name:?} not in workspaces.toml"),
        )
    };
    Err((code.to_string(), msg))
}

fn compute_worktree_target(ws: &Workspace, branch: &str) -> PathBuf {
    // A branch like `feature/foo` becomes nested path components under `worktree_root`.
    let mut p = ws.worktree_root.clone();
    for seg in branch.split('/') {
        p.push(seg);
    }
    p
}

/// Whole-component prefix match (`/a/foo-evil` does not match `/a/foo`).
fn path_starts_with(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

/// Walks components past `root`; refuses symlinks. `target` must already pass `starts_with(root)`.
/// Permission errors propagate, since a silent pass would skip the boundary check.
fn check_no_symlink_ancestors(root: &Path, target: &Path) -> Result<(), (String, String)> {
    let suffix = match target.strip_prefix(root) {
        Ok(s) => s,
        Err(_) => {
            // The caller lexically verified this already, so reaching here is a programming error.
            return Err((
                "forbidden".into(),
                format!(
                    "{} not under root {} (lexical check should have caught this)",
                    target.display(),
                    root.display()
                ),
            ));
        }
    };
    let mut cursor = root.to_path_buf();
    // Also check `root` itself: it may not have existed at config load and been created as a
    // symlink later. NotFound means `mkdir -p` below creates it, so no symlink is possible.
    match cursor.symlink_metadata() {
        Ok(md) if md.file_type().is_symlink() => {
            return Err((
                "forbidden".into(),
                format!("worktree_root {} is a symlink", cursor.display()),
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err((
                "io_error".into(),
                format!("stat worktree_root {}: {e}", cursor.display()),
            ));
        }
    }
    for comp in suffix.components() {
        if let Component::Normal(seg) = comp {
            cursor.push(seg);
            match cursor.symlink_metadata() {
                Ok(md) if md.file_type().is_symlink() => {
                    return Err((
                        "forbidden".into(),
                        format!(
                            "{} traverses a symlink at {}",
                            target.display(),
                            cursor.display()
                        ),
                    ));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Tail does not exist yet, so the rest cannot be a symlink.
                    return Ok(());
                }
                Err(e) => {
                    return Err((
                        "io_error".into(),
                        format!("stat ancestor {}: {e}", cursor.display()),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn canonicalize_existing_or_self(p: &Path) -> PathBuf {
    // Walk up to an existing prefix, canonicalize it, then re-attach the missing tail applying
    // `..` / `.` semantics. Without that, `<allowed-root>/../etc` would lexically pass a
    // `starts_with(allowed-root)` check while pointing out of the tree.
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    while !cur.as_os_str().is_empty() {
        if cur.exists()
            && let Ok(c) = std::fs::canonicalize(&cur)
        {
            let mut out = c;
            for t in tail.iter().rev() {
                if t == ".." {
                    out.pop();
                } else if t == "." {
                } else {
                    out.push(t);
                }
            }
            return out;
        }
        match cur.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            None => break,
        }
        if !cur.pop() {
            break;
        }
    }
    // Nothing existed: apply `..`/`.` to the bare input so the lexical guard stays meaningful.
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn git_err_to_action(e: git::GitError) -> (String, String) {
    (e.code.to_string(), e.message)
}

fn required_string(params: &Value, key: &str) -> Result<String, (String, String)> {
    let v = params.get(key).ok_or_else(|| {
        (
            "invalid_params".into(),
            format!("missing required field {key:?}"),
        )
    })?;
    v.as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            (
                "invalid_params".into(),
                format!("{key:?} must be a non-empty string"),
            )
        })
}

fn optional_string(params: &Value, key: &str) -> Result<Option<String>, (String, String)> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err((
            "invalid_params".into(),
            format!("{key:?} must be a string, got {other}"),
        )),
    }
}

fn optional_bool(params: &Value, key: &str) -> Result<Option<bool>, (String, String)> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err((
            "invalid_params".into(),
            format!("{key:?} must be a boolean, got {other}"),
        )),
    }
}

fn send_response(tx: &Sender<String>, id: &str, result: Value) {
    let frame = json!({ "id": id, "ok": true, "result": result });
    let _ = tx.send(frame.to_string());
}

fn send_error(tx: &Sender<String>, id: &str, code: &str, message: &str) {
    let frame = json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message },
    });
    let _ = tx.send(frame.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo(dir: &Path) {
        for cmd in [
            vec!["init", "--initial-branch=main", "."],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["commit", "--allow-empty", "-m", "initial"],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(&cmd)
                .status()
                .unwrap();
            assert!(status.success(), "git {cmd:?} failed");
        }
    }

    fn fixture_with_one_workspace() -> (tempfile::TempDir, Config) {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let cfg_path = dir.path().join("workspaces.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
[[workspace]]
name = "myrepo"
path = {p:?}
default_base = "main"
"#,
                p = repo.display().to_string(),
            ),
        )
        .unwrap();
        let config = Config::from_path(cfg_path);
        (dir, config)
    }

    #[test]
    fn list_workspaces_succeeds_with_fatal_error_payload() {
        // Discovery stays healthy when config has errors: 0 entries plus a non-null fatal_error.
        let cfg = Config {
            workspaces: Vec::new(),
            config_path: PathBuf::from("/tmp/x"),
            fatal_error: Some("nope".to_string()),
        };
        let r = action_list_workspaces(&cfg).unwrap();
        assert_eq!(r["workspaces"].as_array().unwrap().len(), 0);
        assert_eq!(r["fatal_error"], "nope");
    }

    #[test]
    fn list_workspaces_returns_one() {
        let (_d, cfg) = fixture_with_one_workspace();
        let r = action_list_workspaces(&cfg).unwrap();
        let arr = r["workspaces"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "myrepo");
        assert_eq!(arr[0]["current_branch"], "main");
    }

    #[test]
    fn worktree_add_then_remove_round_trips_via_actions() {
        let (_d, cfg) = fixture_with_one_workspace();
        let r = action_worktree_add(&json!({"workspace": "myrepo", "branch": "feature/x"}), &cfg)
            .unwrap();
        assert_eq!(r["branch"], "feature/x");
        // The completion event is published by the platform ActionRegistry, not this handler, so it
        // is not exercised here.
        let added_path = r["path"].as_str().unwrap().to_string();
        let listed = action_list_worktrees(&json!({"workspace": "myrepo"}), &cfg).unwrap();
        let arr = listed["worktrees"].as_array().unwrap();
        assert!(arr.iter().any(|w| w["branch"] == "feature/x"));
        action_worktree_remove(&json!({"path": added_path}), &cfg).unwrap();
        let listed2 = action_list_worktrees(&json!({"workspace": "myrepo"}), &cfg).unwrap();
        let arr2 = listed2["worktrees"].as_array().unwrap();
        assert!(arr2.iter().all(|w| w["branch"] != "feature/x"));
    }

    #[test]
    fn worktree_add_is_idempotent_and_preserves_prompt_passthrough() {
        let (_d, cfg) = fixture_with_one_workspace();
        let first = action_worktree_add(
            &json!({
                "workspace": "myrepo",
                "branch": "feature/again",
                "prompt": "do the thing",
            }),
            &cfg,
        )
        .unwrap();
        let first_path = first["path"].as_str().unwrap().to_string();
        // Re-click with a fresh prompt: must not error, and must echo the same path.
        let second = action_worktree_add(
            &json!({
                "workspace": "myrepo",
                "branch": "feature/again",
                "prompt": "do the thing again",
            }),
            &cfg,
        )
        .unwrap();
        assert_eq!(second["branch"], "feature/again");
        assert_eq!(second["path"].as_str().unwrap(), first_path);
        assert_eq!(second["prompt"], "do the thing again");
        action_worktree_remove(&json!({"path": first_path}), &cfg).unwrap();
    }

    #[test]
    fn worktree_add_falls_through_when_branch_is_primary_checkout() {
        // The primary checkout appears in `git worktree list`; the idempotent scan must not
        // intercept it, or `worktree_add {branch: "main"}` would surface invalid_state instead of
        // branch_exists.
        let (_d, cfg) = fixture_with_one_workspace();
        let err = action_worktree_add(&json!({"workspace": "myrepo", "branch": "main"}), &cfg)
            .unwrap_err();
        assert!(
            matches!(err.0.as_str(), "branch_exists" | "git_error"),
            "expected branch_exists/git_error, got {err:?}"
        );
    }

    #[test]
    fn worktree_add_falls_through_to_create_when_existing_dir_is_gone() {
        // Stale registry (directory removed without `git worktree prune`): the idempotent path
        // filters
        // `!is_dir()`, and the create call surfaces git's own collision error.
        let (_d, cfg) = fixture_with_one_workspace();
        let first = action_worktree_add(
            &json!({"workspace": "myrepo", "branch": "feature/stale"}),
            &cfg,
        )
        .unwrap();
        let first_path = std::path::PathBuf::from(first["path"].as_str().unwrap());
        std::fs::remove_dir_all(&first_path).unwrap();
        let err = action_worktree_add(
            &json!({"workspace": "myrepo", "branch": "feature/stale"}),
            &cfg,
        )
        .unwrap_err();
        assert!(
            matches!(err.0.as_str(), "branch_exists" | "git_error"),
            "expected branch_exists/git_error from create-path, got {err:?}"
        );
        let ws = &cfg.workspaces[0];
        let _ = std::process::Command::new("git")
            .args(["-C", ws.path.to_str().unwrap(), "worktree", "prune"])
            .output();
    }

    #[test]
    fn worktree_remove_refuses_path_outside_allowlist() {
        let (_d, cfg) = fixture_with_one_workspace();
        let err = action_worktree_remove(&json!({"path": "/tmp/random-thing"}), &cfg).unwrap_err();
        assert_eq!(err.0, "forbidden", "got {err:?}");
    }

    #[test]
    fn worktree_remove_strips_dotdot_from_tail() {
        // `<allowed>/../etc` must not pass starts_with via a verbatim re-attached `..`.
        let (_d, cfg) = fixture_with_one_workspace();
        let ws = &cfg.workspaces[0];
        let escape = format!("{}/../etc", ws.worktree_root.display());
        let err = action_worktree_remove(&json!({"path": escape}), &cfg).unwrap_err();
        assert_eq!(err.0, "forbidden", "got {err:?}");
    }

    #[test]
    fn overlapping_workspace_configs_are_rejected() {
        // Configs whose path/worktree_root sit inside another workspace's are refused at load time,
        // so worktree-ownership resolution is unambiguous.
        let dir = tempdir().unwrap();
        let outer = dir.path().join("outer");
        let inner = outer.join("nested");
        std::fs::create_dir_all(&outer).unwrap();
        std::fs::create_dir_all(&inner).unwrap();
        init_repo(&outer);
        init_repo(&inner);
        let cfg_path = dir.path().join("workspaces.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
[[workspace]]
name = "outer"
path = {o:?}
default_base = "main"

[[workspace]]
name = "inner"
path = {i:?}
default_base = "main"
"#,
                o = outer.display().to_string(),
                i = inner.display().to_string(),
            ),
        )
        .unwrap();
        let cfg = Config::from_path(cfg_path);
        assert_eq!(cfg.workspaces.len(), 1);
        assert_eq!(cfg.workspaces[0].name, "outer");
        assert!(cfg.fatal_error.unwrap().contains("overlaps"));
    }

    #[test]
    fn worktree_add_with_sanitize_jira_lowercases_branch() {
        // `{event.linked_jira}` (e.g. "PROJ-456") with `sanitize_jira = true` lands at
        // `<worktree_root>/proj-456`.
        let (_d, cfg) = fixture_with_one_workspace();
        let r = action_worktree_add(
            &json!({
                "workspace": "myrepo",
                "branch": "PROJ-456",
                "sanitize_jira": true,
            }),
            &cfg,
        )
        .unwrap();
        assert_eq!(r["branch"], "proj-456");
        assert!(
            r["path"].as_str().unwrap().ends_with("/proj-456"),
            "got {}",
            r["path"]
        );
    }

    #[test]
    fn worktree_add_without_sanitize_jira_preserves_case() {
        // Default: the branch is passed through verbatim.
        let (_d, cfg) = fixture_with_one_workspace();
        let r = action_worktree_add(
            &json!({
                "workspace": "myrepo",
                "branch": "Feature-X",
            }),
            &cfg,
        )
        .unwrap();
        assert_eq!(r["branch"], "Feature-X");
    }

    #[test]
    fn worktree_add_rejects_option_shaped_base() {
        // A `base` like "-d" or "--detach" must not reach `git worktree add` where it parses as a
        // flag.
        let (_d, cfg) = fixture_with_one_workspace();
        let err = action_worktree_add(
            &json!({"workspace": "myrepo", "branch": "feat/x", "base": "-d"}),
            &cfg,
        )
        .unwrap_err();
        assert_eq!(err.0, "invalid_params", "got {err:?}");
    }

    #[test]
    fn worktree_remove_accepts_path_under_workspace_path() {
        // The allow-list covers both worktree_root and workspace path. git is not invoked since the
        // path is not a worktree.
        let (_d, cfg) = fixture_with_one_workspace();
        let ws = &cfg.workspaces[0];
        let inside = ws.path.join("nonexistent-leaf");
        let err = action_worktree_remove(&json!({"path": inside.display().to_string()}), &cfg)
            .unwrap_err();
        // Fails because the worktree does not exist, not because the path is forbidden.
        assert_ne!(err.0, "forbidden", "got {err:?}");
    }

    #[test]
    fn worktree_add_rejects_symlinked_intermediate_component() {
        // A symlink at <worktree_root>/feature pointing to /tmp/leak would let `feature/x` pass
        // starts_with while git creates the worktree under /tmp/leak.
        let (_d, cfg) = fixture_with_one_workspace();
        let ws = &cfg.workspaces[0];
        std::fs::create_dir_all(&ws.worktree_root).unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.worktree_root.join("feature")).unwrap();
        let err = action_worktree_add(&json!({"workspace": "myrepo", "branch": "feature/x"}), &cfg)
            .unwrap_err();
        assert_eq!(err.0, "forbidden", "got {err:?}");
    }

    #[test]
    fn worktree_add_rejects_invalid_branch_name() {
        let (_d, cfg) = fixture_with_one_workspace();
        let err = action_worktree_add(&json!({"workspace": "myrepo", "branch": "../escape"}), &cfg)
            .unwrap_err();
        assert_eq!(err.0, "invalid_branch", "got {err:?}");
    }

    #[test]
    fn worktree_add_rejects_unknown_workspace() {
        let (_d, cfg) = fixture_with_one_workspace();
        let err = action_worktree_add(&json!({"workspace": "no-such", "branch": "foo"}), &cfg)
            .unwrap_err();
        assert_eq!(err.0, "not_found");
    }

    #[test]
    fn current_branch_action() {
        let (_d, cfg) = fixture_with_one_workspace();
        let r = action_current_branch(&json!({"workspace": "myrepo"}), &cfg).unwrap();
        assert_eq!(r["branch"], "main");
    }

    #[test]
    fn status_via_workspace_param() {
        let (_d, cfg) = fixture_with_one_workspace();
        let r = action_status(&json!({"workspace": "myrepo"}), &cfg).unwrap();
        assert_eq!(r["dirty"], false);
        assert_eq!(r["branch"], "main");
    }

    #[test]
    fn status_via_path_refuses_outside_root() {
        let (_d, cfg) = fixture_with_one_workspace();
        let err = action_status(&json!({"path": "/etc"}), &cfg).unwrap_err();
        assert_eq!(err.0, "forbidden");
    }

    #[test]
    fn status_rejects_both_workspace_and_path() {
        let (_d, cfg) = fixture_with_one_workspace();
        let err = action_status(&json!({"workspace": "myrepo", "path": "/tmp"}), &cfg).unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn unknown_action_returns_not_found() {
        let cfg = Config {
            workspaces: Vec::new(),
            config_path: PathBuf::from("/tmp/x"),
            fatal_error: None,
        };
        let err = handle_action("git.fly", &Value::Null, &cfg).unwrap_err();
        assert_eq!(err.0, "action_not_found");
    }
}
