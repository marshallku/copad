use crate::project::Project;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A pre-defined workflow that the user can dispatch via the projects panel
/// or `coctl workflow run`. v1: dispatcher resolves the spec, substitutes
/// `prompt` with form values + project context, then calls `claude.start`.
/// `default_team` / `default_model` are stored but ignored at dispatch
/// until Phase 22.7's pipeline router + Brain dispatcher ship.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowSpec {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub require_project: bool,
    #[serde(default)]
    pub form_fields: Vec<FormField>,
    #[serde(default)]
    pub default_team: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    pub prompt: String,
    /// Caps total wall-clock for the spawned claude turn. v1 emits a
    /// `workflow.timed_out` event when crossed — does NOT kill the subprocess
    /// (hard kill lands in Phase 22.6 with runledger).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Mirrors life-assistant's `ship` contract — side-effecting flows force
    /// a fresh claude session, never attach/resume. Passed through to
    /// `claude.start` as the `fresh_session` param.
    #[serde(default)]
    pub fresh_session: bool,
    #[serde(default = "default_schema_version")]
    pub v: u32,
}

fn default_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FormField {
    pub name: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: FieldKind,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    /// Regex constraint (mirrors life-assistant's `verify` URL field).
    #[serde(default)]
    pub pattern: Option<String>,
    /// Length cap.
    #[serde(default)]
    pub max_length: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    Text,
    Textarea,
    Select,
}

/// In-process map of `id → spec`. Loaded once at daemon startup from
/// `~/.config/copad/workflows/*.yaml`; restart required for reload.
#[derive(Debug, Clone, Default)]
pub struct WorkflowRegistry {
    specs: HashMap<String, WorkflowSpec>,
}

impl WorkflowRegistry {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Walk `dir` for `*.yaml` / `*.yml`, parse each, drop on parse error with a
    /// `log::warn!`. Files whose `id` collides with an existing entry log a warning
    /// and the first-loaded wins.
    pub fn load_from_dir(dir: &Path) -> Self {
        let mut specs = HashMap::new();
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) => {
                log::debug!("workflow: skip {} ({})", dir.display(), e);
                return Self::empty();
            }
        };
        let mut entries: Vec<PathBuf> = read
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| matches!(p.extension().and_then(|s| s.to_str()), Some("yaml" | "yml")))
            .collect();
        entries.sort();
        for path in entries {
            match parse_spec_file(&path) {
                Ok(spec) => {
                    if specs.contains_key(&spec.id) {
                        log::warn!(
                            "workflow: duplicate id '{}' from {}, skipping",
                            spec.id,
                            path.display()
                        );
                        continue;
                    }
                    specs.insert(spec.id.clone(), spec);
                }
                Err(e) => log::warn!("workflow: skip {} ({})", path.display(), e),
            }
        }
        Self { specs }
    }

    pub fn list(&self) -> Vec<&WorkflowSpec> {
        let mut v: Vec<&WorkflowSpec> = self.specs.values().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn get(&self, id: &str) -> Option<&WorkflowSpec> {
        self.specs.get(id)
    }

    pub fn len(&self) -> usize {
        self.specs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }
}

fn parse_spec_file(path: &Path) -> Result<WorkflowSpec, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    let spec: WorkflowSpec = serde_yml::from_str(&content).map_err(|e| format!("parse: {e}"))?;
    if spec.v != 1 {
        return Err(format!(
            "unsupported schema version v={} (only v=1 accepted)",
            spec.v
        ));
    }
    if spec.id.is_empty() {
        return Err("missing id".into());
    }
    if spec.prompt.is_empty() {
        return Err("missing prompt".into());
    }
    Ok(spec)
}

/// Validate `values` against `spec.form_fields`. Errors:
/// - missing required field
/// - unknown field (not in form_fields)
/// - value violates `pattern` (if set)
/// - value exceeds `max_length` (if set)
pub fn validate_values(
    spec: &WorkflowSpec,
    values: &HashMap<String, String>,
) -> Result<(), ValidationError> {
    let known: HashMap<&str, &FormField> = spec
        .form_fields
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();
    for key in values.keys() {
        if !known.contains_key(key.as_str()) {
            return Err(ValidationError::UnknownField(key.clone()));
        }
    }
    for field in &spec.form_fields {
        let provided = values.get(&field.name).map(|s| s.as_str()).unwrap_or("");
        if provided.is_empty() {
            if field.required {
                return Err(ValidationError::MissingRequired(field.name.clone()));
            }
            continue;
        }
        if let Some(max) = field.max_length
            && provided.len() > max
        {
            return Err(ValidationError::MaxLengthExceeded {
                field: field.name.clone(),
                limit: max,
                actual: provided.len(),
            });
        }
        if let Some(pat) = &field.pattern {
            let re = Regex::new(pat).map_err(|e| ValidationError::InvalidPattern {
                field: field.name.clone(),
                pattern: pat.clone(),
                error: e.to_string(),
            })?;
            if !re.is_match(provided) {
                return Err(ValidationError::PatternMismatch {
                    field: field.name.clone(),
                    pattern: pat.clone(),
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ValidationError {
    #[error("missing required field '{0}'")]
    MissingRequired(String),
    #[error("unknown field '{0}'")]
    UnknownField(String),
    #[error("field '{field}' exceeds max_length {limit} (got {actual})")]
    MaxLengthExceeded {
        field: String,
        limit: usize,
        actual: usize,
    },
    #[error("field '{field}' violates pattern '{pattern}'")]
    PatternMismatch { field: String, pattern: String },
    #[error("field '{field}' has invalid pattern '{pattern}': {error}")]
    InvalidPattern {
        field: String,
        pattern: String,
        error: String,
    },
}

/// Substitute `{name}` placeholders in `template`. Recognized names:
/// - `field_name` → matching form-value (empty string if missing + optional)
/// - `project` → `project.name` (empty if no project)
/// - `project.path` → absolute path string (empty if no project)
/// - `project.subpath` → subpath string (empty if no project or no subpath)
/// - `workspace_path` → caller-resolved workspace string (always provided)
///
/// Unknown placeholder names error rather than silently leaving them in —
/// caller should treat this as a spec authoring bug.
pub fn substitute(
    template: &str,
    values: &HashMap<String, String>,
    project: Option<&Project>,
    workspace_path: &Path,
) -> Result<String, SubstituteError> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '{' {
            if let Some(&(_, next)) = chars.peek()
                && next == '{'
            {
                return Err(SubstituteError::EscapeSyntaxUnsupported);
            }
            let mut name = String::new();
            let mut closed = false;
            for (_, nc) in chars.by_ref() {
                if nc == '}' {
                    closed = true;
                    break;
                }
                name.push(nc);
            }
            if !closed {
                return Err(SubstituteError::UnclosedPlaceholder { partial: name });
            }
            let replacement = resolve_placeholder(&name, values, project, workspace_path)
                .ok_or_else(|| SubstituteError::UnknownPlaceholder(name.clone()))?;
            out.push_str(&replacement);
        } else if c == '}' {
            return Err(SubstituteError::StrayClosingBrace);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn resolve_placeholder(
    name: &str,
    values: &HashMap<String, String>,
    project: Option<&Project>,
    workspace_path: &Path,
) -> Option<String> {
    match name {
        "project" => Some(project.map(|p| p.name.clone()).unwrap_or_default()),
        "project.path" => Some(
            project
                .map(|p| p.path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        "project.subpath" => Some(
            project
                .and_then(|p| p.subpath.as_ref())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        "workspace_path" => Some(workspace_path.to_string_lossy().into_owned()),
        _ => values.get(name).cloned(),
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SubstituteError {
    #[error("unknown placeholder '{0}'")]
    UnknownPlaceholder(String),
    #[error("unclosed placeholder '{{{partial}'")]
    UnclosedPlaceholder { partial: String },
    #[error("stray '}}' in template")]
    StrayClosingBrace,
    #[error("'{{{{' escape syntax not supported in v1")]
    EscapeSyntaxUnsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vmap(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    fn write_temp_yaml(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn substitute_replaces_field_placeholders() {
        let values = vmap(&[("branch", "feat/x"), ("ticket", "AB-1")]);
        let out = substitute(
            "ship branch {branch} (ticket {ticket})",
            &values,
            None,
            Path::new("/tmp"),
        )
        .unwrap();
        assert_eq!(out, "ship branch feat/x (ticket AB-1)");
    }

    #[test]
    fn substitute_replaces_project_placeholders() {
        let proj = Project {
            name: "copad".into(),
            path: PathBuf::from("/home/me/dev/copad"),
            subpath: Some(PathBuf::from("apps/web")),
            ..Default::default()
        };
        let out = substitute(
            "in {project} at {project.path}/{project.subpath} (workspace {workspace_path})",
            &HashMap::new(),
            Some(&proj),
            Path::new("/home/me/dev/copad/apps/web"),
        )
        .unwrap();
        assert_eq!(
            out,
            "in copad at /home/me/dev/copad/apps/web (workspace /home/me/dev/copad/apps/web)"
        );
    }

    #[test]
    fn substitute_errors_on_unknown_placeholder() {
        let err = substitute("hello {nope}", &HashMap::new(), None, Path::new("/")).unwrap_err();
        assert!(matches!(err, SubstituteError::UnknownPlaceholder(s) if s == "nope"));
    }

    #[test]
    fn substitute_errors_on_escape_syntax() {
        let err = substitute("hi {{literal}}", &HashMap::new(), None, Path::new("/")).unwrap_err();
        assert!(matches!(err, SubstituteError::EscapeSyntaxUnsupported));
    }

    #[test]
    fn substitute_errors_on_unclosed_placeholder() {
        let err = substitute("hello {name", &HashMap::new(), None, Path::new("/")).unwrap_err();
        assert!(matches!(err, SubstituteError::UnclosedPlaceholder { .. }));
    }

    #[test]
    fn substitute_empty_project_when_none() {
        let out = substitute(
            "p={project} pp={project.path}",
            &HashMap::new(),
            None,
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(out, "p= pp=");
    }

    #[test]
    fn validate_values_accepts_happy_path() {
        let spec = WorkflowSpec {
            id: "x".into(),
            name: "x".into(),
            description: "".into(),
            require_project: false,
            form_fields: vec![FormField {
                name: "branch".into(),
                label: "Branch".into(),
                kind: FieldKind::Text,
                placeholder: None,
                required: true,
                default: None,
                options: vec![],
                pattern: None,
                max_length: None,
            }],
            default_team: None,
            default_model: None,
            prompt: "ship {branch}".into(),
            timeout_secs: None,
            fresh_session: false,
            v: 1,
        };
        assert!(validate_values(&spec, &vmap(&[("branch", "feat/x")])).is_ok());
    }

    #[test]
    fn validate_values_rejects_missing_required() {
        let spec = WorkflowSpec {
            id: "x".into(),
            name: "x".into(),
            description: "".into(),
            require_project: false,
            form_fields: vec![FormField {
                name: "branch".into(),
                label: "Branch".into(),
                kind: FieldKind::Text,
                placeholder: None,
                required: true,
                default: None,
                options: vec![],
                pattern: None,
                max_length: None,
            }],
            default_team: None,
            default_model: None,
            prompt: "".into(),
            timeout_secs: None,
            fresh_session: false,
            v: 1,
        };
        let err = validate_values(&spec, &HashMap::new()).unwrap_err();
        assert!(matches!(err, ValidationError::MissingRequired(s) if s == "branch"));
    }

    #[test]
    fn validate_values_rejects_unknown_field() {
        let spec = WorkflowSpec {
            id: "x".into(),
            name: "x".into(),
            description: "".into(),
            require_project: false,
            form_fields: vec![],
            default_team: None,
            default_model: None,
            prompt: "".into(),
            timeout_secs: None,
            fresh_session: false,
            v: 1,
        };
        let err = validate_values(&spec, &vmap(&[("nope", "x")])).unwrap_err();
        assert!(matches!(err, ValidationError::UnknownField(s) if s == "nope"));
    }

    #[test]
    fn validate_values_enforces_max_length() {
        let spec = WorkflowSpec {
            id: "x".into(),
            name: "x".into(),
            description: "".into(),
            require_project: false,
            form_fields: vec![FormField {
                name: "url".into(),
                label: "URL".into(),
                kind: FieldKind::Text,
                placeholder: None,
                required: true,
                default: None,
                options: vec![],
                pattern: None,
                max_length: Some(10),
            }],
            default_team: None,
            default_model: None,
            prompt: "".into(),
            timeout_secs: None,
            fresh_session: false,
            v: 1,
        };
        let err = validate_values(&spec, &vmap(&[("url", "12345678901")])).unwrap_err();
        assert!(matches!(err, ValidationError::MaxLengthExceeded { .. }));
    }

    #[test]
    fn validate_values_enforces_pattern() {
        let spec = WorkflowSpec {
            id: "verify".into(),
            name: "verify".into(),
            description: "".into(),
            require_project: false,
            form_fields: vec![FormField {
                name: "url".into(),
                label: "URL".into(),
                kind: FieldKind::Text,
                placeholder: None,
                required: true,
                default: None,
                options: vec![],
                pattern: Some(r"^https?://".into()),
                max_length: None,
            }],
            default_team: None,
            default_model: None,
            prompt: "".into(),
            timeout_secs: None,
            fresh_session: false,
            v: 1,
        };
        assert!(validate_values(&spec, &vmap(&[("url", "https://example.com")])).is_ok());
        let err = validate_values(&spec, &vmap(&[("url", "ftp://nope")])).unwrap_err();
        assert!(matches!(err, ValidationError::PatternMismatch { .. }));
    }

    #[test]
    fn load_from_dir_parses_valid_yamls() {
        let dir = tempdir();
        write_temp_yaml(
            &dir,
            "ship.yaml",
            r#"id: ship
name: /ship
description: Run tests then push
require_project: true
fresh_session: true
timeout_secs: 1800
form_fields:
  - name: branch
    label: Branch
    type: text
    required: true
prompt: |
  /ship for branch {branch} in project {project}
"#,
        );
        write_temp_yaml(
            &dir,
            "verify.yaml",
            r#"id: verify
name: /verify
form_fields:
  - name: url
    label: URL
    type: text
    required: true
    pattern: "^https?://"
    max_length: 2048
prompt: "/verify {url}"
"#,
        );
        let reg = WorkflowRegistry::load_from_dir(&dir);
        assert_eq!(reg.len(), 2);
        assert!(reg.get("ship").unwrap().fresh_session);
        assert_eq!(reg.get("ship").unwrap().timeout_secs, Some(1800));
        assert_eq!(
            reg.get("verify").unwrap().form_fields[0].pattern.as_deref(),
            Some("^https?://")
        );
    }

    #[test]
    fn load_from_dir_skips_invalid() {
        let dir = tempdir();
        write_temp_yaml(&dir, "bad.yaml", "this is not yaml: {{{{");
        write_temp_yaml(&dir, "good.yaml", "id: g\nname: G\nprompt: x\nv: 1\n");
        let reg = WorkflowRegistry::load_from_dir(&dir);
        assert_eq!(reg.len(), 1);
        assert!(reg.get("g").is_some());
    }

    #[test]
    fn load_from_dir_rejects_unsupported_schema_version() {
        let dir = tempdir();
        write_temp_yaml(&dir, "future.yaml", "id: f\nname: F\nprompt: x\nv: 99\n");
        let reg = WorkflowRegistry::load_from_dir(&dir);
        assert!(reg.is_empty());
    }

    #[test]
    fn load_from_dir_defaults_schema_version_to_1() {
        let dir = tempdir();
        write_temp_yaml(&dir, "p.yaml", "id: p\nname: P\nprompt: x\n");
        let reg = WorkflowRegistry::load_from_dir(&dir);
        assert_eq!(reg.get("p").unwrap().v, 1);
    }

    #[test]
    fn load_from_dir_handles_missing_dir() {
        let reg = WorkflowRegistry::load_from_dir(Path::new("/nonexistent/path/asdfgh"));
        assert!(reg.is_empty());
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        let suffix: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        p.push(format!("copad-wf-test-{suffix}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
