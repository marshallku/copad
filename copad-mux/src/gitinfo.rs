//! Cheap git-branch lookup for the sidebar `spaces` subtitle — reads `.git/HEAD`
//! directly (no `git` subprocess), walking up from a directory and following a
//! worktree `.git` file. Returns the branch name, or a short SHA for a detached HEAD.

use std::path::Path;

/// The branch (or short detached SHA) of the repo containing `start`, or `None` if
/// `start` isn't inside a git repo.
pub fn branch(start: &Path) -> Option<String> {
    let mut dir = start;
    loop {
        let git = dir.join(".git");
        if git.is_dir() {
            return parse_head(&std::fs::read_to_string(git.join("HEAD")).ok()?);
        }
        if git.is_file() {
            // A worktree/submodule: `.git` is a file `gitdir: <path>` → HEAD lives
            // there. A relative gitdir is relative to THIS dir (git's rule), so
            // `dir.join` — which keeps an absolute path as-is — resolves both.
            let content = std::fs::read_to_string(&git).ok()?;
            let gitdir = content.lines().next()?.strip_prefix("gitdir:")?.trim();
            let head = dir.join(gitdir).join("HEAD");
            return parse_head(&std::fs::read_to_string(head).ok()?);
        }
        dir = dir.parent()?;
    }
}

/// The checkout containing `start`: its top directory and whether it is a LINKED worktree
/// (`git worktree add`), told apart from a submodule or a separate git dir — all three use a
/// `.git` FILE — by its gitdir living under the main repo's `worktrees/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub root: std::path::PathBuf,
    pub linked: bool,
}

pub fn repo(start: &Path) -> Option<Repo> {
    let mut dir = start;
    loop {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(Repo {
                root: dir.to_path_buf(),
                linked: false,
            });
        }
        if git.is_file() {
            let content = std::fs::read_to_string(&git).ok()?;
            let gitdir = content.lines().next()?.strip_prefix("gitdir:")?.trim();
            let linked = Path::new(gitdir)
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == "worktrees");
            return Some(Repo {
                root: dir.to_path_buf(),
                linked,
            });
        }
        dir = dir.parent()?;
    }
}

/// Parse a `.git/HEAD` payload: `ref: refs/heads/<branch>` → branch; else (detached)
/// the first 8 chars of the raw object id.
fn parse_head(head: &str) -> Option<String> {
    let head = head.trim();
    if head.is_empty() {
        return None;
    }
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None => Some(head.chars().take(8).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_head, repo};

    #[test]
    fn a_linked_worktree_is_told_apart_from_a_submodule() {
        let base = std::env::temp_dir().join(format!("comux-gitinfo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let wt = base.join("wt");
        let sub = base.join("sub");
        let main = base.join("main");
        for d in [&wt, &sub, &main.join(".git")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(wt.join(".git"), "gitdir: /r/.git/worktrees/wt\n").unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../main/.git/modules/sub\n").unwrap();
        let deep = wt.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        let r = repo(&deep).unwrap();
        assert_eq!(r.root, wt);
        assert!(r.linked);
        assert!(!repo(&sub).unwrap().linked);
        assert!(!repo(&main).unwrap().linked);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn branch_ref() {
        assert_eq!(
            parse_head("ref: refs/heads/master\n").as_deref(),
            Some("master")
        );
        assert_eq!(
            parse_head("ref: refs/heads/autoresearch/qmp-ra").as_deref(),
            Some("autoresearch/qmp-ra")
        );
    }

    #[test]
    fn detached_head_is_short_sha() {
        assert_eq!(parse_head("a1b2c3d4e5f6\n").as_deref(), Some("a1b2c3d4"));
    }

    #[test]
    fn empty_is_none() {
        assert_eq!(parse_head("  \n"), None);
    }
}
