//! Atlassian Cloud REST v3 client. Auth is HTTP Basic with base64 `email:api_token`. The same
//! client serves `auth` (one-shot `/myself`), the polling loop and the action surface.
//!
//! Bodies for `description` / `comment` fields are wrapped in ADF (Atlassian Document Format):
//! Jira Cloud no longer accepts Wiki Markup. The ADF helpers live here so all callers get a
//! consistent wrap shape.

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub account_id: String,
    pub display_name: String,
    pub email_address: String,
}

pub fn basic_auth_header(email: &str, api_token: &str) -> String {
    let raw = format!("{email}:{api_token}");
    format!("Basic {}", base64_encode(raw.as_bytes()))
}

/// `GET /rest/api/3/myself` — credential probe.
pub fn validate_credentials(
    base_url: &str,
    email: &str,
    api_token: &str,
) -> Result<UserInfo, String> {
    let url = join_url(base_url, "/rest/api/3/myself");
    let body = http_get(&url, email, api_token)?;
    parse_myself(&body)
}

pub fn parse_myself(body: &Value) -> Result<UserInfo, String> {
    let account_id = body
        .get("accountId")
        .and_then(Value::as_str)
        .ok_or("/myself response missing accountId")?
        .to_string();
    let display_name = body
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let email_address = body
        .get("emailAddress")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(UserInfo {
        account_id,
        display_name,
        email_address,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct Creds<'a> {
    pub base_url: &'a str,
    pub email: &'a str,
    pub api_token: &'a str,
}

/// JSON GET. Errors use snake_case prefixes for trigger pattern-matching;
/// `Retry-After` is appended verbatim on 429.
pub fn http_get(url: &str, email: &str, api_token: &str) -> Result<Value, String> {
    let resp = match ureq::get(url)
        .set("Authorization", &basic_auth_header(email, api_token))
        .set("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let retry_after = r.header("Retry-After").map(str::to_string);
            let body = r.into_string().unwrap_or_default();
            return Err(classify_status(code, &body, retry_after.as_deref()));
        }
        Err(e) => return Err(format!("transport: {e}")),
    };
    resp.into_json::<Value>()
        .map_err(|e| format!("json parse: {e}"))
}

/// JSON POST. `204 No Content` returns `Value::Null`. Same error
/// classification as `http_get`.
pub fn http_post(creds: Creds, url: &str, body: &Value) -> Result<Value, String> {
    let resp = match ureq::post(url)
        .set(
            "Authorization",
            &basic_auth_header(creds.email, creds.api_token),
        )
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send_json(body.clone())
    {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let retry_after = r.header("Retry-After").map(str::to_string);
            let resp_body = r.into_string().unwrap_or_default();
            return Err(classify_status(code, &resp_body, retry_after.as_deref()));
        }
        Err(e) => return Err(format!("transport: {e}")),
    };
    if resp.status() == 204 {
        return Ok(Value::Null);
    }
    resp.into_json::<Value>()
        .map_err(|e| format!("json parse: {e}"))
}

/// Map HTTP status to a snake_case error prefix. The action dispatcher
/// promotes the leading `[a-z_]+` token to the response's `code` field so
/// triggers can pattern-match without substring searches.
pub fn classify_status(code: u16, body: &str, retry_after: Option<&str>) -> String {
    match code {
        401 => format!("unauthorized HTTP 401: {body}"),
        403 => format!("forbidden HTTP 403: {body}"),
        404 => format!("not_found HTTP 404: {body}"),
        429 => match retry_after {
            Some(s) => format!("rate_limited (Retry-After: {s})"),
            None => "rate_limited".to_string(),
        },
        _ => format!("io_error Jira HTTP {code}: {body}"),
    }
}

pub fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    format!("{base}/{path}")
}

/// Result of a paginated `POST /rest/api/3/search/jql`. The legacy
/// `GET /rest/api/3/search` was deprecated by Atlassian in 2024 (sunset
/// 2025); this client uses the cursor-paginated POST endpoint instead.
#[derive(Debug)]
pub struct SearchResponse {
    pub issues: Vec<Value>,
    /// `None` when no more pages (`isLast: true` or token absent).
    pub next_page_token: Option<String>,
}

/// `fields` filters the response shape; pass `["*all"]` for every field
/// including customs.
pub fn search(
    creds: Creds,
    jql: &str,
    next_page_token: Option<&str>,
    max_results: u64,
    fields: &[&str],
) -> Result<SearchResponse, String> {
    let url = format!(
        "{}/rest/api/3/search/jql",
        creds.base_url.trim_end_matches('/')
    );
    let mut body = serde_json::Map::new();
    body.insert("jql".to_string(), json!(jql));
    body.insert("maxResults".to_string(), json!(max_results));
    if !fields.is_empty() {
        body.insert(
            "fields".to_string(),
            json!(fields.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
        );
    }
    if let Some(token) = next_page_token {
        body.insert("nextPageToken".to_string(), json!(token));
    }
    let resp = http_post(creds, &url, &Value::Object(body))?;
    parse_search_response(&resp)
}

/// Pagination ends on `isLast: true`, or when `nextPageToken` is absent.
/// Missing `isLast` defaults to `false` (defensive: Atlassian may simplify
/// the response shape).
pub fn parse_search_response(body: &Value) -> Result<SearchResponse, String> {
    let issues = body
        .get("issues")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let is_last = body.get("isLast").and_then(Value::as_bool).unwrap_or(false);
    let next_page_token = if is_last {
        None
    } else {
        body.get("nextPageToken")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    Ok(SearchResponse {
        issues,
        next_page_token,
    })
}

/// Returns the verbatim Jira issue payload (all fields + changelog), not the
/// trigger-envelope shape. **Comment cap**: `/issue/{key}` returns up to 50
/// comments inline with no `?maxComments` knob; callers needing more must
/// page via `/issue/{key}/comment`.
pub fn get_issue(creds: Creds, key: &str) -> Result<Value, String> {
    validate_issue_key(key)?;
    let url = format!(
        "{}/rest/api/3/issue/{}?fields=*all&expand=changelog,renderedFields",
        creds.base_url.trim_end_matches('/'),
        urlencode(key),
    );
    http_get(&url, creds.email, creds.api_token)
}

/// `description` is ADF-wrapped here. `assignee_account_id` must be an
/// Atlassian accountId (Jira Cloud dropped email-based assignment in 2018).
/// `issue_type` defaults to `"Task"` — team-managed projects without that
/// type will 400 verbatim, signalling the caller to pass it explicitly.
pub fn create_issue(
    creds: Creds,
    project_key: &str,
    summary: &str,
    description: Option<&str>,
    assignee_account_id: Option<&str>,
    parent_key: Option<&str>,
    issue_type: Option<&str>,
) -> Result<Value, String> {
    validate_project_key(project_key)?;
    if let Some(p) = parent_key {
        validate_issue_key(p)?;
    }
    let mut fields = serde_json::Map::new();
    fields.insert("project".to_string(), json!({ "key": project_key }));
    fields.insert("summary".to_string(), json!(summary));
    fields.insert(
        "issuetype".to_string(),
        json!({ "name": issue_type.unwrap_or("Task") }),
    );
    if let Some(desc) = description {
        fields.insert("description".to_string(), text_to_adf(desc));
    }
    if let Some(acct) = assignee_account_id {
        fields.insert("assignee".to_string(), json!({ "accountId": acct }));
    }
    if let Some(parent) = parent_key {
        fields.insert("parent".to_string(), json!({ "key": parent }));
    }
    let body = json!({ "fields": Value::Object(fields) });
    let url = format!("{}/rest/api/3/issue", creds.base_url.trim_end_matches('/'),);
    http_post(creds, &url, &body)
}

pub fn list_transitions(creds: Creds, key: &str) -> Result<Vec<Value>, String> {
    validate_issue_key(key)?;
    let url = format!(
        "{}/rest/api/3/issue/{}/transitions",
        creds.base_url.trim_end_matches('/'),
        urlencode(key),
    );
    let body = http_get(&url, creds.email, creds.api_token)?;
    Ok(body
        .get("transitions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Looks up the transition by destination status name (case-insensitive) so
/// callers can pass `"In Progress"` without knowing Jira's per-workflow
/// numeric ids. Returns `(from, to)` on success; `transition_not_available`
/// when no transition reaches the requested status from the current state.
pub fn transition(
    creds: Creds,
    key: &str,
    target_status_name: &str,
) -> Result<(String, String), String> {
    validate_issue_key(key)?;
    let issue = get_issue(creds, key)?;
    let from_status = issue
        .get("fields")
        .and_then(|f| f.get("status"))
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let transitions = list_transitions(creds, key)?;
    let target_lower = target_status_name.to_ascii_lowercase();
    let matched = transitions.iter().find(|t| {
        t.get("to")
            .and_then(|to| to.get("name"))
            .and_then(Value::as_str)
            .map(|n| n.to_ascii_lowercase() == target_lower)
            .unwrap_or(false)
    });
    let Some(matched) = matched else {
        let available: Vec<String> = transitions
            .iter()
            .filter_map(|t| {
                t.get("to")
                    .and_then(|to| to.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        return Err(format!(
            "transition_not_available no transition to {target_status_name:?} from current state. \
             Available targets: {available:?}"
        ));
    };
    let trans_id = matched
        .get("id")
        .and_then(Value::as_str)
        .ok_or("transition response missing id")?;
    let resolved_to = matched
        .get("to")
        .and_then(|to| to.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(target_status_name)
        .to_string();
    let url = format!(
        "{}/rest/api/3/issue/{}/transitions",
        creds.base_url.trim_end_matches('/'),
        urlencode(key),
    );
    let body = json!({ "transition": { "id": trans_id } });
    http_post(creds, &url, &body)?;
    Ok((from_status, resolved_to))
}

/// Body is ADF-wrapped here. Returns the new comment's id (the system's
/// dedup key for comments).
pub fn add_comment(creds: Creds, key: &str, body_text: &str) -> Result<String, String> {
    validate_issue_key(key)?;
    let url = format!(
        "{}/rest/api/3/issue/{}/comment",
        creds.base_url.trim_end_matches('/'),
        urlencode(key),
    );
    let body = json!({ "body": text_to_adf(body_text) });
    let resp = http_post(creds, &url, &body)?;
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .ok_or("comment response missing id")?
        .to_string();
    Ok(id)
}

/// `[A-Z][A-Z0-9_]*-[0-9]+`. Trust-boundary defense: a malformed key can't
/// splice path fragments (`"FOO/../bar"`) into the request URL.
pub fn validate_issue_key(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("issue key: cannot be empty".to_string());
    }
    let bytes = s.as_bytes();
    if !bytes[0].is_ascii_uppercase() {
        return Err(format!(
            "issue key {s:?}: must start with an uppercase ASCII letter"
        ));
    }
    let dash_pos = s.find('-').ok_or_else(|| {
        format!("issue key {s:?}: must contain a hyphen separating project and number")
    })?;
    if dash_pos == 0 || dash_pos == s.len() - 1 {
        return Err(format!(
            "issue key {s:?}: hyphen must separate non-empty project and number"
        ));
    }
    let (proj, rest) = s.split_at(dash_pos);
    let num = &rest[1..];
    for &b in proj.as_bytes() {
        if !(b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
            return Err(format!(
                "issue key {s:?}: project segment contains invalid character"
            ));
        }
    }
    for &b in num.as_bytes() {
        if !b.is_ascii_digit() {
            return Err(format!(
                "issue key {s:?}: number segment must be ASCII digits"
            ));
        }
    }
    Ok(())
}

/// `[A-Z][A-Z0-9_]*`. Same trust-boundary defense as `validate_issue_key`.
pub fn validate_project_key(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("project key: cannot be empty".to_string());
    }
    let bytes = s.as_bytes();
    if !bytes[0].is_ascii_uppercase() {
        return Err(format!(
            "project key {s:?}: must start with an uppercase ASCII letter"
        ));
    }
    for &b in bytes {
        if !(b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
            return Err(format!(
                "project key {s:?}: invalid character (allowed: A-Z, 0-9, _)"
            ));
        }
    }
    Ok(())
}

/// Wrap plain text in ADF. Each non-empty line becomes its own `paragraph` so single newlines
/// produce visible breaks. Empty input yields a single empty paragraph (Jira rejects an empty
/// `content` array with 400).
pub fn text_to_adf(text: &str) -> Value {
    let mut content = Vec::new();
    if text.is_empty() {
        content.push(json!({ "type": "paragraph" }));
    } else {
        for line in text.split('\n') {
            if line.is_empty() {
                content.push(json!({ "type": "paragraph" }));
            } else {
                content.push(json!({
                    "type": "paragraph",
                    "content": [{ "type": "text", "text": line }]
                }));
            }
        }
    }
    json!({
        "version": 1,
        "type": "doc",
        "content": content,
    })
}

/// Minimal RFC 3986 percent-encode for path/query segments.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// `(assignee = currentUser() OR watcher = currentUser()) AND updated > -<n>h
/// [AND project in (X, Y)]`. Project keys spliced here are charset-validated
/// at config load — no JQL injection from user-supplied content.
pub fn build_polling_jql(lookback_hours: u32, projects: Option<&[String]>) -> String {
    let mut jql = format!(
        "(assignee = currentUser() OR watcher = currentUser()) AND updated > -{lookback_hours}h"
    );
    if let Some(list) = projects
        && !list.is_empty()
    {
        jql.push_str(" AND project in (");
        for (i, p) in list.iter().enumerate() {
            if i > 0 {
                jql.push_str(", ");
            }
            jql.push_str(p);
        }
        jql.push(')');
    }
    jql.push_str(" ORDER BY updated DESC");
    jql
}

/// RFC 4648 base64 with `=` padding.
pub fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    // `as_chunks::<3>()` rather than `chunks_exact(3)`: same split, but the chunk size is in
    // the type, so each `chunk[i]` below is a compile-time-checked array index instead of a
    // bounds-checked slice one (clippy::chunks_exact_to_as_chunks).
    let (chunks, rem) = input.as_chunks::<3>();
    for chunk in chunks {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHABET[(n & 0x3F) as usize] as char);
    }
    match rem.len() {
        0 => {}
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        _ => unreachable!("as_chunks::<3>() remainder is 0..=2"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base64_encode_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encode_handles_binary() {
        assert_eq!(base64_encode(b"\xc3\xa9\xc3\xa1"), "w6nDoQ==");
    }

    #[test]
    fn basic_auth_header_format() {
        // Atlassian's documented example: `fred@example.com:api_token`.
        assert_eq!(
            basic_auth_header("fred@example.com", "api_token"),
            "Basic ZnJlZEBleGFtcGxlLmNvbTphcGlfdG9rZW4="
        );
    }

    #[test]
    fn join_url_no_double_slash() {
        assert_eq!(
            join_url("https://x.atlassian.net", "/rest/api/3/myself"),
            "https://x.atlassian.net/rest/api/3/myself"
        );
        assert_eq!(
            join_url("https://x.atlassian.net/", "/rest/api/3/myself"),
            "https://x.atlassian.net/rest/api/3/myself"
        );
        assert_eq!(
            join_url("https://x.atlassian.net", "rest/api/3/myself"),
            "https://x.atlassian.net/rest/api/3/myself"
        );
    }

    #[test]
    fn classify_status_promotes_known_codes() {
        assert!(classify_status(401, "Login required", None).starts_with("unauthorized"));
        assert!(classify_status(403, "no perm", None).starts_with("forbidden"));
        assert!(classify_status(404, "no issue", None).starts_with("not_found"));
        assert_eq!(
            classify_status(429, "", Some("30")),
            "rate_limited (Retry-After: 30)"
        );
        assert_eq!(classify_status(429, "", None), "rate_limited");
        assert!(classify_status(500, "boom", None).starts_with("io_error"));
        assert!(classify_status(503, "ish", None).starts_with("io_error"));
    }

    #[test]
    fn parse_myself_returns_user_info() {
        let body = json!({
            "accountId": "5b1234567890",
            "displayName": "Marshall Ku",
            "emailAddress": "marshall@example.com",
            "active": true,
        });
        let user = parse_myself(&body).unwrap();
        assert_eq!(user.account_id, "5b1234567890");
        assert_eq!(user.display_name, "Marshall Ku");
        assert_eq!(user.email_address, "marshall@example.com");
    }

    #[test]
    fn parse_myself_rejects_missing_account_id() {
        let body = json!({ "displayName": "x" });
        assert!(parse_myself(&body).is_err());
    }

    #[test]
    fn parse_myself_tolerates_missing_optional_fields() {
        // displayName and emailAddress are optional upstream; a privacy-redacted response must
        // not crash.
        let body = json!({ "accountId": "x" });
        let user = parse_myself(&body).unwrap();
        assert_eq!(user.display_name, "");
        assert_eq!(user.email_address, "");
    }

    #[test]
    fn validate_issue_key_accepts_canonical_shapes() {
        for s in ["PROJ-1", "APP-12345", "I_OS-42", "X1Y2-7"] {
            validate_issue_key(s).unwrap_or_else(|e| panic!("{s:?} rejected: {e}"));
        }
    }

    #[test]
    fn validate_issue_key_rejects_bad_shapes() {
        for bad in [
            "",
            "proj-1",     // lowercase project
            "1PROJ-1",    // starts with digit
            "PROJ",       // no hyphen
            "PROJ-",      // no number
            "-1",         // no project
            "PROJ-1a",    // non-digit number
            "PROJ/1",     // wrong separator (would otherwise allow path injection)
            "PROJ-1/foo", // path injection attempt
            "PROJ-1?x=1", // query string injection
        ] {
            assert!(validate_issue_key(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn validate_project_key_matches_config_charset() {
        validate_project_key("PROJ").unwrap();
        validate_project_key("APP_X").unwrap();
        validate_project_key("X1").unwrap();
        assert!(validate_project_key("proj").is_err());
        assert!(validate_project_key("1PROJ").is_err());
        assert!(validate_project_key("PROJ-X").is_err());
    }

    #[test]
    fn urlencode_passes_unreserved_chars() {
        assert_eq!(urlencode("PROJ-1"), "PROJ-1");
        assert_eq!(urlencode("abc-XYZ_123.~"), "abc-XYZ_123.~");
    }

    #[test]
    fn urlencode_escapes_jql_specials() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a=b"), "a%3Db");
        assert_eq!(urlencode("("), "%28");
        assert_eq!(urlencode(","), "%2C");
        assert_eq!(urlencode("é"), "%C3%A9");
    }

    #[test]
    fn build_polling_jql_no_projects() {
        let jql = build_polling_jql(24, None);
        assert!(jql.contains("currentUser()"));
        assert!(jql.contains("updated > -24h"));
        assert!(!jql.contains("project in"));
        assert!(jql.contains("ORDER BY updated DESC"));
    }

    #[test]
    fn build_polling_jql_with_projects() {
        let projects = vec!["APP".to_string(), "PROJ".to_string()];
        let jql = build_polling_jql(48, Some(&projects));
        assert!(jql.contains("AND project in (APP, PROJ)"));
        assert!(jql.contains("updated > -48h"));
    }

    #[test]
    fn build_polling_jql_empty_projects_omits_clause() {
        let projects: Vec<String> = vec![];
        let jql = build_polling_jql(24, Some(&projects));
        assert!(!jql.contains("project in"));
    }

    #[test]
    fn text_to_adf_wraps_single_line() {
        let v = text_to_adf("hello world");
        assert_eq!(v["type"], "doc");
        assert_eq!(v["version"], 1);
        let content = v["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "paragraph");
        assert_eq!(content[0]["content"][0]["text"], "hello world");
    }

    #[test]
    fn text_to_adf_splits_on_newline() {
        let v = text_to_adf("line one\nline two\n\nline three");
        let content = v["content"].as_array().unwrap();
        assert_eq!(content.len(), 4);
        assert_eq!(content[0]["content"][0]["text"], "line one");
        assert_eq!(content[1]["content"][0]["text"], "line two");
        assert!(content[2].get("content").is_none()); // empty paragraph
        assert_eq!(content[3]["content"][0]["text"], "line three");
    }

    #[test]
    fn text_to_adf_empty_produces_empty_paragraph() {
        let v = text_to_adf("");
        let content = v["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "paragraph");
    }

    #[test]
    fn parse_search_response_with_next_page_token() {
        let body = json!({
            "issues": vec![json!({"key":"A"}); 50],
            "nextPageToken": "tok-2",
            "isLast": false,
        });
        let resp = parse_search_response(&body).unwrap();
        assert_eq!(resp.issues.len(), 50);
        assert_eq!(resp.next_page_token.as_deref(), Some("tok-2"));
    }

    #[test]
    fn parse_search_response_is_last_returns_no_token() {
        let body = json!({
            "issues": vec![json!({"key":"A"}); 20],
            "isLast": true,
            "nextPageToken": "ignored-when-isLast-true",
        });
        let resp = parse_search_response(&body).unwrap();
        assert_eq!(resp.next_page_token, None);
    }

    #[test]
    fn parse_search_response_no_token_no_is_last_treated_as_last() {
        let body = json!({
            "issues": Vec::<Value>::new(),
        });
        let resp = parse_search_response(&body).unwrap();
        assert_eq!(resp.next_page_token, None);
        assert!(resp.issues.is_empty());
    }

    #[test]
    fn parse_search_response_empty_issues_ok() {
        let body = json!({
            "issues": Vec::<Value>::new(),
            "isLast": true,
        });
        let resp = parse_search_response(&body).unwrap();
        assert!(resp.issues.is_empty());
    }
}
