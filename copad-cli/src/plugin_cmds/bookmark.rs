//! `coctl bookmark` — ergonomic wrapper over the `bookmark.*`
//! action surface (Phase 19, BM-1).
//!
//! Maps clap subcommands onto the existing actions exposed by
//! `copad-plugin-bookmark`:
//!
//! | CLI                                          | Action            |
//! |----------------------------------------------|-------------------|
//! | `bookmark add <url> [--tags ...]`            | `bookmark.add`    |
//! | `bookmark list [--status ...]`               | `bookmark.list`   |
//! | `bookmark show <id|url>`                     | `bookmark.show`   |
//! | `bookmark delete <id|url>`                   | `bookmark.delete` |
//!
//! ## ID resolution
//!
//! Every `<id>` arg accepts a prefix of the 8-char `urlhash8`
//! identifier (≥ 1 hex char). When the prefix matches more than one
//! bookmark we error out with the candidate list — same shape as
//! `coctl todo`. Unlike todo, bookmarks have no workspace concept,
//! so disambiguation is purely "use a longer prefix" or "pass the
//! full URL".
//!
//! `<id>` is parsed locally as hex; if the user passes a URL where an
//! id is expected (e.g. `bookmark show https://example.com/`), we
//! detect the URL form and pass it as `{url: ...}` instead — the
//! plugin canonicalizes and looks up by full hash.

use clap::Subcommand;
use serde_json::{Value, json};

use super::call_and_render;

#[derive(Subcommand, Debug)]
pub enum BookmarkCommand {
    /// Capture a URL as a bookmark note. Idempotent: re-adding an
    /// already-captured URL returns the existing entry.
    Add {
        /// URL (http/https; canonicalized server-side — tracking params
        /// stripped, fragment dropped, host lowercased)
        url: String,
        /// Optional title; otherwise derived from host + last path segment
        #[arg(long)]
        title: Option<String>,
        /// Tags (comma-separated).
        #[arg(long)]
        tags: Option<String>,
        /// Source label for the `source` frontmatter field (free-form;
        /// `cli` / `share` are conventional)
        #[arg(long, default_value = "cli")]
        source: String,
    },
    /// List captured bookmarks (newest first).
    List {
        /// Filter by status. BM-1 only writes `queued` (no fetch
        /// pipeline yet); BM-2 will add `extracted`, `failed`, etc.
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag (single tag — matches bookmarks that
        /// contain it).
        #[arg(long)]
        tag: Option<String>,
        /// Only show bookmarks captured at or after this RFC3339
        /// timestamp (e.g. `2026-05-01T00:00:00+09:00`).
        #[arg(long)]
        since: Option<String>,
        /// Cap the number of rows returned.
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Show a single bookmark (frontmatter + body).
    Show {
        /// Either a urlhash8 prefix (`abcd`) or a full URL.
        id_or_url: String,
    },
    /// Delete a bookmark file.
    Delete {
        /// Either a urlhash8 prefix or a full URL.
        id_or_url: String,
    },
}

pub fn dispatch(cmd: &BookmarkCommand, socket_path: &str, json_out: bool) -> i32 {
    match cmd {
        BookmarkCommand::Add {
            url,
            title,
            tags,
            source,
        } => {
            let mut params = json!({ "url": url, "source": source });
            if let Some(t) = title {
                params["title"] = json!(t);
            }
            if let Some(t) = tags {
                let parts: Vec<String> = t
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !parts.is_empty() {
                    params["tags"] = json!(parts);
                }
            }
            call_and_render(socket_path, "bookmark.add", params, json_out, |v| {
                let id = v.get("id").and_then(Value::as_str).unwrap_or("?");
                let title = v.get("title").and_then(Value::as_str).unwrap_or("");
                let status = v.get("status").and_then(Value::as_str).unwrap_or("?");
                let existed = v.get("existed").and_then(Value::as_bool).unwrap_or(false);
                let prefix = if existed { "exists" } else { "added " };
                println!("{prefix} {id}  [{status}]  {title}");
            })
        }
        BookmarkCommand::List {
            status,
            tag,
            since,
            limit,
        } => {
            let mut params = json!({});
            if let Some(s) = status {
                params["status"] = json!(s);
            }
            if let Some(t) = tag {
                params["tag"] = json!(t);
            }
            if let Some(s) = since {
                params["since"] = json!(s);
            }
            if let Some(n) = limit {
                params["limit"] = json!(n);
            }
            call_and_render(socket_path, "bookmark.list", params, json_out, render_list)
        }
        BookmarkCommand::Show { id_or_url } => {
            let params = id_or_url_params(id_or_url);
            call_and_render(socket_path, "bookmark.show", params, json_out, render_show)
        }
        BookmarkCommand::Delete { id_or_url } => {
            let params = id_or_url_params(id_or_url);
            call_and_render(socket_path, "bookmark.delete", params, json_out, |v| {
                let id = v.get("id").and_then(Value::as_str).unwrap_or("?");
                println!("deleted {id}");
            })
        }
    }
}

/// `http(s)://` (case-insensitive) → URL; otherwise treated as a
/// urlhash8 prefix (server validates hex + existence).
fn id_or_url_params(input: &str) -> Value {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        json!({ "url": trimmed })
    } else {
        json!({ "id": trimmed })
    }
}

fn render_list(v: &Value) {
    let items = v.get("items").and_then(Value::as_array);
    let Some(items) = items else {
        println!("(no items)");
        return;
    };
    if items.is_empty() {
        println!("(no bookmarks)");
        return;
    }
    for item in items {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("?");
        let status = item.get("status").and_then(Value::as_str).unwrap_or("?");
        let title = item.get("title").and_then(Value::as_str).unwrap_or("");
        let url = item.get("url").and_then(Value::as_str).unwrap_or("");
        let tags: Vec<String> = item
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!("  tags={}", tags.join(","))
        };
        println!("{id}  [{status:>9}]  {title}");
        println!("           {url}{tag_str}");
    }
}

fn render_show(v: &Value) {
    let id = v.get("id").and_then(Value::as_str).unwrap_or("?");
    let title = v.get("title").and_then(Value::as_str).unwrap_or("");
    let url = v.get("url").and_then(Value::as_str).unwrap_or("");
    let status = v.get("status").and_then(Value::as_str).unwrap_or("?");
    let captured = v.get("captured_at").and_then(Value::as_str).unwrap_or("");
    let source = v.get("source").and_then(Value::as_str).unwrap_or("");

    println!("{id}  {title}");
    println!("  url      {url}");
    println!("  status   {status}");
    println!("  captured {captured}");
    if !source.is_empty() {
        println!("  source   {source}");
    }
    if let Some(tags) = v.get("tags").and_then(Value::as_array)
        && !tags.is_empty()
    {
        let names: Vec<String> = tags
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        if !names.is_empty() {
            println!("  tags     {}", names.join(", "));
        }
    }
    if let Some(linked) = v.get("linked_kb").and_then(Value::as_array)
        && !linked.is_empty()
    {
        let names: Vec<String> = linked
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        if !names.is_empty() {
            println!("  linked   {}", names.join(", "));
        }
    }
    if let Some(err) = v.get("fetch_error").and_then(Value::as_str)
        && !err.is_empty()
    {
        println!("  error    {err}");
    }
    let body = v.get("body").and_then(Value::as_str).unwrap_or("");
    if !body.trim().is_empty() {
        println!();
        for line in body.lines() {
            println!("  {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_or_url_routes_lowercase_url_as_url() {
        assert_eq!(
            id_or_url_params("https://example.com/x"),
            json!({ "url": "https://example.com/x" })
        );
    }

    #[test]
    fn id_or_url_routes_uppercase_url_as_url() {
        assert_eq!(
            id_or_url_params("HTTPS://example.com/x"),
            json!({ "url": "HTTPS://example.com/x" })
        );
    }

    #[test]
    fn id_or_url_routes_hex_prefix_as_id() {
        assert_eq!(id_or_url_params("abcd1234"), json!({ "id": "abcd1234" }));
        assert_eq!(id_or_url_params("ab"), json!({ "id": "ab" }));
    }
}
