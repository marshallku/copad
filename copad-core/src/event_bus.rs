use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_SUBSCRIBER_BUFFER: usize = 256;

/// Monotonic per-process counter for bridge crossings. Each
/// `_bus.publish` ingest (daemon side) and each daemon→GUI receive
/// (GUI side) calls [`next_bridge_id`] to stamp the event; the
/// symmetric outgoing forwarder skips events whose `bridge_id` is
/// `Some`. Starts at 1 so `Some(0)` stays available for tests.
static NEXT_BRIDGE_ID: AtomicU64 = AtomicU64::new(1);

/// Issue a fresh bridge id. Wraparound takes ~10⁹ years at any
/// realistic event rate; not worth a leak-safe variant.
pub fn next_bridge_id() -> u64 {
    NEXT_BRIDGE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Provenance of a bus event. Consulted by `TriggerEngine` fan-out to
/// decide whether a trigger may fire on it. `Internal` covers everything
/// the daemon itself produces (plugin stdio publishes, action completion
/// events, time-based wakeups, bridge-relayed events from another local
/// bus). `External` covers events that entered via socket
/// `events.publish` — i.e. arbitrary same-UID processes including
/// SSH-forwarded hook callers. See `docs/harness-integration.md`
/// § "Trust boundary" and the trigger `[security] accept_external`
/// opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    #[default]
    Internal,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub kind: String,
    pub source: String,
    pub timestamp_ms: u64,
    pub payload: Value,
    /// Local-only marker — set when this event was lifted off a bridge
    /// crossing (daemon→GUI or GUI→daemon) and re-published on the
    /// destination bus. `#[serde(skip)]` keeps it out of every wire
    /// frame so plugins/clients never see it. Forwarders skip events
    /// with `Some(_)` so an event that already crossed once cannot
    /// loop back through the other direction's forwarder.
    #[serde(skip, default)]
    pub bridge_id: Option<u64>,
    /// Trust-boundary provenance. Serde default = `Internal` so that
    /// older wire formats deserialize as the safe default (treated as
    /// trusted), and constructors that don't explicitly set it stay
    /// trusted. The single chokepoint that flips this to `External`
    /// is the daemon's `events.publish` handler.
    #[serde(default)]
    pub origin: Origin,
}

impl Event {
    pub fn new(kind: impl Into<String>, source: impl Into<String>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            source: source.into(),
            timestamp_ms: now_millis(),
            payload,
            bridge_id: None,
            origin: Origin::default(),
        }
    }

    pub fn with_bridge_id(mut self, id: u64) -> Self {
        self.bridge_id = Some(id);
        self
    }

    pub fn with_origin(mut self, origin: Origin) -> Self {
        self.origin = origin;
        self
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct EventReceiver {
    inner: Receiver<Event>,
}

/// Lets consumers (e.g. the service supervisor's subscribe-forwarder)
/// break out of a blocking recv periodically without busy-spinning, so
/// an external stop flag can drive teardown.
#[derive(Debug, Clone)]
pub enum RecvOutcome {
    Event(Event),
    Timeout,
    Disconnected,
}

impl EventReceiver {
    pub fn try_recv(&self) -> Option<Event> {
        self.inner.try_recv().ok()
    }

    pub fn recv(&self) -> Option<Event> {
        self.inner.recv().ok()
    }

    /// Distinguishes "got an event", "timed out", and "bus dropped this
    /// subscriber" so callers can check an external stop flag between
    /// waits without losing events that arrived during the wait.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> RecvOutcome {
        match self.inner.recv_timeout(timeout) {
            Ok(e) => RecvOutcome::Event(e),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => RecvOutcome::Timeout,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => RecvOutcome::Disconnected,
        }
    }
}

enum SubscriberSender {
    Bounded(SyncSender<Event>),
    Unbounded(Sender<Event>),
}

impl SubscriberSender {
    fn deliver(&self, event: Event) -> DeliveryResult {
        match self {
            Self::Bounded(tx) => match tx.try_send(event) {
                Ok(()) => DeliveryResult::Ok,
                Err(TrySendError::Full(_)) => DeliveryResult::Full,
                Err(TrySendError::Disconnected(_)) => DeliveryResult::Disconnected,
            },
            Self::Unbounded(tx) => match tx.send(event) {
                Ok(()) => DeliveryResult::Ok,
                Err(_) => DeliveryResult::Disconnected,
            },
        }
    }
}

enum DeliveryResult {
    Ok,
    Full,
    Disconnected,
}

struct Subscriber {
    pattern: String,
    sender: SubscriberSender,
}

pub struct EventBus {
    subscribers: Mutex<Vec<Subscriber>>,
    default_buffer: usize,
    /// Bounded ring buffer of recent events. Powers the `event.history`
    /// socket action ("what just happened?" — useful for catching up
    /// AFK without keeping a live subscription open). Mutex is separate
    /// from `subscribers` so a publishing thread that has the
    /// subscribers lock isn't blocked by a history reader, and vice
    /// versa.
    history: Mutex<std::collections::VecDeque<Event>>,
    history_cap: usize,
}

const DEFAULT_HISTORY_CAP: usize = 500;

impl EventBus {
    pub fn new() -> Self {
        Self::with_default_buffer(DEFAULT_SUBSCRIBER_BUFFER)
    }

    pub fn with_default_buffer(default_buffer: usize) -> Self {
        Self::with_capacities(default_buffer, DEFAULT_HISTORY_CAP)
    }

    pub fn with_capacities(default_buffer: usize, history_cap: usize) -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            default_buffer,
            history: Mutex::new(std::collections::VecDeque::with_capacity(
                history_cap.max(1),
            )),
            history_cap: history_cap.max(1),
        }
    }

    pub fn subscribe(&self, pattern: impl Into<String>) -> EventReceiver {
        self.subscribe_with_buffer(pattern, self.default_buffer)
    }

    pub fn subscribe_with_buffer(
        &self,
        pattern: impl Into<String>,
        buffer: usize,
    ) -> EventReceiver {
        let (tx, rx) = sync_channel(buffer);
        self.subscribers.lock().unwrap().push(Subscriber {
            pattern: pattern.into(),
            sender: SubscriberSender::Bounded(tx),
        });
        EventReceiver { inner: rx }
    }

    /// Unbounded — for external wire streams (socket `event.subscribe`
    /// projection) where loss violates the client contract. Caller drains.
    pub fn subscribe_unbounded(&self, pattern: impl Into<String>) -> EventReceiver {
        let (tx, rx) = channel();
        self.subscribers.lock().unwrap().push(Subscriber {
            pattern: pattern.into(),
            sender: SubscriberSender::Unbounded(tx),
        });
        EventReceiver { inner: rx }
    }

    /// Publish an event whose `bridge_id` is already set by the caller.
    /// Used by bridge-receive paths (daemon's `_bus.publish` handler and
    /// the GUI's daemon-bridge reader) so the symmetric outgoing
    /// forwarder can recognize this event as "already crossed once" and
    /// not re-forward it.
    pub fn publish_bridged(&self, event: Event, bridge_id: u64) {
        self.publish(event.with_bridge_id(bridge_id));
    }

    pub fn publish(&self, event: Event) {
        // Push to history BEFORE distributing — a subscriber callback
        // could re-publish synchronously and we want the source event
        // recorded first.
        {
            let mut h = self.history.lock().unwrap();
            if h.len() >= self.history_cap {
                h.pop_front();
            }
            h.push_back(event.clone());
        }
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|sub| {
            if !pattern_matches(&sub.pattern, &event.kind) {
                return true;
            }
            match sub.sender.deliver(event.clone()) {
                DeliveryResult::Ok => true,
                DeliveryResult::Full => {
                    log::warn!(
                        "event bus subscriber pattern={:?} buffer full, dropping kind={:?}",
                        sub.pattern,
                        event.kind
                    );
                    true
                }
                DeliveryResult::Disconnected => false,
            }
        });
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }

    /// Recent events in arrival order (oldest first). `since_ms`
    /// filters to events with `timestamp_ms >= since_ms`; `kind_glob`
    /// filters by event-kind glob (same matcher as `subscribe`).
    /// Returns a snapshot — callers don't see live updates after this
    /// call.
    pub fn history(&self, since_ms: Option<u64>, kind_glob: Option<&str>) -> Vec<Event> {
        let h = self.history.lock().unwrap();
        h.iter()
            .filter(|e| since_ms.is_none_or(|cutoff| e.timestamp_ms >= cutoff))
            .filter(|e| kind_glob.is_none_or(|g| pattern_matches(g, &e.kind)))
            .cloned()
            .collect()
    }

    pub fn history_capacity(&self) -> usize {
        self.history_cap
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Single source of truth for `event_kind` matching (shared with
/// `trigger::Trigger`). Rules: `*` matches everything; `foo.*` matches
/// any deeper kind starting with `foo.` (`foo.bar.baz` matches; bare
/// `foo` does not); otherwise exact equality.
pub fn pattern_matches(pattern: &str, kind: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        return kind.len() > prefix.len()
            && kind.starts_with(prefix)
            && kind.as_bytes()[prefix.len()] == b'.';
    }
    pattern == kind
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mk(kind: &str) -> Event {
        Event::new(kind, "test", json!({}))
    }

    #[test]
    fn pattern_exact_match() {
        assert!(pattern_matches("foo.bar", "foo.bar"));
        assert!(!pattern_matches("foo.bar", "foo.baz"));
        assert!(!pattern_matches("foo.bar", "foo"));
    }

    #[test]
    fn pattern_star_matches_anything() {
        assert!(pattern_matches("*", "anything.at.all"));
        assert!(pattern_matches("*", "x"));
    }

    #[test]
    fn pattern_prefix_wildcard() {
        assert!(pattern_matches("foo.*", "foo.bar"));
        assert!(pattern_matches("foo.*", "foo.bar.baz"));
        assert!(!pattern_matches("foo.*", "foo"));
        assert!(!pattern_matches("foo.*", "foobar"));
        assert!(!pattern_matches("foo.*", "bar.foo"));
    }

    #[test]
    fn recv_timeout_returns_event_when_available() {
        let bus = EventBus::new();
        let rx = bus.subscribe("k");
        bus.publish(mk("k"));
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            RecvOutcome::Event(e) => assert_eq!(e.kind, "k"),
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn recv_timeout_returns_timeout_when_idle() {
        let bus = EventBus::new();
        let rx = bus.subscribe("k");
        match rx.recv_timeout(std::time::Duration::from_millis(20)) {
            RecvOutcome::Timeout => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
        // Bus is still alive; the channel is still connected — sanity check
        // that another event can still arrive after a timeout.
        bus.publish(mk("k"));
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            RecvOutcome::Event(_) => {}
            other => panic!("expected Event after timeout, got {other:?}"),
        }
    }

    #[test]
    fn recv_timeout_returns_disconnected_when_bus_dropped() {
        let rx = {
            let bus = EventBus::new();
            bus.subscribe("k")
        };
        // Bus is dropped; sender side gone.
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            RecvOutcome::Disconnected => {}
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[test]
    fn publish_delivers_to_matching_subscriber() {
        let bus = EventBus::new();
        let rx = bus.subscribe("calendar.*");
        bus.publish(mk("calendar.event_imminent"));
        let e = rx.try_recv().expect("matching event should arrive");
        assert_eq!(e.kind, "calendar.event_imminent");
    }

    #[test]
    fn publish_skips_non_matching_subscriber() {
        let bus = EventBus::new();
        let rx = bus.subscribe("slack.*");
        bus.publish(mk("calendar.event_imminent"));
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn multiple_subscribers_all_receive() {
        let bus = EventBus::new();
        let rx_all = bus.subscribe("*");
        let rx_foo = bus.subscribe("foo.*");
        let rx_bar = bus.subscribe("bar.*");
        bus.publish(mk("foo.created"));
        assert_eq!(rx_all.try_recv().unwrap().kind, "foo.created");
        assert_eq!(rx_foo.try_recv().unwrap().kind, "foo.created");
        assert!(rx_bar.try_recv().is_none());
    }

    #[test]
    fn full_subscriber_drops_newest_and_preserves_queued() {
        let bus = EventBus::new();
        let rx = bus.subscribe_with_buffer("*", 2);
        bus.publish(mk("a"));
        bus.publish(mk("b"));
        bus.publish(mk("c"));
        assert_eq!(rx.try_recv().unwrap().kind, "a");
        assert_eq!(rx.try_recv().unwrap().kind, "b");
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn dropped_receiver_is_cleaned_up_on_next_publish() {
        let bus = EventBus::new();
        let rx = bus.subscribe("*");
        bus.publish(mk("first"));
        assert_eq!(bus.subscriber_count(), 1);
        drop(rx);
        bus.publish(mk("second"));
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn unbounded_subscriber_never_drops() {
        let bus = EventBus::new();
        let rx = bus.subscribe_unbounded("*");
        for i in 0..1000 {
            bus.publish(mk(&format!("k{i}")));
        }
        for i in 0..1000 {
            assert_eq!(rx.try_recv().unwrap().kind, format!("k{i}"));
        }
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn unbounded_and_bounded_coexist() {
        let bus = EventBus::new();
        let rx_u = bus.subscribe_unbounded("*");
        let rx_b = bus.subscribe_with_buffer("*", 2);
        bus.publish(mk("a"));
        bus.publish(mk("b"));
        bus.publish(mk("c"));
        // Unbounded got all three; bounded kept only first two.
        assert_eq!(rx_u.try_recv().unwrap().kind, "a");
        assert_eq!(rx_u.try_recv().unwrap().kind, "b");
        assert_eq!(rx_u.try_recv().unwrap().kind, "c");
        assert!(rx_u.try_recv().is_none());
        assert_eq!(rx_b.try_recv().unwrap().kind, "a");
        assert_eq!(rx_b.try_recv().unwrap().kind, "b");
        assert!(rx_b.try_recv().is_none());
    }

    #[test]
    fn event_timestamp_is_populated() {
        let before = now_millis();
        let e = Event::new("x", "y", json!({}));
        let after = now_millis();
        assert!(e.timestamp_ms >= before && e.timestamp_ms <= after);
    }

    #[test]
    fn fresh_event_has_no_bridge_id() {
        let e = Event::new("x", "y", json!({}));
        assert!(e.bridge_id.is_none());
    }

    #[test]
    fn bridge_id_is_skipped_on_serialization() {
        let e = Event::new("x", "y", json!({"k": "v"})).with_bridge_id(42);
        let json = serde_json::to_string(&e).unwrap();
        assert!(
            !json.contains("bridge_id"),
            "bridge_id leaked to wire: {json}"
        );
        let back: Event = serde_json::from_str(&json).unwrap();
        assert!(
            back.bridge_id.is_none(),
            "bridge_id must default to None on deserialization"
        );
    }

    #[test]
    fn next_bridge_id_is_monotonic_and_nonzero() {
        let a = next_bridge_id();
        let b = next_bridge_id();
        assert!(a >= 1, "starts at 1 or later");
        assert!(b > a, "monotonically increasing");
    }

    #[test]
    fn publish_bridged_stamps_id_on_bus_subscriber() {
        let bus = EventBus::new();
        let rx = bus.subscribe_unbounded("*");
        bus.publish_bridged(Event::new("x", "y", json!({})), 7);
        let got = rx.try_recv().expect("event delivered");
        assert_eq!(got.bridge_id, Some(7));
    }

    #[test]
    fn history_records_recent_in_arrival_order() {
        let bus = EventBus::new();
        bus.publish(mk("a"));
        bus.publish(mk("b"));
        bus.publish(mk("c"));
        let h = bus.history(None, None);
        let kinds: Vec<&str> = h.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["a", "b", "c"]);
    }

    #[test]
    fn history_drops_oldest_when_capacity_exceeded() {
        let bus = EventBus::with_capacities(DEFAULT_SUBSCRIBER_BUFFER, 2);
        bus.publish(mk("a"));
        bus.publish(mk("b"));
        bus.publish(mk("c"));
        let h = bus.history(None, None);
        let kinds: Vec<&str> = h.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["b", "c"]);
    }

    #[test]
    fn history_filters_by_since_ms() {
        let bus = EventBus::new();
        bus.publish(mk("a"));
        let after_a = bus
            .history(None, None)
            .first()
            .map(|e| e.timestamp_ms)
            .unwrap();
        // Sleep enough that b's timestamp_ms is strictly greater than a's.
        std::thread::sleep(std::time::Duration::from_millis(5));
        bus.publish(mk("b"));
        let after_a_filtered = bus.history(Some(after_a + 1), None);
        assert_eq!(after_a_filtered.len(), 1);
        assert_eq!(after_a_filtered[0].kind, "b");
    }

    #[test]
    fn history_filters_by_kind_glob() {
        let bus = EventBus::new();
        bus.publish(mk("jira.assigned"));
        bus.publish(mk("slack.dm"));
        bus.publish(mk("jira.commented"));
        let jira_only = bus.history(None, Some("jira.*"));
        let kinds: Vec<&str> = jira_only.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["jira.assigned", "jira.commented"]);
    }

    #[test]
    fn fresh_event_defaults_to_internal_origin() {
        let e = Event::new("x", "y", json!({}));
        assert_eq!(e.origin, Origin::Internal);
    }

    #[test]
    fn with_origin_overrides_default() {
        let e = Event::new("x", "y", json!({})).with_origin(Origin::External);
        assert_eq!(e.origin, Origin::External);
    }

    #[test]
    fn origin_round_trips_through_serde() {
        let e = Event::new("x", "y", json!({"a": 1})).with_origin(Origin::External);
        let json_str = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&json_str).unwrap();
        assert_eq!(back.origin, Origin::External);
    }

    #[test]
    fn origin_deserializes_as_internal_when_missing() {
        // Forward-compat with older serialized payloads that don't
        // carry the field at all.
        let raw = r#"{"kind":"k","source":"s","timestamp_ms":0,"payload":null}"#;
        let e: Event = serde_json::from_str(raw).unwrap();
        assert_eq!(e.origin, Origin::Internal);
    }

    #[test]
    fn history_min_capacity_clamped_to_one() {
        let bus = EventBus::with_capacities(DEFAULT_SUBSCRIBER_BUFFER, 0);
        assert_eq!(bus.history_capacity(), 1);
        bus.publish(mk("a"));
        bus.publish(mk("b"));
        let h = bus.history(None, None);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].kind, "b");
    }
}
