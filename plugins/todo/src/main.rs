//! First-party Todo service plugin for copad.
//!
//! Markdown-checkbox files at `~/docs/todos/<workspace>/<id>.md`
//! with YAML-ish frontmatter as the source of truth — vim and git
//! are first-class clients alongside this binary's actions.
//!
//! Architecture mirrors the calendar plugin: a stdio RPC loop on
//! the main thread plus a background poller thread that emits
//! file-watcher events. We intentionally do NOT delegate writes
//! through `kb.ensure` (an inter-plugin RPC) — the atomic-create
//! primitives are 30 lines of libc and rolling them in-crate is
//! cheaper than the build-graph entanglement.
//!
//! Activation `onStartup` (rather than `onAction:todo.*`) so the
//! file-watcher is alive whenever copad is running. Otherwise an
//! external `vim` edit only surfaces as `todo.changed` after the
//! first user-initiated action, which would silently break
//! triggers like `todo.completed → slack.post_message`.
//!
//! Unix-only (Linux + macOS) — `store.rs` needs `O_NOFOLLOW` plus a
//! kernel atomic-create-or-fail primitive, both of which we get via
//! `copad_core::fs_atomic`.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "copad-plugin-todo supports Linux and macOS. Other Unixes need a \
     backend-specific atomic-create primitive — extend copad_core::fs_atomic."
);

mod config;
mod loop_prompt;
mod prompt;
mod store;
mod todo;
mod watcher;

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use serde_json::{Value, json};

use config::Config;
use store::Store;
use todo::{Priority, Status};
use watcher::Watcher;

const PROTOCOL_VERSION: u32 = 1;

fn main() {
    let config = Arc::new(Config::from_env());
    if let Some(err) = &config.fatal_error {
        eprintln!("[todo] config error (actions will return config_error): {err}");
    }
    eprintln!(
        "[todo] root = {}, default_workspace = {}",
        config.root.display(),
        config.default_workspace
    );

    // Even with a fatal_error we still try to open the store —
    // the error already came from env validation, not the
    // filesystem, so the store itself is fine. If `Store::new`
    // ALSO fails (e.g. unwriteable docs dir), that becomes a
    // second-level fatal_error and actions error out uniformly.
    let store_result = Store::new(config.root.clone());
    let (store, store_error): (Option<Arc<Store>>, Option<String>) = match store_result {
        Ok(s) => (Some(Arc::new(s)), None),
        Err(e) => {
            let (code, msg) = e.code_message();
            eprintln!("[todo] store init failed: {code}/{msg}");
            (None, Some(format!("{code}: {msg}")))
        }
    };

    let stdin = std::io::stdin();
    // Direct-write Stdout wrapped in a Mutex. Replaces the previous
    // mpsc + writer-thread design — that pattern leaked frames on
    // shutdown because the writer thread had no bounded drain
    // window inside the supervisor's 200ms SIGKILL grace. Each
    // producer (handle_frame, Watcher) now acquires the Mutex,
    // writes, flushes, and releases. There's no queue, so a hard
    // process::exit doesn't have any "in-flight" frames to lose —
    // every emit is fully committed to the stdout buffer +
    // flushed to the parent pipe before its caller returns.
    let writer: Writer = Arc::new(Mutex::new(Box::new(std::io::stdout())));

    let initialized = Arc::new(AtomicBool::new(false));
    // `shutdown` lets the watcher exit promptly on stdin EOF /
    // `shutdown` method, instead of one full poll cycle later.
    // Cooperative shutdown semantics; on hard SIGKILL we still
    // exit immediately, but no frames are lost because nothing is
    // queued.
    let shutdown = Arc::new(AtomicBool::new(false));

    let watcher_handle = if let Some(s) = &store {
        let watcher = Watcher::new(
            config.clone(),
            s.clone(),
            writer.clone(),
            initialized.clone(),
            shutdown.clone(),
        );
        Some(thread::spawn(move || watcher.run()))
    } else {
        None
    };

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
                eprintln!("[todo] parse error: {e}");
                continue;
            }
        };
        handle_frame(
            &frame,
            &writer,
            &initialized,
            &shutdown,
            &config,
            store.as_ref(),
            store_error.as_deref(),
        );
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
    }

    // Cooperative shutdown: signal + JOIN. The direct-write model
    // means each `emit()` call commits its frame end-to-end before
    // returning, but a watcher mid-emit (between writeln and flush
    // — yes, our lock spans both, but at the syscall-issued layer
    // there's still a brief window) when main returns would be
    // killed by the OS process tear-down. Joining the watcher
    // ensures its current iteration (sleep poll + optional scan +
    // any emits) fully completes before we return. The watcher
    // notices the flag within ~100ms during its chunked sleep, so
    // worst-case shutdown is ~100ms + one scan duration — well
    // inside the supervisor's 200ms SIGKILL grace for typical
    // workspaces.
    shutdown.store(true, Ordering::SeqCst);
    if let Some(h) = watcher_handle {
        let _ = h.join();
    }
}

/// Outer Mutex (over Stdout's own internal one) so we can hold the lock
/// across `writeln!` + `flush` — otherwise a producer preempted between
/// the two could interleave with another thread's writeln and corrupt
/// the line-delimited protocol. Tests inject a `Vec<u8>` via the same shape.
pub type Writer = Arc<Mutex<Box<dyn std::io::Write + Send>>>;

/// Atomic line emission. Stdout-write errors are logged but not
/// propagated — supervisor detects via EOF on the read side.
pub fn emit(writer: &Writer, line: &str) {
    let mut out = match writer.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Err(e) = writeln!(out, "{line}") {
        eprintln!("[todo] writeln failed: {e}");
        return;
    }
    if let Err(e) = out.flush() {
        eprintln!("[todo] flush failed: {e}");
    }
}

fn handle_frame(
    frame: &Value,
    writer: &Writer,
    initialized: &AtomicBool,
    shutdown: &AtomicBool,
    config: &Config,
    store: Option<&Arc<Store>>,
    store_error: Option<&str>,
) {
    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    let id = frame.get("id").and_then(Value::as_str).unwrap_or("");
    let params = frame.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let proto = params.get("protocol_version").and_then(Value::as_u64);
            if proto != Some(PROTOCOL_VERSION as u64) {
                send_error(
                    writer,
                    id,
                    "protocol_mismatch",
                    &format!("todo plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                writer,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": [
                        "todo.create",
                        "todo.list",
                        "todo.update",
                        "todo.set_status",
                        "todo.start",
                        "todo.delete",
                        "todo.render_loop_prompt",
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
            let result = handle_action(&name, &action_params, config, store, store_error, writer);
            match result {
                Ok(v) => send_response(writer, id, v),
                Err((code, msg)) => send_error(writer, id, &code, &msg),
            }
        }
        "event.dispatch" => {
            // No subscriptions — quietly ignore.
        }
        "shutdown" => {
            // Don't process::exit here — that hard-kills the
            // writer thread mid-flush and loses queued frames. Set
            // the rendezvous flag and let the read loop break, so
            // main's drain path joins the writer cleanly. Process
            // exits naturally when main returns.
            shutdown.store(true, Ordering::SeqCst);
        }
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                writer,
                id,
                "unknown_method",
                &format!("todo plugin: unknown method {other}"),
            );
        }
        _ => {}
    }
}

fn handle_action(
    name: &str,
    params: &Value,
    config: &Config,
    store: Option<&Arc<Store>>,
    store_error: Option<&str>,
    writer: &Writer,
) -> Result<Value, (String, String)> {
    if let Some(err) = &config.fatal_error {
        return Err(("config_error".into(), err.clone()));
    }
    let store = match store {
        Some(s) => s,
        None => {
            return Err((
                "config_error".into(),
                store_error
                    .map(str::to_string)
                    .unwrap_or_else(|| "todo store not initialized".to_string()),
            ));
        }
    };
    match name {
        "todo.create" => action_create(params, config, store).map(|t| {
            json!({
                "id": t.id,
                "workspace": t.workspace,
                "todo": t.to_json(),
            })
        }),
        "todo.list" => action_list(params, store),
        "todo.update" => action_update(params, config, store),
        "todo.set_status" => action_set_status(params, config, store),
        "todo.start" => action_start(params, config, store, writer),
        "todo.delete" => action_delete(params, config, store),
        "todo.render_loop_prompt" => action_render_loop_prompt(params, config, store),
        other => Err((
            "action_not_found".into(),
            format!("todo plugin does not handle {other}"),
        )),
    }
}

fn action_create(
    params: &Value,
    config: &Config,
    store: &Arc<Store>,
) -> Result<crate::todo::Todo, (String, String)> {
    let workspace = string_param_or_default(params, "workspace", &config.default_workspace)?;
    let title = required_string(params, "title")?;
    let body = optional_string(params, "body")?.unwrap_or_default();
    let priority = match optional_string(params, "priority")? {
        Some(s) => Priority::parse(&s).ok_or_else(|| {
            (
                "invalid_params".to_string(),
                format!("priority {s:?} not in {{low, normal, high}}"),
            )
        })?,
        None => Priority::Normal,
    };
    let due = optional_string(params, "due")?;
    let id = optional_string(params, "id")?;
    let linked_jira = optional_string(params, "linked_jira")?;
    let linked_slack = optional_array(params, "linked_slack")?;
    let linked_kb = optional_string_array(params, "linked_kb")?;
    let tags = optional_string_array(params, "tags")?;
    let prompt = optional_string(params, "prompt")?;

    store
        .create(
            &workspace,
            id,
            &title,
            &body,
            priority,
            due,
            linked_jira,
            linked_slack,
            linked_kb,
            tags,
            prompt,
        )
        .map_err(|e| {
            let (c, m) = e.code_message();
            (c.to_string(), m)
        })
}

fn action_list(params: &Value, store: &Arc<Store>) -> Result<Value, (String, String)> {
    let workspace_filter = optional_string(params, "workspace")?;
    let status_filter = match optional_string(params, "status")? {
        Some(s) => Some(Status::parse(&s).ok_or_else(|| {
            (
                "invalid_params".to_string(),
                format!("status {s:?} not in {{open, in_progress, blocked, done}}"),
            )
        })?),
        None => None,
    };
    let due_before = optional_string(params, "due_before")?;
    let tag_filter = optional_string(params, "tag")?;
    let mut todos = store.list_all(workspace_filter.as_deref()).map_err(|e| {
        let (c, m) = e.code_message();
        (c.to_string(), m)
    })?;
    if let Some(target) = status_filter {
        todos.retain(|t| t.status == target);
    }
    if let Some(due_max) = &due_before {
        todos.retain(|t| match &t.due {
            Some(d) => d.as_str() < due_max.as_str(),
            None => false,
        });
    }
    if let Some(tag) = &tag_filter {
        todos.retain(|t| t.tags.iter().any(|x| x == tag));
    }
    // Stable order — by status (open first), then due, then id.
    // Easier-to-read panel without UI sorting.
    todos.sort_by(|a, b| {
        let order = |s: Status| match s {
            Status::InProgress => 0,
            Status::Open => 1,
            Status::Blocked => 2,
            Status::Done => 3,
        };
        order(a.status)
            .cmp(&order(b.status))
            .then_with(|| {
                a.due
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.due.as_deref().unwrap_or(""))
            })
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(json!({
        "todos": todos.iter().map(crate::todo::Todo::to_json).collect::<Vec<_>>(),
    }))
}

/// "Field absent ⇒ leave alone"; for `due`/`linked_jira`/`prompt`,
/// empty-string or null clears. `status` is deliberately routed
/// through `todo.set_status` instead — `update` regenerates via
/// `render_new` (drops user frontmatter ordering/comments), which
/// is right for form edits but wrong for the status-toggle path.
fn action_update(
    params: &Value,
    config: &Config,
    store: &Arc<Store>,
) -> Result<Value, (String, String)> {
    let workspace = string_param_or_default(params, "workspace", &config.default_workspace)?;
    let id = required_string(params, "id")?;
    let title = optional_present_string(params, "title")?;
    let body = match params.get("body") {
        None => None,
        Some(Value::Null) => Some(String::new()),
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            return Err((
                "invalid_params".into(),
                format!("'body' must be a string, got {other}"),
            ));
        }
    };
    // optional_present_string rejects an explicit empty string — passing
    // `append_subtask: ""` would otherwise silently no-op AND bypass the
    // mutual-exclusion check with `body`. If present, it must be non-empty.
    let append_subtask = optional_present_string(params, "append_subtask")?;
    let priority = match optional_string(params, "priority")? {
        Some(s) => Some(Priority::parse(&s).ok_or_else(|| {
            (
                "invalid_params".to_string(),
                format!("priority {s:?} not in {{low, normal, high}}"),
            )
        })?),
        None => None,
    };
    let due = optional_clearable_string(params, "due")?;
    let linked_jira = optional_clearable_string(params, "linked_jira")?;
    let linked_kb = match params.get("linked_kb") {
        None => None,
        Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(arr)) => Some(
            arr.iter()
                .map(|v| {
                    v.as_str().map(str::to_string).ok_or((
                        "invalid_params".to_string(),
                        "'linked_kb' entries must be strings".to_string(),
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(other) => {
            return Err((
                "invalid_params".into(),
                format!("'linked_kb' must be an array, got {other}"),
            ));
        }
    };
    let tags = match params.get("tags") {
        None => None,
        Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(arr)) => Some(
            arr.iter()
                .map(|v| {
                    v.as_str().map(str::to_string).ok_or((
                        "invalid_params".to_string(),
                        "'tags' entries must be strings".to_string(),
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(other) => {
            return Err((
                "invalid_params".into(),
                format!("'tags' must be an array, got {other}"),
            ));
        }
    };
    let prompt = optional_clearable_string(params, "prompt")?;

    let updated = store
        .update(
            &workspace,
            &id,
            title,
            body,
            append_subtask,
            priority,
            due,
            linked_jira,
            linked_kb,
            tags,
            prompt,
        )
        .map_err(|e| {
            let (c, m) = e.code_message();
            (c.to_string(), m)
        })?;
    Ok(json!({
        "id": id,
        "workspace": workspace,
        "todo": updated.to_json(),
    }))
}

fn action_set_status(
    params: &Value,
    config: &Config,
    store: &Arc<Store>,
) -> Result<Value, (String, String)> {
    let workspace = string_param_or_default(params, "workspace", &config.default_workspace)?;
    let id = required_string(params, "id")?;
    let status_str = required_string(params, "status")?;
    let new_status = Status::parse(&status_str).ok_or_else(|| {
        (
            "invalid_params".to_string(),
            format!("status {status_str:?} not in {{open, in_progress, blocked, done}}"),
        )
    })?;
    let (prev, next) = store.set_status(&workspace, &id, new_status).map_err(|e| {
        let (c, m) = e.code_message();
        (c.to_string(), m)
    })?;
    Ok(json!({
        "id": id,
        "workspace": workspace,
        "previous_status": prev.as_str(),
        "status": next.as_str(),
    }))
}

/// Emits `todo.start_requested` with the full Todo payload. Users
/// can chain it (e.g. `→ git.worktree_add → claude.start` via
/// completion-event triggers, or single-step `→ kb.ensure` /
/// `→ slack.post_message`); the action itself is fire-and-forget.
fn action_start(
    params: &Value,
    config: &Config,
    store: &Arc<Store>,
    writer: &Writer,
) -> Result<Value, (String, String)> {
    let workspace = string_param_or_default(params, "workspace", &config.default_workspace)?;
    let id = required_string(params, "id")?;
    let todo = store.read(&workspace, &id).map_err(|e| {
        let (c, m) = e.code_message();
        (c.to_string(), m)
    })?;
    let docs_root = prompt::docs_root_for(&config.root);
    let assembled_prompt = prompt::assemble(&todo, docs_root.as_deref());
    let mut payload = todo.to_json();
    // Add the layered prompt as a NEW field rather than overwriting
    // the raw `prompt` frontmatter field. Two surfaces, two meanings:
    // - `payload.prompt`        = the user's literal frontmatter
    //                              `prompt` (None / explicit string).
    // - `payload.assembled_prompt` = global preamble + workspace
    //                              preamble + Todo prompt-or-body +
    //                              linked_kb fan-in, late-bound at
    //                              start time. This is what trigger
    //                              chains feed to claude.start.
    // Keeping both means trigger interpolation can pick either — and
    // a future consumer that wants raw input can still read `prompt`
    // without rebuilding the assembly itself.
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "assembled_prompt".to_string(),
            Value::String(assembled_prompt.clone()),
        );
    }
    let frame = json!({
        "method": "event.publish",
        "params": {
            "kind": "todo.start_requested",
            "payload": payload.clone(),
        }
    });
    emit(writer, &frame.to_string());
    Ok(json!({
        "id": id,
        "workspace": workspace,
        "todo": payload,
        "assembled_prompt": assembled_prompt,
    }))
}

fn action_delete(
    params: &Value,
    config: &Config,
    store: &Arc<Store>,
) -> Result<Value, (String, String)> {
    let workspace = string_param_or_default(params, "workspace", &config.default_workspace)?;
    let id = required_string(params, "id")?;
    store.delete(&workspace, &id).map_err(|e| {
        let (c, m) = e.code_message();
        (c.to_string(), m)
    })?;
    Ok(json!({ "id": id, "workspace": workspace }))
}

/// Render the autonomous-loop prompt for an existing todo.
///
/// The rendered protocol addresses the todo by id+workspace directly,
/// so the `loop` tag is NOT required for this specific path to work.
/// We auto-add it anyway for two reasons: (1) `coctl todo list --tag
/// loop` becomes a useful filter for finding active/historical agent
/// loops, and (2) the parallel manual-fill template at
/// `~/.claude/loop-template.md` uses `(workspace, tag=loop, title)` for
/// its bootstrap lookup, so a todo created via this action can be
/// resumed via the manual flow later without re-tagging by hand.
///
/// Returns `loop_tag_added: true` only when this call performed the tag
/// mutation (so callers can surface "we changed something" vs "already
/// tagged, no-op"); the post-call state is "todo has `loop`" either way.
fn action_render_loop_prompt(
    params: &Value,
    config: &Config,
    store: &Arc<Store>,
) -> Result<Value, (String, String)> {
    let workspace = string_param_or_default(params, "workspace", &config.default_workspace)?;
    let id = required_string(params, "id")?;
    let mut t = store.read(&workspace, &id).map_err(|e| {
        let (c, m) = e.code_message();
        (c.to_string(), m)
    })?;
    let already_tagged = t.tags.iter().any(|tag| tag == "loop");
    if !already_tagged {
        let mut new_tags = t.tags.clone();
        new_tags.push("loop".into());
        t = store
            .update(
                &workspace,
                &id,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(new_tags),
                None,
            )
            .map_err(|e| {
                let (c, m) = e.code_message();
                (c.to_string(), m)
            })?;
    }
    let prompt = loop_prompt::render(&t.title, &workspace, &id);
    Ok(json!({
        "id": id,
        "workspace": workspace,
        "loop_tag_added": !already_tagged,
        "prompt": prompt,
    }))
}

// -- param helpers --

fn required_string(params: &Value, key: &str) -> Result<String, (String, String)> {
    let v = params.get(key).ok_or_else(|| {
        (
            "invalid_params".to_string(),
            format!("missing required field {key:?}"),
        )
    })?;
    v.as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            (
                "invalid_params".to_string(),
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

/// Like `optional_string` but `None` = "absent, don't touch" and
/// `Some("")` = "user supplied empty" (rejected at the store layer
/// for `title`). Clearable fields use `optional_clearable_string` instead.
fn optional_present_string(params: &Value, key: &str) -> Result<Option<String>, (String, String)> {
    match params.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(String::new())),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err((
            "invalid_params".into(),
            format!("{key:?} must be a string, got {other}"),
        )),
    }
}

/// JSON → clearable-string mapping: missing key → `None`; `null` or
/// `""` → `Some(None)` (clear); non-empty → `Some(Some(s))` (set).
fn optional_clearable_string(
    params: &Value,
    key: &str,
) -> Result<Option<Option<String>>, (String, String)> {
    match params.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) if s.is_empty() => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(other) => Err((
            "invalid_params".into(),
            format!("{key:?} must be a string, got {other}"),
        )),
    }
}

fn string_param_or_default(
    params: &Value,
    key: &str,
    default: &str,
) -> Result<String, (String, String)> {
    Ok(optional_string(params, key)?.unwrap_or_else(|| default.to_string()))
}

fn optional_array(params: &Value, key: &str) -> Result<Vec<Value>, (String, String)> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(other) => Err((
            "invalid_params".into(),
            format!("{key:?} must be an array, got {other}"),
        )),
    }
}

fn optional_string_array(params: &Value, key: &str) -> Result<Vec<String>, (String, String)> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or((
                    "invalid_params".to_string(),
                    format!("{key:?} entries must be strings"),
                ))
            })
            .collect(),
        Some(other) => Err((
            "invalid_params".into(),
            format!("{key:?} must be an array, got {other}"),
        )),
    }
}

fn send_response(writer: &Writer, id: &str, result: Value) {
    let frame = json!({ "id": id, "ok": true, "result": result });
    emit(writer, &frame.to_string());
}

fn send_error(writer: &Writer, id: &str, code: &str, message: &str) {
    let frame = json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message },
    });
    emit(writer, &frame.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Test-only `Writer` backend that captures into a shared `Vec<u8>`
    /// so tests can read back the line-delimited frames.
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

    type Fixture = (
        tempfile::TempDir,
        Config,
        Arc<Store>,
        Writer,
        Arc<Mutex<Vec<u8>>>,
    );

    fn fixture() -> Fixture {
        let dir = tempdir().unwrap();
        let config = Config {
            root: dir.path().join("todos"),
            default_workspace: "default".into(),
            poll_interval: std::time::Duration::from_secs(2),
            fatal_error: None,
        };
        let store = Arc::new(Store::new(config.root.clone()).unwrap());
        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer: Writer = Arc::new(Mutex::new(Box::new(TestSink(captured.clone()))));
        (dir, config, store, writer, captured)
    }

    #[test]
    fn create_then_list_returns_one() {
        let (_d, config, store, _tx, _rx) = fixture();
        let created = action_create(
            &json!({"title": "ship 15.1", "priority": "high"}),
            &config,
            &store,
        )
        .unwrap();
        assert_eq!(created.title, "ship 15.1");
        assert_eq!(created.priority, Priority::High);
        let listed = action_list(&Value::Null, &store).unwrap();
        let arr = listed["todos"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "ship 15.1");
    }

    #[test]
    fn create_rejects_empty_title() {
        let (_d, config, store, _tx, _rx) = fixture();
        let err = action_create(&json!({"title": ""}), &config, &store).unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn create_rejects_unknown_priority() {
        let (_d, config, store, _tx, _rx) = fixture();
        let err = action_create(
            &json!({"title": "x", "priority": "urgent"}),
            &config,
            &store,
        )
        .unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn set_status_round_trip() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(&json!({"title": "x"}), &config, &store).unwrap();
        let r = action_set_status(&json!({"id": t.id, "status": "done"}), &config, &store).unwrap();
        assert_eq!(r["previous_status"], "open");
        assert_eq!(r["status"], "done");
    }

    #[test]
    fn list_filters_by_status_and_tag() {
        let (_d, config, store, _tx, _rx) = fixture();
        action_create(&json!({"title": "a", "tags": ["work"]}), &config, &store).unwrap();
        let b = action_create(
            &json!({"title": "b", "tags": ["personal"]}),
            &config,
            &store,
        )
        .unwrap();
        action_set_status(&json!({"id": b.id, "status": "done"}), &config, &store).unwrap();
        let work_open = action_list(&json!({"status": "open", "tag": "work"}), &store).unwrap();
        assert_eq!(work_open["todos"].as_array().unwrap().len(), 1);
        let all_done = action_list(&json!({"status": "done"}), &store).unwrap();
        assert_eq!(all_done["todos"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn list_filters_by_due_before() {
        let (_d, config, store, _tx, _rx) = fixture();
        action_create(
            &json!({"title": "soon", "due": "2026-04-30"}),
            &config,
            &store,
        )
        .unwrap();
        action_create(
            &json!({"title": "later", "due": "2026-06-30"}),
            &config,
            &store,
        )
        .unwrap();
        let r = action_list(&json!({"due_before": "2026-05-01"}), &store).unwrap();
        let arr = r["todos"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "soon");
    }

    #[test]
    fn start_emits_event_and_returns_payload() {
        let (_d, config, store, writer, captured) = fixture();
        let t = action_create(&json!({"title": "kickoff"}), &config, &store).unwrap();
        let r = action_start(&json!({"id": t.id}), &config, &store, &writer).unwrap();
        assert_eq!(r["todo"]["title"], "kickoff");
        // emit() writes one line + \n; parse the captured bytes.
        let bytes = captured.lock().unwrap().clone();
        let line = std::str::from_utf8(&bytes).unwrap().trim_end();
        let frame: Value = serde_json::from_str(line).unwrap();
        assert_eq!(frame["method"], "event.publish");
        assert_eq!(frame["params"]["kind"], "todo.start_requested");
    }

    #[test]
    fn update_changes_specific_fields_and_leaves_others() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(
            &json!({"title": "old title", "body": "old body", "tags": ["a", "b"]}),
            &config,
            &store,
        )
        .unwrap();
        // Touch only title + tags. body, priority, due, jira, kb, prompt absent ⇒ untouched.
        let r = action_update(
            &json!({"id": t.id, "title": "new title", "tags": ["a", "b", "c"]}),
            &config,
            &store,
        )
        .unwrap();
        assert_eq!(r["todo"]["title"], "new title");
        // body round-trips with a trailing newline normalization from render_new.
        assert_eq!(r["todo"]["body"].as_str().unwrap().trim(), "old body");
        assert_eq!(r["todo"]["tags"], json!(["a", "b", "c"]));
        assert_eq!(r["todo"]["status"], "open");
    }

    #[test]
    fn update_clears_due_and_linked_jira_via_empty_string() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(
            &json!({
                "title": "x",
                "due": "2026-05-01",
                "linked_jira": "PROJ-1",
                "prompt": "do the thing",
            }),
            &config,
            &store,
        )
        .unwrap();
        let r = action_update(
            &json!({"id": t.id, "due": "", "linked_jira": null, "prompt": ""}),
            &config,
            &store,
        )
        .unwrap();
        assert!(r["todo"]["due"].is_null());
        assert!(r["todo"]["linked_jira"].is_null());
        assert!(r["todo"]["prompt"].is_null());
    }

    #[test]
    fn update_rejects_empty_title_when_provided() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(&json!({"title": "x"}), &config, &store).unwrap();
        let err = action_update(&json!({"id": t.id, "title": "   "}), &config, &store).unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn update_rejects_unknown_priority() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(&json!({"title": "x"}), &config, &store).unwrap();
        let err =
            action_update(&json!({"id": t.id, "priority": "urgent"}), &config, &store).unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn update_returns_not_found_for_missing_todo() {
        let (_d, config, store, _tx, _rx) = fixture();
        let err =
            action_update(&json!({"id": "T-missing", "title": "x"}), &config, &store).unwrap_err();
        assert_eq!(err.0, "not_found");
    }

    #[test]
    fn update_append_subtask_appends_to_empty_body() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(&json!({"title": "x"}), &config, &store).unwrap();
        let r = action_update(
            &json!({"id": t.id, "append_subtask": "first task"}),
            &config,
            &store,
        )
        .unwrap();
        assert_eq!(
            r["todo"]["body"].as_str().unwrap().trim(),
            "- [ ] first task"
        );
    }

    #[test]
    fn update_append_subtask_handles_existing_body_newline() {
        let (_d, config, store, _tx, _rx) = fixture();
        // Body without trailing newline (render_new will add one on store,
        // but parse round-trips body content as authored).
        let t =
            action_create(&json!({"title": "x", "body": "- [ ] one"}), &config, &store).unwrap();
        let r = action_update(
            &json!({"id": t.id, "append_subtask": "two"}),
            &config,
            &store,
        )
        .unwrap();
        let body = r["todo"]["body"].as_str().unwrap();
        // Both lines present; no doubled blank line between them.
        assert!(body.contains("- [ ] one"));
        assert!(body.contains("- [ ] two"));
        assert!(!body.contains("\n\n- [ ] two"));
    }

    #[test]
    fn update_rejects_body_and_append_subtask_together() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(&json!({"title": "x"}), &config, &store).unwrap();
        let err = action_update(
            &json!({"id": t.id, "body": "replacement", "append_subtask": "also this"}),
            &config,
            &store,
        )
        .unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn update_rejects_empty_append_subtask() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(&json!({"title": "x"}), &config, &store).unwrap();
        let err =
            action_update(&json!({"id": t.id, "append_subtask": ""}), &config, &store).unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn fatal_error_short_circuits_actions() {
        let (_d, _, store, writer, _rx) = fixture();
        let bad_config = Config {
            root: store.root().to_path_buf(),
            default_workspace: "default".into(),
            poll_interval: std::time::Duration::from_secs(2),
            fatal_error: Some("bogus".into()),
        };
        let err = handle_action(
            "todo.create",
            &json!({"title": "x"}),
            &bad_config,
            Some(&store),
            None,
            &writer,
        )
        .unwrap_err();
        assert_eq!(err.0, "config_error");
    }

    #[test]
    fn unknown_action_returns_action_not_found() {
        let (_d, config, store, writer, _rx) = fixture();
        let err = handle_action(
            "todo.fly",
            &Value::Null,
            &config,
            Some(&store),
            None,
            &writer,
        )
        .unwrap_err();
        assert_eq!(err.0, "action_not_found");
    }

    #[test]
    fn render_loop_prompt_substitutes_title_and_workspace() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(&json!({"title": "doc trim"}), &config, &store).unwrap();
        let r = action_render_loop_prompt(&json!({"id": t.id}), &config, &store).unwrap();
        let prompt = r["prompt"].as_str().unwrap();
        assert!(prompt.contains("LOOP NAME: doc trim"));
        assert!(prompt.contains(&format!("TODO ID: {}", t.id)));
        // workspace defaults to "default" in the fixture; verify it appears.
        assert!(prompt.contains("WORKSPACE: default"));
    }

    #[test]
    fn render_loop_prompt_auto_adds_loop_tag() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(&json!({"title": "x"}), &config, &store).unwrap();
        assert!(!t.tags.iter().any(|tag| tag == "loop"));
        let r = action_render_loop_prompt(&json!({"id": t.id}), &config, &store).unwrap();
        assert_eq!(r["loop_tag_added"], json!(true));
        // Re-fetch and confirm the tag actually persisted to the file.
        let listed = action_list(&json!({}), &store).unwrap();
        let todos = listed.get("todos").and_then(Value::as_array).unwrap();
        let me = todos
            .iter()
            .find(|x| x["id"].as_str() == Some(t.id.as_str()))
            .unwrap();
        let tags = me["tags"].as_array().unwrap();
        assert!(tags.iter().any(|v| v.as_str() == Some("loop")));
    }

    #[test]
    fn render_loop_prompt_idempotent_when_already_tagged() {
        let (_d, config, store, _tx, _rx) = fixture();
        let t = action_create(
            &json!({"title": "x", "tags": ["loop", "other"]}),
            &config,
            &store,
        )
        .unwrap();
        let r = action_render_loop_prompt(&json!({"id": t.id}), &config, &store).unwrap();
        assert_eq!(r["loop_tag_added"], json!(false));
        // Tag set unchanged — no duplicate `loop` entry.
        let listed = action_list(&json!({}), &store).unwrap();
        let me = listed["todos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"].as_str() == Some(t.id.as_str()))
            .unwrap();
        let tags: Vec<&str> = me["tags"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let loop_count = tags.iter().filter(|s| **s == "loop").count();
        assert_eq!(loop_count, 1);
    }

    #[test]
    fn render_loop_prompt_returns_not_found_for_missing_todo() {
        let (_d, config, store, _tx, _rx) = fixture();
        let err =
            action_render_loop_prompt(&json!({"id": "T-missing"}), &config, &store).unwrap_err();
        assert_eq!(err.0, "not_found");
    }
}
