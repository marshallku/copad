//! First-party Discord service plugin for copad.
//!
//! Two run modes (selected by `argv[1]`):
//! - **`auth`** — validates the env bot token via `GET /users/@me`
//!   and persists a TokenSet (with bot user_id + global_name) to the
//!   configured store.
//! - **(no args)** — RPC mode. Speaks the copad service-plugin
//!   protocol over stdio, runs the Gateway WebSocket in a background
//!   thread, and publishes `discord.message` / `discord.dm` /
//!   `discord.mention` / `discord.raw` events when MESSAGE_CREATE
//!   dispatches arrive.
//!
//! If RPC mode starts without stored credentials AND the env token is
//! missing, the supervisor handshake still completes — the gateway
//! loop just stays paused. Running `copad-plugin-discord auth` while
//! copad is running is enough; the loop picks up the new credentials
//! on its next reconnect attempt.
//!
//! Caveat — env precedence: `COPAD_DISCORD_BOT_TOKEN`, when set, wins
//! over the stored token (matches Slack's posture, useful for
//! testing). That means the auth-while-running recovery path only
//! works if the env var is UNSET in the supervisor's environment.
//! Once env is set at copad startup, a fresh `auth` run updates the
//! store but the gateway keeps using the env token until copad
//! restarts. `discord.auth_status.credentials_source` reports the
//! live source so the user can see which one is live.
//!
//! See `docs/service-plugins.md` for the protocol contract. Discord
//! plugin is an event emitter + write-action provider — a `kb.append`
//! / `webhook.fire` action on a `discord.mention` event is purely
//! user trigger config, the same shape as the Slack integration.

mod api;
mod config;
mod events;
mod gateway;
mod store;

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use config::Config;
use store::{TokenSet, TokenStore, open_store};

const PROTOCOL_VERSION: u32 = 1;
const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

type StdoutHandle = Arc<std::sync::Mutex<std::io::Stdout>>;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("auth") => run_auth(),
        None => run_rpc(),
        Some(other) => {
            eprintln!("usage: copad-plugin-discord [auth]");
            eprintln!("unknown subcommand: {other:?}");
            std::process::exit(2);
        }
    }
}

fn run_auth() {
    let config = Config::from_env();
    if let Some(err) = &config.fatal_error {
        eprintln!("[discord] auth: config error: {err}");
        std::process::exit(2);
    }
    let token = match &config.bot_token_env {
        Some(t) => t.clone(),
        None => {
            eprintln!(
                "[discord] auth requires COPAD_DISCORD_BOT_TOKEN env. \
                 Get a token from <https://discord.com/developers/applications>"
            );
            std::process::exit(2);
        }
    };
    eprintln!("[discord] validating token via /users/@me...");
    let me = match get_users_me(&token) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[discord] /users/@me failed: {e}");
            std::process::exit(2);
        }
    };
    let user_id = me.get("id").and_then(Value::as_str).map(str::to_string);
    let username = me
        .get("global_name")
        .and_then(Value::as_str)
        .or_else(|| me.get("username").and_then(Value::as_str))
        .map(str::to_string);
    let store = open_store(&config);
    let set = TokenSet {
        bot_token: token,
        user_id: user_id.clone(),
        username: username.clone(),
    };
    if let Err(e) = store.save(&set) {
        eprintln!("[discord] save failed: {e}");
        std::process::exit(2);
    }
    eprintln!(
        "[discord] auth OK. user_id={} username={} stored in {}",
        user_id.as_deref().unwrap_or("?"),
        username.as_deref().unwrap_or("?"),
        store.kind()
    );
}

fn get_users_me(bot_token: &str) -> Result<Value, String> {
    let resp = ureq::get(&format!("{DISCORD_API_BASE}/users/@me"))
        .set("Authorization", &format!("Bot {bot_token}"))
        .set("User-Agent", "copad-plugin-discord (copad, 0.1)")
        .call()
        .map_err(|e| format!("http: {e}"))?;
    let status = resp.status();
    let body = resp.into_string().map_err(|e| format!("read body: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {body}"));
    }
    serde_json::from_str(&body).map_err(|e| format!("decode: {e}"))
}

fn run_rpc() {
    let config = Config::from_env();
    if let Some(err) = &config.fatal_error {
        eprintln!(
            "[discord] config error (gateway disabled; \
             discord.send_message returns not_authenticated; \
             discord.auth_status reports the fatal_error): {err}"
        );
    }
    // Box<dyn TokenStore> → Arc<dyn TokenStore> so the gateway thread
    // can hold an independent reference. The keyring-backed store has
    // its own internal locking; we don't need a Mutex around it.
    let store: Arc<dyn TokenStore> = Arc::from(open_store(&config));
    eprintln!(
        "[discord] workspace={}, store={}",
        config.workspace_label,
        store.kind()
    );

    let stdin = std::io::stdin();
    let stdout: StdoutHandle = Arc::new(std::sync::Mutex::new(std::io::stdout()));
    let initialized = Arc::new(AtomicBool::new(false));
    let stop_signal = Arc::new(AtomicBool::new(false));

    // Gateway loop runs in a background thread, gated on the
    // supervisor's `initialized` notification so events can't race
    // the handshake. The loop polls credentials inside `run_loop` —
    // running `copad-plugin-discord auth` while the plugin is up
    // populates the store and the loop picks it up on its next
    // recheck (no plugin process restart required).
    {
        let init_flag = initialized.clone();
        let stop = stop_signal.clone();
        let writer = stdout.clone();
        let cfg = config.clone();
        let store_for_loop = store.clone();
        thread::spawn(move || {
            while !init_flag.load(Ordering::SeqCst) {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
            gateway::run_loop(&cfg, store_for_loop, &stop, |event| {
                let frame = json!({
                    "method": "event.publish",
                    "params": {
                        "kind": event.kind(),
                        "payload": event.payload_json(),
                    }
                });
                emit(&writer, &frame.to_string());
            });
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
                eprintln!("[discord] parse error: {e}");
                continue;
            }
        };
        handle_frame(&frame, &stdout, &initialized, &stop_signal, &config, &store);
    }
}

fn handle_frame(
    frame: &Value,
    stdout: &StdoutHandle,
    initialized: &AtomicBool,
    stop_signal: &AtomicBool,
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
                    stdout,
                    id,
                    "protocol_mismatch",
                    &format!("discord plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                stdout,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": [
                        "discord.auth_status",
                        "discord.send_message",
                        "discord.get_message",
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
                Ok(v) => send_response(stdout, id, v),
                Err((code, msg)) => send_error(stdout, id, &code, &msg),
            }
        }
        "event.dispatch" => {}
        "shutdown" => {
            stop_signal.store(true, Ordering::SeqCst);
            std::process::exit(0);
        }
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                stdout,
                id,
                "unknown_method",
                &format!("discord plugin: unknown method {other}"),
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
        "discord.auth_status" => {
            // Same shape as Slack's slack.auth_status — reports BOTH
            // configured (env validation OK) and authenticated (creds
            // resolvable for the gateway loop) so a future
            // `coctl context --full` can render both messengers
            // uniformly. credentials_source mirrors what the loop
            // would actually use; reporting "store" while the loop
            // reads from "env" would be a confusing lie.
            let configured = config.fatal_error.is_none();
            let resolved = if configured {
                gateway::current_credentials(config, &**store)
            } else {
                None
            };
            let stored = if configured { store.load() } else { None };
            let credentials_source = resolved.as_ref().map(|c| c.source).unwrap_or("none");
            // Identity (user_id, username) is only validated for the
            // store path — env tokens skip the auth.test step. Match
            // Slack's posture: surface stored identity only when the
            // live source is the store, else null.
            let report_identity = credentials_source == "store";
            Ok(json!({
                "configured": configured,
                "authenticated": resolved.is_some(),
                "credentials_source": credentials_source,
                "store_kind": store.kind(),
                "workspace": config.workspace_label,
                "user_id": if report_identity {
                    stored.as_ref().and_then(|s| s.user_id.clone())
                } else { None },
                "username": if report_identity {
                    stored.as_ref().and_then(|s| s.username.clone())
                } else { None },
                "fatal_error": config.fatal_error.clone(),
            }))
        }
        "discord.send_message" => handle_send_message(params, config, store),
        "discord.get_message" => handle_get_message(params, config, store),
        _ if config.fatal_error.is_some() => {
            Err(("config_error".into(), config.fatal_error.clone().unwrap()))
        }
        other => Err((
            "action_not_found".into(),
            format!("discord plugin does not handle {other}"),
        )),
    }
}

fn handle_send_message(
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
) -> Result<Value, (String, String)> {
    if config.fatal_error.is_some() {
        return Err((
            "not_authenticated".into(),
            "discord plugin is in fatal-config state — see discord.auth_status".into(),
        ));
    }
    let creds = gateway::current_credentials(config, &**store).ok_or((
        "not_authenticated".to_string(),
        "no Discord credentials available — run `copad-plugin-discord auth` or set COPAD_DISCORD_BOT_TOKEN"
            .to_string(),
    ))?;
    let channel_id = require_snowflake(params, "channel_id")?;
    let content = params.get("content").and_then(Value::as_str).ok_or((
        "invalid_params".to_string(),
        "missing 'content' (string)".to_string(),
    ))?;
    if content.is_empty() {
        return Err((
            "invalid_params".to_string(),
            "'content' must be non-empty".to_string(),
        ));
    }
    if content.chars().count() > 2000 {
        return Err((
            "invalid_params".to_string(),
            format!(
                "'content' exceeds Discord's 2000-character limit ({} chars)",
                content.chars().count()
            ),
        ));
    }
    match api::post_message(&creds.bot_token, channel_id, content) {
        Ok((message_id, posted_channel)) => Ok(json!({
            "message_id": message_id,
            "channel_id": posted_channel,
        })),
        // ApiError carries the structured code already
        // (`rate_limited` / `discord_<numeric>` / `io_error`); pass
        // it through so triggers can match `error.code ==
        // "discord_50001"` rather than parsing the message string.
        Err(api::ApiError { code, message }) => Err((code, message)),
    }
}

fn handle_get_message(
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
) -> Result<Value, (String, String)> {
    if config.fatal_error.is_some() {
        return Err((
            "not_authenticated".into(),
            "discord plugin is in fatal-config state — see discord.auth_status".into(),
        ));
    }
    let creds = gateway::current_credentials(config, &**store).ok_or((
        "not_authenticated".to_string(),
        "no Discord credentials available — run `copad-plugin-discord auth` or set COPAD_DISCORD_BOT_TOKEN"
            .to_string(),
    ))?;
    let channel_id = require_snowflake(params, "channel_id")?;
    let message_id = require_snowflake(params, "message_id")?;
    match api::get_message(&creds.bot_token, channel_id, message_id) {
        // Pass through Discord's full message JSON. Trigger
        // interpolation sees object-key fields like
        // `event.await.content` and `event.await.author.id` via the
        // dot-path interpolator. Array fields (`attachments`,
        // `mentions`) are present in the value but not indexable from
        // the DSL today — no `[0]` syntax. Wrapping it under a known
        // key would force users to know our wrapper shape and break
        // the symmetry with `discord.message`'s `event_json` raw
        // field.
        Ok(value) => Ok(value),
        Err(api::ApiError { code, message }) => Err((code, message)),
    }
}

/// Trust-boundary: snowflake check (non-empty decimal-only string)
/// before splicing into a Discord API path so a malformed param can't
/// redirect the authenticated request elsewhere.
fn require_snowflake<'a>(params: &'a Value, key: &str) -> Result<&'a str, (String, String)> {
    let s = params.get(key).and_then(Value::as_str).ok_or_else(|| {
        (
            "invalid_params".to_string(),
            format!("missing '{key}' (string)"),
        )
    })?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err((
            "invalid_params".to_string(),
            format!("'{key}' must be a Discord snowflake (decimal digits only); got {s:?}"),
        ));
    }
    Ok(s)
}

fn send_response(stdout: &StdoutHandle, id: &str, result: Value) {
    let frame = json!({ "id": id, "ok": true, "result": result });
    emit(stdout, &frame.to_string());
}

fn send_error(stdout: &StdoutHandle, id: &str, code: &str, message: &str) {
    let frame = json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message },
    });
    emit(stdout, &frame.to_string());
}

fn emit(stdout: &StdoutHandle, line: &str) {
    let mut out = match stdout.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if writeln!(out, "{line}").is_err() {
        return;
    }
    let _ = out.flush();
}
