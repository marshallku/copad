//! `coctl git` — ergonomic wrapper over the `git.*` action surface.
//!
//! Maps clap subcommands onto the existing actions exposed by
//! `copad-plugin-git`:
//!
//! | CLI                                            | Action                |
//! |------------------------------------------------|-----------------------|
//! | `git workspaces`                               | `git.list_workspaces` |
//! | `git worktrees [--workspace W]`                | `git.list_worktrees`  |
//! | `git wt add <branch> [--workspace] [--sanitize-jira]` | `git.worktree_add`    |
//! | `git wt remove <path> [--force]`               | `git.worktree_remove` |
//! | `git branch [--workspace W]`                   | `git.current_branch`  |
//! | `git status [--workspace W] [--path P]`        | `git.status`          |
//!
//! ## Workspace defaulting
//!
//! Every subcommand that takes `--workspace` falls through:
//! 1. Explicit `--workspace <name>` flag (highest precedence).
//! 2. `COPAD_GIT_DEFAULT_WORKSPACE` env var.
//! 3. Cwd-derived: preflight `git.list_workspaces`, then longest-prefix
//!    match the cwd (canonicalized) against EITHER each workspace's
//!    `path` OR its `worktree_root` (both canonicalized). The
//!    worktree_root match is what makes `cd` into a created worktree
//!    under `<repo>-worktrees/<branch>` resolve correctly.
//! 4. If exactly one workspace is configured, use it.
//! 5. Otherwise the CLI prints the candidate list to stderr and
//!    returns exit 1. The plugin's own `require_workspace` returns
//!    `not_found` without enumerating candidates, so the CLI does
//!    the listing client-side — better UX than the bare error.
//!
//! Cwd-derive is the killer ergonomic — `cd` into a worktree, run
//! `coctl git status`, get the right answer without typing
//! `--workspace`. Implemented client-side via the preflight call;
//! a future Phase 19.X copad-internal `resolve_workspace(cwd)` action
//! could lift this, but until then the CLI does the lookup itself.

use clap::Subcommand;
use serde_json::{Value, json};
use std::path::Path;

use crate::client;

#[derive(Subcommand, Debug)]
pub enum GitCommand {
    /// List configured workspaces with their current branch + worktree count
    Workspaces,
    /// List worktrees for a workspace
    Worktrees {
        /// Workspace name (defaults to env / cwd-derived)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Worktree management (`add` / `remove`)
    #[command(subcommand)]
    Wt(WtCommand),
    /// Print the current branch of a workspace's primary checkout
    Branch {
        /// Workspace name (defaults to env / cwd-derived)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Show working-tree status (branch, ahead/behind, clean/dirty)
    Status {
        /// Workspace name (looked up by name in workspaces.toml)
        #[arg(long, conflicts_with = "path")]
        workspace: Option<String>,
        /// Explicit worktree path (must be under a configured workspace)
        #[arg(long, conflicts_with = "workspace")]
        path: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum WtCommand {
    /// Create a new worktree under the workspace's worktree_root
    Add {
        /// Branch name to create the worktree on
        branch: String,
        /// Workspace name (defaults to env / cwd-derived)
        #[arg(long)]
        workspace: Option<String>,
        /// Lowercase + slash-preserve sanitize before validation
        /// (matches the Phase 15.2 vision-flow-3 trigger contract)
        #[arg(long = "sanitize-jira")]
        sanitize_jira: bool,
    },
    /// Remove a worktree (path must be under a configured workspace)
    Remove {
        /// Path to the worktree directory
        path: String,
        /// Force removal even if the worktree has uncommitted changes
        #[arg(long)]
        force: bool,
    },
}

pub fn dispatch(cmd: &GitCommand, socket_path: &str, json_out: bool) -> i32 {
    match cmd {
        GitCommand::Workspaces => call_and_render(
            socket_path,
            "git.list_workspaces",
            json!({}),
            json_out,
            render_workspaces,
        ),
        GitCommand::Worktrees { workspace } => {
            let ws = match resolve_workspace(socket_path, workspace.as_deref()) {
                Ok(w) => w,
                Err(code) => return code,
            };
            call_and_render(
                socket_path,
                "git.list_worktrees",
                json!({ "workspace": ws }),
                json_out,
                render_worktrees,
            )
        }
        GitCommand::Wt(WtCommand::Add {
            branch,
            workspace,
            sanitize_jira,
        }) => {
            let ws = match resolve_workspace(socket_path, workspace.as_deref()) {
                Ok(w) => w,
                Err(code) => return code,
            };
            let mut params = json!({
                "workspace": ws,
                "branch": branch,
            });
            if *sanitize_jira {
                params["sanitize_jira"] = json!(true);
            }
            call_and_render(socket_path, "git.worktree_add", params, json_out, |v| {
                let path = v.get("path").and_then(Value::as_str).unwrap_or("?");
                let br = v.get("branch").and_then(Value::as_str).unwrap_or("?");
                println!("created {br} → {path}");
            })
        }
        GitCommand::Wt(WtCommand::Remove { path, force }) => {
            let mut params = json!({ "path": path });
            if *force {
                params["force"] = json!(true);
            }
            call_and_render(socket_path, "git.worktree_remove", params, json_out, |v| {
                let p = v.get("path").and_then(Value::as_str).unwrap_or(path);
                println!("removed {p}");
            })
        }
        GitCommand::Branch { workspace } => {
            let ws = match resolve_workspace(socket_path, workspace.as_deref()) {
                Ok(w) => w,
                Err(code) => return code,
            };
            call_and_render(
                socket_path,
                "git.current_branch",
                json!({ "workspace": ws }),
                json_out,
                |v| {
                    let br = v.get("branch").and_then(Value::as_str).unwrap_or("?");
                    println!("{br}");
                },
            )
        }
        GitCommand::Status { workspace, path } => {
            // git.status accepts EITHER workspace OR path, exclusive.
            // Clap already enforces conflict; here we apply the
            // resolve-fallback only if neither was given.
            let mut params = json!({});
            if let Some(p) = path {
                params["path"] = json!(p);
            } else {
                let ws = match resolve_workspace(socket_path, workspace.as_deref()) {
                    Ok(w) => w,
                    Err(code) => return code,
                };
                params["workspace"] = json!(ws);
            }
            call_and_render(socket_path, "git.status", params, json_out, render_status)
        }
    }
}

/// Defaulting chain: `--workspace` → env → cwd-derived (matched against
/// either `path` or `worktree_root`) → single-config-entry → error.
/// Enumerates candidates client-side because the plugin's own
/// `require_workspace` returns bare `not_found` without a list.
fn resolve_workspace(socket_path: &str, explicit: Option<&str>) -> Result<String, i32> {
    if let Some(s) = explicit {
        return Ok(s.to_string());
    }
    if let Ok(env_ws) = std::env::var("COPAD_GIT_DEFAULT_WORKSPACE")
        && !env_ws.is_empty()
    {
        return Ok(env_ws);
    }
    // Preflight list_workspaces. If the call fails, surface the
    // transport error — caller can pass `--workspace` explicitly to
    // bypass the preflight entirely.
    let resp = match client::send_command(socket_path, "git.list_workspaces", json!({})) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: workspace preflight failed: {e}");
            eprintln!("       (pass `--workspace <name>` to skip preflight)");
            return Err(1);
        }
    };
    if !resp.ok {
        let err = resp
            .error
            .map(|e| format!("[{}] {}", e.code, e.message))
            .unwrap_or_default();
        eprintln!("Error: workspace preflight failed: {err}");
        return Err(1);
    }
    let result = resp.result.unwrap_or(Value::Null);
    let arr = result
        .get("workspaces")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let fatal = result
        .get("fatal_error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if arr.is_empty() {
        // `git.list_workspaces` is the discovery surface — it
        // surfaces `fatal_error` in-band even when no workspaces
        // load. Don't paper over that with a generic "no workspaces
        // configured" message; relay the plugin's diagnostic.
        if let Some(err) = fatal {
            eprintln!("Error: workspaces.toml has errors: {err}");
        } else {
            eprintln!("Error: no workspaces configured. Edit ~/.config/copad/workspaces.toml.");
        }
        return Err(1);
    }
    // Canonicalize cwd once so symlinked entry into a workspace
    // (e.g. user has `~/work/copad -> /home/marshall/dev/copad`) still
    // matches the configured (canonical) workspace paths.
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.canonicalize().unwrap_or(p));
    if let Some(ref cwd) = cwd {
        // Longest-prefix match. Workspaces have BOTH a primary `path`
        // and a `worktree_root` (where created worktrees live, often
        // a sibling of `path` like `<repo>-worktrees`). Match against
        // either — `cd` into a worktree should resolve to its
        // workspace just as well as `cd` into the primary checkout.
        let mut best: Option<(usize, String)> = None;
        for w in &arr {
            let Some(name) = w.get("name").and_then(Value::as_str) else {
                continue;
            };
            for field in ["path", "worktree_root"] {
                let Some(prefix) = w.get(field).and_then(Value::as_str) else {
                    continue;
                };
                // Canonicalize the prefix too (best-effort — a
                // configured path that doesn't exist on disk falls
                // back to the literal value). Without this the
                // canonical cwd may not start_with a non-canonical
                // prefix, defeating the symlink fix.
                let prefix_path = Path::new(prefix);
                let canon = prefix_path
                    .canonicalize()
                    .unwrap_or_else(|_| prefix_path.to_path_buf());
                if cwd.starts_with(&canon)
                    && best.as_ref().is_none_or(|(len, _)| prefix.len() > *len)
                {
                    best = Some((prefix.len(), name.to_string()));
                }
            }
        }
        if let Some((_, name)) = best {
            return Ok(name);
        }
    }
    // No cwd match — if exactly one workspace is configured, use it.
    if arr.len() == 1
        && let Some(name) = arr[0].get("name").and_then(Value::as_str)
    {
        return Ok(name.to_string());
    }
    // Multiple workspaces, no cwd match — list candidates so the
    // user can pass `--workspace <name>` knowingly. The plugin's
    // own error wouldn't include the list.
    eprintln!(
        "Error: cannot resolve workspace (cwd doesn't match any configured `path` / `worktree_root`)."
    );
    eprintln!(
        "       Pass `--workspace <name>`, set COPAD_GIT_DEFAULT_WORKSPACE, or `cd` into one of:"
    );
    for w in &arr {
        let name = w.get("name").and_then(Value::as_str).unwrap_or("?");
        let path = w.get("path").and_then(Value::as_str).unwrap_or("?");
        let wt_root = w
            .get("worktree_root")
            .and_then(Value::as_str)
            .unwrap_or("?");
        eprintln!("         {name}: path={path}, worktree_root={wt_root}");
    }
    Err(1)
}

fn render_workspaces(v: &Value) {
    let arr = match v.get("workspaces").and_then(Value::as_array) {
        Some(a) => a,
        None => {
            eprintln!("(no workspaces array in response)");
            return;
        }
    };
    if arr.is_empty() {
        println!("(no workspaces configured)");
        if let Some(err) = v.get("fatal_error").and_then(Value::as_str)
            && !err.is_empty()
        {
            println!("config error: {err}");
        }
        return;
    }
    let name_w = arr
        .iter()
        .filter_map(|w| w.get("name").and_then(Value::as_str))
        .map(str::len)
        .max()
        .unwrap_or(8);
    for w in arr {
        let name = w.get("name").and_then(Value::as_str).unwrap_or("?");
        let path = w.get("path").and_then(Value::as_str).unwrap_or("?");
        let branch = w
            .get("current_branch")
            .and_then(Value::as_str)
            .unwrap_or("(?)");
        let wt_count = w
            .get("worktree_count")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into());
        println!("{name:<name_w$}  {branch:<28}  wt={wt_count}  {path}");
    }
    if let Some(err) = v.get("fatal_error").and_then(Value::as_str)
        && !err.is_empty()
    {
        eprintln!("config has errors: {err}");
    }
}

fn render_worktrees(v: &Value) {
    let ws = v.get("workspace").and_then(Value::as_str).unwrap_or("?");
    let arr = match v.get("worktrees").and_then(Value::as_array) {
        Some(a) => a,
        None => {
            eprintln!("(no worktrees array in response)");
            return;
        }
    };
    if arr.is_empty() {
        println!("workspace={ws}  (no worktrees)");
        return;
    }
    println!("workspace={ws}  count={}", arr.len());
    let branch_w = arr
        .iter()
        .filter_map(|w| w.get("branch").and_then(Value::as_str))
        .map(str::len)
        .max()
        .unwrap_or(20)
        .min(40);
    for w in arr {
        let branch = w
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("(detached)");
        let path = w.get("path").and_then(Value::as_str).unwrap_or("?");
        let head = w
            .get("head_sha")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(8).collect::<String>())
            .unwrap_or_else(|| "????????".into());
        let mut tags = Vec::new();
        if w.get("locked").and_then(Value::as_bool).unwrap_or(false) {
            tags.push("locked");
        }
        if w.get("prunable").and_then(Value::as_bool).unwrap_or(false) {
            tags.push("prunable");
        }
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(","))
        };
        println!("  {head}  {branch:<branch_w$}  {path}{tag_str}");
    }
}

fn render_status(v: &Value) {
    let path = v.get("path").and_then(Value::as_str).unwrap_or("?");
    let branch = v
        .get("branch")
        .and_then(Value::as_str)
        .unwrap_or("(detached)");
    let upstream = v.get("upstream").and_then(Value::as_str);
    let ahead = v.get("ahead").and_then(Value::as_u64).unwrap_or(0);
    let behind = v.get("behind").and_then(Value::as_u64).unwrap_or(0);
    let staged = v.get("staged").and_then(Value::as_u64).unwrap_or(0);
    let unstaged = v.get("unstaged").and_then(Value::as_u64).unwrap_or(0);
    let untracked = v.get("untracked").and_then(Value::as_u64).unwrap_or(0);
    let dirty = v.get("dirty").and_then(Value::as_bool).unwrap_or(false);

    let upstream_str = upstream.map(|u| format!(" → {u}")).unwrap_or_default();
    let ahead_behind = if ahead == 0 && behind == 0 {
        String::new()
    } else {
        format!(" {ahead}↑{behind}↓")
    };
    let clean = if dirty { "dirty" } else { "clean" };
    println!("{path}");
    println!("  {branch}{upstream_str}{ahead_behind}  {clean}");
    if dirty {
        println!("  staged={staged}  unstaged={unstaged}  untracked={untracked}");
    }
}

use super::call_and_render;
