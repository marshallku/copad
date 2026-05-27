//! First-party Jira (Atlassian Cloud) service plugin for copad.
//!
//! Two run modes (selected by `argv[1]`):
//! - **`auth`** — validates the env-supplied API token via
//!   `/rest/api/3/myself` and persists `{email, api_token, base_url,
//!   account_id, display_name}` to the configured store. Exits 0 on
//!   success.
//! - **(no args)** — RPC mode. Speaks the copad service-plugin
//!   protocol over stdio. Slice 16.1 only handles `jira.auth_status`;
//!   slice 16.2 will add the polling loop + 5 read/write actions.
//!
//! If RPC mode starts with no stored credentials AND the env tokens
//! are missing, the supervisor handshake still completes — actions
//! return `not_authenticated` until the user runs the `auth`
//! subcommand or fixes the env.
//!
//! See `docs/service-plugins.md` for the protocol contract. Atlassian
//! Cloud-only (no Server / Data Center). API token + Basic auth — no
//! OAuth round-trip needed for personal use, matching the env+keyring
//! posture every other copad plugin uses today.

#[cfg(not(unix))]
compile_error!(
    "copad-plugin-jira is currently Unix-only. The keyring crate's mock fallback \
     would silently lose tokens on platforms without a native credential-store \
     feature; gate exists to make that failure compile-time instead of runtime."
);

mod config;
mod event;
mod jira;
mod poller;
mod store;

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use serde_json::{Value, json};

use config::Config;
use store::{TokenSet, TokenStore};

const PROTOCOL_VERSION: u32 = 1;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("auth") => run_auth(),
        Some(other) => {
            eprintln!("[jira] unknown subcommand: {other}");
            eprintln!("usage: copad-plugin-jira [auth]");
            std::process::exit(2);
        }
        None => run_rpc(),
    }
}

fn run_auth() {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[jira] config error: {e}");
            std::process::exit(1);
        }
    };
    // `auth` is the one path where the env credentials MUST be
    // present — RPC mode is allowed to fall through to the store,
    // but `auth` is what populates the store in the first place.
    if config.base_url.is_empty() {
        eprintln!(
            "[jira] auth requires COPAD_JIRA_BASE_URL (e.g. https://yourcompany.atlassian.net)"
        );
        std::process::exit(1);
    }
    if config.email.is_empty() {
        eprintln!("[jira] auth requires COPAD_JIRA_EMAIL");
        std::process::exit(1);
    }
    if config.api_token.is_empty() {
        eprintln!(
            "[jira] auth requires COPAD_JIRA_API_TOKEN (generate at id.atlassian.com/manage-profile/security/api-tokens)"
        );
        std::process::exit(1);
    }
    let store = store::open_store(&config);
    eprintln!("[jira] token store: {}", store.kind());

    eprintln!(
        "[jira] validating credentials via {}/rest/api/3/myself...",
        config.base_url
    );
    let user = match jira::validate_credentials(&config.base_url, &config.email, &config.api_token)
    {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[jira] /myself failed: {e}");
            eprintln!(
                "[jira] verify COPAD_JIRA_BASE_URL, COPAD_JIRA_EMAIL, COPAD_JIRA_API_TOKEN \
                 — generate a token at id.atlassian.com/manage-profile/security/api-tokens"
            );
            std::process::exit(1);
        }
    };
    // Cross-check the env email against what Atlassian reports. A
    // common paste-mismatch is one team member's token + another's
    // email; that combination passes validate_credentials silently
    // (Basic auth only checks the secret, not the email field — the
    // email field is just a username) but later API calls under the
    // user-confused identity get confusing.
    if !user.email_address.is_empty() && !user.email_address.eq_ignore_ascii_case(&config.email) {
        // Fields differ. We still allow the auth to proceed — some
        // workspaces hide email via privacy controls, returning an
        // empty string which we already short-circuited. But warn
        // loudly so the user knows what they got.
        eprintln!(
            "[jira] WARNING: COPAD_JIRA_EMAIL ({:?}) does not match the account's \
             emailAddress ({:?}). Auth succeeded with the API token, but you may have \
             pasted credentials from a different Atlassian account than you intended.",
            config.email, user.email_address
        );
    }
    let tokens = TokenSet {
        email: config.email.clone(),
        api_token: config.api_token.clone(),
        base_url: config.base_url.clone(),
        account_id: user.account_id.clone(),
        display_name: user.display_name.clone(),
    };
    if let Err(e) = store.save(&tokens) {
        eprintln!("[jira] failed to save tokens: {e}");
        std::process::exit(1);
    }
    eprintln!(
        "[jira] auth ok — account={} display={:?} stored ({})",
        user.account_id,
        user.display_name,
        store.kind()
    );
}

fn run_rpc() {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[jira] FATAL config error — Jira actions disabled until fixed: {e}");
            Config::minimal_with_error(e)
        }
    };
    let store: Arc<dyn TokenStore> = Arc::from(store::open_store(&config));
    eprintln!(
        "[jira] token store: {} (env credentials: {})",
        store.kind(),
        if config.env_creds_empty() {
            "empty — will fall back to store"
        } else {
            "present — will override store"
        }
    );

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    // Single writer thread funnels all outgoing JSON so init reply,
    // action replies, and (slice 16.2) event.publish notifications
    // never interleave.
    let (tx, rx) = channel::<String>();
    let writer_tx = tx.clone();
    thread::spawn(move || {
        let mut out = stdout.lock();
        for line in rx.iter() {
            if writeln!(out, "{line}").is_err() || out.flush().is_err() {
                break;
            }
        }
    });

    let initialized = Arc::new(AtomicBool::new(false));

    // Polling daemon runs in a background thread. It waits for the
    // `initialized` notification before starting ticks and resolves
    // credentials on every tick (so running `copad-plugin-jira auth`
    // while copad is already up populates the store and the next
    // tick picks it up — no plugin restart needed).
    {
        let cfg_for_poller = Arc::new(config.clone());
        let store_for_poller = store.clone();
        let event_tx = tx.clone();
        let init_flag = initialized.clone();
        thread::spawn(move || {
            let p = poller::Poller::new(cfg_for_poller, store_for_poller, event_tx, init_flag);
            p.run();
        });
    }

    let reader = BufReader::new(stdin.lock());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        let frame: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[jira] parse error: {e}");
                continue;
            }
        };
        handle_frame(&frame, &writer_tx, &initialized, &config, &store);
    }
}

fn handle_frame(
    frame: &Value,
    tx: &Sender<String>,
    initialized: &AtomicBool,
    config: &Config,
    store: &Arc<dyn TokenStore>,
) {
    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    let id = frame.get("id").and_then(Value::as_str).unwrap_or("");
    let params = frame.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let proto = params.get("protocol_version").and_then(Value::as_u64);
            if proto != Some(PROTOCOL_VERSION as u64) {
                send_error(
                    tx,
                    id,
                    "protocol_mismatch",
                    &format!("jira plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": [
                        "jira.auth_status",
                        "jira.list_my_tickets",
                        "jira.get_ticket",
                        "jira.create_ticket",
                        "jira.transition",
                        "jira.add_comment",
                    ],
                    "subscribes": [],
                }),
            );
        }
        "initialized" => {
            initialized.store(true, Ordering::SeqCst);
        }
        "action.invoke" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let action_params = params.get("params").cloned().unwrap_or(Value::Null);
            let result = handle_action(&name, &action_params, config, store);
            match result {
                Ok(v) => send_response(tx, id, v),
                Err((code, msg)) => send_error(tx, id, &code, &msg),
            }
        }
        "event.dispatch" => {
            // jira plugin doesn't subscribe — quietly ignore.
        }
        "shutdown" => std::process::exit(0),
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                tx,
                id,
                "unknown_method",
                &format!("jira plugin: unknown method {other}"),
            );
        }
        _ => {}
    }
}

fn handle_action(
    name: &str,
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
) -> Result<Value, (String, String)> {
    if name == "jira.auth_status" {
        return Ok(auth_status_payload(config, store));
    }
    // Recognize the action name BEFORE checking credentials so an
    // unauthenticated call to a typo'd action surfaces as
    // `action_not_found` (a stable diagnostic) rather than the
    // state-dependent `not_authenticated` (which would change to
    // action_not_found later once the user runs `auth`).
    let known = matches!(
        name,
        "jira.list_my_tickets"
            | "jira.get_ticket"
            | "jira.create_ticket"
            | "jira.transition"
            | "jira.add_comment"
    );
    if !known {
        return Err((
            "action_not_found".to_string(),
            format!("jira plugin does not handle {name}"),
        ));
    }
    // All Jira-touching actions need credentials; short-circuit on
    // fatal_error so a stale stored token can't make a single call
    // succeed before breaking confusingly on the next refresh.
    if config.fatal_error.is_some() {
        return Err((
            "not_authenticated".to_string(),
            "jira plugin is in fatal-config state — see jira.auth_status".to_string(),
        ));
    }
    let resolved = current_credentials(config, &**store).ok_or((
        "not_authenticated".to_string(),
        "no Jira credentials available — run `copad-plugin-jira auth` or set env credentials"
            .to_string(),
    ))?;
    let creds = jira::Creds {
        base_url: &resolved.base_url,
        email: &resolved.email,
        api_token: &resolved.api_token,
    };
    match name {
        "jira.list_my_tickets" => handle_list_my_tickets(creds, params),
        "jira.get_ticket" => handle_get_ticket(creds, params),
        "jira.create_ticket" => handle_create_ticket(creds, params),
        "jira.transition" => handle_transition(creds, params),
        "jira.add_comment" => handle_add_comment(creds, params),
        // Unreachable — the `known` guard above already filtered.
        other => Err((
            "action_not_found".to_string(),
            format!("jira plugin does not handle {other}"),
        )),
    }
}

/// Promotes `jira::http_*`'s prefix-encoded error string to the
/// action's `(code, message)` tuple. EXPLICIT allowlist (vs slack's
/// "any bare snake_case prefix") so internal strings like `"json parse:
/// ..."`, `"project key: ..."`, `"transition response missing id"`
/// can't escape as ad-hoc codes (`json`/`project`/`transition`) and
/// break the trigger-matchable contract. Unknown collapses to `io_error`.
fn map_jira_error(err: String) -> (String, String) {
    /// Public error-code surface — triggers and `coctl` clients
    /// pattern-match on these. Adding a new code is a contract change.
    const KNOWN_CODES: &[&str] = &[
        "unauthorized",
        "forbidden",
        "not_found",
        "rate_limited",
        "transition_not_available",
        "invalid_params",
        "io_error",
    ];
    // Split on whitespace, `(`, AND `:` so prefixes like
    // `"unauthorized HTTP 401:"` and `"rate_limited (Retry-After:..."`
    // both land as the bare keyword.
    let bare = err
        .split(|c: char| c.is_whitespace() || c == '(' || c == ':')
        .next()
        .unwrap_or("");
    if KNOWN_CODES.contains(&bare) {
        (bare.to_string(), err)
    } else {
        ("io_error".to_string(), err)
    }
}

fn handle_list_my_tickets(creds: jira::Creds, params: &Value) -> Result<Value, (String, String)> {
    let status = optional_string(params, "status")?;
    let project = optional_string(params, "project")?;
    let updated_since = optional_string(params, "updated_since")?;

    if let Some(p) = &project {
        jira::validate_project_key(p).map_err(|e| ("invalid_params".to_string(), e))?;
    }
    let mut clauses = vec!["assignee = currentUser()".to_string()];
    if let Some(s) = &status {
        // Status names can have spaces; quote them.
        clauses.push(format!("status = \"{}\"", jql_escape(s)));
    }
    if let Some(p) = &project {
        clauses.push(format!("project = {p}"));
    }
    if let Some(since) = &updated_since {
        // `since` should look like `-2d` / `-1w` / `2026-01-01` —
        // pass through as-is, JQL will validate.
        clauses.push(format!("updated > \"{}\"", jql_escape(since)));
    }
    let jql = format!("{} ORDER BY updated DESC", clauses.join(" AND "));

    // Paginate up to MAX_PAGES so a chatty workspace doesn't truncate
    // silently. Action surface returns the union of pages with an
    // explicit `truncated` flag so callers (triggers, coctl, future
    // panel UIs) can detect partial results and surface a user-visible
    // warning rather than silently acting on an arbitrary prefix of
    // the result set. The new `/search/jql` endpoint dropped `total`
    // from responses (cursor-based pagination doesn't need it), so
    // we no longer report a total count — only `truncated` matters.
    const MAX_PAGES: u64 = 10;
    let fields = [
        "summary", "status", "assignee", "reporter", "project", "updated",
    ];
    let mut tickets = Vec::new();
    let mut next_page_token: Option<String> = None;
    let mut truncated = false;
    for page in 0..MAX_PAGES {
        let resp = jira::search(creds, &jql, next_page_token.as_deref(), 100, &fields)
            .map_err(map_jira_error)?;
        for issue in &resp.issues {
            if let Some(t) = event::from_jira_json(issue, creds.base_url) {
                tickets.push(serde_json::Value::Object(event::to_payload_json(&t)));
            }
        }
        match resp.next_page_token {
            Some(tok) => next_page_token = Some(tok),
            None => break,
        }
        if page + 1 == MAX_PAGES {
            truncated = true;
        }
    }
    Ok(json!({
        "tickets": tickets,
        "truncated": truncated,
    }))
}

fn handle_get_ticket(creds: jira::Creds, params: &Value) -> Result<Value, (String, String)> {
    let key = required_string(params, "key")?;
    jira::validate_issue_key(&key).map_err(|e| ("invalid_params".to_string(), e))?;
    // Returns the verbatim Jira response (per the docs contract:
    // "returns full ticket json"). Triggers / coctl callers who
    // want the trigger envelope shape can use list_my_tickets which
    // returns the envelope form. This split lets `get_ticket`
    // surface fields like custom field values, `changelog`, full
    // `comment` arrays, etc. that the envelope flattens away.
    jira::get_issue(creds, &key).map_err(map_jira_error)
}

fn handle_create_ticket(creds: jira::Creds, params: &Value) -> Result<Value, (String, String)> {
    let project = required_string(params, "project")?;
    let summary = required_string(params, "summary")?;
    let description = optional_string(params, "description")?;
    let assignee = optional_string(params, "assignee")?;
    let parent = optional_string(params, "parent")?;
    let issue_type = optional_string(params, "issue_type")?;
    // Validate at handler level so a bad project key surfaces as
    // `invalid_params` (the public action contract) rather than the
    // io_error fallback that map_jira_error would assign to the
    // bubble-up from create_issue's internal validation.
    jira::validate_project_key(&project).map_err(|e| ("invalid_params".to_string(), e))?;
    if let Some(p) = &parent {
        jira::validate_issue_key(p).map_err(|e| ("invalid_params".to_string(), e))?;
    }
    let resp = jira::create_issue(
        creds,
        &project,
        &summary,
        description.as_deref(),
        assignee.as_deref(),
        parent.as_deref(),
        issue_type.as_deref(),
    )
    .map_err(map_jira_error)?;
    let key = resp
        .get("key")
        .and_then(Value::as_str)
        .ok_or((
            "io_error".to_string(),
            "create response missing key".to_string(),
        ))?
        .to_string();
    let url = format!("{}/browse/{}", creds.base_url.trim_end_matches('/'), key);
    Ok(json!({ "key": key, "url": url }))
}

fn handle_transition(creds: jira::Creds, params: &Value) -> Result<Value, (String, String)> {
    let key = required_string(params, "key")?;
    let status = required_string(params, "status")?;
    jira::validate_issue_key(&key).map_err(|e| ("invalid_params".to_string(), e))?;
    let (from, to) = jira::transition(creds, &key, &status).map_err(map_jira_error)?;
    Ok(json!({ "key": key, "from_status": from, "to_status": to }))
}

fn handle_add_comment(creds: jira::Creds, params: &Value) -> Result<Value, (String, String)> {
    let key = required_string(params, "key")?;
    let body = required_string(params, "body")?;
    jira::validate_issue_key(&key).map_err(|e| ("invalid_params".to_string(), e))?;
    let comment_id = jira::add_comment(creds, &key, &body).map_err(map_jira_error)?;
    Ok(json!({ "comment_id": comment_id }))
}

fn required_string(params: &Value, name: &str) -> Result<String, (String, String)> {
    params
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or((
            "invalid_params".to_string(),
            format!("missing required string param {name:?}"),
        ))
        .and_then(|s| {
            if s.trim().is_empty() {
                Err((
                    "invalid_params".to_string(),
                    format!("required param {name:?} must not be empty"),
                ))
            } else {
                Ok(s)
            }
        })
}

fn optional_string(params: &Value, name: &str) -> Result<Option<String>, (String, String)> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            if s.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(s.clone()))
            }
        }
        Some(_) => Err((
            "invalid_params".to_string(),
            format!("param {name:?} must be a string or null"),
        )),
    }
}

/// JQL string-literal escape: `\` → `\\`, `"` → `\"`. Other chars pass
/// through (JQL is permissive about newlines / control chars in literals).
fn jql_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// `account_id_hint` is captured atomically with the other credential
/// fields (only set when `source == "store"`). The poller's
/// my_account_id resolution uses it so a hot re-auth between
/// `current_credentials` and a follow-up `store.load()` can't produce
/// a credentials/account_id mismatch.
pub struct ResolvedCreds {
    pub source: &'static str,
    pub base_url: String,
    pub email: String,
    pub api_token: String,
    pub account_id_hint: Option<String>,
}

pub fn current_credentials(config: &Config, store: &dyn TokenStore) -> Option<ResolvedCreds> {
    if !config.env_creds_empty() {
        return Some(ResolvedCreds {
            source: "env",
            base_url: config.base_url.clone(),
            email: config.email.clone(),
            api_token: config.api_token.clone(),
            // env-supplied creds haven't been validated by /myself
            // (the `auth` subcommand path is what records account_id);
            // poller will resolve via /myself on first tick.
            account_id_hint: None,
        });
    }
    let t = store.load()?;
    // Reject incomplete stored entries — a manually edited keyring
    // record (or a malformed plaintext file partially overwritten by
    // a stuck write) with empty fields would otherwise make
    // `auth_status` report `authenticated=true` and push the
    // runtime into confusing 401s instead of a clean
    // `not_authenticated`. Same posture as slack/discord's stored
    // credential resolvers.
    if t.email.is_empty() || t.api_token.is_empty() || t.base_url.is_empty() {
        eprintln!(
            "[jira] stored credentials are incomplete (email/api_token/base_url empty); \
             treating as not_authenticated. Re-run `copad-plugin-jira auth` to repair."
        );
        return None;
    }
    // Apply the SAME `*.atlassian.net` host restriction to stored
    // base_url that env validation enforces. Without this gate a
    // tampered keyring entry (or a hand-edited plaintext store with
    // a swapped host) would steer authenticated REST calls to an
    // arbitrary HTTPS endpoint — defeating the env-time defense
    // against API-token exfiltration. Treats validation failure as
    // "stored credentials are unsafe" → not_authenticated; user
    // re-runs `auth` to overwrite with a clean record.
    if let Err(e) = config::validate_base_url(&t.base_url) {
        eprintln!(
            "[jira] stored base_url failed Atlassian Cloud host check ({e}); \
             treating as not_authenticated to avoid sending the API token to an unintended host. \
             Re-run `copad-plugin-jira auth` to repair."
        );
        return None;
    }
    let account_id_hint = if t.account_id.is_empty() {
        None
    } else {
        Some(t.account_id.clone())
    };
    Some(ResolvedCreds {
        source: "store",
        base_url: t.base_url,
        email: t.email,
        api_token: t.api_token,
        account_id_hint,
    })
}

/// Shared `jira.auth_status` payload — re-used by the polling loop and
/// any future client wrapper to keep the shape verbatim.
fn auth_status_payload(config: &Config, store: &Arc<dyn TokenStore>) -> Value {
    // When env validation produced a fatal_error we MUST NOT report
    // authenticated=true based on a fall-through default-workspace
    // store load — the runtime would refuse to make any HTTP call
    // (because handle_action short-circuits on fatal_error in 16.2),
    // so the status surface would lie about the runtime state.
    if let Some(err) = &config.fatal_error {
        return json!({
            "configured": false,
            "authenticated": false,
            "credentials_source": "none",
            "fatal_error": err,
            "store_kind": store.kind(),
            "workspace": config.workspace_label.clone(),
            "base_url": config.base_url.clone(),
            "account_id": Value::Null,
            "display_name": Value::Null,
        });
    }
    let resolved = current_credentials(config, &**store);
    let stored = store.load();
    let credentials_source = resolved.as_ref().map(|c| c.source).unwrap_or("none");
    let authenticated = resolved.is_some();
    // Identity (account_id, display_name) is only verified for the
    // store source — that's the only path where we ran /myself at
    // auth time. For env-overridden credentials we don't have a
    // verified identity for THOSE specific tokens, so reporting the
    // stored identity would be misleading (the env tokens could be
    // from a different Atlassian account than the store). Surface
    // them only when consistent with the live source.
    let report_identity = credentials_source == "store";
    // The base_url field reports the live source's URL — matches
    // resolved.base_url when authenticated, falls back to env when
    // env is set but store is empty (handled by current_credentials),
    // empty when neither is set.
    let base_url = resolved
        .as_ref()
        .map(|c| c.base_url.clone())
        .unwrap_or_else(|| config.base_url.clone());
    json!({
        "configured": true,
        "authenticated": authenticated,
        "credentials_source": credentials_source,
        "fatal_error": Value::Null,
        "store_kind": store.kind(),
        "workspace": config.workspace_label.clone(),
        "base_url": base_url,
        "account_id": if report_identity {
            stored.as_ref().map(|t| t.account_id.clone()).map(Value::String).unwrap_or(Value::Null)
        } else { Value::Null },
        "display_name": if report_identity {
            stored.as_ref().map(|t| t.display_name.clone()).map(Value::String).unwrap_or(Value::Null)
        } else { Value::Null },
    })
}

fn send_response(tx: &Sender<String>, id: &str, result: Value) {
    let frame = json!({ "id": id, "ok": true, "result": result });
    let _ = tx.send(frame.to_string());
}

fn send_error(tx: &Sender<String>, id: &str, code: &str, message: &str) {
    let frame = json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message },
    });
    let _ = tx.send(frame.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use store::TokenStore;

    struct EmptyStore;
    impl TokenStore for EmptyStore {
        fn load(&self) -> Option<TokenSet> {
            None
        }
        fn save(&self, _: &TokenSet) -> Result<(), String> {
            Ok(())
        }
        fn clear(&self) -> Result<(), String> {
            Ok(())
        }
        fn kind(&self) -> &'static str {
            "test-empty"
        }
    }

    struct FixedStore(TokenSet);
    impl TokenStore for FixedStore {
        fn load(&self) -> Option<TokenSet> {
            Some(self.0.clone())
        }
        fn save(&self, _: &TokenSet) -> Result<(), String> {
            Ok(())
        }
        fn clear(&self) -> Result<(), String> {
            Ok(())
        }
        fn kind(&self) -> &'static str {
            "test-fixed"
        }
    }

    fn sample_tokens() -> TokenSet {
        TokenSet {
            email: "marshall@example.com".into(),
            api_token: "tok-stored".into(),
            base_url: "https://stored.atlassian.net".into(),
            account_id: "5b-stored".into(),
            display_name: "Stored Marshall".into(),
        }
    }

    fn good_config() -> Config {
        Config {
            base_url: "https://env.atlassian.net".into(),
            email: "env@example.com".into(),
            api_token: "tok-env".into(),
            workspace_label: "default".into(),
            require_secure_store: false,
            plaintext_path: std::path::PathBuf::from("/tmp/x"),
            poll_interval: std::time::Duration::from_secs(300),
            lookback_hours: 24,
            projects: None,
            fetch_comments: true,
            fatal_error: None,
        }
    }

    #[test]
    fn auth_status_reports_fatal_error_short_circuit() {
        let store: Arc<dyn TokenStore> = Arc::new(FixedStore(sample_tokens()));
        let cfg = Config::minimal_with_error("missing X".to_string());
        let v = auth_status_payload(&cfg, &store);
        assert_eq!(v["configured"], false);
        assert_eq!(v["authenticated"], false);
        assert_eq!(v["credentials_source"], "none");
        assert_eq!(v["fatal_error"], "missing X");
        // Identity must NOT leak even though store has tokens — the
        // runtime won't honor them, so the status must reflect that.
        assert_eq!(v["account_id"], Value::Null);
        assert_eq!(v["display_name"], Value::Null);
    }

    #[test]
    fn auth_status_env_source_hides_identity() {
        let store: Arc<dyn TokenStore> = Arc::new(FixedStore(sample_tokens()));
        let cfg = good_config();
        let v = auth_status_payload(&cfg, &store);
        assert_eq!(v["configured"], true);
        assert_eq!(v["authenticated"], true);
        assert_eq!(v["credentials_source"], "env");
        assert_eq!(v["base_url"], "https://env.atlassian.net");
        assert_eq!(v["account_id"], Value::Null);
        assert_eq!(v["display_name"], Value::Null);
    }

    #[test]
    fn auth_status_store_source_reports_identity() {
        let store: Arc<dyn TokenStore> = Arc::new(FixedStore(sample_tokens()));
        // env empty: store wins.
        let mut cfg = good_config();
        cfg.email = String::new();
        cfg.api_token = String::new();
        cfg.base_url = String::new();
        let v = auth_status_payload(&cfg, &store);
        assert_eq!(v["configured"], true);
        assert_eq!(v["authenticated"], true);
        assert_eq!(v["credentials_source"], "store");
        assert_eq!(v["base_url"], "https://stored.atlassian.net");
        assert_eq!(v["account_id"], "5b-stored");
        assert_eq!(v["display_name"], "Stored Marshall");
    }

    #[test]
    fn auth_status_no_creds_anywhere() {
        let store: Arc<dyn TokenStore> = Arc::new(EmptyStore);
        let mut cfg = good_config();
        cfg.email = String::new();
        cfg.api_token = String::new();
        cfg.base_url = String::new();
        let v = auth_status_payload(&cfg, &store);
        assert_eq!(v["configured"], true);
        assert_eq!(v["authenticated"], false);
        assert_eq!(v["credentials_source"], "none");
    }

    #[test]
    fn handle_action_unknown_returns_action_not_found() {
        let store: Arc<dyn TokenStore> = Arc::new(EmptyStore);
        let cfg = good_config();
        let err = handle_action("jira.no_such_action", &Value::Null, &cfg, &store).unwrap_err();
        assert_eq!(err.0, "action_not_found");
    }

    #[test]
    fn handle_action_routes_auth_status() {
        let store: Arc<dyn TokenStore> = Arc::new(EmptyStore);
        let cfg = good_config();
        let v = handle_action("jira.auth_status", &Value::Null, &cfg, &store).unwrap();
        assert_eq!(v["credentials_source"], "env");
    }

    #[test]
    fn current_credentials_prefers_env() {
        let store: Box<dyn TokenStore> = Box::new(FixedStore(sample_tokens()));
        let cfg = good_config();
        let r = current_credentials(&cfg, &*store).unwrap();
        assert_eq!(r.source, "env");
        assert_eq!(r.email, "env@example.com");
    }

    #[test]
    fn current_credentials_falls_back_to_store() {
        let store: Box<dyn TokenStore> = Box::new(FixedStore(sample_tokens()));
        let mut cfg = good_config();
        cfg.email = String::new();
        cfg.api_token = String::new();
        cfg.base_url = String::new();
        let r = current_credentials(&cfg, &*store).unwrap();
        assert_eq!(r.source, "store");
        assert_eq!(r.email, "marshall@example.com");
    }

    #[test]
    fn current_credentials_rejects_incomplete_store_entry() {
        // A keyring entry hand-edited to drop the api_token (or any
        // of the three required fields) must NOT surface as
        // authenticated — that would mask the corruption behind a
        // confusing 401 at runtime instead of a clean
        // not_authenticated.
        let mut tokens = sample_tokens();
        tokens.api_token = String::new();
        let store: Box<dyn TokenStore> = Box::new(FixedStore(tokens));
        let mut cfg = good_config();
        cfg.email = String::new();
        cfg.api_token = String::new();
        cfg.base_url = String::new();
        assert!(current_credentials(&cfg, &*store).is_none());
    }

    #[test]
    fn required_string_rejects_missing_or_empty() {
        let p = json!({});
        assert_eq!(required_string(&p, "x").unwrap_err().0, "invalid_params");
        let p = json!({ "x": "" });
        assert_eq!(required_string(&p, "x").unwrap_err().0, "invalid_params");
        let p = json!({ "x": "   " });
        assert_eq!(required_string(&p, "x").unwrap_err().0, "invalid_params");
        let p = json!({ "x": 42 });
        assert_eq!(required_string(&p, "x").unwrap_err().0, "invalid_params");
        let p = json!({ "x": "value" });
        assert_eq!(required_string(&p, "x").unwrap(), "value");
    }

    #[test]
    fn optional_string_treats_null_and_empty_as_none() {
        let p = json!({});
        assert!(optional_string(&p, "x").unwrap().is_none());
        let p = json!({ "x": null });
        assert!(optional_string(&p, "x").unwrap().is_none());
        let p = json!({ "x": "" });
        assert!(optional_string(&p, "x").unwrap().is_none());
        let p = json!({ "x": "  " });
        assert!(optional_string(&p, "x").unwrap().is_none());
        let p = json!({ "x": "value" });
        assert_eq!(optional_string(&p, "x").unwrap().as_deref(), Some("value"));
        let p = json!({ "x": 42 });
        assert_eq!(optional_string(&p, "x").unwrap_err().0, "invalid_params");
    }

    #[test]
    fn jql_escape_backslash_and_quote() {
        assert_eq!(jql_escape("plain"), "plain");
        assert_eq!(jql_escape("a\"b"), "a\\\"b");
        assert_eq!(jql_escape("a\\b"), "a\\\\b");
        assert_eq!(jql_escape("\"both\\\""), "\\\"both\\\\\\\"");
    }

    #[test]
    fn map_jira_error_promotes_known_codes_only() {
        // Known codes ride through verbatim.
        let (code, msg) = map_jira_error("unauthorized HTTP 401: bad creds".into());
        assert_eq!(code, "unauthorized");
        assert!(msg.contains("HTTP 401"));
        let (code, _) = map_jira_error("forbidden HTTP 403: x".into());
        assert_eq!(code, "forbidden");
        let (code, _) = map_jira_error("not_found HTTP 404: x".into());
        assert_eq!(code, "not_found");
        let (code, _) = map_jira_error("rate_limited (Retry-After: 30)".into());
        assert_eq!(code, "rate_limited");
        let (code, _) = map_jira_error("transition_not_available no transition".into());
        assert_eq!(code, "transition_not_available");
        let (code, _) = map_jira_error("io_error Jira HTTP 503: gateway timeout".into());
        assert_eq!(code, "io_error");
        // Unknown bare-snake prefixes (internal validation error
        // bubble-ups) MUST collapse to io_error rather than leaking
        // ad-hoc codes that triggers can't reliably match against.
        for unknown in [
            "json parse: bad shape",
            "project key: cannot be empty",
            "create response missing key",
            "transition response missing id",
            "transport: connection refused",
            "issue key PROJ-1: invalid",
        ] {
            let (code, msg) = map_jira_error(unknown.to_string());
            assert_eq!(
                code, "io_error",
                "input {unknown:?} should collapse to io_error"
            );
            assert!(msg.contains(unknown.split_once(' ').map(|(p, _)| p).unwrap_or(unknown)));
        }
    }

    #[test]
    fn handle_action_create_validates_required_params() {
        let store: Arc<dyn TokenStore> = Arc::new(EmptyStore);
        let cfg = good_config();
        // Missing project + summary.
        let err = handle_action("jira.create_ticket", &json!({}), &cfg, &store).unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn handle_action_get_ticket_validates_key_shape() {
        let store: Arc<dyn TokenStore> = Arc::new(EmptyStore);
        let cfg = good_config();
        let err = handle_action(
            "jira.get_ticket",
            &json!({"key": "PROJ/../foo"}),
            &cfg,
            &store,
        )
        .unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn handle_action_short_circuits_on_fatal_error() {
        let store: Arc<dyn TokenStore> = Arc::new(EmptyStore);
        let cfg = Config::minimal_with_error("missing X".into());
        let err = handle_action("jira.list_my_tickets", &json!({}), &cfg, &store).unwrap_err();
        assert_eq!(err.0, "not_authenticated");
    }

    #[test]
    fn current_credentials_rejects_stored_non_atlassian_base_url() {
        // Critical security boundary: env-time validation rejects
        // non-Atlassian hosts to prevent API-token exfiltration. A
        // hand-edited keyring entry must NOT bypass that check by
        // injecting an arbitrary HTTPS URL.
        for bad_host in [
            "https://attacker.example.com",
            "https://atlassian.net.evil.example",
            "https://x.atlassian.net/jira",
            "https://jira.company.com",
        ] {
            let mut tokens = sample_tokens();
            tokens.base_url = bad_host.to_string();
            let store: Box<dyn TokenStore> = Box::new(FixedStore(tokens));
            let mut cfg = good_config();
            cfg.email = String::new();
            cfg.api_token = String::new();
            cfg.base_url = String::new();
            assert!(
                current_credentials(&cfg, &*store).is_none(),
                "should reject stored base_url {bad_host:?}"
            );
        }
    }
}
