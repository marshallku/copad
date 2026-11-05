//! Background poller that publishes the four `jira.*` event kinds.
//!
//! Loop: wait for the supervisor's `initialized`, run the first tick IMMEDIATELY, then sleep
//! `poll_interval` between ticks (same posture as calendar's poller). A tick is skipped silently
//! when `fatal_error` is set or no credentials are usable, so the user can fix env or run
//! `copad-plugin-jira auth` while copad is running and the next tick picks it up.
//!
//! Each tick fetches pages via `jira::search` (ORDER BY updated DESC, JQL
//! `(assignee = currentUser() OR watcher = currentUser()) AND updated > -<lookback>h[ AND project
//! in (X, Y)]`), following `next_page_token` up to MAX_PAGES, and diffs each ticket against the
//! prior `TicketSnapshot`:
//! - First sight: `jira.ticket_assigned` iff assigned to me now; no `status_changed` (no `from`).
//! - Later: assignee changed -> `jira.ticket_assigned` iff newly mine; status changed ->
//!   `jira.status_changed`; each inline `fields.comment.comments[]` entry whose parsed `created`
//!   is strictly newer than the per-ticket watermark -> `jira.comment_added`, plus `jira.mention`
//!   when the body mentions me.
//!
//! Comment events are deduped by id via `fired_comments` (cap 4096; flushed past it).
//! All errors are logged to stderr and swallowed; one bad tick must not kill the daemon.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Config;
use crate::event::{
    self, Comment, Ticket, TicketSnapshot, comment_to_json, extract_inline_comments,
    from_jira_json, to_payload_json,
};
use crate::jira;
use crate::store::TokenStore;

const PAGE_SIZE: u64 = 100;
const MAX_PAGES: u64 = 20; // 20 * 100 = 2000 issues; well past any single user's load
const DEDUPE_CAP: usize = 4096;

pub struct Poller {
    config: Arc<Config>,
    store: Arc<dyn TokenStore>,
    tx: Sender<String>,
    initialized: Arc<AtomicBool>,
    state: Mutex<PollerState>,
}

#[derive(Default)]
struct PollerState {
    /// Cached across ticks. `my_identity_signature` records the (base_url, email) the cache was
    /// built for; a hot re-auth to a different Atlassian account invalidates it so
    /// assigned-to-me / mention detection doesn't mis-attribute to the prior user.
    my_account_id: Option<String>,
    my_identity_signature: Option<(String, String)>,
    /// Sole guard against cross-tick re-emission: each `process_ticket` updates the snapshot to
    /// current, so the next tick (or a duplicate later in the same paginated response) emits
    /// nothing unless something changed Jira-side.
    snapshots: HashMap<String, TicketSnapshot>,
    /// Per-comment dedupe (cap 4096; flush past cap). Snapshot mutation alone isn't enough:
    /// backfilling comments across pages can see the same comment id twice if a fresh one lands
    /// mid-paginate. Status/assignee transitions are NOT deduped: a genuine
    /// `Open -> In Progress -> Open -> In Progress` bounce must emit each.
    fired_comments: HashSet<String>,
}

impl Poller {
    pub fn new(
        config: Arc<Config>,
        store: Arc<dyn TokenStore>,
        tx: Sender<String>,
        initialized: Arc<AtomicBool>,
    ) -> Self {
        Self {
            config,
            store,
            tx,
            initialized,
            state: Mutex::new(PollerState::default()),
        }
    }

    pub fn run(&self) {
        // Wait for `initialized` before any poll cycle, or an event published during init would
        // race the supervisor's handshake.
        while !self.initialized.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
        if let Err(e) = self.tick() {
            eprintln!("[jira] initial poll tick failed: {e}");
        }
        loop {
            thread::sleep(self.config.poll_interval);
            if let Err(e) = self.tick() {
                eprintln!("[jira] poll tick failed: {e}");
            }
        }
    }

    fn tick(&self) -> Result<(), String> {
        if self.config.fatal_error.is_some() {
            return Ok(()); // bad config — silent skip
        }
        let creds = match crate::current_credentials(&self.config, &*self.store) {
            Some(c) => c,
            None => return Ok(()), // not authenticated yet — silent skip
        };
        let creds_borrow = jira::Creds {
            base_url: &creds.base_url,
            email: &creds.email,
            api_token: &creds.api_token,
        };
        // Pass the account_id_hint atomically with the creds: re-reading self.store in
        // `resolve_my_account_id` could pair live env creds with a stale store account_id.
        let my_account_id = self.resolve_my_account_id(creds_borrow, &creds.account_id_hint)?;

        let jql =
            jira::build_polling_jql(self.config.lookback_hours, self.config.projects.as_deref());
        // `*all` returns every field including customs, so `event_json` in emitted events carries
        // the verbatim payload we promise (triggers reach
        // `event.event_json.fields.customfield_10001` etc.), at ~5-10x the response size of an
        // explicit field list.
        let fields = ["*all"];
        let (tickets, truncated) = self.fetch_all_tickets(creds_borrow, &jql, &fields)?;
        eprintln!(
            "[jira] poll tick OK ({} tickets in lookback window)",
            tickets.len()
        );

        // One state-lock scope so a concurrent shutdown can't see a half-applied snapshot diff.
        let mut state = self.state.lock().unwrap();
        // Prune snapshots for tickets that left the lookback window. Otherwise a ticket inactive
        // for > lookback_hours that later updates would diff against months-old state: a bogus
        // `jira.status_changed`, every comment since replayed as `jira.comment_added` /
        // `jira.mention`. Re-entry as first sight suppresses status/comment events (no baseline)
        // but still emits `jira.ticket_assigned` if it's mine. Skipped on truncation (MAX_PAGES
        // hit): without the full picture, pruning would drop legitimate state.
        if !truncated {
            let observed: HashSet<String> = tickets.iter().map(|t| t.key.clone()).collect();
            state.snapshots.retain(|key, _| observed.contains(key));
        }
        for ticket in &tickets {
            self.process_ticket(creds_borrow, ticket, &my_account_id, &mut state);
        }
        self.gc_dedupe_set(&mut state);
        Ok(())
    }

    /// Returns `(tickets, truncated)`. `truncated == true` means MAX_PAGES stopped us short, so
    /// the caller skips snapshot pruning to avoid dropping state for tickets outside the subset.
    fn fetch_all_tickets(
        &self,
        creds: jira::Creds,
        jql: &str,
        fields: &[&str],
    ) -> Result<(Vec<Ticket>, bool), String> {
        let mut all = Vec::new();
        let mut next_page_token: Option<String> = None;
        for page in 0..MAX_PAGES {
            let resp = jira::search(creds, jql, next_page_token.as_deref(), PAGE_SIZE, fields)?;
            for raw in &resp.issues {
                if let Some(t) = from_jira_json(raw, creds.base_url) {
                    all.push(t);
                }
            }
            match resp.next_page_token {
                Some(tok) => next_page_token = Some(tok),
                None => return Ok((all, false)),
            }
            if page + 1 == MAX_PAGES {
                eprintln!(
                    "[jira] search truncated after {MAX_PAGES} pages ({} tickets so far); \
                     consider narrowing COPAD_JIRA_LOOKBACK_HOURS or COPAD_JIRA_PROJECTS",
                    all.len()
                );
            }
        }
        Ok((all, true))
    }

    /// Cached resolution; uses `account_id_hint` (captured atomically by `current_credentials`)
    /// for stored creds and falls back to `/myself` for env-source creds. The cache invalidates
    /// when the live `(base_url, email)` differs from the one it was built for, so a hot re-auth
    /// doesn't mis-attribute mention detection to the prior account.
    fn resolve_my_account_id(
        &self,
        creds: jira::Creds,
        account_id_hint: &Option<String>,
    ) -> Result<String, String> {
        let live_signature = (creds.base_url.to_string(), creds.email.to_string());
        {
            let state = self.state.lock().unwrap();
            // Hit only when the signature matches; a stale entry from a previous account is a miss.
            if let (Some(id), Some(sig)) = (&state.my_account_id, &state.my_identity_signature)
                && sig == &live_signature
            {
                return Ok(id.clone());
            }
        }
        let id = match account_id_hint {
            Some(id) => id.clone(),
            None => {
                let user =
                    jira::validate_credentials(creds.base_url, creds.email, creds.api_token)?;
                user.account_id
            }
        };
        let mut state = self.state.lock().unwrap();
        // Identity changed: also invalidate per-ticket snapshots, since the prior account's
        // "assigned to me" set is irrelevant. fired_comments stays: those were emitted already.
        if state.my_identity_signature.as_ref() != Some(&live_signature) {
            if state.my_identity_signature.is_some() {
                eprintln!(
                    "[jira] credentials identity changed (was {:?}, now {:?}) — \
                     invalidating account_id cache and per-ticket snapshots",
                    state.my_identity_signature, live_signature
                );
                state.snapshots.clear();
            }
            state.my_identity_signature = Some(live_signature);
        }
        state.my_account_id = Some(id.clone());
        Ok(id)
    }

    fn process_ticket(
        &self,
        _creds: jira::Creds,
        ticket: &Ticket,
        my_account_id: &str,
        state: &mut PollerState,
    ) {
        let mut new_snap = ticket.snapshot_without_comments();
        let prev = state.snapshots.get(&ticket.key).cloned();

        match &prev {
            None => {
                // First sight: emit ticket_assigned only if currently mine (status_changed needs
                // a baseline). No historical comment events; just record the high-water mark as
                // the next tick's baseline.
                if ticket.assignee_account_id.as_deref() == Some(my_account_id) {
                    self.publish_event("jira.ticket_assigned", ticket, serde_json::Map::new());
                }
                if self.config.fetch_comments {
                    new_snap.last_comment_created_iso = self.high_water_mark(ticket);
                }
            }
            Some(prev_snap) => {
                // No cross-tick dedup for status changes: the snapshot diff guarantees one emit
                // per genuine transition, so a bounce `Open -> In Progress -> Open -> In Progress`
                // legitimately fires each.
                if prev_snap.status_name != new_snap.status_name {
                    let mut extras = serde_json::Map::new();
                    extras.insert("from".to_string(), json!(prev_snap.status_name.clone()));
                    extras.insert("to".to_string(), json!(new_snap.status_name.clone()));
                    self.publish_event("jira.status_changed", ticket, extras);
                }
                let was_mine = prev_snap.assignee_account_id.as_deref() == Some(my_account_id);
                let is_mine = ticket.assignee_account_id.as_deref() == Some(my_account_id);
                if !was_mine && is_mine {
                    self.publish_event("jira.ticket_assigned", ticket, serde_json::Map::new());
                }
                // Comment delta: emit for comments whose `created` is strictly newer than the
                // prior high-water mark. Filtering by timestamp, not index, keeps it robust
                // against deletions.
                if self.config.fetch_comments {
                    let new_water = self.process_new_comments(
                        ticket,
                        my_account_id,
                        prev_snap.last_comment_created_iso.as_deref(),
                        state,
                    );
                    new_snap.last_comment_created_iso =
                        new_water.or_else(|| prev_snap.last_comment_created_iso.clone());
                }
            }
        }

        state.snapshots.insert(ticket.key.clone(), new_snap);
    }

    /// Latest inline-comment `created` instant as canonical UTC RFC 3339. Parses to chrono before
    /// comparing, since a raw-string max would mis-order Jira's mixed `+0000`/`+00:00` offsets.
    fn high_water_mark(&self, ticket: &Ticket) -> Option<String> {
        extract_inline_comments(&ticket.raw_json)
            .into_iter()
            .filter_map(|c| event::parse_jira_timestamp(&c.created))
            .max()
            .map(|t| t.to_rfc3339())
    }

    /// Emits `jira.comment_added` (and `jira.mention` when applicable) for inline comments whose
    /// parsed `created` is strictly after `since_iso`. Returns the new high-water mark as
    /// canonical UTC RFC 3339. Timestamp-vs-index rationale: see `event::TicketSnapshot`; mixed
    /// `+0000`/`+00:00` offsets: see `event::parse_jira_timestamp`.
    ///
    /// **Inline-comment cap (known limitation)**: `/search` returns at most 50 inline comments per
    /// ticket. If the watermark is older than the oldest inline comment AND more than 50 are newer
    /// than it, the gap is silently lost. Closing it needs a paged fallback to
    /// `/issue/{key}/comment`.
    fn process_new_comments(
        &self,
        ticket: &Ticket,
        my_account_id: &str,
        since_iso: Option<&str>,
        state: &mut PollerState,
    ) -> Option<String> {
        let since_instant = since_iso.and_then(event::parse_jira_timestamp);
        // Parse `created` upfront; drop comments that don't parse rather than risk mis-ordering
        // them against the watermark.
        let mut parsed: Vec<(Comment, chrono::DateTime<chrono::Utc>)> =
            extract_inline_comments(&ticket.raw_json)
                .into_iter()
                .filter_map(|c| event::parse_jira_timestamp(&c.created).map(|t| (c, t)))
                .collect();
        parsed.sort_by_key(|(_, t)| *t);
        let mut high_water_instant: Option<chrono::DateTime<chrono::Utc>> = None;
        for (comment, instant) in parsed {
            // Track the high water across ALL comments (not just newly emitted ones) so the next
            // tick has the latest baseline regardless of filtering.
            high_water_instant = match high_water_instant {
                Some(curr) if curr >= instant => Some(curr),
                _ => Some(instant),
            };
            if let Some(since) = since_instant
                && instant <= since
            {
                continue;
            }
            self.emit_comment_events(ticket, &comment, my_account_id, state);
        }
        high_water_instant.map(|t| t.to_rfc3339())
    }

    /// Emits `jira.comment_added` + (`jira.mention` when the body mentions me and isn't
    /// self-authored) for one comment. Dedupes by id, guarding against the same comment surviving
    /// two ticks if clock skew kept the watermark from advancing past it.
    fn emit_comment_events(
        &self,
        ticket: &Ticket,
        comment: &Comment,
        my_account_id: &str,
        state: &mut PollerState,
    ) {
        if state.fired_comments.contains(&comment.id) {
            return;
        }
        state.fired_comments.insert(comment.id.clone());
        let mut extras = serde_json::Map::new();
        extras.insert("comment".to_string(), comment_to_json(comment));
        self.publish_event("jira.comment_added", ticket, extras);
        if comment.author_account_id.as_deref() != Some(my_account_id)
            && event::adf_contains_mention_of(&comment.body_adf, my_account_id)
        {
            let mut extras = serde_json::Map::new();
            extras.insert("comment".to_string(), comment_to_json(comment));
            self.publish_event("jira.mention", ticket, extras);
        }
    }

    /// After a flush, a comment from a mid-pagination race can re-fire on the next tick (the
    /// snapshot moved past it but the dedupe memory is gone). Accepted over per-entry timestamps.
    fn gc_dedupe_set(&self, state: &mut PollerState) {
        if state.fired_comments.len() > DEDUPE_CAP {
            eprintln!(
                "[jira] comment dedupe set exceeded cap ({}); flushing — may re-fire \
                 boundary comments",
                DEDUPE_CAP
            );
            state.fired_comments.clear();
        }
    }

    fn publish_event(&self, kind: &str, ticket: &Ticket, extras: serde_json::Map<String, Value>) {
        let mut payload = to_payload_json(ticket);
        for (k, v) in extras {
            payload.insert(k, v);
        }
        let frame = json!({
            "method": "event.publish",
            "params": {
                "kind": kind,
                "payload": Value::Object(payload),
            }
        });
        if let Err(e) = self.tx.send(frame.to_string()) {
            eprintln!("[jira] failed to enqueue event: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{TokenSet, TokenStore};
    use serde_json::json;
    use std::sync::mpsc::channel;

    struct FixedStore(Option<TokenSet>);
    impl TokenStore for FixedStore {
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
            "fixed"
        }
    }

    fn fake_ticket(key: &str, status: &str, assignee_id: Option<&str>, comments: u64) -> Ticket {
        // `comments` synthetic inline comments by a non-self author at increasing timestamps, so
        // the watermark filter exercises real ordering.
        let inline_comments: Vec<Value> = (0..comments)
            .map(|i| {
                json!({
                    "id": format!("{}", 10000 + i),
                    "body": {
                        "type": "doc",
                        "content": [{
                            "type": "paragraph",
                            "content": [{ "type": "text", "text": format!("comment {i}") }]
                        }]
                    },
                    "created": format!("2026-05-05T1{}:00:00.000+0000", i),
                    "author": { "accountId": "5b-other", "displayName": "Other" }
                })
            })
            .collect();
        let raw = json!({
            "key": key,
            "fields": {
                "summary": "x",
                "status": { "name": status },
                "assignee": assignee_id.map(|a| json!({ "accountId": a, "displayName": "Me" })),
                "reporter": { "accountId": "5b-rep", "displayName": "Rep" },
                "project": { "key": "PROJ" },
                "updated": "2026-05-05T10:00:00.000+0000",
                "comment": { "comments": inline_comments, "total": comments }
            }
        });
        from_jira_json(&raw, "https://x.atlassian.net").unwrap()
    }

    fn drain_kinds(rx: &std::sync::mpsc::Receiver<String>) -> Vec<String> {
        let mut kinds = Vec::new();
        while let Ok(line) = rx.try_recv() {
            let v: Value = serde_json::from_str(&line).unwrap();
            kinds.push(v["params"]["kind"].as_str().unwrap_or("").to_string());
        }
        kinds
    }

    fn mk_poller_with_tx() -> (Poller, std::sync::mpsc::Receiver<String>) {
        let mut cfg = Config::minimal_with_error("test".to_string());
        cfg.fatal_error = None;
        cfg.fetch_comments = true;
        let store: Arc<dyn TokenStore> = Arc::new(FixedStore(None));
        let (tx, rx) = channel();
        let p = Poller::new(Arc::new(cfg), store, tx, Arc::new(AtomicBool::new(true)));
        (p, rx)
    }

    #[test]
    fn first_sight_assigned_to_me_emits_ticket_assigned() {
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();
        let ticket = fake_ticket("PROJ-1", "Open", Some("5b-me"), 0);
        p.process_ticket(creds, &ticket, "5b-me", &mut state);
        drop(state);
        assert_eq!(drain_kinds(&rx), vec!["jira.ticket_assigned"]);
    }

    #[test]
    fn first_sight_assigned_to_other_emits_nothing() {
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();
        let ticket = fake_ticket("PROJ-1", "Open", Some("5b-other"), 0);
        p.process_ticket(creds, &ticket, "5b-me", &mut state);
        drop(state);
        assert!(drain_kinds(&rx).is_empty());
    }

    #[test]
    fn first_sight_does_not_emit_status_changed() {
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();
        let ticket = fake_ticket("PROJ-1", "Done", Some("5b-other"), 0);
        p.process_ticket(creds, &ticket, "5b-me", &mut state);
        drop(state);
        let kinds = drain_kinds(&rx);
        assert!(!kinds.contains(&"jira.status_changed".to_string()));
    }

    #[test]
    fn assignee_transition_to_me_emits_ticket_assigned() {
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();
        let t1 = fake_ticket("PROJ-1", "Open", Some("5b-other"), 0);
        p.process_ticket(creds, &t1, "5b-me", &mut state);
        drop(state);
        let _ = drain_kinds(&rx);
        let mut state = p.state.lock().unwrap();
        let mut raw = t1.raw_json.clone();
        raw["fields"]["updated"] = json!("2026-05-05T11:00:00.000+0000");
        raw["fields"]["assignee"] = json!({ "accountId": "5b-me", "displayName": "Me" });
        let t2 = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        p.process_ticket(creds, &t2, "5b-me", &mut state);
        drop(state);
        assert_eq!(drain_kinds(&rx), vec!["jira.ticket_assigned"]);
    }

    #[test]
    fn status_change_emits_status_changed() {
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();
        let t1 = fake_ticket("PROJ-1", "Open", Some("5b-me"), 0);
        p.process_ticket(creds, &t1, "5b-me", &mut state);
        drop(state);
        let _ = drain_kinds(&rx);
        let mut state = p.state.lock().unwrap();
        let mut raw = t1.raw_json.clone();
        raw["fields"]["status"] = json!({ "name": "In Progress" });
        let t2 = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        p.process_ticket(creds, &t2, "5b-me", &mut state);
        drop(state);
        assert_eq!(drain_kinds(&rx), vec!["jira.status_changed"]);
    }

    #[test]
    fn dedup_blocks_repeat_within_same_tick() {
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();
        let ticket = fake_ticket("PROJ-1", "Open", Some("5b-me"), 0);
        p.process_ticket(creds, &ticket, "5b-me", &mut state);
        // Processing again with the same updated_iso emits nothing: no field changed.
        let kinds_after_first = drain_kinds(&rx).len();
        p.process_ticket(creds, &ticket, "5b-me", &mut state);
        drop(state);
        assert_eq!(kinds_after_first, 1);
        assert!(drain_kinds(&rx).is_empty());
    }

    #[test]
    fn dedup_cap_flushes_comment_set() {
        let (p, _rx) = mk_poller_with_tx();
        let mut state = p.state.lock().unwrap();
        for i in 0..(DEDUPE_CAP + 1) {
            state.fired_comments.insert(format!("comment-{i}"));
        }
        p.gc_dedupe_set(&mut state);
        assert_eq!(state.fired_comments.len(), 0);
    }

    #[test]
    fn status_changed_emits_on_repeated_bounce() {
        // A ticket bouncing Open -> In Progress -> Open -> In Progress must emit ALL transitions,
        // not just the first (a cross-tick `<from>-><to>` dedup would suppress the third).
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();
        let t0 = fake_ticket("PROJ-1", "Open", Some("5b-me"), 0);
        p.process_ticket(creds, &t0, "5b-me", &mut state);
        let _ = drain_kinds(&rx);
        let mut raw = t0.raw_json.clone();
        raw["fields"]["status"] = json!({ "name": "In Progress" });
        raw["fields"]["updated"] = json!("2026-05-05T11:00:00.000+0000");
        let t1 = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        p.process_ticket(creds, &t1, "5b-me", &mut state);
        raw["fields"]["status"] = json!({ "name": "Open" });
        raw["fields"]["updated"] = json!("2026-05-05T12:00:00.000+0000");
        let t2 = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        p.process_ticket(creds, &t2, "5b-me", &mut state);
        raw["fields"]["status"] = json!({ "name": "In Progress" });
        raw["fields"]["updated"] = json!("2026-05-05T13:00:00.000+0000");
        let t3 = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        p.process_ticket(creds, &t3, "5b-me", &mut state);
        drop(state);
        let kinds = drain_kinds(&rx);
        assert_eq!(
            kinds,
            vec![
                "jira.status_changed",
                "jira.status_changed",
                "jira.status_changed"
            ],
            "expected 3 transitions, got {kinds:?}"
        );
    }

    #[test]
    fn comment_delivery_robust_against_deletion_index_shift() {
        // Jira's comment indices shift left when an old comment is deleted, so a startAt=N fetch
        // would skip the comment that shifted into N. The timestamp watermark is robust to that.
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();

        // Tick 1: first sight emits no comment events, but the watermark is set to the latest
        // comment's `created`.
        let t1 = fake_ticket("PROJ-1", "Open", Some("5b-me"), 5);
        p.process_ticket(creds, &t1, "5b-me", &mut state);
        let _ = drain_kinds(&rx);
        let snap = state.snapshots.get("PROJ-1").unwrap().clone();
        assert!(snap.last_comment_created_iso.is_some());
        let watermark_after_first = snap.last_comment_created_iso.clone().unwrap();
        assert!(
            watermark_after_first.starts_with("2026-05-05T14:00:00"),
            "got {watermark_after_first}"
        );

        // Between ticks: comment[1] is deleted and 2 NEW comments (16:00, 17:00) are added.
        // The timestamp watermark sees only those > 14:00 and emits 2.
        let raw = json!({
            "key": "PROJ-1",
            "fields": {
                "summary": "x",
                "status": { "name": "Open" },
                "assignee": { "accountId": "5b-me", "displayName": "Me" },
                "reporter": { "accountId": "5b-rep", "displayName": "Rep" },
                "project": { "key": "PROJ" },
                "updated": "2026-05-05T18:00:00.000+0000",
                "comment": {
                    "comments": [
                        { "id": "10000", "body": { "type": "doc", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "c0" }] }] }, "created": "2026-05-05T10:00:00.000+0000", "author": { "accountId": "5b-other" } },
                        // 10001 deleted
                        { "id": "10002", "body": { "type": "doc", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "c2" }] }] }, "created": "2026-05-05T12:00:00.000+0000", "author": { "accountId": "5b-other" } },
                        { "id": "10003", "body": { "type": "doc", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "c3" }] }] }, "created": "2026-05-05T13:00:00.000+0000", "author": { "accountId": "5b-other" } },
                        { "id": "10004", "body": { "type": "doc", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "c4" }] }] }, "created": "2026-05-05T14:00:00.000+0000", "author": { "accountId": "5b-other" } },
                        { "id": "20001", "body": { "type": "doc", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "new1" }] }] }, "created": "2026-05-05T16:00:00.000+0000", "author": { "accountId": "5b-other" } },
                        { "id": "20002", "body": { "type": "doc", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "new2" }] }] }, "created": "2026-05-05T17:00:00.000+0000", "author": { "accountId": "5b-other" } }
                    ],
                    "total": 6
                }
            }
        });
        let t2 = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        p.process_ticket(creds, &t2, "5b-me", &mut state);
        drop(state);
        let kinds = drain_kinds(&rx);
        assert_eq!(
            kinds,
            vec!["jira.comment_added", "jira.comment_added"],
            "expected 2 comment_added events for 2 new comments after deletion; got {kinds:?}"
        );
        let snap = p
            .state
            .lock()
            .unwrap()
            .snapshots
            .get("PROJ-1")
            .unwrap()
            .clone();
        assert!(
            snap.last_comment_created_iso
                .as_deref()
                .unwrap()
                .starts_with("2026-05-05T17:00:00"),
            "watermark should advance to latest new comment"
        );
    }

    #[test]
    fn snapshot_pruning_treats_re_entry_as_first_sight() {
        // A ticket that falls out of the lookback window and re-enters must NOT diff against
        // months-old state (replayed comments, bogus status_changed); tick() prunes snapshots not
        // observed, so re-entry hits the first-sight branch.
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();
        let t1 = fake_ticket("PROJ-1", "Open", Some("5b-me"), 5);
        p.process_ticket(creds, &t1, "5b-me", &mut state);
        drop(state);
        let _ = drain_kinds(&rx);
        assert_eq!(
            p.state.lock().unwrap().snapshots.len(),
            1,
            "snapshot should exist after first tick"
        );

        // The tick observes an EMPTY result set (ticket fell out of the lookback window) and
        // prunes snapshots.
        let observed: HashSet<String> = HashSet::new();
        p.state
            .lock()
            .unwrap()
            .snapshots
            .retain(|key, _| observed.contains(key));
        assert_eq!(
            p.state.lock().unwrap().snapshots.len(),
            0,
            "snapshot should be pruned after disappearance"
        );

        // Re-entry with changed status AND 10 more comments: first-sight semantics emit
        // ticket_assigned (still mine), no status_changed (no baseline), and no comment backfill
        // (the high-water mark is seeded silently).
        let mut state = p.state.lock().unwrap();
        let mut raw = t1.raw_json.clone();
        raw["fields"]["status"] = json!({ "name": "Done" });
        raw["fields"]["comment"] = json!({ "total": 15 });
        raw["fields"]["updated"] = json!("2026-06-05T10:00:00.000+0000");
        let t2 = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        p.process_ticket(creds, &t2, "5b-me", &mut state);
        drop(state);
        let kinds = drain_kinds(&rx);
        assert_eq!(
            kinds,
            vec!["jira.ticket_assigned"],
            "re-entry should emit only ticket_assigned, NOT status_changed/comment_added; got {kinds:?}"
        );
    }

    #[test]
    fn account_id_cache_invalidates_on_credential_change() {
        // Re-auth to a different Atlassian account while copad stays up must invalidate the cached
        // my_account_id and the snapshots; otherwise mention detection would compare against the
        // old account indefinitely.
        let (p, _rx) = mk_poller_with_tx();
        let creds_a = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "alice@example.com",
            api_token: "tok-a",
        };
        let id_a = p
            .resolve_my_account_id(creds_a, &Some("5b-alice".into()))
            .unwrap();
        assert_eq!(id_a, "5b-alice");
        p.state.lock().unwrap().snapshots.insert(
            "PROJ-1".into(),
            TicketSnapshot {
                updated_iso: "2026-05-05T10:00:00+00:00".into(),
                status_name: "Open".into(),
                assignee_account_id: Some("5b-alice".into()),
                last_comment_created_iso: None,
            },
        );

        let creds_b = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "bob@example.com",
            api_token: "tok-b",
        };
        let id_b = p
            .resolve_my_account_id(creds_b, &Some("5b-bob".into()))
            .unwrap();
        assert_eq!(id_b, "5b-bob", "should re-resolve, not return cached alice");
        assert!(
            p.state.lock().unwrap().snapshots.is_empty(),
            "snapshots should clear on identity change"
        );

        let id_b2 = p
            .resolve_my_account_id(creds_b, &Some("5b-bob".into()))
            .unwrap();
        assert_eq!(id_b2, "5b-bob");
    }

    #[test]
    fn first_sight_watermark_uses_parsed_instants() {
        // high_water_mark must parse to chrono before max(), so mixed +0000 / +00:00 forms order
        // chronologically rather than lexicographically.
        let (p, _rx) = mk_poller_with_tx();
        // The chronologically-latest comment uses a different offset form than the others, so a
        // lexicographic max would pick wrong.
        let raw = json!({
            "key": "PROJ-1",
            "fields": {
                "summary": "x",
                "status": { "name": "Open" },
                "assignee": { "accountId": "5b-me", "displayName": "Me" },
                "reporter": { "accountId": "5b-rep", "displayName": "Rep" },
                "project": { "key": "PROJ" },
                "updated": "2026-05-05T10:00:00.000+0000",
                "comment": {
                    "comments": [
                        { "id": "1", "body": { "type": "doc", "content": [{"type":"paragraph","content":[{"type":"text","text":"latest"}]}] }, "created": "2026-05-05T15:00:00+00:00", "author": { "accountId": "5b-other" } },
                        { "id": "2", "body": { "type": "doc", "content": [{"type":"paragraph","content":[{"type":"text","text":"older"}]}] }, "created": "2026-05-05T14:00:00.000+0000", "author": { "accountId": "5b-other" } }
                    ],
                    "total": 2
                }
            }
        });
        let ticket = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        let watermark = p.high_water_mark(&ticket).unwrap();
        // The 15:00 comment is chronologically latest; both offset forms canonicalize to UTC.
        assert!(
            watermark.starts_with("2026-05-05T15:00:00"),
            "watermark should be the chronologically-latest comment, got {watermark}"
        );
    }

    #[test]
    fn poller_skips_when_fatal_error() {
        // A fresh poller with fatal_error set (mk_poller_with_tx builds a clean config).
        let cfg = Config::minimal_with_error("bad env".to_string());
        let store: Arc<dyn TokenStore> = Arc::new(FixedStore(None));
        let (tx, _rx2) = channel();
        let p = Poller::new(Arc::new(cfg), store, tx, Arc::new(AtomicBool::new(true)));
        assert!(p.tick().is_ok());
    }

    #[test]
    fn poller_skips_when_no_credentials() {
        let mut cfg = Config::minimal_with_error("x".to_string());
        cfg.fatal_error = None; // pretend env was OK but empty
        let store: Arc<dyn TokenStore> = Arc::new(FixedStore(None));
        let (tx, _rx) = channel();
        let p = Poller::new(Arc::new(cfg), store, tx, Arc::new(AtomicBool::new(true)));
        assert!(p.tick().is_ok());
    }
}
