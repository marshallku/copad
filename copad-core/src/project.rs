use crate::context::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A `[[projects]]` entry from `~/.config/copad/config.toml`. `git_remote` is a canonical
/// `owner/repo`; `None` means not yet inferred (the registry shells out once at startup).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub subpath: Option<PathBuf>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub git_remote: Option<String>,
}

impl Project {
    pub fn workspace_path(&self) -> PathBuf {
        match &self.subpath {
            Some(sub) => self.path.join(sub),
            None => self.path.clone(),
        }
    }

    pub fn matches_name(&self, name_or_alias: &str) -> bool {
        self.name == name_or_alias || self.aliases.iter().any(|a| a == name_or_alias)
    }
}

/// In-process registry of configured projects (small N, linear scan). Reload requires a restart.
#[derive(Debug, Clone, Default)]
pub struct ProjectRegistry {
    projects: Vec<Project>,
}

impl ProjectRegistry {
    pub fn from_projects(projects: Vec<Project>) -> Self {
        Self { projects }
    }

    /// Shells `git remote get-url origin` once for each project with no `git_remote` and caches
    /// the canonical `owner/repo`. Failure stays `None`.
    pub fn refresh_git_remotes(&mut self) {
        for p in &mut self.projects {
            if p.git_remote.is_none() {
                p.git_remote = infer_git_remote(&p.path);
            }
        }
    }

    pub fn list(&self) -> &[Project] {
        &self.projects
    }

    pub fn resolve_by_git_remote(&self, owner_repo: &str) -> Option<&Project> {
        self.projects
            .iter()
            .find(|p| p.git_remote.as_deref() == Some(owner_repo))
    }

    pub fn resolve_by_name(&self, name_or_alias: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.matches_name(name_or_alias))
    }

    pub fn resolve_by_cwd(&self, cwd: &Path) -> Option<&Project> {
        let mut best: Option<&Project> = None;
        for p in &self.projects {
            if cwd.starts_with(&p.path)
                && best.is_none_or(|b| p.path.as_os_str().len() > b.path.as_os_str().len())
            {
                best = Some(p);
            }
        }
        best
    }

    /// Resolution order for `workflow.run`'s active-project fallback: `ctx.pane_context.git_remote`,
    /// then `ctx.active_cwd`, then `None` (the caller decides whether that is a `require_project`
    /// error or falls back to the active cwd as workspace).
    pub fn resolve_active(&self, ctx: &Context) -> Option<&Project> {
        if let Some(pc) = ctx.pane_context.as_ref()
            && !pc.git_remote.is_empty()
            && let Some(hit) = self.resolve_by_git_remote(&pc.git_remote)
        {
            return Some(hit);
        }
        if let Some(cwd) = ctx.active_cwd.as_ref()
            && let Some(hit) = self.resolve_by_cwd(cwd)
        {
            return Some(hit);
        }
        None
    }
}

fn infer_git_remote(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("remote")
        .arg("get-url")
        .arg("origin")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    canonicalize_remote_url(url.trim())
}

/// `git@github.com:owner/repo.git`, `https://github.com/owner/repo.git` and
/// `https://github.com/owner/repo` all become `owner/repo`; mirrors the regex in
/// `examples/shell/copad-context.zsh`. Trailing slashes and `.git` are trimmed repeatedly so
/// `owner/repo.git/` normalizes too.
fn canonicalize_remote_url(url: &str) -> Option<String> {
    let mut trimmed = url;
    loop {
        let stripped = trimmed.trim_end_matches('/');
        let after_git = stripped.strip_suffix(".git").unwrap_or(stripped);
        if after_git == trimmed {
            break;
        }
        trimmed = after_git;
    }
    let after_sep = trimmed.rsplit(['/', ':']).take(2).collect::<Vec<_>>();
    if after_sep.len() != 2 {
        return None;
    }
    let owner_repo = format!("{}/{}", after_sep[1], after_sep[0]);
    if owner_repo.contains('/') && !owner_repo.starts_with('/') && !owner_repo.ends_with('/') {
        Some(owner_repo)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PaneContext;

    fn p(name: &str, path: &str) -> Project {
        Project {
            name: name.into(),
            path: path.into(),
            ..Default::default()
        }
    }

    #[test]
    fn workspace_path_joins_subpath_when_set() {
        let mut proj = p("monorepo", "/home/me/dev/monorepo");
        proj.subpath = Some("apps/web".into());
        assert_eq!(
            proj.workspace_path(),
            PathBuf::from("/home/me/dev/monorepo/apps/web")
        );
    }

    #[test]
    fn workspace_path_returns_path_when_no_subpath() {
        let proj = p("copad", "/home/me/dev/copad");
        assert_eq!(proj.workspace_path(), PathBuf::from("/home/me/dev/copad"));
    }

    #[test]
    fn matches_name_accepts_canonical_or_alias() {
        let mut proj = p("copad", "/x");
        proj.aliases = vec!["copad-app".into(), "term".into()];
        assert!(proj.matches_name("copad"));
        assert!(proj.matches_name("copad-app"));
        assert!(proj.matches_name("term"));
        assert!(!proj.matches_name("other"));
    }

    #[test]
    fn resolve_by_git_remote_exact_match() {
        let mut a = p("copad", "/x/copad");
        a.git_remote = Some("marshallku/copad".into());
        let mut b = p("life", "/x/life");
        b.git_remote = Some("marshallku/life-assistant".into());
        let reg = ProjectRegistry::from_projects(vec![a, b]);
        assert_eq!(
            reg.resolve_by_git_remote("marshallku/copad").unwrap().name,
            "copad"
        );
        assert_eq!(
            reg.resolve_by_git_remote("marshallku/life-assistant")
                .unwrap()
                .name,
            "life"
        );
        assert!(reg.resolve_by_git_remote("other/repo").is_none());
    }

    #[test]
    fn resolve_by_name_accepts_aliases() {
        let mut a = p("copad", "/x/copad");
        a.aliases = vec!["copad-app".into()];
        let reg = ProjectRegistry::from_projects(vec![a]);
        assert_eq!(reg.resolve_by_name("copad").unwrap().name, "copad");
        assert_eq!(reg.resolve_by_name("copad-app").unwrap().name, "copad");
        assert!(reg.resolve_by_name("nope").is_none());
    }

    #[test]
    fn resolve_by_cwd_walks_up_to_path() {
        let a = p("copad", "/home/me/dev/copad");
        let reg = ProjectRegistry::from_projects(vec![a]);
        assert_eq!(
            reg.resolve_by_cwd(Path::new("/home/me/dev/copad/copad-core/src"))
                .unwrap()
                .name,
            "copad"
        );
        assert_eq!(
            reg.resolve_by_cwd(Path::new("/home/me/dev/copad"))
                .unwrap()
                .name,
            "copad"
        );
        assert!(
            reg.resolve_by_cwd(Path::new("/home/me/dev/other"))
                .is_none()
        );
        assert!(reg.resolve_by_cwd(Path::new("/")).is_none());
    }

    #[test]
    fn resolve_by_cwd_longest_match_wins() {
        let a = p("monorepo", "/home/me/dev/monorepo");
        let b = p("inner", "/home/me/dev/monorepo/apps/web");
        let reg = ProjectRegistry::from_projects(vec![a, b]);
        assert_eq!(
            reg.resolve_by_cwd(Path::new("/home/me/dev/monorepo/apps/web/src"))
                .unwrap()
                .name,
            "inner"
        );
        assert_eq!(
            reg.resolve_by_cwd(Path::new("/home/me/dev/monorepo/other"))
                .unwrap()
                .name,
            "monorepo"
        );
    }

    #[test]
    fn resolve_active_prefers_git_remote_over_cwd() {
        let mut a = p("copad", "/home/me/dev/copad");
        a.git_remote = Some("marshallku/copad".into());
        let mut b = p("other", "/home/me/dev/other");
        b.git_remote = Some("marshallku/other".into());
        let reg = ProjectRegistry::from_projects(vec![a, b]);
        let ctx = Context {
            active_cwd: Some(PathBuf::from("/home/me/dev/other")),
            pane_context: Some(PaneContext {
                git_remote: "marshallku/copad".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(reg.resolve_active(&ctx).unwrap().name, "copad");
    }

    #[test]
    fn resolve_active_falls_through_to_cwd_when_pane_remote_empty() {
        let mut a = p("copad", "/home/me/dev/copad");
        a.git_remote = Some("marshallku/copad".into());
        let reg = ProjectRegistry::from_projects(vec![a]);
        let ctx = Context {
            active_cwd: Some(PathBuf::from("/home/me/dev/copad/copad-core")),
            pane_context: Some(PaneContext::default()),
            ..Default::default()
        };
        assert_eq!(reg.resolve_active(&ctx).unwrap().name, "copad");
    }

    #[test]
    fn resolve_active_returns_none_when_no_signal() {
        let reg = ProjectRegistry::from_projects(vec![p("copad", "/home/me/dev/copad")]);
        let ctx = Context::default();
        assert!(reg.resolve_active(&ctx).is_none());
    }

    #[test]
    fn canonicalize_remote_url_handles_trailing_slash_with_git_suffix_round1_i1() {
        assert_eq!(
            canonicalize_remote_url("https://github.com/owner/repo.git/"),
            Some("owner/repo".into()),
        );
        assert_eq!(
            canonicalize_remote_url("https://github.com/owner/repo/"),
            Some("owner/repo".into()),
        );
        assert_eq!(
            canonicalize_remote_url("git@github.com:owner/repo/"),
            Some("owner/repo".into()),
        );
    }

    #[test]
    fn canonicalize_remote_url_handles_ssh_form() {
        assert_eq!(
            canonicalize_remote_url("git@github.com:marshallku/copad.git"),
            Some("marshallku/copad".into())
        );
    }

    #[test]
    fn canonicalize_remote_url_handles_https_form_with_dot_git() {
        assert_eq!(
            canonicalize_remote_url("https://github.com/marshallku/copad.git"),
            Some("marshallku/copad".into())
        );
    }

    #[test]
    fn canonicalize_remote_url_handles_https_form_without_dot_git() {
        assert_eq!(
            canonicalize_remote_url("https://github.com/marshallku/copad"),
            Some("marshallku/copad".into())
        );
    }

    #[test]
    fn refresh_git_remotes_preserves_explicit_value() {
        let mut a = p("copad", "/nonexistent/path");
        a.git_remote = Some("explicit/value".into());
        let mut reg = ProjectRegistry::from_projects(vec![a]);
        reg.refresh_git_remotes();
        assert_eq!(reg.list()[0].git_remote.as_deref(), Some("explicit/value"));
    }
}
