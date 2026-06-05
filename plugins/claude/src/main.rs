//! Claude Code state surfacing plugin.
//!
//! Four read-only actions over the on-disk artifacts that the harness
//! hooks (`auto-handoff.sh`, `track-edit.sh`) maintain under `~/.claude/`:
//!
//! - `claude.last_handoff` — returns the most recent handoff doc
//!   (`~/.claude/handoffs/latest.md`) along with its mtime.
//! - `claude.list_sessions` — enumerates transcript files under
//!   `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. Each entry
//!   is `{session_id, project_path, started_at_ms, last_modified_ms}`.
//!   Optional `project_path` param filters to one project's sessions.
//! - `claude.session_state` — for one session id (or the most recently
//!   modified across all projects, if absent), returns the file path,
//!   first/last timestamps, and message count (line count of the jsonl).
//! - `claude.list_dirty` — reads every `~/.claude/state/dirty-*.log`
//!   (one per active session) and returns the unique edited-file list
//!   per session, deduped.
//!
//! Pure-Rust, no external deps beyond serde_json. Resolves `~/.claude`
//! via `$HOME` at startup; tests can override via `$COPAD_CLAUDE_DIR`.
//!
//! All actions are read-only. The plugin never mutates anything under
//! `~/.claude/`. Failures (missing file, permission denied) surface as
//! structured `claude_state_unavailable` errors so callers can handle
//! "harness not installed" gracefully rather than treating it as a crash.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

const PROTOCOL_VERSION: u32 = 1;

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    // Single-writer channel — same pattern as `copad-plugin-echo`. The
    // plugin itself is single-threaded for request handling (no
    // heartbeat), but the writeln-through-channel design keeps the
    // door open for future event publish.
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

    let claude_dir = resolve_claude_dir();

    // Reader loop is single-threaded — no need to share state through
    // Mutex. Each frame's handler reads `claude_dir` by reference.
    let reader = BufReader::new(stdin.lock());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[claude] parse error: {e}");
                continue;
            }
        };
        handle_frame(&value, &writer_tx, &claude_dir);
    }
}

/// Resolve `~/.claude` at startup. Tests pin `COPAD_CLAUDE_DIR` to an
/// isolated temp tree so they don't read the developer's actual
/// session history. Production callers should never set it.
fn resolve_claude_dir() -> PathBuf {
    if let Ok(p) = std::env::var("COPAD_CLAUDE_DIR")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude")
}

fn handle_frame(value: &Value, tx: &Sender<String>, claude_dir: &Path) {
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    let params = value.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let proto = params.get("protocol_version").and_then(Value::as_u64);
            if proto != Some(PROTOCOL_VERSION as u64) {
                send_error(
                    tx,
                    id,
                    "protocol_mismatch",
                    &format!(
                        "claude plugin only speaks protocol {PROTOCOL_VERSION}; got {proto:?}"
                    ),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": [
                        "claude.session_state",
                        "claude.list_dirty",
                        "claude.last_handoff",
                        "claude.list_sessions",
                    ],
                    "subscribes": [],
                }),
            );
        }
        "initialized" => {}
        "action.invoke" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let action_params = params.get("params").cloned().unwrap_or(Value::Null);
            let dir = claude_dir;
            match name {
                "claude.last_handoff" => match last_handoff(dir) {
                    Ok(v) => send_response(tx, id, v),
                    Err(e) => send_error(tx, id, &e.code, &e.message),
                },
                "claude.list_sessions" => {
                    match optional_string_param(&action_params, "project_path") {
                        Ok(project) => match list_sessions(dir, project.as_deref()) {
                            Ok(v) => send_response(tx, id, v),
                            Err(e) => send_error(tx, id, &e.code, &e.message),
                        },
                        Err(e) => send_error(tx, id, &e.code, &e.message),
                    }
                }
                "claude.session_state" => {
                    match optional_string_param(&action_params, "session_id") {
                        Ok(session_id) => match session_state(dir, session_id.as_deref()) {
                            Ok(v) => send_response(tx, id, v),
                            Err(e) => send_error(tx, id, &e.code, &e.message),
                        },
                        Err(e) => send_error(tx, id, &e.code, &e.message),
                    }
                }
                "claude.list_dirty" => match list_dirty(dir) {
                    Ok(v) => send_response(tx, id, v),
                    Err(e) => send_error(tx, id, &e.code, &e.message),
                },
                other => send_error(
                    tx,
                    id,
                    "action_not_found",
                    &format!("claude plugin does not handle {other}"),
                ),
            }
        }
        "event.dispatch" => {}
        "shutdown" => std::process::exit(0),
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                tx,
                id,
                "unknown_method",
                &format!("claude plugin: unknown method {other}"),
            );
        }
        _ => {}
    }
}

#[derive(Debug)]
struct ActionError {
    code: String,
    message: String,
}

fn err(code: &str, message: impl Into<String>) -> ActionError {
    ActionError {
        code: code.into(),
        message: message.into(),
    }
}

/// Strict optional-string param extractor. Requires `params` to be
/// either `null` (no params) or a JSON object — a top-level non-object
/// (e.g. `42` or `[]`) yields `invalid_params` rather than silently
/// reading "no such key" and returning broader results than intended.
/// Within an object: absent or `null` value → `Ok(None)`; string →
/// `Ok(Some(...))`; non-string → `invalid_params`.
fn optional_string_param(params: &Value, key: &str) -> Result<Option<String>, ActionError> {
    let obj = match params {
        Value::Null => return Ok(None),
        Value::Object(map) => map,
        other => {
            return Err(err(
                "invalid_params",
                format!("params must be an object, got {other}"),
            ));
        }
    };
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(err(
            "invalid_params",
            format!("`{key}` must be a string, got {other}"),
        )),
    }
}

/// `~/.claude/handoffs/latest.md` if present.
fn last_handoff(claude_dir: &Path) -> Result<Value, ActionError> {
    let path = claude_dir.join("handoffs").join("latest.md");
    let content = fs::read_to_string(&path).map_err(|e| {
        err(
            "claude_state_unavailable",
            format!("handoff read failed ({}): {e}", path.display()),
        )
    })?;
    let modified_ms = file_modified_ms(&path);
    Ok(json!({
        "path": path.to_string_lossy(),
        "content": content,
        "modified_at_ms": modified_ms,
    }))
}

/// Enumerate `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
/// `project_filter` matches against the encoded-cwd dirname, NOT the
/// decoded path — callers should pass the dir name as it appears on
/// disk (e.g. `-Users-marshallku-dev-copad`). Sorted newest-first by
/// `last_modified_ms`.
fn list_sessions(claude_dir: &Path, project_filter: Option<&str>) -> Result<Value, ActionError> {
    let projects = claude_dir.join("projects");
    let project_dirs = match fs::read_dir(&projects) {
        Ok(it) => it,
        Err(e) => {
            return Err(err(
                "claude_state_unavailable",
                format!("projects dir not readable ({}): {e}", projects.display()),
            ));
        }
    };
    let mut sessions: Vec<Value> = Vec::new();
    for project_entry in project_dirs.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }
        let project_name = project_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if let Some(filter) = project_filter
            && project_name != filter
        {
            continue;
        }
        let Ok(entries) = fs::read_dir(&project_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str) != Some("jsonl") {
                continue;
            }
            let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let modified_ms = file_modified_ms(&path);
            sessions.push(json!({
                "session_id": session_id,
                "project_path": project_name,
                "session_file": path.to_string_lossy(),
                "last_modified_ms": modified_ms,
            }));
        }
    }
    // Newest first — most use cases want "what was I doing recently".
    sessions.sort_by(|a, b| {
        b.get("last_modified_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .cmp(&a.get("last_modified_ms").and_then(Value::as_u64).unwrap_or(0))
    });
    Ok(json!({
        "count": sessions.len(),
        "sessions": sessions,
    }))
}

/// For one session id (or the most recently modified one across all
/// projects), return file path + first/last timestamps + message
/// count (jsonl line count). The transcripts are append-only one-
/// JSON-object-per-line so line count == message count.
fn session_state(claude_dir: &Path, session_id: Option<&str>) -> Result<Value, ActionError> {
    let projects = claude_dir.join("projects");
    let project_dirs = fs::read_dir(&projects).map_err(|e| {
        err(
            "claude_state_unavailable",
            format!("projects dir not readable ({}): {e}", projects.display()),
        )
    })?;
    let mut best: Option<(PathBuf, u64)> = None;
    for project_entry in project_dirs.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&project_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str) != Some("jsonl") {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let modified = file_modified_ms(&path);
            if let Some(target) = session_id {
                if stem == target {
                    best = Some((path.clone(), modified));
                    break;
                }
            } else if best.as_ref().is_none_or(|(_, m)| modified > *m) {
                best = Some((path.clone(), modified));
            }
        }
        if session_id.is_some() && best.is_some() {
            break;
        }
    }
    let Some((path, modified_ms)) = best else {
        let msg = match session_id {
            Some(id) => format!("no transcript found for session {id}"),
            None => "no sessions found".into(),
        };
        return Err(err("claude_session_not_found", msg));
    };
    let message_count = count_lines(&path).map_err(|e| {
        err(
            "claude_state_unavailable",
            format!("transcript read failed ({}): {e}", path.display()),
        )
    })?;
    let project_path = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let session_id_out = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    Ok(json!({
        "session_id": session_id_out,
        "project_path": project_path,
        "session_file": path.to_string_lossy(),
        "last_modified_ms": modified_ms,
        "message_count": message_count,
    }))
}

/// Enumerate `~/.claude/state/dirty-<session-id>.log`. Each file is a
/// newline-separated list of edited file paths (`track-edit.sh`
/// appends one line per Edit/Write). Returns a dedup'd entry per
/// session.
fn list_dirty(claude_dir: &Path) -> Result<Value, ActionError> {
    let state = claude_dir.join("state");
    let entries = match fs::read_dir(&state) {
        Ok(it) => it,
        Err(e) => {
            return Err(err(
                "claude_state_unavailable",
                format!("state dir not readable ({}): {e}", state.display()),
            ));
        }
    };
    let mut sessions: Vec<Value> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(session_id) = name.strip_prefix("dirty-").and_then(|s| s.strip_suffix(".log"))
        else {
            continue;
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let unique: Vec<String> = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect::<HashSet<String>>()
            .into_iter()
            .collect();
        let count = unique.len();
        sessions.push(json!({
            "session_id": session_id,
            "edited_files": unique,
            "count": count,
        }));
    }
    Ok(json!({
        "count": sessions.len(),
        "sessions": sessions,
    }))
}

fn file_modified_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn count_lines(path: &Path) -> std::io::Result<usize> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut n = 0usize;
    for line in reader.lines() {
        if line?.is_empty() {
            continue;
        }
        n += 1;
    }
    Ok(n)
}

fn send_response(tx: &Sender<String>, id: &str, result: Value) {
    let frame = json!({ "id": id, "ok": true, "result": result });
    let _ = tx.send(frame.to_string());
}

fn send_error(tx: &Sender<String>, id: &str, code: &str, message: &str) {
    let frame = json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message }
    });
    let _ = tx.send(frame.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    static SUFFIX: AtomicU64 = AtomicU64::new(0);

    fn tmpdir() -> PathBuf {
        let n = SUFFIX.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "copad-claude-test-{}-{}-{n}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn last_handoff_returns_content_and_mtime() {
        let dir = tmpdir();
        let handoffs = dir.join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        fs::write(handoffs.join("latest.md"), "# Hello\n").unwrap();
        let out = last_handoff(&dir).unwrap();
        assert_eq!(out["content"], "# Hello\n");
        assert!(out["modified_at_ms"].as_u64().unwrap() > 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_handoff_returns_error_when_missing() {
        let dir = tmpdir();
        let err = last_handoff(&dir).unwrap_err();
        assert_eq!(err.code, "claude_state_unavailable");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_sessions_walks_project_dirs_newest_first() {
        let dir = tmpdir();
        let p1 = dir.join("projects").join("-project-A");
        let p2 = dir.join("projects").join("-project-B");
        fs::create_dir_all(&p1).unwrap();
        fs::create_dir_all(&p2).unwrap();
        fs::write(p1.join("aaa.jsonl"), "{}").unwrap();
        // Sleep so the second file's mtime is strictly later (mtime
        // is ms-resolution on macOS / Linux, but a 5ms gap is safe).
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(p2.join("bbb.jsonl"), "{}").unwrap();
        let out = list_sessions(&dir, None).unwrap();
        assert_eq!(out["count"], 2);
        let sessions = out["sessions"].as_array().unwrap();
        // Newest first.
        assert_eq!(sessions[0]["session_id"], "bbb");
        assert_eq!(sessions[1]["session_id"], "aaa");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_sessions_filters_by_project_dirname() {
        let dir = tmpdir();
        let p1 = dir.join("projects").join("-project-A");
        let p2 = dir.join("projects").join("-project-B");
        fs::create_dir_all(&p1).unwrap();
        fs::create_dir_all(&p2).unwrap();
        fs::write(p1.join("aaa.jsonl"), "{}").unwrap();
        fs::write(p2.join("bbb.jsonl"), "{}").unwrap();
        let out = list_sessions(&dir, Some("-project-A")).unwrap();
        assert_eq!(out["count"], 1);
        assert_eq!(out["sessions"][0]["session_id"], "aaa");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_state_returns_message_count() {
        let dir = tmpdir();
        let proj = dir.join("projects").join("-proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("sess1.jsonl"), "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
        let out = session_state(&dir, Some("sess1")).unwrap();
        assert_eq!(out["session_id"], "sess1");
        assert_eq!(out["message_count"], 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_state_without_id_picks_most_recent() {
        let dir = tmpdir();
        let proj = dir.join("projects").join("-proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("old.jsonl"), "{}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(proj.join("new.jsonl"), "{}\n").unwrap();
        let out = session_state(&dir, None).unwrap();
        assert_eq!(out["session_id"], "new");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_state_unknown_id_returns_not_found() {
        let dir = tmpdir();
        let proj = dir.join("projects").join("-proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("real.jsonl"), "{}").unwrap();
        let err = session_state(&dir, Some("ghost")).unwrap_err();
        assert_eq!(err.code, "claude_session_not_found");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn optional_string_param_strict_rejects_non_string() {
        // C2 fix: a number/bool/etc passed for a string param must
        // not be silently coerced to None — return invalid_params.
        let p = json!({ "project_path": 42 });
        let e = optional_string_param(&p, "project_path").unwrap_err();
        assert_eq!(e.code, "invalid_params");
        // Absent / null = None (still treated as unset).
        assert!(optional_string_param(&json!({}), "x").unwrap().is_none());
        assert!(optional_string_param(&json!({"x": null}), "x").unwrap().is_none());
        // Whole-params null is treated as "no params" — None for any key.
        assert!(optional_string_param(&Value::Null, "x").unwrap().is_none());
        // String passes through.
        assert_eq!(
            optional_string_param(&json!({"x": "y"}), "x").unwrap(),
            Some("y".into())
        );
    }

    #[test]
    fn optional_string_param_rejects_non_object_top_level() {
        // C1 round-2 fix: a top-level non-object/null params (e.g.
        // `42` or `[]`) used to be silently treated as "no key here"
        // and broaden the query. Now it surfaces as invalid_params.
        let e = optional_string_param(&json!(42), "x").unwrap_err();
        assert_eq!(e.code, "invalid_params");
        let e = optional_string_param(&json!([]), "x").unwrap_err();
        assert_eq!(e.code, "invalid_params");
        let e = optional_string_param(&json!("hello"), "x").unwrap_err();
        assert_eq!(e.code, "invalid_params");
    }

    #[test]
    fn session_state_propagates_transcript_read_error() {
        // C1 fix: an unreadable transcript path surfaces as
        // claude_state_unavailable, not message_count = 0.
        // We simulate "unreadable" by making the file a directory
        // (read_to_string / open will EISDIR).
        let dir = tmpdir();
        let proj = dir.join("projects").join("-proj");
        fs::create_dir_all(&proj).unwrap();
        // Sub-dir at the jsonl name — opening it as a file fails.
        let weird = proj.join("brokensess.jsonl");
        fs::create_dir(&weird).unwrap();
        let e = session_state(&dir, Some("brokensess")).unwrap_err();
        assert_eq!(e.code, "claude_state_unavailable");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_dirty_dedupes_within_session_log() {
        let dir = tmpdir();
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("dirty-sess-A.log"),
            "/a.rs\n/b.rs\n/a.rs\n/c.rs\n/a.rs\n",
        )
        .unwrap();
        fs::write(state.join("dirty-sess-B.log"), "/x.rs\n").unwrap();
        // Non-matching file: ignored.
        fs::write(state.join("not-a-dirty-log.txt"), "ignored").unwrap();
        let out = list_dirty(&dir).unwrap();
        assert_eq!(out["count"], 2);
        let sessions = out["sessions"].as_array().unwrap();
        let sess_a = sessions
            .iter()
            .find(|s| s["session_id"] == "sess-A")
            .unwrap();
        assert_eq!(sess_a["count"], 3); // /a.rs, /b.rs, /c.rs (deduped)
        let _ = fs::remove_dir_all(&dir);
    }
}
