//! Web Push (RFC 8030) subscription storage + send pipeline for the
//! PWA notification path.
//!
//! VAPID key pair comes from env (`NESTTY_WEB_BRIDGE_VAPID_PRIVATE` /
//! `NESTTY_WEB_BRIDGE_VAPID_PUBLIC`, URL-safe base64 without padding —
//! the form that `web-push --gen-vapid` and the SPA both consume).
//! Subscriptions persist to `~/.config/nestty/web-bridge-push.jsonl`
//! (one JSON record per line), reloaded from disk on each mutation
//! since the list stays small. Push send uses the isahc client baked
//! into `web-push 0.11`.
//!
//! Why JSONL not SQLite: subscriptions are a flat append-mostly set,
//! the file is human-inspectable when debugging push delivery, and
//! pruning a 410 Gone endpoint is a whole-file rewrite (still cheap
//! at our scale — phone + maybe laptop = ≤5 entries).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use web_push::{
    ContentEncoding, IsahcWebPushClient, SubscriptionInfo, SubscriptionKeys, VapidSignatureBuilder,
    WebPushClient, WebPushError, WebPushMessageBuilder,
};

#[derive(Debug, Clone)]
pub struct PushConfig {
    /// VAPID private key (URL-safe base64, no padding).
    pub vapid_private_b64: String,
    /// VAPID public key (URL-safe base64, no padding) — exposed to the
    /// browser via `GET /api/push/vapid-public` so it can call
    /// `pushManager.subscribe({applicationServerKey: ...})`.
    pub vapid_public_b64: String,
    /// `sub` claim for the VAPID JWT — must be a mailto: or https URL.
    /// Browsers / push services reject signatures without a stable
    /// identifier; we default to a placeholder mailto when env is unset.
    pub vapid_subject: String,
}

impl PushConfig {
    /// Read VAPID config from env. Returns `None` when private/public
    /// keys aren't both set — caller treats push as disabled and
    /// returns 501 from the subscribe endpoint.
    pub fn from_env() -> Option<Self> {
        let priv_k = std::env::var("NESTTY_WEB_BRIDGE_VAPID_PRIVATE").ok()?;
        let pub_k = std::env::var("NESTTY_WEB_BRIDGE_VAPID_PUBLIC").ok()?;
        if priv_k.is_empty() || pub_k.is_empty() {
            return None;
        }
        let subject = std::env::var("NESTTY_WEB_BRIDGE_VAPID_SUBJECT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "mailto:nestty@localhost".to_string());
        Some(PushConfig {
            vapid_private_b64: priv_k,
            vapid_public_b64: pub_k,
            vapid_subject: subject,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    /// Deterministic id = `sha256(endpoint)` hex. Lets the SPA upsert
    /// without tracking the server-assigned id — re-POST the same
    /// browser subscription and we naturally dedupe.
    pub id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    /// Kind filter. Empty = receive all attention-queue kinds. Common
    /// values: "notification", "stop", "codex-turn".
    #[serde(default)]
    pub kinds: Vec<String>,
    pub created_at_ms: i64,
}

impl Subscription {
    pub fn matches_kind(&self, kind: &str) -> bool {
        self.kinds.is_empty() || self.kinds.iter().any(|k| k == kind)
    }
}

/// `sha256(endpoint)` as lowercase hex. Endpoint URLs are long + ugly
/// and contain `/` so they don't make good URL path segments. The
/// hash is stable across re-subscriptions of the same browser.
pub fn subscription_id_for(endpoint: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(endpoint.as_bytes());
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out.iter() {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn subscriptions_path() -> Option<PathBuf> {
    let config = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(config.join("nestty/web-bridge-push.jsonl"))
}

pub fn load_subscriptions() -> Vec<Subscription> {
    let Some(path) = subscriptions_path() else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    parse_subscriptions(&bytes)
}

fn parse_subscriptions(bytes: &[u8]) -> Vec<Subscription> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<Subscription>(line).ok())
        .collect()
}

/// Whole-file rewrite under the parent dir's natural lock granularity.
/// Multiple parallel writers would race, but the only writer here is
/// the (single) plugin process serialised via `AppState`'s tokio
/// Mutex, so a simple rewrite is fine. Creates parent dirs on demand.
pub fn save_subscriptions(subs: &[Subscription]) -> std::io::Result<()> {
    let Some(path) = subscriptions_path() else {
        return Err(std::io::Error::other("no home dir"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut buf = String::new();
    for s in subs {
        let line = serde_json::to_string(s).map_err(std::io::Error::other)?;
        buf.push_str(&line);
        buf.push('\n');
    }
    std::fs::write(&path, buf)
}

#[derive(Debug, Clone, Serialize)]
pub struct PushPayload<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub tag: &'a str,
    pub kind: &'a str,
    pub url: &'a str,
}

/// Send one notification. Surfaces the raw `WebPushError` so caller
/// can detect `EndpointNotValid` / `EndpointNotFound` (410 Gone /
/// 404) and prune the subscription on the next save cycle.
pub async fn send_to(
    config: &PushConfig,
    sub: &Subscription,
    payload: &PushPayload<'_>,
) -> Result<(), WebPushError> {
    let info = SubscriptionInfo {
        endpoint: sub.endpoint.clone(),
        keys: SubscriptionKeys {
            p256dh: sub.p256dh.clone(),
            auth: sub.auth.clone(),
        },
    };
    let mut sig = VapidSignatureBuilder::from_base64(&config.vapid_private_b64, &info)?;
    sig.add_claim("sub", config.vapid_subject.as_str());

    let body = serde_json::to_string(payload).map_err(|_| WebPushError::Unspecified)?;
    let mut builder = WebPushMessageBuilder::new(&info);
    builder.set_payload(ContentEncoding::Aes128Gcm, body.as_bytes());
    builder.set_vapid_signature(sig.build()?);
    let msg = builder.build()?;

    let client = IsahcWebPushClient::new().map_err(|_| WebPushError::Unspecified)?;
    client.send(msg).await
}

/// `true` for terminal endpoint states we should prune on. 410 Gone
/// from FCM / Mozilla means the user revoked the subscription; 404
/// Not Found likewise. Other errors are transient (network, server
/// overload) — keep the subscription, retry on next tick.
pub fn is_terminal_error(err: &WebPushError) -> bool {
    matches!(
        err,
        WebPushError::EndpointNotValid(_) | WebPushError::EndpointNotFound(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_id_is_deterministic() {
        let a = subscription_id_for("https://fcm.googleapis.com/foo");
        let b = subscription_id_for("https://fcm.googleapis.com/foo");
        let c = subscription_id_for("https://fcm.googleapis.com/bar");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64); // sha256 hex
    }

    #[test]
    fn matches_kind_empty_filter_accepts_all() {
        let s = Subscription {
            id: "x".into(),
            endpoint: "e".into(),
            p256dh: "p".into(),
            auth: "a".into(),
            kinds: vec![],
            created_at_ms: 0,
        };
        assert!(s.matches_kind("notification"));
        assert!(s.matches_kind("stop"));
        assert!(s.matches_kind("codex-turn"));
    }

    #[test]
    fn matches_kind_filter_selects() {
        let s = Subscription {
            id: "x".into(),
            endpoint: "e".into(),
            p256dh: "p".into(),
            auth: "a".into(),
            kinds: vec!["notification".into(), "stop".into()],
            created_at_ms: 0,
        };
        assert!(s.matches_kind("notification"));
        assert!(s.matches_kind("stop"));
        assert!(!s.matches_kind("codex-turn"));
    }

    #[test]
    fn parse_subscriptions_tolerates_blank_and_malformed() {
        let raw = b"{\"id\":\"i1\",\"endpoint\":\"e1\",\"p256dh\":\"p\",\"auth\":\"a\",\"kinds\":[],\"created_at_ms\":1}\nnot-json\n\n{\"id\":\"i2\",\"endpoint\":\"e2\",\"p256dh\":\"p\",\"auth\":\"a\",\"kinds\":[\"stop\"],\"created_at_ms\":2}\n";
        let subs = parse_subscriptions(raw);
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].id, "i1");
        assert_eq!(subs[1].id, "i2");
        assert_eq!(subs[1].kinds, vec!["stop".to_string()]);
    }

    #[test]
    fn push_config_from_env_requires_both_keys() {
        // Without env set, expect None.
        // (Tests don't share the env reliably across runners — keep
        // this as a smoke test for the None-path. The Some path is
        // covered via runtime e2e since it requires real keys.)
        unsafe {
            std::env::remove_var("NESTTY_WEB_BRIDGE_VAPID_PRIVATE");
            std::env::remove_var("NESTTY_WEB_BRIDGE_VAPID_PUBLIC");
        }
        assert!(PushConfig::from_env().is_none());
    }
}
