//! `coctl todo` — ergonomic wrapper over the `todo.*` action surface.
//!
//! Maps clap subcommands onto the existing actions exposed by
//! `copad-plugin-todo`:
//!
//! | CLI                                 | Action            |
//! |-------------------------------------|-------------------|
//! | `todo create <title> [--workspace …]` | `todo.create`     |
//! | `todo list [--status …]`              | `todo.list`       |
//! | `todo update <id> [flags…]`           | `todo.update`     |
//! | `todo loop <id> [--copy]`             | `todo.render_loop_prompt` |
//! | `todo set <id> --status <s>`          | `todo.set_status` |
//! | `todo done <id>`                      | `todo.set_status` (status=done) |
//! | `todo doing <id>`                     | `todo.set_status` (status=in_progress) |
//! | `todo block <id>`                     | `todo.set_status` (status=blocked) |
//! | `todo start <id>`                     | `todo.start`      |
//! | `todo delete <id>`                    | `todo.delete`     |
//!
//! ## ID prefix resolution
//!
//! Every command that takes an `<id>` accepts a unique prefix in lieu
//! of the full `T-<datetime>-<seq>` identifier. We preflight a
//! `todo.list` call, find the unique matching id, and substitute it
//! before dispatch. If zero or many ids match, we error out with the
//! candidates so the user can disambiguate. The roundtrip is cheap on
//! a local socket and saves the user from having to look up the full
//! id from the panel for routine actions.
//!
//! ## Workspace defaulting
//!
//! `--workspace` defaults to `COPAD_TODO_DEFAULT_WORKSPACE` env var if
//! set, else `"default"`. Cwd-derived workspace resolution is a
//! Phase 19.X follow-up (needs `git.resolve_workspace` lifted into a
//! copad-internal action) — until then, users on multiple workspaces
//! pass `--workspace` explicitly. The plugin itself defaults to
//! `default` if the field is omitted from `todo.create`, so omitting
//! the flag here is also fine for the common case.

use clap::Subcommand;
use serde_json::{Value, json};

use crate::client;

/// Pretty status icon for a `todo.list` row. Matches the panel's
/// "Doing" column label (`in_progress` → `~`).
fn status_icon(status: &str) -> &'static str {
    match status {
        "open" => "[ ]",
        "in_progress" => "[~]",
        "done" => "[x]",
        "blocked" => "[!]",
        _ => "[?]",
    }
}

#[derive(Subcommand, Debug)]
pub enum TodoCommand {
    /// Create a new todo
    Create {
        /// Title (required)
        title: String,
        /// Body / description
        #[arg(long)]
        body: Option<String>,
        /// Workspace label (defaults to `COPAD_TODO_DEFAULT_WORKSPACE` or "default")
        #[arg(long)]
        workspace: Option<String>,
        /// Priority: low | normal | high
        #[arg(long, default_value = "normal")]
        priority: String,
        /// Due date (ISO 8601, e.g. 2026-05-01)
        #[arg(long)]
        due: Option<String>,
        /// Linked Jira ticket key
        #[arg(long = "linked-jira")]
        linked_jira: Option<String>,
        /// Tags (comma-separated)
        #[arg(long)]
        tags: Option<String>,
    },
    /// List todos
    List {
        /// Filter by status (open|in_progress|done|blocked)
        #[arg(long)]
        status: Option<String>,
        /// Filter by workspace
        #[arg(long)]
        workspace: Option<String>,
        /// Filter by tag (single tag — matches todos that contain it)
        #[arg(long)]
        tag: Option<String>,
        /// Filter to todos due before the given ISO date (e.g. 2026-05-01)
        #[arg(long = "due-before")]
        due_before: Option<String>,
        /// Hide todos with status=done (default false; pass to declutter)
        #[arg(long)]
        hide_done: bool,
    },
    /// Set a todo's status (or use `done` / `doing` / `block` shorthands)
    Set {
        /// Todo id (full id or unique prefix)
        id: String,
        /// New status: open | in_progress | done | blocked
        #[arg(long)]
        status: String,
        /// Scope id resolution to this workspace (todo ids are
        /// workspace-scoped, not globally unique)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Mark a todo done (`set --status done` shorthand)
    Done {
        /// Todo id (full id or unique prefix)
        id: String,
        /// Scope id resolution to this workspace
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Move a todo to in-progress (`set --status in_progress` shorthand)
    Doing {
        /// Todo id (full id or unique prefix)
        id: String,
        /// Scope id resolution to this workspace
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Mark a todo blocked (`set --status blocked` shorthand)
    Block {
        /// Todo id (full id or unique prefix)
        id: String,
        /// Scope id resolution to this workspace
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Trigger the todo.start workflow (vision-flow-3 chain)
    Start {
        /// Todo id (full id or unique prefix)
        id: String,
        /// Scope id resolution to this workspace
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Delete a todo (irreversible)
    Delete {
        /// Todo id (full id or unique prefix)
        id: String,
        /// Scope id resolution to this workspace
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Update mutable fields of an existing todo. Every flag is optional —
    /// omit a flag to leave that field unchanged. Empty-string args for
    /// `--due` / `--linked-jira` / `--prompt` clear the field
    /// (action's null-or-string convention).
    Update {
        /// Todo id (full id or unique prefix)
        id: String,
        /// Scope id resolution to this workspace
        #[arg(long)]
        workspace: Option<String>,
        /// New title
        #[arg(long)]
        title: Option<String>,
        /// Replace body wholesale. Mutually exclusive with --append-subtask.
        #[arg(long, conflicts_with = "append_subtask")]
        body: Option<String>,
        /// Append `- [ ] <text>` to the existing body. Convenience for
        /// adding subtasks mid-loop without round-tripping through `--body`.
        #[arg(long = "append-subtask")]
        append_subtask: Option<String>,
        /// Priority: low | normal | high
        #[arg(long)]
        priority: Option<String>,
        /// Due date (ISO 8601). Empty string clears.
        #[arg(long)]
        due: Option<String>,
        /// Linked Jira ticket key. Empty string clears.
        #[arg(long = "linked-jira")]
        linked_jira: Option<String>,
        /// Linked KB note ids (comma-separated). Replaces the current
        /// link set. Empty string clears.
        #[arg(long = "linked-kb")]
        linked_kb: Option<String>,
        /// Tags (comma-separated). Replaces the current tag set.
        #[arg(long)]
        tags: Option<String>,
        /// Agent-facing instruction stored in the `prompt` frontmatter.
        /// Empty string clears.
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Show full Todo with linked-entity expansion (kb previews,
    /// linked Jira/Slack list)
    Show {
        /// Todo id (full id or unique prefix)
        id: String,
        /// Scope id resolution to this workspace
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Render the autonomous-loop prompt for an existing todo and print
    /// it (or copy to the system clipboard with `--copy`). Auto-tags
    /// the todo with `loop` if not already — not because the rendered
    /// protocol needs it (it addresses the todo by id+workspace), but
    /// so `list --tag loop` discovers it later and the manual-fill
    /// template at `~/.claude/loop-template.md` (which looks up by
    /// `(workspace, tag=loop, title)`) can resume it without re-tagging.
    Loop {
        /// Todo id (full id or unique prefix)
        id: String,
        /// Scope id resolution to this workspace
        #[arg(long)]
        workspace: Option<String>,
        /// Pipe the rendered prompt to wl-copy (Wayland) or xclip (X11)
        /// instead of stdout. Falls back to stdout with a note when no
        /// clipboard tool is available.
        #[arg(long)]
        copy: bool,
    },
}

/// Returns process exit code for `main.rs` to propagate.
pub fn dispatch(cmd: &TodoCommand, socket_path: &str, json_out: bool) -> i32 {
    match cmd {
        TodoCommand::Create {
            title,
            body,
            workspace,
            priority,
            due,
            linked_jira,
            tags,
        } => {
            let mut params = json!({
                "title": title,
                "priority": priority,
            });
            // Workspace resolution: explicit flag > env var > plugin default.
            // Cwd-derived workspace is a Phase 19.X follow-up.
            if let Some(ws) = workspace {
                params["workspace"] = json!(ws);
            } else if let Ok(ws) = std::env::var("COPAD_TODO_DEFAULT_WORKSPACE") {
                params["workspace"] = json!(ws);
            }
            if let Some(b) = body {
                params["body"] = json!(b);
            }
            if let Some(d) = due {
                params["due"] = json!(d);
            }
            if let Some(j) = linked_jira {
                params["linked_jira"] = json!(j);
            }
            if let Some(t) = tags {
                let parts: Vec<&str> = t
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                params["tags"] = json!(parts);
            }
            call_and_render(socket_path, "todo.create", params, json_out, |v| {
                // Response shape: `{id, workspace, todo}` (full Todo
                // payload nested under `todo`). Render the id +
                // workspace; the rest is fetchable via `todo list` /
                // `todo show` (19.2).
                let id = v.get("id").and_then(Value::as_str).unwrap_or("?");
                let ws = v.get("workspace").and_then(Value::as_str).unwrap_or("?");
                println!("created {id} (ws={ws})");
            })
        }
        TodoCommand::List {
            status,
            workspace,
            tag,
            due_before,
            hide_done,
        } => {
            let mut params = json!({});
            if let Some(s) = status {
                params["status"] = json!(s);
            }
            if let Some(w) = workspace {
                params["workspace"] = json!(w);
            }
            if let Some(t) = tag {
                params["tag"] = json!(t);
            }
            if let Some(d) = due_before {
                params["due_before"] = json!(d);
            }
            call_and_render(socket_path, "todo.list", params, json_out, |v| {
                render_list(v, *hide_done);
            })
        }
        TodoCommand::Set {
            id,
            status,
            workspace,
        } => set_status(socket_path, id, status, workspace.as_deref(), json_out),
        TodoCommand::Done { id, workspace } => {
            set_status(socket_path, id, "done", workspace.as_deref(), json_out)
        }
        TodoCommand::Doing { id, workspace } => set_status(
            socket_path,
            id,
            "in_progress",
            workspace.as_deref(),
            json_out,
        ),
        TodoCommand::Block { id, workspace } => {
            set_status(socket_path, id, "blocked", workspace.as_deref(), json_out)
        }
        TodoCommand::Start { id, workspace } => {
            let r = match resolve_id(socket_path, id, workspace.as_deref()) {
                Ok(r) => r,
                Err(code) => return code,
            };
            call_and_render(
                socket_path,
                "todo.start",
                json!({ "id": r.id, "workspace": r.workspace }),
                json_out,
                |_| println!("started {} (ws={})", r.id, r.workspace),
            )
        }
        TodoCommand::Delete { id, workspace } => {
            let r = match resolve_id(socket_path, id, workspace.as_deref()) {
                Ok(r) => r,
                Err(code) => return code,
            };
            call_and_render(
                socket_path,
                "todo.delete",
                json!({ "id": r.id, "workspace": r.workspace }),
                json_out,
                |_| println!("deleted {} (ws={})", r.id, r.workspace),
            )
        }
        TodoCommand::Update {
            id,
            workspace,
            title,
            body,
            append_subtask,
            priority,
            due,
            linked_jira,
            linked_kb,
            tags,
            prompt,
        } => {
            let r = match resolve_id(socket_path, id, workspace.as_deref()) {
                Ok(r) => r,
                Err(code) => return code,
            };
            // `--append-subtask` is wired straight through to the action's
            // `append_subtask` param so the read-modify-write happens inside
            // the action handler — no client-side preflight that would widen
            // the race window. clap's `conflicts_with` already rejects
            // simultaneous `--body` + `--append-subtask`; the action also
            // rejects the combination as a defense-in-depth.
            let mut params = json!({ "id": r.id, "workspace": r.workspace });
            if let Some(t) = title {
                params["title"] = json!(t);
            }
            if let Some(b) = body {
                params["body"] = json!(b);
            }
            if let Some(text) = append_subtask {
                params["append_subtask"] = json!(text);
            }
            if let Some(p) = priority {
                params["priority"] = json!(p);
            }
            if let Some(d) = due {
                params["due"] = if d.is_empty() { Value::Null } else { json!(d) };
            }
            if let Some(j) = linked_jira {
                params["linked_jira"] = if j.is_empty() { Value::Null } else { json!(j) };
            }
            if let Some(kb) = linked_kb {
                let parts: Vec<&str> = kb
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                params["linked_kb"] = json!(parts);
            }
            if let Some(t) = tags {
                let parts: Vec<&str> = t
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                params["tags"] = json!(parts);
            }
            if let Some(p) = prompt {
                params["prompt"] = if p.is_empty() { Value::Null } else { json!(p) };
            }
            call_and_render(socket_path, "todo.update", params, json_out, |_| {
                println!("updated {} (ws={})", r.id, r.workspace);
            })
        }
        TodoCommand::Show { id, workspace } => {
            show(socket_path, id, workspace.as_deref(), json_out)
        }
        TodoCommand::Loop {
            id,
            workspace,
            copy,
        } => {
            let r = match resolve_id(socket_path, id, workspace.as_deref()) {
                Ok(r) => r,
                Err(code) => return code,
            };
            let params = json!({ "id": r.id, "workspace": r.workspace });
            call_and_render(
                socket_path,
                "todo.render_loop_prompt",
                params,
                json_out,
                |v| {
                    let prompt = v.get("prompt").and_then(Value::as_str).unwrap_or("");
                    let tag_added = v
                        .get("loop_tag_added")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if *copy && pipe_to_clipboard(prompt) {
                        eprintln!(
                            "loop prompt for {} (ws={}) copied to clipboard{}",
                            r.id,
                            r.workspace,
                            if tag_added { " — loop tag added" } else { "" },
                        );
                        eprintln!("paste into Claude Code's `/loop` to start.");
                    } else {
                        if *copy {
                            eprintln!(
                                "(no clipboard tool available — wl-copy/xclip not found; falling back to stdout)"
                            );
                        }
                        print!("{prompt}");
                    }
                },
            )
        }
    }
}

/// Best-effort clipboard pipe. Prefers `wl-copy` when `WAYLAND_DISPLAY`
/// is set, else `xclip -selection clipboard` when `DISPLAY` is set,
/// else returns false so the caller can fall back to stdout. Stdin pipe
/// failure (tool not on PATH, write error) also reports false.
fn pipe_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: Vec<(&str, &[&str])> = {
        let mut v: Vec<(&str, &[&str])> = Vec::new();
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            v.push(("wl-copy", &[]));
        }
        if std::env::var_os("DISPLAY").is_some() {
            v.push(("xclip", &["-selection", "clipboard"]));
        }
        v
    };
    for (cmd, args) in candidates {
        let mut child = match Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill();
            continue;
        };
        if stdin.write_all(text.as_bytes()).is_err() {
            let _ = child.kill();
            continue;
        }
        drop(stdin);
        match child.wait() {
            Ok(status) if status.success() => return true,
            _ => continue,
        }
    }
    false
}

/// `todo show` — fan out from one resolved id:
/// - `linked_kb[]` → `kb.read` per entry (best-effort; per-entry errors
///   are swallowed and aggregated). Preview in human mode, full content
///   under `--json`.
/// - `linked_jira` rendered as keys (no `jira.get_ticket` fan-out yet).
/// - `linked_slack` rendered as permalinks (no cheap body-fetch).
/// - Timeline omitted — there's no socket-callable history surface yet.
fn show(
    socket_path: &str,
    id_or_prefix: &str,
    workspace_filter: Option<&str>,
    json_out: bool,
) -> i32 {
    let r = match resolve_id(socket_path, id_or_prefix, workspace_filter) {
        Ok(r) => r,
        Err(code) => return code,
    };
    // Re-fetch the full todo from the resolved id. The preflight in
    // `resolve_id` already pulled `todo.list`, but it discards
    // everything except `id` and `workspace`; doing one more list
    // (filtered by workspace) is cheaper than threading the full
    // object through and doesn't depend on `resolve_id`'s internal
    // shape.
    let todo = match find_todo(socket_path, &r.id, &r.workspace) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let linked_kb_arr = todo
        .get("linked_kb")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut kb_entries: Vec<KbEntry> = Vec::new();
    for kb_id in &linked_kb_arr {
        if let Some(kb_id_str) = kb_id.as_str() {
            let resp = call_one(socket_path, "kb.read", json!({ "id": kb_id_str }));
            kb_entries.push((kb_id_str.to_string(), resp));
        }
    }

    if json_out {
        let kb_json: Vec<Value> = kb_entries
            .iter()
            .map(|(id, res)| match res {
                // Pass through the full kb.read payload (content +
                // frontmatter + path + whatever else the plugin
                // adds in future) so scripts piping
                // `--json` get the same data as a direct
                // `coctl call kb.read` would.
                Ok(v) => {
                    let mut obj = serde_json::Map::new();
                    obj.insert("id".into(), Value::String(id.clone()));
                    obj.insert("ok".into(), Value::Bool(true));
                    if let Value::Object(payload) = v {
                        for (k, v) in payload {
                            obj.insert(k.clone(), v.clone());
                        }
                    }
                    Value::Object(obj)
                }
                Err((code, msg)) => json!({ "id": id, "ok": false, "code": code, "message": msg }),
            })
            .collect();
        let aggregate = json!({
            "todo": todo,
            "linked_kb_resolved": kb_json,
        });
        println!("{}", serde_json::to_string_pretty(&aggregate).unwrap());
        return 0;
    }

    render_show(&todo, &kb_entries);
    0
}

fn find_todo(socket_path: &str, id: &str, workspace: &str) -> Result<Value, i32> {
    let resp =
        match client::send_command(socket_path, "todo.list", json!({ "workspace": workspace })) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error: todo.list failed: {e}");
                return Err(1);
            }
        };
    if !resp.ok {
        let err = resp
            .error
            .map(|e| format!("[{}] {}", e.code, e.message))
            .unwrap_or_default();
        eprintln!("Error: todo.list failed: {err}");
        return Err(1);
    }
    let arr = resp
        .result
        .and_then(|v| v.get("todos").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    arr.into_iter()
        .find(|t| t.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| {
            eprintln!("Error: todo {id} disappeared between resolve and fetch (concurrent edit?)");
            1
        })
}

/// Like `call_and_render` but doesn't print/exit on failure — `show`
/// aggregates per-entry errors and renders them together.
fn call_one(socket_path: &str, method: &str, params: Value) -> Result<Value, (String, String)> {
    let resp = client::send_command(socket_path, method, params)
        .map_err(|e| ("transport_error".to_string(), e.to_string()))?;
    if resp.ok {
        Ok(resp.result.unwrap_or(Value::Null))
    } else {
        Err(resp
            .error
            .map(|e| (e.code, e.message))
            .unwrap_or_else(|| ("unknown".into(), String::new())))
    }
}

type KbEntry = (String, Result<Value, (String, String)>);

fn render_show(todo: &Value, kb_entries: &[KbEntry]) {
    let id = todo.get("id").and_then(Value::as_str).unwrap_or("?");
    let title = todo.get("title").and_then(Value::as_str).unwrap_or("");
    let status = todo.get("status").and_then(Value::as_str).unwrap_or("?");
    let priority = todo.get("priority").and_then(Value::as_str).unwrap_or("?");
    let workspace = todo.get("workspace").and_then(Value::as_str).unwrap_or("?");
    let icon = status_icon(status);

    println!("{icon} {id}  {title}");
    println!("  status={status}  priority={priority}  workspace={workspace}");

    if let Some(due) = todo.get("due").and_then(Value::as_str)
        && !due.is_empty()
    {
        println!("  due {due}");
    }
    if let Some(tags) = todo.get("tags").and_then(Value::as_array)
        && !tags.is_empty()
    {
        let names: Vec<String> = tags
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        if !names.is_empty() {
            println!("  tags {}", names.join(", "));
        }
    }
    if let Some(jira) = todo.get("linked_jira").and_then(Value::as_str)
        && !jira.is_empty()
    {
        // jira.get_ticket fan-out lands once Phase 16 ships; until
        // then we just surface the key.
        println!("  jira {jira}");
    }
    if let Some(slack) = todo.get("linked_slack").and_then(Value::as_array)
        && !slack.is_empty()
    {
        // `linked_slack` is `Vec<Value>` per the todo schema —
        // entries can be permalink strings OR structured objects
        // (e.g. `{team, channel, ts}` matching the slack.reaction
        // payload shape). Render strings verbatim; flatten objects
        // to `key=value` pairs; fall back to JSON for anything else.
        println!("  slack");
        for s in slack {
            match s {
                Value::String(p) => println!("    {p}"),
                Value::Object(map) => {
                    let pairs: Vec<String> = map
                        .iter()
                        .map(|(k, v)| match v {
                            Value::String(s) => format!("{k}={s}"),
                            other => format!("{k}={other}"),
                        })
                        .collect();
                    println!("    {}", pairs.join("  "));
                }
                other => println!("    {other}"),
            }
        }
    }
    let body = todo.get("body").and_then(Value::as_str).unwrap_or("");
    if !body.is_empty() {
        println!();
        println!("body");
        for line in body.lines() {
            println!("  {line}");
        }
    }
    let prompt = todo.get("prompt").and_then(Value::as_str).unwrap_or("");
    if !prompt.is_empty() {
        println!();
        println!("prompt");
        for line in prompt.lines() {
            println!("  {line}");
        }
    }
    if !kb_entries.is_empty() {
        println!();
        println!("linked_kb");
        for (kb_id, res) in kb_entries {
            match res {
                Ok(v) => {
                    let content = v.get("content").and_then(Value::as_str).unwrap_or("");
                    // Strip frontmatter for the preview — the user
                    // came here for body content, not metadata they
                    // already see in the kb file itself.
                    let preview = strip_frontmatter(content);
                    println!("  {kb_id}");
                    let mut shown = 0;
                    for line in preview.lines() {
                        if shown >= 5 {
                            println!("    …");
                            break;
                        }
                        let trimmed = line.trim_end();
                        if trimmed.is_empty() && shown == 0 {
                            // Skip leading blank lines after frontmatter
                            continue;
                        }
                        println!("    {trimmed}");
                        shown += 1;
                    }
                }
                Err((code, msg)) => {
                    println!("  {kb_id}  ({code}: {msg})");
                }
            }
        }
    }
}

/// Strip a leading `---\n...\n---\n` YAML frontmatter block. Returns
/// the input unchanged if no frontmatter is present.
fn strip_frontmatter(content: &str) -> &str {
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return content;
    }
    let after_open = content
        .split_once('\n')
        .map(|(_, rest)| rest)
        .unwrap_or(content);
    if let Some(close_idx) = after_open.find("\n---\n") {
        let after_close = &after_open[close_idx + 5..];
        return after_close.trim_start_matches('\n');
    }
    if let Some(close_idx) = after_open.find("\n---\r\n") {
        let after_close = &after_open[close_idx + 6..];
        return after_close.trim_start_matches('\n');
    }
    content
}

fn set_status(
    socket_path: &str,
    id_or_prefix: &str,
    status: &str,
    workspace_filter: Option<&str>,
    json_out: bool,
) -> i32 {
    if !["open", "in_progress", "done", "blocked"].contains(&status) {
        eprintln!("Error: status must be one of open|in_progress|done|blocked (got {status:?})");
        return 2;
    }
    let r = match resolve_id(socket_path, id_or_prefix, workspace_filter) {
        Ok(r) => r,
        Err(code) => return code,
    };
    call_and_render(
        socket_path,
        "todo.set_status",
        json!({ "id": r.id, "workspace": r.workspace, "status": status }),
        json_out,
        |_| println!("{} → {status} (ws={})", r.id, r.workspace),
    )
}

fn render_list(v: &Value, hide_done: bool) {
    let todos = match v.get("todos").and_then(Value::as_array) {
        Some(arr) => arr,
        None => {
            eprintln!("(no todos array in response)");
            return;
        }
    };
    if todos.is_empty() {
        println!("(no todos)");
        return;
    }
    // Column widths — id is the only one with significant variance.
    let id_w = todos
        .iter()
        .filter_map(|t| t.get("id").and_then(Value::as_str))
        .map(str::len)
        .max()
        .unwrap_or(20);
    let mut shown = 0usize;
    for t in todos {
        let status = t.get("status").and_then(Value::as_str).unwrap_or("?");
        if hide_done && status == "done" {
            continue;
        }
        let id = t.get("id").and_then(Value::as_str).unwrap_or("?");
        let priority = t.get("priority").and_then(Value::as_str).unwrap_or("?");
        let title = t.get("title").and_then(Value::as_str).unwrap_or("");
        let workspace = t.get("workspace").and_then(Value::as_str).unwrap_or("?");
        let icon = status_icon(status);
        let mut meta = vec![format!("ws={workspace}")];
        if let Some(due) = t.get("due").and_then(Value::as_str)
            && !due.is_empty()
        {
            meta.push(format!("due={due}"));
        }
        if let Some(tags) = t.get("tags").and_then(Value::as_array) {
            let names: Vec<String> = tags
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect();
            if !names.is_empty() {
                meta.push(format!("tags={}", names.join(",")));
            }
        }
        if let Some(jira) = t.get("linked_jira").and_then(Value::as_str)
            && !jira.is_empty()
        {
            meta.push(format!("jira={jira}"));
        }
        println!(
            "{icon} {id:<id_w$}  {priority:<6}  {title}  ·  {meta}",
            meta = meta.join(" "),
        );
        shown += 1;
    }
    if shown == 0 {
        println!("(no todos)");
    }
}

/// Pairs id with its actual workspace — actions default `workspace`
/// to the plugin's configured default, so a non-default-workspace
/// todo 404s without this.
struct ResolvedTodo {
    id: String,
    workspace: String,
}

/// Resolves `<id>` (full or prefix) → `ResolvedTodo`. Strategy:
/// exact-match first across visible workspaces; on zero matches, fall
/// back to prefix-match. Either set ambiguous (>1) forces the user to
/// pass `--workspace <ws>` — we never silently pick whichever workspace
/// the listing enumerated first, which would mutate the wrong todo.
/// `workspace_filter` scopes the preflight when the user already knows
/// which workspace to target.
fn resolve_id(
    socket_path: &str,
    prefix: &str,
    workspace_filter: Option<&str>,
) -> Result<ResolvedTodo, i32> {
    let mut params = json!({});
    if let Some(w) = workspace_filter {
        params["workspace"] = json!(w);
    }
    let resp = match client::send_command(socket_path, "todo.list", params) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: id preflight failed: {e}");
            return Err(1);
        }
    };
    if !resp.ok {
        let err = resp
            .error
            .map(|e| format!("[{}] {}", e.code, e.message))
            .unwrap_or_default();
        eprintln!("Error: id preflight failed: {err}");
        return Err(1);
    }
    let todos = resp
        .result
        .and_then(|v| v.get("todos").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let pluck = |t: &Value| {
        let id = t.get("id").and_then(Value::as_str)?.to_string();
        let workspace = t
            .get("workspace")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_string();
        Some((id, workspace))
    };
    let mut exact: Vec<ResolvedTodo> = Vec::new();
    let mut prefix_hits: Vec<ResolvedTodo> = Vec::new();
    for t in &todos {
        let Some((id, workspace)) = pluck(t) else {
            continue;
        };
        if id == prefix {
            exact.push(ResolvedTodo { id, workspace });
        } else if id.starts_with(prefix) {
            prefix_hits.push(ResolvedTodo { id, workspace });
        }
    }
    let candidates = if !exact.is_empty() {
        exact
    } else {
        prefix_hits
    };
    let kind = if !candidates.is_empty() && candidates.iter().any(|r| r.id == prefix) {
        "id"
    } else {
        "id prefix"
    };
    match candidates.len() {
        0 => {
            eprintln!("Error: no todo matches {kind} {prefix:?}");
            Err(1)
        }
        1 => Ok(candidates.into_iter().next().unwrap()),
        _ => {
            let scope = workspace_filter
                .map(|w| format!(" (within workspace {w:?})"))
                .unwrap_or_default();
            eprintln!(
                "Error: {kind} {prefix:?} matches {} todos{scope} — disambiguate via `--workspace <ws>` or a longer prefix:",
                candidates.len()
            );
            for r in candidates {
                eprintln!("  {} (ws={})", r.id, r.workspace);
            }
            Err(1)
        }
    }
}

use super::call_and_render;
