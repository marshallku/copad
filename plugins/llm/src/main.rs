//! First-party LLM service plugin for copad (Anthropic provider).
//!
//! Two run modes:
//! - **`auth`** — reads `ANTHROPIC_API_KEY` from env, validates it
//!   with a 1-token `messages` call, persists `{api_key, validated_at}`
//!   to the configured store. Exits 0 on success.
//! - **(no args)** — RPC mode. Speaks the supervisor protocol over
//!   stdio. Provides `llm.complete`, `llm.usage`, `llm.auth_status`.
//!
//! Activation is `onAction:llm.*` (lazy) — there's no inbound stream
//! to keep alive (no Socket Mode equivalent), so the plugin only
//! spawns when a trigger or `coctl call` invokes one of the
//! actions. Cold-start cost is dominated by the first HTTP call
//! anyway.

#[cfg(not(unix))]
compile_error!(
    "copad-plugin-llm is currently Unix-only. The keyring crate's mock fallback \
     would silently lose tokens on platforms without a native credential-store \
     feature; gate exists to make that failure compile-time instead of runtime."
);

mod anthropic;
mod config;
mod store;
mod usage;

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use chrono::Utc;
use serde_json::{Value, json};

use config::Config;
use store::{TokenSet, TokenStore};

const PROTOCOL_VERSION: u32 = 1;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("auth") => run_auth(),
        Some(other) => {
            eprintln!("[llm] unknown subcommand: {other}");
            eprintln!("usage: copad-plugin-llm [auth]");
            std::process::exit(2);
        }
        None => run_rpc(),
    }
}

fn run_auth() {
    let config = Config::from_env();
    if let Some(err) = &config.fatal_error {
        eprintln!("[llm] config error: {err}");
        std::process::exit(1);
    }
    if config.api_key.is_empty() {
        eprintln!("[llm] auth requires ANTHROPIC_API_KEY (sk-ant-...)");
        std::process::exit(1);
    }
    let store = store::open_store(&config);
    eprintln!("[llm] token store: {}", store.kind());

    eprintln!(
        "[llm] validating API key with a 1-token messages call to {}...",
        config.default_model
    );
    if let Err(e) = anthropic::validate_key(&config.api_key, &config.default_model) {
        eprintln!(
            "[llm] validation failed: {e}\n\
             [llm] confirm ANTHROPIC_API_KEY is correct and COPAD_LLM_DEFAULT_MODEL \
             is a valid model id"
        );
        std::process::exit(1);
    }
    let tokens = TokenSet {
        api_key: config.api_key.clone(),
        validated_at: Some(Utc::now().to_rfc3339()),
    };
    if let Err(e) = store.save(&tokens) {
        eprintln!("[llm] failed to save tokens: {e}");
        std::process::exit(1);
    }
    eprintln!("[llm] auth ok — stored ({})", store.kind());
}

fn run_rpc() {
    let config = Config::from_env();
    if let Some(err) = &config.fatal_error {
        eprintln!("[llm] config errors — some actions will refuse: {err}");
    }
    let store: Arc<dyn TokenStore> = Arc::from(store::open_store(&config));
    eprintln!(
        "[llm] token store: {} (env key: {})",
        store.kind(),
        if config.api_key.is_empty() {
            "empty — falls back to store"
        } else {
            "present — overrides store"
        }
    );

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

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
                eprintln!("[llm] parse error: {e}");
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
                    &format!("llm plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": ["llm.complete", "llm.usage", "llm.auth_status"],
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
            // llm plugin doesn't subscribe — quietly ignore.
        }
        "shutdown" => std::process::exit(0),
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                tx,
                id,
                "unknown_method",
                &format!("llm plugin: unknown method {other}"),
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
    match name {
        "llm.auth_status" => Ok(handle_auth_status(config, store)),
        // `llm.usage` reads the local JSONL log — no Anthropic
        // call, no credentials required. Stays available even
        // after a key typo so users can still inspect prior
        // usage. BUT we refuse when account_resolved is false:
        // a bad COPAD_LLM_ACCOUNT silently falls back to the
        // "default" account's path, and reading some other
        // account's log under the assumption it's the one the
        // user asked for is a wrong-data bug.
        "llm.usage" => {
            if !config.account_resolved {
                return Err((
                    "invalid_params".to_string(),
                    format!(
                        "COPAD_LLM_ACCOUNT is invalid; refusing to read llm-usage-default.jsonl as a stand-in. Fix: {}",
                        config.fatal_error.clone().unwrap_or_default()
                    ),
                ));
            }
            handle_usage(params, config)
        }
        "llm.complete" => {
            // Network-touching: refuse upfront on fatal_error so the
            // status surface and runtime stay consistent and we don't
            // race a default-store credential fall-through.
            if let Some(err) = &config.fatal_error {
                return Err((
                    "not_authenticated".to_string(),
                    format!("llm plugin is in fatal-config state: {err}"),
                ));
            }
            handle_complete(params, config, store)
        }
        other => Err((
            "action_not_found".to_string(),
            format!("llm plugin does not handle {other}"),
        )),
    }
}

fn handle_auth_status(config: &Config, store: &Arc<dyn TokenStore>) -> Value {
    // Always emit the full documented field set so callers can rely
    // on a stable shape across both fatal and healthy paths.
    let stored = store.load();
    if let Some(err) = &config.fatal_error {
        return json!({
            "configured": false,
            "authenticated": false,
            "credentials_source": "none",
            "fatal_error": err,
            "store_kind": store.kind(),
            "account": config.account_label.clone(),
            "default_model": config.default_model.clone(),
            "validated_at": Value::Null,
        });
    }
    // Derive `credentials_source` from the EXACT same resolver
    // `llm.complete` uses so the status surface can never report
    // "authenticated=true" while the runtime would refuse the call.
    // Specifically, a stored TokenSet with an empty `api_key` (e.g.
    // partially-cleared file, corrupted JSON) would otherwise show
    // `credentials_source="store"` while `resolve_api_key` returns
    // None.
    let credentials_source: &str = if !config.api_key.is_empty() {
        "env"
    } else if resolve_api_key(config, &**store).is_some() {
        "store"
    } else {
        "none"
    };
    let authenticated = credentials_source != "none";
    // validated_at only meaningful when we're using stored
    // credentials — env keys haven't been validated by this plugin
    // instance.
    let validated_at = if credentials_source == "store" {
        stored.as_ref().and_then(|t| t.validated_at.clone())
    } else {
        None
    };
    json!({
        "configured": true,
        "authenticated": authenticated,
        "credentials_source": credentials_source,
        "fatal_error": Value::Null,
        "store_kind": store.kind(),
        "account": config.account_label.clone(),
        "default_model": config.default_model.clone(),
        "validated_at": validated_at,
    })
}

/// Env wins, store fallback (slack/calendar pattern; single token so
/// no cross-source mixing pitfall).
fn resolve_api_key(config: &Config, store: &dyn TokenStore) -> Option<String> {
    if !config.api_key.is_empty() {
        return Some(config.api_key.clone());
    }
    store.load().map(|t| t.api_key).filter(|s| !s.is_empty())
}

fn handle_complete(
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
) -> Result<Value, (String, String)> {
    let api_key = resolve_api_key(config, &**store).ok_or((
        "not_authenticated".to_string(),
        "no Anthropic API key — set ANTHROPIC_API_KEY or run `copad-plugin-llm auth`".to_string(),
    ))?;
    let prompt = params.get("prompt").and_then(Value::as_str).ok_or((
        "invalid_params".to_string(),
        "missing 'prompt' (string)".to_string(),
    ))?;
    // Strict type validation — same fail-closed contract as
    // temperature/source/since/by_model. Without this, wrong-typed
    // `system` or `model` ({"model":123}) would silently drop and
    // the plugin would still issue a paid API call with the
    // default model / no system prompt — burning tokens on an
    // accidentally-misconfigured trigger.
    let system = match params.get("system") {
        Some(v) if v.is_null() => None,
        Some(v) => Some(v.as_str().ok_or((
            "invalid_params".to_string(),
            "system must be a string".to_string(),
        ))?),
        None => None,
    };
    let model = match params.get("model") {
        Some(v) if v.is_null() => config.default_model.as_str(),
        Some(v) => v.as_str().ok_or((
            "invalid_params".to_string(),
            "model must be a string".to_string(),
        ))?,
        None => config.default_model.as_str(),
    };
    // Reject oversized max_tokens explicitly rather than silently
    // wrapping `as u32` — a request for `4_294_967_297` would
    // otherwise truncate to 1 and quietly produce a single-token
    // reply, which is worse than failing closed.
    let max_tokens: u32 = match params.get("max_tokens") {
        Some(v) => {
            let n = v.as_u64().ok_or((
                "invalid_params".to_string(),
                "max_tokens must be a positive integer".to_string(),
            ))?;
            u32::try_from(n).map_err(|_| {
                (
                    "invalid_params".to_string(),
                    format!("max_tokens out of range: {n} > {}", u32::MAX),
                )
            })?
        }
        None => config.default_max_tokens,
    };
    if max_tokens == 0 {
        return Err((
            "invalid_params".to_string(),
            "max_tokens must be > 0".to_string(),
        ));
    }
    // Strict type validation — present-but-wrong-type is rejected
    // rather than silently treated as absent. Without this,
    // `{"temperature": "hot"}` would silently drop the param and
    // run with Anthropic's default, contradicting the
    // "invalid_params before the network call" contract.
    let temperature = match params.get("temperature") {
        Some(v) if v.is_null() => None,
        Some(v) => {
            let t = v.as_f64().ok_or((
                "invalid_params".to_string(),
                "temperature must be a number".to_string(),
            ))?;
            if !(0.0..=2.0).contains(&t) {
                return Err((
                    "invalid_params".to_string(),
                    format!("temperature must be in [0.0, 2.0], got {t}"),
                ));
            }
            Some(t)
        }
        None => None,
    };
    let source = match params.get("source") {
        Some(v) if v.is_null() => None,
        Some(v) => Some(
            v.as_str()
                .ok_or((
                    "invalid_params".to_string(),
                    "source must be a string".to_string(),
                ))?
                .to_string(),
        ),
        None => None,
    };

    let req = anthropic::CompleteRequest {
        model,
        max_tokens,
        messages: vec![anthropic::Message {
            role: "user",
            content: prompt,
        }],
        system,
        temperature,
    };
    let resp = anthropic::complete(&api_key, &req, config.http_timeout).map_err(|e| {
        // 401-prefixed errors come back as `auth_error: ...`.
        // Other errors stay under io_error so the protocol
        // shape matches calendar/slack.
        if e.starts_with("auth_error:") {
            ("not_authenticated".to_string(), e)
        } else {
            ("io_error".to_string(), e)
        }
    })?;

    // Best-effort usage logging — failure to write the log MUST NOT
    // fail the action since the user has already paid for the
    // tokens. Stderr surfaces the issue for operators.
    let record = usage::UsageRecord {
        ts: Utc::now().to_rfc3339(),
        model: resp.model.clone(),
        input_tokens: resp.input_tokens,
        output_tokens: resp.output_tokens,
        source,
    };
    if let Err(e) = usage::append(&config.usage_log_path, &record) {
        eprintln!("[llm] usage log append failed (action still succeeds): {e}");
    }

    Ok(json!({
        "text": resp.text,
        "model": resp.model,
        "stop_reason": resp.stop_reason,
        "usage": {
            "input_tokens": resp.input_tokens,
            "output_tokens": resp.output_tokens,
        },
    }))
}

fn handle_usage(params: &Value, config: &Config) -> Result<Value, (String, String)> {
    // Strict types: present-but-wrong-type rejected. Treating
    // `{"since": 0}` as "no filter" would silently broaden the
    // query to all-time and quietly return misleading aggregates.
    let since = parse_optional_rfc3339(params, "since")?;
    let until = parse_optional_rfc3339(params, "until")?;
    let model_filter = match params.get("by_model") {
        Some(v) if v.is_null() => None,
        Some(v) => Some(v.as_str().ok_or((
            "invalid_params".to_string(),
            "by_model must be a string".to_string(),
        ))?),
        None => None,
    };
    let (agg, parse_errors) = usage::aggregate(&config.usage_log_path, since, until, model_filter)
        .map_err(|e| ("io_error".to_string(), e))?;
    let mut out = usage::aggregate_to_json(&agg, parse_errors);
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "since".to_string(),
            since
                .map(|t| Value::String(t.to_rfc3339()))
                .unwrap_or(Value::Null),
        );
        obj.insert(
            "until".to_string(),
            until
                .map(|t| Value::String(t.to_rfc3339()))
                .unwrap_or(Value::Null),
        );
    }
    Ok(out)
}

fn parse_optional_rfc3339(
    params: &Value,
    key: &str,
) -> Result<Option<chrono::DateTime<Utc>>, (String, String)> {
    match params.get(key) {
        Some(v) if v.is_null() => Ok(None),
        Some(v) => {
            let s = v.as_str().ok_or((
                "invalid_params".to_string(),
                format!("{key} must be an RFC3339 string"),
            ))?;
            let parsed = chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|e| ("invalid_params".to_string(), format!("{key}: {e}")))?;
            Ok(Some(parsed.with_timezone(&Utc)))
        }
        None => Ok(None),
    }
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
    use crate::store::TokenSet;

    struct StubStore(Option<TokenSet>);
    impl TokenStore for StubStore {
        fn load(&self) -> Option<TokenSet> {
            self.0.clone()
        }
        fn save(&self, _: &TokenSet) -> Result<(), String> {
            Ok(())
        }
        fn clear(&self) -> Result<(), String> {
            Ok(())
        }
        fn kind(&self) -> &'static str {
            "stub"
        }
    }

    fn cfg_base() -> Config {
        // Build a Config fixture independent of process env so
        // tests don't race each other through env mutation.
        Config {
            api_key: String::new(),
            default_model: "claude-sonnet-4-6".into(),
            default_max_tokens: 4096,
            http_timeout: std::time::Duration::from_secs(120),
            account_label: "default".into(),
            account_resolved: true,
            require_secure_store: false,
            plaintext_path: std::path::PathBuf::from("/tmp/.unused-llm-tok"),
            usage_log_path: std::path::PathBuf::from("/tmp/.unused-llm-usage"),
            fatal_error: None,
        }
    }

    fn cfg_with_env_key(key: &str) -> Config {
        let mut c = cfg_base();
        c.api_key = key.to_string();
        c
    }

    fn cfg_minimal_no_error() -> Config {
        cfg_base()
    }

    #[test]
    fn resolve_key_prefers_env_over_store() {
        let store = StubStore(Some(TokenSet {
            api_key: "sk-ant-store".into(),
            validated_at: None,
        }));
        let key = resolve_api_key(&cfg_with_env_key("sk-ant-env"), &store).unwrap();
        assert_eq!(key, "sk-ant-env");
    }

    #[test]
    fn resolve_key_falls_back_to_store() {
        let store = StubStore(Some(TokenSet {
            api_key: "sk-ant-store".into(),
            validated_at: None,
        }));
        let key = resolve_api_key(&cfg_minimal_no_error(), &store).unwrap();
        assert_eq!(key, "sk-ant-store");
    }

    #[test]
    fn resolve_key_returns_none_with_no_env_no_store() {
        let store = StubStore(None);
        assert!(resolve_api_key(&cfg_minimal_no_error(), &store).is_none());
    }

    #[test]
    fn auth_status_short_circuits_on_fatal_error() {
        let mut c = cfg_base();
        c.fatal_error = Some("bad env".into());
        let store: Arc<dyn TokenStore> = Arc::new(StubStore(None));
        let v = handle_auth_status(&c, &store);
        assert_eq!(v["configured"], false);
        assert_eq!(v["authenticated"], false);
        assert_eq!(v["credentials_source"], "none");
        assert_eq!(v["fatal_error"], "bad env");
        // Documented shape: default_model and validated_at present
        // even on the fatal-error branch so callers can rely on a
        // stable contract.
        assert!(v.get("default_model").is_some());
        assert_eq!(v["validated_at"], Value::Null);
    }

    #[test]
    fn usage_refuses_when_account_unresolved() {
        let mut c = cfg_base();
        c.account_resolved = false;
        c.fatal_error = Some("COPAD_LLM_ACCOUNT: invalid character ...".into());
        let err = handle_action(
            "llm.usage",
            &json!({}),
            &c,
            &(Arc::new(StubStore(None)) as Arc<dyn TokenStore>),
        )
        .unwrap_err();
        assert_eq!(err.0, "invalid_params");
        assert!(
            err.1.contains("COPAD_LLM_ACCOUNT is invalid"),
            "got {}",
            err.1
        );
    }

    #[test]
    fn auth_status_reports_env_source() {
        let store: Arc<dyn TokenStore> = Arc::new(StubStore(Some(TokenSet {
            api_key: "sk-ant-store".into(),
            validated_at: Some("2026-04-27T00:00:00Z".into()),
        })));
        let v = handle_auth_status(&cfg_with_env_key("sk-ant-env"), &store);
        assert_eq!(v["credentials_source"], "env");
        assert_eq!(v["authenticated"], true);
        // validated_at hidden when env source — we don't know if
        // the env key matches the previously-validated stored one.
        assert_eq!(v["validated_at"], Value::Null);
    }

    #[test]
    fn auth_status_reports_store_source_with_validated_at() {
        let store: Arc<dyn TokenStore> = Arc::new(StubStore(Some(TokenSet {
            api_key: "sk-ant-store".into(),
            validated_at: Some("2026-04-27T00:00:00Z".into()),
        })));
        let v = handle_auth_status(&cfg_minimal_no_error(), &store);
        assert_eq!(v["credentials_source"], "store");
        assert_eq!(v["validated_at"], "2026-04-27T00:00:00Z");
    }

    #[test]
    fn complete_rejects_missing_prompt() {
        let store: Arc<dyn TokenStore> = Arc::new(StubStore(Some(TokenSet {
            api_key: "sk-ant-x".into(),
            validated_at: None,
        })));
        let err = handle_complete(&json!({}), &cfg_minimal_no_error(), &store).unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn complete_rejects_zero_max_tokens() {
        let store: Arc<dyn TokenStore> = Arc::new(StubStore(Some(TokenSet {
            api_key: "sk-ant-x".into(),
            validated_at: None,
        })));
        let err = handle_complete(
            &json!({"prompt": "hi", "max_tokens": 0}),
            &cfg_minimal_no_error(),
            &store,
        )
        .unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn complete_rejects_out_of_range_temperature() {
        let store: Arc<dyn TokenStore> = Arc::new(StubStore(Some(TokenSet {
            api_key: "sk-ant-x".into(),
            validated_at: None,
        })));
        let err = handle_complete(
            &json!({"prompt": "hi", "temperature": 3.5}),
            &cfg_minimal_no_error(),
            &store,
        )
        .unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }

    #[test]
    fn complete_returns_not_authenticated_when_no_key() {
        let store: Arc<dyn TokenStore> = Arc::new(StubStore(None));
        let err =
            handle_complete(&json!({"prompt": "hi"}), &cfg_minimal_no_error(), &store).unwrap_err();
        assert_eq!(err.0, "not_authenticated");
    }

    #[test]
    fn usage_invalid_since_returns_invalid_params() {
        let cfg = cfg_minimal_no_error();
        let err = handle_usage(&json!({"since": "not-a-date"}), &cfg).unwrap_err();
        assert_eq!(err.0, "invalid_params");
    }
}
