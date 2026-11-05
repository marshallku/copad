//! Google OAuth 2.0 device-code flow (RFC 8628) + refresh-token exchange. Google does not return a
//! new refresh_token on refresh, so the original is preserved.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::config::Config;
use crate::store::TokenSet;

const DEVICE_CODE_URL: &str = "https://oauth2.googleapis.com/device/code";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";

/// Refresh ~30s before the server-reported expiry so we don't race
/// against a clock-skewed token endpoint.
const REFRESH_LEAD_SECS: u64 = 30;

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    /// Some Google docs say `verification_url`, others `verification_uri`; accept either.
    #[serde(alias = "verification_uri")]
    verification_url: String,
    interval: u64,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    token_type: Option<String>,
    error: Option<String>,
    #[allow(dead_code)]
    error_description: Option<String>,
}

pub fn run_device_code_flow(config: &Config) -> Result<TokenSet, String> {
    if config.is_minimal() {
        return Err("client_id / client_secret missing".to_string());
    }
    let dc = request_device_code(config)?;
    eprintln!("\n[calendar] OAuth device authorization");
    eprintln!("  Visit: {}", dc.verification_url);
    eprintln!("  Code:  {}", dc.user_code);
    eprintln!("  (waiting up to {} seconds)\n", dc.expires_in);

    let mut interval = Duration::from_secs(dc.interval.max(1));
    let deadline = std::time::Instant::now() + Duration::from_secs(dc.expires_in);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(interval);
        match poll_token(config, &dc.device_code) {
            Ok(Some(tokens)) => return Ok(tokens),
            Ok(None) => {
                continue;
            }
            Err(PollError::SlowDown) => {
                interval = interval.saturating_add(Duration::from_secs(5));
                eprintln!(
                    "[calendar] server requested slow_down; new interval {}s",
                    interval.as_secs()
                );
            }
            Err(PollError::Denied) => return Err("user denied access".to_string()),
            Err(PollError::Expired) => return Err("device code expired".to_string()),
            Err(PollError::Other(e)) => return Err(e),
        }
    }
    Err("timed out waiting for user authorization".to_string())
}

fn request_device_code(config: &Config) -> Result<DeviceCodeResponse, String> {
    let resp = ureq::post(DEVICE_CODE_URL)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_form(&[("client_id", &config.client_id), ("scope", SCOPE)])
        .map_err(|e| format!("device_code request: {e}"))?;
    if resp.status() != 200 {
        let status = resp.status();
        let body = resp.into_string().unwrap_or_default();
        return Err(format!("device_code HTTP {status}: {body}"));
    }
    resp.into_json::<DeviceCodeResponse>()
        .map_err(|e| format!("device_code parse: {e}"))
}

enum PollError {
    SlowDown,
    Denied,
    Expired,
    Other(String),
}

fn poll_token(config: &Config, device_code: &str) -> Result<Option<TokenSet>, PollError> {
    let resp = match ureq::post(TOKEN_URL)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_form(&[
            ("client_id", &config.client_id),
            ("client_secret", &config.client_secret),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]) {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(PollError::Other(format!("token request: {e}"))),
    };
    let body: TokenResponse = resp
        .into_json()
        .map_err(|e| PollError::Other(format!("token response parse: {e}")))?;

    if let Some(err) = body.error.as_deref() {
        return match err {
            "authorization_pending" => Ok(None),
            "slow_down" => Err(PollError::SlowDown),
            "access_denied" => Err(PollError::Denied),
            "expired_token" => Err(PollError::Expired),
            other => Err(PollError::Other(format!("token error: {other}"))),
        };
    }

    let access_token = body
        .access_token
        .ok_or_else(|| PollError::Other("token response missing access_token".to_string()))?;
    let refresh_token = body
        .refresh_token
        .ok_or_else(|| PollError::Other("token response missing refresh_token".to_string()))?;
    let expires_in = body.expires_in.unwrap_or(3600);
    let scope = body.scope.unwrap_or_else(|| SCOPE.to_string());
    let token_type = body.token_type.unwrap_or_else(|| "Bearer".to_string());

    Ok(Some(TokenSet {
        access_token,
        refresh_token,
        expires_at_unix: now_unix() + expires_in,
        scope,
        token_type,
    }))
}

/// Returns a fresh `TokenSet` carrying the original refresh_token —
/// Google doesn't rotate it on refresh.
pub fn refresh(config: &Config, existing: &TokenSet) -> Result<TokenSet, String> {
    if config.is_minimal() {
        return Err("client_id / client_secret missing".to_string());
    }
    let resp = match ureq::post(TOKEN_URL)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_form(&[
            ("client_id", &config.client_id),
            ("client_secret", &config.client_secret),
            ("refresh_token", &existing.refresh_token),
            ("grant_type", "refresh_token"),
        ]) {
        Ok(r) => r,
        Err(ureq::Error::Status(status, r)) => {
            let body = r.into_string().unwrap_or_default();
            return Err(format!("refresh HTTP {status}: {body}"));
        }
        Err(e) => return Err(format!("refresh request: {e}")),
    };
    let body: TokenResponse = resp
        .into_json()
        .map_err(|e| format!("refresh parse: {e}"))?;
    if let Some(err) = body.error {
        return Err(format!(
            "refresh error: {err} ({})",
            body.error_description.unwrap_or_default()
        ));
    }
    let access_token = body
        .access_token
        .ok_or_else(|| "refresh response missing access_token".to_string())?;
    let expires_in = body.expires_in.unwrap_or(3600);
    Ok(TokenSet {
        access_token,
        refresh_token: existing.refresh_token.clone(),
        expires_at_unix: now_unix() + expires_in,
        scope: body.scope.unwrap_or_else(|| existing.scope.clone()),
        token_type: body
            .token_type
            .unwrap_or_else(|| existing.token_type.clone()),
    })
}

pub fn is_expired(tokens: &TokenSet) -> bool {
    let now = now_unix();
    tokens.expires_at_unix.saturating_sub(REFRESH_LEAD_SECS) <= now
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_expired_treats_recently_minted_token_as_fresh() {
        let t = TokenSet {
            access_token: "x".into(),
            refresh_token: "r".into(),
            expires_at_unix: now_unix() + 3600,
            scope: "s".into(),
            token_type: "Bearer".into(),
        };
        assert!(!is_expired(&t));
    }

    #[test]
    fn is_expired_returns_true_within_refresh_lead() {
        let t = TokenSet {
            access_token: "x".into(),
            refresh_token: "r".into(),
            expires_at_unix: now_unix() + 10, // less than REFRESH_LEAD_SECS
            scope: "s".into(),
            token_type: "Bearer".into(),
        };
        assert!(is_expired(&t));
    }

    #[test]
    fn is_expired_returns_true_for_past_token() {
        let t = TokenSet {
            access_token: "x".into(),
            refresh_token: "r".into(),
            expires_at_unix: 1,
            scope: "s".into(),
            token_type: "Bearer".into(),
        };
        assert!(is_expired(&t));
    }
}
