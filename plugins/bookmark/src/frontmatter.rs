//! Minimal frontmatter reader/writer for bookmark notes. The schema is small and stable (`url`,
//! `title`, `captured_at`, `status`, `source`, `tags`, `linked_kb`, `fetch_error`, `content_type`)
//! and users vim-edit the files, so the reader is lenient instead of using `serde_yaml`.
//!
//! The reader accepts inline arrays (`tags: [a, b]`) and block lists (vim users reflow into that
//! form), strips single/double quotes around scalars, and does no escape handling (quote values
//! containing colons). The writer always emits the inline form.

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum FmValue {
    Scalar(String),
    List(Vec<String>),
}

impl FmValue {
    pub fn as_scalar(&self) -> Option<&str> {
        match self {
            FmValue::Scalar(s) => Some(s.as_str()),
            FmValue::List(_) => None,
        }
    }

    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            FmValue::List(v) => Some(v.as_slice()),
            FmValue::Scalar(_) => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Frontmatter {
    pub fields: BTreeMap<String, FmValue>,
}

impl Frontmatter {
    pub fn get_scalar(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(FmValue::as_scalar)
    }

    pub fn get_list(&self, key: &str) -> Option<&[String]> {
        self.fields.get(key).and_then(FmValue::as_list)
    }

    pub fn set_scalar(&mut self, key: &str, value: impl Into<String>) {
        self.fields
            .insert(key.to_string(), FmValue::Scalar(value.into()));
    }

    pub fn set_list(&mut self, key: &str, value: Vec<String>) {
        self.fields.insert(key.to_string(), FmValue::List(value));
    }
}

/// Split a markdown file into (frontmatter, body); without a leading `---` the whole file is body.
pub fn split(file: &str) -> (Frontmatter, &str) {
    let mut lines = file.split_inclusive('\n');
    let first = match lines.next() {
        Some(l) => l,
        None => return (Frontmatter::default(), ""),
    };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return (Frontmatter::default(), file);
    }

    let mut fm_text = String::new();
    let mut body_start: Option<usize> = None;
    let mut consumed = first.len();
    for line in lines {
        consumed += line.len();
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            body_start = Some(consumed);
            break;
        }
        fm_text.push_str(line);
    }

    let body = match body_start {
        Some(idx) => {
            let raw = &file[idx..];
            // Skip one leading blank line, as markdown formatters do.
            raw.strip_prefix('\n').unwrap_or(raw)
        }
        None => "", // unterminated frontmatter — treat body as empty.
    };

    (parse_frontmatter(&fm_text), body)
}

fn parse_frontmatter(text: &str) -> Frontmatter {
    let mut fm = Frontmatter::default();
    let mut iter = text.lines().peekable();
    while let Some(line) = iter.next() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key_raw, value_raw)) = line.split_once(':') else {
            continue;
        };
        let key = key_raw.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let value_raw = value_raw.trim();

        if value_raw.is_empty() {
            let mut items = Vec::new();
            while let Some(peeked) = iter.peek() {
                let stripped = peeked.trim_start_matches([' ', '\t']);
                if let Some(item) = stripped.strip_prefix("- ") {
                    items.push(strip_quotes(item.trim()).to_string());
                    iter.next();
                } else if peeked.trim().is_empty() {
                    iter.next();
                } else {
                    break;
                }
            }
            if !items.is_empty() {
                fm.fields.insert(key, FmValue::List(items));
            } else {
                fm.fields.insert(key, FmValue::Scalar(String::new()));
            }
            continue;
        }

        if let Some(inner) = value_raw
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
        {
            let items: Vec<String> = if inner.trim().is_empty() {
                Vec::new()
            } else {
                inner
                    .split(',')
                    .map(|s| strip_quotes(s.trim()).to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };
            fm.fields.insert(key, FmValue::List(items));
            continue;
        }

        let scalar = strip_quotes(value_raw).to_string();
        fm.fields.insert(key, FmValue::Scalar(scalar));
    }
    fm
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let first = bytes[0];
        let last = bytes[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Fixed key order, lists always inline. Output: `---\n...---\n\n<body>` (`---\n...---\n` for an empty body).
pub fn render(fm: &Frontmatter, body: &str) -> String {
    // Canonical key order keeps diffs clean after vim edits shuffle them; unknown keys follow
    // alphabetically.
    const PRIMARY: &[&str] = &[
        "url",
        "title",
        "captured_at",
        "source",
        "status",
        "stub_hash",
        "fetch_error",
        "content_type",
        "tags",
        "linked_kb",
    ];

    let mut out = String::from("---\n");
    let mut written = std::collections::HashSet::new();
    for key in PRIMARY {
        if let Some(v) = fm.fields.get(*key) {
            write_kv(&mut out, key, v);
            written.insert(*key);
        }
    }
    for (key, v) in &fm.fields {
        if written.contains(key.as_str()) {
            continue;
        }
        write_kv(&mut out, key, v);
    }
    out.push_str("---\n");
    if !body.is_empty() {
        out.push('\n');
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn write_kv(out: &mut String, key: &str, v: &FmValue) {
    match v {
        FmValue::Scalar(s) => {
            if needs_quote(s) {
                out.push_str(&format!("{key}: {}\n", quote_scalar(s)));
            } else {
                out.push_str(&format!("{key}: {s}\n"));
            }
        }
        FmValue::List(items) => {
            let rendered: Vec<String> = items
                .iter()
                .map(|i| {
                    if needs_quote(i) {
                        quote_scalar(i)
                    } else {
                        i.clone()
                    }
                })
                .collect();
            out.push_str(&format!("{key}: [{}]\n", rendered.join(", ")));
        }
    }
}

fn needs_quote(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    s.chars()
        .any(|c| matches!(c, ':' | '#' | ',' | '[' | ']' | '"' | '\''))
        || s.starts_with(' ')
        || s.ends_with(' ')
}

fn quote_scalar(s: &str) -> String {
    // Double-quote and escape embedded `"`; enough for our controlled inputs.
    format!("\"{}\"", s.replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_no_frontmatter() {
        let (fm, body) = split("hello\nworld\n");
        assert!(fm.fields.is_empty());
        assert_eq!(body, "hello\nworld\n");
    }

    #[test]
    fn split_with_frontmatter() {
        let input = "---\nurl: https://example.com/\ntitle: Example\n---\n\nbody text\n";
        let (fm, body) = split(input);
        assert_eq!(fm.get_scalar("url"), Some("https://example.com/"));
        assert_eq!(fm.get_scalar("title"), Some("Example"));
        assert_eq!(body, "body text\n");
    }

    #[test]
    fn parse_inline_list() {
        let input = "---\ntags: [rust, terminal, plugin]\n---\n";
        let (fm, _) = split(input);
        assert_eq!(
            fm.get_list("tags").unwrap(),
            &[
                "rust".to_string(),
                "terminal".to_string(),
                "plugin".to_string()
            ]
        );
    }

    #[test]
    fn parse_block_list() {
        let input = "---\ntags:\n  - rust\n  - terminal\n---\n";
        let (fm, _) = split(input);
        assert_eq!(
            fm.get_list("tags").unwrap(),
            &["rust".to_string(), "terminal".to_string()]
        );
    }

    #[test]
    fn parse_empty_inline_list() {
        let input = "---\nlinked_kb: []\n---\n";
        let (fm, _) = split(input);
        assert!(fm.get_list("linked_kb").unwrap().is_empty());
    }

    #[test]
    fn parse_strips_quotes() {
        let input = "---\ntitle: \"Hello, World\"\nstatus: 'queued'\n---\n";
        let (fm, _) = split(input);
        assert_eq!(fm.get_scalar("title"), Some("Hello, World"));
        assert_eq!(fm.get_scalar("status"), Some("queued"));
    }

    #[test]
    fn render_round_trip_canonical_order() {
        let mut fm = Frontmatter::default();
        fm.set_scalar("status", "queued");
        fm.set_scalar("url", "https://example.com/");
        fm.set_scalar("title", "Example");
        fm.set_list("tags", vec!["rust".into()]);
        let out = render(&fm, "body\n");
        let url_idx = out.find("url:").unwrap();
        let status_idx = out.find("status:").unwrap();
        let tags_idx = out.find("tags:").unwrap();
        assert!(url_idx < status_idx);
        assert!(status_idx < tags_idx);
    }

    #[test]
    fn render_quotes_when_needed() {
        let mut fm = Frontmatter::default();
        fm.set_scalar("title", "Hello: World");
        let out = render(&fm, "");
        assert!(out.contains("title: \"Hello: World\""));
    }

    #[test]
    fn round_trip_preserves_data() {
        let original = "---\nurl: https://example.com/x\ntitle: \"Hello, World\"\nstatus: queued\ntags: [a, b, c]\nlinked_kb: []\n---\n\nbody line 1\nbody line 2\n";
        let (fm, body) = split(original);
        let rendered = render(&fm, body);
        let (fm2, body2) = split(&rendered);
        assert_eq!(fm2.get_scalar("url"), fm.get_scalar("url"));
        assert_eq!(fm2.get_scalar("title"), fm.get_scalar("title"));
        assert_eq!(fm2.get_list("tags"), fm.get_list("tags"));
        assert_eq!(body2, body);
    }
}
