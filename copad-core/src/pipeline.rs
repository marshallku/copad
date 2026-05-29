//! Phase 22.7 — Pipeline (team/role) + Brain dispatcher data model.
//!
//! A `Team` is a named multi-role workflow. Each `Stage` materializes
//! `Role`-defined prompts with outputs from prior stages and
//! dispatches `claude.start` (or `codex.start`) per role. A `Role`
//! carries its model name + prompt template + tool whitelist.
//!
//! v1 ships the data model + 3-tier YAML loader + action surface.
//! Actual execution orchestration (multi-stage with parallel role
//! dispatch, output materialization, stage barriers) is **deferred**
//! — see roadmap.md § 22.7 decisions. Until then, the panel team
//! picker (when wired) prefills role params for the user to dispatch
//! claude/codex sessions manually.
//!
//! Three-tier search at registry build time:
//! 1. Project-local override: `<project_root>/.copad/{teams,roles}/*.yaml`
//! 2. User config: `~/.config/copad/pipeline/{teams,roles}/*.yaml`
//! 3. Builtin: embedded via `include_str!`

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleAssignment {
    pub role: String,
    /// Optional override for the role's default model.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Stage {
    pub name: String,
    pub roles: Vec<RoleAssignment>,
    /// If true, all roles run concurrently; the next stage barriers on
    /// completion of all. If false, roles run sequentially.
    #[serde(default)]
    pub parallel: bool,
    /// Optional relative path (under the mission workspace) where this
    /// stage's combined output is materialized.
    #[serde(default)]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Team {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Role names this team uses. Maps to `Role` entries in the role
    /// registry (looked up by name, tier-resolved).
    pub roles: Vec<String>,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Role {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub model: String,
    pub prompt_template: String,
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug)]
pub struct PipelineRegistry {
    teams: HashMap<String, Team>,
    roles: HashMap<String, Role>,
}

impl PipelineRegistry {
    pub fn new(user_root: &Path, project_root: Option<&Path>) -> Self {
        let mut teams: HashMap<String, Team> = HashMap::new();
        let mut roles: HashMap<String, Role> = HashMap::new();

        // Tier 3 — builtins.
        for (_name, body) in BUILTIN_TEAMS {
            if let Ok(team) = serde_yml::from_str::<Team>(body) {
                teams.insert(team.name.clone(), team);
            }
        }
        for (_name, body) in BUILTIN_ROLES {
            if let Ok(role) = serde_yml::from_str::<Role>(body) {
                roles.insert(role.name.clone(), role);
            }
        }

        // Tier 2 — user config.
        load_dir_into::<Team>(&user_root.join("teams"), &mut teams, |t| &t.name);
        load_dir_into::<Role>(&user_root.join("roles"), &mut roles, |r| &r.name);

        // Tier 1 — project-local override (takes precedence).
        if let Some(p) = project_root {
            load_dir_into::<Team>(&p.join(".copad/teams"), &mut teams, |t| &t.name);
            load_dir_into::<Role>(&p.join(".copad/roles"), &mut roles, |r| &r.name);
        }

        Self { teams, roles }
    }

    pub fn list_teams(&self) -> Vec<Team> {
        let mut v: Vec<Team> = self.teams.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn list_roles(&self) -> Vec<Role> {
        let mut v: Vec<Role> = self.roles.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn team(&self, name: &str) -> Option<Team> {
        self.teams.get(name).cloned()
    }

    pub fn role(&self, name: &str) -> Option<Role> {
        self.roles.get(name).cloned()
    }

    /// Resolve a model-name-prefix to the dispatcher backend. Returns
    /// `("claude.start", model)` or `("codex.start", model)`.
    ///
    /// Recognized prefixes:
    /// - `claude-*`, `opus`, `sonnet`, `haiku` → `claude.start`
    /// - `codex-*`, `gpt-*`, `o1-*` → `codex.start`
    ///
    /// Unknown models fall back to `claude.start` (the original action;
    /// codex routing is opt-in by explicit prefix to avoid silent
    /// re-routing of historic configs).
    pub fn route_model(model: &str) -> (&'static str, String) {
        let lower = model.to_lowercase();
        let is_codex = lower.starts_with("codex-")
            || lower.starts_with("gpt-")
            || lower.starts_with("o1-")
            || lower == "codex";
        if is_codex {
            ("codex.start", model.to_string())
        } else {
            ("claude.start", model.to_string())
        }
    }
}

fn load_dir_into<T: for<'de> Deserialize<'de>>(
    dir: &Path,
    target: &mut HashMap<String, T>,
    name_of: impl Fn(&T) -> &String,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match path.extension().and_then(|s| s.to_str()) {
            Some("yaml") | Some("yml") => {}
            _ => continue,
        }
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Ok(item) = serde_yml::from_str::<T>(&raw) {
            target.insert(name_of(&item).clone(), item);
        }
    }
}

const BUILTIN_TEAMS: &[(&str, &str)] = &[(
    "ship-review",
    include_str!("pipeline/team-ship-review.yaml"),
)];

const BUILTIN_ROLES: &[(&str, &str)] = &[
    ("planner", include_str!("pipeline/role-planner.yaml")),
    (
        "implementer",
        include_str!("pipeline/role-implementer.yaml"),
    ),
    ("reviewer", include_str!("pipeline/role-reviewer.yaml")),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_root(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "copad-pipeline-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            label
        ));
        p
    }

    #[test]
    fn builtins_load() {
        let r = PipelineRegistry::new(&unique_root("builtins"), None);
        assert!(r.team("ship-review").is_some());
        assert!(r.role("planner").is_some());
        assert!(r.role("implementer").is_some());
        assert!(r.role("reviewer").is_some());
    }

    #[test]
    fn user_config_overrides_builtin() {
        let user = unique_root("user");
        fs::create_dir_all(user.join("teams")).unwrap();
        let custom = r#"name: ship-review
description: "user override"
roles:
  - planner
stages:
  - name: solo
    roles:
      - role: planner
"#;
        fs::write(user.join("teams/ship-review.yaml"), custom).unwrap();
        let r = PipelineRegistry::new(&user, None);
        let t = r.team("ship-review").unwrap();
        assert_eq!(t.description, "user override");
        assert_eq!(t.roles, vec!["planner"]);
        let _ = fs::remove_dir_all(&user);
    }

    #[test]
    fn project_override_wins_over_user_config() {
        let user = unique_root("u2");
        let project = unique_root("p2");
        fs::create_dir_all(user.join("teams")).unwrap();
        fs::create_dir_all(project.join(".copad/teams")).unwrap();
        let user_body = r#"name: ship-review
description: "user"
roles: [planner]
stages: []
"#;
        let project_body = r#"name: ship-review
description: "project"
roles: [planner]
stages: []
"#;
        fs::write(user.join("teams/ship-review.yaml"), user_body).unwrap();
        fs::write(project.join(".copad/teams/ship-review.yaml"), project_body).unwrap();
        let r = PipelineRegistry::new(&user, Some(&project));
        let t = r.team("ship-review").unwrap();
        assert_eq!(t.description, "project");
        let _ = fs::remove_dir_all(&user);
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn route_model_dispatches_claude_vs_codex() {
        assert_eq!(
            PipelineRegistry::route_model("claude-sonnet-4-6"),
            ("claude.start", "claude-sonnet-4-6".to_string())
        );
        assert_eq!(
            PipelineRegistry::route_model("opus"),
            ("claude.start", "opus".to_string())
        );
        assert_eq!(
            PipelineRegistry::route_model("codex-1"),
            ("codex.start", "codex-1".to_string())
        );
        assert_eq!(
            PipelineRegistry::route_model("gpt-5"),
            ("codex.start", "gpt-5".to_string())
        );
        // Unknown model falls back to claude.start (no silent codex re-route).
        assert_eq!(
            PipelineRegistry::route_model("custom-model"),
            ("claude.start", "custom-model".to_string())
        );
    }

    #[test]
    fn list_teams_and_roles_sorted() {
        let r = PipelineRegistry::new(&unique_root("sorted"), None);
        let teams = r.list_teams();
        assert!(teams.windows(2).all(|w| w[0].name <= w[1].name));
        let roles = r.list_roles();
        assert!(roles.windows(2).all(|w| w[0].name <= w[1].name));
    }

    #[test]
    fn malformed_user_yaml_is_skipped_silently() {
        let user = unique_root("malformed");
        fs::create_dir_all(user.join("teams")).unwrap();
        fs::write(
            user.join("teams/broken.yaml"),
            "not: valid: team:\n  : whatever",
        )
        .unwrap();
        // Should not panic.
        let r = PipelineRegistry::new(&user, None);
        // Builtins still loaded.
        assert!(r.team("ship-review").is_some());
        let _ = fs::remove_dir_all(&user);
    }
}
