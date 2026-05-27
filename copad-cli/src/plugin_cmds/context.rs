//! `coctl context` aggregator (Phase 19.2 slice a).
//!
//! Default `coctl context` (no flags) and `coctl context --full`
//! both go through this module: fan out to `context.snapshot`,
//! `session.info`, `git.list_workspaces`, `git.status`, `todo.list`,
//! `calendar.list_events`, `slack.auth_status`, `discord.auth_status`
//! and render a single dense "where am I" view.
//!
//! `coctl --json context` (no `--full`) keeps the raw
//! `context.snapshot` shape verbatim for backward compatibility with
//! scripts already piping it; main.rs handles that fast path.
//! `coctl --json context --full` returns the aggregate as one
//! object — useful for scripting "what's the user's current
//! cross-plugin state?" without N round-trips from the caller.
//!
//! Each section is independent: a failed sub-call (e.g. lazy `git.*`
//! wakeup timing out, calendar plugin in fatal_config) shows
//! `(unavailable)` for that section, never aborts the whole render.
//! Cross-plugin enrichment via composition, not via a new aggregate
//! action — keeps the plugin protocol surface flat and the
//! enrichment logic in one place (this file).

use serde_json::{Value, json};
use std::path::Path;

use crate::client;

/// Returns 0 even when some sections are unavailable — only IPC
/// transport failures or a rejected `context.snapshot` exit non-zero.
pub fn dispatch(socket_path: &str, json_out: bool) -> i32 {
    let snapshot = match fetch(socket_path, "context.snapshot", json!({})) {
        Ok(v) => v,
        Err((code, msg)) => {
            eprintln!("Error: context.snapshot failed: [{code}] {msg}");
            return 1;
        }
    };
    let active_panel = snapshot
        .get("active_panel")
        .and_then(Value::as_str)
        .map(str::to_string);
    let active_cwd = snapshot
        .get("active_cwd")
        .and_then(Value::as_str)
        .map(str::to_string);

    // Side calls — each Result<Value> is "did this succeed", with
    // None meaning "skip this section".
    // session.info contract is platform-divergent: copad-linux respects
    // the `id` param and returns that panel's info, but copad-macos's
    // current impl ignores `id` and returns active-tab info regardless
    // (copad-macos is stub-only per project memory; tracked as a
    // cross-platform follow-up). Defensive guard: only accept the
    // response when its `id` field actually matches what we asked
    // for. On macOS this drops the panel-detail rendering rather
    // than showing wrong-panel info; on Linux it's a no-op.
    let panel_info = active_panel.as_ref().and_then(|id| {
        let resp = fetch(socket_path, "session.info", json!({ "id": id })).ok()?;
        let returned_id = resp.get("id").and_then(Value::as_str)?;
        (returned_id == id).then_some(resp)
    });
    let workspace = active_cwd
        .as_deref()
        .and_then(|cwd| resolve_workspace_from_cwd(socket_path, cwd));
    // git.status with `workspace=ws` reports the primary checkout's
    // state; with `path=cwd` it reports the cwd's own worktree state.
    // For "where am I" semantics we want the latter — when the user
    // sits in a secondary worktree under `<repo>-worktrees/<branch>`,
    // that worktree's branch/ahead/behind/dirty matters, not the
    // primary checkout's. Plugin's allow-list still validates the
    // path is under a configured workspace's `path` or
    // `worktree_root` (copad-plugin-git/src/main.rs::action_status).
    let git_status = active_cwd
        .as_deref()
        .and_then(|cwd| fetch(socket_path, "git.status", json!({ "path": cwd })).ok());
    let todos = workspace
        .as_ref()
        .and_then(|ws| fetch(socket_path, "todo.list", json!({ "workspace": ws })).ok());
    let calendar = fetch(
        socket_path,
        "calendar.list_events",
        json!({ "lookahead_hours": 2 }),
    )
    .ok();
    let slack_auth = fetch(socket_path, "slack.auth_status", json!({})).ok();
    let discord_auth = fetch(socket_path, "discord.auth_status", json!({})).ok();

    if json_out {
        let aggregate = json!({
            "snapshot": snapshot,
            "panel": panel_info,
            "workspace": workspace,
            "git_status": git_status,
            "todos": todos,
            "calendar": calendar,
            "slack_auth": slack_auth,
            "discord_auth": discord_auth,
        });
        println!("{}", serde_json::to_string_pretty(&aggregate).unwrap());
        return 0;
    }

    render_human(
        &snapshot,
        active_cwd.as_deref(),
        workspace.as_deref(),
        panel_info.as_ref(),
        git_status.as_ref(),
        todos.as_ref(),
        calendar.as_ref(),
        slack_auth.as_ref(),
        discord_auth.as_ref(),
    );
    0
}

/// Issue a single action call. `Err((code, message))` on either
/// transport failure or a rejected response.
fn fetch(socket_path: &str, method: &str, params: Value) -> Result<Value, (String, String)> {
    let resp = client::send_command(socket_path, method, params)
        .map_err(|e| ("transport_error".to_string(), e.to_string()))?;
    if resp.ok {
        Ok(resp.result.unwrap_or(Value::Null))
    } else {
        let err = resp
            .error
            .map(|e| (e.code, e.message))
            .unwrap_or_else(|| ("unknown".into(), String::new()));
        Err(err)
    }
}

/// Longest-prefix match cwd against canonicalized `path` /
/// `worktree_root` from `git.list_workspaces`. Mirrors `plugin_cmds::git`'s
/// resolver. `None` on any failure (workspace-bound aggregate sections
/// then skip).
fn resolve_workspace_from_cwd(socket_path: &str, cwd: &str) -> Option<String> {
    let resp = fetch(socket_path, "git.list_workspaces", json!({})).ok()?;
    let arr = resp.get("workspaces").and_then(Value::as_array)?;
    let cwd_path = Path::new(cwd);
    let cwd_canon = cwd_path
        .canonicalize()
        .unwrap_or_else(|_| cwd_path.to_path_buf());
    let mut best: Option<(usize, String)> = None;
    for w in arr {
        // Skip malformed rows rather than aborting the whole lookup —
        // a single bad workspace entry shouldn't suppress otherwise
        // valid matches. Mirrors `plugin_cmds::git::resolve_workspace`.
        let Some(name) = w.get("name").and_then(Value::as_str) else {
            continue;
        };
        for field in ["path", "worktree_root"] {
            let Some(prefix) = w.get(field).and_then(Value::as_str) else {
                continue;
            };
            let prefix_path = Path::new(prefix);
            let canon = prefix_path
                .canonicalize()
                .unwrap_or_else(|_| prefix_path.to_path_buf());
            if cwd_canon.starts_with(&canon)
                && best.as_ref().is_none_or(|(len, _)| prefix.len() > *len)
            {
                best = Some((prefix.len(), name.to_string()));
            }
        }
    }
    best.map(|(_, name)| name)
}

#[allow(clippy::too_many_arguments)]
fn render_human(
    snapshot: &Value,
    active_cwd: Option<&str>,
    workspace: Option<&str>,
    panel_info: Option<&Value>,
    git_status: Option<&Value>,
    todos: Option<&Value>,
    calendar: Option<&Value>,
    slack_auth: Option<&Value>,
    discord_auth: Option<&Value>,
) {
    // ── header ──────────────────────────────────────────────────────
    let header = match (workspace, git_status) {
        (Some(ws), Some(gs)) => format!("context  workspace={ws}  {}", git_summary(gs)),
        (Some(ws), None) => format!("context  workspace={ws}"),
        (None, _) => "context  (no workspace match for cwd)".to_string(),
    };
    println!("{header}");

    // ── panel + cwd ─────────────────────────────────────────────────
    println!();
    println!("panel");
    if let Some(p) = panel_info {
        let tab = p.get("tab").and_then(Value::as_u64).unwrap_or(0);
        let kind = p.get("type").and_then(Value::as_str).unwrap_or("?");
        let title = p.get("title").and_then(Value::as_str).unwrap_or("?");
        let plugin = p.get("plugin").and_then(Value::as_str);
        let detail = match (kind, plugin) {
            ("plugin", Some(plug)) => format!("{kind} · {plug} · \"{title}\""),
            _ => format!("{kind} · \"{title}\""),
        };
        println!("  tab {tab} · {detail}");
    } else if let Some(id) = snapshot.get("active_panel").and_then(Value::as_str) {
        println!("  (no session.info for {id})");
    } else {
        println!("  (no active panel)");
    }
    println!("  cwd {}", active_cwd.unwrap_or("(none)"));

    // ── todos ───────────────────────────────────────────────────────
    println!();
    if let Some(t) = todos {
        render_todos_section(t, workspace.unwrap_or("?"));
    } else if workspace.is_some() {
        println!("todos");
        println!("  (unavailable)");
    }

    // ── calendar ────────────────────────────────────────────────────
    println!();
    println!("calendar (next 2h)");
    if let Some(c) = calendar {
        render_calendar_section(c);
    } else {
        println!("  (unavailable)");
    }

    // ── messengers ──────────────────────────────────────────────────
    println!();
    println!("messengers");
    println!("  slack    {}", auth_summary(slack_auth));
    println!("  discord  {}", auth_summary(discord_auth));
}

fn git_summary(gs: &Value) -> String {
    let branch = gs
        .get("branch")
        .and_then(Value::as_str)
        .unwrap_or("(detached)");
    let upstream = gs.get("upstream").and_then(Value::as_str);
    let ahead = gs.get("ahead").and_then(Value::as_u64).unwrap_or(0);
    let behind = gs.get("behind").and_then(Value::as_u64).unwrap_or(0);
    let dirty = gs.get("dirty").and_then(Value::as_bool).unwrap_or(false);

    let upstream_str = upstream.map(|u| format!(" → {u}")).unwrap_or_default();
    let ahead_behind = if ahead == 0 && behind == 0 {
        String::new()
    } else {
        format!(" ({ahead}↑{behind}↓)")
    };
    let clean = if dirty { " dirty" } else { " clean" };
    format!("{branch}{upstream_str}{ahead_behind}{clean}")
}

fn render_todos_section(t: &Value, workspace: &str) {
    let arr = match t.get("todos").and_then(Value::as_array) {
        Some(a) => a,
        None => {
            println!("todos ({workspace})");
            println!("  (no todos array in response)");
            return;
        }
    };
    let mut open: Vec<&Value> = Vec::new();
    let mut in_progress: Vec<&Value> = Vec::new();
    for todo in arr {
        match todo.get("status").and_then(Value::as_str) {
            Some("open") => open.push(todo),
            Some("in_progress") => in_progress.push(todo),
            _ => {}
        }
    }
    println!(
        "todos ({workspace})  {} open · {} in_progress",
        open.len(),
        in_progress.len()
    );
    if open.is_empty() && in_progress.is_empty() {
        println!("  (none)");
        return;
    }
    let id_w = arr
        .iter()
        .filter_map(|t| t.get("id").and_then(Value::as_str))
        .map(str::len)
        .max()
        .unwrap_or(20);
    // in-progress first (more relevant), then open, capped at 10 total
    let total = open.len() + in_progress.len();
    let limit = 10;
    for (shown, t) in in_progress.iter().chain(open.iter()).enumerate() {
        if shown >= limit {
            println!("  ... {} more", total - limit);
            break;
        }
        let id = t.get("id").and_then(Value::as_str).unwrap_or("?");
        let title = t.get("title").and_then(Value::as_str).unwrap_or("");
        let priority = t.get("priority").and_then(Value::as_str).unwrap_or("?");
        let icon = match t.get("status").and_then(Value::as_str) {
            Some("in_progress") => "[~]",
            _ => "[ ]",
        };
        println!("  {icon} {id:<id_w$}  {priority:<6}  {title}");
    }
}

fn render_calendar_section(c: &Value) {
    let arr = match c.get("events").and_then(Value::as_array) {
        Some(a) => a,
        None => {
            println!("  (no events array in response)");
            return;
        }
    };
    if arr.is_empty() {
        println!("  (no upcoming events)");
        return;
    }
    for ev in arr {
        // calendar.list_events emits `start_time` as RFC3339 +
        // a separate `all_day` boolean. For all-day events there
        // is no clock time worth showing — render "all day" instead.
        let title = ev
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("(untitled)");
        let all_day = ev.get("all_day").and_then(Value::as_bool).unwrap_or(false);
        let time = if all_day {
            "all-day".to_string()
        } else {
            let start = ev.get("start_time").and_then(Value::as_str).unwrap_or("");
            // Trim RFC3339 to HH:MM (strip date prefix). Falls back
            // to the raw value if the string isn't `<date>T<time>...`.
            start
                .split_once('T')
                .map(|(_, t)| t.split(':').take(2).collect::<Vec<_>>().join(":"))
                .unwrap_or_else(|| start.to_string())
        };
        println!("  {time:<7}  {title}");
    }
}

fn auth_summary(auth: Option<&Value>) -> String {
    let Some(a) = auth else {
        return "(unavailable)".into();
    };
    let configured = a
        .get("configured")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let authenticated = a
        .get("authenticated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !configured {
        if let Some(err) = a.get("fatal_error").and_then(Value::as_str)
            && !err.is_empty()
        {
            return format!("not configured · {err}");
        }
        return "not configured".into();
    }
    if !authenticated {
        return "configured · not signed in".into();
    }
    // Identity preference order: username > user_id; team_id when
    // present (Slack).
    let username = a.get("username").and_then(Value::as_str);
    let user_id = a.get("user_id").and_then(Value::as_str);
    let team_id = a.get("team_id").and_then(Value::as_str);
    let mut parts = vec!["connected".to_string()];
    if let Some(name) = username {
        parts.push(format!("bot={name}"));
    } else if let Some(uid) = user_id {
        parts.push(format!("user={uid}"));
    }
    if let Some(team) = team_id {
        parts.push(format!("team={team}"));
    }
    parts.join(" · ")
}
