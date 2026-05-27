//! Action dispatcher for the bookmark plugin.
//!
//! BM-1 surface (this module is what `main.rs` calls into):
//! - `bookmark.add`     — canonicalize URL, dedup by urlhash8, write
//!   queued stub via [`Store::create`]. No fetch yet — BM-2 will spawn
//!   a worker that transitions `status: queued → extracted | failed`.
//! - `bookmark.list`    — walk and filter. Filters: `status`, `tag`,
//!   `since` (RFC3339), `limit`.
//! - `bookmark.show`    — read by `id` (prefix-resolved) or `url`
//!   (canonicalized → exact urlhash8 lookup).
//! - `bookmark.delete`  — same resolution path; unlinks the file.
//!
//! Error contract is `(code, message)` as `(String, String)`, which
//! `main.rs` lifts into the JSON envelope. Codes are stable surface
//! and intentionally short (`invalid_url`, `not_found`,
//! `ambiguous_id`, `io_error`, `invalid_params`, `unsupported_scheme`).

use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde_json::{Value, json};

use crate::canonical::{self, CanonicalError};
use crate::store::{CreateOutcome, CreateRequest, Match, Store, StoreError, slug};

pub struct Bookmark {
    store: Store,
}

impl Bookmark {
    pub fn from_env() -> Result<Self, String> {
        let raw_root = std::env::var("COPAD_BOOKMARK_ROOT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                format!("{home}/docs/bookmarks")
            });
        let root = PathBuf::from(raw_root);
        let store = Store::new(root).map_err(|e| {
            let (_, m) = e.code_message();
            m
        })?;
        Ok(Self { store })
    }

    pub fn root(&self) -> &std::path::Path {
        self.store.root()
    }

    pub fn invoke(&self, action: &str, params: &Value) -> Result<Value, (&'static str, String)> {
        match action {
            "bookmark.add" => self.add(params),
            "bookmark.list" => self.list(params),
            "bookmark.show" => self.show(params),
            "bookmark.delete" => self.delete(params),
            other => Err((
                "unknown_action",
                format!("bookmark plugin does not provide {other}"),
            )),
        }
    }

    fn add(&self, params: &Value) -> Result<Value, (&'static str, String)> {
        let url_str = params
            .get("url")
            .and_then(Value::as_str)
            .ok_or(("invalid_params", "missing 'url'".to_string()))?;
        let canon = canonical::canonicalize(url_str).map_err(canonical_to_action_err)?;

        let title_input = params
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());

        let source = params
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("cli");

        let tags = params
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let title = match title_input {
            Some(t) => t.to_string(),
            None => derive_title_from_url(&canon.url),
        };
        let slug = slug(&title);

        let now: DateTime<Local> = Local::now();
        let outcome = self
            .store
            .create(CreateRequest {
                urlhash8: &canon.urlhash8,
                slug: &slug,
                canonical_url: &canon.url,
                title: &title,
                source,
                tags: &tags,
                now,
            })
            .map_err(store_to_action_err)?;

        let (m, existed) = match outcome {
            CreateOutcome::Created(m) => (m, false),
            CreateOutcome::Existed(m) => (m, true),
        };

        Ok(json!({
            "id": m.id,
            "path": m.path.to_string_lossy(),
            "url": m.url,
            "title": m.title,
            "status": m.status,
            "captured_at": m.captured_at,
            "tags": m.tags,
            "existed": existed,
        }))
    }

    fn list(&self, params: &Value) -> Result<Value, (&'static str, String)> {
        let status_filter = params
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let tag_filter = params
            .get("tag")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let since_filter = params
            .get("since")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);

        let mut all = self.store.list_all();
        if let Some(s) = status_filter {
            all.retain(|m| m.status == s);
        }
        if let Some(tag) = tag_filter {
            all.retain(|m| m.tags.iter().any(|t| t == tag));
        }
        if let Some(since) = since_filter {
            // Parse `since` as RFC3339; lexicographic compare on the raw
            // strings is not chronology-preserving once timezone offsets
            // differ (`...Z` vs `...+09:00` for the same instant). Be
            // strict on user input but lenient on stored captured_at —
            // a hand-edited file with a malformed date is kept rather
            // than silently dropped from the result set.
            let since_dt = DateTime::parse_from_rfc3339(since).map_err(|e| {
                (
                    "invalid_params",
                    format!("--since must be RFC3339; got {since:?}: {e}"),
                )
            })?;
            all.retain(|m| {
                DateTime::parse_from_rfc3339(&m.captured_at)
                    .map(|c| c >= since_dt)
                    .unwrap_or(true)
            });
        }
        if let Some(n) = limit {
            all.truncate(n);
        }

        let items: Vec<Value> = all.into_iter().map(match_to_summary).collect();
        Ok(json!({ "items": items }))
    }

    fn show(&self, params: &Value) -> Result<Value, (&'static str, String)> {
        let m = self.resolve(params)?;
        let (fm, body) = self.store.read_full(&m.path).map_err(store_to_action_err)?;

        let mut payload = match_to_summary(m);
        // Spread frontmatter fields onto the response, plus body.
        let pmap = payload.as_object_mut().unwrap();
        for key in [
            "url",
            "title",
            "captured_at",
            "source",
            "status",
            "fetch_error",
            "content_type",
        ] {
            if let Some(v) = fm.get_scalar(key) {
                pmap.insert(key.to_string(), Value::String(v.to_string()));
            }
        }
        for key in ["tags", "linked_kb"] {
            if let Some(items) = fm.get_list(key) {
                pmap.insert(
                    key.to_string(),
                    Value::Array(items.iter().cloned().map(Value::String).collect()),
                );
            }
        }
        pmap.insert("body".to_string(), Value::String(body));
        Ok(payload)
    }

    fn delete(&self, params: &Value) -> Result<Value, (&'static str, String)> {
        let m = self.resolve(params)?;
        self.store.delete(&m).map_err(store_to_action_err)?;
        Ok(json!({ "ok": true, "id": m.id, "path": m.path.to_string_lossy() }))
    }

    /// `{id}` is a urlhash8 prefix (≥1 hex; ambiguity returns the
    /// candidate list); `{url}` is canonicalized then full-hash matched.
    fn resolve(&self, params: &Value) -> Result<Match, (&'static str, String)> {
        if let Some(id) = params.get("id").and_then(Value::as_str) {
            return self.store.find_by_id(id).map_err(store_to_action_err);
        }
        if let Some(url) = params.get("url").and_then(Value::as_str) {
            let canon = canonical::canonicalize(url).map_err(canonical_to_action_err)?;
            return self.store.find_by_urlhash(&canon.urlhash8).ok_or((
                "not_found",
                format!("no bookmark for url {} ({})", canon.url, canon.urlhash8),
            ));
        }
        Err(("invalid_params", "must supply 'id' or 'url'".to_string()))
    }
}

fn canonical_to_action_err(e: CanonicalError) -> (&'static str, String) {
    let (code, msg) = e.code_message();
    (code, msg)
}

fn store_to_action_err(e: StoreError) -> (&'static str, String) {
    let (code, msg) = e.code_message();
    (code, msg)
}

fn match_to_summary(m: Match) -> Value {
    json!({
        "id": m.id,
        "path": m.path.to_string_lossy(),
        "url": m.url,
        "title": m.title,
        "status": m.status,
        "captured_at": m.captured_at,
        "tags": m.tags,
    })
}

/// When the user didn't supply a title, fall back to the URL host +
/// last path segment so `bookmark list` is at least skim-readable.
fn derive_title_from_url(url: &str) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return url.to_string(),
    };
    let host = parsed.host_str().unwrap_or("(unknown)");
    let last_segment = parsed
        .path_segments()
        .and_then(|mut s| s.next_back().filter(|s| !s.is_empty()).map(String::from));
    match last_segment {
        Some(seg) => format!("{host}/{seg}"),
        None => host.to_string(),
    }
}
