//! Git-worktree engine for `comux worktree {create,list,rm}` — the standalone,
//! `State`-free core (ported from `~/dev/tmx`, minus tmux). It shells out to `git`,
//! places a new worktree as a SIBLING of the repo's MAIN worktree named
//! `{repo}-{branch}`, and runs a configured per-repo post-create hook. The
//! control/TUI layers add comux sessions + live-session safety on top; nothing here
//! touches `State`, so it is unit-testable against a throwaway `git init` repo.
//!
//! Identity is anchored on git, never on the bare computed path: naming is
//! non-injective (`feat/x` and `feat-x` both render to `feat-x`), so callers compare
//! the *registered* worktree's canonical path + checked-out branch, and this module
//! parses `git worktree list --porcelain -z` (NUL-delimited, unquoted paths) so paths
//! with spaces / non-UTF-8 bytes survive intact.

use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Default worktree directory naming pattern. Tokens: `{repo}` (main worktree dir
/// name), `{branch}` (branch with `/` → `-`).
pub const DEFAULT_NAMING: &str = "{repo}-{branch}";

/// One entry from `git worktree list --porcelain`.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    /// Short branch name (`refs/heads/` stripped), or `None` when detached/bare.
    pub branch: Option<String>,
    /// The main (first) worktree — never a removal target.
    pub is_main: bool,
    /// `git worktree lock`ed — refuse removal up front.
    pub locked: bool,
}

/// The computed placement for a new worktree (from [`plan_path`]).
#[derive(Debug, Clone)]
pub struct Planned {
    pub main_root: PathBuf,
    pub repo_name: String,
    pub dir_name: String,
    /// `main_root.parent()/dir_name` — a validated plain sibling directory.
    pub worktree_path: PathBuf,
}

/// The git toplevel of the worktree containing `start` (may itself be a linked
/// worktree — use [`list_entries`]/[`plan_path`] to reach the MAIN worktree).
pub fn resolve_repo_root(start: &Path) -> Result<PathBuf, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("not a git repository: {}", start.display()));
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Err(format!(
            "could not resolve a repo root from {}",
            start.display()
        ));
    }
    Ok(PathBuf::from(s))
}

/// Canonicalize for identity comparison, falling back to the lexical path when the
/// target doesn't exist (e.g. a just-removed worktree) so callers still get a stable
/// key rather than a hard error.
pub fn canonical_or_lexical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Render the directory name for `{repo}`/`{branch}` (empty pattern → default).
pub fn render_naming(pattern: &str, repo: &str, branch: &str) -> String {
    let effective = if pattern.trim().is_empty() {
        DEFAULT_NAMING
    } else {
        pattern
    };
    let safe_branch = branch.replace('/', "-");
    effective
        .replace("{repo}", repo)
        .replace("{branch}", &safe_branch)
}

/// A rendered name must be exactly one normal path component so the worktree can only
/// land beside the main worktree — never an absolute path or a `../` traversal
/// smuggled in through a custom `naming` pattern.
fn validate_dir_name(name: &str) -> Result<(), String> {
    let mut comps = Path::new(name).components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(format!(
            "worktree name '{name}' is not a plain directory component \
             (check `[worktree] naming` — no '/', '..', or absolute paths)"
        )),
    }
}

/// Compute where a new worktree for `branch` should live, from an already-fetched
/// entry list (so callers make a single `git worktree list` call and reuse it for the
/// identity/recovery checks).
pub fn plan_path(entries: &[Entry], naming: &str, branch: &str) -> Result<Planned, String> {
    if branch.trim().is_empty() {
        return Err("branch name is required".into());
    }
    let main_root = entries
        .iter()
        .find(|e| e.is_main)
        .map(|e| e.path.clone())
        .ok_or_else(|| "could not determine the main worktree".to_string())?;
    let repo_name = main_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
        .to_string();
    let dir_name = render_naming(naming, &repo_name, branch);
    validate_dir_name(&dir_name)?;
    let parent = main_root
        .parent()
        .ok_or_else(|| "the main worktree has no parent directory".to_string())?;
    let worktree_path = parent.join(&dir_name);
    Ok(Planned {
        main_root,
        repo_name,
        dir_name,
        worktree_path,
    })
}

/// `git -C <repo_root> worktree add -b <branch> <worktree_path> [<from>]`. Creates the
/// worktree AND a new branch; git's own output goes to our stderr. This is the sole
/// durable side effect of a create — succeeds iff `git worktree add` succeeds.
pub fn add(repo_root: &Path, worktree_path: &Path, branch: &str, from: &str) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo_root)
        .args(["worktree", "add", "-b", branch])
        .arg(worktree_path);
    if !from.is_empty() {
        cmd.arg(from);
    }
    let out = cmd.output().map_err(|e| format!("git worktree add: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Run a per-repo post-create hook (`bash -c <script>`, cwd = worktree, `WORKTREE_PATH`
/// exported). Best-effort: returns `Some(error)` on failure (the worktree already
/// exists, so a hook failure never rolls it back), `None` on success.
pub fn run_hook(script: &str, worktree_path: &Path) -> Option<String> {
    match Command::new("bash")
        .arg("-c")
        .arg(script)
        .current_dir(worktree_path)
        .env("WORKTREE_PATH", worktree_path)
        .output()
    {
        Ok(o) if o.status.success() => None,
        Ok(o) => Some(format!(
            "post-create hook failed ({}): {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Some(format!("post-create hook could not run: {e}")),
    }
}

/// `git -C <repo_root> worktree remove [--force] <path>`.
pub fn remove(repo_root: &Path, path: &Path, force: bool) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo_root).args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg(path);
    let out = cmd
        .output()
        .map_err(|e| format!("git worktree remove: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `git -C <repo_root> branch -d|-D <branch>` (`-D` when `force`).
pub fn delete_branch(repo_root: &Path, branch: &str, force: bool) -> Result<(), String> {
    let flag = if force { "-D" } else { "-d" };
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["branch", flag, branch])
        .output()
        .map_err(|e| format!("git branch: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

/// Parse `git worktree list` into entries (marks the first/main worktree).
pub fn list_entries(repo_root: &Path) -> Result<Vec<Entry>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()
        .map_err(|e| format!("git worktree list: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_porcelain_z(&out.stdout))
}

/// Parse the NUL-delimited porcelain stream. Attributes are `key[ value]` tokens; an
/// empty token ends a record. Paths are taken as raw bytes (git's `-z` disables
/// quoting) so spaces / non-UTF-8 survive.
fn parse_porcelain_z(data: &[u8]) -> Vec<Entry> {
    struct Build {
        path: PathBuf,
        branch: Option<String>,
        locked: bool,
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut cur: Option<Build> = None;
    let flush = |cur: &mut Option<Build>, entries: &mut Vec<Entry>| {
        if let Some(b) = cur.take() {
            entries.push(Entry {
                path: b.path,
                branch: b.branch,
                is_main: false,
                locked: b.locked,
            });
        }
    };
    for tok in data.split(|&b| b == 0) {
        if tok.is_empty() {
            flush(&mut cur, &mut entries);
            continue;
        }
        let sp = tok.iter().position(|&b| b == b' ');
        let (key, val): (&[u8], &[u8]) = match sp {
            Some(i) => (&tok[..i], &tok[i + 1..]),
            None => (tok, &[]),
        };
        match key {
            b"worktree" => {
                flush(&mut cur, &mut entries);
                cur = Some(Build {
                    path: PathBuf::from(std::ffi::OsStr::from_bytes(val)),
                    branch: None,
                    locked: false,
                });
            }
            b"branch" => {
                if let Some(b) = cur.as_mut() {
                    let s = String::from_utf8_lossy(val);
                    let short = s.strip_prefix("refs/heads/").unwrap_or(&s);
                    b.branch = Some(short.to_string());
                }
            }
            b"locked" => {
                if let Some(b) = cur.as_mut() {
                    b.locked = true;
                }
            }
            _ => {} // HEAD / detached / bare — nothing else is needed here.
        }
    }
    flush(&mut cur, &mut entries);
    // git always lists the main worktree first; express that as the main marker.
    if let Some(first) = entries.first_mut() {
        first.is_main = true;
    }
    entries
}

/// Resolve a `rm` target (a path or a short branch name) to a removable, non-main
/// entry. Path targets resolve against `caller_cwd` (the server's cwd is unrelated),
/// then canonicalize; a branch name matches the short branch. Ambiguous or unmatched
/// targets error rather than pick one.
pub fn resolve_target(entries: &[Entry], target: &str, caller_cwd: &Path) -> Result<Entry, String> {
    let as_path = {
        let p = Path::new(target);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            caller_cwd.join(p)
        };
        canonical_or_lexical(&abs)
    };
    if let Some(e) = entries
        .iter()
        .find(|e| !e.is_main && canonical_or_lexical(&e.path) == as_path)
    {
        return Ok(e.clone());
    }
    let by_branch: Vec<&Entry> = entries
        .iter()
        .filter(|e| !e.is_main && e.branch.as_deref() == Some(target))
        .collect();
    match by_branch.as_slice() {
        [] => Err(format!("no worktree matches '{target}'")),
        [one] => Ok((*one).clone()),
        many => Err(format!(
            "'{target}' is ambiguous ({} worktrees match)",
            many.len()
        )),
    }
}

/// Pure removal preflight shared by the server arm and the no-server local path:
/// resolve the target, refuse a locked worktree (without `--force`), refuse removing
/// the worktree the caller is currently inside (even with `--force`, matching tmx),
/// and refuse `-d` on a detached worktree before anything destructive runs.
pub fn validate_removal(
    entries: &[Entry],
    target: &str,
    caller_cwd: &Path,
    delete_branch: bool,
) -> Result<Entry, String> {
    let entry = resolve_target(entries, target, caller_cwd)?;
    // A locked worktree is refused unconditionally — even `--force` (git itself needs a
    // double `--force` for a locked worktree; unlock deliberately first).
    if entry.locked {
        return Err(format!(
            "worktree {} is locked — unlock it with `git worktree unlock` first",
            entry.path.display()
        ));
    }
    if canonical_or_lexical(caller_cwd).starts_with(canonical_or_lexical(&entry.path)) {
        return Err("refusing to remove the worktree you are currently in".into());
    }
    if delete_branch && entry.branch.is_none() {
        return Err("--delete-branch given, but this worktree is detached (no branch)".into());
    }
    Ok(entry)
}

// --- landing a worktree's branch (`comux worktree land`, decisions #149) ---

/// How a worktree's branch is brought into the main worktree's branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Rebase the branch onto the destination, then fast-forward the destination. Linear
    /// history; refused for a branch carrying merge commits (a plain rebase drops them).
    Rebase,
    /// A merge commit on the destination (`--no-ff --no-edit`).
    Merge,
}

/// Everything `land` resolved up front, pinned so a later step can tell if the world moved.
#[derive(Debug, Clone)]
pub struct LandPlan {
    pub main: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    /// The main worktree's branch at planning time, and its tip then.
    pub into: String,
    pub into_tip: String,
    pub strategy: Strategy,
    /// Commits on `branch` not on `into` (0 = nothing to land, go straight to cleanup).
    pub ahead: u32,
}

/// Run git as a non-interactive tool: no prompt, no editor, nothing on stdin. Returns
/// trimmed stdout, or an error carrying git's own stderr.
fn git_out(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!("git {} failed: {err}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The branch `dir` has checked out, or `None` when detached.
pub fn current_branch(dir: &Path) -> Result<Option<String>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["symbolic-ref", "--short", "-q", "HEAD"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git symbolic-ref: {e}"))?;
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((out.status.success() && !b.is_empty()).then_some(b))
}

/// An operation already in progress in `dir`'s worktree (rebase, merge, cherry-pick,
/// revert), by name. Land never starts on top of one — and never aborts one it did not start.
pub fn operation_in_progress(dir: &Path) -> Result<Option<&'static str>, String> {
    for (marker, what) in [
        ("rebase-merge", "a rebase"),
        ("rebase-apply", "a rebase"),
        ("MERGE_HEAD", "a merge"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
    ] {
        let path = git_out(dir, &["rev-parse", "--git-path", marker])?;
        let path = if Path::new(&path).is_absolute() {
            PathBuf::from(path)
        } else {
            dir.join(path)
        };
        if path.exists() {
            return Ok(Some(what));
        }
    }
    Ok(None)
}

/// Whether `dir` has changes: tracked always, untracked (non-ignored) too when asked.
pub fn is_dirty(dir: &Path, untracked: bool) -> Result<bool, String> {
    let mode = if untracked {
        "--untracked-files=normal"
    } else {
        "--untracked-files=no"
    };
    Ok(!git_out(dir, &["status", "--porcelain", mode])?.is_empty())
}

fn is_ancestor(dir: &Path, a: &str, b: &str) -> Result<bool, String> {
    let st = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["merge-base", "--is-ancestor", a, b])
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("git merge-base: {e}"))?;
    match st.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!("git merge-base --is-ancestor {a} {b} failed")),
    }
}

/// Everything that can refuse, checked BEFORE anything is rewritten.
pub fn plan_land(main: &Path, target: &Entry, strategy: Strategy) -> Result<LandPlan, String> {
    let Some(branch) = target.branch.clone() else {
        return Err("that worktree is detached — nothing to land".into());
    };
    if target.locked {
        return Err(format!(
            "worktree {} is locked — unlock it with `git worktree unlock` first",
            target.path.display()
        ));
    }
    let Some(into) = current_branch(main)? else {
        return Err("the main worktree is detached — check out the branch to land into".into());
    };
    if into == branch {
        return Err(format!("the main worktree is on '{branch}' itself"));
    }
    for (dir, which) in [(main, "main worktree"), (target.path.as_path(), "worktree")] {
        if let Some(op) = operation_in_progress(dir)? {
            return Err(format!(
                "the {which} is in the middle of {op} — finish or abort it first"
            ));
        }
    }
    if is_dirty(&target.path, true)? {
        return Err(format!(
            "{} has uncommitted or untracked changes — commit or stash them (removing it would lose them)",
            target.path.display()
        ));
    }
    if is_dirty(main, false)? {
        return Err(
            "the main worktree has uncommitted changes — commit or stash them first".into(),
        );
    }
    let into_tip = git_out(
        main,
        &["rev-parse", "--verify", &format!("refs/heads/{into}")],
    )?;
    // Fully qualified: a TAG named like the branch would otherwise win the lookup.
    let range = format!("refs/heads/{into}..refs/heads/{branch}");
    let ahead: u32 = git_out(main, &["rev-list", "--count", &range])?
        .parse()
        .unwrap_or(0);
    if strategy == Strategy::Rebase
        && ahead > 0
        && !git_out(main, &["rev-list", "--merges", &range])?.is_empty()
    {
        return Err(format!(
            "'{branch}' carries merge commits a rebase would drop — land it with --merge"
        ));
    }
    Ok(LandPlan {
        main: main.to_path_buf(),
        worktree: target.path.clone(),
        branch,
        into,
        into_tip,
        strategy,
        ahead,
    })
}

/// Bring `plan.branch` into `plan.into`. Re-checks that the destination is still the branch
/// and tip it was planned against right before writing to it. On failure the error says
/// which phase completed — a successful rebase followed by a failed fast-forward leaves the
/// rewritten branch in place, and that has to be said, not hidden.
pub fn perform_land(plan: &LandPlan) -> Result<(), String> {
    if plan.ahead == 0 {
        return Ok(()); // already contained: nothing to bring in
    }
    let pinned = || -> Result<(), String> {
        let now = current_branch(&plan.main)?;
        let tip = git_out(&plan.main, &["rev-parse", "--verify", "HEAD"])?;
        if now.as_deref() != Some(plan.into.as_str()) || tip != plan.into_tip {
            return Err(format!(
                "the main worktree moved since planning (now {} at {}) — nothing was merged",
                now.as_deref().unwrap_or("detached"),
                &tip[..tip.len().min(8)]
            ));
        }
        Ok(())
    };
    match plan.strategy {
        Strategy::Rebase => {
            if let Err(e) = git_out(&plan.worktree, &["rebase", &plan.into_tip]) {
                // Abort ONLY the rebase this call started (none was running at planning).
                let aborted = git_out(&plan.worktree, &["rebase", "--abort"]).is_ok();
                return Err(format!(
                    "rebasing '{}' onto '{}' failed{} — nothing was merged: {e}",
                    plan.branch,
                    plan.into,
                    if aborted {
                        " and was aborted"
                    } else {
                        " (and `git rebase --abort` failed too — check the worktree)"
                    }
                ));
            }
            pinned()?;
            let source = format!("refs/heads/{}", plan.branch);
            git_out(&plan.main, &["merge", "--ff-only", &source]).map_err(|e| {
                format!(
                    "'{}' was rebased onto '{}', but fast-forwarding '{}' failed — the worktree is kept: {e}",
                    plan.branch, plan.into, plan.into
                )
            })?;
        }
        Strategy::Merge => {
            pinned()?;
            let source = format!("refs/heads/{}", plan.branch);
            if let Err(e) = git_out(&plan.main, &["merge", "--no-ff", "--no-edit", &source]) {
                let aborted = git_out(&plan.main, &["merge", "--abort"]).is_ok();
                return Err(format!(
                    "merging '{}' into '{}' failed{}: {e}",
                    plan.branch,
                    plan.into,
                    if aborted {
                        " and was aborted"
                    } else {
                        " (and `git merge --abort` failed too — check the main worktree)"
                    }
                ));
            }
        }
    }
    Ok(())
}

/// Delete `branch` once its tip is PROVEN contained in `into`. Not `branch -d` (it judges
/// against the upstream, which a locally landed branch can be ahead of) and not `-D` after a
/// separate check (another git process could advance the branch in between): the tip is
/// resolved to an OID, that OID is checked, and the ref is deleted ONLY IF it still holds that
/// OID (`update-ref -d <ref> <oid>` is atomic). The branch's config section goes after it.
pub fn delete_landed_branch(main: &Path, branch: &str, into: &str) -> Result<(), String> {
    // `update-ref` has none of `branch -D`'s protection for a branch some OTHER worktree still
    // has checked out (`worktree add --force` allows that): deleting it would leave that
    // worktree's HEAD on a missing ref. Refuse instead.
    if let Some(other) = list_entries(main)?
        .into_iter()
        .find(|e| e.branch.as_deref() == Some(branch))
    {
        return Err(format!(
            "'{branch}' is still checked out in {} — kept",
            other.path.display()
        ));
    }
    let r = format!("refs/heads/{branch}");
    let tip = git_out(main, &["rev-parse", "--verify", &r])?;
    if !is_ancestor(main, &tip, &format!("refs/heads/{into}"))? {
        return Err(format!(
            "'{branch}' has commits not on '{into}' (something committed after landing) — kept"
        ));
    }
    git_out(main, &["update-ref", "-d", &r, &tip])
        .map_err(|e| format!("'{branch}' moved while it was being deleted — kept: {e}"))?;
    let _ = git_out(
        main,
        &["config", "--remove-section", &format!("branch.{branch}")],
    );
    Ok(())
}

/// One land at a time per repository: a lock file in the COMMON git dir, created
/// exclusively and removed on drop.
pub struct LandLock(PathBuf);

impl LandLock {
    pub fn acquire(main: &Path) -> Result<Self, String> {
        let common = git_out(main, &["rev-parse", "--git-common-dir"])?;
        let common = if Path::new(&common).is_absolute() {
            PathBuf::from(common)
        } else {
            main.join(common)
        };
        let path = common.join("comux-land.lock");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| {
                format!(
                    "another `comux worktree land` is running in this repo (or left {} behind)",
                    path.display()
                )
            })?;
        Ok(Self(path))
    }
}

impl Drop for LandLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "comux-wt-test-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed in {}", dir.display());
    }

    fn init_repo() -> PathBuf {
        let dir = tmp_dir("repo");
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("README"), "x").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        dir
    }

    #[test]
    fn naming_default_and_slash() {
        assert_eq!(render_naming("", "copad", "feat/x"), "copad-feat-x");
        assert_eq!(render_naming("{branch}", "copad", "a/b/c"), "a-b-c");
        assert_eq!(render_naming("wt-{repo}", "copad", "x"), "wt-copad");
    }

    #[test]
    fn validate_rejects_traversal_and_absolute() {
        assert!(validate_dir_name("copad-feat-x").is_ok());
        assert!(validate_dir_name("../evil").is_err());
        assert!(validate_dir_name("/abs").is_err());
        assert!(validate_dir_name("a/b").is_err());
        assert!(validate_dir_name("").is_err());
        assert!(validate_dir_name("..").is_err());
    }

    #[test]
    fn plan_path_rejects_traversal_naming() {
        let entries = vec![Entry {
            path: PathBuf::from("/dev/copad"),
            branch: Some("main".into()),
            is_main: true,
            locked: false,
        }];
        assert!(plan_path(&entries, "../{branch}", "x").is_err());
        let ok = plan_path(&entries, DEFAULT_NAMING, "feat/x").unwrap();
        assert_eq!(ok.dir_name, "copad-feat-x");
        assert_eq!(ok.worktree_path, PathBuf::from("/dev/copad-feat-x"));
    }

    #[test]
    fn create_list_remove_roundtrip() {
        let repo = init_repo();
        let entries = list_entries(&repo).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_main);

        let planned = plan_path(&entries, DEFAULT_NAMING, "feat/x").unwrap();
        add(&repo, &planned.worktree_path, "feat/x", "").unwrap();
        assert!(planned.worktree_path.join("README").exists());

        let after = list_entries(&repo).unwrap();
        let wt = after
            .iter()
            .find(|e| !e.is_main)
            .expect("linked worktree present");
        assert_eq!(wt.branch.as_deref(), Some("feat/x"));
        assert_eq!(
            canonical_or_lexical(&wt.path),
            canonical_or_lexical(&planned.worktree_path)
        );

        // resolve by branch and by path both hit the linked worktree.
        assert!(resolve_target(&after, "feat/x", &repo).is_ok());
        assert!(resolve_target(&after, planned.worktree_path.to_str().unwrap(), &repo).is_ok());
        assert!(resolve_target(&after, "nope", &repo).is_err());
        // the main worktree is never a target.
        assert!(resolve_target(&after, "main", &repo).is_err());

        remove(&repo, &planned.worktree_path, false).unwrap();
        assert!(!planned.worktree_path.exists());
        delete_branch(&repo, "feat/x", true).unwrap();

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn validate_removal_guards() {
        let entries = vec![
            Entry {
                path: PathBuf::from("/dev/copad"),
                branch: Some("main".into()),
                is_main: true,
                locked: false,
            },
            Entry {
                path: PathBuf::from("/dev/copad-feat-x"),
                branch: Some("feat-x".into()),
                is_main: false,
                locked: false,
            },
            Entry {
                path: PathBuf::from("/dev/copad-detached"),
                branch: None,
                is_main: false,
                locked: false,
            },
            Entry {
                path: PathBuf::from("/dev/copad-locked"),
                branch: Some("locked".into()),
                is_main: false,
                locked: true,
            },
        ];
        let elsewhere = Path::new("/tmp");
        // main is never removable.
        assert!(validate_removal(&entries, "main", elsewhere, false).is_err());
        // happy path.
        assert!(validate_removal(&entries, "feat-x", elsewhere, false).is_ok());
        // -d on a detached worktree is rejected up front.
        assert!(validate_removal(&entries, "/dev/copad-detached", elsewhere, true).is_err());
        // locked is refused (even a later --force at the git level won't reach it).
        assert!(validate_removal(&entries, "locked", elsewhere, false).is_err());
        // refusing removal of the worktree the caller is inside.
        let inside = Path::new("/dev/copad-feat-x/src");
        assert!(validate_removal(&entries, "feat-x", inside, false).is_err());
    }

    #[test]
    fn hook_success_and_failure() {
        let dir = tmp_dir("hook");
        assert_eq!(run_hook("test -n \"$WORKTREE_PATH\"", &dir), None);
        assert!(run_hook("exit 3", &dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- land ---

    /// A main repo plus a linked worktree on `feat` with one new revision in it.
    fn repo_and_worktree() -> (PathBuf, PathBuf) {
        let main = init_repo();
        let wt = main.with_file_name(format!(
            "{}-wt",
            main.file_name().unwrap().to_string_lossy()
        ));
        git(
            &main,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        std::fs::write(wt.join("feature.txt"), "new\n").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "feature"]);
        (main, wt)
    }

    fn entry_for(main: &Path, wt: &Path) -> Entry {
        let entries = list_entries(main).unwrap();
        resolve_target(&entries, wt.to_str().unwrap(), main).unwrap()
    }

    fn log(dir: &Path) -> String {
        git_out(dir, &["log", "--format=%s"]).unwrap()
    }

    #[test]
    fn rebase_land_fast_forwards_main_and_the_branch_is_then_deletable() {
        let (main, wt) = repo_and_worktree();
        // main moved on too, so the rebase has something to do.
        std::fs::write(main.join("other.txt"), "x").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "main moved"]);
        let plan = plan_land(&main, &entry_for(&main, &wt), Strategy::Rebase).unwrap();
        assert_eq!((plan.into.as_str(), plan.ahead), ("main", 1));
        perform_land(&plan).unwrap();
        let l = log(&main);
        assert!(
            l.starts_with("feature\nmain moved"),
            "linear, feature on top: {l}"
        );
        remove(&main, &wt, false).unwrap();
        delete_landed_branch(&main, "feat", "main").unwrap();
        assert!(
            git_out(&main, &["branch", "--list", "feat"])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn merge_land_makes_a_merge_revision() {
        let (main, wt) = repo_and_worktree();
        std::fs::write(main.join("other.txt"), "x").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "main moved"]);
        let plan = plan_land(&main, &entry_for(&main, &wt), Strategy::Merge).unwrap();
        perform_land(&plan).unwrap();
        let parents = git_out(&main, &["log", "-1", "--format=%p"]).unwrap();
        assert_eq!(parents.split_whitespace().count(), 2, "a merge revision");
    }

    #[test]
    fn a_conflicting_rebase_is_aborted_and_main_is_untouched() {
        let (main, wt) = repo_and_worktree();
        std::fs::write(main.join("feature.txt"), "conflicting\n").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "conflict on main"]);
        let before_main = git_out(&main, &["rev-parse", "HEAD"]).unwrap();
        let before_feat = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();
        let plan = plan_land(&main, &entry_for(&main, &wt), Strategy::Rebase).unwrap();
        let err = perform_land(&plan).unwrap_err();
        assert!(
            err.contains("aborted") && err.contains("nothing was merged"),
            "{err}"
        );
        assert_eq!(git_out(&main, &["rev-parse", "HEAD"]).unwrap(), before_main);
        assert_eq!(git_out(&wt, &["rev-parse", "HEAD"]).unwrap(), before_feat);
        assert_eq!(
            operation_in_progress(&wt).unwrap(),
            None,
            "no rebase left behind"
        );
    }

    #[test]
    fn refusals_happen_before_anything_is_rewritten() {
        let (main, wt) = repo_and_worktree();
        std::fs::write(wt.join("scratch.txt"), "untracked\n").unwrap();
        let err = plan_land(&main, &entry_for(&main, &wt), Strategy::Rebase).unwrap_err();
        assert!(err.contains("untracked"), "{err}");
        std::fs::remove_file(wt.join("scratch.txt")).unwrap();
        std::fs::write(main.join("README"), "dirty").unwrap();
        let err = plan_land(&main, &entry_for(&main, &wt), Strategy::Rebase).unwrap_err();
        assert!(err.contains("main worktree has uncommitted"), "{err}");
    }

    #[test]
    fn a_moved_destination_is_refused_and_a_newer_branch_is_kept() {
        let (main, wt) = repo_and_worktree();
        let plan = plan_land(&main, &entry_for(&main, &wt), Strategy::Merge).unwrap();
        // Someone lands something on main between planning and merging.
        std::fs::write(main.join("late.txt"), "x").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "late"]);
        let err = perform_land(&plan).unwrap_err();
        assert!(err.contains("moved since planning"), "{err}");
        // A branch with commits not on main is never deleted.
        let err = delete_landed_branch(&main, "feat", "main").unwrap_err();
        assert!(err.contains("kept"), "{err}");
    }

    #[test]
    fn a_tag_named_like_the_branch_does_not_redirect_the_land() {
        let (main, wt) = repo_and_worktree();
        // A tag `feat` on main's base: an unqualified `main..feat` would read it as the tag.
        git(&main, &["tag", "feat", "main"]);
        let plan = plan_land(&main, &entry_for(&main, &wt), Strategy::Rebase).unwrap();
        assert_eq!(plan.ahead, 1, "counted against the BRANCH");
        perform_land(&plan).unwrap();
        assert!(log(&main).starts_with("feature"));
    }

    #[test]
    fn a_branch_checked_out_elsewhere_is_never_deleted() {
        let (main, wt) = repo_and_worktree();
        let plan = plan_land(&main, &entry_for(&main, &wt), Strategy::Rebase).unwrap();
        perform_land(&plan).unwrap();
        // A second worktree on the same branch (git allows it with --force).
        let other = wt.with_file_name(format!(
            "{}-again",
            wt.file_name().unwrap().to_string_lossy()
        ));
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "--force",
                other.to_str().unwrap(),
                "feat",
            ],
        );
        remove(&main, &wt, false).unwrap();
        let err = delete_landed_branch(&main, "feat", "main").unwrap_err();
        assert!(err.contains("still checked out"), "{err}");
        assert!(
            !git_out(&main, &["branch", "--list", "feat"])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn one_land_at_a_time() {
        let main = init_repo();
        let first = LandLock::acquire(&main).unwrap();
        assert!(LandLock::acquire(&main).is_err());
        drop(first);
        assert!(LandLock::acquire(&main).is_ok());
    }
}
