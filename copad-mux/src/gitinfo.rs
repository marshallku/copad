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
    use super::parse_head;

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
