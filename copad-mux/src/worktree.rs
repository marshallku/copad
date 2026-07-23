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
}
