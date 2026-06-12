//! First-party Slack service plugin for copad.
//!
//! Two run modes (selected by `argv[1]`):
//! - **`auth`** — validates the env tokens against Slack's
//!   `auth.test` endpoint and persists the validated TokenSet
//!   (with team/user IDs) to the configured store. Exits 0 on
//!   success.
//! - **(no args)** — RPC mode. Speaks the copad service-plugin
//!   protocol over stdio, runs Socket Mode WebSocket in a background
//!   thread, and publishes `slack.mention` / `slack.dm` events when
//!   real human messages arrive.
//!
//! If RPC mode starts with no stored credentials AND the env tokens
//! are missing, the supervisor handshake still completes — the
//! Socket Mode loop just stays paused. The user can run
//! `copad-plugin-slack auth` while copad is running and the loop
//! picks up the new credentials on its next reconnect attempt.
//!
//! See `docs/service-plugins.md` for the protocol contract. Slack
//! plugin is purely an event emitter (and an authenticator) — the
//! action it takes when a mention arrives is entirely user trigger
//! config (kb.append, webhook.fire, etc.).

#[cfg(not(unix))]
compile_error!(
    "copad-plugin-slack is currently Unix-only. The keyring crate's mock fallback \
     would silently lose tokens on platforms without a native credential-store \
     feature; gate exists to make that failure compile-time instead of runtime."
);

mod bot_rpc;
mod channels;
mod collect;
mod config;
mod events;
mod socket_mode;
mod store;

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use serde_json::{Value, json};

use bot_rpc::{BotRpcRegistry, IncomingMessage, PendingWait, compile_regex, is_matchable_subtype};
use channels::{BotRpcConfig, ChannelEntry, ChannelProfile, ChannelStore, WaitMode};
use collect::{CollectLine, CollectStore};
use config::Config;
use events::SlackEvent;
use store::{TokenSet, TokenStore};

const PROTOCOL_VERSION: u32 = 1;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("auth") => run_auth(),
        Some(other) => {
            eprintln!("[slack] unknown subcommand: {other}");
            eprintln!("usage: copad-plugin-slack [auth]");
            std::process::exit(2);
        }
        None => run_rpc(),
    }
}

fn run_auth() {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[slack] config error: {e}");
            std::process::exit(1);
        }
    };
    if config.bot_token.is_empty() {
        eprintln!("[slack] auth requires COPAD_SLACK_BOT_TOKEN (xoxb-...)");
        std::process::exit(1);
    }
    if config.app_token.is_empty() {
        eprintln!("[slack] auth requires COPAD_SLACK_APP_TOKEN (xapp-...)");
        std::process::exit(1);
    }
    let store = store::open_store(&config);
    eprintln!("[slack] token store: {}", store.kind());

    eprintln!("[slack] validating bot token via auth.test...");
    let (bot_team_id, bot_user_id) = match socket_mode::auth_test(&config.bot_token) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[slack] auth.test (bot) failed: {e}");
            std::process::exit(1);
        }
    };
    // `auth.test` on xapp tokens returns `{ok, app_id, app_name}` with
    // no team_id/user_id, so the cross-workspace match against the bot
    // can't actually be enforced here. `apps.connections.open` below
    // is the only meaningful xapp validation Slack supports for our
    // use case (token alive + has `connections:write` scope).
    eprintln!("[slack] validating app token via apps.connections.open...");
    if let Err(e) = socket_mode::validate_app_token(&config.app_token) {
        eprintln!(
            "[slack] apps.connections.open failed: {e}\n\
             [slack] the App-Level Token must have the `connections:write` scope"
        );
        std::process::exit(1);
    }
    // Same-workspace check: a user token from a different team would let
    // `as_user: true` posts target unintended workspaces.
    let user_token = if config.user_token.is_empty() {
        String::new()
    } else {
        eprintln!("[slack] validating user token via auth.test...");
        let (user_team_id, _) = match socket_mode::auth_test(&config.user_token) {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "[slack] auth.test (user) failed: {e}\n\
                     [slack] verify the User OAuth Token (xoxp-...) is correct \
                     and the user scopes are granted (OAuth & Permissions → User Token Scopes)"
                );
                std::process::exit(1);
            }
        };
        if user_team_id != bot_team_id {
            eprintln!(
                "[slack] token mismatch — user token belongs to team {user_team_id} but bot belongs to {bot_team_id}.\n\
                 [slack] the user token must come from the SAME workspace as the bot."
            );
            std::process::exit(1);
        }
        config.user_token.clone()
    };
    let team_id = bot_team_id;
    let user_id = bot_user_id;
    let tokens = TokenSet {
        bot_token: config.bot_token.clone(),
        app_token: config.app_token.clone(),
        team_id: Some(team_id.clone()),
        user_id: Some(user_id.clone()),
        user_token: user_token.clone(),
    };
    if let Err(e) = store.save(&tokens) {
        eprintln!("[slack] failed to save tokens: {e}");
        std::process::exit(1);
    }
    let user_token_note = if user_token.is_empty() {
        "bot-only"
    } else {
        "bot+user"
    };
    eprintln!(
        "[slack] auth ok — team={team_id} user={user_id} mode={user_token_note} stored ({})",
        store.kind()
    );
}

fn run_rpc() {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[slack] FATAL config error — Socket Mode disabled until fixed: {e}");
            Config::minimal_with_error(e)
        }
    };
    let store: Arc<dyn TokenStore> = Arc::from(store::open_store(&config));
    let channel_store = Arc::new(ChannelStore::new(config.channel_path.clone()));
    let collect_store = Arc::new(CollectStore::new(config.collect_dir.clone()));
    let bot_rpc_registry = Arc::new(BotRpcRegistry::new());
    let bot_user_id_cache: Arc<std::sync::Mutex<Option<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(None));
    eprintln!(
        "[slack] token store: {} (env tokens: {})",
        store.kind(),
        if config.env_tokens_empty() {
            "empty — will fall back to store"
        } else {
            "present — will override store"
        }
    );
    eprintln!("[slack] channel store: {}", config.channel_path.display());
    eprintln!("[slack] collect dir:   {}", config.collect_dir.display());

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    // Single writer thread funnels all outgoing JSON so init reply,
    // action replies, and event.publish notifications never interleave.
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
    let stop_signal = Arc::new(AtomicBool::new(false));

    // Socket Mode loop runs in a background thread. It waits for the
    // `initialized` notification before connecting so events can't
    // race the handshake. The loop itself is responsible for
    // resolving credentials (env then store) on every iteration —
    // running `copad-plugin-slack auth` while copad is already up
    // populates the store and the loop picks it up on the next
    // recheck (no plugin process restart required).
    {
        let init_flag = initialized.clone();
        let stop = stop_signal.clone();
        let event_tx = tx.clone();
        let cfg = config.clone();
        let store_for_loop = store.clone();
        let collect_for_loop = collect_store.clone();
        let channels_for_loop = channel_store.clone();
        let registry_for_loop = bot_rpc_registry.clone();
        thread::spawn(move || {
            while !init_flag.load(Ordering::SeqCst) {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(std::time::Duration::from_millis(100));
            }
            socket_mode::run_loop(&cfg, store_for_loop, &stop, |event| {
                // Route to collect/bot_rpc BEFORE the firehose publish so
                // a slow listener can't delay disk writes / response
                // matching. Both are best-effort: errors log but never
                // suppress the outgoing event.publish frame.
                route_for_collect_and_bot_rpc(
                    &event,
                    &channels_for_loop,
                    &collect_for_loop,
                    &registry_for_loop,
                    &event_tx,
                );
                let frame = json!({
                    "method": "event.publish",
                    "params": {
                        "kind": event.kind(),
                        "payload": event.payload_json(),
                    }
                });
                let _ = event_tx.send(frame.to_string());
            });
        });
    }

    // Sweeper publishes slack.bot_rpc.timeout for each deadlined wait.
    {
        let registry = bot_rpc_registry.clone();
        let stop = stop_signal.clone();
        let sweeper_tx = tx.clone();
        bot_rpc::start_sweeper(registry, stop, move |expired| {
            bot_rpc::dispatch_timeout(&sweeper_tx, &expired);
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
                eprintln!("[slack] parse error: {e}");
                continue;
            }
        };
        handle_frame(
            &frame,
            &writer_tx,
            &initialized,
            &stop_signal,
            &config,
            &store,
            &channel_store,
            &bot_rpc_registry,
            &bot_user_id_cache,
        );
    }
}

/// Routes a `SlackEvent::Raw` message to (a) JSONL append for
/// collect-profile channels and (b) bot_rpc registry's
/// `ingest_message` for any pending wait correlations. Both paths
/// filter on the matcher subtype contract so edits / joins / etc.
/// don't sneak in.
fn route_for_collect_and_bot_rpc(
    event: &SlackEvent,
    channels: &Arc<ChannelStore>,
    collect_store: &Arc<CollectStore>,
    registry: &Arc<BotRpcRegistry>,
    tx: &Sender<String>,
) {
    let SlackEvent::Raw(raw) = event else {
        return;
    };
    if raw.event_type != "message" {
        return;
    }
    let inner = &raw.event_json;
    let subtype = inner.get("subtype").and_then(Value::as_str);
    if !is_matchable_subtype(subtype) {
        return;
    }
    // Author extraction: prefer `user`, accept `bot_message` shape
    // where Slack puts `user` alongside `bot_id`. Bots without `user`
    // (rare; legacy webhooks) are dropped — see `wait_user_filter`
    // contract in bot_rpc.rs.
    let Some(user) = inner.get("user").and_then(Value::as_str) else {
        return;
    };
    let Some(channel) = raw.channel.clone() else {
        return;
    };
    let Some(ts) = raw.ts.clone() else {
        return;
    };
    let text = inner
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let thread_ts = inner
        .get("thread_ts")
        .and_then(Value::as_str)
        .map(str::to_string);

    // Bot-RPC ingest first — registry's recent buffer must be primed
    // before the collect append (cheap, in-memory). If a match fires
    // here, publish completed.
    let msg = IncomingMessage {
        channel: channel.clone(),
        ts: ts.clone(),
        user: user.to_string(),
        text: text.clone(),
        thread_ts: thread_ts.clone(),
        event_id: raw.event_id.clone(),
        team_id: raw.team_id.clone(),
        received_at_ms: collect::now_ms(),
    };
    if let Some(matched) = registry.ingest_message(msg) {
        bot_rpc::dispatch_completed(tx, &matched);
    }

    // Collect: routing reads the channel entry on every message.
    // Cheap (small JSON file) and avoids cache invalidation when
    // the user upserts a channel mid-stream.
    //
    // Stricter filter than the bot-rpc matcher: knowledge-base capture
    // wants normal human messages only. `bot_message` subtype is
    // allowed for bot-rpc (Jira replies) but explicitly excluded here
    // per the intent's AC — "bot 메시지 또는 subtype 있는 message
    // (edit/delete/join 등)가 와도 jsonl에 기록되지 않음".
    if subtype.is_some() {
        return;
    }
    if inner.get("bot_id").is_some() {
        return;
    }
    let file = channels.load();
    let matches_collect = file
        .channels
        .iter()
        .any(|c| c.id == channel && matches!(c.profile, ChannelProfile::Collect));
    if matches_collect {
        let line = CollectLine {
            channel: channel.clone(),
            ts,
            user: user.to_string(),
            text,
            thread_ts,
            team_id: raw.team_id.clone(),
            event_id: raw.event_id.clone(),
            captured_at_ms: collect::now_ms(),
        };
        if let Err(e) = collect_store.append(&line) {
            eprintln!("[slack] collect append failed: {e}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_frame(
    frame: &Value,
    tx: &Sender<String>,
    initialized: &AtomicBool,
    stop_signal: &AtomicBool,
    config: &Config,
    store: &Arc<dyn TokenStore>,
    channel_store: &Arc<ChannelStore>,
    bot_rpc_registry: &Arc<BotRpcRegistry>,
    bot_user_id_cache: &Arc<std::sync::Mutex<Option<(String, String)>>>,
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
                    &format!("slack plugin speaks protocol {PROTOCOL_VERSION}; got {proto:?}"),
                );
                return;
            }
            send_response(
                tx,
                id,
                json!({
                    "service_version": env!("CARGO_PKG_VERSION"),
                    "provides": [
                        "slack.auth_status",
                        "slack.post_message",
                        "slack.get_message",
                        "slack.channels.list",
                        "slack.channels.upsert",
                        "slack.channels.remove",
                        "slack.channels.discover",
                        "slack.bot_rpc.invoke",
                    ],
                    "subscribes": [],
                }),
            );
        }
        "initialized" => {
            initialized.store(true, Ordering::SeqCst);
            if let Some(err) = &config.fatal_error {
                emit_config_error(tx, err);
            }
        }
        "action.invoke" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let action_params = params.get("params").cloned().unwrap_or(Value::Null);
            let result = handle_action(
                &name,
                &action_params,
                config,
                store,
                channel_store,
                bot_rpc_registry,
                bot_user_id_cache,
                tx,
            );
            match result {
                Ok(v) => send_response(tx, id, v),
                Err((code, msg)) => send_error(tx, id, &code, &msg),
            }
        }
        "event.dispatch" => {
            // slack plugin doesn't subscribe — quietly ignore.
        }
        "shutdown" => {
            stop_signal.store(true, Ordering::SeqCst);
            std::process::exit(0);
        }
        other if !other.is_empty() && !id.is_empty() => {
            send_error(
                tx,
                id,
                "unknown_method",
                &format!("slack plugin: unknown method {other}"),
            );
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_action(
    name: &str,
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
    channel_store: &Arc<ChannelStore>,
    bot_rpc_registry: &Arc<BotRpcRegistry>,
    bot_user_id_cache: &Arc<std::sync::Mutex<Option<(String, String)>>>,
    tx: &Sender<String>,
) -> Result<Value, (String, String)> {
    if name == "slack.auth_status" {
        // Round-5 fix: when env validation produced a fatal_error,
        // the runtime loop refuses to connect — so auth_status
        // MUST NOT report `authenticated=true` based on a
        // fall-through default-workspace store load, otherwise the
        // status surface would lie about the runtime state.
        // Short-circuit to the disabled view.
        if let Some(err) = &config.fatal_error {
            return Ok(json!({
                "configured": false,
                "authenticated": false,
                "credentials_source": "none",
                "fatal_error": err,
                "store_kind": store.kind(),
                "workspace": config.workspace_label.clone(),
                "has_user_token": false,
                "team_id": Value::Null,
                "user_id": Value::Null,
            }));
        }
        // Resolve credentials through the SAME function the Socket
        // Mode loop uses — keeps reported `credentials_source`
        // identical to the live source the runtime would actually
        // use. Returning anything else would let the user see
        // "store" in auth_status while the loop reads from "env",
        // which is the round-2 cross-review concern.
        let resolved = socket_mode::current_credentials(config, &**store);
        let stored = store.load();
        let credentials_source = resolved.as_ref().map(|c| c.source).unwrap_or("none");
        let authenticated = resolved.is_some();
        // Identity (team_id, user_id) only meaningful when the live
        // source is the store — that's the only path where we
        // validated identity via auth.test at `auth` time. For
        // env-overridden credentials we don't have a verified
        // (team_id, user_id) for THOSE specific tokens, so reporting
        // the stored identity would be misleading (the env tokens
        // could be from a different workspace). Surface them only
        // when consistent with the live source.
        let report_identity = credentials_source == "store";
        let has_user_token = resolved.as_ref().is_some_and(|c| c.user_token.is_some());
        return Ok(json!({
            "configured": true,
            "authenticated": authenticated,
            "credentials_source": credentials_source,
            "fatal_error": Value::Null,
            "store_kind": store.kind(),
            "workspace": config.workspace_label.clone(),
            "has_user_token": has_user_token,
            "team_id": if report_identity {
                stored.as_ref().and_then(|t| t.team_id.clone())
            } else { None },
            "user_id": if report_identity {
                stored.as_ref().and_then(|t| t.user_id.clone())
            } else { None },
        }));
    }
    if name == "slack.post_message" {
        return handle_post_message(params, config, store);
    }
    if name == "slack.get_message" {
        return handle_get_message(params, config, store);
    }
    if name == "slack.channels.list" {
        return handle_channels_list(channel_store);
    }
    if name == "slack.channels.upsert" {
        return handle_channels_upsert(params, config, store, channel_store);
    }
    if name == "slack.channels.remove" {
        return handle_channels_remove(params, channel_store);
    }
    if name == "slack.channels.discover" {
        return handle_channels_discover(params, config, store);
    }
    if name == "slack.bot_rpc.invoke" {
        return handle_bot_rpc_invoke(
            params,
            config,
            store,
            channel_store,
            bot_rpc_registry,
            bot_user_id_cache,
            tx,
        );
    }
    Err((
        "action_not_found".to_string(),
        format!("slack plugin does not handle {name}"),
    ))
}

/// `slack.channels.discover {cursor?, types?, limit?}` — single page
/// of `conversations.list`. Caller paginates by passing back the
/// `next_cursor` in subsequent calls. `types` defaults to
/// `public_channel,private_channel` (DMs/MPIMs rarely have meaningful
/// names; user can override).
fn handle_channels_discover(
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
) -> Result<Value, (String, String)> {
    if config.fatal_error.is_some() {
        return Err((
            "not_authenticated".to_string(),
            "slack plugin is in fatal-config state — see slack.auth_status".to_string(),
        ));
    }
    let creds = socket_mode::current_credentials(config, &**store).ok_or((
        "not_authenticated".to_string(),
        "no Slack credentials available — run `copad-plugin-slack auth` or set env tokens"
            .to_string(),
    ))?;
    let cursor = params.get("cursor").and_then(Value::as_str);
    // Hard-cap at 200 per intent AC ("첫 페이지 최대 200"). Caller can
    // pass `limit` for *smaller* pages (e.g. 50) to keep modal scroll
    // sane, but cannot widen — that would invite Tier-2 rate-limit
    // pressure under user-driven pagination.
    let limit_raw = params.get("limit").and_then(Value::as_u64).unwrap_or(200);
    let limit = limit_raw.min(200) as u32;
    let types_raw = params
        .get("types")
        .and_then(Value::as_str)
        .unwrap_or("public_channel,private_channel");
    // Allowlist: Slack documents exactly these four. Reject unknown
    // segments before sending — the API would 200-with-ok=false but a
    // pre-check is cleaner than parsing back the error.
    for t in types_raw.split(',').map(str::trim) {
        if !matches!(t, "public_channel" | "private_channel" | "mpim" | "im") {
            return Err((
                "invalid_params".to_string(),
                format!("'types' contains unknown segment {t:?}"),
            ));
        }
    }
    let (channels, next_cursor) =
        socket_mode::conversations_list(&creds.bot_token, cursor, types_raw, limit)
            .map_err(map_slack_error)?;
    let summarized: Vec<Value> = channels
        .iter()
        .map(|c| {
            json!({
                "id": c.get("id").and_then(Value::as_str).unwrap_or(""),
                "name": c.get("name").and_then(Value::as_str).unwrap_or(""),
                "is_private": c.get("is_private").and_then(Value::as_bool).unwrap_or(false),
                "is_member": c.get("is_member").and_then(Value::as_bool).unwrap_or(false),
                "is_archived": c.get("is_archived").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    Ok(json!({
        "channels": summarized,
        "next_cursor": next_cursor,
    }))
}

/// Map a Slack-shaped error string to (code, message) the same way
/// `handle_get_message` does — Slack-shaped codes (lowercase + underscore)
/// promote to top-level so triggers can branch.
fn map_slack_error(err: String) -> (String, String) {
    let bare = err
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("");
    if !bare.is_empty() && bare.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        (bare.to_string(), err)
    } else {
        ("io_error".to_string(), err)
    }
}

/// `slack.bot_rpc.invoke {channel, text?}` — posts the configured
/// template (or `text` override) to the channel and registers a
/// pending wait for the response. Returns `{request_ts}` so the
/// caller can correlate the upcoming `slack.bot_rpc.completed` /
/// `slack.bot_rpc.timeout` notifications.
#[allow(clippy::too_many_arguments)]
fn handle_bot_rpc_invoke(
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
    channel_store: &Arc<ChannelStore>,
    bot_rpc_registry: &Arc<BotRpcRegistry>,
    bot_user_id_cache: &Arc<std::sync::Mutex<Option<(String, String)>>>,
    tx: &Sender<String>,
) -> Result<Value, (String, String)> {
    if config.fatal_error.is_some() {
        return Err((
            "not_authenticated".to_string(),
            "slack plugin is in fatal-config state — see slack.auth_status".to_string(),
        ));
    }
    let channel_id = params.get("channel").and_then(Value::as_str).ok_or((
        "invalid_params".to_string(),
        "missing 'channel' (string)".to_string(),
    ))?;
    if !channels::is_valid_channel_id(channel_id) {
        return Err((
            "invalid_params".to_string(),
            format!("'channel' must be a Slack id; got {channel_id:?}"),
        ));
    }
    let creds = socket_mode::current_credentials(config, &**store).ok_or((
        "not_authenticated".to_string(),
        "no Slack credentials available — run `copad-plugin-slack auth` or set env tokens"
            .to_string(),
    ))?;
    let file = channel_store.load();
    let entry = file
        .channels
        .iter()
        .find(|c| c.id == channel_id)
        .cloned()
        .ok_or((
            "precondition_failed".to_string(),
            format!("channel {channel_id} not registered — upsert it first"),
        ))?;
    if !matches!(entry.profile, ChannelProfile::BotRpc) {
        return Err((
            "precondition_failed".to_string(),
            format!(
                "channel {channel_id} has profile {:?}, not bot-rpc",
                entry.profile
            ),
        ));
    }
    // Re-validate the loaded entry against current rules — legacy rows
    // written before the zero-correlation / zero-timeout gates were
    // added would otherwise sneak past `compile_regex` alone and
    // complete on unrelated channel chatter.
    channels::validate_entry(&entry).map_err(|e| {
        (
            "precondition_failed".to_string(),
            format!("legacy row failed validation — re-upsert to fix: {e}"),
        )
    })?;
    let cfg = entry.bot_rpc.ok_or((
        "precondition_failed".to_string(),
        format!("channel {channel_id} is bot-rpc but has no bot_rpc config"),
    ))?;
    let compiled_regex = compile_regex(&cfg).map_err(|e| ("invalid_params".to_string(), e))?;
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cfg.default_template.clone());
    if text.is_empty() {
        return Err((
            "invalid_params".to_string(),
            "no 'text' override and channel has empty default_template".to_string(),
        ));
    }
    // bot_user_id is REQUIRED — without it the self-echo guard in the
    // matcher can't function. Prefer the resolved credentials' value,
    // fall back to live auth.test (cached by bot_token) so env-only
    // setups still work.
    let bot_user_id = resolve_bot_user_id(&creds, bot_user_id_cache)?;
    let (request_ts, posted_channel) =
        socket_mode::post_message(&creds.bot_token, channel_id, &text, None)
            .map_err(map_slack_error)?;
    let deadline_ms = collect::now_ms().saturating_add(cfg.wait_timeout_ms);
    let wait = PendingWait {
        request_ts: request_ts.clone(),
        channel: posted_channel.clone(),
        config: cfg,
        deadline_ms,
        bot_user_id,
        compiled_regex,
    };
    // add() also replays the recent message buffer — a response that
    // arrived between postMessage send and HTTP response receive will
    // already have been ingested via the Socket Mode loop, so we
    // catch it here without any race.
    if let Some(matched) = bot_rpc_registry.add(wait) {
        bot_rpc::dispatch_completed(tx, &matched);
    }
    Ok(json!({
        "request_ts": request_ts,
        "channel": posted_channel,
    }))
}

/// Resolves the bot's own Slack user id, caching against the
/// originating bot token. Required by `bot_rpc.invoke` so the matcher's
/// self-echo guard has a real value to compare against.
fn resolve_bot_user_id(
    creds: &socket_mode::ResolvedCredentials,
    cache: &Arc<std::sync::Mutex<Option<(String, String)>>>,
) -> Result<String, (String, String)> {
    if let Some(uid) = creds.bot_user_id.clone() {
        return Ok(uid);
    }
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_token, cached_uid)) = guard.as_ref()
            && cached_token == &creds.bot_token
        {
            return Ok(cached_uid.clone());
        }
    }
    let (_, user_id) = socket_mode::auth_test(&creds.bot_token).map_err(|e| {
        (
            "bot_user_unresolved".to_string(),
            format!("auth.test for bot user id: {e}"),
        )
    })?;
    {
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some((creds.bot_token.clone(), user_id.clone()));
    }
    Ok(user_id)
}

fn handle_channels_list(channel_store: &Arc<ChannelStore>) -> Result<Value, (String, String)> {
    let file = channel_store.load();
    Ok(json!({
        "channels": file.channels,
        "version": file.version,
    }))
}

/// `slack.channels.upsert` — insert or replace by `id`. For C/G prefix
/// channels, this calls `conversations.info` to enrich `name` (overrides
/// user-provided name on success; falls back to user input on API failure
/// so a missing-scope error doesn't block the user from registering a
/// channel they care about).
fn handle_channels_upsert(
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
    channel_store: &Arc<ChannelStore>,
) -> Result<Value, (String, String)> {
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or((
            "invalid_params".to_string(),
            "missing 'id' (string)".to_string(),
        ))?
        .to_string();
    // Validate the channel id BEFORE any outbound Slack call (the C/G
    // `conversations.info` enrichment below). An unvalidated caller-
    // controlled value would otherwise reach slack.com in the query
    // string before `invalid_params` is returned — the ChannelStore
    // re-validates on persist, but only the early check keeps malformed
    // input out of the network.
    if !channels::is_valid_channel_id(&id) {
        return Err((
            "invalid_params".to_string(),
            format!(
                "'id' must match [CDGU][A-Z0-9]+ (Slack channel id); got {:?}",
                id
            ),
        ));
    }
    let user_name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let profile_str = params
        .get("profile")
        .and_then(Value::as_str)
        .ok_or((
            "invalid_params".to_string(),
            "missing 'profile' (\"read\" | \"collect\" | \"bot-rpc\")".to_string(),
        ))?
        .to_string();
    let profile: ChannelProfile = serde_json::from_value(Value::String(profile_str.clone()))
        .map_err(|_| {
            (
                "invalid_params".to_string(),
                format!(
                    "'profile' must be one of read|collect|bot-rpc; got {:?}",
                    profile_str
                ),
            )
        })?;
    let bot_rpc: Option<BotRpcConfig> = match params.get("bot_rpc") {
        None | Some(Value::Null) => None,
        Some(v) => Some(parse_bot_rpc(v)?),
    };

    // Auto-fetch name for public/private channels. Best effort: if the API
    // fails (no scope, transient error), fall back to user-provided name.
    // We deliberately do NOT abort upsert on lookup failure — the registry
    // entry is local-only metadata, and a missing-scope error shouldn't
    // block the user from saving a channel they care about.
    let resolved_name = if channels::channel_supports_name(&id) {
        let creds = socket_mode::current_credentials(config, &**store);
        match creds.as_ref() {
            Some(c) => match socket_mode::conversations_info(&c.bot_token, &id) {
                Ok(Some(api_name)) => api_name,
                Ok(None) => user_name.clone(),
                Err(e) => {
                    eprintln!(
                        "[slack] conversations.info {id} failed: {e} — \
                         falling back to user-provided name"
                    );
                    user_name.clone()
                }
            },
            None => user_name.clone(),
        }
    } else {
        user_name.clone()
    };

    let entry = ChannelEntry {
        id,
        name: resolved_name,
        profile,
        bot_rpc,
        collect: None,
        added_at: 0,
        updated_at: 0,
    };
    let saved = channel_store
        .upsert(entry)
        .map_err(|e| ("invalid_params".to_string(), e))?;
    Ok(json!({ "channel": saved }))
}

fn parse_bot_rpc(v: &Value) -> Result<BotRpcConfig, (String, String)> {
    let default_template = v
        .get("default_template")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let wait_mode_str = v.get("wait_mode").and_then(Value::as_str).ok_or((
        "invalid_params".to_string(),
        "'bot_rpc.wait_mode' required (\"first-reply\" | \"regex\")".to_string(),
    ))?;
    let wait_mode: WaitMode = serde_json::from_value(Value::String(wait_mode_str.to_string()))
        .map_err(|_| {
            (
                "invalid_params".to_string(),
                format!(
                    "'bot_rpc.wait_mode' must be first-reply|regex; got {:?}",
                    wait_mode_str
                ),
            )
        })?;
    let wait_regex = v
        .get("wait_regex")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let wait_user_filter = v
        .get("wait_user_filter")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let wait_timeout_ms = v
        .get("wait_timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let wait_in_thread = v
        .get("wait_in_thread")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(BotRpcConfig {
        default_template,
        wait_mode,
        wait_regex,
        wait_user_filter,
        wait_timeout_ms,
        wait_in_thread,
    })
}

fn handle_channels_remove(
    params: &Value,
    channel_store: &Arc<ChannelStore>,
) -> Result<Value, (String, String)> {
    let id = params.get("id").and_then(Value::as_str).ok_or((
        "invalid_params".to_string(),
        "missing 'id' (string)".to_string(),
    ))?;
    let removed = channel_store
        .remove(id)
        .map_err(|e| ("io_error".to_string(), e))?;
    Ok(json!({ "removed": removed }))
}

fn handle_get_message(
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
) -> Result<Value, (String, String)> {
    if config.fatal_error.is_some() {
        return Err((
            "not_authenticated".to_string(),
            "slack plugin is in fatal-config state — see slack.auth_status".to_string(),
        ));
    }
    let creds = socket_mode::current_credentials(config, &**store).ok_or((
        "not_authenticated".to_string(),
        "no Slack credentials available — run `copad-plugin-slack auth` or set env tokens"
            .to_string(),
    ))?;
    let channel = params.get("channel").and_then(Value::as_str).ok_or((
        "invalid_params".to_string(),
        "missing 'channel' (string)".to_string(),
    ))?;
    let ts = params.get("ts").and_then(Value::as_str).ok_or((
        "invalid_params".to_string(),
        "missing 'ts' (string)".to_string(),
    ))?;
    let as_user = params
        .get("as_user")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let token = select_action_token(&creds, as_user)?;
    // Slack channel ids start with C/D/G/U and are uppercase
    // alphanumeric. ts looks like "1700000000.000100" — digits and
    // exactly one dot. Validate to close the same trust-boundary
    // gap Discord's send_message guards against (a malicious
    // trigger pushing `../auth.test` into the URL position would
    // re-route the authenticated request).
    if !is_valid_slack_id(channel) {
        return Err((
            "invalid_params".to_string(),
            format!("'channel' must be a Slack id (alphanumeric); got {channel:?}"),
        ));
    }
    if !is_valid_slack_ts(ts) {
        return Err((
            "invalid_params".to_string(),
            format!("'ts' must be a Slack timestamp (digits.digits); got {ts:?}"),
        ));
    }
    match socket_mode::get_message(token, channel, ts) {
        Ok(value) => Ok(value),
        // Slack errors come through in two shapes:
        //   - bare error code (`channel_not_found`, `not_in_channel`,
        //     `missing_scope`, `message_not_found`)
        //   - prefix + suffix (`rate_limited (Retry-After: 30)`,
        //     `conversations.history HTTP 503: <body>`)
        // Promote the bare-code prefix to the top-level error code
        // when it parses as Slack-shaped (lowercase + underscore
        // only — every documented Slack error code is in that
        // charset). Transport-shaped messages (with `.`, digits,
        // mixed case in the prefix) stay under `io_error` with the
        // full body preserved in the message field.
        Err(err) => {
            let bare = err
                .split(|c: char| c.is_whitespace() || c == '(')
                .next()
                .unwrap_or("");
            if !bare.is_empty() && bare.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                Err((bare.to_string(), err))
            } else {
                Err(("io_error".to_string(), err))
            }
        }
    }
}

/// `as_user: true` hard-fails when no user token is configured —
/// silent fallback to the bot would post under the wrong identity.
fn select_action_token(
    creds: &socket_mode::ResolvedCredentials,
    as_user: bool,
) -> Result<&str, (String, String)> {
    if as_user {
        creds.user_token.as_deref().ok_or((
            "user_token_unavailable".to_string(),
            "no Slack User OAuth Token configured — set COPAD_SLACK_USER_TOKEN \
             or include it when running `copad-plugin-slack auth`"
                .to_string(),
        ))
    } else {
        Ok(&creds.bot_token)
    }
}

/// `[A-Z0-9]+`. Prefix (C/D/G/U/W/T) is intentionally NOT enforced —
/// `get_message` works for DM channels and shared-channel mirrors too.
fn is_valid_slack_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// `<seconds>.<microseconds>` — exactly two digit-only segments.
fn is_valid_slack_ts(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 2
        && !parts[0].is_empty()
        && !parts[1].is_empty()
        && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
}

fn handle_post_message(
    params: &Value,
    config: &Config,
    store: &Arc<dyn TokenStore>,
) -> Result<Value, (String, String)> {
    if config.fatal_error.is_some() {
        return Err((
            "not_authenticated".to_string(),
            "slack plugin is in fatal-config state — see slack.auth_status".to_string(),
        ));
    }
    // Resolve the bot token through the SAME path the Socket Mode
    // loop uses so write actions don't accidentally diverge from
    // read events. A user who's authenticated only via env, or
    // only via store, gets the right token here either way.
    let creds = socket_mode::current_credentials(config, &**store).ok_or((
        "not_authenticated".to_string(),
        "no Slack credentials available — run `copad-plugin-slack auth` or set env tokens"
            .to_string(),
    ))?;
    let channel = params.get("channel").and_then(Value::as_str).ok_or((
        "invalid_params".to_string(),
        "missing 'channel' (string)".to_string(),
    ))?;
    let text = params.get("text").and_then(Value::as_str).ok_or((
        "invalid_params".to_string(),
        "missing 'text' (string)".to_string(),
    ))?;
    let thread_ts = params.get("thread_ts").and_then(Value::as_str);
    let as_user = params
        .get("as_user")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let token = select_action_token(&creds, as_user)?;

    match socket_mode::post_message(token, channel, text, thread_ts) {
        Ok((ts, posted_channel)) => Ok(json!({
            "ts": ts,
            "channel": posted_channel,
        })),
        // Surface Slack's structured error codes verbatim — the
        // common ones are documented at api.slack.com/methods/chat.postMessage:
        // `missing_scope`, `not_in_channel`, `channel_not_found`,
        // `is_archived`, `msg_too_long`, `rate_limited`. Caller
        // (trigger / coctl) can branch on these without
        // re-parsing message strings.
        Err(err) => Err(("io_error".to_string(), err)),
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

/// Env vars/steps that fix a fatal config error — quoted verbatim in the toast the supervisor
/// raises, so a misconfigured plugin tells the user exactly what to set instead of failing silent.
const CONFIG_REMEDIATION: &str =
    "set COPAD_SLACK_BOT_TOKEN and COPAD_SLACK_APP_TOKEN, or run `copad-plugin-slack auth`";

/// Publish a one-shot `plugin.config_error` event so the daemon can toast the misconfiguration.
/// Emitted only after `initialized`, since frames sent while the service is still `Starting` may
/// be dropped before the supervisor bridges this plugin's stdout onto the bus.
fn emit_config_error(tx: &Sender<String>, message: &str) {
    let frame = json!({
        "method": "event.publish",
        "params": {
            "kind": "plugin.config_error",
            "payload": { "service": "slack", "message": message, "remediation": CONFIG_REMEDIATION },
        }
    });
    let _ = tx.send(frame.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_config_error_frame_shape() {
        let (tx, rx) = channel::<String>();
        emit_config_error(&tx, "COPAD_SLACK_BOT_TOKEN is required");
        let frame: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(frame["method"], "event.publish");
        assert_eq!(frame["params"]["kind"], "plugin.config_error");
        assert_eq!(frame["params"]["payload"]["service"], "slack");
        assert_eq!(
            frame["params"]["payload"]["message"],
            "COPAD_SLACK_BOT_TOKEN is required"
        );
        assert!(
            frame["params"]["payload"]["remediation"]
                .as_str()
                .unwrap()
                .contains("COPAD_SLACK_BOT_TOKEN")
        );
    }
}
