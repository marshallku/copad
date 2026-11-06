//! Network and console records an agent can read, and the limits on them. Records go to a file
//! the agent greps rather than into every tool response, because a verbose per-action dump makes
//! browser tooling unaffordable in a context window.
//!
//! 1. Coverage is declared: patching `fetch`/XHR is not a packet log (subresources,
//!    `sendBeacon`, WebSocket frames and service-worker traffic are invisible). Every response
//!    carries [`NETLOG_COVERAGE`] so an empty list never reads as "no request was made".
//! 2. Redaction is hygiene, not a boundary: a page can `console.log` anything. The boundary is
//!    [`super::TabMode::Protected`], which stops the capture script being installed at all.
//!    Refusing reads while the agent-readable file keeps filling would defend nothing.
//! 3. Bodies are off by default; turning them on is deliberate.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// What the capture mechanism actually sees; returned verbatim in every `webview.net` response.
pub const NETLOG_COVERAGE: &str = "js+navigation";

/// Header names whose values are replaced before a record is written, matched case-insensitively
/// against the whole name.
pub const REDACT_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "proxy-authorization",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
    "x-csrf-token",
];

/// Placeholder for a redacted value; distinct from an empty string so "absent" differs from
/// "withheld".
pub const REDACTED: &str = "«redacted»";

/// Longest HTTP method string kept verbatim; the page picks the method, so it is bounded.
const MAX_METHOD: usize = 32;

/// Field names redacted inside a captured body or console line, case-insensitively.
pub const REDACT_FIELDS: &[&str] = &[
    "access_token",
    "api_key",
    "apikey",
    "auth",
    "authorization",
    "client_secret",
    "code",
    "id_token",
    "otp",
    "passwd",
    "password",
    "pwd",
    "refresh_token",
    "secret",
    "session",
    "sig",
    "signature",
    "token",
];

static JSON_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)(["']({})["']\s*:\s*)(["'])(?:\\.|[^"'\\])*(["'])"#,
        REDACT_FIELDS.join("|")
    ))
    .expect("static field pattern compiles")
});

static FORM_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("(?i)\\b({})=[^&\\s\"']*", REDACT_FIELDS.join("|")))
        .expect("static field pattern compiles")
});

static BEARERISH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{8,}|\beyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]*")
        .expect("static bearer pattern compiles")
});

/// Redact credential-shaped content inside free text (a captured body, a console line); returns
/// the markers describing what was hit. Redact THEN clamp: truncating first could cut a secret
/// below its pattern and leave a prefix (`design_grab` follows the same rule). Hygiene, not a
/// boundary: `console.log(pw)` has no field name or pattern to match.
pub fn redact_text(text: &mut String) -> Vec<String> {
    let mut hits = Vec::new();
    let after_json = JSON_FIELD.replace_all(text, |c: &regex::Captures| {
        format!("{}{}{}{}", &c[1], &c[3], REDACTED, &c[4])
    });
    if after_json != *text {
        hits.push("body:field".into());
        *text = after_json.into_owned();
    }
    let after_form =
        FORM_FIELD.replace_all(text, |c: &regex::Captures| format!("{}={REDACTED}", &c[1]));
    if after_form != *text {
        if !hits.iter().any(|h| h == "body:field") {
            hits.push("body:field".into());
        }
        *text = after_form.into_owned();
    }
    let after_bearer = BEARERISH.replace_all(text, REDACTED);
    if after_bearer != *text {
        hits.push("body:bearer".into());
        *text = after_bearer.into_owned();
    }
    hits
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LogCaps {
    pub capture_bodies: bool,
    pub body_truncate: usize,
    /// Hard ceiling on one SERIALIZED record, enforced by [`sanitize_net`] / [`sanitize_console`]
    /// against the JSON actually written. A page controls URLs, header values and console text,
    /// so budgeting a single field leaves the record unbounded.
    pub per_record: usize,
    pub ring_records: usize,
    /// Whichever ring ceiling (records or bytes) is hit first evicts oldest-first.
    pub ring_bytes: usize,
}

impl Default for LogCaps {
    fn default() -> Self {
        Self {
            capture_bodies: false,
            body_truncate: 8 * 1024,
            per_record: 32 * 1024,
            ring_records: 2000,
            ring_bytes: 8 * 1024 * 1024,
        }
    }
}

/// How a record reached us — part of keeping [`NETLOG_COVERAGE`] honest.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetSource {
    Script,
    /// A main-frame or subframe navigation, seen by the native navigation delegate.
    Navigation,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct NetRecord {
    pub ts: u64,
    pub tab_id: String,
    pub source: NetSource,
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redacted: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleLevel {
    Debug,
    Log,
    Info,
    Warn,
    Error,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ConsoleRecord {
    pub ts: u64,
    pub tab_id: String,
    pub level: ConsoleLevel,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redacted: Vec<String>,
}

/// JSONL sink for one pane; per-pane rather than per-tab so a closing tab does not orphan a file.
pub fn log_path(panel_id: &str) -> Option<std::path::PathBuf> {
    // Panel ids arrive from the RPC wire, so they get the tab-id charset rule: no untrusted
    // string is joined as a path unvalidated.
    if !super::tabs::is_valid_tab_id(panel_id) {
        return None;
    }
    Some(
        super::state_root()
            .join("browser")
            .join("logs")
            .join(format!("{panel_id}.jsonl")),
    )
}

pub fn redact_headers(headers: &mut [(String, String)]) -> Vec<String> {
    let mut hit = Vec::new();
    for (name, value) in headers.iter_mut() {
        if REDACT_HEADERS.iter().any(|d| name.eq_ignore_ascii_case(d)) {
            *value = REDACTED.to_string();
            hit.push(name.clone());
        }
    }
    hit
}

/// Truncate a body to the cap on a char boundary; returns `true` when it truncated.
pub fn truncate_body(body: &mut String, cap: usize) -> bool {
    if body.len() <= cap {
        return false;
    }
    let mut end = cap;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    body.truncate(end);
    true
}

/// Apply the caps and header redaction before a record is written. Returns `false` when the
/// record cannot be brought under [`LogCaps::per_record`] even after shedding everything
/// sheddable; the caller drops it.
#[must_use]
pub fn sanitize_net(record: &mut NetRecord, caps: &LogCaps) -> bool {
    let mut redacted = redact_headers(&mut record.request_headers);
    redacted.extend(redact_headers(&mut record.response_headers));

    if !caps.capture_bodies {
        if record.body.take().is_some() {
            redacted.push("body".into());
        }
    } else if let Some(body) = record.body.as_mut() {
        redacted.extend(redact_text(body));
        if truncate_body(body, caps.body_truncate) {
            redacted.push("body:truncated".into());
        }
    }

    record.redacted.extend(redacted);
    enforce_net_cap(record, caps.per_record)
}

/// Serialized size of a record, or `usize::MAX` if it cannot be serialized (callers then treat it
/// as over budget).
fn serialized_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value)
        .map(|b| b.len())
        .unwrap_or(usize::MAX)
}

/// Shed a net record's page-controlled fields until the JSON fits `cap`, cheapest first: body,
/// headers, then URL. The ts/tab/method/status skeleton is never shed: a record saying only "a
/// POST happened at T and was too big to log" is still useful.
fn enforce_net_cap(record: &mut NetRecord, cap: usize) -> bool {
    if serialized_len(record) <= cap {
        return true;
    }
    if record.body.take().is_some() {
        record.redacted.push("body:oversize".into());
        if serialized_len(record) <= cap {
            return true;
        }
    }
    if !record.request_headers.is_empty() || !record.response_headers.is_empty() {
        record.request_headers.clear();
        record.response_headers.clear();
        record.redacted.push("headers:oversize".into());
        if serialized_len(record) <= cap {
            return true;
        }
    }
    if record.method.len() > MAX_METHOD {
        truncate_body(&mut record.method, MAX_METHOD);
        if serialized_len(record) <= cap {
            return true;
        }
    }
    if !record.url.is_empty() {
        record.redacted.push("url:truncated".into());
        while serialized_len(record) > cap && !record.url.is_empty() {
            let target = record.url.len() / 2;
            if target == 0 {
                record.url.clear();
            } else {
                truncate_body(&mut record.url, target);
            }
        }
        if serialized_len(record) <= cap {
            return true;
        }
    }
    // `redacted` is itself variable-length (every shed above pushes onto it). Collapse it LAST,
    // to one honest marker: keeping "Authorization" while dropping "body:oversize" would
    // misreport what happened.
    if record.redacted.len() > 1 {
        record.redacted = vec!["shed:oversize".into()];
        if serialized_len(record) <= cap {
            return true;
        }
    }
    // The skeleton alone still exceeds the cap (an absurdly long `tab_id`, or a cap below the
    // skeleton size): refuse rather than write an over-budget line.
    false
}

/// Cap a console record's text. Console text is not name-redacted (no name to key off), which is
/// why protected tabs never install the capture script.
#[must_use]
pub fn sanitize_console(record: &mut ConsoleRecord, caps: &LogCaps) -> bool {
    let hits = redact_text(&mut record.text);
    record.redacted.extend(hits);
    // Budget against the SERIALIZED record: JSON escaping (a page can emit only quotes and
    // newlines) and the ts/tab/level metadata add bytes beyond the raw text.
    if serialized_len(record) <= caps.per_record {
        return true;
    }
    record.redacted.push("text:truncated".into());
    while serialized_len(record) > caps.per_record && !record.text.is_empty() {
        let target = record.text.len() / 2;
        if target == 0 {
            record.text.clear();
        } else {
            truncate_body(&mut record.text, target);
        }
    }
    if serialized_len(record) <= caps.per_record {
        return true;
    }
    // `source` (a stack frame's script URL) is page-controlled too; emptying `text` alone could
    // leave a 40 KiB `source` above the ceiling.
    if record.source.take().is_some() {
        record.redacted.push("source:oversize".into());
        if serialized_len(record) <= caps.per_record {
            return true;
        }
    }
    if record.redacted.len() > 1 {
        record.redacted = vec!["shed:oversize".into()];
        if serialized_len(record) <= caps.per_record {
            return true;
        }
    }
    false
}

// The JSONL sink: one file per pane holding both kinds, each line tagged with `kind`. The agent
// greps slices of it instead of receiving a verbose dump after every action.

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Net,
    Console,
}

/// Filters for a read; the default is the most recent `limit` records of both kinds.
#[derive(Debug, Clone, Default)]
pub struct ReadQuery {
    pub kind: Option<Kind>,
    pub since: Option<u64>,
    pub tab_id: Option<String>,
    pub level: Option<ConsoleLevel>,
    pub contains: Option<String>,
    pub limit: usize,
}

fn ensure_dir() -> Result<std::path::PathBuf, String> {
    let dir = super::state_root().join("browser").join("logs");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    Ok(dir)
}

/// Append one record. Returns false when it was DROPPED for exceeding the per-record cap even
/// after shedding; a caller treating that as success would under-report.
pub fn append_net(panel_id: &str, mut record: NetRecord, caps: &LogCaps) -> Result<bool, String> {
    if !sanitize_net(&mut record, caps) {
        return Ok(false);
    }
    append_line(
        panel_id,
        Kind::Net,
        &serde_json::to_value(&record).map_err(|e| e.to_string())?,
        caps,
    )?;
    Ok(true)
}

pub fn append_console(
    panel_id: &str,
    mut record: ConsoleRecord,
    caps: &LogCaps,
) -> Result<bool, String> {
    if !sanitize_console(&mut record, caps) {
        return Ok(false);
    }
    append_line(
        panel_id,
        Kind::Console,
        &serde_json::to_value(&record).map_err(|e| e.to_string())?,
        caps,
    )?;
    Ok(true)
}

fn append_line(
    panel_id: &str,
    kind: Kind,
    body: &serde_json::Value,
    caps: &LogCaps,
) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let path = log_path(panel_id).ok_or_else(|| format!("invalid panel id: {panel_id:?}"))?;
    ensure_dir()?;
    let mut line = serde_json::json!({ "kind": kind });
    if let (Some(obj), Some(fields)) = (line.as_object_mut(), body.as_object()) {
        for (k, v) in fields {
            obj.insert(k.clone(), v.clone());
        }
    }
    let mut text = serde_json::to_string(&line).map_err(|e| e.to_string())?;
    text.push('\n');

    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;

    // Trim lazily: rewriting on every append would be a rewrite storm, so the file may overshoot
    // by a margin and is then cut back. Both ceilings trigger it, not just bytes: with small
    // records a byte-only trigger lets the file run tens of thousands of lines past the record
    // cap.
    let len = f.metadata().map(|m| m.len()).unwrap_or(0) as usize;
    let over_bytes = len > caps.ring_bytes + caps.ring_bytes / 4;
    let over_records = appends_since_trim(&path) > caps.ring_records / 4;
    if over_bytes || over_records {
        trim(&path, caps)?;
        reset_append_counter(&path);
    }
    Ok(())
}

/// Appends since the last trim, per log path. Counted in memory so the check doesn't touch the
/// whole log on every append. It must stay ONE map: a second `static` inside the reset function
/// would be a different map and the counter would never reset.
static APPEND_COUNTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn appends_since_trim(path: &std::path::Path) -> usize {
    let mut counts = APPEND_COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let n = counts.entry(path.to_path_buf()).or_insert(0);
    *n += 1;
    *n
}

fn reset_append_counter(path: &std::path::Path) {
    let mut counts = APPEND_COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    counts.insert(path.to_path_buf(), 0);
}

fn trim(path: &std::path::Path, caps: &LogCaps) -> Result<(), String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in raw.lines().rev() {
        if kept.len() >= caps.ring_records || bytes + line.len() + 1 > caps.ring_bytes {
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("open {}: {e}", tmp.display()))?;
        f.write_all(out.as_bytes())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rewrite {}: {e}", path.display())
    })
}

/// Read matching records, newest last (the order they happened in).
pub fn read(panel_id: &str, query: &ReadQuery) -> Result<Vec<serde_json::Value>, String> {
    let path = log_path(panel_id).ok_or_else(|| format!("invalid panel id: {panel_id:?}"))?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let limit = if query.limit == 0 { 200 } else { query.limit };
    let mut out: Vec<serde_json::Value> = Vec::new();
    for line in raw.lines().rev() {
        if out.len() >= limit {
            break;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if !matches_query(&value, query) {
            continue;
        }
        out.push(value);
    }
    out.reverse();
    Ok(out)
}

fn matches_query(value: &serde_json::Value, query: &ReadQuery) -> bool {
    let kind = value.get("kind").and_then(|v| v.as_str());
    if let Some(want) = query.kind {
        let want_str = match want {
            Kind::Net => "net",
            Kind::Console => "console",
        };
        if kind != Some(want_str) {
            return false;
        }
    }
    if let Some(since) = query.since
        && value.get("ts").and_then(|v| v.as_u64()).unwrap_or(0) <= since
    {
        return false;
    }
    if let Some(tab) = &query.tab_id
        && value.get("tab_id").and_then(|v| v.as_str()) != Some(tab.as_str())
    {
        return false;
    }
    if let Some(level) = query.level {
        let Some(actual) = value
            .get("level")
            .and_then(|v| serde_json::from_value::<ConsoleLevel>(v.clone()).ok())
        else {
            return false;
        };
        if actual < level {
            return false;
        }
    }
    if let Some(needle) = &query.contains {
        let hay = value
            .get("url")
            .and_then(|v| v.as_str())
            .or_else(|| value.get("text").and_then(|v| v.as_str()))
            .unwrap_or("");
        if !hay.to_lowercase().contains(&needle.to_lowercase()) {
            return false;
        }
    }
    true
}

pub fn clear(panel_id: &str) -> Result<usize, String> {
    let path = log_path(panel_id).ok_or_else(|| format!("invalid panel id: {panel_id:?}"))?;
    let count = std::fs::read_to_string(&path)
        .map(|r| r.lines().count())
        .unwrap_or(0);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(count),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net() -> NetRecord {
        NetRecord {
            ts: 1,
            tab_id: "t1".into(),
            source: NetSource::Script,
            method: "POST".into(),
            url: "https://api.example.com/login".into(),
            status: Some(200),
            duration_ms: Some(42),
            request_headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Authorization".into(), "Bearer sk-live-abc".into()),
            ],
            response_headers: vec![("Set-Cookie".into(), "session=abc; HttpOnly".into())],
            body: Some("{\"password\":\"hunter2\"}".into()),
            redacted: vec![],
        }
    }

    #[test]
    fn credential_headers_are_withheld_and_named() {
        let mut r = net();
        assert!(sanitize_net(&mut r, &LogCaps::default()));
        assert_eq!(r.request_headers[1].1, REDACTED);
        assert_eq!(r.response_headers[0].1, REDACTED);
        assert!(r.redacted.contains(&"Authorization".to_string()));
        assert!(r.redacted.contains(&"Set-Cookie".to_string()));
        assert_eq!(r.request_headers[0].1, "application/json");
    }

    #[test]
    fn header_matching_ignores_case() {
        let mut h = vec![
            ("AUTHORIZATION".into(), "x".into()),
            ("cookie".into(), "y".into()),
        ];
        let hit = redact_headers(&mut h);
        assert_eq!(hit.len(), 2);
        assert!(h.iter().all(|(_, v)| v == REDACTED));
    }

    #[test]
    fn bodies_are_dropped_by_default_and_the_drop_is_recorded() {
        let mut r = net();
        assert!(sanitize_net(&mut r, &LogCaps::default()));
        assert!(r.body.is_none());
        assert!(r.redacted.contains(&"body".to_string()));
    }

    #[test]
    fn an_opted_in_body_is_truncated_not_dropped() {
        let caps = LogCaps {
            capture_bodies: true,
            body_truncate: 8,
            ..Default::default()
        };
        let mut r = net();
        assert!(sanitize_net(&mut r, &caps));
        assert_eq!(r.body.as_deref(), Some("{\"passwo"));
        assert!(r.redacted.contains(&"body:truncated".to_string()));
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        let mut s = "한글한글".to_string();
        truncate_body(&mut s, 4);
        assert_eq!(s, "한");
        assert!(s.is_char_boundary(s.len()));
    }

    #[test]
    fn a_short_body_is_left_alone() {
        let mut s = "hi".to_string();
        assert!(!truncate_body(&mut s, 8));
        assert_eq!(s, "hi");
    }

    #[test]
    fn an_oversize_net_record_sheds_page_controlled_fields_until_it_fits() {
        let caps = LogCaps {
            capture_bodies: true,
            per_record: 300,
            ..Default::default()
        };
        let mut r = net();
        r.url = format!("https://api.example.com/{}", "x".repeat(4000));
        r.request_headers.push(("X-Trace".into(), "y".repeat(4000)));
        assert!(sanitize_net(&mut r, &caps));
        assert!(
            serde_json::to_vec(&r).unwrap().len() <= caps.per_record,
            "record still {} bytes",
            serde_json::to_vec(&r).unwrap().len()
        );
        assert_eq!(r.method, "POST");
        assert!(
            r.redacted
                .iter()
                .any(|x| x.ends_with(":oversize") || x == "url:truncated")
        );
    }

    #[test]
    fn a_normal_record_is_not_shed_by_the_cap() {
        let mut r = net();
        assert!(sanitize_net(&mut r, &LogCaps::default()));
        assert_eq!(r.url, "https://api.example.com/login");
        assert_eq!(r.request_headers.len(), 2);
        assert!(!r.redacted.iter().any(|x| x.ends_with(":oversize")));
    }

    #[test]
    fn console_escaping_cannot_push_a_record_over_the_cap() {
        let caps = LogCaps {
            per_record: 120,
            ..Default::default()
        };
        let mut c = ConsoleRecord {
            ts: 1,
            tab_id: "t1".into(),
            level: ConsoleLevel::Log,
            text: "\"".repeat(400),
            source: None,
            redacted: vec![],
        };
        assert!(sanitize_console(&mut c, &caps));
        assert!(serde_json::to_vec(&c).unwrap().len() <= caps.per_record);
        assert!(c.redacted.contains(&"text:truncated".to_string()));
    }

    #[test]
    fn console_text_is_capped_and_flagged() {
        // The cap is against the SERIALIZED record, so the text budget is what the ts/tab/level
        // skeleton leaves.
        let caps = LogCaps {
            per_record: 100,
            ..Default::default()
        };
        let mut c = ConsoleRecord {
            ts: 1,
            tab_id: "t1".into(),
            level: ConsoleLevel::Log,
            text: "a".repeat(500),
            source: None,
            redacted: vec![],
        };
        assert!(sanitize_console(&mut c, &caps));
        assert!(serde_json::to_vec(&c).unwrap().len() <= caps.per_record);
        assert!(
            !c.text.is_empty(),
            "a 100-byte budget should still fit some text"
        );
        assert!(c.redacted.contains(&"text:truncated".to_string()));
    }

    #[test]
    fn a_console_record_within_budget_is_untouched() {
        let mut c = ConsoleRecord {
            ts: 1,
            tab_id: "t1".into(),
            level: ConsoleLevel::Warn,
            text: "hello".into(),
            source: None,
            redacted: vec![],
        };
        assert!(sanitize_console(&mut c, &LogCaps::default()));
        assert_eq!(c.text, "hello");
        assert!(c.redacted.is_empty());
    }

    #[test]
    fn log_path_refuses_a_panel_id_that_could_escape_the_log_dir() {
        super::super::with_state_root(std::env::temp_dir().join("copad-netlog-path"), || {
            assert!(log_path("../../etc/passwd").is_none());
            assert!(log_path("panel1").is_some());
        });
    }

    #[test]
    fn an_oversize_console_source_is_shed_after_the_text() {
        let caps = LogCaps {
            per_record: 200,
            ..Default::default()
        };
        let mut c = ConsoleRecord {
            ts: 1,
            tab_id: "t1".into(),
            level: ConsoleLevel::Error,
            text: "boom".into(),
            source: Some("x".repeat(40_000)),
            redacted: vec![],
        };
        assert!(sanitize_console(&mut c, &caps));
        assert!(serde_json::to_vec(&c).unwrap().len() <= caps.per_record);
        assert!(c.source.is_none());
    }

    #[test]
    fn a_page_controlled_method_and_redaction_list_are_bounded_too() {
        let caps = LogCaps {
            per_record: 260,
            ..Default::default()
        };
        let mut r = net();
        r.method = "M".repeat(5000);
        r.url = "https://e.com/".into();
        for i in 0..200 {
            r.request_headers
                .push((format!("Authorization{i}"), "z".into()));
        }
        assert!(sanitize_net(&mut r, &caps));
        assert!(serde_json::to_vec(&r).unwrap().len() <= caps.per_record);
        assert!(r.method.len() <= MAX_METHOD);
    }

    #[test]
    fn a_record_that_cannot_be_shed_under_the_cap_is_rejected_not_written() {
        // The skeleton alone exceeds an absurd cap: refuse rather than write.
        let caps = LogCaps {
            per_record: 1,
            ..Default::default()
        };
        let mut r = net();
        assert!(!sanitize_net(&mut r, &caps));

        let mut c = ConsoleRecord {
            ts: 1,
            tab_id: "t1".into(),
            level: ConsoleLevel::Log,
            text: "x".into(),
            source: None,
            redacted: vec![],
        };
        assert!(!sanitize_console(&mut c, &caps));
    }

    #[test]
    fn an_opted_in_body_has_credential_shaped_content_redacted() {
        let caps = LogCaps {
            capture_bodies: true,
            ..Default::default()
        };
        let mut r = net();
        assert!(sanitize_net(&mut r, &caps));
        let body = r.body.as_deref().unwrap();
        assert!(!body.contains("hunter2"), "{body}");
        assert!(body.contains(REDACTED), "{body}");
        assert!(r.redacted.contains(&"body:field".to_string()));
    }

    #[test]
    fn form_encoded_and_bearer_shaped_content_is_redacted_too() {
        let mut s = "user=me&password=hunter2 Authorization: Bearer sk-live-abcdefghij".to_string();
        let hits = redact_text(&mut s);
        assert!(!s.contains("hunter2"), "{s}");
        assert!(!s.contains("sk-live-abcdefghij"), "{s}");
        assert!(hits.contains(&"body:field".to_string()));
        assert!(hits.contains(&"body:bearer".to_string()));
        assert!(s.contains("password="), "{s}");
        assert!(s.contains("user=me"), "{s}");
    }

    #[test]
    fn a_bare_jwt_with_no_field_name_is_still_caught() {
        let mut s = "token is eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcdef".to_string();
        let hits = redact_text(&mut s);
        assert!(!s.contains("eyJhbGci"), "{s}");
        assert!(hits.contains(&"body:bearer".to_string()));
    }

    #[test]
    fn ordinary_text_is_left_completely_alone() {
        let mut s = "GET /users/42 returned 3 rows in 12ms".to_string();
        let before = s.clone();
        assert!(redact_text(&mut s).is_empty());
        assert_eq!(s, before);
    }

    #[test]
    fn console_lines_get_the_same_hygiene_as_bodies() {
        let mut c = ConsoleRecord {
            ts: 1,
            tab_id: "t1".into(),
            level: ConsoleLevel::Log,
            text: "{\"password\":\"hunter2\"}".into(),
            source: None,
            redacted: vec![],
        };
        assert!(sanitize_console(&mut c, &LogCaps::default()));
        assert!(!c.text.contains("hunter2"), "{}", c.text);
    }

    #[test]
    fn coverage_is_a_declared_constant_so_the_readout_cannot_overclaim() {
        assert_eq!(NETLOG_COVERAGE, "js+navigation");
    }

    #[test]
    fn defaults_keep_bodies_off() {
        assert!(!LogCaps::default().capture_bodies);
    }

    fn with_temp_state<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _guard = super::super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("copad-netlog-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let prev_root = super::super::replace_state_root(Some(root.join("state")));
        let out = f();
        super::super::replace_state_root(prev_root);
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    fn console_at(ts: u64, level: ConsoleLevel, text: &str) -> ConsoleRecord {
        ConsoleRecord {
            ts,
            tab_id: "t1".into(),
            level,
            text: text.into(),
            source: None,
            redacted: vec![],
        }
    }

    #[test]
    fn records_of_both_kinds_land_in_one_file_and_read_back_in_order() {
        with_temp_state("roundtrip", || {
            let caps = LogCaps::default();
            assert!(append_net("panel1", net(), &caps).unwrap());
            assert!(
                append_console("panel1", console_at(2, ConsoleLevel::Log, "hello"), &caps).unwrap()
            );

            let all = read(
                "panel1",
                &ReadQuery {
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(all.len(), 2);
            assert_eq!(all[0]["kind"], "net");
            assert_eq!(all[1]["kind"], "console");
            assert!(all[0]["ts"].as_u64() < all[1]["ts"].as_u64());
        });
    }

    #[test]
    fn reading_a_pane_that_never_logged_is_empty_not_an_error() {
        with_temp_state("empty", || {
            assert!(read("panel1", &ReadQuery::default()).unwrap().is_empty());
        });
    }

    #[test]
    fn filters_narrow_by_kind_time_tab_level_and_substring() {
        with_temp_state("filters", || {
            let caps = LogCaps::default();
            append_net("panel1", net(), &caps).unwrap();
            append_console("panel1", console_at(5, ConsoleLevel::Debug, "noise"), &caps).unwrap();
            append_console("panel1", console_at(6, ConsoleLevel::Error, "boom"), &caps).unwrap();

            let only_console = read(
                "panel1",
                &ReadQuery {
                    kind: Some(Kind::Console),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(only_console.len(), 2);

            // `level` is a floor, not equality: asking for warnings must not hide errors.
            let serious = read(
                "panel1",
                &ReadQuery {
                    kind: Some(Kind::Console),
                    level: Some(ConsoleLevel::Warn),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(serious.len(), 1);
            assert_eq!(serious[0]["text"], "boom");

            let recent = read(
                "panel1",
                &ReadQuery {
                    since: Some(5),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(recent.len(), 1);

            let matching = read(
                "panel1",
                &ReadQuery {
                    contains: Some("LOGIN".into()),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(matching.len(), 1);
            assert_eq!(matching[0]["kind"], "net");

            let other_tab = read(
                "panel1",
                &ReadQuery {
                    tab_id: Some("nope".into()),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(other_tab.is_empty());
        });
    }

    #[test]
    fn a_limit_keeps_the_newest_records_not_the_oldest() {
        with_temp_state("limit", || {
            let caps = LogCaps::default();
            for i in 1..=10 {
                append_console(
                    "panel1",
                    console_at(i, ConsoleLevel::Log, &format!("line{i}")),
                    &caps,
                )
                .unwrap();
            }
            let out = read(
                "panel1",
                &ReadQuery {
                    limit: 3,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(out.len(), 3);
            assert_eq!(out[0]["text"], "line8");
            assert_eq!(out[2]["text"], "line10");
        });
    }

    #[test]
    fn the_record_cap_is_enforced_without_waiting_for_the_byte_cap() {
        // Triggering on size alone let a log of small records run tens of thousands of lines past
        // the record cap.
        with_temp_state("ring-records", || {
            let caps = LogCaps {
                ring_records: 20,
                ring_bytes: 8 * 1024 * 1024,
                ..Default::default()
            };
            for i in 1..=400 {
                append_console("panel1", console_at(i, ConsoleLevel::Log, "x"), &caps).unwrap();
            }
            let lines = std::fs::read_to_string(log_path("panel1").unwrap())
                .unwrap()
                .lines()
                .count();
            assert!(
                lines <= caps.ring_records + caps.ring_records / 4 + 1,
                "log kept {lines} records against a {} cap, nowhere near the byte cap",
                caps.ring_records
            );
        });
    }

    #[test]
    fn the_ring_trims_oldest_first_and_keeps_the_file_bounded() {
        with_temp_state("ring", || {
            let caps = LogCaps {
                ring_bytes: 2000,
                ring_records: 20,
                ..Default::default()
            };
            for i in 1..=200 {
                append_console(
                    "panel1",
                    console_at(i, ConsoleLevel::Log, &format!("line{i}")),
                    &caps,
                )
                .unwrap();
            }
            let path = log_path("panel1").unwrap();
            let size = std::fs::metadata(&path).unwrap().len() as usize;
            assert!(
                size <= caps.ring_bytes + caps.ring_bytes / 4,
                "log grew to {size} bytes against a {} cap",
                caps.ring_bytes
            );
            let out = read(
                "panel1",
                &ReadQuery {
                    limit: 1000,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(out.last().unwrap()["text"], "line200");
            assert!(out.len() < 200);
        });
    }

    #[test]
    fn a_record_that_cannot_be_shed_under_the_cap_is_reported_as_dropped() {
        with_temp_state("dropped", || {
            let caps = LogCaps {
                per_record: 1,
                ..Default::default()
            };
            assert!(
                !append_console("panel1", console_at(1, ConsoleLevel::Log, "x"), &caps).unwrap()
            );
            assert!(read("panel1", &ReadQuery::default()).unwrap().is_empty());
        });
    }

    #[test]
    fn clear_removes_the_log_and_reports_the_count() {
        with_temp_state("clear", || {
            let caps = LogCaps::default();
            for i in 1..=3 {
                append_console("panel1", console_at(i, ConsoleLevel::Log, "x"), &caps).unwrap();
            }
            assert_eq!(clear("panel1").unwrap(), 3);
            assert_eq!(clear("panel1").unwrap(), 0);
            assert!(read("panel1", &ReadQuery::default()).unwrap().is_empty());
        });
    }

    #[test]
    fn an_invalid_panel_id_cannot_write_or_read_outside_the_log_dir() {
        with_temp_state("traversal", || {
            let caps = LogCaps::default();
            assert!(
                append_console(
                    "../../etc/passwd",
                    console_at(1, ConsoleLevel::Log, "x"),
                    &caps
                )
                .is_err()
            );
            assert!(read("../../etc/passwd", &ReadQuery::default()).is_err());
            assert!(clear("../../etc/passwd").is_err());
        });
    }

    #[test]
    fn a_corrupt_line_is_skipped_rather_than_failing_the_whole_read() {
        with_temp_state("corrupt", || {
            let caps = LogCaps::default();
            append_console("panel1", console_at(1, ConsoleLevel::Log, "good"), &caps).unwrap();
            let path = log_path("panel1").unwrap();
            let mut raw = std::fs::read_to_string(&path).unwrap();
            raw.push_str("{not json\n");
            std::fs::write(&path, raw).unwrap();
            let out = read(
                "panel1",
                &ReadQuery {
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(out.len(), 1);
        });
    }
}
