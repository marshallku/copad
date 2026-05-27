//! Discord REST API helpers for write actions.
//!
//! Slice 2 ships only `send_message` (POST `/channels/{id}/messages`).
//! Future: edit/delete, reactions, file uploads, threads.
//!
//! Rate limiting: Discord uses per-route bucket headers (`X-RateLimit-*`)
//! plus a global limit. We don't pre-emptively throttle — we surface
//! HTTP 429 verbatim with the `Retry-After` value so triggers / the
//! UI can decide whether to back off and retry. A future scheduler
//! can layer on top of this without changing the call site.
//!
//! Errors return as `(code, message)` so the plugin's RPC layer can
//! forward `code` directly as the action error code. That preserves
//! Discord's numeric error codes (`discord_50001` for "Missing
//! Access" etc.) at the structured layer instead of burying them in
//! free-text — without that, downstream triggers couldn't branch on
//! specific Discord failures via the action-completion fanout, which
//! only preserves `{code, message}`.

use std::time::Duration;

use serde_json::{Value, json};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// `code` ∈ `rate_limited` / `discord_<numeric>` / `io_error`.
pub struct ApiError {
    pub code: String,
    pub message: String,
}

/// Shared error classifier — `rate_limited` (429 + Retry-After),
/// `discord_<numeric>` (body parses as `{code, message}`), `io_error`
/// (transport / unparseable body).
fn classify_response_error(err: ureq::Error) -> ApiError {
    match err {
        ureq::Error::Status(429, r) => {
            let header_retry = r
                .header("Retry-After")
                .map(str::to_string)
                .unwrap_or_default();
            let body_retry = r
                .into_json::<Value>()
                .ok()
                .and_then(|v| v.get("retry_after").and_then(Value::as_f64))
                .map(|s| format!("{s:.3}"))
                .unwrap_or_default();
            ApiError {
                code: "rate_limited".to_string(),
                message: format!(
                    "Retry-After: {}; body retry_after: {}",
                    if header_retry.is_empty() {
                        "unknown"
                    } else {
                        &header_retry
                    },
                    if body_retry.is_empty() {
                        "unknown"
                    } else {
                        &body_retry
                    }
                ),
            }
        }
        ureq::Error::Status(http_code, r) => {
            let body = r.into_string().unwrap_or_default();
            if let Ok(v) = serde_json::from_str::<Value>(&body)
                && let Some(dc) = v.get("code").and_then(Value::as_u64)
            {
                let dm = v
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)");
                return ApiError {
                    code: format!("discord_{dc}"),
                    message: format!("HTTP {http_code}: {dm}"),
                };
            }
            ApiError {
                code: "io_error".to_string(),
                message: format!("HTTP {http_code}: {body}"),
            }
        }
        e => ApiError {
            code: "io_error".to_string(),
            message: format!("transport: {e}"),
        },
    }
}

/// `(message_id, channel_id)` on success — `message_id` is the canonical
/// handle for edits/reactions. `content` must be ≤ 2000 chars (Discord
/// hard limit); over-length is rejected with `BASE_TYPE_MAX_LENGTH`.
pub fn post_message(
    bot_token: &str,
    channel_id: &str,
    content: &str,
) -> Result<(String, String), ApiError> {
    let body = json!({ "content": content });
    let resp = ureq::post(&format!(
        "{DISCORD_API_BASE}/channels/{channel_id}/messages"
    ))
    .set("Authorization", &format!("Bot {bot_token}"))
    .set("User-Agent", "copad-plugin-discord (copad, 0.1)")
    .timeout(HTTP_TIMEOUT)
    .send_json(body)
    .map_err(classify_response_error)?;
    let v: Value = resp.into_json().map_err(|e| ApiError {
        code: "io_error".to_string(),
        message: format!("send_message response parse: {e}"),
    })?;
    let message_id = v
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError {
            code: "io_error".to_string(),
            message: "send_message response missing id".to_string(),
        })?
        .to_string();
    let posted_channel = v
        .get("channel_id")
        .and_then(Value::as_str)
        .unwrap_or(channel_id)
        .to_string();
    Ok((message_id, posted_channel))
}

/// Single-message fetch (reactions don't carry the body). Returns the
/// verbatim Discord message object so triggers reach any nested field.
pub fn get_message(bot_token: &str, channel_id: &str, message_id: &str) -> Result<Value, ApiError> {
    let resp = ureq::get(&format!(
        "{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}"
    ))
    .set("Authorization", &format!("Bot {bot_token}"))
    .set("User-Agent", "copad-plugin-discord (copad, 0.1)")
    .timeout(HTTP_TIMEOUT)
    .call()
    .map_err(classify_response_error)?;
    resp.into_json().map_err(|e| ApiError {
        code: "io_error".to_string(),
        message: format!("get_message response parse: {e}"),
    })
}
