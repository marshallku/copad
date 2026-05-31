//! Bot-RPC: "post a message, wait for a matching response, fire an
//! event" runtime.
//!
//! - `BotRpcRegistry` tracks live `PendingWait`s and ingests message
//!   events through `ingest_message`. A successful match removes the
//!   wait and produces a `MatchedResponse` the caller publishes as
//!   `slack.bot_rpc.completed`.
//! - `RecentMessageBuffer` (one bounded FIFO per channel) closes the
//!   race where Slack delivers the bot's response over Socket Mode
//!   *before* `chat.postMessage` returns with our `request_ts`. When
//!   `add()` registers a fresh wait, the recent buffer is replayed
//!   through the same `match_event` path so the wait gets one chance
//!   to consume a message that arrived during the HTTP round-trip.
//! - `sweep_expired` removes deadlined waits; a background thread
//!   (`start_sweeper`) ticks ~4 times a second and emits
//!   `slack.bot_rpc.timeout` for each one.
//!
//! Correlation contract (matches the codex round-2 review):
//! - `channel` must equal the wait channel.
//! - Response `(ts)` must be strictly after the wait's `request_ts`
//!   (compared as nanosecond tuples — Slack's `<sec>.<frac>` format).
//! - Author `event.user` must NOT equal the bot's own user id
//!   (resolved at invoke time; required, never `None`).
//! - `wait_user_filter`, when set, compared to `event.user` only —
//!   never to `bot_id`.
//! - `wait_in_thread == true` (legacy default is `false` to keep
//!   existing saved configs from silently breaking) requires
//!   `event.thread_ts == request_ts`.
//! - `WaitMode::Regex` requires `Regex::captures` to succeed; named
//!   groups land in the published payload.
//!
//! Subtype filter for the matcher: accept `subtype` absent or
//! `"bot_message"` (Jira-style bots reply with `bot_message`).
//! Everything else (`message_changed`, `message_deleted`, joins) is
//! ignored.

use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};

use crate::channels::{BotRpcConfig, WaitMode};
use crate::collect::now_ms;

const RECENT_BUFFER_PER_CHANNEL: usize = 256;
const RECENT_BUFFER_WINDOW_MS: u64 = 30_000;
const SWEEPER_TICK: Duration = Duration::from_millis(250);

/// Snapshot of an incoming Slack message, post-filter. `text` may be
/// empty (a `bot_message` with only blocks/attachments). Held in the
/// recent buffer and used by `match_event`.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub channel: String,
    pub ts: String,
    pub user: String,
    pub text: String,
    pub thread_ts: Option<String>,
    pub event_id: Option<String>,
    pub team_id: Option<String>,
    /// Wall-clock at the moment the event arrived. Drives the recent
    /// buffer's 30s window — `ts` is Slack's own clock which can skew.
    pub received_at_ms: u64,
}

#[derive(Debug, Clone)]
pub struct PendingWait {
    pub request_ts: String,
    pub channel: String,
    pub config: BotRpcConfig,
    pub deadline_ms: u64,
    /// REQUIRED — never None. invoke must resolve this via auth.test
    /// before constructing a wait so the matcher can reliably reject
    /// the bot's own echo.
    pub bot_user_id: String,
    /// Pre-compiled regex (Regex mode only). Re-compiled on invoke
    /// rather than trusted from persisted state.
    pub compiled_regex: Option<Regex>,
}

#[derive(Debug, Clone)]
pub struct MatchedResponse {
    pub channel: String,
    pub request_ts: String,
    pub response_ts: String,
    pub user: String,
    pub text: String,
    pub thread_ts: Option<String>,
    pub event_id: Option<String>,
    pub team_id: Option<String>,
    pub captures: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ExpiredWait {
    pub channel: String,
    pub request_ts: String,
}

struct Inner {
    waits: Vec<PendingWait>,
    /// per-channel ring buffer keyed by channel id. Cap is
    /// `RECENT_BUFFER_PER_CHANNEL`; older entries are dropped.
    recent: BTreeMap<String, VecDeque<IncomingMessage>>,
}

pub struct BotRpcRegistry {
    inner: Arc<Mutex<Inner>>,
}

impl BotRpcRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                waits: Vec::new(),
                recent: BTreeMap::new(),
            })),
        }
    }

    /// Register a fresh wait AND replay any messages we've already
    /// seen on this channel in the last 30s. Returns the resulting
    /// match if a buffered event satisfied the wait. The caller
    /// publishes the match exactly the same way as a real-time hit.
    pub fn add(&self, wait: PendingWait) -> Option<MatchedResponse> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let recent_for_channel = inner.recent.get(&wait.channel).cloned();
        inner.waits.push(wait);
        let added_idx = inner.waits.len() - 1;
        if let Some(buf) = recent_for_channel {
            for msg in buf.iter() {
                if let Some(matched) = try_match(&inner.waits[added_idx], msg) {
                    inner.waits.swap_remove(added_idx);
                    return Some(matched);
                }
            }
        }
        None
    }

    /// Ingest one incoming message. Pushes into the recent buffer first
    /// (so `add`'s replay always sees this message if it arrived just
    /// before invoke), then scans pending waits for a match. Returns
    /// `Some(matched)` on the FIRST matching wait, removing that wait.
    pub fn ingest_message(&self, msg: IncomingMessage) -> Option<MatchedResponse> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        push_to_recent(&mut inner.recent, &msg);
        let mut hit: Option<usize> = None;
        let mut matched: Option<MatchedResponse> = None;
        for (idx, wait) in inner.waits.iter().enumerate() {
            if let Some(m) = try_match(wait, &msg) {
                hit = Some(idx);
                matched = Some(m);
                break;
            }
        }
        if let Some(idx) = hit {
            inner.waits.swap_remove(idx);
        }
        matched
    }

    /// Remove any waits whose deadline has passed. Returns the removed
    /// waits so the caller can publish `slack.bot_rpc.timeout` for each.
    pub fn sweep_expired(&self, now: u64) -> Vec<ExpiredWait> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut expired = Vec::new();
        let mut idx = 0;
        while idx < inner.waits.len() {
            if inner.waits[idx].deadline_ms <= now {
                let w = inner.waits.swap_remove(idx);
                expired.push(ExpiredWait {
                    channel: w.channel,
                    request_ts: w.request_ts,
                });
            } else {
                idx += 1;
            }
        }
        expired
    }

    #[cfg(test)]
    pub fn len_for_test(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.waits.len()
    }
}

impl Default for BotRpcRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn push_to_recent(map: &mut BTreeMap<String, VecDeque<IncomingMessage>>, msg: &IncomingMessage) {
    let buf = map
        .entry(msg.channel.clone())
        .or_insert_with(|| VecDeque::with_capacity(RECENT_BUFFER_PER_CHANNEL));
    let now = msg.received_at_ms;
    while let Some(front) = buf.front() {
        if now.saturating_sub(front.received_at_ms) > RECENT_BUFFER_WINDOW_MS {
            buf.pop_front();
        } else {
            break;
        }
    }
    if buf.len() == RECENT_BUFFER_PER_CHANNEL {
        buf.pop_front();
    }
    buf.push_back(msg.clone());
}

/// All correlation gates in one place. Returns `Some(MatchedResponse)`
/// when the message satisfies the wait; `None` otherwise.
fn try_match(wait: &PendingWait, msg: &IncomingMessage) -> Option<MatchedResponse> {
    if msg.channel != wait.channel {
        return None;
    }
    // Strict ordering — reject the bot's own posted request (which
    // Slack echoes back) and any pre-existing channel chatter.
    let req = parse_slack_ts(&wait.request_ts)?;
    let resp = parse_slack_ts(&msg.ts)?;
    if resp <= req {
        return None;
    }
    if msg.user == wait.bot_user_id {
        return None;
    }
    if !wait.config.wait_user_filter.is_empty() && msg.user != wait.config.wait_user_filter {
        return None;
    }
    if wait.config.wait_in_thread {
        match &msg.thread_ts {
            Some(tt) if tt == &wait.request_ts => {}
            _ => return None,
        }
    }
    let captures = match wait.config.wait_mode {
        WaitMode::FirstReply => BTreeMap::new(),
        WaitMode::Regex => {
            let re = wait.compiled_regex.as_ref()?;
            let caps = re.captures(&msg.text)?;
            extract_named_captures(re, &caps)
        }
    };
    Some(MatchedResponse {
        channel: msg.channel.clone(),
        request_ts: wait.request_ts.clone(),
        response_ts: msg.ts.clone(),
        user: msg.user.clone(),
        text: msg.text.clone(),
        thread_ts: msg.thread_ts.clone(),
        event_id: msg.event_id.clone(),
        team_id: msg.team_id.clone(),
        captures,
    })
}

fn extract_named_captures(re: &Regex, caps: &regex::Captures<'_>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for name in re.capture_names().flatten() {
        if let Some(m) = caps.name(name) {
            out.insert(name.to_string(), m.as_str().to_string());
        }
    }
    out
}

/// Slack ts is `<sec>.<frac>`. Pad the fractional part to 9 digits
/// (nanoseconds) so `(secs, nanos)` tuple ordering is correct for
/// variable-width input — `"5.1"` and `"5.05"` no longer collide.
pub fn parse_slack_ts(s: &str) -> Option<(u64, u64)> {
    let (sec, frac) = s.split_once('.')?;
    if sec.is_empty() || frac.is_empty() {
        return None;
    }
    if !sec.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs: u64 = sec.parse().ok()?;
    // Right-pad fractional to 9 chars so it always represents nanos.
    // Strings longer than 9 are accepted but truncated — Slack docs
    // only specify microseconds (6) so 9-truncation is safe and
    // monotonic by construction.
    let padded: String = frac.chars().chain(std::iter::repeat('0')).take(9).collect();
    let nanos: u64 = padded.parse().ok()?;
    Some((secs, nanos))
}

/// Wait classifier exists so the matcher and unit tests share one
/// rule. Subtypes outside the allowlist short-circuit before the
/// recent buffer is touched.
pub fn is_matchable_subtype(subtype: Option<&str>) -> bool {
    matches!(subtype, None | Some("bot_message"))
}

/// Compile a `BotRpcConfig`'s regex at invoke time. `WaitMode::FirstReply`
/// returns `None`. Empty regex strings under `WaitMode::Regex` would
/// have been rejected by `validate_entry`, but we still handle them as
/// `None` for defense in depth.
pub fn compile_regex(cfg: &BotRpcConfig) -> Result<Option<Regex>, String> {
    match cfg.wait_mode {
        WaitMode::FirstReply => Ok(None),
        WaitMode::Regex => {
            if cfg.wait_regex.is_empty() {
                return Err("wait_mode=regex requires non-empty wait_regex".to_string());
            }
            Regex::new(&cfg.wait_regex)
                .map(Some)
                .map_err(|e| format!("wait_regex compile error: {e}"))
        }
    }
}

/// Spawns the background sweeper. Calls `on_expired` for every
/// timed-out wait on each tick. `stop` flips to true on plugin
/// shutdown — the thread exits within `SWEEPER_TICK` after that.
pub fn start_sweeper<F>(
    registry: Arc<BotRpcRegistry>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    mut on_expired: F,
) -> std::thread::JoinHandle<()>
where
    F: FnMut(ExpiredWait) + Send + 'static,
{
    std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let now = now_ms();
            for expired in registry.sweep_expired(now) {
                on_expired(expired);
            }
            std::thread::sleep(SWEEPER_TICK);
        }
    })
}

/// Helper used by main.rs to format the `slack.bot_rpc.completed`
/// payload. Public so a unit test can verify the JSON shape.
pub fn completed_payload(matched: &MatchedResponse) -> Value {
    json!({
        "channel": matched.channel,
        "request_ts": matched.request_ts,
        "response_ts": matched.response_ts,
        "user": matched.user,
        "text": matched.text,
        "thread_ts": matched.thread_ts,
        "event_id": matched.event_id,
        "team_id": matched.team_id,
        "captures": serde_json::to_value(&matched.captures).unwrap_or(Value::Null),
    })
}

pub fn timeout_payload(expired: &ExpiredWait) -> Value {
    json!({
        "channel": expired.channel,
        "request_ts": expired.request_ts,
    })
}

/// Convenience for the writer thread — frames the `event.publish`
/// envelope around an arbitrary payload kind. Used for both completed
/// and timeout events.
pub fn publish_frame(kind: &str, payload: Value) -> String {
    json!({
        "method": "event.publish",
        "params": {
            "kind": kind,
            "payload": payload,
        }
    })
    .to_string()
}

/// Dispatch a completed/timeout event onto the shared writer channel.
/// Errors silently — the channel is closed only at shutdown.
pub fn dispatch_completed(tx: &Sender<String>, matched: &MatchedResponse) {
    let _ = tx.send(publish_frame(
        "slack.bot_rpc.completed",
        completed_payload(matched),
    ));
}

pub fn dispatch_timeout(tx: &Sender<String>, expired: &ExpiredWait) {
    let _ = tx.send(publish_frame(
        "slack.bot_rpc.timeout",
        timeout_payload(expired),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: WaitMode, regex: &str, user_filter: &str, in_thread: bool) -> BotRpcConfig {
        BotRpcConfig {
            default_template: "/jira CHA-1".into(),
            wait_mode: mode,
            wait_regex: regex.into(),
            wait_user_filter: user_filter.into(),
            wait_timeout_ms: 30_000,
            wait_in_thread: in_thread,
        }
    }

    fn wait(channel: &str, request_ts: &str, cfg: BotRpcConfig) -> PendingWait {
        let compiled_regex = compile_regex(&cfg).unwrap();
        PendingWait {
            request_ts: request_ts.into(),
            channel: channel.into(),
            config: cfg,
            deadline_ms: now_ms() + 30_000,
            bot_user_id: "U_BOT".into(),
            compiled_regex,
        }
    }

    fn msg(
        channel: &str,
        ts: &str,
        user: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> IncomingMessage {
        IncomingMessage {
            channel: channel.into(),
            ts: ts.into(),
            user: user.into(),
            text: text.into(),
            thread_ts: thread_ts.map(str::to_string),
            event_id: Some("Ev0".into()),
            team_id: Some("T0".into()),
            received_at_ms: now_ms(),
        }
    }

    #[test]
    fn first_reply_matches_next_message_in_channel() {
        let reg = BotRpcRegistry::new();
        reg.add(wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::FirstReply, "", "", false),
        ));
        let matched = reg.ingest_message(msg(
            "C123",
            "1700000000.000200",
            "U_JIRA",
            "CHA-1 created",
            None,
        ));
        let m = matched.expect("first reply should match");
        assert_eq!(m.user, "U_JIRA");
        assert_eq!(m.response_ts, "1700000000.000200");
        assert_eq!(reg.len_for_test(), 0, "wait removed on match");
    }

    #[test]
    fn rejects_self_echo() {
        let reg = BotRpcRegistry::new();
        reg.add(wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::FirstReply, "", "", false),
        ));
        // Slack echoes the bot's own postMessage — user == bot_user_id.
        let matched = reg.ingest_message(msg("C123", "1700000000.000200", "U_BOT", "echo", None));
        assert!(matched.is_none());
        assert_eq!(reg.len_for_test(), 1, "wait still pending");
    }

    #[test]
    fn rejects_pre_request_ts() {
        let reg = BotRpcRegistry::new();
        reg.add(wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::FirstReply, "", "", false),
        ));
        // Old chatter from before the request — must not match.
        let matched = reg.ingest_message(msg("C123", "1700000000.000050", "U_JIRA", "old", None));
        assert!(matched.is_none());
    }

    #[test]
    fn rejects_other_channel() {
        let reg = BotRpcRegistry::new();
        reg.add(wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::FirstReply, "", "", false),
        ));
        let matched = reg.ingest_message(msg("C999", "1700000000.000200", "U_JIRA", "wrong", None));
        assert!(matched.is_none());
    }

    #[test]
    fn enforces_user_filter() {
        let reg = BotRpcRegistry::new();
        reg.add(wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::FirstReply, "", "U_JIRA", false),
        ));
        assert!(
            reg.ingest_message(msg("C123", "1700000000.000200", "U_OTHER", "noise", None))
                .is_none()
        );
        assert!(
            reg.ingest_message(msg("C123", "1700000000.000300", "U_JIRA", "yes", None))
                .is_some()
        );
    }

    #[test]
    fn regex_mode_extracts_named_captures() {
        let reg = BotRpcRegistry::new();
        reg.add(wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::Regex, r"^(?<ticket>[A-Z]+-\d+)\b", "", false),
        ));
        let m = reg
            .ingest_message(msg(
                "C123",
                "1700000000.000200",
                "U_JIRA",
                "CHA-42 created",
                None,
            ))
            .expect("regex match");
        assert_eq!(m.captures.get("ticket").unwrap(), "CHA-42");
    }

    #[test]
    fn regex_mode_rejects_non_match() {
        let reg = BotRpcRegistry::new();
        reg.add(wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::Regex, r"^OK\b", "", false),
        ));
        let m = reg.ingest_message(msg("C123", "1700000000.000200", "U_JIRA", "nope", None));
        assert!(m.is_none());
        // Wait must remain — a non-match shouldn't consume.
        assert_eq!(reg.len_for_test(), 1);
    }

    #[test]
    fn wait_in_thread_requires_thread_ts_match() {
        let reg = BotRpcRegistry::new();
        reg.add(wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::FirstReply, "", "", true),
        ));
        // No thread_ts → no match.
        assert!(
            reg.ingest_message(msg(
                "C123",
                "1700000000.000200",
                "U_JIRA",
                "channel msg",
                None
            ))
            .is_none()
        );
        // Wrong thread → no match.
        assert!(
            reg.ingest_message(msg(
                "C123",
                "1700000000.000300",
                "U_JIRA",
                "other thread",
                Some("1700000000.999999")
            ))
            .is_none()
        );
        // Matching thread → match.
        let m = reg
            .ingest_message(msg(
                "C123",
                "1700000000.000400",
                "U_JIRA",
                "in thread",
                Some("1700000000.000100"),
            ))
            .expect("thread match");
        assert_eq!(m.thread_ts.as_deref(), Some("1700000000.000100"));
    }

    #[test]
    fn replay_recent_catches_message_that_arrived_before_add() {
        // Codex round-1 PB1: Slack delivers the response over Socket
        // Mode before chat.postMessage's HTTP call returns. We push
        // the message first, THEN add the wait — replay must catch it.
        let reg = BotRpcRegistry::new();
        let arrived = msg("C123", "1700000000.000200", "U_JIRA", "early bird", None);
        assert!(
            reg.ingest_message(arrived).is_none(),
            "no waits yet → no match"
        );
        let matched = reg.add(wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::FirstReply, "", "", false),
        ));
        let m = matched.expect("recent buffer should have replayed");
        assert_eq!(m.response_ts, "1700000000.000200");
        assert_eq!(reg.len_for_test(), 0, "wait removed on replay match");
    }

    #[test]
    fn recent_buffer_window_evicts_old_entries() {
        // Simulate a stale message older than the 30s window — must
        // not survive into replay.
        let reg = BotRpcRegistry::new();
        let stale = IncomingMessage {
            channel: "C123".into(),
            ts: "1700000000.000200".into(),
            user: "U_JIRA".into(),
            text: "ancient".into(),
            thread_ts: None,
            event_id: None,
            team_id: None,
            received_at_ms: now_ms().saturating_sub(60_000),
        };
        reg.ingest_message(stale);
        // Now ingest something fresh to trigger the window cleanup.
        let _ = reg.ingest_message(msg("C123", "1700000000.000300", "U_OTHER", "fresh", None));
        // Add a wait whose request_ts is between stale and fresh —
        // stale should have been evicted, so no replay match.
        let matched = reg.add(wait(
            "C123",
            "1700000000.000250",
            cfg(WaitMode::FirstReply, "", "U_GONE", false),
        ));
        assert!(matched.is_none());
    }

    #[test]
    fn sweep_expired_removes_and_returns_dead_waits() {
        let reg = BotRpcRegistry::new();
        let mut w = wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::FirstReply, "", "", false),
        );
        w.deadline_ms = 1; // long expired
        reg.add(w);
        let expired = reg.sweep_expired(now_ms());
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].channel, "C123");
        assert_eq!(reg.len_for_test(), 0);
    }

    #[test]
    fn parse_slack_ts_orders_variable_width_fractions() {
        let a = parse_slack_ts("1700000000.1").unwrap();
        let b = parse_slack_ts("1700000000.05").unwrap();
        assert!(a > b, "0.1 > 0.05");
        let c = parse_slack_ts("1700000000.000100").unwrap();
        let d = parse_slack_ts("1700000000.000200").unwrap();
        assert!(d > c, "monotonic across canonical 6-digit µs");
    }

    #[test]
    fn parse_slack_ts_rejects_garbage() {
        assert!(parse_slack_ts("not.a.ts").is_none());
        assert!(parse_slack_ts("1700000000").is_none());
        assert!(parse_slack_ts(".000100").is_none());
        assert!(parse_slack_ts("1700000000.").is_none());
        assert!(parse_slack_ts("1700000000.abc").is_none());
    }

    #[test]
    fn matchable_subtype_allows_bot_message_rejects_edits() {
        assert!(is_matchable_subtype(None));
        assert!(is_matchable_subtype(Some("bot_message")));
        assert!(!is_matchable_subtype(Some("message_changed")));
        assert!(!is_matchable_subtype(Some("message_deleted")));
        assert!(!is_matchable_subtype(Some("channel_join")));
    }

    #[test]
    fn two_waits_same_channel_distinguished_by_request_ts() {
        let reg = BotRpcRegistry::new();
        reg.add(wait(
            "C123",
            "1700000000.000100",
            cfg(WaitMode::Regex, r"^A:", "", false),
        ));
        reg.add(wait(
            "C123",
            "1700000000.000110",
            cfg(WaitMode::Regex, r"^B:", "", false),
        ));
        // Only the B-wait should consume this message.
        let m = reg
            .ingest_message(msg("C123", "1700000000.000200", "U_OTHER", "B: done", None))
            .unwrap();
        assert_eq!(m.request_ts, "1700000000.000110");
        assert_eq!(reg.len_for_test(), 1, "A-wait still pending");
    }

    #[test]
    fn completed_payload_includes_thread_event_team() {
        let matched = MatchedResponse {
            channel: "C".into(),
            request_ts: "1.1".into(),
            response_ts: "2.2".into(),
            user: "U".into(),
            text: "t".into(),
            thread_ts: Some("1.1".into()),
            event_id: Some("E".into()),
            team_id: Some("T".into()),
            captures: BTreeMap::new(),
        };
        let v = completed_payload(&matched);
        assert_eq!(v["thread_ts"], "1.1");
        assert_eq!(v["event_id"], "E");
        assert_eq!(v["team_id"], "T");
        assert_eq!(v["captures"], json!({}));
    }

    #[test]
    fn compile_regex_errors_on_invalid_pattern() {
        let c = cfg(WaitMode::Regex, "[unterminated", "", false);
        assert!(compile_regex(&c).is_err());
    }

    #[test]
    fn compile_regex_returns_none_for_first_reply() {
        let c = cfg(WaitMode::FirstReply, "", "", false);
        assert!(compile_regex(&c).unwrap().is_none());
    }

    #[test]
    fn recent_buffer_caps_at_per_channel_limit() {
        // Stress the FIFO: push more than capacity, then add a wait
        // whose replay window covers only the most recent entry.
        let reg = BotRpcRegistry::new();
        for i in 0..(RECENT_BUFFER_PER_CHANNEL + 50) {
            let m = msg(
                "C123",
                &format!("1700000000.{:06}", i),
                "U_OTHER",
                &format!("m-{i}"),
                None,
            );
            reg.ingest_message(m);
        }
        // request_ts older than every buffered ts — replay should
        // pick the OLDEST surviving entry (FIFO order).
        let matched = reg.add(wait(
            "C123",
            "1700000000.000000",
            cfg(WaitMode::FirstReply, "", "", false),
        ));
        let m = matched.unwrap();
        // FIFO eviction means index `50` is the oldest surviving
        // entry (50 oldest evicted out of 50 over-cap).
        assert_eq!(m.response_ts, format!("1700000000.{:06}", 50));
    }
}
