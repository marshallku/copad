//! Anthropic Messages API client: `POST https://api.anthropic.com/v1/messages` with `x-api-key`,
//! `anthropic-version: 2023-06-01`, and a JSON body of `model`, `max_tokens`, `messages` and
//! optional `system` / `temperature`.
//!
//! Errors mirror the slack plugin's `chat.postMessage` shape so callers branch uniformly:
//! 401 -> `auth_error`; 429 -> `rate_limited (Retry-After: <seconds>)`; other 4xx -> the error
//! code from the response body if present; 5xx / network -> a generic transport error.
//! Non-streaming only.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Serialize)]
pub struct CompleteRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub messages: Vec<Message<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message<'a> {
    pub role: &'a str, // "user" | "assistant"
    pub content: &'a str,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompleteResponse {
    /// Concatenation of all `content[type=text]` blocks, so callers need not walk the typed-block
    /// array.
    pub text: String,
    pub model: String,
    pub stop_reason: Option<String>,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

pub fn complete(
    api_key: &str,
    req: &CompleteRequest<'_>,
    http_timeout: Duration,
) -> Result<CompleteResponse, String> {
    let body =
        serde_json::to_string(req).map_err(|e| format!("complete request serialize: {e}"))?;
    let resp = match ureq::post(MESSAGES_URL)
        .set("x-api-key", api_key)
        .set("anthropic-version", ANTHROPIC_VERSION)
        .set("content-type", "application/json")
        .timeout(http_timeout)
        .send_string(&body)
    {
        Ok(r) => r,
        Err(ureq::Error::Status(401, r)) => {
            let body = r.into_string().unwrap_or_default();
            return Err(format!("auth_error: {body}"));
        }
        Err(ureq::Error::Status(429, r)) => {
            let retry = r
                .header("retry-after")
                .map(str::to_string)
                .unwrap_or_default();
            return Err(format!(
                "rate_limited (Retry-After: {})",
                if retry.is_empty() { "unknown" } else { &retry }
            ));
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            return Err(format!("messages HTTP {code}: {body}"));
        }
        Err(e) => return Err(format!("messages transport: {e}")),
    };
    parse_response(
        resp.into_json::<Value>()
            .map_err(|e| format!("messages parse: {e}"))?,
    )
}

fn parse_response(body: Value) -> Result<CompleteResponse, String> {
    // Success shape: { id, type:"message", role, model, content:[{type, text}], stop_reason, usage
    // }.
    // Logical failures carry `{type:"error", error:{type, message}}`, normally with a 4xx, but the
    // top-level `type:"error"` can also appear in a 200, so handle both.
    if body.get("type").and_then(Value::as_str) == Some("error") {
        let error_type = body
            .get("error")
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let error_msg = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(format!("{error_type}: {error_msg}"));
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "messages response missing model".to_string())?
        .to_string();
    let stop_reason = body
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    // Concatenate every text block, skipping others (tool_use): requests are text-only, so none
    // are expected.
    let text = body
        .get("content")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    let t = b.get("type").and_then(Value::as_str)?;
                    if t == "text" {
                        b.get("text").and_then(Value::as_str).map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let input_tokens = body
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let output_tokens = body
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    Ok(CompleteResponse {
        text,
        model,
        stop_reason,
        input_tokens,
        output_tokens,
    })
}

/// 1-token "ping" so the `auth` subcommand can surface invalid keys before persisting.
pub fn validate_key(api_key: &str, model: &str) -> Result<(), String> {
    let req = CompleteRequest {
        model,
        max_tokens: 1,
        messages: vec![Message {
            role: "user",
            content: "ping",
        }],
        system: None,
        temperature: None,
    };
    complete(api_key, &req, Duration::from_secs(20)).map(|_r| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_response_concatenates_text_blocks() {
        let body = json!({
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "stop_reason": "end_turn",
            "content": [
                {"type": "text", "text": "Hello"},
                {"type": "text", "text": ", world."},
            ],
            "usage": {"input_tokens": 12, "output_tokens": 4},
        });
        let r = parse_response(body).unwrap();
        assert_eq!(r.text, "Hello, world.");
        assert_eq!(r.model, "claude-sonnet-4-6");
        assert_eq!(r.input_tokens, 12);
        assert_eq!(r.output_tokens, 4);
        assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn parse_response_skips_non_text_blocks() {
        let body = json!({
            "type": "message",
            "model": "x",
            "content": [
                {"type": "text", "text": "user-visible"},
                {"type": "tool_use", "id": "t1", "name": "f", "input": {}},
            ],
            "usage": {"input_tokens": 0, "output_tokens": 0},
        });
        let r = parse_response(body).unwrap();
        assert_eq!(r.text, "user-visible");
    }

    #[test]
    fn parse_response_surfaces_top_level_error() {
        let body = json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": "bad model"},
        });
        let err = parse_response(body).unwrap_err();
        assert!(err.starts_with("invalid_request_error:"), "got {err}");
    }

    #[test]
    fn parse_response_handles_missing_usage() {
        let body = json!({
            "type": "message",
            "model": "x",
            "content": [{"type": "text", "text": "ok"}],
        });
        let r = parse_response(body).unwrap();
        assert_eq!(r.input_tokens, 0);
        assert_eq!(r.output_tokens, 0);
    }
}
