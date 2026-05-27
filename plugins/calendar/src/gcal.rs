//! Google Calendar v3 read-only client.
//!
//! Calls `events.list` on the primary calendar with `singleEvents=true`
//! so recurring events are pre-expanded into per-instance entries —
//! that's what triggers want (each instance has its own start time +
//! per-instance attendee responses, and the `recurringEventId` field
//! lets users target a recurrence series in their `[[triggers]]` rules).
//!
//! Token refresh: on a 401 we trigger a refresh-token exchange via
//! the `oauth` module, persist the new `TokenSet` through the
//! `TokenStore`, and retry the request once. A second 401 is fatal
//! (we don't loop indefinitely if the refresh_token itself was
//! revoked — caller must re-run `copad-plugin-calendar auth`).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::config::Config;
use crate::event::{CalendarEvent, from_gcal_json};
use crate::oauth;
use crate::store::{TokenSet, TokenStore};

const API_BASE: &str = "https://www.googleapis.com/calendar/v3";

pub struct Client {
    config: Config,
    store: Arc<dyn TokenStore>,
    tokens: TokenSet,
}

impl Client {
    pub fn new(config: Config, store: Arc<dyn TokenStore>) -> Result<Self, String> {
        let tokens = store.load().ok_or_else(|| {
            "no stored credentials — run `copad-plugin-calendar auth`".to_string()
        })?;
        Ok(Self {
            config,
            store,
            tokens,
        })
    }

    /// Walks `nextPageToken` (capped at `MAX_PAGES`) so a busy window
    /// doesn't silently lose events past page 1.
    pub fn list_events(
        &mut self,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>, String> {
        const MAX_PAGES: usize = 20; // 20 * 250 = 5000 events; well past any realistic 24h window
        self.ensure_fresh_token()?;
        let base = format!(
            "{API_BASE}/calendars/primary/events?timeMin={}&timeMax={}&singleEvents=true&orderBy=startTime&maxResults=250",
            urlencode(&time_min.to_rfc3339()),
            urlencode(&time_max.to_rfc3339()),
        );
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;
        for page in 0..MAX_PAGES {
            let url = match &page_token {
                Some(t) => format!("{base}&pageToken={}", urlencode(t)),
                None => base.clone(),
            };
            let body = self.get_with_retry(&url)?;
            let items = body
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(|| "list_events response missing 'items'".to_string())?;
            for item in items {
                if let Some(e) = from_gcal_json(item) {
                    all.push(e);
                }
            }
            page_token = body
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string);
            if page_token.is_none() {
                return Ok(all);
            }
            if page + 1 == MAX_PAGES {
                eprintln!(
                    "[calendar] list_events truncated after {MAX_PAGES} pages; \
                     consider narrowing COPAD_CALENDAR_LOOKAHEAD_HOURS"
                );
            }
        }
        Ok(all)
    }

    /// Look up a single event by id on the primary calendar.
    /// Returns `Ok(None)` when the API responds 404.
    pub fn get_event(&mut self, event_id: &str) -> Result<Option<CalendarEvent>, String> {
        self.ensure_fresh_token()?;
        let url = format!(
            "{API_BASE}/calendars/primary/events/{}",
            urlencode(event_id),
        );
        match self.get_raw(&url) {
            Ok(body) => Ok(from_gcal_json(&body)),
            Err(GcalError::Status(404, _)) => Ok(None),
            Err(GcalError::Status(401, _)) => {
                // Refresh + retry once.
                self.force_refresh()?;
                match self.get_raw(&url) {
                    Ok(body) => Ok(from_gcal_json(&body)),
                    Err(GcalError::Status(404, _)) => Ok(None),
                    Err(e) => Err(e.to_string()),
                }
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn ensure_fresh_token(&mut self) -> Result<(), String> {
        if oauth::is_expired(&self.tokens) {
            self.force_refresh()?;
        }
        Ok(())
    }

    fn force_refresh(&mut self) -> Result<(), String> {
        let new_tokens = oauth::refresh(&self.config, &self.tokens)?;
        self.store.save(&new_tokens)?;
        self.tokens = new_tokens;
        Ok(())
    }

    fn get_with_retry(&mut self, url: &str) -> Result<Value, String> {
        match self.get_raw(url) {
            Ok(body) => Ok(body),
            Err(GcalError::Status(401, _)) => {
                self.force_refresh()?;
                self.get_raw(url).map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn get_raw(&self, url: &str) -> Result<Value, GcalError> {
        let resp = match ureq::get(url)
            .set(
                "Authorization",
                &format!("Bearer {}", self.tokens.access_token),
            )
            .set("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(15))
            .call()
        {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(GcalError::Status(code, body));
            }
            Err(e) => return Err(GcalError::Transport(e.to_string())),
        };
        resp.into_json::<Value>()
            .map_err(|e| GcalError::Transport(format!("json parse: {e}")))
    }
}

enum GcalError {
    Status(u16, String),
    Transport(String),
}

impl std::fmt::Display for GcalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GcalError::Status(code, body) => write!(f, "HTTP {code}: {body}"),
            GcalError::Transport(s) => write!(f, "{s}"),
        }
    }
}

/// Minimal RFC 3986 percent-encode for path/query segments.
fn urlencode(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_passes_unreserved() {
        assert_eq!(urlencode("abcXYZ-_.~012"), "abcXYZ-_.~012");
    }

    #[test]
    fn urlencode_escapes_reserved_and_unicode() {
        assert_eq!(urlencode(":+ "), "%3A%2B%20");
        assert_eq!(
            urlencode("2026-04-26T10:00:00+09:00"),
            "2026-04-26T10%3A00%3A00%2B09%3A00"
        );
        // Non-ASCII byte sequences encode each byte.
        assert_eq!(urlencode("é"), "%C3%A9");
    }
}
