//! Background poller that publishes the four `jira.*` event kinds.
//!
//! Loop:
//! 1. Wait until the supervisor has sent `initialized` (so events
//!    don't leak before copad has finished the handshake).
//! 2. First tick runs IMMEDIATELY (no leading sleep), then sleep
//!    `poll_interval` between subsequent ticks. Same posture as
//!    calendar's poller.
//! 3. If `fatal_error` is set OR neither env nor store has usable
//!    credentials, skip silently — the user can fix the env or run
//!    `copad-plugin-jira auth` while copad is already running and
//!    the next tick picks up the new state.
//! 4. Fetch ticket pages via `jira::search` (ORDER BY updated DESC,
//!    JQL: `(assignee = currentUser() OR watcher = currentUser()) AND
//!    updated > -<lookback>h[ AND project in (X, Y)]`), follow
//!    pagination via `next_start_at`, capped at MAX_PAGES.
//! 5. Diff each ticket against the prior `TicketSnapshot`:
//!    - **First sight** (snapshot is None): emit `jira.ticket_assigned`
//!      iff assigned to me right now. Do NOT emit `status_changed` —
//!      we have no `from` to report.
//!    - **Subsequent sight**: assignee changed → `jira.ticket_assigned`
//!      iff newly assigned to me. Status changed → `jira.status_changed`.
//!      Walk the inline `fields.comment.comments[]` and emit
//!      `jira.comment_added` per comment whose parsed `created`
//!      instant is strictly newer than the per-ticket watermark,
//!      plus `jira.mention` when the comment body mentions me.
//! 6. Save the new snapshot.
//! 7. Dedupe via a `HashSet<(key, kind, discriminator)>` so a transient
//!    re-publication can't fire twice. Cap at 4096; flush past cap.
//!
//! All errors are logged to stderr and swallowed; one bad poll tick
//! must not kill the daemon.

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
    /// Cached across ticks. `my_identity_signature` records the
    /// (base_url, email) the cache was built for — a hot re-auth to a
    /// different Atlassian account invalidates the cache so
    /// assigned-to-me / mention detection doesn't silently mis-attribute
    /// to the prior user.
    my_account_id: Option<String>,
    my_identity_signature: Option<(String, String)>,
    /// Sole guard against cross-tick re-emission: each `process_ticket`
    /// mutates the snapshot to current, so the next tick (or a duplicate
    /// observation later in the same paginated response) emits nothing
    /// unless something genuinely changed Jira-side.
    snapshots: HashMap<String, TicketSnapshot>,
    /// Per-comment dedupe (cap 4096; flush past cap). Snapshot mutation
    /// alone isn't enough: backfilling many comments across pages can
    /// see the same comment id twice if a fresh one lands mid-paginate.
    /// Status/assignee transitions are NOT deduped — a genuine
    /// `Open → In Progress → Open → In Progress` bounce must emit each.
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
        // Wait for the `initialized` notification before starting
        // any poll cycle. Without this, an event published during
        // init would race against the supervisor's handshake.
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
        // Pass the account_id_hint atomically with the creds — without
        // this, `resolve_my_account_id` would re-read self.store and
        // could pair the live env creds with a stale store account_id
        // if the store changed between the two reads.
        let my_account_id = self.resolve_my_account_id(creds_borrow, &creds.account_id_hint)?;

        let jql =
            jira::build_polling_jql(self.config.lookback_hours, self.config.projects.as_deref());
        // `*all` returns every field including customs so `event_json`
        // in emitted events carries the verbatim payload we promise
        // (triggers reach `event.event_json.fields.customfield_10001`
        // etc.). Trade-off: ~5-10× larger response than the explicit
        // field list. Acceptable for v1 — typical workspaces have a
        // few dozen tickets in the lookback window. If response size
        // becomes a real cost we can add `COPAD_JIRA_LEAN_PAYLOAD=1`
        // to opt out, but only when someone actually reports it.
        let fields = ["*all"];
        let (tickets, truncated) = self.fetch_all_tickets(creds_borrow, &jql, &fields)?;
        eprintln!(
            "[jira] poll tick OK ({} tickets in lookback window)",
            tickets.len()
        );

        // Process inside a single state-lock scope so a concurrent
        // shutdown can't observe a half-applied snapshot diff.
        let mut state = self.state.lock().unwrap();
        // Prune snapshots for tickets that fell out of the lookback
        // window. Without this a ticket that goes inactive for >
        // lookback_hours then later receives an update would diff
        // against months-old state — emitting a bogus
        // `jira.status_changed` for transitions that already
        // happened, replaying every comment since the snapshot as
        // brand-new `jira.comment_added` / `jira.mention`, etc.
        // Treating the re-entry as first-sight suppresses status/
        // comment events (no baseline) but still emits
        // `jira.ticket_assigned` if it's currently mine — exactly
        // the right semantics. Skipped on truncation: if the search
        // hit MAX_PAGES we don't have the full picture and pruning
        // would drop legitimate state.
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

    /// Returns `(tickets, truncated)`. `truncated == true` means MAX_PAGES
    /// stopped us short — the caller skips snapshot pruning to avoid
    /// dropping state for tickets outside the truncated subset.
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

    /// Cached resolution; uses `account_id_hint` (atomically captured by
    /// `current_credentials`) for stored creds, falls back to a `/myself`
    /// call for env-source creds. Cache invalidates when the live
    /// `(base_url, email)` differs from the one the cached id was built
    /// for, so a hot re-auth doesn't mis-attribute mention detection
    /// to the prior account.
    fn resolve_my_account_id(
        &self,
        creds: jira::Creds,
        account_id_hint: &Option<String>,
    ) -> Result<String, String> {
        let live_signature = (creds.base_url.to_string(), creds.email.to_string());
        {
            let state = self.state.lock().unwrap();
            // Cache hit only when the signature matches — stale
            // entry from a previous account is treated as a miss.
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
        // Identity changed — also invalidate per-ticket snapshots
        // since the prior account's "assigned to me" set has no
        // bearing on the new account's. fired_comments stays — they
        // were emitted historically and shouldn't fire again.
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
                // First sight. Only emit ticket_assigned if currently
                // assigned to me; status_changed needs a baseline.
                // Comments: do NOT emit historical events — just
                // record the high water mark so the next tick has a
                // baseline to filter against.
                if ticket.assignee_account_id.as_deref() == Some(my_account_id) {
                    self.publish_event("jira.ticket_assigned", ticket, serde_json::Map::new());
                }
                if self.config.fetch_comments {
                    new_snap.last_comment_created_iso = self.high_water_mark(ticket);
                }
            }
            Some(prev_snap) => {
                // Status change. No cross-tick dedup — the snapshot
                // diff guarantees one emit per genuine transition;
                // a bounce `Open → In Progress → Open → In Progress`
                // legitimately fires three events, one per state
                // change.
                if prev_snap.status_name != new_snap.status_name {
                    let mut extras = serde_json::Map::new();
                    extras.insert("from".to_string(), json!(prev_snap.status_name.clone()));
                    extras.insert("to".to_string(), json!(new_snap.status_name.clone()));
                    self.publish_event("jira.status_changed", ticket, extras);
                }
                // Assignee transitioned to me. Same posture as above.
                let was_mine = prev_snap.assignee_account_id.as_deref() == Some(my_account_id);
                let is_mine = ticket.assignee_account_id.as_deref() == Some(my_account_id);
                if !was_mine && is_mine {
                    self.publish_event("jira.ticket_assigned", ticket, serde_json::Map::new());
                }
                // Comment delta — walk inline comments and emit for
                // any whose `created` timestamp is strictly newer than
                // the prior snapshot's high water mark. Robust against
                // deletion: deleting an old comment doesn't affect the
                // ordering of newer ones, and we filter by timestamp
                // not by index so deletion can't make us skip.
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

    /// Latest inline-comment `created` instant as canonical UTC RFC 3339.
    /// Parses to chrono before comparing — raw-string max would mis-order
    /// Jira's mixed `+0000`/`+00:00` offsets. None when no parseable comments.
    fn high_water_mark(&self, ticket: &Ticket) -> Option<String> {
        extract_inline_comments(&ticket.raw_json)
            .into_iter()
            .filter_map(|c| event::parse_jira_timestamp(&c.created))
            .max()
            .map(|t| t.to_rfc3339())
    }

    /// Emits `jira.comment_added` (and `jira.mention` when applicable)
    /// for inline comments whose parsed `created` is strictly after
    /// `since_iso`. Returns the new high-water mark as canonical UTC
    /// RFC 3339. Timestamp-vs-index rationale: see `event::TicketSnapshot`.
    /// Mixed `+0000`/`+00:00` offset handling: see `event::parse_jira_timestamp`.
    ///
    /// **Inline-comment cap (known limitation)**: `/search` returns at most
    /// 50 inline comments per ticket. If the watermark is older than the
    /// oldest inline comment AND there are >50 newer than the watermark,
    /// the gap range is silently lost. Closing this requires falling back
    /// to `/issue/{key}/comment` paged when the oldest inline comment is
    /// newer than the watermark (signal of pagination overflow); deferred
    /// until a real workflow hits it.
    fn process_new_comments(
        &self,
        ticket: &Ticket,
        my_account_id: &str,
        since_iso: Option<&str>,
        state: &mut PollerState,
    ) -> Option<String> {
        let since_instant = since_iso.and_then(event::parse_jira_timestamp);
        // Parse each comment's created upfront; drop any whose
        // timestamp doesn't parse rather than risk mis-ordering them
        // against the watermark.
        let mut parsed: Vec<(Comment, chrono::DateTime<chrono::Utc>)> =
            extract_inline_comments(&ticket.raw_json)
                .into_iter()
                .filter_map(|c| event::parse_jira_timestamp(&c.created).map(|t| (c, t)))
                .collect();
        parsed.sort_by_key(|(_, t)| *t);
        let mut high_water_instant: Option<chrono::DateTime<chrono::Utc>> = None;
        for (comment, instant) in parsed {
            // Track high water across ALL comments (not just
            // newly-emitted ones) so the next tick has the most
            // recent baseline regardless of filtering.
            high_water_instant = match high_water_instant {
                Some(curr) if curr >= instant => Some(curr),
                _ => Some(instant),
            };
            // Filter to strictly-newer-than-watermark.
            if let Some(since) = since_instant
                && instant <= since
            {
                continue;
            }
            self.emit_comment_events(ticket, &comment, my_account_id, state);
        }
        high_water_instant.map(|t| t.to_rfc3339())
    }

    /// Emits `jira.comment_added` + (`jira.mention` when body mentions
    /// me and isn't self-authored) for one comment. Dedupes by id —
    /// guards against the same comment surviving two ticks if clock skew
    /// kept the watermark from advancing past it.
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

    /// Worst case after a flush: a comment from a mid-pagination race
    /// on the same tick re-fires on the next tick (snapshot moved past
    /// it but dedupe memory is gone). Acceptable vs per-entry timestamps.
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
        // Generate `comments` synthetic inline comments, each authored
        // by 5b-other (non-self) at increasing timestamps so the
        // watermark-based filter exercises real ordering.
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
        // Status is "Done" but no baseline → no status_changed.
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
        // First tick: assigned to other.
        let t1 = fake_ticket("PROJ-1", "Open", Some("5b-other"), 0);
        p.process_ticket(creds, &t1, "5b-me", &mut state);
        drop(state);
        let _ = drain_kinds(&rx);
        // Second tick: now assigned to me, with a different updated_iso
        // (real Jira would bump the timestamp).
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
        // Process again with same updated_iso → no fresh event.
        // (Snapshot now exists but updated didn't change so neither
        // does any other field — branches don't fire.)
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
        // Codex round-2 C2: a ticket bouncing Open → In Progress →
        // Open → In Progress must emit ALL three transitions, not
        // just the first one (the cross-tick `<from>-><to>` dedup
        // we removed used to suppress the third).
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();
        // T0: Open (first sight, no event)
        let t0 = fake_ticket("PROJ-1", "Open", Some("5b-me"), 0);
        p.process_ticket(creds, &t0, "5b-me", &mut state);
        let _ = drain_kinds(&rx);
        // T1: → In Progress
        let mut raw = t0.raw_json.clone();
        raw["fields"]["status"] = json!({ "name": "In Progress" });
        raw["fields"]["updated"] = json!("2026-05-05T11:00:00.000+0000");
        let t1 = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        p.process_ticket(creds, &t1, "5b-me", &mut state);
        // T2: → Open
        raw["fields"]["status"] = json!({ "name": "Open" });
        raw["fields"]["updated"] = json!("2026-05-05T12:00:00.000+0000");
        let t2 = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        p.process_ticket(creds, &t2, "5b-me", &mut state);
        // T3: → In Progress AGAIN
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
        // Codex round-7 C1: Jira's comment indices shift left when
        // an old comment is deleted. A startAt=N fetch after a
        // deletion would skip the new comment that shifted into
        // position N. The timestamp-based watermark this method
        // uses is robust to that — `created` timestamps don't
        // change when other comments are deleted.
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        let mut state = p.state.lock().unwrap();

        // Tick 1: ticket has 5 comments, all timestamps known.
        // First sight: no comment events emitted, but watermark is set
        // to the latest comment's `created`.
        let t1 = fake_ticket("PROJ-1", "Open", Some("5b-me"), 5);
        p.process_ticket(creds, &t1, "5b-me", &mut state);
        // Drain ticket_assigned event (first sight, mine).
        let _ = drain_kinds(&rx);
        let snap = state.snapshots.get("PROJ-1").unwrap().clone();
        assert!(snap.last_comment_created_iso.is_some());
        let watermark_after_first = snap.last_comment_created_iso.clone().unwrap();
        // fake_ticket comment 4 has created="2026-05-05T14:00:00..."
        assert!(
            watermark_after_first.starts_with("2026-05-05T14:00:00"),
            "got {watermark_after_first}"
        );

        // Between ticks: comment[1] deleted (index 1), 2 NEW comments
        // added with timestamps 16:00 and 17:00. The remaining comments
        // are at indices [0, 2, 3, 4, new1, new2] = 6 items but with
        // the original [10000, 10002, 10003, 10004] + new ids.
        // Watermark filter sees only timestamps > 14:00 and emits 2.
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
        // Two new comments → exactly 2 comment_added events.
        // Status didn't change, assignee didn't change, so no other
        // events.
        assert_eq!(
            kinds,
            vec!["jira.comment_added", "jira.comment_added"],
            "expected 2 comment_added events for 2 new comments after deletion; got {kinds:?}"
        );
        // Watermark advanced to the latest new comment's timestamp.
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
        // Codex round-6 C1: a ticket that falls out of the lookback
        // window then re-enters must NOT diff against months-old
        // state (which would replay historical comments / emit a
        // bogus status_changed for transitions that already happened).
        // The tick() loop prunes snapshots not in the current
        // observed set, so re-entry hits the first-sight branch.
        let (p, rx) = mk_poller_with_tx();
        let creds = jira::Creds {
            base_url: "https://x.atlassian.net",
            email: "e",
            api_token: "t",
        };
        // Tick 1: ticket assigned to me with 5 comments. Establishes baseline.
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

        // Simulate: tick observes EMPTY result set (ticket fell out
        // of lookback window). The pruning logic in tick() drops
        // snapshots not in the observed set.
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

        // Now ticket re-enters with status changed AND 10 more comments.
        // First-sight semantics: emits ticket_assigned (still mine),
        // does NOT emit status_changed (no baseline), does NOT
        // backfill historical comments (no watermark → high_water
        // mark seeded silently from the inline comments).
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
        // Codex round-9 C2: re-auth to a different Atlassian account
        // while copad stays up must invalidate the cached
        // my_account_id (and snapshots, since the prior account's
        // assigned-to-me set is irrelevant). Otherwise mention
        // detection would compare against the old account
        // indefinitely.
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
        // Seed a snapshot to confirm it gets cleared on identity change.
        p.state.lock().unwrap().snapshots.insert(
            "PROJ-1".into(),
            TicketSnapshot {
                updated_iso: "2026-05-05T10:00:00+00:00".into(),
                status_name: "Open".into(),
                assignee_account_id: Some("5b-alice".into()),
                last_comment_created_iso: None,
            },
        );

        // Re-auth: same base_url, different email.
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

        // Same creds again → cache hit.
        let id_b2 = p
            .resolve_my_account_id(creds_b, &Some("5b-bob".into()))
            .unwrap();
        assert_eq!(id_b2, "5b-bob");
    }

    #[test]
    fn first_sight_watermark_uses_parsed_instants() {
        // Codex round-9 C1: high_water_mark must parse to chrono
        // before max() so mixed +0000 / +00:00 forms order
        // chronologically rather than lexicographically.
        let (p, _rx) = mk_poller_with_tx();
        // Build a ticket where the chronologically-latest comment
        // uses a different offset form than the others. Lexicographic
        // max would pick wrong; parsed-instant max picks correctly.
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
                        // Latest in real time, but uses +00:00 offset.
                        { "id": "1", "body": { "type": "doc", "content": [{"type":"paragraph","content":[{"type":"text","text":"latest"}]}] }, "created": "2026-05-05T15:00:00+00:00", "author": { "accountId": "5b-other" } },
                        // Older in real time, uses +0000 offset.
                        { "id": "2", "body": { "type": "doc", "content": [{"type":"paragraph","content":[{"type":"text","text":"older"}]}] }, "created": "2026-05-05T14:00:00.000+0000", "author": { "accountId": "5b-other" } }
                    ],
                    "total": 2
                }
            }
        });
        let ticket = from_jira_json(&raw, "https://x.atlassian.net").unwrap();
        let watermark = p.high_water_mark(&ticket).unwrap();
        // The 15:00 comment is chronologically latest. Both forms
        // canonicalize to the same UTC RFC 3339, and the parsed-max
        // picks the 15:00 one regardless of original offset format.
        assert!(
            watermark.starts_with("2026-05-05T15:00:00"),
            "watermark should be the chronologically-latest comment, got {watermark}"
        );
    }

    #[test]
    fn poller_skips_when_fatal_error() {
        // Construct a fresh poller with fatal_error set (the
        // mk_poller_with_tx helper produces a clean config; we want
        // the bad branch here). Drive tick() and confirm no panic.
        let cfg = Config::minimal_with_error("bad env".to_string());
        let store: Arc<dyn TokenStore> = Arc::new(FixedStore(None));
        let (tx, _rx2) = channel();
        let p = Poller::new(Arc::new(cfg), store, tx, Arc::new(AtomicBool::new(true)));
        // tick should return Ok(()) without making any HTTP call.
        assert!(p.tick().is_ok());
    }

    #[test]
    fn poller_skips_when_no_credentials() {
        // Empty config (no env, no fatal), empty store: tick is OK,
        // no HTTP calls.
        let mut cfg = Config::minimal_with_error("x".to_string());
        cfg.fatal_error = None; // pretend env was OK but empty
        let store: Arc<dyn TokenStore> = Arc::new(FixedStore(None));
        let (tx, _rx) = channel();
        let p = Poller::new(Arc::new(cfg), store, tx, Arc::new(AtomicBool::new(true)));
        assert!(p.tick().is_ok());
    }
}
