//! Workspace configuration for the git plugin.
//!
//! Loaded from `~/.config/copad/workspaces.toml` (override with
//! `COPAD_GIT_WORKSPACES_FILE`):
//!
//! ```toml
//! [[workspace]]
//! name = "copad"
//! path = "/home/marshall/dev/copad"
//! default_base = "master"
//! worktree_root = "/home/marshall/dev/copad-worktrees"  # optional
//!
//! [[workspace]]
//! name = "site"
//! path = "/home/marshall/dev/site"
//! default_base = "main"
//! ```
//!
//! Validation: name uses the KB folder charset so it is safe in event payloads / log lines;
//! `path` and `worktree_root` are canonicalized at load time. A missing config file is fine
//! (empty workspace list; `git.list_workspaces` returns `[]`).
//!
//! `default_base` is the branch `worktree_add` uses when the caller omits `base`. It is not
//! checked against disk at load: git's error at the worktree_add call is more precise.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
struct WorkspacesFile {
    #[serde(default, rename = "workspace")]
    workspaces: Vec<WorkspaceEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkspaceEntry {
    name: String,
    path: String,
    /// Semantically required (entry refused if missing/empty) but
    /// deserialized as `Option<String>` so a single malformed block
    /// can't abort `toml::from_str` for the whole file. No silent
    /// default — repos differ (`master`/`trunk`/`develop`), and a
    /// default would branch worktree_add from the wrong base.
    #[serde(default)]
    default_base: Option<String>,
    #[serde(default)]
    worktree_root: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub name: String,
    /// Canonicalized at load time.
    pub path: PathBuf,
    pub default_base: String,
    /// Default `<path>-worktrees`. May not yet exist at load (created
    /// on first `git worktree add`); only the existing prefix is
    /// canonicalized — see `canonicalize_or_normalize`.
    pub worktree_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub workspaces: Vec<Workspace>,
    pub config_path: PathBuf,
    /// Set on malformed entries (bad name, missing/nonexistent path,
    /// name collisions). Workspace-bound actions then surface
    /// `config_error`. "Config file missing" is NOT fatal.
    pub fatal_error: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        Self::from_path(config_path())
    }

    /// Test seam — `COPAD_GIT_WORKSPACES_FILE` is process-wide and
    /// would race under parallel tests.
    pub fn from_path(path: PathBuf) -> Self {
        let mut errors: Vec<String> = Vec::new();
        let mut workspaces: Vec<Workspace> = Vec::new();

        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    workspaces,
                    config_path: path,
                    fatal_error: None,
                };
            }
            Err(e) => {
                return Self {
                    workspaces,
                    config_path: path.clone(),
                    fatal_error: Some(format!("read {}: {e}", path.display())),
                };
            }
        };

        let parsed: WorkspacesFile = match toml::from_str(&raw) {
            Ok(p) => p,
            Err(e) => {
                return Self {
                    workspaces,
                    config_path: path.clone(),
                    fatal_error: Some(format!("parse {}: {e}", path.display())),
                };
            }
        };

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for entry in parsed.workspaces {
            if let Err(e) = validate_name(&entry.name) {
                errors.push(format!("workspace {:?}: {e}", entry.name));
                continue;
            }
            // Defer the duplicate-name guard until the rest of validation passes: otherwise a
            // malformed first block "reserves" the name and a later valid block is rejected as a
            // duplicate, contradicting "bad entry skipped, keep loading". `seen` is re-checked
            // before pushing.
            let raw_path = PathBuf::from(&entry.path);
            // Path MUST exist and be a directory: a stale path would otherwise surface later as
            // confusing `git worktree add` errors.
            let canon_path = match std::fs::canonicalize(&raw_path) {
                Ok(p) => p,
                Err(e) => {
                    errors.push(format!(
                        "workspace {:?} path {}: {e}",
                        entry.name,
                        raw_path.display()
                    ));
                    continue;
                }
            };
            if !canon_path.is_dir() {
                errors.push(format!(
                    "workspace {:?} path {} is not a directory",
                    entry.name,
                    canon_path.display()
                ));
                continue;
            }
            // Probe with `git rev-parse --is-inside-work-tree`: a bare `.git` existence check
            // passes any file named `.git` and misses gitlink files (submodules, secondary
            // worktrees). The fork+exec is a one-time cost at config load.
            let probe = std::process::Command::new("git")
                .arg("-C")
                .arg(&canon_path)
                .arg("rev-parse")
                .arg("--is-inside-work-tree")
                .output();
            let is_repo = match probe {
                Ok(out) => {
                    out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true"
                }
                Err(_) => false,
            };
            if !is_repo {
                errors.push(format!(
                    "workspace {:?} path {} is not a git working tree",
                    entry.name,
                    canon_path.display()
                ));
                continue;
            }
            // Explicit worktree_root must be absolute: the downstream `Path::starts_with`
            // allow-list checks need a stable absolute prefix. The default `<path>-worktrees` is
            // absolute since `path` was canonicalized above.
            let raw_root = match entry.worktree_root.as_deref() {
                Some(s) if !s.is_empty() => {
                    let p = PathBuf::from(s);
                    if !p.is_absolute() {
                        errors.push(format!(
                            "workspace {:?} worktree_root must be absolute, got {s:?}",
                            entry.name
                        ));
                        continue;
                    }
                    p
                }
                _ => default_worktree_root(&canon_path),
            };
            let canon_root = canonicalize_or_normalize(&raw_root);
            let default_base = match entry.default_base {
                Some(s) if !s.is_empty() => s,
                _ => {
                    errors.push(format!(
                        "workspace {:?}: default_base is required (set it to the \
                         branch worktree_add should branch from when no `base` \
                         param is supplied)",
                        entry.name
                    ));
                    continue;
                }
            };
            if !seen.insert(entry.name.clone()) {
                errors.push(format!(
                    "duplicate workspace name {:?} (names must be unique)",
                    entry.name
                ));
                continue;
            }
            // Reject overlap with already-loaded workspaces (any of {path_a, root_a} under any of
            // {path_b, root_b}, either direction). Otherwise longest-prefix owner resolution in
            // `worktree_remove` / `status {path}` is ambiguous and git operations get misrouted.
            let new_paths = [&canon_path, &canon_root];
            let overlap = workspaces.iter().find(|other| {
                let other_paths = [&other.path, &other.worktree_root];
                new_paths.iter().any(|np| {
                    other_paths
                        .iter()
                        .any(|op| np.starts_with(op) || op.starts_with(np))
                })
            });
            if let Some(other) = overlap {
                errors.push(format!(
                    "workspace {:?} overlaps with already-configured workspace {:?} \
                     (one path/worktree_root sits inside the other; configs must be \
                     non-overlapping so worktree ownership is unambiguous)",
                    entry.name, other.name
                ));
                continue;
            }
            workspaces.push(Workspace {
                name: entry.name,
                path: canon_path,
                default_base,
                worktree_root: canon_root,
            });
        }

        let fatal_error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };

        Self {
            workspaces,
            config_path: path,
            fatal_error,
        }
    }

    pub fn find(&self, name: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.name == name)
    }
}

/// Canonicalizes the existing prefix and re-appends the missing tail with `..`/`.` collapsed, so
/// a not-yet-created worktree_root still holds a stable absolute path. Without the collapsing,
/// downstream `Path::starts_with` allow-list checks misbehave on a verbatim `/foo/bar/../baz`.
fn canonicalize_or_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    while !cur.as_os_str().is_empty() {
        if cur.exists()
            && let Ok(c) = std::fs::canonicalize(&cur)
        {
            let mut out = c;
            for t in tail.iter().rev() {
                if t == ".." {
                    out.pop();
                } else if t == "." {
                    // skip
                } else {
                    out.push(t);
                }
            }
            return out;
        }
        match cur.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            None => break,
        }
        if !cur.pop() {
            break;
        }
    }
    // Nothing existed along the way — apply `..` / `.` semantics
    // to the bare input so downstream checks don't have to.
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn default_worktree_root(workspace_path: &Path) -> PathBuf {
    // `<path>-worktrees`, with a hard fallback to `<path>/.worktrees` if the parent / file_name
    // decomposition fails.
    if let (Some(parent), Some(name)) = (workspace_path.parent(), workspace_path.file_name()) {
        let mut suffixed = name.to_os_string();
        suffixed.push("-worktrees");
        parent.join(suffixed)
    } else {
        workspace_path.join(".worktrees")
    }
}

fn config_path() -> PathBuf {
    if let Ok(s) = std::env::var("COPAD_GIT_WORKSPACES_FILE")
        && !s.is_empty()
    {
        return PathBuf::from(s);
    }
    config_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("copad")
        .join("workspaces.toml")
}

fn config_home() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home).join(".config"));
    }
    None
}

/// `[A-Za-z0-9_\-.@]+` minus the reserved `.` / `..` (same charset as KB/todo). Trust boundary:
/// the name appears in event payloads, log lines and derived paths.
pub fn validate_name(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("workspace name cannot be empty".to_string());
    }
    if s == "." || s == ".." {
        return Err(format!("{s:?} is reserved"));
    }
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@');
        if !ok {
            return Err(format!(
                "invalid character {c:?} (allowed: ASCII alphanumeric and _ - . @)"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let s = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "--initial-branch=main", "--quiet"])
            .status()
            .unwrap();
        assert!(s.success());
    }

    #[test]
    fn missing_config_file_is_ok() {
        let dir = tempdir().unwrap();
        let cfg = Config::from_path(dir.path().join("nonexistent.toml"));
        assert!(cfg.workspaces.is_empty());
        assert!(cfg.fatal_error.is_none());
    }

    #[test]
    fn valid_entry_loads_and_canonicalizes() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("myrepo");
        write_repo(&repo);
        let cfg_path = dir.path().join("workspaces.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
[[workspace]]
name = "myrepo"
path = {path:?}
default_base = "main"
"#,
                path = repo.display().to_string()
            ),
        )
        .unwrap();
        let cfg = Config::from_path(cfg_path);
        assert!(cfg.fatal_error.is_none(), "fatal: {:?}", cfg.fatal_error);
        assert_eq!(cfg.workspaces.len(), 1);
        let w = &cfg.workspaces[0];
        assert_eq!(w.name, "myrepo");
        assert!(w.path.is_absolute());
        assert_eq!(w.default_base, "main");
        assert!(
            w.worktree_root
                .to_string_lossy()
                .ends_with("myrepo-worktrees")
        );
    }

    #[test]
    fn non_repo_dir_rejected() {
        let dir = tempdir().unwrap();
        let not_a_repo = dir.path().join("plain");
        std::fs::create_dir_all(&not_a_repo).unwrap();
        let cfg_path = dir.path().join("workspaces.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
[[workspace]]
name = "plain"
path = {path:?}
default_base = "main"
"#,
                path = not_a_repo.display().to_string()
            ),
        )
        .unwrap();
        let cfg = Config::from_path(cfg_path);
        assert!(cfg.workspaces.is_empty());
        assert!(cfg.fatal_error.unwrap().contains("not a git working tree"));
    }

    #[test]
    fn fake_dot_git_file_rejected() {
        // A bogus regular file named `.git` must not pass: the rev-parse probe recognizes it as
        // not a git working tree.
        let dir = tempdir().unwrap();
        let fake = dir.path().join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(fake.join(".git"), "garbage not a real gitlink\n").unwrap();
        let cfg_path = dir.path().join("workspaces.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
[[workspace]]
name = "fake"
path = {path:?}
default_base = "main"
"#,
                path = fake.display().to_string()
            ),
        )
        .unwrap();
        let cfg = Config::from_path(cfg_path);
        assert!(cfg.workspaces.is_empty());
        assert!(cfg.fatal_error.unwrap().contains("not a git working tree"));
    }

    #[test]
    fn duplicate_names_rejected() {
        let dir = tempdir().unwrap();
        let r1 = dir.path().join("r1");
        let r2 = dir.path().join("r2");
        write_repo(&r1);
        write_repo(&r2);
        let cfg_path = dir.path().join("workspaces.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
[[workspace]]
name = "dup"
path = {p1:?}
default_base = "main"

[[workspace]]
name = "dup"
path = {p2:?}
default_base = "main"
"#,
                p1 = r1.display().to_string(),
                p2 = r2.display().to_string(),
            ),
        )
        .unwrap();
        let cfg = Config::from_path(cfg_path);
        assert_eq!(cfg.workspaces.len(), 1);
        assert!(cfg.fatal_error.unwrap().contains("duplicate"));
    }

    #[test]
    fn missing_default_base_skips_only_that_entry() {
        // A single bad entry must NOT brick other valid entries: it surfaces in `fatal_error` and
        // is omitted from `workspaces`, while the good entry still loads.
        let dir = tempdir().unwrap();
        let bad = dir.path().join("bad");
        let good = dir.path().join("good");
        write_repo(&bad);
        write_repo(&good);
        let cfg_path = dir.path().join("workspaces.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
[[workspace]]
name = "bad"
path = {b:?}

[[workspace]]
name = "good"
path = {g:?}
default_base = "main"
"#,
                b = bad.display().to_string(),
                g = good.display().to_string(),
            ),
        )
        .unwrap();
        let cfg = Config::from_path(cfg_path);
        assert_eq!(cfg.workspaces.len(), 1);
        assert_eq!(cfg.workspaces[0].name, "good");
        assert!(
            cfg.fatal_error.unwrap().contains("default_base"),
            "fatal_error should mention the missing field"
        );
    }

    #[test]
    fn relative_worktree_root_rejected() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("r");
        write_repo(&repo);
        let cfg_path = dir.path().join("workspaces.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
[[workspace]]
name = "r"
path = {p:?}
default_base = "main"
worktree_root = "./relative"
"#,
                p = repo.display().to_string(),
            ),
        )
        .unwrap();
        let cfg = Config::from_path(cfg_path);
        assert!(cfg.workspaces.is_empty());
        assert!(
            cfg.fatal_error.unwrap().contains("must be absolute"),
            "should reject relative worktree_root"
        );
    }

    #[test]
    fn bad_name_rejected() {
        for s in ["", "..", ".", "a/b", "x\0y"] {
            assert!(validate_name(s).is_err(), "should reject {s:?}");
        }
        for s in ["main", "site", "a.b-c_d", "scope@team"] {
            validate_name(s).unwrap_or_else(|e| panic!("rejected {s:?}: {e}"));
        }
    }
}
