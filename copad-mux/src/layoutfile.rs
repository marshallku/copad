//! Declarative workspaces (decisions #162): a TOML file describing a session — its tabs, how
//! each tab's panes are arranged, and what to type into each — that `comux apply-layout`
//! builds in one go. zellij's layouts and tmuxinator/tmuxp's project files, in comux's terms.
//!
//! ```toml
//! name = "api"                 # the session (default: the file's name)
//! cwd = "~/dev/api"            # where it starts (default: the file's directory)
//!
//! [[tab]]
//! name = "code"
//! layout = "main-vertical"     # any `select-layout` preset; default tiled
//! [[tab.pane]]
//! run = "nvim ."
//! [[tab.pane]]
//! run = ["cargo build", "cargo watch -x test"]
//! [[tab.pane]]                 # a plain shell
//!
//! [[tab]]
//! name = "agent"
//! cwd = "web"                  # relative to the session's cwd
//! [[tab.pane]]
//! run = "claude"
//! ```
//!
//! Every pane is a shell; `run` lines are TYPED into it, as tmuxinator does — so aliases and
//! your rc files apply, the shell's history has them, and what they print stays when they
//! end. This module only parses and validates: it produces a [`Plan`], and the CLI executes
//! it with the same requests a script would send.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::LayoutPreset;

/// The most tabs one file may ask for.
pub const MAX_TABS: usize = 32;
/// The most panes one tab may ask for.
pub const MAX_PANES: usize = 16;

/// A validated layout, every path absolute.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub session: String,
    pub cwd: PathBuf,
    pub tabs: Vec<PlanTab>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanTab {
    pub name: Option<String>,
    pub cwd: PathBuf,
    /// `None` for a one-pane tab: there is nothing to arrange.
    pub layout: Option<LayoutPreset>,
    /// At least one.
    pub panes: Vec<PlanPane>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanPane {
    pub cwd: PathBuf,
    /// Lines to type, each followed by Enter, in order.
    pub run: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    name: Option<String>,
    cwd: Option<String>,
    #[serde(default, rename = "tab")]
    tabs: Vec<RawTab>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTab {
    name: Option<String>,
    cwd: Option<String>,
    layout: Option<String>,
    #[serde(default, rename = "pane")]
    panes: Vec<RawPane>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPane {
    cwd: Option<String>,
    run: Option<Run>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Run {
    One(String),
    Many(Vec<String>),
}

/// Read and validate `path`.
pub fn load(path: &Path) -> Result<Plan, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let base = path
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("layout")
        .to_string();
    parse(&text, &base, &stem).map_err(|e| format!("{}: {e}", path.display()))
}

/// Validate a layout's `text`; relative paths are taken from `base`, and `stem` names the
/// session when the file does not.
pub fn parse(text: &str, base: &Path, stem: &str) -> Result<Plan, String> {
    let raw: RawFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let session = raw
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| stem.to_string());
    let cwd = resolve(base, raw.cwd.as_deref());
    let mut raw_tabs = raw.tabs;
    if raw_tabs.is_empty() {
        raw_tabs.push(RawTab {
            name: None,
            cwd: None,
            layout: None,
            panes: Vec::new(),
        });
    }
    if raw_tabs.len() > MAX_TABS {
        return Err(format!("{} tabs; at most {MAX_TABS}", raw_tabs.len()));
    }
    let mut tabs = Vec::with_capacity(raw_tabs.len());
    for (ti, t) in raw_tabs.into_iter().enumerate() {
        let tab_cwd = resolve(&cwd, t.cwd.as_deref());
        let mut raw_panes = t.panes;
        if raw_panes.is_empty() {
            raw_panes.push(RawPane::default());
        }
        if raw_panes.len() > MAX_PANES {
            return Err(format!(
                "tab {}: {} panes; at most {MAX_PANES}",
                ti + 1,
                raw_panes.len()
            ));
        }
        let layout = match t.layout.as_deref() {
            None => LayoutPreset::Tiled,
            Some(name) => LayoutPreset::from_name(name).ok_or_else(|| {
                format!(
                    "tab {}: unknown layout '{name}' (even-horizontal, even-vertical, \
                     main-horizontal, main-vertical, tiled)",
                    ti + 1
                )
            })?,
        };
        let layout = (raw_panes.len() > 1).then_some(layout);
        let mut panes = Vec::with_capacity(raw_panes.len());
        for (pi, p) in raw_panes.into_iter().enumerate() {
            let run = match p.run {
                None => Vec::new(),
                Some(Run::One(l)) => vec![l],
                Some(Run::Many(v)) => v,
            };
            // One line each: a newline inside would press Enter mid-command, and a control
            // character would type something the file does not show.
            if let Some(bad) = run.iter().find(|l| l.chars().any(char::is_control)) {
                return Err(format!(
                    "tab {} pane {}: `run` lines are single lines without control characters \
                     ({bad:?})",
                    ti + 1,
                    pi + 1
                ));
            }
            panes.push(PlanPane {
                cwd: resolve(&tab_cwd, p.cwd.as_deref()),
                run: run.into_iter().filter(|l| !l.trim().is_empty()).collect(),
            });
        }
        tabs.push(PlanTab {
            name: t
                .name
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty()),
            cwd: tab_cwd,
            layout,
            panes,
        });
    }
    Ok(Plan { session, cwd, tabs })
}

/// `p` against `base`: `~` is the home directory, a relative path joins `base`, absent is
/// `base` itself.
fn resolve(base: &Path, p: Option<&str>) -> PathBuf {
    let Some(p) = p.map(str::trim).filter(|p| !p.is_empty()) else {
        return base.to_path_buf();
    };
    if (p == "~" || p.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty())
    {
        return PathBuf::from(home).join(p.trim_start_matches('~').trim_start_matches('/'));
    }
    let p = Path::new(p);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

impl Plan {
    /// Every directory the plan names that is not an existing directory, in order. The server
    /// falls back to another directory for one it cannot use, and `run` lines would then run
    /// somewhere the file did not say — so a plan with any is refused before anything is built.
    pub fn missing_dirs(&self) -> Vec<&Path> {
        let mut out: Vec<&Path> = Vec::new();
        let dirs = std::iter::once(self.cwd.as_path()).chain(self.tabs.iter().flat_map(|t| {
            std::iter::once(t.cwd.as_path()).chain(t.panes.iter().map(|p| p.cwd.as_path()))
        }));
        for d in dirs {
            if !d.is_dir() && !out.contains(&d) {
                out.push(d);
            }
        }
        out
    }

    /// What applying the plan will build, one line per tab and pane, for `--dry-run`.
    pub fn describe(&self) -> String {
        let mut out = format!("session {} in {}\n", self.session, self.cwd.display());
        for (ti, t) in self.tabs.iter().enumerate() {
            out.push_str(&format!(
                "  tab {}{}{} in {}\n",
                ti + 1,
                t.name
                    .as_ref()
                    .map(|n| format!(" \"{n}\""))
                    .unwrap_or_default(),
                t.layout
                    .map(|l| format!(" ({})", l.name()))
                    .unwrap_or_default(),
                t.cwd.display()
            ));
            for (pi, p) in t.panes.iter().enumerate() {
                let run = if p.run.is_empty() {
                    "a shell".to_string()
                } else {
                    p.run.join(" ; ")
                };
                out.push_str(&format!(
                    "    pane {}: {run}  [{}]\n",
                    pi + 1,
                    p.cwd.display()
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(text: &str) -> Result<Plan, String> {
        parse(text, Path::new("/proj"), "proj")
    }

    #[test]
    fn a_full_file_resolves_every_path_and_line() {
        let p = plan(
            r#"
            name = "api"
            cwd = "/srv/api"
            [[tab]]
            name = "code"
            layout = "main-vertical"
            [[tab.pane]]
            run = "nvim ."
            [[tab.pane]]
            run = ["cargo build", "", "cargo watch -x test"]
            cwd = "crates/core"
            [[tab]]
            cwd = "web"
            [[tab.pane]]
            "#,
        )
        .unwrap();
        assert_eq!(p.session, "api");
        assert_eq!(p.cwd, PathBuf::from("/srv/api"));
        assert_eq!(p.tabs.len(), 2);
        let code = &p.tabs[0];
        assert_eq!(code.name.as_deref(), Some("code"));
        assert_eq!(code.layout, Some(LayoutPreset::MainVertical));
        assert_eq!(code.panes[0].run, ["nvim ."]);
        assert_eq!(
            code.panes[1].run,
            ["cargo build", "cargo watch -x test"],
            "blank lines are dropped"
        );
        assert_eq!(code.panes[1].cwd, PathBuf::from("/srv/api/crates/core"));
        let web = &p.tabs[1];
        assert_eq!(web.cwd, PathBuf::from("/srv/api/web"));
        assert_eq!(web.layout, None, "one pane, nothing to arrange");
        assert!(web.panes[0].run.is_empty());
        assert!(p.describe().contains("pane 1: nvim ."));
    }

    #[test]
    fn an_empty_file_is_one_shell_named_after_the_file_in_its_directory() {
        let p = plan("").unwrap();
        assert_eq!(p.session, "proj");
        assert_eq!(p.cwd, PathBuf::from("/proj"));
        assert_eq!(p.tabs.len(), 1);
        assert_eq!(p.tabs[0].panes.len(), 1);
        // Two panes with no layout given are tiled.
        let p = plan("[[tab]]\n[[tab.pane]]\n[[tab.pane]]\n").unwrap();
        assert_eq!(p.tabs[0].layout, Some(LayoutPreset::Tiled));
    }

    #[test]
    fn directories_that_do_not_exist_are_named() {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let p = parse("[[tab]]\ncwd = \"no-such-dir\"\n[[tab.pane]]\n", &tmp, "x").unwrap();
        assert_eq!(p.missing_dirs(), [tmp.join("no-such-dir").as_path()]);
        assert!(plan_in(&tmp).missing_dirs().is_empty());
    }

    fn plan_in(dir: &Path) -> Plan {
        parse("", dir, "x").unwrap()
    }

    #[test]
    fn mistakes_are_refused_with_where_they_are() {
        assert!(
            plan("[[tab]]\nlayout = \"diagonal\"\n[[tab.pane]]\n[[tab.pane]]\n")
                .unwrap_err()
                .contains("tab 1: unknown layout 'diagonal'")
        );
        assert!(
            plan("[[tab]]\n[[tab.pane]]\nrun = \"ls\\nrm -rf /\"\n")
                .unwrap_err()
                .contains("tab 1 pane 1")
        );
        assert!(plan("colour = \"blue\"").is_err(), "unknown keys are typos");
        let many = "[[tab]]\n".to_string() + &"[[tab.pane]]\n".repeat(MAX_PANES + 1);
        assert!(plan(&many).unwrap_err().contains("at most"));
    }
}
