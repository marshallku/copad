//! Todo file format: markdown checkbox files with YAML-ish
//! frontmatter at `~/docs/todos/<workspace>/<id>.md`.
//!
//! ```text
//! ---
//! id: T-123
//! status: open
//! created: 2026-04-27T12:00:00Z
//! due: 2026-05-01
//! priority: normal
//! workspace: copad
//! linked_jira: PROJ-456
//! linked_kb:
//!   - meetings/abc.md
//! tags: [feature, api]
//! ---
//! body markdown with `- [ ]` subtasks
//! ```
//!
//! File-as-source-of-truth. The plugin parses on read and never
//! caches mutated state — `vim` editing the file mid-session is the
//! supported workflow. Actions write the full file then atomic-rename
//! so a crash never leaves a torn file visible to a concurrent
//! reader (same invariant as `kb.ensure`).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Open,
    InProgress,
    Blocked,
    Done,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::InProgress => "in_progress",
            Status::Blocked => "blocked",
            Status::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Status::Open),
            "in_progress" => Some(Status::InProgress),
            "blocked" => Some(Status::Blocked),
            "done" => Some(Status::Done),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Priority::Low),
            "normal" => Some(Priority::Normal),
            "high" => Some(Priority::High),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub id: String,
    pub status: Status,
    /// RFC 3339 string (not parsed) so a byte-identical vim edit
    /// doesn't re-canonicalize the timestamp.
    pub created: String,
    pub workspace: String,
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub due: Option<String>,
    pub linked_jira: Option<String>,
    pub linked_slack: Vec<Value>,
    pub linked_kb: Vec<String>,
    pub tags: Vec<String>,
    /// Agent-facing instruction that takes precedence over `body` when
    /// assembling the per-Todo prompt layer. `body` stays the
    /// human-readable description; `prompt` is the imperative.
    pub prompt: Option<String>,
}

impl Todo {
    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("id".into(), Value::String(self.id.clone()));
        obj.insert("status".into(), Value::String(self.status.as_str().into()));
        obj.insert("created".into(), Value::String(self.created.clone()));
        obj.insert("workspace".into(), Value::String(self.workspace.clone()));
        obj.insert("title".into(), Value::String(self.title.clone()));
        obj.insert("body".into(), Value::String(self.body.clone()));
        obj.insert(
            "priority".into(),
            Value::String(self.priority.as_str().into()),
        );
        obj.insert(
            "due".into(),
            self.due
                .as_ref()
                .map(|s| Value::String(s.clone()))
                .unwrap_or(Value::Null),
        );
        obj.insert(
            "linked_jira".into(),
            self.linked_jira
                .as_ref()
                .map(|s| Value::String(s.clone()))
                .unwrap_or(Value::Null),
        );
        obj.insert(
            "linked_slack".into(),
            Value::Array(self.linked_slack.clone()),
        );
        obj.insert(
            "linked_kb".into(),
            Value::Array(
                self.linked_kb
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
        obj.insert(
            "tags".into(),
            Value::Array(self.tags.iter().map(|s| Value::String(s.clone())).collect()),
        );
        obj.insert(
            "prompt".into(),
            self.prompt
                .as_ref()
                .map(|s| Value::String(s.clone()))
                .unwrap_or(Value::Null),
        );
        Value::Object(obj)
    }
}

/// File path is the canonical key for `id` and `workspace` — frontmatter
/// values for those are informational only (rename-safe). Files without
/// frontmatter still parse, every field defaulting safely. Title comes
/// from the first non-blank body line (`# ` prefix stripped if present),
/// or the id when the body is empty.
pub fn parse(content: &str, id: &str, workspace: &str) -> Todo {
    let (fm, body_str) = split_frontmatter(content);
    let fm = fm.unwrap_or_default();

    let status = fm
        .get("status")
        .and_then(|v| v.as_str())
        .and_then(Status::parse)
        .unwrap_or(Status::Open);

    let priority = fm
        .get("priority")
        .and_then(|v| v.as_str())
        .and_then(Priority::parse)
        .unwrap_or(Priority::Normal);

    let created = fm
        .get("created")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default();

    let due = fm
        .get("due")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let linked_jira = fm
        .get("linked_jira")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let linked_slack = fm
        .get("linked_slack")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let linked_kb = fm
        .get("linked_kb")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let tags = fm
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let prompt = fm
        .get("prompt")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let (title, body) = derive_title_and_body(body_str, id);

    Todo {
        id: id.to_string(),
        status,
        created,
        workspace: workspace.to_string(),
        title,
        body,
        priority,
        due,
        linked_jira,
        linked_slack,
        linked_kb,
        tags,
        prompt,
    }
}

/// Frontmatter uses inline `[a, b]` for scalar arrays, block form
/// (`linked_slack:\n  - {...}`) for object arrays. Does NOT byte-stable
/// round-trip a previously-loaded file — vim users own the file format.
/// The status-preserving in-place rewrite path is `update_status_in_text`.
pub fn render_new(todo: &Todo) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("id: {}\n", todo.id));
    out.push_str(&format!("status: {}\n", todo.status.as_str()));
    if !todo.created.is_empty() {
        out.push_str(&format!("created: {}\n", todo.created));
    }
    out.push_str(&format!("priority: {}\n", todo.priority.as_str()));
    out.push_str(&format!("workspace: {}\n", todo.workspace));
    if let Some(due) = &todo.due {
        out.push_str(&format!("due: {due}\n"));
    }
    if let Some(jira) = &todo.linked_jira {
        out.push_str(&format!("linked_jira: {jira}\n"));
    }
    if !todo.linked_slack.is_empty() {
        out.push_str("linked_slack:\n");
        for item in &todo.linked_slack {
            out.push_str(&format!(
                "  - {}\n",
                serde_json::to_string(item).unwrap_or_else(|_| "{}".to_string())
            ));
        }
    }
    if !todo.linked_kb.is_empty() {
        out.push_str("linked_kb:\n");
        for path in &todo.linked_kb {
            out.push_str(&format!("  - {path}\n"));
        }
    }
    if !todo.tags.is_empty() {
        out.push_str(&format!("tags: [{}]\n", todo.tags.join(", ")));
    }
    if let Some(p) = &todo.prompt {
        // Multi-line prompts use a YAML literal block scalar so the
        // raw text round-trips without per-line escaping. A single-line
        // prompt could go inline but the block form is uniform and
        // also survives quotes / backslashes the user might put in
        // an agent instruction.
        out.push_str("prompt: |\n");
        for line in p.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("---\n\n");
    out.push_str(&format!("# {}\n", todo.title));
    if !todo.body.is_empty() {
        // `body` is invariant: title-line stripped during parse so
        // it never contains the H1. render_new prepends the H1
        // exactly once and the body picks up after a blank line.
        // This way parse → render → parse round-trips byte-stable
        // for the body field, which the wire payload depends on
        // (`todo.list` / `todo.start_requested` consumers can rely
        // on `body` being just the markdown the user wrote).
        out.push('\n');
        out.push_str(&todo.body);
        if !todo.body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Rewrites the `status:` line only, preserving every other byte
/// (frontmatter ordering, comments, body) as the user last saved it.
/// `None` when the file lacks a `---` fence or a `status:` line — caller
/// falls back to `render_new`.
pub fn update_status_in_text(content: &str, new_status: Status) -> Option<String> {
    let prefix_len = if content.starts_with("---\n") {
        4
    } else if content.starts_with("---\r\n") {
        5
    } else {
        return None;
    };
    let body = &content[prefix_len..];
    // Locate the closing fence.
    let mut offset = 0usize;
    let mut close_at: Option<usize> = None;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            close_at = Some(offset);
            break;
        }
        offset += line.len();
    }
    let block_end = close_at?;
    let block = &body[..block_end];

    let mut new_block = String::with_capacity(block.len());
    let mut found = false;
    for line in block.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if !found
            && let Some((k, _)) = trimmed.split_once(':')
            && k.trim() == "status"
        {
            // Preserve the original line ending.
            let ending = &line[trimmed.len()..];
            new_block.push_str(&format!("status: {}{}", new_status.as_str(), ending));
            found = true;
            continue;
        }
        new_block.push_str(line);
    }
    if !found {
        return None;
    }
    let mut out = String::with_capacity(content.len());
    out.push_str(&content[..prefix_len]);
    out.push_str(&new_block);
    out.push_str(&body[block_end..]);
    Some(out)
}

/// Strips the title line so `parse → render → parse` is byte-stable for
/// `Todo.body` — the field never carries the title and `render_new`
/// re-prepends it. Title is the first non-blank line (`# ` prefix
/// stripped if present); when the body is empty, falls back to the id
/// and returns body unchanged.
fn derive_title_and_body(body_str: &str, id: &str) -> (String, String) {
    // Walk the original `&str` so we can compute byte ranges and
    // splice the title line out cleanly.
    let mut cursor = 0usize;
    while cursor < body_str.len() {
        let rest = &body_str[cursor..];
        let line_end = rest
            .find('\n')
            .map(|n| cursor + n)
            .unwrap_or(body_str.len());
        let line = &body_str[cursor..line_end];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Skip this blank line entirely.
            cursor = line_end + 1;
            continue;
        }
        let title = if let Some(rest) = trimmed.strip_prefix("# ") {
            rest.trim().to_string()
        } else {
            trimmed.to_string()
        };
        // Remove this title line. Drop one trailing newline if
        // present and one further blank line so re-rendering
        // doesn't accumulate blank rows on each round-trip.
        let mut removal_end = line_end;
        if removal_end < body_str.len() && body_str.as_bytes()[removal_end] == b'\n' {
            removal_end += 1;
            if removal_end < body_str.len() && body_str.as_bytes()[removal_end] == b'\n' {
                removal_end += 1;
            }
        }
        let mut new_body = String::with_capacity(body_str.len());
        new_body.push_str(&body_str[..cursor]);
        new_body.push_str(&body_str[removal_end..]);
        return (title, new_body);
    }
    (id.to_string(), body_str.to_string())
}

/// Module-local rather than shared with `copad-plugin-kb`: todo needs
/// block-form arrays (`linked_kb`/`linked_slack`) that the kb parser
/// doesn't support.
fn split_frontmatter(content: &str) -> (Option<Map<String, Value>>, &str) {
    let prefix_len = if content.starts_with("---\n") {
        4
    } else if content.starts_with("---\r\n") {
        5
    } else {
        return (None, content);
    };
    let body = &content[prefix_len..];
    let mut offset = 0usize;
    let mut close_at: Option<usize> = None;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            close_at = Some(offset);
            break;
        }
        offset += line.len();
    }
    let Some(block_end) = close_at else {
        return (None, content);
    };
    let block = &body[..block_end];
    let after_close = &body[block_end..];
    // Skip the closing `---\n` (or `---\r\n`).
    let after = after_close
        .strip_prefix("---\n")
        .or_else(|| after_close.strip_prefix("---\r\n"))
        .or_else(|| after_close.strip_prefix("---"))
        .unwrap_or(after_close);
    // Strip a single leading blank line so the rendered body starts
    // at the user's actual content.
    let after = after.strip_prefix('\n').unwrap_or(after);
    let map = parse_frontmatter_block(block);
    (Some(map), after)
}

fn parse_frontmatter_block(block: &str) -> Map<String, Value> {
    let mut obj = Map::new();
    let mut lines = block.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        let (k, v) = match trimmed.split_once(':') {
            Some(p) => p,
            None => continue,
        };
        let key = k.trim();
        let val_str = v.trim();
        if key.is_empty() {
            continue;
        }
        if val_str == "|" {
            // YAML literal block scalar: `key: |\n  line1\n  line2`
            // — preserves newlines verbatim. Used for `prompt:` so
            // multi-line agent instructions round-trip without
            // single-line escaping. Each continuation line must be
            // indented (2-space convention as render_new emits).
            let mut buf = String::new();
            while let Some(next) = lines.peek() {
                let nt = next.trim_end_matches(['\n', '\r']);
                let stripped =
                    nt.strip_prefix("  ")
                        .or(if nt.is_empty() { Some("") } else { None });
                let Some(content) = stripped else { break };
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(content);
                lines.next();
            }
            obj.insert(key.to_string(), Value::String(buf));
        } else if val_str.is_empty() {
            // Block-form list collector: `key:\n  - foo\n  - bar`
            let mut arr = Vec::new();
            while let Some(next) = lines.peek() {
                let nt = next.trim_end();
                let stripped = nt.strip_prefix("  - ").or_else(|| nt.strip_prefix("- "));
                let Some(item) = stripped else { break };
                arr.push(parse_yaml_value(item.trim()));
                lines.next();
            }
            obj.insert(key.to_string(), Value::Array(arr));
        } else {
            obj.insert(key.to_string(), parse_yaml_value(val_str));
        }
    }
    obj
}

fn parse_yaml_value(v: &str) -> Value {
    if v.is_empty() {
        return Value::Null;
    }
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        return Value::String(v[1..v.len() - 1].to_string());
    }
    if v.starts_with('[') && v.ends_with(']') {
        let inner = &v[1..v.len() - 1];
        if inner.trim().is_empty() {
            return Value::Array(Vec::new());
        }
        let arr: Vec<Value> = inner
            .split(',')
            .map(|s| parse_yaml_value(s.trim()))
            .collect();
        return Value::Array(arr);
    }
    if v.starts_with('{') && v.ends_with('}') {
        // Inline JSON-ish object — let serde_json take a shot, fall
        // back to verbatim string if it can't parse. Used by
        // `linked_slack` items.
        if let Ok(parsed) = serde_json::from_str::<Value>(v) {
            return parsed;
        }
    }
    if v == "true" {
        return Value::Bool(true);
    }
    if v == "false" {
        return Value::Bool(false);
    }
    if v == "null" || v == "~" {
        return Value::Null;
    }
    if let Ok(n) = v.parse::<i64>() {
        return Value::Number(n.into());
    }
    Value::String(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_no_frontmatter() {
        let body = "# Buy milk\nremember to grab oat too";
        let t = parse(body, "T-1", "default");
        assert_eq!(t.id, "T-1");
        assert_eq!(t.status, Status::Open);
        assert_eq!(t.priority, Priority::Normal);
        assert_eq!(t.title, "Buy milk");
        assert_eq!(t.workspace, "default");
        assert!(t.tags.is_empty());
    }

    #[test]
    fn parse_full_frontmatter() {
        let body = "---\n\
            id: T-42\n\
            status: in_progress\n\
            created: 2026-04-27T10:00:00Z\n\
            priority: high\n\
            workspace: copad\n\
            due: 2026-05-01\n\
            linked_jira: COPAD-1\n\
            linked_kb:\n  - meetings/abc.md\n  - notes/x.md\n\
            tags: [feature, api]\n\
            ---\n\n# Real title\nbody here\n";
        let t = parse(body, "T-42", "copad");
        assert_eq!(t.status, Status::InProgress);
        assert_eq!(t.priority, Priority::High);
        assert_eq!(t.due.as_deref(), Some("2026-05-01"));
        assert_eq!(t.linked_jira.as_deref(), Some("COPAD-1"));
        assert_eq!(
            t.linked_kb,
            vec!["meetings/abc.md".to_string(), "notes/x.md".to_string()]
        );
        assert_eq!(t.tags, vec!["feature".to_string(), "api".to_string()]);
        assert_eq!(t.title, "Real title");
        assert!(t.body.contains("body here"));
    }

    #[test]
    fn parse_unknown_status_falls_back_to_open() {
        let body = "---\nstatus: weird\n---\n# x\n";
        let t = parse(body, "T-X", "default");
        assert_eq!(t.status, Status::Open);
    }

    #[test]
    fn render_new_round_trips_essentials() {
        let t = Todo {
            id: "T-1".into(),
            status: Status::Open,
            created: "2026-04-27T00:00:00Z".into(),
            workspace: "default".into(),
            title: "Hello".into(),
            body: String::new(),
            priority: Priority::Normal,
            due: Some("2026-04-30".into()),
            linked_jira: Some("PROJ-1".into()),
            linked_slack: Vec::new(),
            linked_kb: vec!["meetings/x.md".into()],
            tags: vec!["a".into(), "b".into()],
            prompt: None,
        };
        let s = render_new(&t);
        let parsed = parse(&s, "T-1", "default");
        assert_eq!(parsed.due.as_deref(), Some("2026-04-30"));
        assert_eq!(parsed.linked_jira.as_deref(), Some("PROJ-1"));
        assert_eq!(parsed.linked_kb, vec!["meetings/x.md".to_string()]);
        assert_eq!(parsed.tags, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(parsed.title, "Hello");
    }

    #[test]
    fn update_status_preserves_other_lines() {
        let original = "---\n\
            id: T-1\n\
            status: open\n\
            # comment line\n\
            priority: high\n\
            ---\n\
            body";
        let updated = update_status_in_text(original, Status::Done).unwrap();
        assert!(updated.contains("status: done\n"));
        assert!(updated.contains("# comment line\n"));
        assert!(updated.contains("priority: high\n"));
        assert!(updated.ends_with("body"));
    }

    #[test]
    fn update_status_returns_none_when_no_status_line() {
        let original = "---\nid: T-1\n---\nbody";
        assert!(update_status_in_text(original, Status::Done).is_none());
    }

    #[test]
    fn update_status_returns_none_when_no_frontmatter() {
        assert!(update_status_in_text("# just markdown", Status::Done).is_none());
    }

    #[test]
    fn parse_round_trips_inline_slack_link() {
        let body = "---\n\
            id: T-1\n\
            linked_slack:\n  - {\"team\":\"T0\",\"channel\":\"D1\",\"ts\":\"1700.0\"}\n\
            ---\n\
            ";
        let t = parse(body, "T-1", "default");
        assert_eq!(t.linked_slack.len(), 1);
        assert_eq!(t.linked_slack[0]["team"], "T0");
    }

    #[test]
    fn derive_title_falls_back_to_first_nonempty_line() {
        let t = parse("\n\nfirst line\nsecond", "T-1", "default");
        assert_eq!(t.title, "first line");
    }

    #[test]
    fn derive_title_uses_id_when_body_empty() {
        let t = parse("", "T-99", "default");
        assert_eq!(t.title, "T-99");
    }

    #[test]
    fn parse_strips_title_line_from_body() {
        let raw = "# Real title\n\nactual body content\n";
        let t = parse(raw, "T-1", "default");
        assert_eq!(t.title, "Real title");
        assert_eq!(t.body, "actual body content\n");
    }

    #[test]
    fn parse_render_round_trips_body_field() {
        // The wire payload's body must NOT accumulate H1 lines
        // across write/read cycles. Original C2 cross-review bug.
        let original = Todo {
            id: "T-1".into(),
            status: Status::Open,
            created: "2026-04-27T00:00:00Z".into(),
            workspace: "default".into(),
            title: "Hello".into(),
            body: "actual body content\n".into(),
            priority: Priority::Normal,
            due: None,
            linked_jira: None,
            linked_slack: Vec::new(),
            linked_kb: Vec::new(),
            tags: Vec::new(),
            prompt: None,
        };
        let rendered = render_new(&original);
        let parsed = parse(&rendered, "T-1", "default");
        assert_eq!(parsed.title, "Hello");
        assert_eq!(parsed.body, "actual body content\n");

        let rerendered = render_new(&parsed);
        assert_eq!(
            rendered, rerendered,
            "round-trip 2 must equal round-trip 1 byte-for-byte"
        );
    }

    #[test]
    fn parse_handles_first_line_no_h1() {
        let raw = "first line is the title\n\nrest of body\n";
        let t = parse(raw, "T-1", "default");
        assert_eq!(t.title, "first line is the title");
        assert_eq!(t.body, "rest of body\n");
    }
}
