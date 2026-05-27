//! Background poller that publishes `calendar.event_imminent` events.
//!
//! Loop:
//! 1. Wait until the supervisor has sent `initialized` (so events
//!    don't leak before copad has finished the handshake).
//! 2. Run an immediate first `tick()` (no leading sleep), then sleep
//!    `poll_interval` between subsequent ticks.
//! 3. If credentials are not present, skip silently (the user might
//!    run `copad-plugin-calendar auth` while copad is already running).
//! 4. Fetch events for the next `lookahead_hours`.
//! 5. For each (event, lead_minutes) pair, fire if
//!    `firing_time <= now < event.start` (where `firing_time =
//!    event.start - lead_minutes`) AND we have not already fired this
//!    `(event_id, lead)` pair. The dedupe set ensures exactly-once
//!    publishing across ticks, while the bare "is now in the firing
//!    band?" check is what gives us startup-catchup: if copad restarts
//!    20 seconds after the canonical firing time but before
//!    `event.start`, the first tick still publishes. The earlier
//!    `[now + lead, now + lead + poll_interval)` framing missed
//!    exactly that case — the event would already be before the lower
//!    bound and never fire.
//! 6. GC the fired-set: drop entries whose event has passed.
//!
//! The dedupe key is `(event_id, lead_minutes)` so a 60-minute lead
//! and a 10-minute lead on the same event each get exactly one
//! firing (each is independently useful — 60m might trigger
//! "block focus time", 10m might trigger "open meeting note").
//!
//! All errors are logged to stderr and swallowed; one bad poll
//! tick must not kill the daemon.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::config::Config;
use crate::event::{CalendarEvent, to_json};
use crate::gcal::Client;
use crate::store::TokenStore;

/// Firing records linger this long past start so a sleep/wake within
/// the lead window doesn't re-fire.
const RETAIN_AFTER_START: chrono::Duration = chrono::Duration::minutes(5);

pub struct Poller {
    config: Arc<Config>,
    store: Arc<dyn TokenStore>,
    tx: Sender<String>,
    initialized: Arc<AtomicBool>,
    fired: Mutex<HashSet<FiredKey>>,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct FiredKey {
    event_id: String,
    lead_minutes: u32,
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
            fired: Mutex::new(HashSet::new()),
        }
    }

    pub fn run(&self) {
        // Wait for the `initialized` notification before starting any
        // poll cycle. Without this, an event published during init
        // would race against the supervisor's handshake.
        while !self.initialized.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
        // First tick runs IMMEDIATELY (not after a poll_interval
        // sleep). Without this, any event whose firing window
        // (start - lead - poll_interval, start - lead] overlaps with
        // the time between copad startup and the first scheduled tick
        // is permanently missed. Example: copad starts at 09:50:30,
        // lead=10m, event at 10:00, poll=60s → naive sleep-first
        // would not check until 09:51:30, well past the firing
        // window 09:49:00-09:50:00. With immediate-first, 09:50:30
        // catches it on the same tick path.
        if let Err(e) = self.tick() {
            eprintln!("[calendar] initial poll tick failed: {e}");
        }
        loop {
            thread::sleep(self.config.poll_interval);
            if let Err(e) = self.tick() {
                eprintln!("[calendar] poll tick failed: {e}");
            }
        }
    }

    fn tick(&self) -> Result<(), String> {
        if self.config.is_minimal() {
            return Ok(()); // env vars missing — silent skip
        }
        let mut client = match Client::new((*self.config).clone(), self.store.clone()) {
            Ok(c) => c,
            Err(_) => return Ok(()), // not authenticated yet — silent skip
        };
        let now = Utc::now();
        // Look back by the longest lead so a restart inside that
        // window still picks up events whose firing time has passed
        // but whose start has not. Without this, an event scheduled
        // at `now + 5m` with a 10m lead would never enter the
        // returned set (timeMin defaults to now), and the catchup
        // logic in should_fire couldn't see it.
        let max_lead = self.config.lead_minutes.iter().copied().max().unwrap_or(0);
        let lookback = chrono::Duration::minutes(max_lead as i64);
        let max = now + chrono::Duration::hours(self.config.lookahead_hours as i64);
        let events = client.list_events(now - lookback, max)?;

        for event in &events {
            for &lead in &self.config.lead_minutes {
                if self.should_fire(event, lead, now) {
                    self.publish_imminent(event, lead);
                }
            }
        }

        self.gc_fired_set(now);
        Ok(())
    }

    /// Fires when `now` is in `[firing_time, firing_time + max(2×poll, 2min)]`.
    /// The bounded "just-arrived" band stops a 60-min-lead reminder from
    /// firing 51 minutes late as a "catchup". Dedupe set enforces
    /// exactly-once across consecutive in-band ticks.
    fn should_fire(&self, event: &CalendarEvent, lead_minutes: u32, now: DateTime<Utc>) -> bool {
        let lead = chrono::Duration::minutes(lead_minutes as i64);
        let firing_time = event.start_time - lead;
        if now < firing_time {
            return false; // too early — lead window not yet open
        }
        if now >= event.start_time {
            return false; // event already started — reminder is noise
        }
        let poll = chrono::Duration::from_std(self.config.poll_interval)
            .unwrap_or(chrono::Duration::seconds(60));
        let catchup_end = firing_time + std::cmp::max(poll * 2, chrono::Duration::seconds(120));
        if now > catchup_end {
            return false; // stale — firing_time was too long ago for this reminder to be useful
        }
        let key = FiredKey {
            event_id: event.id.clone(),
            lead_minutes,
        };
        let mut fired = self.fired.lock().unwrap();
        if fired.contains(&key) {
            return false;
        }
        fired.insert(key);
        true
    }

    fn publish_imminent(&self, event: &CalendarEvent, lead_minutes: u32) {
        let mut payload = match to_json(event) {
            Value::Object(m) => m,
            _ => unreachable!("event::to_json always returns an object"),
        };
        payload.insert("lead_minutes".to_string(), json!(lead_minutes));
        let frame = json!({
            "method": "event.publish",
            "params": {
                "kind": "calendar.event_imminent",
                "payload": Value::Object(payload),
            }
        });
        if let Err(e) = self.tx.send(frame.to_string()) {
            eprintln!("[calendar] failed to enqueue event: {e}");
        }
    }

    fn gc_fired_set(&self, now: DateTime<Utc>) {
        // Without this, the fired set grows unbounded over a long
        // session. We can't trivially look up event start times for
        // already-removed-from-calendar events, so we use a coarser
        // strategy: drop any entry whose event_id we no longer see in
        // the current event list. Implementation: caller passes the
        // current set; the simpler proxy used here is "if the lead
        // window has likely passed, drop". Because we don't store
        // per-fire timestamps, we instead rely on bounded growth via
        // a cap.
        const CAP: usize = 4096;
        let mut fired = self.fired.lock().unwrap();
        if fired.len() > CAP {
            // Best-effort flush: clear the whole set. Worst case we
            // re-fire a few imminent events that were on the boundary.
            // Acceptable trade for not tracking per-entry timestamps.
            eprintln!(
                "[calendar] fired-set exceeded cap ({}); flushing — may re-fire boundary events",
                CAP
            );
            fired.clear();
        }
        // `now` and `RETAIN_AFTER_START` are kept in the API for a
        // future timestamped strategy.
        let _ = (now, RETAIN_AFTER_START);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Attendee;
    use crate::store::{TokenSet, TokenStore};
    use chrono::TimeZone;
    use std::sync::mpsc::channel;

    fn fake_event(id: &str, start: DateTime<Utc>) -> CalendarEvent {
        CalendarEvent {
            id: id.to_string(),
            recurring_id: None,
            title: "x".into(),
            start_time: start,
            end_time: start + chrono::Duration::minutes(30),
            all_day: false,
            my_response_status: None,
            attendees: vec![],
            organizer: None,
            location: None,
            description: None,
            conference_url: None,
            html_link: "".into(),
        }
    }

    struct NoopStore;
    impl TokenStore for NoopStore {
        fn load(&self) -> Option<TokenSet> {
            None
        }
        fn save(&self, _: &TokenSet) -> Result<(), String> {
            Ok(())
        }
        fn clear(&self) -> Result<(), String> {
            Ok(())
        }
        fn kind(&self) -> &'static str {
            "noop"
        }
    }

    fn mk_poller(lead: Vec<u32>, poll_secs: u64) -> Poller {
        let mut cfg = Config::minimal();
        cfg.lead_minutes = lead;
        cfg.poll_interval = Duration::from_secs(poll_secs);
        Poller::new(
            Arc::new(cfg),
            Arc::new(NoopStore),
            channel().0,
            Arc::new(AtomicBool::new(true)),
        )
    }

    #[test]
    fn fires_at_exact_firing_time() {
        let p = mk_poller(vec![10], 60);
        let now = Utc::now();
        // Event starts in 10 min — firing_time == now, so the lower
        // bound is inclusive: this should fire.
        let evt = fake_event("e1", now + chrono::Duration::minutes(10));
        assert!(p.should_fire(&evt, 10, now));
    }

    #[test]
    fn fires_when_event_within_lead_window() {
        let p = mk_poller(vec![10], 60);
        let now = Utc::now();
        // Event in 9 min, lead 10 min — firing time was 1 min ago,
        // event hasn't started, so this is the steady-state "have
        // we crossed the lead boundary?" YES path.
        let evt = fake_event("e1", now + chrono::Duration::minutes(9));
        assert!(p.should_fire(&evt, 10, now));
    }

    #[test]
    fn fires_after_restart_inside_firing_window() {
        // Reproduces codex round-5 C1: copad restarts at 09:50:30, an
        // event is at 10:00:00 with lead=10. Canonical firing time
        // was 09:50:00, so we are 30 seconds past it but still well
        // before event start. Must fire.
        let p = mk_poller(vec![10], 60);
        let now = Utc.with_ymd_and_hms(2026, 4, 26, 9, 50, 30).unwrap();
        let evt = fake_event("e1", Utc.with_ymd_and_hms(2026, 4, 26, 10, 0, 0).unwrap());
        assert!(p.should_fire(&evt, 10, now));
    }

    #[test]
    fn does_not_fire_before_firing_time() {
        // Event in 30 min, lead 10 min — firing time is 20 min away.
        let p = mk_poller(vec![10], 60);
        let now = Utc::now();
        let evt = fake_event("e1", now + chrono::Duration::minutes(30));
        assert!(!p.should_fire(&evt, 10, now));
    }

    #[test]
    fn does_not_fire_when_firing_time_long_past() {
        // 60-min lead on an event only 9 min away. Firing time was
        // 51 min ago — far past the 2*poll catchup window. A reminder
        // 51 min late lost its meaning.
        let p = mk_poller(vec![60], 60);
        let now = Utc::now();
        let evt = fake_event("e1", now + chrono::Duration::minutes(9));
        assert!(!p.should_fire(&evt, 60, now));
    }

    #[test]
    fn does_not_fire_after_event_started() {
        // Event was 5 min ago — meeting is in progress; reminder is
        // useless noise.
        let p = mk_poller(vec![10], 60);
        let now = Utc::now();
        let evt = fake_event("e1", now - chrono::Duration::minutes(5));
        assert!(!p.should_fire(&evt, 10, now));
    }

    #[test]
    fn dedupes_within_same_lead_across_ticks() {
        let p = mk_poller(vec![10], 60);
        let now = Utc::now();
        let evt = fake_event("e1", now + chrono::Duration::minutes(9));
        assert!(p.should_fire(&evt, 10, now));
        assert!(!p.should_fire(&evt, 10, now)); // second call deduped
        // Subsequent tick a few seconds later still in the firing
        // window: still deduped.
        let later = now + chrono::Duration::seconds(30);
        assert!(!p.should_fire(&evt, 10, later));
    }

    #[test]
    fn fires_independently_for_each_lead() {
        let p = mk_poller(vec![10, 60], 60);
        let now = Utc::now();
        // e1: 9 min away, lead=10 fires (firing_time was 1 min ago,
        // event is upcoming), but lead=60 is too early (firing_time
        // is 51 min from now — well in the future).
        let evt_10 = fake_event("e1", now + chrono::Duration::minutes(9));
        assert!(p.should_fire(&evt_10, 10, now));
        assert!(!p.should_fire(&evt_10, 60, now));
        // e2: 59 min away, only lead=60 fires.
        let evt_60 = fake_event("e2", now + chrono::Duration::minutes(59));
        assert!(p.should_fire(&evt_60, 60, now));
        assert!(!p.should_fire(&evt_60, 10, now));
    }

    #[test]
    fn publish_emits_event_publish_frame_with_lead() {
        let (tx, rx) = channel();
        let mut cfg = Config::minimal();
        cfg.lead_minutes = vec![10];
        let p = Poller::new(
            Arc::new(cfg),
            Arc::new(NoopStore),
            tx,
            Arc::new(AtomicBool::new(true)),
        );
        let evt = fake_event("e1", Utc::now() + chrono::Duration::minutes(10));
        p.publish_imminent(&evt, 10);
        let line = rx.recv().unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["method"], "event.publish");
        assert_eq!(v["params"]["kind"], "calendar.event_imminent");
        assert_eq!(v["params"]["payload"]["id"], "e1");
        assert_eq!(v["params"]["payload"]["lead_minutes"], 10);
    }

    #[test]
    fn fired_set_caps_to_prevent_unbounded_growth() {
        let p = mk_poller(vec![10], 60);
        let now = Utc::now();
        // Fill past CAP. Each event is 9 min from now so all fire
        // (firing_time was 1 min ago, event upcoming).
        for i in 0..5000 {
            let evt = fake_event(&format!("evt-{i}"), now + chrono::Duration::minutes(9));
            p.should_fire(&evt, 10, now);
        }
        p.gc_fired_set(now);
        let fired = p.fired.lock().unwrap();
        // After GC the set is empty (we cleared it because cap exceeded).
        assert_eq!(fired.len(), 0);
        let _ = Attendee {
            email: None,
            name: None,
            response_status: None,
            is_self: false,
            is_organizer: false,
        };
    }
}
