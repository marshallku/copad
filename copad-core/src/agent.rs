//! Phase 22.5 — Agent profile + autonomy + memory.
//!
//! An `Agent` is a persistent profile consumed by mission turns:
//! `profile.md` is the claude system-prompt fragment, `autonomy.yaml`
//! bounds what the agent may do without approval, `memory.jsonl` is
//! an append-only journal of what the agent has learned. v1 ships
//! read+create — background memory compaction (rolling `summary.md`)
//! lands later.
//!
//! Persistence layout (`~/.local/state/copad/agents/<id>/`):
//! - `profile.md`   — claude system-prompt fragment
//! - `autonomy.yaml` — budget + approval rules
//! - `memory.jsonl` — append-only timeline of memory entries

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Bounded ability declaration. v1 carries the fields most-used by
/// missions today; everything else lives in the YAML as opaque map
/// fields the orchestrator can interpret.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AutonomyConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    /// Action names the agent may invoke without an approval gate.
    #[serde(default)]
    pub auto_actions: Vec<String>,
    /// Wake-budget cap (max turns per 24h). 0 = uncapped.
    #[serde(default)]
    pub max_turns_per_day: u32,
    /// Free-form notes / additional rules the orchestrator should respect.
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryEntry {
    pub timestamp_ms: i64,
    pub kind: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Agent {
    pub id: String,
    pub profile_md: String,
    pub autonomy: AutonomyConfig,
    #[serde(default)]
    pub memory: Vec<MemoryEntry>,
}

#[derive(Debug)]
pub struct AgentRegistry {
    root: PathBuf,
    inner: Mutex<HashMap<String, Agent>>,
}

impl AgentRegistry {
    pub fn new(root: PathBuf) -> Self {
        let inner = Mutex::new(HashMap::new());
        let reg = Self { root, inner };
        reg.seed_builtins();
        reg.reload_from_disk();
        reg
    }

    fn seed_builtins(&self) {
        // Builtins live as include_str! bundles so the binary stays
        // self-contained. User overrides at `~/.local/state/copad/
        // agents/<id>/profile.md` take precedence — `reload_from_disk`
        // runs AFTER this, so any seed entry with the same id is
        // replaced by the user's version on load.
        for (id, profile_md) in BUILTIN_AGENTS {
            let autonomy = AutonomyConfig {
                model: Some("claude-sonnet-4-6".into()),
                tools: vec![],
                auto_actions: vec![],
                max_turns_per_day: 0,
                notes: String::new(),
            };
            let agent = Agent {
                id: (*id).into(),
                profile_md: (*profile_md).into(),
                autonomy,
                memory: Vec::new(),
            };
            self.inner.lock().unwrap().insert((*id).into(), agent);
        }
    }

    pub fn reload_from_disk(&self) {
        let _ = fs::create_dir_all(&self.root);
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let id = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let profile_md = fs::read_to_string(path.join("profile.md")).unwrap_or_default();
            let autonomy: AutonomyConfig = match fs::read_to_string(path.join("autonomy.yaml")) {
                Ok(s) => serde_yml::from_str(&s).unwrap_or_default(),
                Err(_) => AutonomyConfig::default(),
            };
            let memory = read_memory_jsonl(&path.join("memory.jsonl"));
            self.inner.lock().unwrap().insert(
                id.clone(),
                Agent {
                    id,
                    profile_md,
                    autonomy,
                    memory,
                },
            );
        }
    }

    pub fn list(&self) -> Vec<Agent> {
        let mut v: Vec<Agent> = self.inner.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn get(&self, id: &str) -> Option<Agent> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    /// Append a memory entry to the agent's `memory.jsonl` AND the in-memory
    /// list. Single-syscall write via `O_APPEND`.
    ///
    /// **Codex round-9 C1**: validates the id is path-safe AND refers to
    /// a known agent BEFORE any filesystem write. The prior version
    /// joined caller-supplied `id` directly into `self.root` and called
    /// `create_dir_all` — `id: "../outside"` would create directories
    /// outside the agents root and write `memory.jsonl` there.
    pub fn append_memory(
        &self,
        id: &str,
        kind: &str,
        body: &str,
        now_ms: i64,
    ) -> Result<MemoryEntry, String> {
        Self::validate_id(id)?;
        // Verify agent exists in registry (covers builtins + disk-overrides).
        if !self.inner.lock().unwrap().contains_key(id) {
            return Err(format!("agent not found: {id}"));
        }
        let entry = MemoryEntry {
            timestamp_ms: now_ms,
            kind: kind.to_string(),
            body: body.to_string(),
        };
        let dir = self.root.join(id);
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        let path = dir.join("memory.jsonl");
        let line =
            serde_json::to_string(&entry).map_err(|e| format!("serialize memory entry: {e}"))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        writeln!(file, "{line}").map_err(|e| format!("write {}: {e}", path.display()))?;
        let mut guard = self.inner.lock().unwrap();
        if let Some(a) = guard.get_mut(id) {
            a.memory.push(entry.clone());
        }
        Ok(entry)
    }

    /// Path-safe id check: non-empty, no `/`, no `..`, no NUL, no leading `.`.
    fn validate_id(id: &str) -> Result<(), String> {
        if id.is_empty() {
            return Err("agent id cannot be empty".into());
        }
        if id.contains('/') || id.contains('\\') || id.contains('\0') {
            return Err(format!("agent id '{id}' contains path separator or nul"));
        }
        if id == "." || id == ".." || id.starts_with('.') {
            return Err(format!("agent id '{id}' cannot start with '.'"));
        }
        Ok(())
    }
}

fn read_memory_jsonl(path: &Path) -> Vec<MemoryEntry> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(e) = serde_json::from_str::<MemoryEntry>(&line) {
            out.push(e);
        }
    }
    out
}

const BUILTIN_AGENTS: &[(&str, &str)] = &[
    ("architect", include_str!("agents/architect.md")),
    ("api-dev", include_str!("agents/api-dev.md")),
    ("reviewer", include_str!("agents/reviewer.md")),
    ("critic", include_str!("agents/critic.md")),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_root(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "copad-agent-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            label
        ));
        p
    }

    #[test]
    fn builtins_loaded_at_init() {
        let r = AgentRegistry::new(unique_root("init"));
        let ids: Vec<String> = r.list().into_iter().map(|a| a.id).collect();
        assert!(ids.contains(&"architect".to_string()));
        assert!(ids.contains(&"reviewer".to_string()));
        assert!(ids.contains(&"critic".to_string()));
        assert!(ids.contains(&"api-dev".to_string()));
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn append_memory_round_trips_via_disk() {
        let root = unique_root("memrt");
        let r = AgentRegistry::new(root.clone());
        let entry = r.append_memory("architect", "fact", "x is y", 1).unwrap();
        assert_eq!(entry.body, "x is y");
        // Build a new registry from the same disk.
        let r2 = AgentRegistry::new(root.clone());
        let a = r2.get("architect").unwrap();
        assert_eq!(a.memory.len(), 1);
        assert_eq!(a.memory[0].body, "x is y");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn disk_profile_overrides_builtin() {
        let root = unique_root("override");
        fs::create_dir_all(root.join("architect")).unwrap();
        fs::write(root.join("architect/profile.md"), "OVERRIDE BODY").unwrap();
        fs::write(root.join("architect/autonomy.yaml"), "model: opus\n").unwrap();
        let r = AgentRegistry::new(root.clone());
        let a = r.get("architect").unwrap();
        assert_eq!(a.profile_md, "OVERRIDE BODY");
        assert_eq!(a.autonomy.model.as_deref(), Some("opus"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn append_memory_rejects_traversal_round9_c1() {
        let r = AgentRegistry::new(unique_root("traversal"));
        // Even though "../foo" might look like a path, it must be rejected
        // before any fs IO.
        let err = r.append_memory("../outside", "fact", "x", 1).unwrap_err();
        assert!(err.contains("path separator") || err.contains("not found"));
        // NUL byte rejection
        let err = r.append_memory("a\0b", "f", "x", 1).unwrap_err();
        assert!(err.contains("nul") || err.contains("not found"));
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn append_memory_rejects_unknown_agent_round9_c1() {
        let r = AgentRegistry::new(unique_root("unknown"));
        let err = r
            .append_memory("not-a-real-agent", "f", "x", 1)
            .unwrap_err();
        assert!(err.contains("agent not found"));
        let _ = fs::remove_dir_all(&r.root);
    }

    #[test]
    fn missing_agent_returns_none() {
        let r = AgentRegistry::new(unique_root("missing"));
        assert!(r.get("nonexistent").is_none());
        let _ = fs::remove_dir_all(&r.root);
    }
}
